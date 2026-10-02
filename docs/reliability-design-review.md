## Review: dedibridge reliability plan

**Verdict: OK with notes.** The four goals are right, but the plan needs changes before work starts. It ranks CH32 RX as a performance item when it is a correctness bug, and it treats it as a DMA-only fix when the bottleneck runs end to end. Daemon reconnect needs to move ahead of the watchdog. Stage 4 should be cut down to its well-supported core. I checked everything below in the source or in the pinned HAL, and I mark anything I couldn't verify. I made no edits and ran no builds or tests.

### What the plan already gets right
- **No replay** matches the documented contract (`docs/socket-protocol.md:123-127`). The host only retries `STATUS_BUSY`, which the firmware returns without side effects: UART_WRITE uses `try_send` (`aux.rs:225-232`) and SET_BAUD is refused while TX is busy.
- **The attach sequence is already a barrier:** `CMD_UART_SET_BAUD` then `CMD_UART_FLUSH_RX` (`daemon.rs:127-128`).
- **Most operations already have deadlines:**
  - bulk USB: 3500 ms (`bulk.rs`, `Shared::usb`)
  - `poll_wip`: 250 ms; `wait_ready`: erase window + 25 ms, cancellable (`flash.rs:69-110`)
  - aux IN: 100 ms (`aux.rs:152`); TX TC: 100 ms
  - GPIO pulses release independently of USB.
- **The 60 s chip erase is simulated:** `busy_until` returns a fake WIP (`flash.rs:113-119, 144-160`). The host polls and the firmware is never blocked. A sensible watchdog can't trip on it.

### Findings, most important first

**P1-1 — CH32 RX silently drops bytes today.** This breaks the documented promise "No bytes are silently dropped" (`socket-protocol.md:124`).
- How it happens:
  - `ChRx::read_byte` makes a new one-byte HAL DMA read for every byte (`boards/ch32v307/src/backend.rs:85-93`).
  - `detect_previous_overrun` defaults to false (`usart/mod.rs:131`). With that setting, every read starts by reading STATR then DATAR (`usart/mod.rs:504-508`). That throws away any byte that arrived between reads, with no count.
  - The read future's OnDrop clears DMAR (`:475-491`), so the line is unwatched between bytes. That gap includes the yield every 16 bytes (`aux.rs:103`).
  - After an error, `run_rx` sleeps 1 ms (`aux.rs:95-99`). That is about 300 byte-times at 3 Mbaud, recorded as a single loss.
- Consequence: Stage-1 counters would show zero loss on CH32 while bytes are being lost.
- Fix: either label CH32 RX counters untrusted until stage 2, or try a stopgap of `Config { detect_previous_overrun: true, .. }` in `Uart::new` (`main.rs:76-85`).
- Unverified for the stopgap: whether enabling DMAR with RXNE already pending makes the DMA transfer that byte. This needs a hardware test.
- F103 (IRQ ORE/FE, `stm32f103/src/backend.rs:86-99`) and Pico (`rxstall`, `rp2040/src/backend.rs:186-196`) detect their loss.

**P1-2 — Stage 2 is scoped as a DMA fix, but the throughput limit is spread across the whole path.**
- Firmware core:
  - Every RX byte goes through a 256-byte `Channel<u8>` (`aux.rs:49`).
  - `run_usb` rebuilds `select4(output.read(..), …)` for every byte (`aux.rs:265-266`). On CH32, each lost select drops a `GuardedEndpoint::read` future, which runs `PacketGuard::drop`: a critical section plus `retire_packet` (`backend.rs` Drop impl; `transport.rs` GuardedEndpoint).
  - One 3.3 µs byte-time at 3 Mbaud is about 480 cycles at 144 MHz, so this path can't keep up.
  - Sending one IN packet can block the loop for up to 100 ms (`aux.rs:152`). Meanwhile the 256-byte channel holds only 0.85 ms at 3 Mbaud.
  - Failed IN sends are added to `rx_lost` (`aux.rs:302`), so "host stopped reading" is reported as UART loss.
- Host:
  - Only 2 IN transfers are kept in flight (`device.rs` `Device::open`, `for _ in 0..2`).
  - `send()` waits up to 1 s for OUT completion and drains no IN meanwhile.
  - `Device::event` treats any transfer error, or one malformed packet, as fatal. It returns before resubmitting the buffer.
- Fix:
  1. Add a `read_chunk(&mut [u8]) -> Result<usize, UartError>` trait method that defaults to the current per-byte behaviour. Have `run_usb` fill 61-byte packets directly.
  2. Raise host IN depth to 8 or more.
  3. Count IN-timeout drops separately.
  4. Qualify a sustained baud per board. Advertised `max_baud` is 3 M (CH32, Pico) and 2.25 M (F103); none is shown to be sustainable.

**P1-3 — Pinned-HAL facts that should shape the CH32 ring design.**
- A ring buffer already exists and is public: `ReadableRingBuffer` (`dma/mod.rs` `pub use dma_bdma::*`; `dma_bdma.rs:485`). Reuse it rather than writing one, but note:
  - **`clear()` is not a flush barrier while DMA runs.** It sets `start = 0` regardless of the DMA position (`ringbuffer.rs:66-69`), so the next `read` hands back old bytes. To flush, either drop the ring (Drop stops it and spins, `dma_bdma.rs:609-616`) and rebuild it (`new` → `configure` resets the count and NDTR), or write a consumer that snapshots NDTR.
  - **Overrun detection depends on the TC IRQ counter.** It uses position plus a `complete_count` that only the TC IRQ increments (`dma_bdma.rs:140-152`; the algorithm is at `ringbuffer.rs:106-240`).
  - **A DMA transfer-error IRQ panics** (`dma_bdma.rs:138`), and on CH32 `panic_halt` means a permanent hang (`main.rs:27`).
  - **The HAL USART IRQ stops RX on any error.** With EIE set, any FE/NE/ORE clears DMAR (`usart/mod.rs:42-58`). Continuous RX would stop silently on the first framing error, which is common when a DUT power-cycles. Either don't set EIE, or bind your own USART2 handler.
  - **Clearing IDLE or error flags needs a STATR→DATAR read** (`usart/mod.rs:505-507, 583`), which races the DMA. Prefer polling at ≤1 ms (the existing 1 ms batching) over an IDLE IRQ. Read STATR error bits but don't clear them, and treat the counts as lower bounds.
  - **`reconfigure` swaps its arguments:** it passes `(cr.re(), cr.te())` into `(enable_tx, enable_rx)` (`usart/mod.rs:965-970` vs `978-996`). This is harmless while both bits are set. A TX-only `UartTx::new` sets RE=0 (`:198`), so the backend must set RE itself. For baud changes, write BRR directly as the F103 backend does.
  - **What overwrite detection can and can't see:**
    - It catches wraps through position and counter checks, as long as the TC IRQ isn't masked for close to a full buffer's worth of byte-times. It can't detect corruption inside a byte.
    - Policy should be conservative: a false positive (failed session) is acceptable, a false negative is not. Fail when lag exceeds `cap/2`.
    - Optionally also read the hardware TCIF inside the check's critical section, which needs your own consumer.
  - **Sizing:** the ring only needs to cover executor latency, not host stalls. Covering the 100 ms aux IN timeout would take about 30 KB at 3 Mbaud. 2–4 KiB on CH32 is cheap: static RAM is 11,056 B, and V307 RAM is at least 32 KiB depending on configuration (I didn't check the memory.x). Size from measured high-water marks.

**P1-4 — Reconnect has to come before the watchdog, and the current daemon fails open.**
- On USB error, the actor thread exits (`daemon.rs:215-222`), but `run` keeps accepting connections (`daemon.rs:383-394`). The result is a zombie socket that answers "USB actor stopped". Any watchdog reset ends there.
- Required semantics:
  - Fix the identity to the serial resolved at first open (`main.rs:157` may open without `--serial`).
  - Re-validate GET_INFO: version 1, same board, same serial.
  - Tag each `Action` with the device generation at the time it was accepted, and reject mismatches. If one long-lived actor channel is reused, messages queued before the gap would otherwise run on the new device.
  - While the device is absent, fail fast. Never queue across a gap.
  - Classify errors: nusb "disconnected" means rediscover; a malformed packet means log it, resubmit and count it, not kill the session.
  - A request timeout (2 s, `device.rs` `request`) is ambiguous: a GPIO pulse or write chunk may have executed. Return an error, never retry. Partial writes leave an unknown byte count; document this, since the Go adapter's `Write` returns `written` per 4096-byte chunk only.
  - The PTY path changes after reconnect, and old PTY readers get EIO. That's acceptable.
  - Don't restore GPIO after a reboot; report it through reset cause and boot count.
- Simplest alternative: exit non-zero on actor death plus a systemd restart. That's acceptable, but a tiny in-process rediscovery loop keeps the socket alive and gives clearer "device not present" errors. dutctl opens per run, so either approach works with it.

**P2-5 — Diagnostics compatibility details.**
- Don't change the aux `VERSION`: `parse_info` rejects anything except 1 (`device.rs`), and Go checks `ready.Version != 1`.
- Add `CMD_GET_DIAG` (new opcode) plus a `CAP_DIAG` bit (1<<2) in `DeviceInfo.flags`. Old firmware answers `STATUS_INVALID` (`aux.rs:185`); treat that as "unsupported". Old hosts pass unknown flags through.
- Payload is at most 59 bytes (61 minus command/status), so use paged fixed layouts with saturating little-endian u32 fields.
- On the socket, add a one-shot op `{"op":"diagnostics"}`. Old daemons reject unknown ops (`wire.rs:8`).
- Make counters monotonic since boot. Keep them separate from the reportable `rx_lost`/`tx_lost`: FLUSH resets `rx_lost` (`aux.rs:244`) but not `tx_lost`.
- Counters to split out:
  - UART: hw-error events, ring overruns, queue drops, aux IN timeout events and bytes, TX failures, RX high-water
  - USB: bulk USB timeouts, generation increments (`Shared.generation` already exists), CH32 IRQ-path retirements ("late recovery")
  - Flash: errors per `flash::Error` variant
- Identity and health fields:
  - uptime (u64 ms or u32 s)
  - reset cause, read and cleared early. CH32 needs PAC code: ch32-hal has a reset reason only for the x0 family (`rcc/x0.rs:87-114`) and no IWDG driver.
  - boot counter in noinit RAM with a magic value
  - firmware version or hash
- Skip a boot nonce: every reset re-enumerates, including F103 through its D+ pulse (`stm32f103/src/main.rs:92-98`).
- Optional: record the last panic file:line in noinit RAM. Never record DUT content.
- F103 has about 7.1 KiB of flash headroom (`hardware-validation.md`).

**P2-6 — Shrink Stage 4.**
- Facts:
  - All boards hang forever on panic: CH32 uses `panic_halt`; F103 and RP2040 use `panic_probe` → udf/HardFault.
  - Most waits are already bounded (see above).
  - On CH32 the executor is `platform-spin` (`Cargo.toml`), so it never sleeps with WFI.
- **4a (do this):**
  - A watchdog fed by one executor-liveness task (for example every 250 ms, with a 2–4 s timeout). This catches panics, HardFaults, spin loops and IRQ storms. Idle, unplugged, no-reader and erase states stay healthy automatically.
  - The panic handler should do a best-effort release of GPIO and CS, record the cause, then reset.
  - Boot-loop guard: after 3 abnormal resets each with uptime under 10 s, don't arm the watchdog.
  - Gate it behind a compile-time feature, off in debug builds. STM32/CH32 IWDG can't be stopped once started.
- **4b (defer):** "progress obligations" feeding. The legitimate step bound (≥60 s + 25 ms + 3.5 s) is longer than the hardware maximum: about 26 s for the F103/CH32 IWDG on a 40 kHz LSI, about 8.3 s for RP2040. So 4b is a software supervisor layered on top. Its only real targets are cross-task waits (bulk join, aux `gpio_result`/`baud_result`), and the source shows no deadlock there. Add it only if Stage-1 data shows a logical stall that 4a misses. Don't infer a "USB wedge" from host silence: it looks the same as an unplugged cable or a host that isn't reading.
- **Electrical:** a reboot changes the DUT's state.
  - The outputs start released (`OutputOpenDrain … Level::High`), so a DUT held in reset will boot, possibly from a half-written flash, and a powered-off DUT will power on.
  - During reset, F103/CH32 pins float.
  - I believe (not verified here) that RP2040 pads reset with pull-downs on, which can pull CS, reset or power lines low if the external pull-ups are weak. Scope this.
- **Unverified:** whether CH32V307 has an IWDG freeze-on-debug bit (STM32F1 DBGMCU and RP2040 pause-on-debug do exist), and whether noinit RAM survives an IWDG reset on CH32, and on RP2040 given what the bootrom uses.

**P2-7 — Overengineering to avoid:** the IDLE-line IRQ, a boot nonce, the 4b supervisor, a DMA rewrite for F103 (qualify its baud instead), and autonomous "wedge" detection. A host-triggered reboot command is optional and only useful if aux still works; if added, never replay it.

### Recommended stages (smallest sound steps)
- **S0, host only, testable with mocks:**
  - error classification and resubmit in `Device::event`
  - IN depth 8 or more
  - daemon fails closed when the actor dies
- **S1:** diagnostics as in P2-5, plus the CH32 RX stopgap or the "untrusted counter" label.
- **S2:** reconnect with a generation gate (P1-4).
- **S3:** CH32 ring (P1-3) plus `read_chunk` in the core. F103 and Pico keep the default, but run regressions on all three boards.
- **S4a:** watchdog. **S4b:** only if the data justifies it.

### Acceptance tests
**Software:**
- Ring consumer on a mock DMA controller:
  - wrap at `cap−1`, `cap`, `cap+1`, `2·cap`
  - TC IRQ arriving late
  - partial-tail reads
  - flush mid-wrap: no stale bytes after the ACK
  - consumer cancelled between copy and commit: no duplicated and no skipped bytes
- Counter semantics: monotonic counters vs reportable ones.
- `GET_DIAG` against old firmware returns `INVALID` and the host reports "unsupported".
- Daemon:
  - a disconnect fails every session
  - queued old-generation actions are rejected
  - `request()` call counts prove nothing is replayed
  - a different serial, board or version is refused
  - while absent, requests fail fast
- Go adapter: disconnect mid-`Write` returns an error with no retry.

**Hardware:**
- CH32 at 115200 / 921600 / 2 M / 3 M, with a sequence-numbered external source:
  - bursts of 1 KiB to 1 MiB while flashprog reads and writes and GPIO pulses run
  - every byte delivered, or an explicit loss event; never a silent gap
- Pause the reader (SIGSTOP) for 10 ms, 100 ms and 5 s: expect an explicit overflow, then recovery after a flush.
- Inject framing errors, a break and a DUT power-cycle: RX must keep running.
- Latency for a single byte ≤2 ms.
- Flush and baud barriers: no pre-ACK bytes after the ACK, except a frame already in flight.
- Unplug/replug and MCU reset during idle, UART traffic, a bulk IN/OUT transfer and a pulse:
  - old clients fail
  - new clients succeed only after revalidation
  - swapping in device B on the same port must not bind it
- Watchdog:
  - with a debug-only panic or spin trigger, it resets within T and the reset cause is reported
  - boot-loop guard trips
  - no reset during a 60 s chip erase, a 32 MiB read, 10 min unplugged, or 10 min with no reader
- Debugger halt behaviour, for each board.
- Scope reset, power and CS across a watchdog reset, including the RP2040 pull-down case.

### Residual risks
- I didn't verify any hardware-dependent claim: DMA request on a pending RXNE, CH32 debug freeze, noinit retention, RP2040 pad defaults on the real wiring.
- The HAL ring's correctness assumes the DMA IRQ is never masked for long, and ch32-hal is a fork that hasn't been qualified.
- Sustained throughput on every board is unknown until measured.
- An automatic reboot always changes the DUT's GPIO state.