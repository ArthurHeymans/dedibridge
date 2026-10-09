use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use cargo_metadata::{Message, Metadata, MetadataCommand, PackageId, TargetKind};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(about = "Build, check and package DediBridge; flash only when explicitly requested")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Format workspace members, without touching external path dependencies.
    Fmt {
        #[arg(long)]
        check: bool,
    },
    /// Build one board's release firmware.
    Build { board: Board },
    /// Build and flash one board's release firmware using probe-rs (requires connected hardware).
    Flash { board: Board },
    /// Build and report one board's ELF section sizes using llvm-size.
    Size { board: Board },
    /// Run warning-denied Clippy on one board's release firmware.
    Clippy { board: Board },
    /// Run host/core/protocol, xtask and Go adapter tests.
    Test,
    /// Check formatting, host and firmware Clippy, and all firmware builds.
    Check,
    /// Build all firmware and package three ELFs and the Pico UF2.
    Artifacts,
    /// Run dedibridgectl, forwarding all remaining arguments unchanged.
    Host {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Board {
    Rp2040,
    Stm32f103,
    Ch32v307,
}

struct BoardConfig {
    package: &'static str,
    target: &'static str,
    toolchain: &'static str,
    chip: &'static str,
    extra: &'static [&'static str],
}

impl Board {
    const ALL: [Self; 3] = [Self::Rp2040, Self::Stm32f103, Self::Ch32v307];

    fn config(self) -> BoardConfig {
        match self {
            Self::Rp2040 => BoardConfig {
                package: "dedi-rp2040",
                target: "thumbv6m-none-eabi",
                toolchain: "stable",
                chip: "RP2040",
                extra: &[],
            },
            Self::Stm32f103 => BoardConfig {
                package: "dedi-stm32f103",
                target: "thumbv7m-none-eabi",
                toolchain: "stable",
                chip: "STM32F103C8",
                extra: &[],
            },
            Self::Ch32v307 => BoardConfig {
                package: "dedi-ch32v307",
                target: "boards/ch32v307/riscv32imfc-unknown-none-elf.json",
                toolchain: "nightly",
                chip: "CH32V307VCT6",
                extra: &["-Zbuild-std=core", "-Zjson-target-spec"],
            },
        }
    }
}

struct Workspace {
    metadata: Metadata,
}

impl Workspace {
    fn load() -> Result<Self> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml");
        let metadata = MetadataCommand::new()
            .manifest_path(manifest)
            .no_deps()
            .other_options(vec!["--locked".into()])
            .exec()
            .context("could not read workspace metadata")?;
        Ok(Self { metadata })
    }

    fn root(&self) -> &Path {
        self.metadata.workspace_root.as_std_path()
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.current_dir(self.root());
        command
    }

    fn cargo(&self, subcommand: &str) -> Command {
        let mut command = self.command("cargo");
        command.args(["+stable", subcommand]);
        command
    }

    fn board_cargo(&self, board: Board, subcommand: &str) -> Command {
        let config = board.config();
        let mut command = self.command("cargo");
        command
            .arg(format!("+{}", config.toolchain))
            .args([subcommand, "--release", "--locked", "-p", config.package])
            .args(["--target", config.target])
            .args(config.extra);
        command
    }

    fn host_packages(&self, command: &mut Command) {
        // Keep bare-metal crates out of host compilation; test/check xtask too.
        for id in self.metadata.workspace_default_members.iter() {
            command.args(["-p", self.metadata[id].name.as_str()]);
        }
        command.args(["-p", "xtask"]);
    }

    fn format(&self, check: bool) -> Result<()> {
        let mut command = self.cargo("fmt");
        for id in &self.metadata.workspace_members {
            command.args(["-p", self.metadata[id].name.as_str()]);
        }
        if check {
            command.args(["--", "--check"]);
        }
        run(&mut command)
    }

    fn build(&self, board: Board) -> Result<PathBuf> {
        let binary = board.config().package;
        let package = self
            .metadata
            .workspace_members
            .iter()
            .find(|id| self.metadata[*id].name.as_str() == binary)
            .with_context(|| format!("workspace package {binary} is missing"))?;
        let mut command = self.board_cargo(board, "build");
        command.args(["--bin", binary, "--message-format=json-render-diagnostics"]);
        command.stdout(Stdio::piped());
        eprintln!("+ {command:?}");
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start {command:?}"))?;
        let reader = BufReader::new(child.stdout.take().context("missing Cargo stdout")?);
        let executable = read_executable(reader, package, binary);
        if executable.is_err() {
            // Do not leave Cargo blocked on a pipe if message parsing fails.
            let _ = child.kill();
        }
        let status = child.wait().context("could not wait for Cargo build")?;
        let executable = executable?;
        ensure!(status.success(), "{command:?} failed with {status}");
        executable.with_context(|| format!("Cargo did not report an executable for {binary}"))
    }

    fn clippy(&self, board: Board) -> Result<()> {
        run(self
            .board_cargo(board, "clippy")
            .args(["--", "-D", "warnings"]))
    }

    fn test(&self) -> Result<()> {
        let mut command = self.cargo("test");
        command.arg("--locked");
        self.host_packages(&mut command);
        run(&mut command)?;
        run(self
            .command("go")
            .current_dir(self.root().join("integrations/dutctl"))
            .args(["test", "-race", "./..."]))
    }

    fn check(&self) -> Result<()> {
        self.format(true)?;
        let mut command = self.cargo("clippy");
        command.args(["--locked", "--all-targets"]);
        self.host_packages(&mut command);
        run(command.args(["--", "-D", "warnings"]))?;
        for board in Board::ALL {
            self.clippy(board)?;
            self.build(board)?;
        }
        Ok(())
    }

    fn artifacts(&self) -> Result<()> {
        let output = self.root().join("artifacts");
        fs::create_dir_all(&output).context("could not create artifacts directory")?;
        for board in Board::ALL {
            let elf = self.build(board)?;
            let destination = output.join(format!("{}.elf", board.config().package));
            fs::copy(&elf, &destination).with_context(|| {
                format!(
                    "could not copy {} to {}",
                    elf.display(),
                    destination.display()
                )
            })?;
        }
        run(self
            .command("elf2uf2-rs")
            .arg(output.join("dedi-rp2040.elf"))
            .arg(output.join("dedi-rp2040.uf2")))
    }

    fn execute(&self, task: Task) -> Result<()> {
        match task {
            Task::Fmt { check } => self.format(check),
            Task::Build { board } => {
                println!("{}", self.build(board)?.display());
                Ok(())
            }
            Task::Flash { board } => {
                let elf = self.build(board)?;
                run(self
                    .command("probe-rs")
                    .args([
                        "run",
                        "--chip",
                        board.config().chip,
                        "--rtt-channel-mode",
                        "no-block-skip",
                    ])
                    .arg(elf))
            }
            Task::Size { board } => {
                let elf = self.build(board)?;
                run(self.command("llvm-size").arg(elf))
            }
            Task::Clippy { board } => self.clippy(board),
            Task::Test => self.test(),
            Task::Check => self.check(),
            Task::Artifacts => self.artifacts(),
            Task::Host { args } => run(self
                .cargo("run")
                .args(["--locked", "-p", "dedibridgectl", "--"])
                .args(args)),
        }
    }
}

fn read_executable(
    reader: impl BufRead,
    package: &PackageId,
    binary: &str,
) -> Result<Option<PathBuf>> {
    let mut executable = None;
    for message in Message::parse_stream(reader) {
        match message.context("could not parse Cargo build output")? {
            Message::CompilerArtifact(artifact)
                if &artifact.package_id == package
                    && artifact.target.name == binary
                    && artifact.target.kind.contains(&TargetKind::Bin)
                    && !artifact.profile.test =>
            {
                if let Some(path) = artifact.executable {
                    ensure!(
                        executable.is_none(),
                        "Cargo reported multiple executables for {binary}"
                    );
                    executable = Some(path.into_std_path_buf());
                }
            }
            Message::CompilerMessage(message) => {
                if let Some(rendered) = message.message.rendered {
                    eprint!("{rendered}");
                }
            }
            Message::TextLine(line) => eprintln!("{line}"),
            _ => {}
        }
    }
    Ok(executable)
}

fn run(command: &mut Command) -> Result<()> {
    eprintln!("+ {command:?}");
    let status = command.status().with_context(|| {
        format!("could not start {command:?}; is the tool installed and on PATH?")
    })?;
    if !status.success() {
        bail!("{command:?} failed with {status}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    Workspace::load()?.execute(cli.task)
}

#[cfg(test)]
mod tests;
