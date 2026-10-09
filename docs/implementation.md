# Implementation notes

[← DediBridge](../README.md) · [Building](building.md) · [Socket protocol](socket-protocol.md)

## Shared firmware

All boards share the USB request handler, bulk queue/cancellation logic,
auxiliary protocol, UART batching/backpressure, and GPIO pulse service.
CH32 and STM32 also share the single-lane flash engine. Unsupported multi-lane
reads are rejected rather than silently performed as single-lane reads.
SST word programming uses Dediprog bulk AAI mode: each 256-byte data block
starts an addressed `0xAD` stream, polls WIP between words and ends with WRDI.
Error and cancellation paths also attempt WRDI before releasing flash ownership.

The `Flash`, `SingleSpi`, `UartRx/UartTx`, `Recovery`, and `BulkIn/BulkOut`
boundaries express cancellation, bus release, physical TX completion, and
packet completion; they are application contracts, not a generic MCU HAL.

Interface 0 has fixed EP1 OUT and EP2 IN. Pico reserves EP15 for its USB IN
double-buffer path; other boards do not allocate that reservation. F103 uses
448 of its 512 bytes of USB packet RAM: a 64-byte endpoint table, 128-byte EP0,
and four 64-byte bulk buffers.

## UART and GPIO

The socket API provides binary UART data, acknowledged writes, an RX-flush
barrier, and explicit overflow/disconnect errors. Busy USB writes are retried
within a deadline. Firmware queues are bounded, and overloaded consoles fail
visibly. GPIO pulses have their own task and release even if USB stalls or
disconnects.

CH32 reception uses a continuously running 4 KiB DMA ring, bounded chunk reads,
and a 1 ms tail poll. Cancelling a consumer wait does not stop reception.
Ambiguous DMA progress fails the stream conservatively instead of publishing
uncertain bytes. Flush stops/rebases DMA and clears hardware state before ACK.
Optional diagnostics distinguish hardware errors, progress failures, and
observed DMA lag. The RX-gap warning remains set pending real DMA/timer/error-IRQ
qualification; zero counters do not prove a complete stream.

Shared UART/USB buffering is a bounded 1 KiB byte pipe. F103 drains its existing
IRQ queue in chunks; Pico retains its PIO engine. Pico flush restarts its RX
state machine and discards partial frames. F103's 36 MHz UART clock supports
550–2,250,000 baud with its divider; values below 550 are rejected even though
the shared protocol's preliminary check starts at 300. Advertised maximum baud
is a configuration limit, not a qualified sustained-throughput rating.

## Recovery

CH32 READ_PROG_INFO starts a fresh host session and cancels abandoned bulk work.
Its forced erase-busy windows survive unrelated commands. Bulk operations wait
cancellably and retain the original 25 ms settling delay. Auxiliary transfers
have endpoint-indexed cancellation guards, including cleanup of late CH32 tokens.
Those controller races still require hardware testing.

On reconnect, the daemon revalidates board/version, rejects old-generation
queued actions, and never replays commands, GPIO state, or serial sessions.
See [reconnect semantics](socket-protocol.md#reconnect-and-uncertain-execution).

## Qualification

The combined firmware, especially F103 USB recovery/pin muxing and the retained
RP2040 fast path, still needs [hardware validation](hardware-validation.md).
The [parity re-review](parity-review.md) closes all eight reported software
findings. The [reliability design review](reliability-design-review.md) records
further UART/recovery concerns. Neither is a hardware sign-off.
