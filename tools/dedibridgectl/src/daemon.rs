use crate::{
    device::{DeviceIo, Packet, parse_info},
    wire::{self, Info, Request, Response},
};
use dedi_protocol::aux::*;
use std::{
    io::{self, BufReader},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

#[derive(Clone)]
pub struct Sink {
    pub tx: SyncSender<Response>,
    pub failed: Arc<AtomicBool>,
}
struct Session {
    token: u64,
    sink: Sink,
}
pub enum Action {
    Control(Request),
    Attach { token: u64, baud: u32, sink: Sink },
    Write { token: u64, data: Vec<u8> },
    Flush { token: u64 },
    Detach { token: u64 },
}
struct Message {
    action: Action,
    reply: SyncSender<Response>,
}
#[derive(Clone)]
pub struct Actor {
    tx: SyncSender<Message>,
}
impl Actor {
    pub fn call(&self, action: Action) -> io::Result<Response> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Message { action, reply })
            .map_err(|_| io::Error::other("USB actor stopped"))?;
        rx.recv()
            .map_err(|_| io::Error::other("USB actor stopped"))?
            .check()
    }
}

fn event(session: &mut Option<Session>, packet: Packet) {
    let Some(active) = session.as_mut() else {
        return;
    };
    let response = match packet.kind {
        EVT_UART_DATA => Response::Data {
            data: wire::encode(&packet.data),
        },
        EVT_UART_OVERFLOW => {
            Response::error("UART bytes were lost (device overflow or UART error)")
        }
        _ => return,
    };
    let fatal = matches!(response, Response::Error { .. });
    if fatal || active.sink.tx.try_send(response).is_err() {
        active.sink.failed.store(true, Ordering::SeqCst);
        // Keep the lease until the socket closes; never let another client
        // configure baud while a failed client's queued writes still exist.
    }
}

fn request(
    device: &mut impl DeviceIo,
    session: &mut Option<Session>,
    kind: u8,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    device.request(kind, data, &mut |packet| event(session, packet))
}
fn gpio_state(data: Vec<u8>) -> io::Result<Response> {
    let [inputs, outputs, directions, caps] = data.as_slice() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid GPIO state",
        ));
    };
    Ok(Response::State {
        inputs: *inputs,
        outputs: *outputs,
        directions: *directions,
        caps: *caps,
    })
}
fn owns(session: &Option<Session>, token: u64) -> io::Result<()> {
    if session
        .as_ref()
        .is_some_and(|s| s.token == token && !s.sink.failed.load(Ordering::SeqCst))
    {
        Ok(())
    } else {
        Err(io::Error::other("serial session is closed or failed"))
    }
}
fn execute(
    device: &mut impl DeviceIo,
    session: &mut Option<Session>,
    action: Action,
    serial: &str,
    info: &Info,
) -> io::Result<Response> {
    match action {
        Action::Attach { token, baud, sink } => {
            if session.is_some() {
                return Err(io::Error::other("serial UART already has a client"));
            }
            if !(300..=info.max_baud).contains(&baud) {
                return Err(io::Error::other("baud outside device range"));
            }
            request(device, session, CMD_UART_SET_BAUD, &baud.to_le_bytes())?;
            request(device, session, CMD_UART_FLUSH_RX, &[])?;
            *session = Some(Session { token, sink });
            Ok(Response::Ready {
                version: wire::VERSION,
                serial: serial.into(),
                info: info.clone(),
            })
        }
        Action::Write { token, data } => {
            owns(session, token)?;
            for chunk in data.chunks(MAX_PAYLOAD_LEN) {
                owns(session, token)?;
                request(device, session, CMD_UART_WRITE, chunk)?;
            }
            owns(session, token)?;
            Ok(Response::Written)
        }
        Action::Flush { token } => {
            owns(session, token)?;
            request(device, session, CMD_UART_FLUSH_RX, &[])?;
            Ok(Response::ResetInput)
        }
        Action::Detach { token } => {
            if session.as_ref().is_some_and(|s| s.token == token) {
                *session = None;
            }
            Ok(Response::Written)
        }
        Action::Control(command) => {
            let output_mask = |mask: u8| mask != 0 && mask & !info.gpio_outputs == 0;
            let (kind, data) = match command {
                Request::Info => {
                    return Ok(Response::Info {
                        serial: serial.into(),
                        info: info.clone(),
                    });
                }
                Request::State => (CMD_GPIO_GET_STATE, vec![]),
                Request::Pulse { mask, ms } if output_mask(mask) => {
                    (CMD_GPIO_PULSE_LOW, vec![mask, ms as u8, (ms >> 8) as u8])
                }
                Request::Direction { mask, values } if output_mask(mask) => {
                    (CMD_GPIO_SET_DIRECTION, vec![mask, values])
                }
                Request::Set { mask, values } if output_mask(mask) => {
                    // One actor operation: no other client's GPIO command can
                    // interleave the original direction-then-output sequence.
                    gpio_state(request(
                        device,
                        session,
                        CMD_GPIO_SET_DIRECTION,
                        &[mask, mask],
                    )?)?;
                    (CMD_GPIO_SET_OUTPUT, vec![mask, values])
                }
                Request::Output { mask, values } if output_mask(mask) => {
                    (CMD_GPIO_SET_OUTPUT, vec![mask, values])
                }
                _ => {
                    return Err(io::Error::other(
                        "invalid control request or unsupported GPIO",
                    ));
                }
            };
            gpio_state(request(device, session, kind, &data)?)
        }
    }
}

pub fn spawn(mut device: impl DeviceIo + Send + 'static, serial: String) -> io::Result<Actor> {
    let info = parse_info(&device.request(CMD_GET_INFO, &[], &mut |_| {})?)?;
    let (tx, rx) = mpsc::sync_channel::<Message>(16);
    thread::spawn(move || {
        let mut session = None;
        loop {
            match rx.try_recv() {
                Ok(message) => {
                    let response =
                        execute(&mut device, &mut session, message.action, &serial, &info)
                            .unwrap_or_else(Response::error);
                    let _ = message.reply.send(response);
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match device.event() {
                Ok(Some(packet)) => event(&mut session, packet),
                Ok(None) => {}
                Err(error) => {
                    eprintln!("USB disconnected: {error}");
                    if let Some(active) = session.take() {
                        active.sink.failed.store(true, Ordering::SeqCst);
                    }
                    break;
                }
            }
        }
    });
    Ok(Actor { tx })
}

struct Lease {
    actor: Actor,
    token: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.actor.call(Action::Detach { token: self.token });
    }
}

fn stream_writer(mut socket: UnixStream, events: Receiver<Response>, failed: Arc<AtomicBool>) {
    loop {
        if failed.load(Ordering::SeqCst) {
            let _ = wire::write(
                &mut socket,
                &Response::error("serial stream failed: overflow or device disconnected"),
            );
            break;
        }
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(response) => {
                if wire::write(&mut socket, &response).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    failed.store(true, Ordering::SeqCst);
    let _ = socket.shutdown(Shutdown::Both);
}

pub fn serve(mut socket: UnixStream, actor: Actor, token: u64) -> io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(socket.try_clone()?);
    let first = wire::read(&mut reader)?;
    let Request::Serial { baud } = first else {
        let response = actor
            .call(Action::Control(first))
            .unwrap_or_else(Response::error);
        wire::write(&mut socket, &response)?;
        return Ok(());
    };
    let (tx, events) = mpsc::sync_channel(256);
    let failed = Arc::new(AtomicBool::new(false));
    let sink = Sink {
        tx: tx.clone(),
        failed: failed.clone(),
    };
    let ready = match actor.call(Action::Attach { token, baud, sink }) {
        Ok(ready) => ready,
        Err(error) => {
            wire::write(&mut socket, &Response::error(error))?;
            return Ok(());
        }
    };
    let _lease = Lease {
        actor: actor.clone(),
        token,
    };
    wire::write(&mut socket, &ready)?;
    socket.set_read_timeout(None)?;
    let writer_socket = socket.try_clone()?;
    let writer_failed = failed.clone();
    let writer = thread::spawn(move || stream_writer(writer_socket, events, writer_failed));
    let result = (|| {
        loop {
            let action = match wire::read::<Request>(&mut reader)? {
                Request::Write { data } => Action::Write {
                    token,
                    data: wire::decode(&data)?,
                },
                Request::ResetInput => Action::Flush { token },
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid serial request",
                    ));
                }
            };
            let response = actor.call(action).unwrap_or_else(Response::error);
            let fatal = matches!(response, Response::Error { .. });
            if tx.try_send(response).is_err() {
                failed.store(true, Ordering::SeqCst);
                break;
            }
            if fatal {
                break;
            }
        }
        Ok(())
    })();
    // Dropping a connection must also release its actor subscription. The
    // writer is bounded by its socket timeout; shutdown wakes both directions.
    failed.store(true, Ordering::SeqCst);
    let _ = socket.shutdown(Shutdown::Both);
    drop(tx);
    let _ = writer.join();
    result
}

struct SocketGuard(std::path::PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(path: &Path, actor: Actor) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let directory = std::fs::symlink_metadata(parent)?;
    let uid = unsafe { libc::geteuid() };
    if !directory.is_dir() || directory.uid() != uid || directory.permissions().mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "use an owned socket directory without group/other write access",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "socket path is not an owned Unix socket",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if path.exists() {
        // Never remove a socket on a transient error: only ECONNREFUSED proves
        // a stale socket; other failures (permissions, backlog) are reported.
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "daemon socket is active",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path)?,
            Err(e) => return Err(e),
        }
    }
    let listener = UnixListener::bind(path)?;
    let _guard = SocketGuard(path.into());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    println!("socket: {}", path.display());
    for (index, socket) in listener.incoming().enumerate() {
        let socket = socket?;
        let actor = actor.clone();
        thread::spawn(move || {
            if let Err(e) = serve(socket, actor, index as u64 + 1)
                && e.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("client: {e}");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;
    struct Fake {
        writes: Vec<Vec<u8>>,
    }
    impl DeviceIo for Fake {
        fn request(
            &mut self,
            kind: u8,
            data: &[u8],
            events: &mut dyn FnMut(Packet),
        ) -> io::Result<Vec<u8>> {
            if kind == CMD_UART_WRITE {
                self.writes.push(data.to_vec());
                events(Packet {
                    kind: EVT_UART_DATA,
                    id: 0,
                    data: data.to_vec(),
                });
            }
            Ok(if kind == CMD_GET_INFO {
                DeviceInfo::new(3, 1, 3_000_000).as_bytes().to_vec()
            } else if kind == CMD_GPIO_GET_STATE {
                vec![15, 3, 0, 7]
            } else {
                vec![]
            })
        }
        fn event(&mut self) -> io::Result<Option<Packet>> {
            thread::sleep(Duration::from_millis(1));
            Ok(None)
        }
    }
    #[test]
    fn combined_set_enables_output_from_released_state_in_one_actor_operation() {
        struct Gpio {
            directions: u8,
            outputs: u8,
            calls: Vec<u8>,
        }
        impl DeviceIo for Gpio {
            fn request(
                &mut self,
                kind: u8,
                data: &[u8],
                _: &mut dyn FnMut(Packet),
            ) -> io::Result<Vec<u8>> {
                self.calls.push(kind);
                match kind {
                    CMD_GPIO_SET_DIRECTION => {
                        self.directions = (self.directions & !data[0]) | (data[1] & data[0])
                    }
                    CMD_GPIO_SET_OUTPUT => {
                        self.outputs = (self.outputs & !data[0]) | (data[1] & data[0])
                    }
                    _ => panic!("unexpected GPIO command"),
                }
                Ok(vec![0, self.outputs, self.directions, 7])
            }
            fn event(&mut self) -> io::Result<Option<Packet>> {
                Ok(None)
            }
        }
        let info = parse_info(DeviceInfo::new(1, 0x3f, 3_000_000).as_bytes()).unwrap();
        let mut device = Gpio {
            directions: 0,
            outputs: 3,
            calls: vec![],
        };
        let mut session = None;
        let state = execute(
            &mut device,
            &mut session,
            Action::Control(Request::Set { mask: 1, values: 0 }),
            "TEST",
            &info,
        )
        .unwrap();
        assert!(matches!(
            state,
            Response::State {
                directions: 1,
                outputs: 2,
                ..
            }
        ));
        assert_eq!(device.calls, [CMD_GPIO_SET_DIRECTION, CMD_GPIO_SET_OUTPUT]);
        execute(
            &mut device,
            &mut session,
            Action::Control(Request::Direction { mask: 1, values: 0 }),
            "TEST",
            &info,
        )
        .unwrap();
        assert_eq!(device.directions, 0);
    }
    #[test]
    fn socket_session_round_trip_and_controls_during_uart() {
        let actor = spawn(Fake { writes: vec![] }, "TEST123".into()).unwrap();
        let (mut socket, server) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server_actor = actor.clone();
        let worker = thread::spawn(move || {
            let _ = serve(server, server_actor, 1);
        });
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        wire::write(&mut socket, &Request::Serial { baud: 115200 }).unwrap();
        assert!(matches!(
            wire::read::<Response>(&mut reader).unwrap(),
            Response::Ready { version: 1, .. }
        ));
        assert!(matches!(
            actor.call(Action::Control(Request::State)).unwrap(),
            Response::State { inputs: 15, .. }
        ));
        let bytes = vec![0, 255, 3, b'\n'];
        wire::write(
            &mut socket,
            &Request::Write {
                data: wire::encode(&bytes),
            },
        )
        .unwrap();
        let mut received = vec![];
        loop {
            match wire::read::<Response>(&mut reader).unwrap() {
                Response::Data { data } => received.extend(wire::decode(&data).unwrap()),
                Response::Written => break,
                response => panic!("unexpected {response:?}"),
            }
        }
        assert_eq!(received, bytes);
        wire::write(&mut socket, &Request::ResetInput).unwrap();
        assert!(matches!(
            wire::read::<Response>(&mut reader).unwrap(),
            Response::ResetInput
        ));
        socket.shutdown(Shutdown::Both).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn exclusive_lease_chunking_and_visible_overflow() {
        let mut device = Fake { writes: vec![] };
        let info = parse_info(DeviceInfo::new(3, 1, 3_000_000).as_bytes()).unwrap();
        let mut session = None;
        let (tx, rx) = mpsc::sync_channel(4);
        let failed = Arc::new(AtomicBool::new(false));
        let sink = Sink {
            tx,
            failed: failed.clone(),
        };
        execute(
            &mut device,
            &mut session,
            Action::Attach {
                token: 1,
                baud: 115200,
                sink: sink.clone(),
            },
            "ABC",
            &info,
        )
        .unwrap();
        assert!(
            execute(
                &mut device,
                &mut session,
                Action::Attach {
                    token: 2,
                    baud: 115200,
                    sink
                },
                "ABC",
                &info
            )
            .is_err()
        );
        execute(
            &mut device,
            &mut session,
            Action::Write {
                token: 1,
                data: vec![42; 100],
            },
            "ABC",
            &info,
        )
        .unwrap();
        assert_eq!(
            device.writes.iter().map(Vec::len).collect::<Vec<_>>(),
            [61, 39]
        );
        assert!(matches!(rx.recv().unwrap(), Response::Data { .. }));
        event(
            &mut session,
            Packet {
                kind: EVT_UART_OVERFLOW,
                id: 0,
                data: vec![0, 1, 0, 0, 0],
            },
        );
        assert!(failed.load(Ordering::SeqCst));
        assert!(
            execute(
                &mut device,
                &mut session,
                Action::Write {
                    token: 1,
                    data: vec![1]
                },
                "ABC",
                &info
            )
            .is_err()
        );
    }
}
