//! Quiesce a USB endpoint before reclaiming controller-owned DPRAM buffers.

use embassy_rp::pac;
use portable_atomic::{AtomicU32, Ordering};

pub(crate) static USB_EPOCH: AtomicU32 = AtomicU32::new(0);

fn clear_alias() -> pac::usb::Usb {
    // SAFETY: RP2040's APB +0x3000 window atomically clears register bits.
    unsafe { pac::usb::Usb::from_ptr(pac::USB.as_ptr().byte_add(0x3000)) }
}

pub(crate) fn reset() {
    USB_EPOCH.fetch_add(1, Ordering::SeqCst);
    pac::USB_DPRAM.ep_in_control(1).modify(|w| {
        w.set_double_buffered(false);
        w.set_interrupt_per_double_buff(false);
        w.set_interrupt_per_buff(true);
    });
    let clear = clear_alias();
    clear.ep_abort().write(|w| {
        w.set_ep_in(2, true);
        w.set_ep_out(1, true);
    });
    clear.ep_abort_done().write(|w| {
        w.set_ep_in(2, true);
        w.set_ep_out(1, true);
    });
}

pub(crate) struct AbortGuard {
    input: bool,
    index: usize,
}

impl AbortGuard {
    pub(crate) fn new(input: bool, index: usize) -> Option<Self> {
        let ram = pac::USB_DPRAM;
        let enabled = if input {
            ram.ep_in_control(index - 1).read().enable()
        } else {
            ram.ep_out_control(index - 1).read().enable()
        };
        if !enabled {
            return None;
        }
        let usb = pac::USB;
        clear_alias().ep_abort_done().write(|w| {
            if input {
                w.set_ep_in(index, true);
            } else {
                w.set_ep_out(index, true);
            }
        });
        usb.ep_abort().modify(|w| {
            if input {
                w.set_ep_in(index, true);
            } else {
                w.set_ep_out(index, true);
            }
        });
        // Abort completes after the current transaction. Bound cleanup even
        // on unsupported/faulty silicon; never rewrite still-owned buffers.
        for _ in 0..16_384 {
            let done = usb.ep_abort_done().read();
            if if input {
                done.ep_in(index)
            } else {
                done.ep_out(index)
            } {
                return Some(Self { input, index });
            }
        }
        // Leave ABORT asserted and disable this endpoint until USB reset.
        // Returning stale data is worse than an explicit transport failure.
        if input {
            ram.ep_in_control(index - 1).modify(|w| w.set_enable(false));
        } else {
            ram.ep_out_control(index - 1)
                .modify(|w| w.set_enable(false));
        }
        None
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        // DPRAM control updates must cross into clk_usb before the endpoint
        // can observe AVAILABLE or STALL again (RP2040 section 4.1.2.7.1).
        cortex_m::asm::delay(12);
        let clear = clear_alias();
        clear.ep_abort().write(|w| {
            if self.input {
                w.set_ep_in(self.index, true);
            } else {
                w.set_ep_out(self.index, true);
            }
        });
        clear.ep_abort_done().write(|w| {
            if self.input {
                w.set_ep_in(self.index, true);
            } else {
                w.set_ep_out(self.index, true);
            }
        });
    }
}
