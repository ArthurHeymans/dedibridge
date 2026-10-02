mod client;
mod daemon;
mod device;
mod wire;

use clap::{Parser, Subcommand, ValueEnum};
use std::{io, path::PathBuf};
use wire::{Request, Response};

#[derive(Parser)]
#[command(about = "Hardware-neutral SF600 auxiliary UART and board control")]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    List,
    Daemon {
        #[arg(long)]
        serial: Option<String>,
    },
    Info,
    State,
    Reset {
        #[arg(default_value_t = 100)]
        ms: u16,
    },
    Power {
        #[arg(default_value_t = 500)]
        ms: u16,
    },
    Poweroff {
        #[arg(default_value_t = 5000)]
        ms: u16,
    },
    /// Pull a named pin (reset/power) or numeric mask low.
    Pulse {
        mask: PinMask,
        ms: u16,
    },
    Dir {
        pin: PinMask,
        direction: Direction,
    },
    Set {
        pin: PinMask,
        value: Level,
    },
    Release {
        pin: PinMask,
    },
    Direction {
        mask: u8,
        values: u8,
    },
    Output {
        mask: u8,
        values: u8,
    },
    Console {
        #[arg(long, default_value_t = 115200)]
        baud: u32,
    },
    Pty {
        #[arg(long, default_value_t = 115200)]
        baud: u32,
    },
}
#[derive(Clone, Copy, Debug)]
struct PinMask(u8);
impl std::str::FromStr for PinMask {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reset" => Ok(Self(1)),
            "power" => Ok(Self(2)),
            "power-state" => Ok(Self(4)),
            "aux" => Ok(Self(8)),
            _ => value
                .parse()
                .map(Self)
                .map_err(|_| "expected a named pin or numeric mask".into()),
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum Direction {
    In,
    Out,
}
#[derive(Clone, Copy, ValueEnum)]
enum Level {
    #[value(name = "0")]
    Low,
    #[value(name = "1")]
    High,
}
impl Command {
    fn control_request(self) -> Option<Request> {
        Some(match self {
            Command::Info => Request::Info,
            Command::State => Request::State,
            Command::Reset { ms } => Request::Pulse { mask: 1, ms },
            Command::Power { ms } | Command::Poweroff { ms } => Request::Pulse { mask: 2, ms },
            Command::Pulse { mask, ms } => Request::Pulse { mask: mask.0, ms },
            Command::Dir { pin, direction } => Request::Direction {
                mask: pin.0,
                values: if matches!(direction, Direction::Out) {
                    pin.0
                } else {
                    0
                },
            },
            Command::Set { pin, value } => Request::Set {
                mask: pin.0,
                values: if matches!(value, Level::High) {
                    pin.0
                } else {
                    0
                },
            },
            Command::Release { pin } => Request::Direction {
                mask: pin.0,
                values: 0,
            },
            Command::Direction { mask, values } => Request::Direction { mask, values },
            Command::Output { mask, values } => Request::Output { mask, values },
            _ => return None,
        })
    }
}
fn default_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/dedibridge-{}", unsafe { libc::geteuid() })))
        .join("dedibridge.sock")
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket);
    let request = match cli.command {
        Command::List => {
            for device in device::devices()? {
                println!(
                    "{} {}:{}",
                    device.serial_number().unwrap_or("<no serial>"),
                    device.bus_id(),
                    device.device_address()
                );
            }
            return Ok(());
        }
        Command::Daemon { serial } => {
            let (device, serial) = device::Device::open(serial.as_deref())?;
            let reconnect_serial = serial.clone();
            let actor = daemon::spawn(device, serial, move || {
                device::Device::open(Some(&reconnect_serial))
            })?;
            return Ok(daemon::run(&socket, actor)?);
        }
        Command::Console { baud } => {
            let input = std::fs::File::open("/dev/stdin")?;
            let output = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/stdout")?;
            let _raw = client::RawTerminal::new(std::os::fd::AsRawFd::as_raw_fd(&input))?;
            return Ok(client::bridge(&socket, baud, input, output, Some(0x1d))?);
        }
        Command::Pty { baud } => {
            let (input, output, _slave, name) = client::pty()?;
            println!("pty: {name}");
            return Ok(client::bridge(&socket, baud, input, output, None)?);
        }
        command => command.control_request().expect("control command"),
    };
    match client::control(&socket, request)? {
        Response::State {
            inputs,
            outputs,
            directions,
            caps,
        } => println!(
            "inputs={inputs:#04x} outputs={outputs:#04x} directions={directions:#04x} caps={caps:#04x}"
        ),
        response => wire::write(&mut io::stdout(), &response)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_named_gpio_commands_and_numeric_masks_translate() {
        for (args, expected) in [
            (
                vec!["dir", "reset", "out"],
                r#"{"op":"direction","mask":1,"values":1}"#,
            ),
            (
                vec!["dir", "power", "in"],
                r#"{"op":"direction","mask":2,"values":0}"#,
            ),
            (
                vec!["set", "reset", "0"],
                r#"{"op":"set","mask":1,"values":0}"#,
            ),
            (
                vec!["set", "power", "1"],
                r#"{"op":"set","mask":2,"values":2}"#,
            ),
            (
                vec!["release", "reset"],
                r#"{"op":"direction","mask":1,"values":0}"#,
            ),
            (
                vec!["pulse", "power", "50"],
                r#"{"op":"pulse","mask":2,"ms":50}"#,
            ),
            (
                vec!["pulse", "3", "50"],
                r#"{"op":"pulse","mask":3,"ms":50}"#,
            ),
            (vec!["reset"], r#"{"op":"pulse","mask":1,"ms":100}"#),
            (vec!["power"], r#"{"op":"pulse","mask":2,"ms":500}"#),
            (vec!["poweroff"], r#"{"op":"pulse","mask":2,"ms":5000}"#),
            (vec!["state"], r#"{"op":"state"}"#),
        ] {
            let cli = Cli::try_parse_from(std::iter::once("dedibridgectl").chain(args)).unwrap();
            let actual = serde_json::to_value(cli.command.control_request().unwrap()).unwrap();
            assert_eq!(
                actual,
                serde_json::from_str::<serde_json::Value>(expected).unwrap()
            );
        }
    }
}
