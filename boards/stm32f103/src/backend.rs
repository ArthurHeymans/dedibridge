use dedi_core::{
    flash::{Error, SingleSpi},
    transport::Recovery,
};
use embassy_stm32::{
    gpio::{Input, Output},
    interrupt::typelevel::Interrupt,
    mode::Async,
    pac,
    spi::{Spi, mode::Master},
    time::Hertz,
    usart::UartTx,
};

pub struct FlashBus {
    pub spi: Spi<'static, Async, Master>,
    pub cs: Output<'static>,
}
fn pin_mode(pin: usize, active: bool, cs: bool) {
    // F1 MODE/CNF fields: 0xB AF push-pull 50 MHz, 0x3 GPIO push-pull,
    // 0x4 floating input, 0x8 pull-up input (ODR parked high for CS).
    let value = if active {
        if cs { 3 } else { 11 }
    } else if cs {
        8
    } else {
        4
    };
    pac::GPIOB.cr(1).modify(|w| {
        let shift = (pin - 8) * 4;
        w.0 = (w.0 & !(15 << shift)) | (value << shift);
    });
}
impl SingleSpi for FlashBus {
    fn select(&mut self, active: bool) {
        if !active {
            self.cs.set_high();
        }
        pin_mode(13, active, false);
        pin_mode(15, active, false);
        pin_mode(12, active, true);
        if active {
            self.cs.set_low();
        }
    }
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error> {
        let mut config = embassy_stm32::spi::Config::default();
        config.frequency = Hertz(hz);
        self.spi.set_config(&config).map_err(|_| Error::Hardware)
    }
    fn blocking_write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.spi.blocking_write(data).map_err(|_| Error::Hardware)
    }
    fn blocking_read(&mut self, data: &mut [u8]) -> Result<(), Error> {
        if data.is_empty() {
            Ok(())
        } else {
            self.spi.blocking_read(data).map_err(|_| Error::Hardware)
        }
    }
    async fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            Ok(())
        } else {
            self.spi.write(data).await.map_err(|_| Error::Hardware)
        }
    }
    async fn read(&mut self, data: &mut [u8]) -> Result<(), Error> {
        self.spi.read(data).await.map_err(|_| Error::Hardware)
    }
}

// Matches main's 72 MHz PLL and APB1 /2. USART2 always uses APB1 on F103.
pub const UART_CLOCK_HZ: u32 = 36_000_000;
static UART_RX: embassy_sync::channel::Channel<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    u8,
    256,
> = embassy_sync::channel::Channel::new();
static UART_ERROR: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    (),
> = embassy_sync::signal::Signal::new();
pub struct RxInterrupt;
impl embassy_stm32::interrupt::typelevel::Handler<embassy_stm32::interrupt::typelevel::USART2>
    for RxInterrupt
{
    unsafe fn on_interrupt() {
        // Own RX rather than BufferedUart's handler: it currently logs hardware
        // errors and silently drops bytes when its ring is full.
        let sr = pac::USART2.sr().read();
        let error = sr.ore() || sr.fe() || sr.ne() || sr.pe();
        if sr.rxne() || error {
            // Keep SR/DR adjacent: this sequence clears RXNE and error flags.
            let byte = pac::USART2.dr().read().dr() as u8;
            if error || UART_RX.try_send(byte).is_err() {
                UART_ERROR.signal(());
            }
        }
    }
}
pub struct StmRx {
    _pin: Input<'static>,
}
pub struct StmTx(pub UartTx<'static, Async>);
impl StmRx {
    pub fn new(pin: Input<'static>) -> Self {
        pac::USART2.cr1().modify(|w| {
            w.set_re(true);
            w.set_rxneie(true);
            w.set_peie(true);
        });
        pac::USART2.cr3().modify(|w| w.set_eie(true));
        embassy_stm32::interrupt::typelevel::USART2::unpend();
        unsafe {
            embassy_stm32::interrupt::typelevel::USART2::enable();
        }
        Self { _pin: pin }
    }
}
impl dedi_core::aux::UartRx for StmRx {
    async fn read_byte(&mut self) -> Result<u8, dedi_core::aux::UartError> {
        match embassy_futures::select::select(UART_ERROR.wait(), UART_RX.receive()).await {
            embassy_futures::select::Either::First(()) => Err(dedi_core::aux::UartError),
            embassy_futures::select::Either::Second(byte) => Ok(byte),
        }
    }
    fn set_baud(&mut self, baud: u32) -> Result<(), dedi_core::aux::UartError> {
        if baud == 0 {
            return Err(dedi_core::aux::UartError);
        }
        let divider = (UART_CLOCK_HZ + baud / 2) / baud;
        if !(16..=u32::from(u16::MAX)).contains(&divider) {
            return Err(dedi_core::aux::UartError);
        }
        // Core checked TX physically idle. Update the shared UART, keeping the
        // RX IRQ/ring and DMA TX ownership intact; the format remains 8N1.
        critical_section::with(|_| {
            pac::USART2.cr1().modify(|w| w.set_ue(false));
            pac::USART2.brr().write(|w| w.set_brr(divider as u16));
            pac::USART2.cr1().modify(|w| w.set_ue(true));
        });
        Ok(())
    }
    async fn flush_rx(&mut self) -> Result<(), dedi_core::aux::UartError> {
        critical_section::with(|_| {
            // Also discard DR and its sticky errors before clearing the ring.
            pac::USART2.sr().read();
            pac::USART2.dr().read();
            UART_RX.clear();
            UART_ERROR.reset();
        });
        Ok(())
    }
}
impl dedi_core::aux::UartTx for StmTx {
    async fn write(&mut self, data: &[u8]) -> Result<(), dedi_core::aux::UartError> {
        self.0
            .write(data)
            .await
            .map_err(|_| dedi_core::aux::UartError)?;
        // DMA completion is not UART completion. RX owns the USART IRQ, so
        // avoid the HAL's TC-interrupt flush and wait for the last stop bit.
        embassy_time::with_timeout(embassy_time::Duration::from_millis(100), async {
            while !pac::USART2.sr().read().tc() {
                embassy_futures::yield_now().await;
            }
        })
        .await
        .map_err(|_| dedi_core::aux::UartError)
    }
}

pub struct StmRecovery;
pub struct PacketGuard {
    input: bool,
    completed: bool,
}
fn endpoint_status(input: bool, stalled: bool) {
    use pac::usb::vals::Stat;
    critical_section::with(|_| {
        let register = pac::USB.epr(if input { 2 } else { 1 });
        let old = register.read();
        if (input && old.stat_tx() == Stat::DISABLED) || (!input && old.stat_rx() == Stat::DISABLED)
        {
            return;
        }
        let mut value = old;
        value.set_ctr_rx(true);
        value.set_ctr_tx(true);
        value.set_dtog_rx(false);
        value.set_dtog_tx(false);
        value.set_stat_rx(Stat::from_bits(0));
        value.set_stat_tx(Stat::from_bits(0));
        if input {
            let wanted = if stalled { Stat::STALL } else { Stat::NAK };
            value.set_stat_tx(Stat::from_bits(old.stat_tx().to_bits() ^ wanted.to_bits()));
        } else {
            let wanted = if stalled { Stat::STALL } else { Stat::VALID };
            value.set_stat_rx(Stat::from_bits(old.stat_rx().to_bits() ^ wanted.to_bits()));
        }
        register.write_value(value);
    });
}
impl Recovery for StmRecovery {
    type Guard = PacketGuard;
    fn packet_guard(input: bool) -> PacketGuard {
        PacketGuard {
            input,
            completed: false,
        }
    }
    fn complete(guard: &mut PacketGuard) {
        guard.completed = true;
    }
    async fn write_complete() -> Result<(), embassy_usb::driver::EndpointError> {
        use pac::usb::vals::Stat;
        loop {
            match pac::USB.epr(2).read().stat_tx() {
                Stat::NAK => return Ok(()),
                Stat::DISABLED => return Err(embassy_usb::driver::EndpointError::Disabled),
                _ => embassy_futures::yield_now().await,
            }
        }
    }
    fn prepare(input: bool, stalled: bool) {
        endpoint_status(input, stalled);
    }
}
impl Drop for PacketGuard {
    fn drop(&mut self) {
        if self.input && !self.completed {
            endpoint_status(true, false);
        }
    }
}
