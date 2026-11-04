# Browser firmware installer

[← Building](building.md) · [Pico](boards/pico.md) · [CH32V307](boards/ch32v307.md)

The installer in [`web/`](../web/README.md) is a static **GitHub Pages + WebUSB**
application for the original RP2040 Pico/Pico H and CH32V307VCT6. Blue Pill is
out of scope. It installs DediBridge onto the MCU, not onto an external SPI
flash attached to DediBridge.

**Status: USB installation and programmer operation have been tested end to
end on Pico and CH32V307VCT6.** CI also runs image/protocol tests and headless
browser tests. Successful default-branch and tag builds deploy the installer;
PRs and other branches never deploy.

## Use and fallbacks

The installer is served at <https://arthurheymans.github.io/dedibridge/>. Desktop Chrome or Edge with
[WebUSB](https://developer.mozilla.org/en-US/docs/Web/API/WebUSB_API) and HTTPS
is required; Firefox/Safari users retain download/manual-install instructions.

1. Choose your exact board and the bundled firmware, or explicitly choose a
   trusted local UF2/BIN. Bundled downloads are size-limited and SHA-256 checked.
2. Disconnect external targets. Enter the factory bootloader manually:
   **BOOTSEL** on Pico; **BOOT0 high / BOOT1 low at reset** on CH32.
3. Click **Connect bootloader**, select the USB device, check the detected
   identity and image, and confirm the erase warning. For CH32, restore BOOT0
   low **without resetting** before installation.
4. Install. Programming and verification are mandatory; only then is reboot
   requested. A reboot error is distinguished from a programming/verify error.
   Check normal DediBridge enumeration afterwards.

Stop closes the USB session; it cannot undo an erase. Re-enter bootloader mode
and reconnect before retrying an interrupted install. No automatic reboot from
the running DediBridge firmware into its ROM bootloader is assumed.

Windows may need WinUSB on the vendor bootloader interface, **not** the Pico
mass-storage interface. Linux needs bootloader-specific udev permissions;
a runtime-only rule does not cover the ROM bootloader. The installer's **USB
access & udev rules** panel includes the canonical rules download, install/reload
commands and NixOS setup. It opens automatically on access-denied errors. Apply
the rules, unplug/replug in BOOTSEL/ISP mode, and reconnect; see
[USB permissions](usage.md#usb-permissions). Manual options:

- Pico: download UF2, hold BOOTSEL while plugging in, copy to `RPI-RP2`.
- CH32: enter ISP and run `wchisp info`, then
  `wchisp flash dedi-ch32v307.bin`. See the [board guide](boards/ch32v307.md).

## Implementation and dependencies

The UI is framework-free TypeScript, HTML and CSS. `images.ts` validates inputs;
`releases.ts` handles same-origin release downloads; `installer.ts` owns the
single USB session and confirmation/operation gates; independent `backends/`
adapters implement WCH ISP and PICOBOOT. No CDN scripts, runtime GitHub token,
CORS proxy or backend service are used.

### CH32V307

The installer pins the fixed
[ArthurHeymans/wchisp-web fork](https://github.com/ArthurHeymans/wchisp-web) at
[`cc4038b`](https://github.com/ArthurHeymans/wchisp-web/tree/cc4038be6a4498be2fbe3143abb2a973d9d4e7e4).
Upstream npm `wchisp-web` 0.2.1 did not identify CH32V307 correctly: the family
used device type `0x10`, the variant `0x17`, and the family filter prevented a
match. The fork fixes variant-aware lookup and CH32V30x metadata against native
[wchisp's device table](https://github.com/ch32-rs/wchisp/blob/main/devices/0x17-CH32V30x.yaml),
adds regression tests, rejects unknown/protected chips and invalid firmware
before destructive operations, implements WebUSB transfer deadlines/cleanup,
and exports ESM correctly for browser bundlers.

DediBridge accepts only VCT6 (`0x70`, device type `0x17`), checks identity/config
again before erase, rejects read/write protection and the incompatible 32 KiB
SRAM partition, and never writes option bytes or automatically unprotects.
Raw BIN is capped at 192 KiB to fit all supported flash partitions. WCH ISP
VERIFY is mandatory. The factory bootloader uses its own USB identity, not the
running DediBridge interface or the board's WCH-Link debugger.

### Pico

The MIT-licensed
[pico⚡flash PICOBOOT library](https://github.com/piersfinlayson/picoflash/tree/9f647ee9c46a21f4d1efc2d272f19be6cd309d1d)
is vendored unmodified at an exact revision with attribution. Only RP2040 is
enabled. UF2 must have family ID `0xe48bff56`, complete unique block numbers,
256-byte payloads, and contiguous, non-overlapping addresses starting at flash
offset zero within the original Pico's 2 MiB. Erase is sector-aligned; written
bytes are read back and compared before reboot.

USB identity does not distinguish every RP2040 board. Board confirmation is
still required; Pico W, Pico 2 and arbitrary RP2040 boards are not supported.
A raw CH32 BIN likewise cannot prove its intended board. Local images are
explicitly labelled unauthenticated; structural checks are not release signing.

### Licensing and WASM

The browser application is **GPL-2.0-only**, separately from DediBridge's
MIT/Apache firmware. Builds publish the installer and WCH source/build inputs
alongside the application, plus GPL and Pico's MIT licenses.

WASM is not required for USB access and is deliberately absent from this
implementation. A future Rust/WASM backend would need a portable ISP state
machine and asynchronous transport abstraction: native `wchisp` currently uses
libusb/native serial dependencies, which cannot simply compile to WebUSB.
Neither WASM nor JavaScript bypasses browser permissions or OS driver setup.

## Release delivery

CI tests/builds the web app on pushes and PRs, including headless Chromium
checks at the `/dedibridge/` project path. Firmware is built once. Successful
default-branch and tag builds publish the matching installer and images on Pages.
Tag builds also publish Pico ELF/UF2, CH32 ELF/BIN and `SHA256SUMS` as Release assets.
The Pages job downloads those same artifacts, verifies all checksums and image
structure, generates a manifest with the tag/commit/hash/size, and copies the
exact bytes into the static site. It does not rebuild firmware or fetch GitHub
Release redirects from browser JavaScript.

Only the deployment job has Pages/OIDC write permissions. Actions and the WCH
dependency are commit-pinned; PRs never deploy. Enable Pages' **GitHub Actions**
source and allow the default branch and release tags in the `github-pages`
environment if deployment protection rules are configured. The repository
homepage links directly to the installer. The app always displays the bundled
branch/tag and source commit. Checksums detect corruption, not compromise of the release/site itself.

For local commands and code layout, see [`web/README.md`](../web/README.md).

## Validation coverage

The operator confirmed end-to-end installation and programmer operation on both
supported boards. This does not assert coverage of every OS/browser version or
all fault-injection cases. Additional coverage should record exact board,
bootloader, browser and OS versions and exercise physical disconnects, protected
or incompatible CH32 configurations, interrupted writes and ROM-bootloader
recovery. CI separately checks image/hash equality, project-path URLs and mocked
failure cases.
