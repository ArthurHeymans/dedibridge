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
    input: bool,
}
impl Recovery for ChRecovery {
    type Guard = PacketGuard;
    fn packet_guard(input: bool) -> PacketGuard {
        PacketGuard { input }
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
        use ch32_hal::pac::usbhs::vals::{EpRxResponse, EpTog, EpTxResponse, UsbToken};
        critical_section::with(|_| {
            let r = ch32_hal::pac::USBHS;
            let d = unsafe { ch32_hal::pac::usbhs::Usbd::from_ptr(r.as_ptr()) };
            let index = if self.input { 2 } else { 1 };
            if self.input {
                d.ep_tx_ctrl(index)
                    .modify(|v| v.set_mask_uep_t_res(EpTxResponse::NAK));
            } else {
                d.ep_rx_ctrl(index)
                    .modify(|v| v.set_mask_uep_r_res(EpRxResponse::NAK));
            }
            if d.int_fg().read().transfer() {
                let st = r.int_st().read();
                if st.endp() as usize == index
                    && st.token()
                        == if self.input {
                            UsbToken::IN
                        } else {
                            UsbToken::OUT
                        }
                {
                    if self.input || st.tog_ok() {
                        if self.input {
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
                }
            }
        });
    }
}
