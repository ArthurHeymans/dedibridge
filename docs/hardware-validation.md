# Hardware acceptance checks

These tests require real boards and a known SPI flash/DUT. They have **not**
been run on the combined firmware. Do not confuse successful cross-linking or
host mocks with hardware qualification.

## Software validation

Local validation passes 45 Rust tests, host/core/protocol and all three firmware
Clippy targets with warnings denied, Go race tests, and the patched dutctl
serial-module/configuration tests. Focused tests cover QPI dispatch, abandoned
IN/OUT session cancellation (including maximum-size IN), erase-window retention,
wait/settle/cancellation, endpoint-indexed guard lifetimes/final ACK and exactly-once cached OUT/reset
invalidation, named GPIO
commands/combined set, successful/USB-failed multi-block IN/OUT pipeline termination,
independent positive-duration pulse release despite a
stalled USB reply, and preservation of PASS/BUSY on failure. The endpoint
mocks validate the transport contract, **not CH32 register timing**. Reliability
coverage also includes BUSY-only retry/no replay, partial writes, generation-safe
reconnect and identity refusal, flush reply ordering, diagnostic compatibility
and saturation, chunked queue wrap/full drops and USB short tails, and conservative
DMA progress checks across wraps, long stalls, copy/commit, and fresh epochs.
Those checks model accounting policy, not the HAL's physical DMA writer.

All three release ELFs link. Local artifact generation and actionlint pass;
Pico UF2 blocks were checked for magic, RP2040 family, addresses, and ordering.
GitHub-hosted CI/tag publishing itself has not been run. Release section totals:

| Target | Flash (`text + data`) | Static RAM (`data + bss`) |
|---|---:|---:|
| RP2040 | 68,728 B | 7,556 B |
| CH32V307 | 52,036 B | 16,264 B |
| STM32F103C8 | 63,996 B | 8,780 B |

Small linker alignment gaps are excluded. Static RAM includes task storage and
RTT buffers, but not peak interrupt/runtime stack use. F103 leaves only about
1.5 KiB flash (before alignment) and 11.4 KiB RAM before runtime stack use; USB
PMA is separate. Further firmware features need explicit size checks. The retained
PIO dependency emits a third-party `proc-macro-error2` future-compatibility
warning; it does not currently fail the build.

## Physical checks

For each board:

1. Inspect the pinout, I/O voltage, CS/WP/HOLD pull-ups, crystal, USB pull-up,
   and debugger wiring. Verify reset/power outputs start released and the idle
   flash bus is Hi-Z; check Pico IO2/IO3 as well as CS/SCK/MOSI.
2. Flash the appropriate ELF with `cargo xtask flash BOARD`. Verify USB descriptors,
   the unique serial, EP1/2 on interface 0, and EP3/4 on interface 1. F103 must
   fit its 64 KiB flash/20 KiB SRAM/512-byte PMA budget. CH32 bulk descriptors
   must be 512-byte MPS on a high-speed link.
3. Run flashprog identify, read twice and compare, write a disposable image,
   verify, and read it back. Exercise 3- and 4-byte addressing. Pico: additionally
   exercise dual/quad reads; CH32/F103 must reject those modes. For Pico's
   experimental QPI path, capture all opcode/address/data widths and qualify a
   documented flash's QPI entry/read/exit sequence; ordinary transceive/program
   commands remain single-lane. Do not infer QPI qualification from dispatch tests.
4. Kill flashprog during IN and OUT bulk transfers, unplug/reset USB, and start
   a fresh session without rebooting the MCU. Observe CS release, endpoint
   recovery, queued verify behavior, final-packet delivery and error LEDs. On
   CH32, READ_PROG_INFO without a USB reset must cancel abandoned IN/OUT before
   a new SPI probe. Erase then WREN/WRDI/status must preserve synthetic WIP; bulk
   reads/writes submitted early must wait through every erase window plus settling.
   Repeat against a delayed-WIP emulator. Set PASS/BUSY, induce an error, and
   confirm only ERROR is added.
5. Start the daemon with explicit serial selection. Attach UART through the
   socket/PTY at 115200, then a lower/higher baud. Verify binary bytes, framing
   errors, full TX queues, and that baud changes wait until TX is physically idle.
6. With a UART client active, run state/reset/power/poweroff while flashprog is
   reading. Stop the host from reading and unplug USB while a pulse is active:
   the GPIO service must still release reset/power at its deadline.
7. Saturate UART output and stop the terminal: expect an explicit stream error,
   not a wedged management interface or apparently successful lost writes.
   Reattach and verify the RX flush discards old bytes, including driver buffers.
8. Connect two units: require explicit selection, check distinct identities,
   and confirm the two daemons never control the wrong board.
9. Apply the dutctl adapter and run real send/expect/monitor sequences, timeout,
   cancellation, serial-client exclusivity, and USB-disconnect scenarios.

F103-specific risks to verify: clone/pull-up differences, D+ startup reset,
SPI2 pin mode transitions, DMA IRQ routing, final bulk-IN completion before
endpoint cleanup, and recovery after a bus reset/deconfiguration. Test at the
550-baud lower divider limit too; lower rates must fail explicitly.

CH32-specific USB checks: send GPIO_GET_STATE with no IN reader; let the 100 ms
response timeout elapse, then read late while UART is quiet. Both auxiliary and
SF600 commands must still progress. Repeat with completion pending exactly at
timeout, in-flight completion arriving after cleanup, cancelled OUT reception,
and bus reset/deconfiguration during cancellation. Confirm DATA toggles advance
exactly once, completed packets are not discarded, and another endpoint's pending
completion is never cleared. These races require real controller traces.

## Reliability qualification (still outstanding)

- Run sequence-numbered UART at 115200/921600/2 Mbaud, plus 3 Mbaud where
  supported, in 1 KiB–1 MiB bursts during concurrent flash/GPIO/diagnostic work.
  Capture exact missing/duplicate bytes, errors, queue and DMA lag high-water,
  and CPU service latency. Validate the 1 ms DMA poll + 1 ms USB tail budget;
  a single quiet-line byte should reach the host within 2 ms under normal load.
- CH32: force delayed TC servicing, cap−1/cap/cap+1/two-cap producer advances,
  wraps during copy, cancellation while waiting, and flush/baud changes at a
  wrap or a partial frame. Verify no old bytes reappear after ACK. Check timer
  monotonicity and DMA/PFIC/debug-freeze behavior; ambiguous intervals must fail
  rather than appear as an empty ring. Late TC races may fail conservatively.
- Inject framing/noise/break/overrun and DUT power cycles, including during TX
  TC status polling. Verify the CH32 error IRQ latches the failure, stops RX,
  and the task safely rearms. DMA transfer-error panic remains a known unhandled
  recovery case until opt-in watchdog work is qualified.
- Pause readers for 10 ms/100 ms/5 s. Queue/USB loss must be visible; diagnostics
  stay monotonic and explicit flush alone clears reportable RX loss. Check both
  old firmware (unsupported diagnostics) and optional-page fallback.
- Unplug/replug/reset during UART, bulk IN/OUT, idle and pulses. Old leases and
  queued generations must fail; no uncertain write, GPIO state or session is
  replayed. Swap devices and verify serial/board/version refusal.
- Before enabling watchdogs, scope DUT reset/power/CS during bridge reset on
  every board. Exercise panic/spin/reset loops/debugging and long erase/read,
  idle/no-reader/unplug cases. Host silence is not a reset condition. Reset
  causes and retained boot/fault evidence are not implemented or qualified.

RP2040-specific risks to verify: the retained direct-register EP2 double-buffer
fast path/reservation, final-buffer draining, cancellation at a DMA boundary,
and PIO UART flush/error detection. Register-level optimizations are intentionally
board-owned and need physical tests.
