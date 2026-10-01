use crate::wire::Info;
use dedi_protocol::{AUX_IN, AUX_INTERFACE, AUX_OUT, USB_PID, USB_VID, aux::*};
use nusb::{
    Endpoint, MaybeFuture,
    transfer::{Bulk, In, Out},
};
use std::{
    io,
    time::{Duration, Instant},
};
use zerocopy::FromBytes;

pub struct Packet {
    pub kind: u8,
    pub id: u8,
    pub data: Vec<u8>,
}
pub trait DeviceIo {
    fn request(
        &mut self,
        kind: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<Vec<u8>>;
    fn event(&mut self) -> io::Result<Option<Packet>>;
}
pub struct Device {
    rx: Endpoint<Bulk, In>,
    tx: Endpoint<Bulk, Out>,
    next_id: u8,
}
fn usb_error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
pub fn devices() -> io::Result<Vec<nusb::DeviceInfo>> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(usb_error)?
        .filter(|d| d.vendor_id() == USB_VID && d.product_id() == USB_PID)
        .collect())
}
impl Device {
    pub fn open(serial: Option<&str>) -> io::Result<(Self, String)> {
        let devices = devices()?
            .into_iter()
            .filter(|d| serial.is_none_or(|s| d.serial_number() == Some(s)))
            .collect::<Vec<_>>();
        if devices.len() != 1 {
            return Err(io::Error::other(format!(
                "matched {} devices; select exactly one with --serial (use `list`)",
                devices.len()
            )));
        }
        let info = &devices[0];
        let serial = info
            .serial_number()
            .ok_or_else(|| io::Error::other("device has no stable USB serial"))?
            .to_owned();
        let device = info.open().wait().map_err(usb_error)?;
        let config = device.active_configuration().map_err(usb_error)?;
        let has_aux = config.interface_alt_settings().any(|i| {
            i.interface_number() == AUX_INTERFACE
                && i.class() == 0xff
                && i.subclass() == 0xd1
                && i.protocol() == 1
        });
        if !has_aux {
            return Err(io::Error::other(
                "device has no DediBridge auxiliary interface",
            ));
        }
        let interface = device
            .claim_interface(AUX_INTERFACE)
            .wait()
            .map_err(usb_error)?;
        let tx = interface
            .endpoint::<Bulk, Out>(AUX_OUT)
            .map_err(usb_error)?;
        let mut rx = interface.endpoint::<Bulk, In>(AUX_IN).map_err(usb_error)?;
        for _ in 0..2 {
            let buffer = rx.allocate(rx.max_packet_size());
            rx.submit(buffer);
        }
        Ok((Self { rx, tx, next_id: 1 }, serial))
    }
    fn send(&mut self, kind: u8, id: u8, data: &[u8]) -> io::Result<()> {
        let mut buffer = [0; PACKET_LEN];
        let bytes = encode(&mut buffer, kind, id, data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "USB payload too large"))?;
        self.tx.submit(bytes.to_vec().into());
        let complete = self
            .tx
            .wait_next_complete(Duration::from_secs(1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "USB write timeout"))?;
        complete.status.map_err(usb_error)?;
        if complete.actual_len != bytes.len() {
            return Err(io::Error::other("short USB write"));
        }
        Ok(())
    }
}
impl DeviceIo for Device {
    fn event(&mut self) -> io::Result<Option<Packet>> {
        let Some(complete) = self.rx.wait_next_complete(Duration::from_millis(1)) else {
            return Ok(None);
        };
        complete.status.map_err(usb_error)?;
        let bytes = &complete.buffer[..complete.actual_len];
        let data = payload(bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid auxiliary packet"))?
            .to_vec();
        let packet = Packet {
            kind: bytes[0],
            id: bytes[1],
            data,
        };
        self.rx.submit(complete.buffer);
        Ok(Some(packet))
    }
    fn request(
        &mut self,
        kind: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.send(kind, id, data)?;
            let response = loop {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "auxiliary response timeout",
                    ));
                }
                if let Some(packet) = self.event()? {
                    if packet.kind == EVT_RESPONSE && packet.id == id {
                        break packet.data;
                    }
                    events(packet);
                }
            };
            if response.len() < 2 || response[0] != kind {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed auxiliary response",
                ));
            }
            match response[1] {
                STATUS_OK => return Ok(response[2..].to_vec()),
                STATUS_BUSY => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "UART queue remained full",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                status => {
                    return Err(io::Error::other(format!(
                        "command {kind:#x} rejected: status {status}"
                    )));
                }
            }
        }
    }
}
pub fn parse_info(data: &[u8]) -> io::Result<Info> {
    let info = DeviceInfo::read_from_bytes(data)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid device capabilities"))?;
    if info.version != VERSION {
        return Err(io::Error::other(format!(
            "unsupported auxiliary protocol version {}",
            info.version
        )));
    }
    Ok(Info {
        version: info.version,
        board: info.board,
        io_modes: info.io_modes,
        gpio_outputs: info.gpio_outputs,
        gpio_inputs: info.gpio_inputs,
        flags: info.flags,
        max_baud: info.max_baud.get(),
    })
}
