use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read, Write};

pub const VERSION: u8 = 1;
pub const MAX_LINE: usize = 16384;
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Info,
    Diagnostics,
    State,
    Pulse { mask: u8, ms: u16 },
    Direction { mask: u8, values: u8 },
    Output { mask: u8, values: u8 },
    Set { mask: u8, values: u8 },
    Serial { baud: u32 },
    Write { data: String },
    ResetInput,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Info {
    pub version: u8,
    pub board: u8,
    pub io_modes: u8,
    pub gpio_outputs: u8,
    pub gpio_inputs: u8,
    pub flags: u8,
    pub max_baud: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Diagnostics {
    pub version: u8,
    pub board: u8,
    pub uptime_secs: u32,
    pub firmware_version: String,
    pub rx_gaps_unobserved: bool,
    pub reset_cause: Option<u32>,
    pub boot_count: Option<u32>,
    pub uart: std::collections::BTreeMap<String, u32>,
    pub activity: std::collections::BTreeMap<String, u32>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ready {
        version: u8,
        serial: String,
        info: Info,
    },
    Info {
        serial: String,
        info: Info,
    },
    Diagnostics {
        serial: String,
        /// None means unsupported by this firmware, not zero counters.
        diagnostics: Option<Diagnostics>,
    },
    State {
        inputs: u8,
        outputs: u8,
        directions: u8,
        caps: u8,
    },
    Written,
    ResetInput,
    Data {
        data: String,
    },
    Error {
        message: String,
    },
}
impl Response {
    pub fn error(error: impl std::fmt::Display) -> Self {
        Self::Error {
            message: error.to_string(),
        }
    }
    pub fn check(self) -> io::Result<Self> {
        if let Self::Error { message } = self {
            Err(io::Error::other(message))
        } else {
            Ok(self)
        }
    }
}
pub fn read<T: serde::de::DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<T> {
    let mut line = Vec::new();
    Read::take(&mut *reader, (MAX_LINE + 1) as u64).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "socket closed",
        ));
    }
    if line.len() > MAX_LINE || line.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized or truncated socket frame",
        ));
    }
    serde_json::from_slice(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
pub fn write(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
pub fn encode(data: &[u8]) -> String {
    STANDARD.encode(data)
}
pub fn decode(data: &str) -> io::Result<Vec<u8>> {
    let bytes = STANDARD
        .decode(data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write exceeds 4096 bytes",
        ));
    }
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framed_binary_round_trip_and_bounds() {
        let mut wire = Vec::new();
        write(
            &mut wire,
            &Request::Write {
                data: encode(&[0, 255, b'\n']),
            },
        )
        .unwrap();
        let Request::Write { data } = read(&mut io::Cursor::new(wire)).unwrap() else {
            panic!()
        };
        assert_eq!(decode(&data).unwrap(), [0, 255, b'\n']);
        assert!(read::<Request>(&mut io::Cursor::new(vec![b'x'; MAX_LINE + 1])).is_err());
    }
}
