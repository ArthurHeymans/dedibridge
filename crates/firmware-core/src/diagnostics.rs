use crate::flash::Error;
use dedi_protocol::diagnostics::{self as wire, Activity, Health, Uart};
use embassy_time::Instant;
use portable_atomic::{AtomicU32, Ordering};
use zerocopy::byteorder::{LittleEndian, U32};

pub struct Counter(AtomicU32);
impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }
    pub fn add(&self, amount: u32) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(amount))
            });
    }
    pub fn increment(&self) {
        self.add(1);
    }
    pub fn maximum(&self, value: u32) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }
    fn wire(&self) -> U32<LittleEndian> {
        U32::new(self.0.load(Ordering::Relaxed))
    }
}
impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct UartCounters {
    pub rx_bytes_read: Counter,
    pub tx_bytes_accepted: Counter,
    pub rx_driver_errors: Counter,
    pub tx_driver_errors: Counter,
    pub rx_queue_dropped_bytes: Counter,
    pub rx_delivery_dropped_bytes: Counter,
    pub rx_delivery_failures: Counter,
    pub rx_queue_high_water: Counter,
}
impl UartCounters {
    pub const fn new() -> Self {
        Self {
            rx_bytes_read: Counter::new(),
            tx_bytes_accepted: Counter::new(),
            rx_driver_errors: Counter::new(),
            tx_driver_errors: Counter::new(),
            rx_queue_dropped_bytes: Counter::new(),
            rx_delivery_dropped_bytes: Counter::new(),
            rx_delivery_failures: Counter::new(),
            rx_queue_high_water: Counter::new(),
        }
    }
    pub fn snapshot(&self) -> Uart {
        Uart {
            version: wire::VERSION,
            page: wire::UART_PAGE,
            reserved: [0; 2],
            rx_bytes_read: self.rx_bytes_read.wire(),
            tx_bytes_accepted: self.tx_bytes_accepted.wire(),
            rx_driver_errors: self.rx_driver_errors.wire(),
            tx_driver_errors: self.tx_driver_errors.wire(),
            rx_queue_dropped_bytes: self.rx_queue_dropped_bytes.wire(),
            rx_delivery_dropped_bytes: self.rx_delivery_dropped_bytes.wire(),
            rx_delivery_failures: self.rx_delivery_failures.wire(),
            rx_queue_high_water: self.rx_queue_high_water.wire(),
        }
    }
}
impl Default for UartCounters {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ActivityCounters {
    pub bulk_usb_timeouts: Counter,
    pub cancellations: Counter,
    pub flash_hardware_errors: Counter,
    pub flash_busy_errors: Counter,
    pub flash_busy_timeouts: Counter,
    pub flash_cancelled: Counter,
    pub flash_unsupported: Counter,
    pub bulk_completed: Counter,
    pub bulk_failed: Counter,
    pub aux_in_failures: Counter,
    pub aux_out_errors: Counter,
    pub late_usb_retirements: Counter,
}
impl ActivityCounters {
    pub const fn new() -> Self {
        Self {
            bulk_usb_timeouts: Counter::new(),
            cancellations: Counter::new(),
            flash_hardware_errors: Counter::new(),
            flash_busy_errors: Counter::new(),
            flash_busy_timeouts: Counter::new(),
            flash_cancelled: Counter::new(),
            flash_unsupported: Counter::new(),
            bulk_completed: Counter::new(),
            bulk_failed: Counter::new(),
            aux_in_failures: Counter::new(),
            aux_out_errors: Counter::new(),
            late_usb_retirements: Counter::new(),
        }
    }
    pub fn flash_result<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            match error {
                Error::Hardware => &self.flash_hardware_errors,
                Error::FlashBusy => &self.flash_busy_errors,
                Error::FlashBusyTimeout => &self.flash_busy_timeouts,
                Error::Cancelled => &self.flash_cancelled,
                Error::Unsupported => &self.flash_unsupported,
            }
            .increment();
        }
        result
    }
    pub fn snapshot(&self) -> Activity {
        Activity {
            version: wire::VERSION,
            page: wire::ACTIVITY_PAGE,
            reserved: [0; 2],
            bulk_usb_timeouts: self.bulk_usb_timeouts.wire(),
            cancellations: self.cancellations.wire(),
            flash_hardware_errors: self.flash_hardware_errors.wire(),
            flash_busy_errors: self.flash_busy_errors.wire(),
            flash_busy_timeouts: self.flash_busy_timeouts.wire(),
            flash_cancelled: self.flash_cancelled.wire(),
            flash_unsupported: self.flash_unsupported.wire(),
            bulk_completed: self.bulk_completed.wire(),
            bulk_failed: self.bulk_failed.wire(),
            aux_in_failures: self.aux_in_failures.wire(),
            aux_out_errors: self.aux_out_errors.wire(),
            late_usb_retirements: self.late_usb_retirements.wire(),
        }
    }
}
impl Default for ActivityCounters {
    fn default() -> Self {
        Self::new()
    }
}
pub static ACTIVITY: ActivityCounters = ActivityCounters::new();

pub struct DmaCounters {
    pub hardware_errors: Counter,
    pub progress_errors: Counter,
    pub ring_high_water: Counter,
}
pub static RX_DMA: DmaCounters = DmaCounters {
    hardware_errors: Counter::new(),
    progress_errors: Counter::new(),
    ring_high_water: Counter::new(),
};
impl DmaCounters {
    pub fn snapshot(&self) -> wire::DmaRx {
        wire::DmaRx {
            version: wire::VERSION,
            page: wire::DMA_RX_PAGE,
            reserved: [0; 2],
            hardware_errors: self.hardware_errors.wire(),
            progress_errors: self.progress_errors.wire(),
            ring_high_water: self.ring_high_water.wire(),
        }
    }
}

pub fn health(board: u8) -> Health {
    let mut firmware_version = [0; 16];
    let version = env!("CARGO_PKG_VERSION").as_bytes();
    let len = version.len().min(firmware_version.len());
    firmware_version[..len].copy_from_slice(&version[..len]);
    Health {
        version: wire::VERSION,
        page: wire::HEALTH_PAGE,
        board,
        // CH32 DMA/timer/error-IRQ races remain unqualified. Do not claim zero
        // counters prove completeness. No board has qualified boot retention.
        flags: if board == 2 {
            wire::RX_GAPS_UNOBSERVED | wire::DMA_RX_COUNTERS
        } else {
            0
        },
        uptime_secs: U32::new(Instant::now().as_secs().min(u64::from(u32::MAX)) as u32),
        firmware_version,
        reset_cause: U32::new(0),
        boot_count: U32::new(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_saturate_and_high_water_never_decreases() {
        let c = Counter::new();
        c.add(u32::MAX - 1);
        c.add(3);
        c.increment();
        assert_eq!(c.wire().get(), u32::MAX);
        let high = Counter::new();
        high.maximum(17);
        high.maximum(3);
        assert_eq!(high.wire().get(), 17);
    }
    #[test]
    fn flash_error_variants_are_separate_and_results_unchanged() {
        let c = ActivityCounters::new();
        for e in [
            Error::Hardware,
            Error::FlashBusy,
            Error::FlashBusyTimeout,
            Error::Cancelled,
            Error::Unsupported,
        ] {
            assert_eq!(c.flash_result::<()>(Err(e)), Err(e));
        }
        assert_eq!(c.flash_result(Ok(42)), Ok(42));
        let s = c.snapshot();
        assert_eq!(
            [
                s.flash_hardware_errors.get(),
                s.flash_busy_errors.get(),
                s.flash_busy_timeouts.get(),
                s.flash_cancelled.get(),
                s.flash_unsupported.get()
            ],
            [1; 5]
        );
    }
}
