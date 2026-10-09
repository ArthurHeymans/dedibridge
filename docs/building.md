# Building and development

[← DediBridge](../README.md)

## Toolchains

Install [Rust](https://rustup.rs/). Host tooling and ARM boards use stable;
CH32V307 needs nightly with `rust-src`:

```sh
rustup toolchain install stable
rustup target add --toolchain stable thumbv6m-none-eabi thumbv7m-none-eabi
rustup toolchain install nightly --component rust-src
```

The CH32 HAL is pinned to the published `ArthurHeymans/ch32-hal` fork at
`23f0e114`, including USBHS recovery fixes. No sibling HAL checkout is needed.

## Build one board

```sh
cargo xtask build rp2040
cargo xtask build ch32v307
cargo xtask build stm32f103
```

Each command prints the release firmware ELF path. `cargo xtask flash BOARD`
builds and flashes release firmware with [probe-rs-tools](https://probe.rs/);
it is the only task that operates on attached hardware. See the
[Pico](boards/pico.md), [CH32V307](boards/ch32v307.md), and
[Blue Pill](boards/blue-pill.md) guides for wiring and flashing.

## Firmware downloads

[CI](https://github.com/ArthurHeymans/dedibridge/actions) uploads a
`dedibridge-firmware` artifact. Tag builds publish the same files as
[release assets](https://github.com/ArthurHeymans/dedibridge/releases):

| Board | Probe firmware | Drag-and-drop firmware |
|---|---|---|
| Pico | `dedi-rp2040.elf` | `dedi-rp2040.uf2` |
| CH32V307 | `dedi-ch32v307.elf` | — |
| Blue Pill | `dedi-stm32f103.elf` | — |

Create the bundle in `artifacts/` with `cargo xtask artifacts`
(requires `elf2uf2-rs` 2.2.0). For a Pico UF2 alone:

```sh
cargo xtask build rp2040
elf2uf2-rs target/thumbv6m-none-eabi/release/dedi-rp2040 dedibridge.uf2
```

## Checks

```sh
cargo xtask fmt --check
cargo xtask test
cargo xtask check
cargo xtask clippy rp2040
cargo xtask size stm32f103
cargo xtask --help
```

- `fmt` formats workspace members, not external path dependencies.
- `test` runs host/core/protocol/task-runner tests and the Go adapter's race tests.
- `check` runs formatting, warning-denied host and firmware Clippy, and separate
  release builds for all three boards.
- `clippy BOARD` and `size BOARD` operate on one board.

External tools (`go`, `probe-rs`, `llvm-size`, `elf2uf2-rs`) must be on `PATH`
for their respective tasks. There is no default embedded target or global
`build-std` setting: plain `cargo test` tests the host crates. **Do not build or
Clippy with `--workspace`**: the MCU, architecture, executor, and time-driver
features cannot be combined in a single compilation.

## Source layout

| Directory | Contents |
|---|---|
| `boards/` | Peripherals, pin muxing, controller recovery, Pico PIO/USB paths |
| `crates/firmware-core/` | Shared USB, flash, UART, GPIO and LED services |
| `crates/protocol/` | No-std zerocopy wire layouts, SF600 parsing, device identity |
| `tools/dedibridgectl/` | USB daemon, socket clients, console and PTY |
| `integrations/dutctl/` | Go serial adapter and configuration patch |
| `xtask/` | Rust build/check/package/flash task runner |

The task runner uses `cargo_metadata` and Cargo's JSON messages to locate
firmware, including cached builds and `CARGO_TARGET_DIR` overrides.

See [implementation notes](implementation.md), [hardware checks](hardware-validation.md),
[parity review](parity-review.md), and [reliability review](reliability-design-review.md).
Software checks are not a hardware qualification.
