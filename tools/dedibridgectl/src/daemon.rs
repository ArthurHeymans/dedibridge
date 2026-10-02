use crate::{
    device::{DeviceIo, Packet, diagnostics, parse_info},
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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
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
impl Drop for Session {
    fn drop(&mut self) {
        self.sink.failed.store(true, Ordering::SeqCst);
    }
}
pub enum Action {
    Control(Request),
    Attach { token: u64, baud: u32, sink: Sink },
    Write { token: u64, data: Vec<u8> },
    Flush { token: u64 },
    Detach { token: u64 },
}
struct Message {
    generation: u64,
    action: Action,
    stream_reply: bool,
    reply: SyncSender<Response>,
}
#[derive(Clone, Copy)]
struct Availability {
    generation: u64,
    present: bool,
}
#[derive(Clone)]
pub struct Actor {
    tx: SyncSender<Message>,
    availability: Arc<Mutex<Availability>>,
}
impl Actor {
    pub fn call(&self, action: Action) -> io::Result<Response> {
        self.call_inner(action, false)
    }
    fn call_stream(&self, action: Action) -> io::Result<Response> {
        self.call_inner(action, true)
    }
    fn call_inner(&self, action: Action, stream_reply: bool) -> io::Result<Response> {
        let (reply, rx) = mpsc::sync_channel(1);
        // Admission is the generation snapshot, not the channel send. If the
        // bounded queue delays sending across a disconnect, dispatch still
        // rejects this old-generation action. Never hold the state lock while
        // blocking on the channel (the actor also needs that lock).
        let generation = {
            let state = self.availability.lock().unwrap();
            if !state.present {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "device not present",
                ));
            }
            state.generation
        };
        self.tx
            .send(Message {
                generation,
                action,
                stream_reply,
                reply,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "USB actor stopped"))?;
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
                Request::Diagnostics => {
                    return Ok(Response::Diagnostics {
                        serial: serial.into(),
                        diagnostics: diagnostics(device, info, &mut |packet| {
                            event(session, packet)
                        })?,
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

fn fail_session(session: &mut Option<Session>) {
    if let Some(active) = session.as_ref() {
        active.sink.failed.store(true, Ordering::SeqCst);
    }
}

fn invalidate(session: &mut Option<Session>, availability: &Mutex<Availability>) {
    let mut state = availability.lock().unwrap();
    state.present = false;
    state.generation = state
        .generation
        .checked_add(1)
        .expect("device generation exhausted");
    fail_session(session);
    *session = None;
}

// Fail closed even if the actor exits unexpectedly (including a panic).
struct ActorLife(Arc<Mutex<Availability>>);
impl Drop for ActorLife {
    fn drop(&mut self) {
        self.0.lock().unwrap().present = false;
    }
}

fn validate_identity(
    serial: &str,
    expected: &Info,
    found_serial: &str,
    found: &Info,
) -> io::Result<()> {
    if serial != found_serial || expected.board != found.board || expected.version != found.version
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reconnected device identity changed",
        ));
    }
    Ok(())
}

fn dispatch(
    device: &mut impl DeviceIo,
    session: &mut Option<Session>,
    action: Action,
    generation_state: (u64, Availability),
    serial: &str,
    info: &Info,
) -> io::Result<Response> {
    let (generation, state) = generation_state;
    if !state.present || generation != state.generation {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "device connection changed; command not executed",
        ));
    }
    execute(device, session, action, serial, info)
}

pub fn spawn<D: DeviceIo + Send + 'static>(
    mut device: D,
    serial: String,
    mut reconnect: impl FnMut() -> io::Result<(D, String)> + Send + 'static,
) -> io::Result<Actor> {
    let mut info = parse_info(&device.request(CMD_GET_INFO, &[], &mut |_| {})?)?;
    let (tx, rx) = mpsc::sync_channel::<Message>(16);
    let availability = Arc::new(Mutex::new(Availability {
        generation: 1,
        present: true,
    }));
    let actor = Actor {
        tx,
        availability: availability.clone(),
    };
    thread::spawn(move || {
        let _life = ActorLife(availability.clone());
        let mut device = Some(device);
        let mut session: Option<Session> = None;
        let mut next_open = Instant::now();
        loop {
            if let Some(active) = device.as_mut() {
                match rx.try_recv() {
                    Ok(message) => {
                        let reply_sink = if message.stream_reply {
                            match (&message.action, session.as_ref()) {
                                (
                                    Action::Write { token, .. } | Action::Flush { token },
                                    Some(client),
                                ) if *token == client.token => Some(client.sink.clone()),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        let state = *availability.lock().unwrap();
                        let response = dispatch(
                            active,
                            &mut session,
                            message.action,
                            (message.generation, state),
                            &serial,
                            &info,
                        )
                        .unwrap_or_else(Response::error);
                        // Command replies and UART DATA use the same actor
                        // producer. In particular, post-flush DATA cannot race
                        // the socket thread and overtake ResetInput.
                        if let Some(sink) = reply_sink
                            && sink.tx.try_send(response.clone()).is_err()
                        {
                            sink.failed.store(true, Ordering::SeqCst);
                        }
                        if active.link_failed() {
                            invalidate(&mut session, &availability);
                            device = None;
                            next_open = Instant::now() + Duration::from_millis(500);
                        }
                        let _ = message.reply.send(response);
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if let Some(active) = device.as_mut() {
                    match active.event() {
                        Ok(Some(packet)) => event(&mut session, packet),
                        Ok(None) => {}
                        Err(error)
                            if error.kind() == io::ErrorKind::InvalidData
                                && !active.link_failed() =>
                        {
                            eprintln!("Malformed USB packet; serial session failed: {error}");
                            fail_session(&mut session);
                        }
                        Err(error) => {
                            eprintln!("USB connection failed: {error}");
                            invalidate(&mut session, &availability);
                            device = None;
                            next_open = Instant::now() + Duration::from_millis(500);
                        }
                    }
                }
            } else {
                // Retire every old-generation message without touching USB.
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(message) => {
                        let _ = message.reply.send(Response::error(
                            "device connection changed; command not executed",
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if Instant::now() >= next_open {
                    let opened = reconnect().and_then(|(mut candidate, found_serial)| {
                        let found =
                            parse_info(&candidate.request(CMD_GET_INFO, &[], &mut |_| {})?)?;
                        validate_identity(&serial, &info, &found_serial, &found)?;
                        Ok((candidate, found))
                    });
                    match opened {
                        Ok((candidate, found)) => {
                            device = Some(candidate);
                            info = found;
                            availability.lock().unwrap().present = true;
                            eprintln!("USB reconnected: {serial}; old sessions remain closed");
                        }
                        Err(error) => eprintln!("Waiting for {serial}: {error}"),
                    }
                    next_open = Instant::now() + Duration::from_millis(500);
                }
            }
        }
        fail_session(&mut session);
    });
    Ok(actor)
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
            if let Err(error) = actor.call_stream(action) {
                // Admission failures have no actor-produced stream reply.
                let _ = tx.try_send(Response::error(error));
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
    fn disconnected_admission_and_old_generation_commands_fail_closed() {
        let (tx, queued) = mpsc::sync_channel(16);
        let availability = Arc::new(Mutex::new(Availability {
            generation: 1,
            present: true,
        }));
        let actor = Actor {
            tx,
            availability: availability.clone(),
        };
        let (sink_tx, _) = mpsc::sync_channel(1);
        let failed = Arc::new(AtomicBool::new(false));
        let mut session = Some(Session {
            token: 1,
            sink: Sink {
                tx: sink_tx,
                failed: failed.clone(),
            },
        });
        invalidate(&mut session, &availability);
        assert!(failed.load(Ordering::SeqCst));
        assert!(session.is_none());
        assert_eq!(
            actor
                .call(Action::Control(Request::State))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
        availability.lock().unwrap().present = true;
        let state = *availability.lock().unwrap();
        let info = parse_info(DeviceInfo::new(3, 1, 3_000_000).as_bytes()).unwrap();
        let mut device = Fake { writes: vec![] };
        let (sink_tx, _) = mpsc::sync_channel(1);
        for action in [
            Action::Write {
                token: 1,
                data: vec![42],
            },
            Action::Control(Request::Pulse { mask: 1, ms: 500 }),
            Action::Attach {
                token: 2,
                baud: 115200,
                sink: Sink {
                    tx: sink_tx,
                    failed,
                },
            },
        ] {
            assert_eq!(
                dispatch(&mut device, &mut session, action, (1, state), "TEST", &info)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::NotConnected
            );
        }
        assert!(device.writes.is_empty());
        assert!(session.is_none());
    }
    #[test]
    fn flush_reply_precedes_immediate_post_barrier_data() {
        struct FlushSource {
            fake: Fake,
            flushes: usize,
            fresh: bool,
        }
        impl DeviceIo for FlushSource {
            fn request(
                &mut self,
                kind: u8,
                data: &[u8],
                events: &mut dyn FnMut(Packet),
            ) -> io::Result<Vec<u8>> {
                if kind == CMD_UART_FLUSH_RX {
                    self.flushes += 1;
                    self.fresh = self.flushes > 1;
                }
                self.fake.request(kind, data, events)
            }
            fn event(&mut self) -> io::Result<Option<Packet>> {
                if std::mem::take(&mut self.fresh) {
                    return Ok(Some(Packet {
                        kind: EVT_UART_DATA,
                        id: 0,
                        data: b"fresh".to_vec(),
                    }));
                }
                self.fake.event()
            }
        }
        let actor = spawn(
            FlushSource {
                fake: Fake { writes: vec![] },
                flushes: 0,
                fresh: false,
            },
            "TEST".into(),
            || Err(io::ErrorKind::NotFound.into()),
        )
        .unwrap();
        let (tx, events) = mpsc::sync_channel(4);
        actor
            .call(Action::Attach {
                token: 1,
                baud: 115200,
                sink: Sink {
                    tx,
                    failed: Arc::new(AtomicBool::new(false)),
                },
            })
            .unwrap();
        actor.call_stream(Action::Flush { token: 1 }).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            Response::ResetInput
        ));
        let Response::Data { data } = events.recv_timeout(Duration::from_secs(2)).unwrap() else {
            panic!("expected fresh DATA")
        };
        assert_eq!(wire::decode(&data).unwrap(), b"fresh");
    }
    #[test]
    fn reconnect_requires_original_identity() {
        let info = parse_info(DeviceInfo::new(3, 1, 3_000_000).as_bytes()).unwrap();
        validate_identity("A", &info, "A", &info).unwrap();
        assert!(validate_identity("A", &info, "B", &info).is_err());
        let mut changed = info.clone();
        changed.board = 2;
        assert!(validate_identity("A", &info, "A", &changed).is_err());
        changed = info.clone();
        changed.version = 2;
        assert!(validate_identity("A", &info, "A", &changed).is_err());
    }
    #[test]
    fn disconnect_during_write_invalidates_lease_and_reconnects_without_replay() {
        struct Unplug {
            fake: Fake,
            failed: bool,
            writes: Arc<std::sync::atomic::AtomicUsize>,
            ready: Option<SyncSender<()>>,
        }
        impl DeviceIo for Unplug {
            fn link_failed(&self) -> bool {
                self.failed
            }
            fn request(
                &mut self,
                kind: u8,
                data: &[u8],
                events: &mut dyn FnMut(Packet),
            ) -> io::Result<Vec<u8>> {
                if kind == CMD_UART_WRITE {
                    self.writes.fetch_add(1, Ordering::SeqCst);
                    self.failed = true;
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                self.fake.request(kind, data, events)
            }
            fn event(&mut self) -> io::Result<Option<Packet>> {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(());
                }
                self.fake.event()
            }
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (opened_tx, opened_rx) = mpsc::sync_channel(1);
        let (allow_tx, allow_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let new_writes = writes.clone();
        let actor = spawn(
            Unplug {
                fake: Fake { writes: vec![] },
                failed: false,
                writes: writes.clone(),
                ready: None,
            },
            "TEST".into(),
            move || {
                opened_tx.send(()).unwrap();
                allow_rx.recv().unwrap();
                Ok((
                    Unplug {
                        fake: Fake { writes: vec![] },
                        failed: false,
                        writes: new_writes.clone(),
                        ready: Some(ready_tx.clone()),
                    },
                    "TEST".into(),
                ))
            },
        )
        .unwrap();
        let (sink_tx, _) = mpsc::sync_channel(1);
        let failed = Arc::new(AtomicBool::new(false));
        actor
            .call(Action::Attach {
                token: 1,
                baud: 115200,
                sink: Sink {
                    tx: sink_tx,
                    failed: failed.clone(),
                },
            })
            .unwrap();
        assert!(
            actor
                .call(Action::Write {
                    token: 1,
                    data: vec![42; 100]
                })
                .is_err()
        );
        assert!(failed.load(Ordering::SeqCst));
        opened_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(
            actor
                .call(Action::Control(Request::State))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::NotConnected
        );
        allow_tx.send(()).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            actor
                .call(Action::Write {
                    token: 1,
                    data: vec![99]
                })
                .is_err()
        );
        let (sink_tx, _) = mpsc::sync_channel(1);
        actor
            .call(Action::Attach {
                token: 2,
                baud: 115200,
                sink: Sink {
                    tx: sink_tx,
                    failed: Arc::new(AtomicBool::new(false)),
                },
            })
            .unwrap();
        assert!(matches!(
            actor.call(Action::Control(Request::State)).unwrap(),
            Response::State { .. }
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
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
        let actor = spawn(Fake { writes: vec![] }, "TEST123".into(), || {
            Err(io::ErrorKind::NotFound.into())
        })
        .unwrap();
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
