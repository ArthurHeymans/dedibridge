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
fn rx_error_wakes_usb_even_without_a_data_byte() {
    let state = AuxState::new(DeviceInfo::new(3, 1, 2_250_000));
    let input = ep(Direction::In);
    let packets = input.packets.clone();
    block_on(async {
        let mut receive = core::pin::pin!(state.run_rx(rx(true)));
        let mut usb = core::pin::pin!(state.run_usb(ep(Direction::Out), input));
        assert!(poll!(receive.as_mut()).is_pending());
        assert!(poll!(usb.as_mut()).is_pending());
        assert_eq!(packets.borrow().len(), 1);
        assert_eq!(packets.borrow()[0][0], EVT_UART_OVERFLOW);
        assert_eq!(payload(&packets.borrow()[0]), Some(&[0, 1, 0, 0, 0][..]));
    });
}
