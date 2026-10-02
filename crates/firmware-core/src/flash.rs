use crate::protocol::IoMode;
use embassy_time::{Duration, Instant, Timer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum Error {
    Hardware,
    FlashBusy,
    FlashBusyTimeout,
    Cancelled,
    Unsupported,
}

/// A flash engine, not a conventional SPI device: RP2040 implements multi-lane
/// phases in PIO. Futures are always awaited to completion by the bulk worker;
/// backends must bound status polling and release CS on errors.
pub trait Flash {
    const IO_MODES: u8;
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error>;
    fn select(&mut self, selected: bool);
    fn transceive(&mut self, command: &[u8], response: &mut [u8]) -> Result<(), Error>;
    #[allow(clippy::too_many_arguments)]
    async fn start_read(
        &mut self,
        opcode: u8,
        address: u32,
        addr_len: u8,
        mode: IoMode,
        mode_byte: Option<u8>,
        dummy_cycles: u8,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), Error>;
    async fn read_block(&mut self, data: &mut [u8], mode: IoMode) -> Result<(), Error>;
    async fn write_page(
        &mut self,
        opcode: u8,
        address: u32,
        addr_len: u8,
        data: &[u8],
        cancelled: impl Fn() -> bool,
    ) -> Result<(), Error>;
    fn end_transfer(&mut self) {
        self.select(false);
    }
}

/// The small hardware surface needed by both STM32 and CH32 single-lane flash.
/// acquire/release include pin muxing: a deasserted CS alone isn't bus isolation.
pub trait SingleSpi {
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error>;
    fn select(&mut self, selected: bool);
    fn blocking_write(&mut self, data: &[u8]) -> Result<(), Error>;
    fn blocking_read(&mut self, data: &mut [u8]) -> Result<(), Error>;
    async fn write(&mut self, data: &[u8]) -> Result<(), Error>;
    async fn read(&mut self, data: &mut [u8]) -> Result<(), Error>;
}

pub struct SingleFlash<S> {
    bus: S,
    busy_until: Option<Instant>,
}
impl<S: SingleSpi> SingleFlash<S> {
    pub fn new(mut bus: S) -> Self {
        bus.select(false);
        Self {
            bus,
            busy_until: None,
        }
    }
    async fn wait_ready(&mut self, cancelled: &impl Fn() -> bool) -> Result<(), Error> {
        // The original CH32 backend also settles for 25 ms after an erase.
        let ready_at = self
            .busy_until
            .map(|until| until.max(Instant::now()) + Duration::from_millis(25));
        loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            let now = Instant::now();
            let Some(until) = ready_at.filter(|t| now < *t) else {
                self.busy_until = None;
                return Ok(());
            };
            // Keep flash ownership, but let USB cancellation and UART/GPIO run.
            Timer::after((until - now).min(Duration::from_millis(1))).await;
        }
    }
    async fn poll_wip(&mut self, cancelled: &impl Fn() -> bool) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            let mut status = [0];
            self.bus.select(true);
            let result = async {
                self.bus.write(&[0x05]).await?;
                self.bus.read(&mut status).await
            }
            .await;
            self.bus.select(false);
            result?;
            if status[0] & 1 == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::FlashBusyTimeout);
            }
            Timer::after_micros(50).await;
        }
    }
}

pub fn erase_busy_duration(command: &[u8]) -> Option<Duration> {
    match command.first()? {
        0x20 | 0x21 => Some(Duration::from_millis(250)),
        0x52 | 0x5c => Some(Duration::from_millis(750)),
        0xd8 | 0xdc => Some(Duration::from_millis(1500)),
        0x60 | 0xc7 => Some(Duration::from_secs(60)),
        _ => None,
    }
}

fn address_command(opcode: u8, address: u32, addr_len: u8) -> Result<([u8; 5], usize), Error> {
    if !matches!(addr_len, 3 | 4) {
        return Err(Error::Unsupported);
    }
    let mut command = [0; 5];
    command[0] = opcode;
    let address = address.to_be_bytes();
    command[1..1 + addr_len as usize].copy_from_slice(&address[4 - addr_len as usize..]);
    Ok((command, 1 + addr_len as usize))
}

impl<S: SingleSpi> Flash for SingleFlash<S> {
    const IO_MODES: u8 = 1;
    fn set_frequency(&mut self, hz: u32) -> Result<(), Error> {
        self.bus
            .set_frequency(hz.clamp(1, crate::config::MAX_SPI_FREQ_HZ))
    }
    fn select(&mut self, selected: bool) {
        self.bus.select(selected);
    }
    fn transceive(&mut self, command: &[u8], response: &mut [u8]) -> Result<(), Error> {
        if command.first() == Some(&0x05) && self.busy_until.is_some_and(|t| Instant::now() < t) {
            response.fill(1);
            return Ok(());
        }
        self.bus.select(true);
        let result = self
            .bus
            .blocking_write(command)
            .and_then(|()| self.bus.blocking_read(response));
        self.bus.select(false);
        if result.is_ok()
            && response.is_empty()
            && let Some(duration) = erase_busy_duration(command)
        {
            self.busy_until = Some(Instant::now() + duration);
        }
        result
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
        self.wait_ready(&cancelled).await?;
        if mode != IoMode::Single || mode_byte.is_some() || !dummy_cycles.is_multiple_of(8) {
            return Err(Error::Unsupported);
        }
        let (command, len) = address_command(opcode, address, addr_len)?;
        self.bus.select(true);
        let result = async {
            self.bus.write(&command[..len]).await?;
            let dummy = [0; 32];
            self.bus
                .write(&dummy[..usize::from(dummy_cycles / 8)])
                .await
        }
        .await;
        if result.is_err() {
            self.bus.select(false);
        }
        result
    }
    async fn read_block(&mut self, data: &mut [u8], mode: IoMode) -> Result<(), Error> {
        if mode != IoMode::Single {
            return Err(Error::Unsupported);
        }
        self.bus.read(data).await
    }
    async fn write_page(
        &mut self,
        opcode: u8,
        address: u32,
        addr_len: u8,
        data: &[u8],
        cancelled: impl Fn() -> bool,
    ) -> Result<(), Error> {
        self.wait_ready(&cancelled).await?;
        let (command, len) = address_command(opcode, address, addr_len)?;
        self.bus.select(true);
        let wren = self.bus.write(&[0x06]).await;
        self.bus.select(false);
        wren?;
        Timer::after_micros(1).await;
        self.bus.select(true);
        let result = async {
            self.bus.write(&command[..len]).await?;
            self.bus.write(data).await
        }
        .await;
        self.bus.select(false);
        result?;
        Timer::after_micros(1000).await;
        self.poll_wip(&cancelled).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Bus {
        writes: std::vec::Vec<std::vec::Vec<u8>>,
    }
    impl SingleSpi for Bus {
        fn set_frequency(&mut self, _: u32) -> Result<(), Error> {
            Ok(())
        }
        fn select(&mut self, _: bool) {}
        fn blocking_write(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.writes.push(bytes.to_vec());
            Ok(())
        }
        fn blocking_read(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
            bytes.fill(0);
            Ok(())
        }
        async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.blocking_write(bytes)
        }
        async fn read(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
            self.blocking_read(bytes)
        }
    }
    #[test]
    fn all_erase_windows_survive_unrelated_write_only_commands() {
        for (opcode, ms) in [
            (0x20, 250),
            (0x21, 250),
            (0x52, 750),
            (0x5c, 750),
            (0xd8, 1500),
            (0xdc, 1500),
            (0x60, 60000),
            (0xc7, 60000),
        ] {
            assert_eq!(
                erase_busy_duration(&[opcode]),
                Some(Duration::from_millis(ms))
            );
            let mut flash = SingleFlash::new(Bus::default());
            flash.transceive(&[opcode], &mut []).unwrap();
            let until = flash.busy_until.unwrap();
            for other in [0x06, 0x04, 0x01] {
                flash.transceive(&[other], &mut []).unwrap();
                assert_eq!(flash.busy_until, Some(until));
                let mut status = [0; 16];
                flash.transceive(&[0x05], &mut status).unwrap();
                assert_eq!(status, [1; 16]);
            }
        }
    }
    #[test]
    fn early_bulk_waits_for_erase_settling_and_cancellation_keeps_window() {
        use futures::{executor::block_on, poll};
        use std::cell::Cell;
        let mut flash = SingleFlash::new(Bus::default());
        flash.busy_until = Some(Instant::now() + Duration::from_millis(2));
        let settled_at = flash.busy_until.unwrap() + Duration::from_millis(25);
        block_on(flash.start_read(3, 0, 3, IoMode::Single, None, 0, || false)).unwrap();
        assert!(Instant::now() >= settled_at);
        assert_eq!(flash.bus.writes[0], [3, 0, 0, 0]);
        assert!(flash.busy_until.is_none());
        for write in [false, true] {
            flash.busy_until = Some(Instant::now() + Duration::from_secs(60));
            let until = flash.busy_until;
            let count = flash.bus.writes.len();
            let cancelled = Cell::new(false);
            block_on(async {
                let operation = async {
                    if write {
                        flash
                            .write_page(2, 0, 3, &[0; 256], || cancelled.get())
                            .await
                    } else {
                        flash
                            .start_read(3, 0, 3, IoMode::Single, None, 0, || cancelled.get())
                            .await
                    }
                };
                let mut operation = core::pin::pin!(operation);
                assert!(poll!(operation.as_mut()).is_pending());
                cancelled.set(true);
                assert_eq!(operation.await, Err(Error::Cancelled));
            });
            assert_eq!(
                flash.bus.writes.len(),
                count,
                "no SPI during forced erase window"
            );
            assert_eq!(flash.busy_until, until);
        }
    }
    #[test]
    fn address_encoding() {
        let (cmd, len) = address_command(0x0b, 0x12345678, 3).unwrap();
        assert_eq!(&cmd[..len], &[0x0b, 0x34, 0x56, 0x78]);
        assert_eq!(address_command(3, 0, 2), Err(Error::Unsupported));
    }
}
