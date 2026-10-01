# DediBridge

One SF600-compatible programmer application, three hardware backends, and one
UART/board-control interface. Extracted from the sibling `dedipico` and
`dedich32` projects; neither original checkout is modified.

| Board | Flash engine | USB | SPI lanes |
|---|---|---|---|
| RP2040 / Pico | PIO + DMA, double-buffered USB IN | Full speed | single / dual / quad |
| CH32V307VCT6 | SPI2 + DMA | High speed | single |
| STM32F103C8 / Blue Pill POC | SPI2 + DMA | Full speed | single |

All boards use the same USB request handler, bulk queue/cancellation logic,
auxiliary protocol, UART batching/backpressure, and GPIO pulse service. CH32
and STM32 also share the entire single-lane flash implementation. Backend
capabilities are explicit; unsupported multi-lane reads are rejected, not ACKed
and silently performed incorrectly. QPI and AAI programming are not supported.

**Status:** software tests and cross-linked firmware builds are available. The
new combined firmwares, especially F103 USB recovery/pin muxing and the retained
RP2040 fast path, still need hardware validation. Building is not evidence of
successful flashing or USB interoperability. See [hardware checks](docs/hardware-validation.md).

## Build and flash

Rust stable is used for host tooling and ARM targets. The CH32 custom target
requires nightly with `rust-src`. The CH32 HAL is pinned to the published
`ArthurHeymans/ch32-hal` fork at `23f0e114`, including its USBHS recovery fixes;
no sibling HAL checkout is needed.

```sh
rustup toolchain install nightly --component rust-src
./dev test
./dev check
./dev build rp2040
./dev build ch32v307
./dev build stm32f103
./dev size stm32f103
# Explicit hardware operation, with probe-rs-tools installed:
./dev flash stm32f103
```

`./dev fmt` formats only this workspace. `./dev check` runs host Clippy and
builds each board separately. Do not use
`cargo build --workspace` or `cargo clippy --workspace`: architecture, MCU,
executor, and time-driver features cannot be combined into one compilation.
The workspace has **no default embedded target or global build-std setting**;
plain `cargo test` tests protocol/core/host crates on the host.

Pico UF2 creation remains available through `elf2uf2-rs`:

```sh
elf2uf2-rs target/thumbv6m-none-eabi/release/dedi-rp2040 dedibridge.uf2
```

F103 uses the official C8 64 KiB flash / 20 KiB SRAM limit, not the unofficial
128 KiB sometimes found on clones. Its 512-byte USB packet RAM uses 448 bytes
(64-byte endpoint table, 128-byte EP0, four 64-byte bulk buffers). RP2040's EP15
reservation is not allocated on other boards. SPI2 on F103 runs at up to 18 MHz
with this 72 MHz clock configuration. CH32 requires a high-speed USB link; a
full-speed fallback descriptor configuration is not implemented.

## Wiring

| Function | Pico | CH32V307 | STM32F103C8 |
|---|---|---|---|
| Flash CS / SCK / MOSI / MISO | GP7 / GP2 / GP3 / GP4 | PB12 / PB13 / PB15 / PB14 | PB12 / PB13 / PB15 / PB14 |
| Flash IO2 / IO3 (WP# / HOLD#) | GP5 / GP6 | external pull-ups | external pull-ups |
| UART TX / RX | GP0 / GP1 | PA2 / PA3 | PA2 / PA3 |
| RESET# / POWER_SW# | GP8 / GP9 | PB8 / PB9 | PB8 / PB9 |
| Power-state / auxiliary input | GP10 / GP11 | PB10 / PB11 | PB10 / PB11 |
| Pass / Busy / Error LED | GP25 / GP14 / GP15 | PC0 / PC1 / PC2 | PC13 (active-low) / none / none |
| USB | onboard | PB6 / PB7 USBHS | PA11 D- / PA12 D+ |

F103 assumes an 8 MHz HSE crystal and a proper external **1.5 kΩ D+ pull-up**.
Check Blue Pill boards: incorrect pull-up resistors and non-ST clones are
common. The firmware pulses D+ low at startup to force re-enumeration.

Flash outputs are released when idle; CS is parked with a weak pull-up. Pico
also releases IO2/IO3 overrides. Use appropriate external CS/WP/HOLD pull-ups.
Hi-Z is not electrical isolation: ensure the target controller has released the
bus before programming. UART is **3.3 V TTL**, crossed TX/RX with shared ground.
Do not connect two flash power supplies together; 1.8 V targets need level
shifting and a suitable supply. Reset/power outputs only pull low or release;
use external transistors/level shifting for 5 V control lines unless the
open-drain electrical design has been checked. POWER_SW# is a button input,
not a power supply switch.

## Flashprog

Interface 0 remains VID:PID `0483:dada`, SF600 V7.2.22 / protocol V3, with fixed
EP1 OUT and EP2 IN. Device serial and flashprog EEPROM selection ID are derived
from the board's unique ID. No alternative flash programmer CLI is introduced.

```sh
flashprog -p dediprog --flash-name
flashprog -p dediprog -r dump.bin
flashprog -p dediprog -w image.bin
# Pico only:
flashprog -p dediprog:iomode=quad -r dump.bin
```

Voltage commands are compatibility stubs: **there is no voltage switching**.

## Unified host interface

The daemon needs USB permissions. `udev/70-dedibridge.rules` provides desktop
`uaccess` rules. For headless NixOS, assign the daemon/dutagent account to a
USB-access group instead:

```nix
users.groups.dedibridge = {};
users.users.dutagent.extraGroups = [ "dedibridge" ];
services.udev.extraRules = ''
  SUBSYSTEM=="usb", ATTR{idVendor}=="0483", ATTR{idProduct}=="dada", GROUP="dedibridge", MODE="0660"
'';
```

Run the daemon and dutagent as the same account for the owner-only socket.

```sh
./dev host list
./dev host --socket /run/user/1000/board-a.sock daemon --serial YOUR_USB_SERIAL
# Separate terminals, while flashprog owns interface 0:
./dev host --socket /run/user/1000/board-a.sock state
./dev host --socket /run/user/1000/board-a.sock reset
./dev host --socket /run/user/1000/board-a.sock poweroff
./dev host --socket /run/user/1000/board-a.sock console --baud 115200
# Or obtain a PTY for picocom and other terminal programs:
./dev host --socket /run/user/1000/board-a.sock pty --baud 115200
```

The daemon exclusively claims **interface 1**, never the flash interface.
Multiple devices require explicit serial selection and separate socket paths.
Only one UART client is permitted per daemon, but GPIO RPCs remain available
while that client is connected. Exit a terminal console with **Ctrl+]**; Ctrl+C
is sent to the DUT. PTYs and redirected binary streams have no local escape.
Changing a PTY's termios baud does **not** set
the hardware baud; select it with `--baud` when attaching.

The versioned socket API supports binary UART data, acknowledged writes, an
RX-flush barrier, and explicit errors for overflow/disconnect. Firmware queues
are bounded. Busy USB writes are retried within a deadline rather than dropped;
an overloaded console fails visibly. A write ACK means accepted into the
firmware TX queue, not delivered to or acknowledged by the DUT. Already accepted
UART bytes are not cancelled by closing a client. GPIO pulses run in a separate
firmware task and still release if USB stalls or disconnects.

CH32 UART currently retains byte-at-a-time RX DMA from the original firmware;
high rates or blocking control transfers can overrun it. Overflow is explicit,
but reliable peak throughput needs hardware qualification and possibly buffered
RX. Pico flush restarts its RX state machine and discards any partial frame.

Hotplug/reconnect and automatic daemon supervision are not implemented; restart
the daemon after a USB disconnect. Host tooling is Linux/Unix-specific (PTY,
Unix socket, `nusb`). Protocol additions require the combined firmware; the
host deliberately rejects an old auxiliary interface without GET_INFO/version
support.

## dutctl serial integration

`integrations/dutctl/` contains a drop-in transport adapter, tests, and a small
configuration patch for `~/src/dutctl-9e`. Its existing send/expect/monitor engine
is reused without modification. The patch was tested against a private copy of
that checkout; the original is untouched.

See [integration instructions](integrations/dutctl/README.md). Configuration:

```yaml
uses:
  - module: serial
    passthrough: true
    with:
      dedi_socket: /run/user/1000/board-a.sock
      baud: 115200
```

Use `dedi_socket` **instead of** `port`. Both cannot be configured together.
Reset/power can already be invoked through `dedibridgectl` using dutctl's shell
module; a native dutctl GPIO module is a separate follow-up.

## Layout

- `crates/protocol`: no-std, zerocopy wire layouts, SF600 parsing, stable identity.
- `crates/firmware-core`: hardware-independent USB handler, shared bulk worker,
  single-lane flash engine, UART services, GPIO/LED policy.
- `boards/*`: peripheral construction, task wrappers, pin muxing, controller
  recovery, and RP2040 PIO/USB fast paths.
- `tools/dedibridgectl`: USB actor, daemon, socket clients, console/PTY.
- `integrations/dutctl`: standalone-testable Go serial adapter and patch.

The small boundaries (`Flash`, `SingleSpi`, `UartRx/UartTx`, `Recovery`,
`BulkIn/BulkOut`) make cancellation, bus release, physical TX completion, and
packet completion explicit. They are application contracts, not a new generic
MCU HAL. See [socket protocol](docs/socket-protocol.md) and [source attribution](NOTICE.md).
