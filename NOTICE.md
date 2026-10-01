# Source attribution and licensing

This project incorporates source from the local `../dedipico` and `../dedich32`
projects. `dedipico` is MIT licensed; `dedich32` is MIT OR Apache-2.0 licensed.
The inherited MIT notice is retained in `LICENSE-MIT`. Their originals remain
unchanged and retain their own histories.

The RP2040 PIO flash engine, PIO programs, and direct-register bulk-IN fast path
are derived from `dedipico/src/`. The auxiliary wire protocol, identity, and
SF600 parsing originated in that project too. CH32 pin muxing and packet cleanup
are derived from `dedich32/src/`. Those imported sources are used under MIT.
New code is available under MIT OR Apache-2.0; source-specific MIT terms remain
applicable to inherited code. Both license texts are included.

`integrations/dutctl/serial.patch` modifies the BSD-style-licensed dutctl serial
module and preserves its original Blindspot Software notice. The new Go adapter
is DediBridge code; installing it does not remove dutctl's license obligations.

The CH32 HAL is an external, commit-pinned Git dependency, not vendored here.
Embassy HALs and other Cargo dependencies retain their own licenses.
