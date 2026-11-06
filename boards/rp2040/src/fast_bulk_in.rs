use core::sync::atomic::compiler_fence;

use defmt::warn;
use embassy_rp::pac;
use embassy_usb::driver::EndpointError;
use portable_atomic::{AtomicBool, Ordering};

use crate::{
    config::{BULK_BLOCK_SIZE, USB_MAX_PACKET_SIZE},
    usb_abort::{AbortGuard, USB_EPOCH},
    usb_buffer_control::{packet_control, write_halfword},
};

static FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Double-buffered writer for DediProg's fixed bulk-IN endpoint (EP2 IN).
///
/// Embassy's RP2040 driver currently allocates one DPRAM buffer per endpoint
/// and does not expose the controller's double-buffer mode. This wrapper reads
/// the address Embassy allocated from the endpoint control register instead of
/// depending on allocation order, then owns EP2's buffer control while active.
pub struct FastBulkIn {
    buffer0_addr: usize,
    next_buf: usize,
    next_pid: bool,
    reset_pending: bool,
    epoch: u32,
}

impl FastBulkIn {
    const EP_INDEX: usize = 2;
    const EP_CONTROL_INDEX: usize = Self::EP_INDEX - 1;
    const RESERVE_EP_INDEX: usize = 15;
    const RESERVE_EP_CONTROL_INDEX: usize = Self::RESERVE_EP_INDEX - 1;
    const BUFFER_STRIDE: usize = USB_MAX_PACKET_SIZE as usize;

    pub fn new_ep2() -> Option<Self> {
        let dpram = pac::USB_DPRAM;
        let control = dpram.ep_in_control(Self::EP_CONTROL_INDEX).read();
        if !control.enable() || dpram.ep_in_buffer_control(Self::EP_INDEX).read().stall() {
            return None;
        }
        let buffer0_addr = usize::from(control.buffer_address());
        let reserve_addr = usize::from(
            dpram
                .ep_in_control(Self::RESERVE_EP_CONTROL_INDEX)
                .read()
                .buffer_address(),
        );
        if reserve_addr != buffer0_addr + Self::BUFFER_STRIDE {
            if !FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
                warn!("EP2 double-buffer reservation is not adjacent; using slow bulk IN");
            }
            return None;
        }

        dpram.ep_in_control(Self::EP_CONTROL_INDEX).modify(|w| {
            w.set_interrupt_per_buff(true);
            w.set_interrupt_per_double_buff(false);
            w.set_double_buffered(true);
        });
        dpram.ep_in_buffer_control(Self::EP_INDEX).write(|w| {
            w.set_reset(true);
            w.set_pid(0, true);
            w.set_pid(1, false);
        });

        Some(Self {
            buffer0_addr,
            next_buf: 0,
            // DediProg transfers contain whole 512-byte blocks. Successful
            // transfers return to DATA0; a reopened host pipe starts there too.
            next_pid: false,
            reset_pending: true,
            epoch: USB_EPOCH.load(Ordering::SeqCst),
        })
    }

    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), EndpointError> {
        if packet.len() > Self::BUFFER_STRIDE {
            return Err(EndpointError::BufferOverflow);
        }

        let dpram = pac::USB_DPRAM;
        let buf_index = self.next_buf;

        loop {
            if self.epoch != USB_EPOCH.load(Ordering::SeqCst)
                || !dpram.ep_in_control(Self::EP_CONTROL_INDEX).read().enable()
            {
                return Err(EndpointError::Disabled);
            }
            let control = dpram.ep_in_buffer_control(Self::EP_INDEX).read();
            if !control.available(buf_index) && !control.full(buf_index) {
                break;
            }
            embassy_futures::yield_now().await;
        }

        compiler_fence(Ordering::SeqCst);
        // SAFETY: buffer0_addr is the enabled EP2 address allocated by Embassy.
        // Double-buffer mode reserves the adjacent max-packet-sized buffer.
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                (pac::USB_DPRAM.as_ptr() as *mut u8)
                    .add(self.buffer0_addr + buf_index * Self::BUFFER_STRIDE),
                packet.len(),
            )
        };
        dst.copy_from_slice(packet);
        compiler_fence(Ordering::SeqCst);

        if self.epoch != USB_EPOCH.load(Ordering::SeqCst)
            || !dpram.ep_in_control(Self::EP_CONTROL_INDEX).read().enable()
        {
            return Err(EndpointError::Disabled);
        }

        // Both banks have the same length/PID/FULL/AVAILABLE layout. Build a
        // fresh low-half value, then store it only into our released bank.
        // Never RMW the shared word: hardware may complete the other bank.
        // RESET must accompany the first AVAILABLE buffer. A reset-only write
        // with both banks unavailable is not enough: clearing it before AVAIL
        // can leave hardware selecting bank 1 after an abandoned transfer.
        let staged = packet_control(
            packet.len() as u16,
            self.next_pid,
            self.reset_pending,
            false,
        );
        let published =
            packet_control(packet.len() as u16, self.next_pid, self.reset_pending, true);
        let register = dpram
            .ep_in_buffer_control(Self::EP_INDEX)
            .as_ptr()
            .cast::<u32>();
        // SAFETY: AVAILABLE is clear for this bank, checked above. The
        // adjacent bank may still be owned by the USB controller.
        unsafe { write_halfword(register, buf_index, staged) };
        cortex_m::asm::delay(12);
        // SAFETY: publish the same owned half after the required 12 cycles.
        unsafe { write_halfword(register, buf_index, published) };

        self.reset_pending = false;
        self.next_pid = !self.next_pid;
        self.next_buf ^= 1;
        Ok(())
    }

    pub async fn write_block(&mut self, data: &[u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError> {
        for chunk in data.chunks(USB_MAX_PACKET_SIZE as usize) {
            self.write_packet(chunk).await?;
        }
        Ok(())
    }
}

impl Drop for FastBulkIn {
    fn drop(&mut self) {
        // SET_INTERFACE/reset has already initialized a new endpoint epoch.
        // Old-stream cleanup must not rewrite its PID or discard new packets.
        if self.epoch != USB_EPOCH.load(Ordering::SeqCst) {
            return;
        }
        let Some(_abort) = AbortGuard::new(true, Self::EP_INDEX) else {
            return;
        };
        let dpram = pac::USB_DPRAM;
        dpram.ep_in_control(Self::EP_CONTROL_INDEX).modify(|w| {
            w.set_interrupt_per_buff(true);
            w.set_interrupt_per_double_buff(false);
            w.set_double_buffered(false);
        });
        dpram.ep_in_buffer_control(Self::EP_INDEX).write(|w| {
            w.set_reset(true);
            w.set_pid(0, true);
            w.set_pid(1, false);
        });
    }
}
