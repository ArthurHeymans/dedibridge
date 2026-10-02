use crate::{
    diagnostics::{self, ACTIVITY, UartCounters},
    gpio::BoardIo,
};
use dedi_protocol::aux::*;
use dedi_protocol::diagnostics::{ACTIVITY_PAGE, HEALTH_PAGE, UART_PAGE};
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embassy_usb::driver::{EndpointIn, EndpointOut};
use portable_atomic::{AtomicBool, AtomicU32, Ordering};
use zerocopy::IntoBytes;

#[derive(Clone, Copy, Debug)]
pub struct UartError;

pub trait UartRx {
    async fn read_byte(&mut self) -> Result<u8, UartError>;
    /// Reconfigure both RX and TX while TX is idle. Called only after the read
    /// future has been dropped; successful application, not queuing, is ACKed.
    fn set_baud(&mut self, baud: u32) -> Result<(), UartError>;
    /// Discard bytes already buffered by the peripheral/driver, not only the
    /// USB service's software queue. Bound draining even on a noisy console.
    async fn flush_rx(&mut self) -> Result<(), UartError>;
}
pub trait UartTx {
    /// Complete when the last stop bit is sent, so baud can change safely.
    async fn write(&mut self, data: &[u8]) -> Result<(), UartError>;
}
#[derive(Clone, Copy)]
enum RxCommand {
    Baud(u32),
    Flush,
}

#[derive(Clone, Copy)]
struct TxPacket {
    bytes: [u8; MAX_PAYLOAD_LEN],
    len: usize,
}
#[derive(Clone, Copy)]
struct GpioCommand {
    kind: u8,
    mask: u8,
    value: u8,
    ms: u16,
}

pub struct AuxState {
    info: DeviceInfo,
    diagnostics: UartCounters,
    rx: Channel<CriticalSectionRawMutex, u8, 256>,
    tx: Channel<CriticalSectionRawMutex, TxPacket, 4>,
    baud: Signal<CriticalSectionRawMutex, RxCommand>,
    baud_result: Signal<CriticalSectionRawMutex, bool>,
    gpio: Channel<CriticalSectionRawMutex, GpioCommand, 1>,
    gpio_result: Signal<CriticalSectionRawMutex, [u8; 4]>,
    gpio_changed: Signal<CriticalSectionRawMutex, [u8; 4]>,
    tx_busy: AtomicBool,
    rx_lost: AtomicU32,
    tx_lost: AtomicU32,
    loss: Signal<CriticalSectionRawMutex, ()>,
}
impl AuxState {
    pub const fn new(mut info: DeviceInfo) -> Self {
        info.flags |= CAP_DIAG;
        Self {
            info,
            diagnostics: UartCounters::new(),
            rx: Channel::new(),
            tx: Channel::new(),
            baud: Signal::new(),
            baud_result: Signal::new(),
            gpio: Channel::new(),
            gpio_result: Signal::new(),
            gpio_changed: Signal::new(),
            tx_busy: AtomicBool::new(false),
            rx_lost: AtomicU32::new(0),
            tx_lost: AtomicU32::new(0),
            loss: Signal::new(),
        }
    }
    pub async fn run_rx(&self, mut uart: impl UartRx) -> ! {
        let mut budget = 0;
        loop {
            match select(self.baud.wait(), uart.read_byte()).await {
                Either::First(command) => {
                    let result = match command {
                        RxCommand::Baud(baud) => uart.set_baud(baud),
                        RxCommand::Flush => uart.flush_rx().await,
                    };
                    self.baud_result.signal(result.is_ok());
                }
                Either::Second(Ok(byte)) => {
                    self.diagnostics.rx_bytes_read.increment();
                    if self.rx.try_send(byte).is_err() {
                        self.diagnostics.rx_queue_dropped_bytes.increment();
                        self.rx_lost.fetch_add(1, Ordering::Relaxed);
                        self.loss.signal(());
                    } else {
                        self.diagnostics
                            .rx_queue_high_water
                            .maximum(self.rx.len() as u32);
                    }
                }
                Either::Second(Err(_)) => {
                    self.diagnostics.rx_driver_errors.increment();
                    self.rx_lost.fetch_add(1, Ordering::Relaxed);
                    self.loss.signal(());
                    Timer::after_millis(1).await;
                }
            }
            // Continuous RX must not starve GPIO deadlines or USB servicing.
            budget += 1;
            if budget == 16 {
                budget = 0;
                embassy_futures::yield_now().await;
            }
        }
    }
    pub async fn run_tx(&self, mut uart: impl UartTx) -> ! {
        loop {
            let packet = self.tx.receive().await;
            self.tx_busy.store(true, Ordering::SeqCst);
            if uart.write(&packet.bytes[..packet.len]).await.is_err() {
                self.diagnostics.tx_driver_errors.increment();
                self.tx_lost.fetch_add(packet.len as u32, Ordering::Relaxed);
                self.loss.signal(());
            }
            self.tx_busy.store(false, Ordering::SeqCst);
        }
    }
    /// Owns GPIO independently of USB. Reset/power pulses always release even
    /// when the host stops reading, disconnects, or a UART response is stalled.
    pub async fn run_gpio(&self, mut gpio: impl BoardIo) -> ! {
        loop {
            if gpio.finish_pulse_if_due() {
                self.gpio_changed.signal(gpio.state());
            }
            let deadline = gpio.pulse_deadline().unwrap_or(Instant::MAX);
            match select(self.gpio.receive(), Timer::at(deadline)).await {
                Either::First(cmd) => {
                    match cmd.kind {
                        CMD_GPIO_SET_DIRECTION => gpio.set_direction(cmd.mask, cmd.value),
                        CMD_GPIO_SET_OUTPUT => gpio.set_output(cmd.mask, cmd.value),
                        CMD_GPIO_PULSE_LOW => gpio.pulse_low(cmd.mask, cmd.ms),
                        _ => {}
                    }
                    self.gpio_result.signal(gpio.state());
                }
                Either::Second(()) => {
                    if gpio.finish_pulse_if_due() {
                        self.gpio_changed.signal(gpio.state());
                    }
                }
            }
        }
    }
    async fn packet(&self, ep: &mut impl EndpointIn, kind: u8, id: u8, data: &[u8]) -> bool {
        let mut buffer = [0; PACKET_LEN];
        let Some(packet) = encode(&mut buffer, kind, id, data) else {
            return false;
        };
        let sent = matches!(
            with_timeout(Duration::from_millis(100), ep.write(packet)).await,
            Ok(Ok(()))
        );
        if !sent {
            ACTIVITY.aux_in_failures.increment();
        }
        sent
    }
    async fn response(
        &self,
        ep: &mut impl EndpointIn,
        id: u8,
        command: u8,
        status: u8,
        data: &[u8],
    ) {
        if id == 0 {
            return;
        }
        let mut buffer = [0; MAX_PAYLOAD_LEN];
        buffer[0] = command;
        buffer[1] = status;
        buffer[2..2 + data.len()].copy_from_slice(data);
        self.packet(ep, EVT_RESPONSE, id, &buffer[..2 + data.len()])
            .await;
    }
    async fn command(&self, ep: &mut impl EndpointIn, packet: &[u8]) {
        let Some(data) = payload(packet) else {
            return;
        };
        let command = packet[0];
        let id = packet[1];
        if command == CMD_GET_INFO && data.is_empty() {
            self.response(ep, id, command, STATUS_OK, self.info.as_bytes())
                .await;
            return;
        }
        if command == CMD_GET_DIAG && data.len() == 1 {
            match data[0] {
                HEALTH_PAGE => {
                    self.response(
                        ep,
                        id,
                        command,
                        STATUS_OK,
                        diagnostics::health(self.info.board).as_bytes(),
                    )
                    .await
                }
                UART_PAGE => {
                    self.response(
                        ep,
                        id,
                        command,
                        STATUS_OK,
                        self.diagnostics.snapshot().as_bytes(),
                    )
                    .await
                }
                ACTIVITY_PAGE => {
                    self.response(ep, id, command, STATUS_OK, ACTIVITY.snapshot().as_bytes())
                        .await
                }
                _ => self.response(ep, id, command, STATUS_INVALID, &[]).await,
            }
            return;
        }
        let mut status = STATUS_INVALID;
        let mut state = None;
        let valid_mask = |mask: u8| mask != 0 && mask & !(GPIO_RESET | GPIO_POWER) == 0;
        let gpio_cmd = match command {
            CMD_GPIO_GET_STATE if data.is_empty() => Some(GpioCommand {
                kind: command,
                mask: 0,
                value: 0,
                ms: 0,
            }),
            CMD_GPIO_SET_DIRECTION | CMD_GPIO_SET_OUTPUT
                if data.len() == 2 && valid_mask(data[0]) =>
            {
                Some(GpioCommand {
                    kind: command,
                    mask: data[0],
                    value: data[1],
                    ms: 0,
                })
            }
            CMD_GPIO_PULSE_LOW if data.len() == 3 && valid_mask(data[0]) => Some(GpioCommand {
                kind: command,
                mask: data[0],
                value: 0,
                ms: u16::from_le_bytes([data[1], data[2]]),
            }),
            _ => None,
        };
        if let Some(cmd) = gpio_cmd {
            self.gpio.send(cmd).await;
            state = Some(self.gpio_result.wait().await);
            status = STATUS_OK;
        } else if command == CMD_UART_SET_BAUD && data.len() == 4 {
            let baud = u32::from_le_bytes(data.try_into().unwrap());
            if (300..=self.info.max_baud.get()).contains(&baud) {
                if self.tx_busy.load(Ordering::SeqCst) || !self.tx.is_empty() {
                    status = STATUS_BUSY;
                } else {
                    self.baud.signal(RxCommand::Baud(baud));
                    if self.baud_result.wait().await {
                        status = STATUS_OK;
                    }
                }
            }
        } else if command == CMD_UART_WRITE {
            let mut tx = TxPacket {
                bytes: [0; MAX_PAYLOAD_LEN],
                len: data.len(),
            };
            tx.bytes[..data.len()].copy_from_slice(data);
            status = if self.tx.try_send(tx).is_ok() {
                self.diagnostics.tx_bytes_accepted.add(data.len() as u32);
                STATUS_OK
            } else {
                STATUS_BUSY
            };
        } else if command == CMD_UART_FLUSH_RX && data.is_empty() {
            self.baud.signal(RxCommand::Flush);
            if self.baud_result.wait().await {
                while self.rx.try_receive().is_ok() {}
                self.rx_lost.store(0, Ordering::Relaxed);
                status = STATUS_OK;
            }
        }
        self.response(
            ep,
            id,
            command,
            status,
            state.as_ref().map_or(&[], |s| s.as_slice()),
        )
        .await;
    }
    pub async fn run_usb(&self, mut output: impl EndpointOut, mut input: impl EndpointIn) -> ! {
        // CH32's USBHS driver requires an OUT buffer at least its descriptor
        // MPS (512); packet validation still limits auxiliary messages to 64.
        let mut usb_buf = [0; 512];
        let mut uart_buf = [0; MAX_PAYLOAD_LEN];
        let mut len = 0;
        let mut deadline = Instant::MAX;
        loop {
            match select4(
                output.read(&mut usb_buf),
                self.gpio_changed.wait(),
                select(self.loss.wait(), self.rx.receive()),
                Timer::at(deadline),
            )
            .await
            {
                Either4::First(Ok(count)) => {
                    let packet = &usb_buf[..count];
                    if packet.first() == Some(&CMD_UART_FLUSH_RX) && payload(packet) == Some(&[]) {
                        len = 0;
                        deadline = Instant::MAX;
                    }
                    self.command(&mut input, packet).await;
                }
                Either4::First(Err(_)) => {
                    ACTIVITY.aux_out_errors.increment();
                    Timer::after_millis(1).await;
                }
                Either4::Second(state) => {
                    self.packet(&mut input, EVT_GPIO_STATE, 0, &state).await;
                }
                Either4::Third(Either::First(())) => {}
                Either4::Third(Either::Second(byte)) => {
                    if len == 0 {
                        deadline = Instant::now() + Duration::from_millis(1);
                    }
                    uart_buf[len] = byte;
                    len += 1;
                }
                Either4::Fourth(()) => {}
            }
            if len > 0 && (len == MAX_PAYLOAD_LEN || Instant::now() >= deadline) {
                if !self
                    .packet(&mut input, EVT_UART_DATA, 0, &uart_buf[..len])
                    .await
                {
                    self.diagnostics.rx_delivery_failures.increment();
                    self.diagnostics.rx_delivery_dropped_bytes.add(len as u32);
                    self.rx_lost.fetch_add(len as u32, Ordering::Relaxed);
                }
                len = 0;
                deadline = Instant::MAX;
            }
            for (direction, counter) in [(0, &self.rx_lost), (1, &self.tx_lost)] {
                let lost = counter.swap(0, Ordering::Relaxed);
                if lost != 0 {
                    let mut report = [direction, 0, 0, 0, 0];
                    report[1..].copy_from_slice(&lost.to_le_bytes());
                    if !self.packet(&mut input, EVT_UART_OVERFLOW, 0, &report).await {
                        counter.fetch_add(lost, Ordering::Relaxed);
                    }
                }
            }
            embassy_futures::yield_now().await;
        }
    }
}

#[cfg(test)]
#[path = "aux_tests.rs"]
mod tests;
