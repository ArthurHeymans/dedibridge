#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

use crate::config::BULK_BLOCK_SIZE;
use embassy_usb::driver::{Endpoint, EndpointAddress, EndpointError, EndpointIn, EndpointOut};

/// Controller-specific cleanup for a cancelled packet future, and endpoint
/// recovery. The ordinary STM32/RP/CH drivers remain Embassy implementations.
pub trait Recovery {
    type Guard;
    /// CH32 treats READ_PROG_INFO as a new host session; Pico historically does not.
    const CANCEL_ON_PROG_INFO: bool = false;
    fn packet_guard(endpoint: EndpointAddress) -> Self::Guard;
    fn complete(_guard: &mut Self::Guard) {}
    /// Controllers that must retire a cancelled auxiliary OUT completion can
    /// preserve its accepted payload for the next read instead of losing it.
    fn take_cancelled_out(
        _endpoint: EndpointAddress,
        _data: &mut [u8],
    ) -> Option<Result<usize, EndpointError>> {
        None
    }
    async fn write_complete(_endpoint: EndpointAddress) -> Result<(), EndpointError> {
        Ok(())
    }
    fn prepare(input: bool, stalled: bool);
    fn reset() {}
}

/// Apply board-specific cancellation and physical completion to auxiliary
/// endpoints too. The guard lives *inside* the packet future, so a timeout or
/// losing a select drops it before another packet can reuse that endpoint.
pub struct GuardedEndpoint<E, R> {
    endpoint: E,
    marker: core::marker::PhantomData<R>,
}
impl<E, R> GuardedEndpoint<E, R> {
    pub fn new(endpoint: E) -> Self {
        Self {
            endpoint,
            marker: core::marker::PhantomData,
        }
    }
}
impl<E: Endpoint, R> Endpoint for GuardedEndpoint<E, R> {
    fn info(&self) -> &embassy_usb::driver::EndpointInfo {
        self.endpoint.info()
    }
    async fn wait_enabled(&mut self) {
        self.endpoint.wait_enabled().await;
    }
}
impl<E: EndpointIn, R: Recovery> EndpointIn for GuardedEndpoint<E, R> {
    async fn write(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        let address = self.endpoint.info().addr;
        let mut guard = R::packet_guard(address);
        self.endpoint.write(data).await?;
        R::write_complete(address).await?;
        R::complete(&mut guard);
        Ok(())
    }
}
impl<E: EndpointOut, R: Recovery> EndpointOut for GuardedEndpoint<E, R> {
    async fn read(&mut self, data: &mut [u8]) -> Result<usize, EndpointError> {
        let address = self.endpoint.info().addr;
        let mut guard = R::packet_guard(address);
        let count = match R::take_cancelled_out(address, data) {
            Some(result) => result?,
            None => self.endpoint.read(data).await?,
        };
        R::complete(&mut guard);
        Ok(count)
    }
}

pub trait BulkIn {
    async fn wait_enabled(&mut self);
    fn start(&mut self) {}
    async fn write_block(&mut self, data: &[u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError>;
    async fn flush(&mut self) -> Result<(), EndpointError> {
        Ok(())
    }
    fn finish(&mut self) {}
}
pub trait BulkOut {
    async fn wait_enabled(&mut self);
    async fn read_block(&mut self, data: &mut [u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError>;
}

pub struct UsbIn<E, R> {
    pub endpoint: E,
    packet_size: usize,
    marker: core::marker::PhantomData<R>,
}
pub struct UsbOut<E, R> {
    pub endpoint: E,
    marker: core::marker::PhantomData<R>,
}
impl<E, R> UsbIn<E, R> {
    pub fn new(endpoint: E, packet_size: u16) -> Self {
        Self {
            endpoint,
            packet_size: packet_size.into(),
            marker: core::marker::PhantomData,
        }
    }
}
impl<E, R> UsbOut<E, R> {
    pub fn new(endpoint: E) -> Self {
        Self {
            endpoint,
            marker: core::marker::PhantomData,
        }
    }
}
impl<E: EndpointIn, R: Recovery> BulkIn for UsbIn<E, R> {
    async fn wait_enabled(&mut self) {
        self.endpoint.wait_enabled().await;
    }
    async fn write_block(&mut self, data: &[u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError> {
        let address = self.endpoint.info().addr;
        let mut guard = R::packet_guard(address);
        for packet in data.chunks(self.packet_size) {
            self.endpoint.write(packet).await?;
        }
        R::write_complete(address).await?;
        R::complete(&mut guard);
        Ok(())
    }
}
impl<E: EndpointOut, R: Recovery> BulkOut for UsbOut<E, R> {
    async fn wait_enabled(&mut self) {
        self.endpoint.wait_enabled().await;
    }
    async fn read_block(&mut self, data: &mut [u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError> {
        let mut guard = R::packet_guard(self.endpoint.info().addr);
        let mut offset = 0;
        while offset < data.len() {
            let count = self.endpoint.read(&mut data[offset..]).await?;
            if count == 0 {
                return Err(EndpointError::BufferOverflow);
            }
            offset += count;
        }
        R::complete(&mut guard);
        Ok(())
    }
}
