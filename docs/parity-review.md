# DediBridge parity-fix re-review — final

## Review

**All F1–F8 are resolved at source level.** The additional accepted-OUT packet-loss issue found during this re-review is also addressed by the final EP3 cache/lifecycle changes.

**No issues found.**

- **Correct:** original capabilities and operational coverage targeted by F1–F8 are restored.
- **Fixed:** parent implementation fixes verified below; reviewer made no changes.
- **Merge verdict: OK with notes.**
- **Acceptance:** software/source-level only, not hardware qualification or an unconditional end-to-end parity guarantee.

This supersedes the earlier BLOCK disposition concerning destructive auxiliary OUT cleanup.

## Per-finding disposition

Original prefixes: **P** = `/home/arthur/src/dedipico`; **C** = `/home/arthur/src/dedich32`. New references are relative to `/home/arthur/src/dedibridge`.

| Finding | Final disposition and evidence |
|---|---|
| **F1 — Experimental QPI removed** | **Resolved.** `boards/rp2040/src/backend.rs:33–34` restores `IO_MODES=0x3f`, making the retained four-bit read implementation reachable. Matches P `src/usb_handler.rs:206–214`, `src/spi_flash.rs:595–609`. Dispatch test verifies Pico acceptance and single-lane rejection. Documentation correctly describes experimental reads, not qualified QPI programming. |
| **F2 — CH32 fresh-session cancellation removed** | **Resolved.** `crates/firmware-core/src/handler.rs:144–148` cancels on READ_PROG_INFO under CH32-specific policy, matching C `src/usb_handler.rs:85–95`. Pico does not acquire this historically different behavior. Tests cover abandoned IN/OUT, old follow-up cancellation and maximum-size IN termination. |
| **F3 — Erase-busy protection changed** | **Resolved.** `crates/firmware-core/src/flash.rs:69–85,154–159` preserves erase windows across unrelated writes and waits cancellably through expiry plus 25 ms settling. Matches C `src/spi_flash.rs:156–164,279–293`. Tests cover all erase durations, synthetic status, settling and cancellation without premature SPI activity. |
| **F4 — Auxiliary timeout cleanup missing** | **Resolved at source level, including the OUT regression discovered during re-review.** Endpoint-indexed guards cover auxiliary IN/OUT. CH32 caches accepted EP3 payload before retiring its completion; the next read consumes it before rearming. EP1 abandoned bulk data is deliberately not cached. Reset/deconfiguration invalidate cache and cancellation masks. Details below. |
| **F5 — Named CLI/combined set absent** | **Resolved.** `tools/dedibridgectl/src/main.rs:39–133` restores named `dir/set/release/pulse`; `daemon.rs:172–183` executes direction then output as one serialized actor operation. Matches P host `main.rs:425–437`. Tests cover translation/defaults and combined set from released state. |
| **F6 — Firmware release delivery removed** | **Resolved in implementation.** `dev:46–54` packages three ELFs and Pico UF2; `.github/workflows/check.yml:28–52` uploads artifacts and publishes on tags. Restores both originals’ release-delivery capabilities. Live GitHub execution remains unverified. |
| **F7 — Firmware Clippy omitted** | **Resolved.** `dev:19–23,37–43` runs warning-denied Clippy separately for each board/target; CI installs nightly Clippy. |
| **F8 — Failure clears PASS/BUSY** | **Resolved.** `crates/firmware-core/src/bulk.rs:97–112,334` saves LED state and ORs ERROR, matching P `src/leds.rs:63–74`. Test asserts host mask 3 becomes 7. |

## Changed blast radius

### Auxiliary OUT preservation and controller cleanup

The intermediate guard discarded an acknowledged EP3 packet when UART/GPIO won the receive select. The final implementation addresses that mechanism:

- `boards/ch32v307/src/backend.rs:204–224` copies valid accepted EP3 DMA payload **before** clearing the completion.
- Endpoint/token checks precede retirement; valid OUT toggles advance in the same retirement path.
- `crates/firmware-core/src/transport.rs:65–75` consumes cached data before invoking the underlying read, avoiding unnecessary rearming.
- `boards/ch32v307/src/backend.rs:267–288` takes the cache once and rejects invalid lifecycle state.
- `handler.rs:71–79` invokes recovery reset on reset/deconfiguration; CH32 clears cache/masks at `backend.rs:290–294`.
- Disabled endpoints or pending bus reset prevent new EP3 caching.
- Successfully completed guards do not retire again.
- Only EP3 is cached; stale EP1 flash bytes are not replayed into another bulk generation.

`transport_tests.rs:94–116` checks cached delivery exactly once and reset invalidation; `:118–161` checks endpoint identity and final-ACK guard lifetime.

**Limit:** those tests model the transport contract, not actual CH32 MMIO, DMA or IRQ timing.

### Bulk END sentinel

No concrete defect found.

`bulk.rs:218–255,274–322` publishes a bounded END sentinel after completion/early failure and releases slots while consuming it. Successful consumers stop after the configured block count; the unused END can occupy a released slot and disappears with the local channel.

Generation-scoped follow-up discard, verification retention after non-cancellation flash failure, and USB-failure distinction remain intact at `:315–345`. Flash futures remain awaited rather than arbitrarily cancelled.

**Coverage note:** successful multi-block execution and the full flash-failure/USB-failure/queued-verify matrix still lack focused tests.

### Firmware deadlines and concurrent operations

- Forced erase waiting yields every ≤1 ms and checks cancellation before SPI activity.
- Page status polling and USB waits remain bounded.
- Positive-duration pulse testing demonstrates release while its USB response remains stalled.
- Combined host `set` preserves direction-then-output ordering without interleaving another actor command. Its non-atomic electrical behavior/no rollback is documented.
- Added Go test now actually rejects an incompatible ready version.

## Validation and acceptance limits

### Independent evidence

Reviewed actual current fix delta, affected sources/tests, current originals and retained/pinned HAL behavior. No project files were changed and no tests/builds were independently executed by this reviewer.

### Parent-run evidence

Supervisor reports successful final validation:

- **26 Rust tests** and standalone Go race tests.
- Formatting and warning-denied host/core/protocol plus all three firmware-target Clippy checks.
- Three release links and artifact generation.
- Patched dutctl serial/dut race tests and actionlint.
- UF2 inspection: 251 valid RP2040 blocks, including family, magic, address/count ordering.
- Empty staged-path inventory.

Reported flash/static-RAM totals:

| Board | Flash | Static RAM |
|---|---:|---:|
| Pico | 64,212 B | 7,364 B |
| CH32 | 46,170 B | 11,056 B |
| F103 | 58,268 B | 7,788 B |

These totals do not establish runtime/interrupt stack margin.

### Outstanding physical qualification

Still required:

- CH32 late completions, cancellation/reuse, reset/deconfiguration, accepted EP3 delivery, endpoint isolation and exactly-once DATA toggles.
- Pico PIO lanes, experimental QPI entry/read/exit, DMA cancellation and final double-buffer draining.
- Delayed-WIP emulator behavior and real identify/read/write/verify.
- Idle pin ownership, WP/HOLD pull-ups and GPIO electrical release.
- UART stop-bit/baud/flush ordering, throughput and concurrent GPIO/flash operation.
- F103 USB/pull-up/clone behavior, custom RX IRQ, DMA routing and stack margin.
- Actual flashprog/dutctl interoperability and GitHub tag publishing.

### Parent validation addendum (after reviewer completion)

No production code changed after this review. A focused multi-block IN/OUT test
was added for successful and USB-failed pipeline END-marker termination, packet
counts, bus return, and LED behavior. All **27 Rust tests**, Go race tests, patched
dutctl tests, full target Clippy/build checks, and actionlint passed again. The
full flash-failure/queued-verify matrix and physical checks remain outstanding.
This addendum is parent-run evidence, not an additional independent review.

**Verdict:** all reported parity findings are closed in the inspected software. Hardware-unqualified functionality must remain explicitly labeled; “no functionality lost” should not be presented as a fully demonstrated hardware claim.

**Merge verdict: OK with notes.**