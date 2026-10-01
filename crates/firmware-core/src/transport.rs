use crate::config::BULK_BLOCK_SIZE;
use embassy_usb::driver::{EndpointError, EndpointIn, EndpointOut};

/// Controller-specific cleanup for a cancelled packet future, and endpoint
/// recovery. The ordinary STM32/RP/CH drivers remain Embassy implementations.
pub trait Recovery {
    type Guard;
    fn packet_guard(input: bool) -> Self::Guard;
    fn complete(_guard: &mut Self::Guard) {}
    async fn write_complete() -> Result<(), EndpointError> {
        Ok(())
    }
    fn prepare(input: bool, stalled: bool);
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
        let mut guard = R::packet_guard(true);
        for packet in data.chunks(self.packet_size) {
            self.endpoint.write(packet).await?;
        }
        R::write_complete().await?;
        R::complete(&mut guard);
        Ok(())
    }
}
impl<E: EndpointOut, R: Recovery> BulkOut for UsbOut<E, R> {
    async fn wait_enabled(&mut self) {
        self.endpoint.wait_enabled().await;
    }
    async fn read_block(&mut self, data: &mut [u8; BULK_BLOCK_SIZE]) -> Result<(), EndpointError> {
        let mut guard = R::packet_guard(false);
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
