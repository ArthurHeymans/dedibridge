# Auxiliary and local socket protocols

## USB auxiliary interface

Vendor interface 1 has subclass `0xd1`, protocol `1`, EP3 OUT (`0x03`) and EP4
IN (`0x84`). SF600 stays on interface 0. Bulk endpoint MPS is 64 on full-speed
boards and 512 on CH32 high speed. The **wire message** remains <=64 bytes on
both speeds, one message per USB packet/transfer (HS messages are short packets).
Host receive buffers must be at least the endpoint MPS.

Messages are `[kind:u8, request_id:u8, payload_len:u8, payload...]`. Length must
match exactly and payload is at most 61 bytes. ID 0 is unsolicited; nonzero IDs
correlate responses. No padded messages. The host has one request outstanding
and routes UART events while waiting for its response.

| Kind | Payload |
|---|---|
| `0x00` GET_INFO | empty |
| `0x01` GPIO_GET_STATE | empty |
| `0x02` GPIO_SET_DIRECTION | mask, directions |
| `0x03` GPIO_SET_OUTPUT | mask, values |
| `0x04` GPIO_PULSE_LOW | mask, duration_ms:LE16 |
| `0x10` UART_SET_BAUD | baud:LE32 |
| `0x11` UART_WRITE | binary bytes |
| `0x12` UART_FLUSH_RX | empty |
| `0x80` RESPONSE | command, status, response bytes |
| `0x81` GPIO_STATE | inputs, outputs, directions, caps |
| `0x90` UART_DATA | binary bytes |
| `0x91` UART_OVERFLOW | direction (0 RX, 1 TX), affected count:LE32 |

Status: 0 OK, 1 INVALID, 2 BUSY. UART_WRITE accepts a whole message into its
bounded TX queue or returns BUSY; it never accepts only a prefix. UART_SET_BAUD
responds only after application, returns BUSY while TX is queued/active, and
rejects invalid rates rather than silently clamping. RX flush discards both
software and peripheral/driver buffers; response ordering is the flush barrier.
A UART hardware error is an overflow event too; its affected-byte count can be
an estimate, not an exact measurement of dropped physical UART frames.

GPIO bits: RESET#=0, POWER_SW#=1, power-state=2, auxiliary input=3. Only bits 0/1
may be written. Output=0 plus direction=1 pulls low; otherwise release. Pulse
commands restore the previous output/direction state at the deadline, independent
of USB readiness. A new GPIO write/pulse first ends any previous pulse.

GET_INFO returns `DeviceInfo` from `crates/protocol`:

```text
version:u8 (=1), board:u8 (1 Pico, 2 CH32, 3 F103), io_modes:u8,
gpio_outputs:u8, gpio_inputs:u8, flags:u8, reserved:[u8;2], max_baud:LE32
```

`io_modes` is a bitset indexed by the SF600 IoMode enum; current values are
`0x3f` for Pico (including experimental QPI reads) and `0x01` for CH32/F103. Existing GPIO caps remain unchanged for
compatibility; new clients use the distinct input/output masks in GET_INFO.
Identity is the USB serial string; board number is informational, never a host
transport-selection switch.

## Local daemon API v1

Unix stream socket, permissions `0600`. Use a private per-user directory and
one socket per device. Each frame is UTF-8 JSON followed by LF, at most 16 KiB
including LF. Binary data uses standard padded base64. Client writes are at
most 4096 decoded bytes per frame; the daemon divides them into USB payloads.
Unknown operations, malformed frames, incompatible protocol versions, and bad
GPIO masks are errors. This is a local API, not an authenticated TCP protocol.

### One-shot control connection

Send one of these, read one response, close:

```json
{"op":"info"}
{"op":"state"}
{"op":"pulse","mask":1,"ms":100}
{"op":"direction","mask":1,"values":0}
{"op":"output","mask":1,"values":0}
{"op":"set","mask":1,"values":0}
```

Responses:

```json
{"type":"info","serial":"ABC123","info":{"version":1,"board":3,"io_modes":1,"gpio_outputs":3,"gpio_inputs":15,"flags":3,"max_baud":2250000}}
{"type":"state","inputs":15,"outputs":3,"directions":0,"caps":7}
{"type":"error","message":"..."}
```

Output alone does not change direction. `set` enables the masked outputs, then
sets their values, as one serialized daemon actor operation (two acknowledged
USB commands, not an electrical atomic update). A failure is returned if either
command fails; there is no rollback. Use a pulse for reset/power button semantics.
No control command detaches the serial client or claims interface 0.

### Serial connection

The first request acquires the sole UART lease, sets baud, and flushes old RX:

```json
{"op":"serial","baud":115200}
```

Success returns `{"type":"ready","version":1,"serial":"...","info":{...}}`.
Failure returns an error (for example, another UART client owns the device).
After ready, requests and unsolicited events share the connection:

```json
{"op":"write","data":"aGVscG8N"}
{"type":"written"}
{"op":"reset_input"}
{"type":"reset_input"}
{"type":"data","data":"aGVsbG8NCg=="}
```

One command may be outstanding per serial client. UART DATA can arrive before
or after its reply, so clients need a demultiplexing reader, not a sequential
request/response reader that ignores events. `written` means all USB chunks were
accepted into firmware queues; it is not a DUT-level acknowledgement.

For reset-input, clear the client buffer and discard DATA until the reset-input
response is encountered **in the reader**. After that response, resume buffering
immediately; otherwise a fast prompt could be discarded between the response
and the caller returning. The Go adapter implements that ordering.

Device overflow, client-buffer overflow, a slow/stalled reader, USB disconnect,
or a failed write invalidates the serial session. No bytes are silently dropped
while reporting success. GPIO connections remain independent. Close releases
the lease; already accepted UART bytes may still finish transmitting. There is
no session replay, automatic hotplug reconnection, or console broadcast.
