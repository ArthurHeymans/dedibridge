//! RP2040 USB DPRAM has independently owned 16-bit buffer-control halves.
//!
//! A 32-bit read-modify-write can resurrect the other buffer's AVAILABLE/FULL
//! bits if USB hardware completes it between the CPU's read and write. DPRAM
//! supports halfword stores, unlike the RP2040's ordinary peripheral registers.

#[cfg(not(test))]
use embassy_rp::pac;
#[cfg(test)]
use rp_pac as pac;

/// Build a bank's control half without reading the neighboring hardware bank.
/// Keep RESET asserted while publishing AVAILABLE for the first bank.
#[inline]
pub(crate) fn packet_control(length: u16, pid: bool, reset: bool, available: bool) -> u16 {
    let mut control = pac::usb_dpram::regs::EpBufferControl(0);
    control.set_length(0, length);
    control.set_pid(0, pid);
    control.set_full(0, true);
    control.set_reset(reset);
    control.set_available(0, available);
    control.0 as u16
}

/// Update only the buffer-control half owned by software.
///
/// # Safety
/// `control` must point to an aligned, writable RP2040 USB DPRAM buffer-control
/// word, and hardware must have released the selected bank to software. The
/// other bank may still be owned and modified by the USB controller.
#[inline]
pub(crate) unsafe fn write_halfword(control: *mut u32, bank: usize, value: u16) {
    assert!(bank < 2);
    // SAFETY: each half is 16-bit aligned; the caller owns this bank. A single
    // STRH must not read or write the neighboring hardware-owned half.
    unsafe { control.cast::<u16>().add(bank).write_volatile(value) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C, align(4))]
    struct Banks([u16; 2]);

    #[test]
    fn selector_reset_survives_publication_of_the_first_available_bank() {
        for pid in [false, true] {
            let staged = packet_control(64, pid, true, false);
            let published = packet_control(64, pid, true, true);
            assert_eq!(staged ^ published, 1 << 10);
            let control = pac::usb_dpram::regs::EpBufferControl(u32::from(published));
            assert!(control.reset());
            assert!(control.available(0));
            assert!(control.full(0));
            assert_eq!(control.pid(0), pid);
            assert_eq!(control.length(0), 64);
            assert!(
                !pac::usb_dpram::regs::EpBufferControl(u32::from(packet_control(
                    64, !pid, false, true
                )))
                .reset()
            );
        }
    }

    #[test]
    fn updating_one_bank_preserves_the_other_owned_bank() {
        let mut banks = Banks([0, 0xe440]);
        let control = banks.0.as_mut_ptr().cast::<u32>();
        unsafe { write_halfword(control, 0, 0x8440) };
        assert_eq!(banks.0, [0x8440, 0xe440]);
        unsafe { write_halfword(control, 1, 0xa440) };
        assert_eq!(banks.0, [0x8440, 0xa440]);
    }

    #[test]
    fn hardware_completion_is_not_overwritten_by_a_stale_word() {
        for completed in 0..2 {
            let mut banks = Banks([0x8440; 2]);
            let stale = banks.0;
            // USB completes the neighboring buffer after a hypothetical
            // full-word read. Replaying that word would re-arm the buffer.
            banks.0[completed] = 0;
            let control = banks.0.as_mut_ptr().cast::<u32>();
            let owned = completed ^ 1;
            unsafe { write_halfword(control, owned, stale[owned]) };
            assert_eq!(banks.0[completed], 0);
            assert_eq!(banks.0[owned], 0x8440);
        }
    }

    #[test]
    #[should_panic]
    fn rejects_an_invalid_bank_before_accessing_memory() {
        unsafe { write_halfword(core::ptr::null_mut(), 2, 0) };
    }
}
