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
    fn check_busy(&mut self, cancelled: &impl Fn() -> bool) -> Result<(), Error> {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if self.busy_until.is_some_and(|until| Instant::now() < until) {
            return Err(Error::FlashBusy);
        }
        self.busy_until = None;
        Ok(())
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
        if result.is_ok() && response.is_empty() {
            self.busy_until = erase_busy_duration(command).map(|d| Instant::now() + d);
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
        self.check_busy(&cancelled)?;
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
        self.check_busy(&cancelled)?;
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
    #[test]
    fn address_encoding() {
        let (cmd, len) = address_command(0x0b, 0x12345678, 3).unwrap();
        assert_eq!(&cmd[..len], &[0x0b, 0x34, 0x56, 0x78]);
        assert_eq!(address_command(3, 0, 2), Err(Error::Unsupported));
    }
}
