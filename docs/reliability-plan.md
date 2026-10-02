# Reliability improvements

The [Opus 5.5 design review](reliability-design-review.md) is a read-only
second opinion. Its source observations are not hardware qualification. Work
proceeds in separate jj changes, preserving SF600 and the existing board engines.

## Host transport and connection lifecycle

Implemented in the first follow-up:

- Eight auxiliary IN transfers in flight; UART events are drained during slow
  OUT completion. Responses received before OUT completion preserve packet
  order: post-ACK UART DATA cannot overtake a flush ACK.
- Successful malformed IN completions recycle their buffers and fail an active
  serial session. Transport failures and ambiguous command failures invalidate
  the link rather than retrying an operation with unknown effects.
- The daemon retains the socket and reopens only its initial serial, checking
  GET_INFO version and board. Absent-device calls fail fast. Admitted actions
  retain their connection generation even if channel backpressure delays them;
  stale actions cannot touch the new connection.
- UART/PTY/dutctl sessions do not reconnect. Fresh attach performs baud + flush;
  neither commands nor GPIO state are restored automatically.
- Serial command replies are enqueued by the actor, the same producer as UART
  DATA. This removes the actor/socket-thread race around reset-input replies.

Mock/software acceptance is separate from unplug/replug qualification. A
reopen is not proof of an MCU reset, nor proof that the last command did not
execute. See the [socket contract](socket-protocol.md).

## Read-only diagnostic foundation

Implemented in the second follow-up: auxiliary and socket versions remain 1.
A capability bit and a new query opcode expose three independently versioned
pages within the existing 59-byte response-data budget, using zerocopy
little-endian layouts. Old firmware reports unsupported; old hosts can ignore
the new capability. Counter snapshots across pages are not atomic. Counters
saturate rather than wrap and are monotonic since boot; RX-flush clears
reportable loss, not lifetime diagnostics. The host `diagnostics`/`diag` query
returns uptime/package version and UART/USB/flash counters.

Distinguish UART hardware-error events, queue/ring overflow, and UART bytes
lost on auxiliary IN delivery. Exact physical frame-loss counts are often
unavailable. Never record DUT contents or write every error to MCU flash.
Report unsupported/unavailable fields honestly. CH32's optional DMA page
separates hardware-error IRQ events, progress/overrun failures, and observed
ring lag high-water from USB/software-queue loss. Its RX-gap warning remains set
until DMA/timer/error-IRQ races are hardware-qualified; zero counters do not
prove that its byte stream is complete.

Reset causes and retained fault records require board-specific implementation
and retention validation. Do not invent a reliable boot counter from ordinary
zero-initialized RAM or claim noinit retention without checking linker/startup
and boot-ROM behavior.

## Continuous CH32 RX and shared batching

Implemented in the third follow-up:

- The pinned HAL's public 4 KiB circular RX DMA ring runs independently of
  consumer futures. Reads copy up to 61 available bytes; cancelling a wait does
  not stop DMA. A 1 ms timer polls short tails, avoiding IDLE/DR clearing races.
- Conservative producer-position accounting wraps the HAL copy: observations
  must occur before half a ring could arrive at 8N1 baud, unread lag must remain
  below half capacity, and the post-copy check must pass before committing.
  Delayed/collapsed TC accounting, a stopped receiver, or ambiguous copy progress
  fails the stream and restarts RX rather than delivering an uncertain copy.
  At 3 Mbaud the observation limit is about 6.8 ms. This is deliberately stricter
  than physical capacity; late TC IRQs may produce conservative false failures.
- A board-owned USART2 error IRQ latches a fault and disables RX requests. The
  task stops reception/DMA, clears SR/DR only while stopped, rebases both the
  physical producer and HAL consumer, and rearms. This is also the explicit
  flush barrier; HAL `clear()` alone would replay old bytes. Baud changes update
  BRR only after physical TX completion and rebase RX too.
- The shared RX queue is a 1 KiB byte pipe, with bounded chunk insertion/draining
  and explicit partial/full-queue loss. It replaces per-byte USB select/cancel
  work with available-byte batches; a short USB tail still has a 1 ms deadline.
  F103 drains its existing IRQ queue in chunks; Pico's engine is unchanged.
- An optional diagnostic page advertises CH32 hardware-error events, progress
  failures, and observed ring lag high-water. The original three page layouts
  stay unchanged; hosts retain base diagnostics when the optional page is absent.
  Reportable loss counters now saturate too, including failed-report restoration.

The ring covers executor latency, not 100 ms auxiliary IN stalls or indefinite
host pauses. The RX task continues draining while USB awaits completion; a full
shared pipe drops visibly. Debug halts, DMA register/TC timing, error-IRQ latching
(including TX status reads), and timer continuity still require real qualification.
The HAL's DMA transfer-error IRQ still panics; watchdog recovery is not enabled.

Keep F103 IRQ RX and Pico's existing engine unless qualification shows a need
for a different implementation. Advertised max baud is a configuration limit,
not a measured sustained-throughput guarantee. Sequence-numbered traffic under
concurrent flash/GPIO load is the required acceptance test on every board.

## Later: opt-in watchdog

Start with executor-liveness watchdog coverage for panic/spin/IRQ starvation,
not speculative USB-wedge inference. Host silence, idle UART, waiting for USB,
and legitimate erase-busy windows are healthy. Most operation waits already
have deadlines; a logical cross-task supervisor should be justified by observed
stalls and concrete cleanup obligations, not added as a generic framework.

Watchdog enforcement remains opt-in until hardware-qualified. Once started,
STM32/CH32 IWDG cannot simply be disabled. Debug halt behavior and an abnormal
reset-loop policy need explicit support. Record reset/fault evidence where
retention is proven. Do not perform complicated peripheral cleanup from a panic
handler: it may run with locks or interrupts in an unsafe state.

**A bridge reset is not electrically neutral.** It can release DUT reset or a
power-button line, allow a DUT to boot from partially written flash, and change
CS/pad pulls during startup. External pull-ups, MCU reset pad defaults, and
GPIO/CS waveforms must be checked on each real board. No automatic restoration
or replay of prior GPIO/flash/UART commands is permitted after a reset.
