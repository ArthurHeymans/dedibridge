use crate::{
    config::{BULK_BLOCK_SIZE, PAGE_SIZE},
    diagnostics::ACTIVITY,
    flash::{Error, Flash},
    gpio::LedControl,
    protocol::BulkOperation,
    transport::{BulkIn, BulkOut, Recovery},
};
use core::cell::RefCell;
use core::future::Future;
use critical_section::Mutex;
use embassy_futures::{
    join::join,
    select::{Either, select},
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal, zerocopy_channel::Channel,
};
use embassy_time::{Duration, with_timeout};
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Clone, Copy)]
struct Block {
    bytes: [u8; BULK_BLOCK_SIZE],
    end: bool,
}

struct Queued {
    operation: BulkOperation,
    generation: u32,
}
#[derive(Default)]
struct Queue {
    pending: Option<Queued>,
    active: bool,
}
impl Queue {
    fn submit(&mut self, operation: BulkOperation, generation: u32) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(Queued {
            operation,
            generation,
        });
        true
    }
    fn take(&mut self) -> Option<Queued> {
        let op = self.pending.take();
        self.active = op.is_some();
        op
    }
    fn discard(&mut self, generation: u32, keep_read: bool) {
        if self.pending.as_ref().is_some_and(|p| {
            p.generation == generation
                && !(keep_read && matches!(p.operation, BulkOperation::Read { .. }))
        }) {
            self.pending = None;
        }
    }
}

pub struct Shared<F, L, R> {
    flash: Mutex<RefCell<Option<F>>>,
    leds: Mutex<RefCell<(L, u8)>>,
    queue: Mutex<RefCell<Queue>>,
    wake: Signal<CriticalSectionRawMutex, ()>,
    cancel: Signal<CriticalSectionRawMutex, ()>,
    generation: AtomicU32,
    marker: core::marker::PhantomData<R>,
}
impl<F: Flash, L: LedControl, R: Recovery> Shared<F, L, R> {
    pub fn new(flash: F, leds: L) -> Self {
        Self {
            flash: Mutex::new(RefCell::new(Some(flash))),
            leds: Mutex::new(RefCell::new((leds, 0))),
            queue: Mutex::new(RefCell::new(Queue::default())),
            wake: Signal::new(),
            cancel: Signal::new(),
            generation: AtomicU32::new(0),
            marker: core::marker::PhantomData,
        }
    }
    pub fn control<T>(&self, f: impl FnOnce(&mut F) -> T) -> Option<T> {
        let mut flash = critical_section::with(|cs| {
            let q = self.queue.borrow(cs).borrow();
            if q.active || q.pending.is_some() {
                return None;
            }
            self.flash.borrow(cs).borrow_mut().take()
        })?;
        // No await: the cooperative bulk worker cannot run while control owns
        // the bus. IRQs must stay enabled during blocking SPI, especially for
        // buffered UART RX. The mutex protects ownership, not the entire I/O.
        let result = f(&mut flash);
        critical_section::with(|cs| *self.flash.borrow(cs).borrow_mut() = Some(flash));
        Some(result)
    }
    pub fn set_leds(&self, mask: u8) {
        critical_section::with(|cs| {
            let mut leds = self.leds.borrow(cs).borrow_mut();
            leds.1 = mask;
            leds.0.set(mask);
        });
    }
    fn set_error(&self) {
        critical_section::with(|cs| {
            let mut leds = self.leds.borrow(cs).borrow_mut();
            let mask = leds.1 | 4;
            leds.1 = mask;
            leds.0.set(mask);
        });
    }
    pub fn submit(&self, operation: BulkOperation) -> bool {
        let input = matches!(operation, BulkOperation::Read { .. });
        let accepted = critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            let idle = !queue.active;
            let accepted = queue.submit(operation, self.generation.load(Ordering::SeqCst));
            if accepted && idle {
                R::prepare(input, false);
            }
            accepted
        });
        if accepted {
            self.wake.signal(());
        }
        accepted
    }
    pub fn cancel(&self) {
        ACTIVITY.cancellations.increment();
        let old = self.generation.fetch_add(1, Ordering::SeqCst);
        critical_section::with(|cs| self.queue.borrow(cs).borrow_mut().discard(old, false));
        // Release a manually selected bus too, but never touch an active DMA owner.
        self.control(|flash| flash.end_transfer());
        self.cancel.signal(());
        self.wake.signal(());
    }
    fn cancelled(&self, generation: u32) -> bool {
        self.generation.load(Ordering::SeqCst) != generation
    }
    async fn usb<T>(&self, generation: u32, future: impl Future<Output = T>) -> Result<T, ()> {
        if self.cancelled(generation) {
            return Err(());
        }
        match with_timeout(
            Duration::from_millis(3500),
            select(future, self.cancel.wait()),
        )
        .await
        {
            Ok(Either::First(result)) if !self.cancelled(generation) => Ok(result),
            Err(_) => {
                ACTIVITY.bulk_usb_timeouts.increment();
                Err(())
            }
            _ => Err(()),
        }
    }

    /// Two software buffers overlap USB with flash DMA. Errors drain the channel
    /// without doing more I/O so neither half of the pipeline can deadlock.
    pub async fn run(&self, mut input: impl BulkIn, mut output: impl BulkOut) -> ! {
        loop {
            self.wake.wait().await;
            loop {
                let queued = critical_section::with(|cs| self.queue.borrow(cs).borrow_mut().take());
                let Some(Queued {
                    operation,
                    generation,
                }) = queued
                else {
                    break;
                };
                self.cancel.reset();
                let flash = critical_section::with(|cs| self.flash.borrow(cs).borrow_mut().take());
                let Some(mut flash) = flash else {
                    critical_section::with(|cs| self.queue.borrow(cs).borrow_mut().active = false);
                    continue;
                };
                let mut buffers = [Block {
                    bytes: [0; BULK_BLOCK_SIZE],
                    end: false,
                }; 2];
                let mut channel = Channel::<CriticalSectionRawMutex, Block>::new(&mut buffers);
                let (mut sender, mut receiver) = channel.split();
                let failed = AtomicBool::new(self.cancelled(generation));
                let mut keep_verify = false;
                let is_read = matches!(operation, BulkOperation::Read { .. });
                match operation {
                    BulkOperation::Read {
                        address,
                        block_count,
                        opcode,
                        addr_len,
                        io_mode,
                        mode_byte,
                        dummy_cycles,
                    } => {
                        let enabled = self.usb(generation, input.wait_enabled()).await;
                        let started = if enabled.is_ok() {
                            ACTIVITY.flash_result(
                                flash
                                    .start_read(
                                        opcode,
                                        address,
                                        addr_len,
                                        io_mode,
                                        mode_byte,
                                        dummy_cycles,
                                        || self.cancelled(generation),
                                    )
                                    .await,
                            )
                        } else {
                            Err(Error::Cancelled)
                        };
                        if started.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        if !failed.load(Ordering::Relaxed) {
                            input.start();
                        }
                        join(
                            async {
                                for _ in 0..block_count {
                                    if self.cancelled(generation) || failed.load(Ordering::Relaxed)
                                    {
                                        failed.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                    let slot = sender.send().await;
                                    slot.end = false;
                                    if ACTIVITY
                                        .flash_result(
                                            flash.read_block(&mut slot.bytes, io_mode).await,
                                        )
                                        .is_err()
                                    {
                                        failed.store(true, Ordering::Relaxed);
                                    }
                                    sender.send_done();
                                }
                                sender.send().await.end = true;
                                sender.send_done();
                            },
                            async {
                                for _ in 0..block_count {
                                    let slot = receiver.receive().await;
                                    if slot.end {
                                        receiver.receive_done();
                                        break;
                                    }
                                    if !failed.load(Ordering::Relaxed)
                                        && !matches!(
                                            self.usb(generation, input.write_block(&slot.bytes))
                                                .await,
                                            Ok(Ok(()))
                                        )
                                    {
                                        failed.store(true, Ordering::Relaxed);
                                    }
                                    receiver.receive_done();
                                }
                            },
                        )
                        .await;
                        if !failed.load(Ordering::Relaxed)
                            && !matches!(self.usb(generation, input.flush()).await, Ok(Ok(())))
                        {
                            failed.store(true, Ordering::Relaxed);
                        }
                        input.finish();
                    }
                    BulkOperation::Write {
                        address,
                        block_count,
                        opcode,
                        addr_len,
                    } => {
                        if self.usb(generation, output.wait_enabled()).await.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        let usb_failed = AtomicBool::new(false);
                        join(
                            async {
                                for _ in 0..block_count {
                                    if self.cancelled(generation) || failed.load(Ordering::Relaxed)
                                    {
                                        failed.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                    let slot = sender.send().await;
                                    slot.end = false;
                                    if !matches!(
                                        self.usb(generation, output.read_block(&mut slot.bytes))
                                            .await,
                                        Ok(Ok(()))
                                    ) {
                                        failed.store(true, Ordering::Relaxed);
                                        usb_failed.store(true, Ordering::Relaxed);
                                    }
                                    sender.send_done();
                                }
                                sender.send().await.end = true;
                                sender.send_done();
                            },
                            async {
                                for page in 0..block_count {
                                    let slot = receiver.receive().await;
                                    if slot.end {
                                        receiver.receive_done();
                                        break;
                                    }
                                    if !failed.load(Ordering::Relaxed) {
                                        let result = flash
                                            .write_page(
                                                opcode,
                                                address.wrapping_add(
                                                    u32::from(page) * PAGE_SIZE as u32,
                                                ),
                                                addr_len,
                                                &slot.bytes[..PAGE_SIZE],
                                                || self.cancelled(generation),
                                            )
                                            .await;
                                        if let Err(error) = ACTIVITY.flash_result(result) {
                                            keep_verify = error != Error::Cancelled;
                                            failed.store(true, Ordering::Relaxed);
                                        }
                                    }
                                    receiver.receive_done();
                                }
                            },
                        )
                        .await;
                        keep_verify &= !usb_failed.load(Ordering::Relaxed);
                        if !failed.load(Ordering::Relaxed) {
                            embassy_time::Timer::after_millis(25).await;
                        }
                    }
                }
                flash.end_transfer();
                critical_section::with(|cs| *self.flash.borrow(cs).borrow_mut() = Some(flash));
                if failed.load(Ordering::Relaxed) {
                    ACTIVITY.bulk_failed.increment();
                    R::prepare(is_read, true);
                    self.set_error();
                } else {
                    ACTIVITY.bulk_completed.increment();
                }
                critical_section::with(|cs| {
                    let mut q = self.queue.borrow(cs).borrow_mut();
                    if failed.load(Ordering::Relaxed) {
                        q.discard(generation, keep_verify);
                    }
                    q.active = false;
                    if let Some(pending) = q.pending.as_ref() {
                        R::prepare(
                            matches!(pending.operation, BulkOperation::Read { .. }),
                            false,
                        );
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write() -> BulkOperation {
        BulkOperation::Write {
            address: 0,
            block_count: 1,
            opcode: 2,
            addr_len: 3,
        }
    }
    #[test]
    fn one_follow_up_and_generation_scoped_cancellation() {
        let mut q = Queue::default();
        assert!(q.submit(write(), 0));
        assert!(!q.submit(write(), 0));
        assert!(q.take().is_some());
        assert!(q.submit(write(), 1));
        q.discard(0, false);
        assert!(q.pending.is_some());
        q.discard(1, false);
        assert!(q.pending.is_none());
    }

    struct MockFlash<const MODES: u8 = 1>(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl<const MODES: u8> Flash for MockFlash<MODES> {
        const IO_MODES: u8 = MODES;
        fn set_frequency(&mut self, _: u32) -> Result<(), Error> {
            Ok(())
        }
        fn select(&mut self, selected: bool) {
            if !selected {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn transceive(&mut self, _: &[u8], _: &mut [u8]) -> Result<(), Error> {
            Ok(())
        }
        async fn start_read(
            &mut self,
            _: u8,
            _: u32,
            _: u8,
            _: crate::protocol::IoMode,
            _: Option<u8>,
            _: u8,
            _: impl Fn() -> bool,
        ) -> Result<(), Error> {
            Ok(())
        }
        async fn read_block(
            &mut self,
            data: &mut [u8],
            _: crate::protocol::IoMode,
        ) -> Result<(), Error> {
            data.fill(0x42);
            Ok(())
        }
        async fn write_page(
            &mut self,
            _: u8,
            _: u32,
            _: u8,
            _: &[u8],
            _: impl Fn() -> bool,
        ) -> Result<(), Error> {
            Ok(())
        }
    }
    #[derive(Default, Clone)]
    struct NoLeds(std::rc::Rc<std::cell::Cell<u8>>);
    impl LedControl for NoLeds {
        fn set(&mut self, mask: u8) {
            self.0.set(mask);
        }
    }
    struct MockRecovery;
    impl Recovery for MockRecovery {
        type Guard = ();
        const CANCEL_ON_PROG_INFO: bool = true;
        fn packet_guard(_: embassy_usb::driver::EndpointAddress) {}
        fn prepare(_: bool, _: bool) {}
    }
    struct StalledIn;
    impl BulkIn for StalledIn {
        async fn wait_enabled(&mut self) {}
        async fn write_block(
            &mut self,
            _: &[u8; 512],
        ) -> Result<(), embassy_usb::driver::EndpointError> {
            core::future::pending().await
        }
    }
    struct UnusedOut;
    impl BulkOut for UnusedOut {
        async fn wait_enabled(&mut self) {}
        async fn read_block(
            &mut self,
            _: &mut [u8; 512],
        ) -> Result<(), embassy_usb::driver::EndpointError> {
            panic!("read operation used OUT")
        }
    }
    fn request(command: u8, value: u16) -> embassy_usb::control::Request {
        use embassy_usb::{
            control::{Recipient, RequestType},
            driver::Direction,
        };
        embassy_usb::control::Request {
            direction: Direction::In,
            request_type: RequestType::Vendor,
            recipient: Recipient::Device,
            request: command,
            value,
            index: 0,
            length: 16,
        }
    }
    #[test]
    fn qpi_dispatch_retains_experimental_reads_but_rejects_single_lane_boards() {
        use embassy_usb::{Handler, control::OutResponse};
        let identity = dedi_protocol::identity::DeviceIdentity::from_unique_id([0; 8]);
        let pico = Shared::<_, _, MockRecovery>::new(
            MockFlash::<0x3f>(Default::default()),
            NoLeds::default(),
        );
        let single = Shared::<_, _, MockRecovery>::new(
            MockFlash::<1>(Default::default()),
            NoLeds::default(),
        );
        let mut handler = crate::handler::DediprogHandler::new(&pico, &identity);
        assert!(matches!(
            handler.control_out(request(crate::protocol::CMD_IO_MODE, 5), &[]),
            Some(OutResponse::Accepted)
        ));
        let setup = [1, 0, 0, 9, 0x03, 0, 0, 0, 0, 0, 3, 1];
        assert!(matches!(
            handler.control_out(request(crate::protocol::CMD_READ, 0), &setup),
            Some(OutResponse::Accepted)
        ));
        critical_section::with(|cs| {
            assert!(matches!(
                pico.queue
                    .borrow(cs)
                    .borrow()
                    .pending
                    .as_ref()
                    .unwrap()
                    .operation,
                BulkOperation::Read {
                    io_mode: crate::protocol::IoMode::Qpi,
                    ..
                }
            ))
        });
        let mut handler = crate::handler::DediprogHandler::new(&single, &identity);
        assert!(matches!(
            handler.control_out(request(crate::protocol::CMD_IO_MODE, 5), &[]),
            Some(OutResponse::Rejected)
        ));
    }
    struct StalledOut;
    impl BulkOut for StalledOut {
        async fn wait_enabled(&mut self) {}
        async fn read_block(
            &mut self,
            _: &mut [u8; 512],
        ) -> Result<(), embassy_usb::driver::EndpointError> {
            core::future::pending().await
        }
    }
    #[test]
    fn fresh_session_cancels_abandoned_out_and_old_follow_up_before_spi_probe() {
        use embassy_usb::{
            Handler,
            control::{InResponse, OutResponse},
        };
        use futures::{executor::block_on, poll};
        let shared = Shared::<_, _, MockRecovery>::new(
            MockFlash::<1>(Default::default()),
            NoLeds::default(),
        );
        let identity = dedi_protocol::identity::DeviceIdentity::from_unique_id([0; 8]);
        let mut handler = crate::handler::DediprogHandler::new(&shared, &identity);
        assert!(shared.submit(write()));
        block_on(async {
            let mut worker = std::boxed::Box::pin(shared.run(StalledIn, StalledOut));
            assert!(poll!(worker.as_mut()).is_pending());
            assert!(shared.submit(write()));
            let mut buf = [0; 16];
            assert!(matches!(
                handler.control_in(request(crate::protocol::CMD_READ_PROG_INFO, 0), &mut buf),
                Some(InResponse::Accepted(_))
            ));
            for _ in 0..8 {
                assert!(poll!(worker.as_mut()).is_pending());
                if shared.control(|_| ()).is_some() {
                    assert!(matches!(
                        handler.control_out(request(crate::protocol::CMD_TRANSCEIVE, 1), &[0x9f]),
                        Some(OutResponse::Accepted)
                    ));
                    return;
                }
            }
            panic!("old OUT operation retained ownership");
        });
    }
    struct Packets {
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }
    impl BulkIn for Packets {
        async fn wait_enabled(&mut self) {}
        async fn write_block(
            &mut self,
            bytes: &[u8; 512],
        ) -> Result<(), embassy_usb::driver::EndpointError> {
            assert_eq!(bytes, &[0x42; 512]);
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                Err(embassy_usb::driver::EndpointError::Disabled)
            } else {
                Ok(())
            }
        }
    }
    impl BulkOut for Packets {
        async fn wait_enabled(&mut self) {}
        async fn read_block(
            &mut self,
            bytes: &mut [u8; 512],
        ) -> Result<(), embassy_usb::driver::EndpointError> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            bytes.fill(0x42);
            if self.fail {
                Err(embassy_usb::driver::EndpointError::Disabled)
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn pipeline_end_marker_handles_normal_and_failed_in_and_out_without_extra_packets() {
        use futures::executor::block_on;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for read in [false, true] {
            for fail in [false, true] {
                let count = Arc::new(AtomicUsize::new(0));
                let leds = NoLeds::default();
                let shared = Shared::<_, _, MockRecovery>::new(
                    MockFlash::<1>(Default::default()),
                    leds.clone(),
                );
                shared.set_leds(3);
                assert!(shared.submit(if read {
                    BulkOperation::Read {
                        address: 0,
                        block_count: 3,
                        opcode: 3,
                        addr_len: 3,
                        io_mode: crate::protocol::IoMode::Single,
                        mode_byte: None,
                        dummy_cycles: 0,
                    }
                } else {
                    BulkOperation::Write {
                        address: 0,
                        block_count: 3,
                        opcode: 2,
                        addr_len: 3,
                    }
                }));
                block_on(async {
                    let input = Packets {
                        count: count.clone(),
                        fail,
                    };
                    let output = Packets {
                        count: count.clone(),
                        fail,
                    };
                    let finished = async {
                        loop {
                            if shared.control(|_| ()).is_some() {
                                break;
                            }
                            embassy_time::Timer::after_millis(1).await;
                        }
                    };
                    assert!(matches!(
                        embassy_time::with_timeout(
                            Duration::from_secs(1),
                            select(shared.run(input, output), finished)
                        )
                        .await,
                        Ok(Either::Second(()))
                    ));
                });
                assert_eq!(count.load(Ordering::SeqCst), if fail { 1 } else { 3 });
                assert_eq!(leds.0.get(), if fail { 7 } else { 3 });
            }
        }
    }
    #[test]
    fn blocking_control_keeps_exclusive_ownership_without_holding_a_borrow() {
        let shared = Shared::<_, _, MockRecovery>::new(
            MockFlash::<1>(Default::default()),
            NoLeds::default(),
        );
        assert_eq!(shared.control(|_| shared.control(|_| 1)), Some(None));
        assert_eq!(shared.control(|_| 2), Some(2));
    }
    #[test]
    fn cancellation_of_stalled_usb_drains_pipeline_and_returns_bus() {
        use futures::{executor::block_on, poll};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let released = Arc::new(AtomicUsize::new(0));
        let leds = NoLeds::default();
        let shared =
            Shared::<_, _, MockRecovery>::new(MockFlash::<1>(released.clone()), leds.clone());
        shared.set_leds(3);
        assert!(shared.submit(BulkOperation::Read {
            address: 0,
            block_count: u16::MAX,
            opcode: 3,
            addr_len: 3,
            io_mode: crate::protocol::IoMode::Single,
            mode_byte: None,
            dummy_cycles: 0
        }));
        block_on(async {
            let mut worker = std::boxed::Box::pin(shared.run(StalledIn, UnusedOut));
            assert!(poll!(worker.as_mut()).is_pending());
            assert!(shared.control(|_| ()).is_none());
            let identity = dedi_protocol::identity::DeviceIdentity::from_unique_id([0; 8]);
            let mut handler = crate::handler::DediprogHandler::new(&shared, &identity);
            let mut response = [0; 16];
            assert!(matches!(
                embassy_usb::Handler::control_in(
                    &mut handler,
                    request(crate::protocol::CMD_READ_PROG_INFO, 0),
                    &mut response
                ),
                Some(embassy_usb::control::InResponse::Accepted(_))
            ));
            // Join polls each half once; consuming slots wakes the producer
            // for the next executor turn rather than recursively polling it.
            for _ in 0..8 {
                assert!(poll!(worker.as_mut()).is_pending());
                if shared.control(|_| ()).is_some() {
                    assert_eq!(released.load(Ordering::SeqCst), 1);
                    assert_eq!(leds.0.get(), 7, "failure must preserve host PASS/BUSY");
                    return;
                }
            }
            panic!("cancelled pipeline did not return flash ownership");
        });
    }
}
