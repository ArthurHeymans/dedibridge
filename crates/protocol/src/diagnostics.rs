//! Read-only diagnostic pages. Layout version is independent of aux VERSION.
use zerocopy::byteorder::{LittleEndian, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const VERSION: u8 = 1;
pub const HEALTH_PAGE: u8 = 0;
pub const UART_PAGE: u8 = 1;
pub const ACTIVITY_PAGE: u8 = 2;
/// Mandatory pages; optional pages are advertised by Health.flags.
pub const PAGE_COUNT: u8 = 3;
pub const DMA_RX_PAGE: u8 = 3;
pub const DMA_RX_COUNTERS: u8 = 1 << 3;
pub const RX_GAPS_UNOBSERVED: u8 = 1 << 0;
pub const RESET_CAUSE_KNOWN: u8 = 1 << 1;
pub const BOOT_COUNT_KNOWN: u8 = 1 << 2;

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Health {
    pub version: u8,
    pub page: u8,
    pub board: u8,
    pub flags: u8,
    pub uptime_secs: U32<LittleEndian>,
    pub firmware_version: [u8; 16],
    pub reset_cause: U32<LittleEndian>,
    pub boot_count: U32<LittleEndian>,
}

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Uart {
    pub version: u8,
    pub page: u8,
    pub reserved: [u8; 2],
    pub rx_bytes_read: U32<LittleEndian>,
    pub tx_bytes_accepted: U32<LittleEndian>,
    pub rx_driver_errors: U32<LittleEndian>,
    pub tx_driver_errors: U32<LittleEndian>,
    pub rx_queue_dropped_bytes: U32<LittleEndian>,
    pub rx_delivery_dropped_bytes: U32<LittleEndian>,
    pub rx_delivery_failures: U32<LittleEndian>,
    pub rx_queue_high_water: U32<LittleEndian>,
}

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Activity {
    pub version: u8,
    pub page: u8,
    pub reserved: [u8; 2],
    pub bulk_usb_timeouts: U32<LittleEndian>,
    pub cancellations: U32<LittleEndian>,
    pub flash_hardware_errors: U32<LittleEndian>,
    pub flash_busy_errors: U32<LittleEndian>,
    pub flash_busy_timeouts: U32<LittleEndian>,
    pub flash_cancelled: U32<LittleEndian>,
    pub flash_unsupported: U32<LittleEndian>,
    pub bulk_completed: U32<LittleEndian>,
    pub bulk_failed: U32<LittleEndian>,
    pub aux_in_failures: U32<LittleEndian>,
    pub aux_out_errors: U32<LittleEndian>,
    pub late_usb_retirements: U32<LittleEndian>,
}

#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct DmaRx {
    pub version: u8,
    pub page: u8,
    pub reserved: [u8; 2],
    pub hardware_errors: U32<LittleEndian>,
    pub progress_errors: U32<LittleEndian>,
    pub ring_high_water: U32<LittleEndian>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_fit_one_response_without_padding() {
        assert_eq!(core::mem::size_of::<Health>(), 32);
        assert_eq!(core::mem::size_of::<Uart>(), 36);
        assert_eq!(core::mem::size_of::<Activity>(), 52);
        assert_eq!(core::mem::size_of::<DmaRx>(), 16);
        for len in [
            core::mem::size_of::<Health>(),
            core::mem::size_of::<Uart>(),
            core::mem::size_of::<Activity>(),
            core::mem::size_of::<DmaRx>(),
        ] {
            assert!(len <= crate::aux::MAX_PAYLOAD_LEN - 2);
        }
    }
}
