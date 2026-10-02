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
    /// A failed transport or ambiguous command must be reopened, never retried.
    fn link_failed(&self) -> bool {
        false
    }
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
    failed: bool,
    response: Option<Packet>,
}
const RX_DEPTH: usize = 8;

fn packet(bytes: &[u8]) -> io::Result<Packet> {
    let data = payload(bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid auxiliary packet"))?
        .to_vec();
    Ok(Packet {
        kind: bytes[0],
        id: bytes[1],
        data,
    })
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
        for _ in 0..RX_DEPTH {
            let buffer = rx.allocate(rx.max_packet_size());
            rx.submit(buffer);
        }
        Ok((
            Self {
                rx,
                tx,
                next_id: 1,
                failed: false,
                response: None,
            },
            serial,
        ))
    }
    fn send(
        &mut self,
        kind: u8,
        id: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<()> {
        let mut buffer = [0; PACKET_LEN];
        let bytes = encode(&mut buffer, kind, id, data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "USB payload too large"))?;
        self.tx.submit(bytes.to_vec().into());
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(complete) = self.tx.wait_next_complete(Duration::from_millis(1)) {
                complete.status.map_err(io::Error::from)?;
                if complete.actual_len != bytes.len() {
                    return Err(io::Error::other("short USB write"));
                }
                return Ok(());
            }
            // OUT completion may stall while UART IN remains active. Keep its
            // buffers circulating, bounded so traffic cannot hide the deadline.
            for _ in 0..RX_DEPTH {
                // Preserve the flush barrier: never deliver post-ACK DATA to
                // the callback before request_loop has consumed that ACK.
                if self.response.is_some() {
                    break;
                }
                match self.receive() {
                    Ok(Some(packet)) if packet.kind == EVT_RESPONSE && packet.id == id => {
                        self.response = Some(packet);
                    }
                    Ok(Some(packet)) => events(packet),
                    Ok(None) => break,
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                        events(Packet {
                            kind: EVT_UART_OVERFLOW,
                            id: 0,
                            data: vec![],
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "USB write timeout"));
            }
        }
    }
}
impl Device {
    fn receive(&mut self) -> io::Result<Option<Packet>> {
        let Some(complete) = self.rx.wait_next_complete(Duration::from_millis(1)) else {
            return Ok(None);
        };
        if let Err(error) = complete.status {
            self.failed = true;
            return Err(error.into());
        }
        let result = packet(&complete.buffer[..complete.actual_len]);
        // Even malformed packets retire a receive buffer. Recycle it before
        // reporting the error, but never silently continue a UART session.
        self.rx.submit(complete.buffer);
        result.map(Some)
    }
}
impl DeviceIo for Device {
    fn link_failed(&self) -> bool {
        self.failed
    }
    fn event(&mut self) -> io::Result<Option<Packet>> {
        if let Some(packet) = self.response.take() {
            return Ok(Some(packet));
        }
        self.receive()
    }
    fn request(
        &mut self,
        kind: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<Vec<u8>> {
        let result = request_loop(kind, data, events, self);
        // Rejections are definitive and harmless. Transport errors, missing
        // ACKs and invalid replies leave execution uncertain: reopen the link.
        if result
            .as_ref()
            .is_err_and(|e| e.kind() != io::ErrorKind::Unsupported)
        {
            self.failed = true;
        }
        result
    }
}

trait RequestIo {
    fn send_request(
        &mut self,
        kind: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<u8>;
    fn next_event(&mut self) -> io::Result<Option<Packet>>;
}
impl RequestIo for Device {
    fn send_request(
        &mut self,
        kind: u8,
        data: &[u8],
        events: &mut dyn FnMut(Packet),
    ) -> io::Result<u8> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.send(kind, id, data, events)?;
        Ok(id)
    }
    fn next_event(&mut self) -> io::Result<Option<Packet>> {
        self.event()
    }
}
fn request_loop(
    kind: u8,
    data: &[u8],
    events: &mut dyn FnMut(Packet),
    device: &mut impl RequestIo,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let id = device.send_request(kind, data, events)?;
        let response = loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "auxiliary response timeout",
                ));
            }
            if let Some(packet) = device.next_event()? {
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
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("command {kind:#x} rejected: status {status}"),
                ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Requests {
        calls: usize,
        replies: VecDeque<io::Result<Option<Packet>>>,
    }
    impl RequestIo for Requests {
        fn send_request(&mut self, _: u8, _: &[u8], _: &mut dyn FnMut(Packet)) -> io::Result<u8> {
            self.calls += 1;
            Ok(self.calls as u8)
        }
        fn next_event(&mut self) -> io::Result<Option<Packet>> {
            self.replies.pop_front().expect("unexpected receive")
        }
    }
    fn response(id: u8, status: u8) -> io::Result<Option<Packet>> {
        Ok(Some(Packet {
            kind: EVT_RESPONSE,
            id,
            data: vec![CMD_UART_WRITE, status],
        }))
    }
    #[test]
    fn only_definitive_busy_replies_are_retried() {
        let mut device = Requests {
            calls: 0,
            replies: VecDeque::from([
                response(1, STATUS_BUSY),
                Ok(Some(Packet {
                    kind: EVT_UART_DATA,
                    id: 0,
                    data: vec![42],
                })),
                response(2, STATUS_OK),
            ]),
        };
        let mut received = vec![];
        request_loop(
            CMD_UART_WRITE,
            &[1],
            &mut |p| received.extend(p.data),
            &mut device,
        )
        .unwrap();
        assert_eq!(device.calls, 2);
        assert_eq!(received, [42]);
        for reply in [
            Err(io::ErrorKind::TimedOut.into()),
            Err(io::ErrorKind::ConnectionAborted.into()),
            response(1, STATUS_INVALID),
            Ok(Some(Packet {
                kind: EVT_RESPONSE,
                id: 1,
                data: vec![],
            })),
        ] {
            let mut device = Requests {
                calls: 0,
                replies: VecDeque::from([reply]),
            };
            assert!(request_loop(CMD_UART_WRITE, &[1], &mut |_| {}, &mut device).is_err());
            assert_eq!(
                device.calls, 1,
                "ambiguous/rejected writes must not be replayed"
            );
        }
    }
    #[test]
    fn malformed_packets_are_not_presented_as_data() {
        for bytes in [
            &[][..],
            &[EVT_UART_DATA, 0, 2, 1][..],
            &[EVT_UART_DATA, 0, 62][..],
        ] {
            assert_eq!(
                packet(bytes).err().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let p = packet(&[EVT_UART_DATA, 0, 2, 1, 255]).unwrap();
        assert_eq!(p.data, [1, 255]);
    }
}
