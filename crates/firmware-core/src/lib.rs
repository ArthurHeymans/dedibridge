#![no_std]
#![allow(async_fn_in_trait)]

#[cfg(test)]
extern crate std;

pub mod aux;
pub mod bulk;
pub mod diagnostics;
pub mod flash;
pub mod gpio;
pub mod handler;
pub mod rx_ring;
pub mod transport;
pub mod usb;
pub use dedi_protocol::sf600 as protocol;

pub mod config {
    pub const BULK_BLOCK_SIZE: usize = 512;
    pub const PAGE_SIZE: usize = 256;
    pub const DEFAULT_SPI_FREQ_HZ: u32 = 24_000_000;
    pub const MAX_SPI_FREQ_HZ: u32 = 24_000_000;
    pub const SPI_CMD_WRITE_ENABLE: u8 = 0x06;
    pub const SPI_CMD_READ_STATUS: u8 = 0x05;
    pub const SPI_STATUS_WIP: u8 = 1;
}
