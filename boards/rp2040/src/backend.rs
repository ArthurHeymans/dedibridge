use crate::{
    UsbDriver,
    fast_bulk_in::FastBulkIn,
    spi_flash::{SpiError, SpiFlash},
};
use dedi_core::{
    flash::{Error, Flash},
    protocol::IoMode,
    transport::{BulkIn, Recovery},
};
use embassy_rp::{
    pac,
    peripherals::PIO1,
    pio_programs::uart::{PioUartRx, PioUartTx},
};
use embassy_usb::driver::{Endpoint, EndpointError, EndpointIn};
use fixed::{traits::ToFixed, types::extra::U8};

impl From<SpiError> for Error {
    fn from(e: SpiError) -> Self {
        match e {
            SpiError::PioTimeout => Self::Hardware,
            SpiError::FlashBusy => Self::FlashBusy,
            SpiError::FlashBusyTimeout => Self::FlashBusyTimeout,
            SpiError::Cancelled => Self::Cancelled,
        }
    }
}
impl Flash for SpiFlash<'_> {
    fn end_transfer(&mut self) {
        SpiFlash::end_transfer(self);
    }
    const IO_MODES: u8 = 0x1f;
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error> {
        SpiFlash::set_frequency(self, hz);
        Ok(())
    }
    fn select(&mut self, yes: bool) {
        if yes {
            self.cs_assert();
        } else {
            self.cs_deassert();
        }
    }
    fn transceive(&mut self, command: &[u8], response: &mut [u8]) -> Result<(), Error> {
        let result = if response.is_empty() {
            self.write_only_blocking(command)
        } else {
            self.transceive_blocking(command, response)
        };
        result.map_err(Into::into)
    }
    async fn start_read(
        &mut self,
        opcode: u8,
        address: u32,
        addr_len: u8,
        mode: IoMode,
        mode_byte: Option<u8>,
        dummy_cycles: u8,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), Error> {
        SpiFlash::start_read(
            self,
            opcode,
            address,
            addr_len,
            mode,
            mode_byte,
            dummy_cycles,
            cancelled,
        )
        .await
        .map_err(Into::into)
    }
    async fn read_block(&mut self, data: &mut [u8], mode: IoMode) -> Result<(), Error> {
        SpiFlash::read_block(self, data, mode)
            .await
            .map_err(Into::into)
    }
    async fn write_page(
        &mut self,
        opcode: u8,
        address: u32,
        addr_len: u8,
        data: &[u8],
        cancelled: impl Fn() -> bool,
    ) -> Result<(), Error> {
        SpiFlash::write_page(self, opcode, address, addr_len, data, cancelled)
            .await
            .map_err(Into::into)
    }
}

pub struct PicoRecovery;
impl Recovery for PicoRecovery {
    type Guard = ();
    fn packet_guard(_: bool) {}
    fn prepare(input: bool, stalled: bool) {
        let ram = pac::USB_DPRAM;
        if input {
            ram.ep_in_control(1).modify(|w| {
                w.set_interrupt_per_buff(true);
                w.set_interrupt_per_double_buff(false);
                w.set_double_buffered(false);
            });
            ram.ep_in_buffer_control(2).write(|w| {
                w.set_reset(true);
                w.set_pid(0, true);
                w.set_pid(1, false);
                w.set_stall(stalled);
            });
        } else {
            let control = ram.ep_out_buffer_control(1);
            control.write(|w| {
                w.set_reset(true);
                w.set_pid(0, false);
                w.set_length(0, 64);
                w.set_stall(stalled);
            });
            if !stalled {
                cortex_m::asm::delay(12);
                control.write(|w| {
                    w.set_pid(0, false);
                    w.set_length(0, 64);
                    w.set_available(0, true);
                });
            }
        }
    }
}

type In = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn;
pub struct PicoIn {
    endpoint: In,
    fast: Option<FastBulkIn>,
}
impl PicoIn {
    pub fn new(endpoint: In) -> Self {
        Self {
            endpoint,
            fast: None,
        }
    }
}
impl BulkIn for PicoIn {
    async fn wait_enabled(&mut self) {
        self.endpoint.wait_enabled().await;
    }
    fn start(&mut self) {
        self.fast = FastBulkIn::new_ep2();
    }
    async fn write_block(&mut self, data: &[u8; 512]) -> Result<(), EndpointError> {
        if let Some(fast) = &mut self.fast {
            fast.write_block(data).await
        } else {
            for packet in data.chunks(64) {
                self.endpoint.write(packet).await?;
            }
            Ok(())
        }
    }
    async fn flush(&mut self) -> Result<(), EndpointError> {
        if self.fast.is_some() {
            loop {
                if !pac::USB_DPRAM.ep_in_control(1).read().enable() {
                    return Err(EndpointError::Disabled);
                }
                let control = pac::USB_DPRAM.ep_in_buffer_control(2).read();
                if !control.available(0) && !control.available(1) {
                    break;
                }
                embassy_futures::yield_now().await;
            }
        }
        Ok(())
    }
    fn finish(&mut self) {
        self.fast = None;
    }
}

pub struct PicoRx(pub PioUartRx<'static, PIO1, 1>);
pub struct PicoTx(pub PioUartTx<'static, PIO1, 0>);
fn uart_rx_error() -> Result<(), dedi_core::aux::UartError> {
    let framing = pac::PIO1.irq().read().0 & (1 << 5) != 0;
    let overflow = pac::PIO1.fdebug().read().rxstall() & 2 != 0;
    if framing || overflow {
        pac::PIO1.irq().write(|w| w.0 = 1 << 5);
        pac::PIO1.fdebug().write(|w| w.set_rxstall(2));
        Err(dedi_core::aux::UartError)
    } else {
        Ok(())
    }
}
impl dedi_core::aux::UartRx for PicoRx {
    async fn read_byte(&mut self) -> Result<u8, dedi_core::aux::UartError> {
        loop {
            uart_rx_error()?;
            // A framing error need not push a byte. Detect it even if the line
            // remains low and the normal FIFO future never completes.
            let received = embassy_futures::select::select(
                self.0.read_u8(),
                embassy_time::Timer::after_millis(1),
            )
            .await;
            uart_rx_error()?;
            if let embassy_futures::select::Either::First(byte) = received {
                return Ok(byte);
            }
        }
    }
    fn set_baud(&mut self, baud: u32) -> Result<(), dedi_core::aux::UartError> {
        let clock = embassy_rp::clocks::clk_sys_freq().to_fixed::<fixed::FixedU64<U8>>();
        let divider = (clock / (8 * baud).to_fixed::<fixed::FixedU64<U8>>())
            .to_fixed::<fixed::FixedU32<U8>>();
        for sm in 0..=1 {
            pac::PIO1
                .sm(sm)
                .clkdiv()
                .write(|w| w.0 = divider.to_bits() << 8);
        }
        Ok(())
    }
    async fn flush_rx(&mut self) -> Result<(), dedi_core::aux::UartError> {
        // A full FIFO can leave an old PUSH stalled outside the FIFO itself.
        // Restart RX too, discarding any partial/in-flight frame at the barrier.
        pac::PIO1
            .ctrl()
            .modify(|w| w.set_sm_enable(w.sm_enable() & !2));
        let sm = pac::PIO1.sm(1);
        sm.shiftctrl().modify(|w| w.set_fjoin_rx(false));
        sm.shiftctrl().modify(|w| w.set_fjoin_rx(true));
        pac::PIO1.ctrl().modify(|w| w.set_sm_restart(2));
        let start = sm.execctrl().read().wrap_bottom();
        sm.instr().write(|w| w.set_instr(u16::from(start)));
        pac::PIO1
            .ctrl()
            .modify(|w| w.set_sm_enable(w.sm_enable() | 2));
        pac::PIO1.irq().write(|w| w.0 = 1 << 5);
        pac::PIO1.fdebug().write(|w| w.set_rxstall(2));
        Ok(())
    }
}
impl dedi_core::aux::UartTx for PicoTx {
    async fn write(&mut self, data: &[u8]) -> Result<(), dedi_core::aux::UartError> {
        embedded_io_async::Write::write_all(&mut self.0, data)
            .await
            .map_err(|_| dedi_core::aux::UartError)?;
        let idle = || {
            pac::PIO1.fstat().read().txempty() & 1 != 0
                && pac::PIO1.sm(0).addr().read().addr()
                    == pac::PIO1.sm(0).execctrl().read().wrap_bottom()
        };
        // PC can briefly be at PULL with an already consumed word during its
        // stop-bit delay. Require idle to persist for one bit before changing baud.
        let divider = u64::from(pac::PIO1.sm(0).clkdiv().read().0 >> 8);
        let bit_us =
            (divider * 8 * 1_000_000).div_ceil(u64::from(embassy_rp::clocks::clk_sys_freq()) * 256);
        embassy_time::with_timeout(embassy_time::Duration::from_secs(1), async {
            loop {
                if idle() {
                    embassy_time::Timer::after_micros(bit_us.max(1)).await;
                    if idle() {
                        break;
                    }
                } else {
                    embassy_futures::yield_now().await;
                }
            }
        })
        .await
        .map_err(|_| dedi_core::aux::UartError)
    }
}
