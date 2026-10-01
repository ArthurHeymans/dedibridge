mod client;
mod daemon;
mod device;
mod wire;

use clap::{Parser, Subcommand};
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
    Pulse {
        mask: u8,
        ms: u16,
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
            return Ok(daemon::run(&socket, daemon::spawn(device, serial)?)?);
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
        Command::Info => Request::Info,
        Command::State => Request::State,
        Command::Reset { ms } => Request::Pulse { mask: 1, ms },
        Command::Power { ms } | Command::Poweroff { ms } => Request::Pulse { mask: 2, ms },
        Command::Pulse { mask, ms } => Request::Pulse { mask, ms },
        Command::Direction { mask, values } => Request::Direction { mask, values },
        Command::Output { mask, values } => Request::Output { mask, values },
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
