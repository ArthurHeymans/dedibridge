use ch32_hal::{
    gpio::Output,
    mode::Async,
    pac::gpio::vals,
    peripherals::{SPI2, USART2},
    spi::Spi,
    time::Hertz,
    usart::{UartRx, UartTx},
};
use dedi_core::{
    flash::{Error, SingleSpi},
    transport::Recovery,
};

pub struct FlashBus {
    pub spi: Spi<'static, SPI2, Async>,
    pub cs: Output<'static>,
}
fn mode(pin: usize, active: bool, cs: bool) {
    ch32_hal::pac::GPIOB.cfghr().modify(|w| {
        w.set_mode(
            pin % 8,
            if active {
                vals::Mode::OUTPUT_50MHZ
            } else {
                vals::Mode::INPUT
            },
        );
        w.set_cnf(
            pin % 8,
            if active {
                if cs {
                    vals::Cnf::ANALOG_IN__PUSH_PULL_OUT
                } else {
                    vals::Cnf::PULL_IN__AF_PUSH_PULL_OUT
                }
            } else if cs {
                vals::Cnf::PULL_IN__AF_PUSH_PULL_OUT
            } else {
                vals::Cnf::FLOATING_IN__OPEN_DRAIN_OUT
            },
        );
    });
}
impl SingleSpi for FlashBus {
    fn select(&mut self, active: bool) {
        if !active {
            self.cs.set_high();
        }
        mode(13, active, false);
        mode(15, active, false);
        mode(12, active, true);
        if active {
            self.cs.set_low();
        }
    }
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error> {
        let mut cfg = self.spi.get_current_config();
        cfg.frequency = Hertz::hz(hz);
        self.spi.set_config(&cfg).map_err(|_| Error::Hardware)
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

pub struct ChRx(pub UartRx<'static, USART2, Async>);
pub struct ChTx(pub UartTx<'static, USART2, Async>);
impl dedi_core::aux::UartRx for ChRx {
    async fn read_byte(&mut self) -> Result<u8, dedi_core::aux::UartError> {
        let mut byte = [0];
        self.0
            .read(&mut byte)
            .await
            .map_err(|_| dedi_core::aux::UartError)?;
        Ok(byte[0])
    }
    fn set_baud(&mut self, baud: u32) -> Result<(), dedi_core::aux::UartError> {
        let mut config = ch32_hal::usart::Config::default();
        config.baudrate = baud;
        self.0
            .set_config(&config)
            .map_err(|_| dedi_core::aux::UartError)
    }
    async fn flush_rx(&mut self) -> Result<(), dedi_core::aux::UartError> {
        // The read future's DMA guard has been dropped before this is called.
        for _ in 0..16 {
            let status = ch32_hal::pac::USART2.statr().read();
            // STATR then DATAR clears stale overrun/framing flags too, even
            // when DMA already consumed the last RXNE byte.
            ch32_hal::pac::USART2.datar().read();
            if !status.rxne() {
                break;
            }
        }
        Ok(())
    }
}
impl dedi_core::aux::UartTx for ChTx {
    async fn write(&mut self, data: &[u8]) -> Result<(), dedi_core::aux::UartError> {
        self.0
            .write(data)
            .await
            .map_err(|_| dedi_core::aux::UartError)?;
        // DMA completion precedes the final UART stop bit.
        embassy_time::with_timeout(embassy_time::Duration::from_millis(100), async {
            while !ch32_hal::pac::USART2.statr().read().tc() {
                embassy_futures::yield_now().await;
            }
        })
        .await
        .map_err(|_| dedi_core::aux::UartError)
    }
}

pub struct ChRecovery;
pub struct PacketGuard {
    endpoint: embassy_usb::driver::EndpointAddress,
    completed: bool,
}
// A token already in flight can finish after Drop examined UIF_TRANSFER.
// The IRQ hook retires that unowned completion rather than letting the HAL
// mask its transfer IRQ forever. Bits are endpoint/direction scoped.
static CANCELLED_PACKETS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
struct OutPacket {
    bytes: [u8; 512],
    len: usize,
}
// Only auxiliary EP3 is preserved. Abandoned EP1 flash data belongs to the old
// bulk generation and must never be replayed into a fresh host session.
static CANCELLED_AUX_OUT: critical_section::Mutex<core::cell::RefCell<Option<OutPacket>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(None));
fn packet_bit(endpoint: embassy_usb::driver::EndpointAddress) -> u32 {
    1 << (endpoint.index() + if endpoint.is_in() { 16 } else { 0 })
}
pub struct CancelledTransferHandler;
impl ch32_hal::interrupt::typelevel::Handler<ch32_hal::interrupt::typelevel::USBHS>
    for CancelledTransferHandler
{
    unsafe fn on_interrupt() {
        use ch32_hal::pac::usbhs::vals::UsbToken;
        let r = ch32_hal::pac::USBHS;
        if !r.int_fg().read().transfer() {
            return;
        }
        let status = r.int_st().read();
        if status.endp() == 0 {
            return;
        }
        let direction = match status.token() {
            UsbToken::IN => embassy_usb::driver::Direction::In,
            UsbToken::OUT => embassy_usb::driver::Direction::Out,
            _ => return,
        };
        let endpoint =
            embassy_usb::driver::EndpointAddress::from_parts(status.endp() as usize, direction);
        if CANCELLED_PACKETS.load(portable_atomic::Ordering::SeqCst) & packet_bit(endpoint) != 0
            && retire_packet(endpoint)
        {
            dedi_core::diagnostics::ACTIVITY
                .late_usb_retirements
                .increment();
        }
    }
}
fn retire_packet(endpoint: embassy_usb::driver::EndpointAddress) -> bool {
    use ch32_hal::pac::usbhs::vals::{EpRxResponse, EpTog, EpTxResponse, UsbToken};
    // Caller holds a critical section or runs in the USB IRQ.
    let r = ch32_hal::pac::USBHS;
    let d = unsafe { ch32_hal::pac::usbhs::Usbd::from_ptr(r.as_ptr()) };
    let index = endpoint.index();
    if endpoint.is_in() {
        d.ep_tx_ctrl(index)
            .modify(|v| v.set_mask_uep_t_res(EpTxResponse::NAK));
    } else {
        d.ep_rx_ctrl(index)
            .modify(|v| v.set_mask_uep_r_res(EpRxResponse::NAK));
    }
    let st = r.int_st().read();
    if !d.int_fg().read().transfer()
        || st.endp() as usize != index
        || st.token()
            != if endpoint.is_in() {
                UsbToken::IN
            } else {
                UsbToken::OUT
            }
    {
        return false;
    }
    if endpoint.is_in() || st.tog_ok() {
        if endpoint.is_out()
            && index == 3
            && d.ep_config().read().r_en(2)
            && !r.int_fg().read().bus_rst()
        {
            critical_section::with(|cs| {
                let mut cache = CANCELLED_AUX_OUT.borrow(cs).borrow_mut();
                // Cancellation disarms EP3; it cannot accept a second packet
                // until this one is taken and a subsequent read rearms it.
                let packet = cache.insert(OutPacket {
                    bytes: [0; 512],
                    len: r.rx_len().read() as usize,
                });
                let buffer = d.ep_rx_dma(index - 1).read() as *const u8;
                for (offset, byte) in packet.bytes[..packet.len.min(512)].iter_mut().enumerate() {
                    // USB DMA RAM, not MMIO; the transfer flag means DMA ended.
                    *byte = unsafe { core::ptr::read_volatile(buffer.add(offset)) };
                }
            });
        }
        if endpoint.is_in() {
            d.ep_tx_ctrl(index).modify(|v| {
                v.set_mask_uep_t_tog(if v.mask_uep_t_tog() == EpTog::DATA0 {
                    EpTog::DATA1
                } else {
                    EpTog::DATA0
                })
            });
        } else {
            d.ep_rx_ctrl(index).modify(|v| {
                v.set_mask_uep_r_tog(if v.mask_uep_r_tog() == EpTog::DATA0 {
                    EpTog::DATA1
                } else {
                    EpTog::DATA0
                })
            });
        }
    }
    d.int_fg().write(|v| v.set_transfer(true));
    d.int_en().modify(|v| v.set_transfer(true));
    true
}
impl Recovery for ChRecovery {
    type Guard = PacketGuard;
    const CANCEL_ON_PROG_INFO: bool = true;
    fn packet_guard(endpoint: embassy_usb::driver::EndpointAddress) -> PacketGuard {
        critical_section::with(|_| {
            // Finish any completion belonging to the last cancelled future
            // before a new one reuses its buffer or waits on its flag.
            if CANCELLED_PACKETS.load(portable_atomic::Ordering::SeqCst) & packet_bit(endpoint) != 0
            {
                retire_packet(endpoint);
            }
            CANCELLED_PACKETS.fetch_and(!packet_bit(endpoint), portable_atomic::Ordering::SeqCst);
        });
        PacketGuard {
            endpoint,
            completed: false,
        }
    }
    fn complete(guard: &mut PacketGuard) {
        guard.completed = true;
    }
    fn take_cancelled_out(
        endpoint: embassy_usb::driver::EndpointAddress,
        data: &mut [u8],
    ) -> Option<Result<usize, embassy_usb::driver::EndpointError>> {
        if endpoint.is_in() || endpoint.index() != 3 {
            return None;
        }
        critical_section::with(|cs| {
            let packet = CANCELLED_AUX_OUT.borrow(cs).borrow_mut().take()?;
            let d = unsafe { ch32_hal::pac::usbhs::Usbd::from_ptr(ch32_hal::pac::USBHS.as_ptr()) };
            if !d.ep_config().read().r_en(2) || ch32_hal::pac::USBHS.int_fg().read().bus_rst() {
                return None;
            }
            Some(
                if packet.len > data.len() || packet.len > packet.bytes.len() {
                    Err(embassy_usb::driver::EndpointError::BufferOverflow)
                } else {
                    data[..packet.len].copy_from_slice(&packet.bytes[..packet.len]);
                    Ok(packet.len)
                },
            )
        })
    }
    fn reset() {
        critical_section::with(|cs| {
            *CANCELLED_AUX_OUT.borrow(cs).borrow_mut() = None;
            CANCELLED_PACKETS.store(0, portable_atomic::Ordering::SeqCst);
        });
    }
    fn prepare(input: bool, stalled: bool) {
        use ch32_hal::pac::usbhs::vals::{EpRxResponse, EpTxResponse};
        critical_section::with(|_| {
            let usb =
                unsafe { ch32_hal::pac::usbhs::Usbd::from_ptr(ch32_hal::pac::USBHS.as_ptr()) };
            if input {
                usb.ep_tx_ctrl(2).modify(|w| {
                    w.set_mask_uep_t_res(if stalled {
                        EpTxResponse::STALL
                    } else {
                        EpTxResponse::NAK
                    })
                });
            } else {
                usb.ep_rx_ctrl(1).modify(|w| {
                    w.set_mask_uep_r_res(if stalled {
                        EpRxResponse::STALL
                    } else {
                        EpRxResponse::NAK
                    })
                });
            }
        });
    }
}
impl Drop for PacketGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        critical_section::with(|_| {
            CANCELLED_PACKETS
                .fetch_or(packet_bit(self.endpoint), portable_atomic::Ordering::SeqCst);
            retire_packet(self.endpoint);
        });
    }
}
