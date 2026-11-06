# Flash, UART and board control

[← DediBridge](../README.md)

## USB permissions

`udev/70-dedibridge.rules` grants the active desktop user access to the runtime
(`0483:dada`), Pico BOOTSEL (`2e8a:0003`) and WCH ISP (`4348:55e0` / `1a86:55e0`)
USB devices. Selecting a device in the browser does not grant these OS permissions.
On systemd Linux, install it from the repository root:

```sh
sudo install -m 644 udev/70-dedibridge.rules /etc/udev/rules.d/70-dedibridge.rules
sudo udevadm control --reload-rules
```

Unplug/replug the board in the desired mode afterwards, then reconnect in the
browser. Keep the `70-` filename: `uaccess` tags must be present before
`73-seat-late.rules` applies the active user's ACL. Do not run the browser as root.

On NixOS, install the rules declaratively (place the downloaded file beside the
Nix module), then run `sudo nixos-rebuild switch` and replug:

```nix
services.udev.packages = [
  (pkgs.writeTextDir "etc/udev/rules.d/70-dedibridge.rules"
    (builtins.readFile ./70-dedibridge.rules))
];
```

On headless NixOS, give the daemon's account access through a group instead
(the following example covers the runtime device only):

```nix
users.groups.dedibridge = {};
users.users.dutagent.extraGroups = [ "dedibridge" ];
services.udev.extraRules = ''
  SUBSYSTEM=="usb", ATTR{idVendor}=="0483", ATTR{idProduct}=="dada", GROUP="dedibridge", MODE="0660"
'';
```

## Program flash

```sh
flashprog -p dediprog --flash-name
flashprog -p dediprog -r dump.bin
flashprog -p dediprog -w image.bin
# Pico only:
flashprog -p dediprog:iomode=quad -r dump.bin
flashprog -p dediprog:iomode=dual -r dump.bin
```

[rflasher](https://github.com/ArthurHeymans/rflasher) also supports the `dediprog`
backend:

```sh
rflasher probe -p dediprog
rflasher read -p dediprog dump.bin
rflasher write -p dediprog image.bin --allow-full-chip
```

rflasher is alpha software; its documentation recommends flashprog for critical
work. Read a backup before writing with either tool.

The device identifies as an SF600 (VID:PID `0483:dada`, firmware V7.2.22,
protocol V3). Its serial and flashprog EEPROM selection ID come from the board's
unique ID. **Voltage commands do not switch power.** Read the
[wiring precautions](wiring.md) first.

### Recover an interrupted SPI transfer

Pico keeps a failed SPI USB pipe halted rather than guessing the host's DATA
toggle and returning shifted data. Stop the programmer, then explicitly recover
both pipes before starting a fresh operation:

```sh
cargo xtask host recover-spi --serial YOUR_USB_SERIAL
```

No daemon is needed. Recovery claims interface 0 exclusively (and refuses if a
programmer still owns it), cancels old SPI work and resets the device and host
pipe state. It does not reset the MCU, touch GPIO/power, or replay flash writes;
interface 1 and its UART lease stay independent. An interrupted write remains
uncertain: read back the flash before deciding what to program next.

## Start the host daemon

```sh
cargo xtask host list
cargo xtask host --socket /run/user/1000/board-a.sock daemon --serial YOUR_USB_SERIAL
```

Replace the socket path with one in your own runtime directory. For multiple
boards, select each USB serial explicitly and use a separate socket per daemon.
The daemon claims **interface 1 only**; SPI clients use interface 0. Clients
that retain the active USB configuration can run concurrently. Stop the daemon
before using a flashprog build that calls `SET_CONFIGURATION`: that operation
conflicts with claimed interfaces and resets both interfaces, not just SPI.
Run clients and the daemon as the same account: the socket is owner-only. Host tooling requires Linux/Unix (PTYs, Unix sockets, `nusb`).

## Console and control

In other terminals, using the same socket path:

```sh
cargo xtask host --socket /run/user/1000/board-a.sock console --baud 115200
cargo xtask host --socket /run/user/1000/board-a.sock state
cargo xtask host --socket /run/user/1000/board-a.sock reset
cargo xtask host --socket /run/user/1000/board-a.sock poweroff
cargo xtask host --socket /run/user/1000/board-a.sock diagnostics
# For picocom or another terminal program:
cargo xtask host --socket /run/user/1000/board-a.sock pty --baud 115200
```

Only one UART client can attach to a daemon; GPIO calls still work during a
serial session. Exit `console` with **Ctrl+]**; Ctrl+C goes to the target.
PTYs and redirected binary streams have no local escape. A PTY's termios baud
setting does **not** change the hardware baud: use `--baud` when attaching.

Named GPIO commands include `dir reset out`, `set reset 0`, `release reset`,
and `pulse power 500`. Pin names are `reset`, `power`, `power-state`, and `aux`;
the last two are input-only. Numeric `direction MASK VALUES`,
`output MASK VALUES`, and `pulse MASK MS` are also supported. `set` combines the
original direction-then-output sequence in one daemon actor operation.

A pulse ACK means the pulse **started**, not that it finished or that the target
is off. Wait for release before issuing another pulse. A long power-button hold
can start an already-off target; it is not an idempotent off command. Confirm
that the target and any standby flash master have released SPI before programming.
Idle Hi-Z pins and an expired button pulse do not establish bus ownership.

`diagnostics` (or `diag`) reads lifetime UART/USB/flash counters, uptime, and
firmware version without resetting counters. Zero counters are not evidence
of a lossless stream. See [diagnostic layouts](socket-protocol.md#read-only-diagnostics).

For the full CLI: `cargo xtask host -- --help`.

## Disconnects and errors

Overflow and disconnects fail visibly. After USB failure the daemon keeps its
socket and rediscovers only its selected serial, but **does not replay actions
or reopen serial sessions**. Reopen your console/PTY or start a new dutctl run
after reconnect. A UART write ACK means accepted into the firmware queue, not
delivered to the target. Closing a client does not cancel already accepted
bytes. See the [uncertain-execution contract](socket-protocol.md#reconnect-and-uncertain-execution).

The host requires the combined firmware's GET_INFO/version support; old
DediPico/dedich32 auxiliary firmware is not compatible. Firmware watchdog
resets and automatic process supervision are not enabled.

## dutctl

The [dutctl adapter](../integrations/dutctl/README.md) reuses its existing
send/expect/monitor engine. Configure `dedi_socket` **instead of** `port`:

```yaml
uses:
  - module: serial
    passthrough: true
    with:
      dedi_socket: /run/user/1000/board-a.sock
      baud: 115200
```

Use dutctl's shell module to invoke `dedibridgectl` for reset/power commands;
a native GPIO module is not included.
