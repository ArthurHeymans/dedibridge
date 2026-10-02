use super::*;
use embassy_usb::driver::{
    Direction, Endpoint, EndpointAddress, EndpointError, EndpointInfo, EndpointType,
};
use futures::{executor::block_on, poll};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    vec::Vec,
};

struct FakeRx {
    baud: Rc<Cell<u32>>,
    flushed: Rc<Cell<bool>>,
    error: bool,
}
impl UartRx for FakeRx {
    async fn read_byte(&mut self) -> Result<u8, UartError> {
        if self.error {
            self.error = false;
            Err(UartError)
        } else {
            core::future::pending().await
        }
    }
    fn set_baud(&mut self, baud: u32) -> Result<(), UartError> {
        self.baud.set(baud);
        Ok(())
    }
    async fn flush_rx(&mut self) -> Result<(), UartError> {
        self.flushed.set(true);
        Ok(())
    }
}
struct Ep {
    info: EndpointInfo,
    packets: Rc<RefCell<Vec<Vec<u8>>>>,
    writes_started: Rc<Cell<usize>>,
    blocked: bool,
}
impl Endpoint for Ep {
    fn info(&self) -> &EndpointInfo {
        &self.info
    }
    async fn wait_enabled(&mut self) {}
}
impl EndpointIn for Ep {
    async fn write(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        self.writes_started.set(self.writes_started.get() + 1);
        if self.blocked {
            return core::future::pending().await;
        }
        self.packets.borrow_mut().push(data.to_vec());
        Ok(())
    }
}
impl EndpointOut for Ep {
    async fn read(&mut self, _: &mut [u8]) -> Result<usize, EndpointError> {
        core::future::pending().await
    }
}
fn ep(direction: Direction) -> Ep {
    Ep {
        info: EndpointInfo {
            addr: EndpointAddress::from_parts(3, direction),
            ep_type: EndpointType::Bulk,
            max_packet_size: 64,
            interval_ms: 0,
        },
        packets: Default::default(),
        writes_started: Default::default(),
        blocked: false,
    }
}
#[test]
fn positive_pulse_releases_while_its_usb_response_is_stalled() {
    use crate::gpio::{BoardGpio, tests::Pin};
    use embedded_hal::digital::InputPin;
    let state = AuxState::new(DeviceInfo::new(1, 0x3f, 3_000_000));
    let reset = Pin::new(true);
    let mut monitor = reset.clone();
    let gpio = BoardGpio::new(reset, Pin::new(true), Pin::new(true), Pin::new(false));
    let mut input = ep(Direction::In);
    input.blocked = true;
    block_on(async {
        let mut gpio_task = core::pin::pin!(state.run_gpio(gpio));
        let mut response = core::pin::pin!(
            state.command(&mut input, &[CMD_GPIO_PULSE_LOW, 1, 3, GPIO_RESET, 5, 0])
        );
        assert!(poll!(response.as_mut()).is_pending());
        assert!(poll!(gpio_task.as_mut()).is_pending());
        assert!(monitor.is_low().unwrap());
        assert!(poll!(response.as_mut()).is_pending());
        Timer::after_millis(10).await;
        assert!(poll!(gpio_task.as_mut()).is_pending());
        assert!(
            monitor.is_high().unwrap(),
            "pulse was held by stalled USB response"
        );
        assert!(poll!(response.as_mut()).is_pending());
    });
}
fn rx(error: bool) -> FakeRx {
    FakeRx {
        baud: Default::default(),
        flushed: Default::default(),
        error,
    }
}

#[test]
fn baud_and_flush_acknowledge_application_not_queueing() {
    let state = AuxState::new(DeviceInfo::new(3, 1, 2_250_000));
    state.diagnostics.rx_queue_dropped_bytes.add(7);
    state.rx_lost.store(7, Ordering::Relaxed);
    let fake = rx(false);
    let baud = fake.baud.clone();
    let flushed = fake.flushed.clone();
    let mut input = ep(Direction::In);
    let packets = input.packets.clone();
    block_on(async {
        let mut worker = core::pin::pin!(state.run_rx(fake));
        let mut buffer = [0; PACKET_LEN];
        let request = encode(&mut buffer, CMD_UART_SET_BAUD, 1, &115200u32.to_le_bytes()).unwrap();
        {
            let mut command = core::pin::pin!(state.command(&mut input, request));
            assert!(poll!(command.as_mut()).is_pending());
            assert_eq!(baud.get(), 0);
            assert!(poll!(worker.as_mut()).is_pending());
            assert_eq!(baud.get(), 115200);
            assert!(packets.borrow().is_empty());
            assert!(poll!(command.as_mut()).is_ready());
        }
        assert_eq!(
            payload(&packets.borrow()[0]),
            Some(&[CMD_UART_SET_BAUD, STATUS_OK][..])
        );
        let request = encode(&mut buffer, CMD_UART_FLUSH_RX, 2, &[]).unwrap();
        {
            let mut command = core::pin::pin!(state.command(&mut input, request));
            assert!(poll!(command.as_mut()).is_pending());
            assert!(!flushed.get());
            assert!(poll!(worker.as_mut()).is_pending());
            assert!(flushed.get());
            assert!(poll!(command.as_mut()).is_ready());
        }
        assert_eq!(state.rx_lost.load(Ordering::Relaxed), 0);
        assert_eq!(
            state.diagnostics.snapshot().rx_queue_dropped_bytes.get(),
            7,
            "flush must not reset lifetime diagnostics"
        );
        state.tx_busy.store(true, Ordering::SeqCst);
        let request = encode(&mut buffer, CMD_UART_SET_BAUD, 3, &9600u32.to_le_bytes()).unwrap();
        state.command(&mut input, request).await;
        assert_eq!(baud.get(), 115200);
        assert_eq!(
            payload(&packets.borrow()[2]),
            Some(&[CMD_UART_SET_BAUD, STATUS_BUSY][..])
        );
    });
}

#[test]
fn diagnostic_pages_are_read_only_and_keep_aux_version_one() {
    use dedi_protocol::diagnostics::{
        Activity, BOOT_COUNT_KNOWN, Health, RESET_CAUSE_KNOWN, RX_GAPS_UNOBSERVED, Uart,
        VERSION as DIAG_VERSION,
    };
    use zerocopy::FromBytes;
    let state = AuxState::new(DeviceInfo::new(2, 1, 3_000_000));
    state.diagnostics.rx_bytes_read.add(100);
    state.rx_lost.store(3, Ordering::Relaxed);
    state.tx_busy.store(true, Ordering::SeqCst);
    let mut input = ep(Direction::In);
    let packets = input.packets.clone();
    block_on(async {
        state.command(&mut input, &[CMD_GET_INFO, 1, 0]).await;
        let info =
            DeviceInfo::read_from_bytes(&payload(&packets.borrow()[0]).unwrap()[2..]).unwrap();
        assert_eq!(info.version, 1);
        assert_ne!(info.flags & CAP_DIAG, 0);
        for page in 0..=3 {
            state
                .command(&mut input, &[CMD_GET_DIAG, page + 2, 1, page])
                .await;
        }
    });
    let packets = packets.borrow();
    let health = Health::read_from_bytes(&payload(&packets[1]).unwrap()[2..]).unwrap();
    assert_eq!(
        (health.version, health.page, health.board),
        (DIAG_VERSION, HEALTH_PAGE, 2)
    );
    assert_ne!(health.flags & RX_GAPS_UNOBSERVED, 0);
    assert_eq!(health.flags & (RESET_CAUSE_KNOWN | BOOT_COUNT_KNOWN), 0);
    let uart = Uart::read_from_bytes(&payload(&packets[2]).unwrap()[2..]).unwrap();
    assert_eq!(uart.rx_bytes_read.get(), 100);
    let activity = Activity::read_from_bytes(&payload(&packets[3]).unwrap()[2..]).unwrap();
    assert_eq!(activity.page, ACTIVITY_PAGE);
    assert_eq!(
        payload(&packets[4]),
        Some(&[CMD_GET_DIAG, STATUS_INVALID][..])
    );
    assert_eq!(state.rx_lost.load(Ordering::Relaxed), 3);
    assert!(state.tx_busy.load(Ordering::SeqCst));
    assert!(state.tx.is_empty() && state.gpio.is_empty());
}

#[test]
fn usb_delivery_loss_is_not_counted_as_a_uart_driver_error() {
    let state = AuxState::new(DeviceInfo::new(3, 1, 2_250_000));
    state.rx.try_send(42).unwrap();
    let mut input = ep(Direction::In);
    input.blocked = true;
    let started = input.writes_started.clone();
    block_on(async {
        let mut usb = core::pin::pin!(state.run_usb(ep(Direction::Out), input));
        assert!(poll!(usb.as_mut()).is_pending());
        Timer::after_millis(2).await;
        for _ in 0..8 {
            assert!(poll!(usb.as_mut()).is_pending());
            if started.get() != 0 {
                break;
            }
        }
        assert_eq!(
            started.get(),
            1,
            "IN timeout must be armed before advancing time"
        );
        Timer::after_millis(110).await;
        for _ in 0..8 {
            assert!(poll!(usb.as_mut()).is_pending());
            if state.diagnostics.snapshot().rx_delivery_failures.get() != 0 {
                break;
            }
        }
        let counters = state.diagnostics.snapshot();
        assert_eq!(counters.rx_delivery_dropped_bytes.get(), 1);
        assert_eq!(counters.rx_delivery_failures.get(), 1);
        assert_eq!(counters.rx_driver_errors.get(), 0);
    });
}

#[test]
fn rx_error_wakes_usb_even_without_a_data_byte() {
    let state = AuxState::new(DeviceInfo::new(3, 1, 2_250_000));
    let input = ep(Direction::In);
    let packets = input.packets.clone();
    block_on(async {
        let mut receive = core::pin::pin!(state.run_rx(rx(true)));
        let mut usb = core::pin::pin!(state.run_usb(ep(Direction::Out), input));
        assert!(poll!(receive.as_mut()).is_pending());
        assert!(poll!(usb.as_mut()).is_pending());
        assert_eq!(state.diagnostics.snapshot().rx_driver_errors.get(), 1);
        assert_eq!(packets.borrow().len(), 1);
        assert_eq!(packets.borrow()[0][0], EVT_UART_OVERFLOW);
        assert_eq!(payload(&packets.borrow()[0]), Some(&[0, 1, 0, 0, 0][..]));
    });
}
