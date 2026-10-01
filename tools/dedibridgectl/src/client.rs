use crate::wire::{self, Request, Response};
use std::{
    fs::File,
    io::{self, BufReader, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

pub fn control(path: &Path, request: Request) -> io::Result<Response> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    wire::write(&mut socket, &request)?;
    wire::read::<Response>(&mut BufReader::new(socket))?.check()
}
pub fn open_serial(path: &Path, baud: u32) -> io::Result<(UnixStream, BufReader<UnixStream>)> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(socket.try_clone()?);
    wire::write(&mut socket, &Request::Serial { baud })?;
    match wire::read::<Response>(&mut reader)?.check()? {
        Response::Ready {
            version: wire::VERSION,
            ..
        } => {}
        _ => return Err(io::Error::other("invalid daemon handshake")),
    }
    socket.set_read_timeout(None)?;
    Ok((socket, reader))
}

pub fn bridge(
    path: &Path,
    baud: u32,
    mut input: File,
    mut output: File,
    escape: Option<u8>,
) -> io::Result<()> {
    let escape = escape.filter(|_| unsafe { libc::isatty(input.as_raw_fd()) } != 0);
    let (mut socket, mut reader) = open_serial(path, baud)?;
    let (tx, acks) = mpsc::sync_channel::<io::Result<()>>(4);
    let receive = std::thread::spawn(move || {
        let result = (|| {
            let _flags = NonBlocking::new(output.as_raw_fd())?;
            loop {
                match wire::read::<Response>(&mut reader)?.check()? {
                    Response::Data { data } => write_output(&mut output, &wire::decode(&data)?)?,
                    Response::Written => {
                        tx.send(Ok(()))
                            .map_err(|_| io::Error::other("console closed"))?;
                    }
                    _ => return Err(io::Error::other("unexpected serial response")),
                }
            }
        })();
        let _ = tx.send(result);
    });
    let mut bytes = [0; 4096];
    let result = (|| {
        loop {
            if let Ok(error) = acks.try_recv() {
                return error;
            }
            let mut poll = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 {
                continue;
            }
            let n = input.read(&mut bytes)?;
            if n == 0 {
                return Ok(());
            }
            let (data, quit) = console_chunk(&bytes[..n], escape);
            if !data.is_empty() {
                wire::write(
                    &mut socket,
                    &Request::Write {
                        data: wire::encode(data),
                    },
                )?;
                acks.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| io::Error::other("serial write acknowledgment timeout"))??;
            }
            if quit {
                return Ok(());
            }
        }
    })();
    let _ = socket.shutdown(Shutdown::Both);
    let _ = receive.join();
    result
}

fn console_chunk(data: &[u8], escape: Option<u8>) -> (&[u8], bool) {
    match escape.and_then(|escape| data.iter().position(|byte| *byte == escape)) {
        Some(n) => (&data[..n], true),
        None => (data, false),
    }
}

struct NonBlocking {
    fd: i32,
    flags: i32,
}
impl NonBlocking {
    fn new(fd: i32) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, flags })
    }
}
impl Drop for NonBlocking {
    fn drop(&mut self) {
        unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.flags) };
    }
}
fn write_output(output: &mut File, mut bytes: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !bytes.is_empty() {
        match output.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "console output closed",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "console output stalled",
                    ));
                }
                let mut poll = libc::pollfd {
                    fd: output.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 100) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn pty() -> io::Result<(File, File, File, String)> {
    let (mut master, mut slave) = (0, 0);
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let name = unsafe {
        let name = libc::ttyname(slave.as_raw_fd());
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned()
    };
    let mut raw = RawTerminal::new(slave.as_raw_fd())?;
    raw.old = None; // This PTY is ours, not the user's terminal; leave it raw.
    // Retain the slave for the lifetime of the bridge, including before a
    // terminal attaches. A slow/unopened PTY eventually fails via backpressure.
    Ok((master.try_clone()?, master, slave, name))
}

pub struct RawTerminal {
    fd: i32,
    old: Option<libc::termios>,
}
impl RawTerminal {
    pub fn new(fd: i32) -> io::Result<Self> {
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(Self { fd, old: None });
        }
        let mut old = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(fd, old.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let old = unsafe { old.assume_init() };
        let mut raw = old;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, old: Some(old) })
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(old) = self.old {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &old) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn console_escape_preserves_prefix_and_pty_is_binary() {
        let bytes = [0, 0x1d, 255];
        assert_eq!(console_chunk(&bytes, Some(0x1d)), (&bytes[..1], true));
        assert_eq!(console_chunk(&bytes, None), (&bytes[..], false));
    }
}
