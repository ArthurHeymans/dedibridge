use super::*;
use embassy_usb::driver::{Direction, EndpointInfo, EndpointType};
use futures::{executor::block_on, poll};
use std::{
    cell::{Cell, RefCell},
    vec::Vec,
};

std::thread_local! {
    static EVENTS: RefCell<Vec<(EndpointAddress, bool)>> = const { RefCell::new(Vec::new()) };
    static ACK: Cell<bool> = const { Cell::new(false) };
    static CACHED: RefCell<Option<[u8; 4]>> = const { RefCell::new(None) };
}
struct Guard {
    address: EndpointAddress,
    completed: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        EVENTS.with(|events| events.borrow_mut().push((self.address, self.completed)));
    }
}
struct Recover;
impl Recovery for Recover {
    type Guard = Guard;
    fn packet_guard(address: EndpointAddress) -> Guard {
        Guard {
            address,
            completed: false,
        }
    }
    fn complete(guard: &mut Guard) {
        guard.completed = true;
    }
    async fn write_complete(_: EndpointAddress) -> Result<(), EndpointError> {
        core::future::poll_fn(|_| {
            if ACK.with(Cell::get) {
                core::task::Poll::Ready(Ok(()))
            } else {
                core::task::Poll::Pending
            }
        })
        .await
    }
    fn take_cancelled_out(
        address: EndpointAddress,
        data: &mut [u8],
    ) -> Option<Result<usize, EndpointError>> {
        if address.is_in() || address.index() != 3 {
            return None;
        }
        CACHED.with(|cache| {
            let packet = cache.borrow_mut().take()?;
            Some(if data.len() < packet.len() {
                Err(EndpointError::BufferOverflow)
            } else {
                data[..packet.len()].copy_from_slice(&packet);
                Ok(packet.len())
            })
        })
    }
    fn prepare(_: bool, _: bool) {}
    fn reset() {
        CACHED.with(|cache| *cache.borrow_mut() = None);
    }
}
struct Ep(EndpointInfo);
impl Endpoint for Ep {
    fn info(&self) -> &EndpointInfo {
        &self.0
    }
    async fn wait_enabled(&mut self) {}
}
impl EndpointIn for Ep {
    // Models drivers whose write returns after arming, before physical ACK.
    async fn write(&mut self, _: &[u8]) -> Result<(), EndpointError> {
        Ok(())
    }
}
impl EndpointOut for Ep {
    async fn read(&mut self, _: &mut [u8]) -> Result<usize, EndpointError> {
        core::future::pending().await
    }
}
fn ep(index: usize, direction: Direction) -> GuardedEndpoint<Ep, Recover> {
    GuardedEndpoint::new(Ep(EndpointInfo {
        addr: EndpointAddress::from_parts(index, direction),
        ep_type: EndpointType::Bulk,
        max_packet_size: 512,
        interval_ms: 0,
    }))
}
#[test]
fn accepted_cancelled_auxiliary_out_is_consumed_once_and_reset_discards_cache() {
    CACHED.with(|cache| *cache.borrow_mut() = Some([0x10, 3, 1, 0xff]));
    let mut output = ep(3, Direction::Out);
    let mut bytes = [0; 512];
    assert_eq!(block_on(output.read(&mut bytes)), Ok(4));
    assert_eq!(&bytes[..4], &[0x10, 3, 1, 0xff]);
    block_on(async {
        let mut second = core::pin::pin!(output.read(&mut bytes));
        assert!(
            poll!(second.as_mut()).is_pending(),
            "cached request was replayed twice"
        );
    });
    CACHED.with(|cache| *cache.borrow_mut() = Some([0x10, 4, 1, 0xee]));
    Recover::reset();
    block_on(async {
        let mut after_reset = core::pin::pin!(output.read(&mut bytes));
        assert!(
            poll!(after_reset.as_mut()).is_pending(),
            "old USB session request survived reset"
        );
    });
}
#[test]
fn auxiliary_cancel_recovers_correct_endpoint_and_success_waits_for_final_ack() {
    EVENTS.with(|events| events.borrow_mut().clear());
    ACK.with(|ack| ack.set(false));
    let mut input = ep(4, Direction::In);
    block_on(async {
        {
            let mut packet = core::pin::pin!(input.write(&[0x81, 1, 0]));
            assert!(poll!(packet.as_mut()).is_pending());
            // Timeout/select dropping the future must recover EP4, not EP2.
        }
        assert_eq!(
            EVENTS.with(|events| events.borrow()[0]),
            (EndpointAddress::from_parts(4, Direction::In), false)
        );
        let mut packet = core::pin::pin!(input.write(&[0x81, 2, 0]));
        assert!(poll!(packet.as_mut()).is_pending());
        ACK.with(|ack| ack.set(true));
        assert!(poll!(packet.as_mut()).is_ready());
    });
    assert!(
        EVENTS.with(|events| events.borrow()[1].1),
        "completed packets must not be cancelled/discarded"
    );
    let mut output = ep(3, Direction::Out);
    let mut bytes = [0; 512];
    block_on(async {
        let mut packet = core::pin::pin!(output.read(&mut bytes));
        assert!(poll!(packet.as_mut()).is_pending());
    });
    assert_eq!(
        EVENTS.with(|events| events.borrow()[2]),
        (EndpointAddress::from_parts(3, Direction::Out), false)
    );
}
