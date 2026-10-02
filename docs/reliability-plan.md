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
Report unsupported/unavailable fields honestly. In particular, CH32's current
per-byte RX path has unreported loss between reads; zero counters do not prove
that its byte stream is complete.

Reset causes and retained fault records require board-specific implementation
and retention validation. Do not invent a reliable boot counter from ordinary
zero-initialized RAM or claim noinit retention without checking linker/startup
and boot-ROM behavior.

## Next: continuous CH32 RX and shared batching

Reuse the pinned HAL's public DMA ring where its safety contract is sufficient.
Investigate the ring's consumer and completion-counter behavior, not only its
API. Circular DMA runs continuously while consumer futures may be cancelled.
Use chunked RX into bounded shared queues and bounded USB batching to avoid
per-byte endpoint cancellation. Poll partial tails on a bounded schedule unless
an IDLE IRQ can be proven not to steal a byte during flag clearing.

Flush must stop/rebase the producer/consumer and clear hardware state before
ACK; HAL ring `clear()` alone is not a running-DMA barrier. Baud changes occur
only with TX idle. USART framing/break/overrun errors must be observable and RX
must recover rather than silently remaining disabled. Missed wraps or ambiguous
DMA progress fail the stream conservatively. Ring sizing covers service latency,
not indefinite USB stalls; high-water measurements inform the final capacity.

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
