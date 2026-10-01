# dutctl serial backend

This directory adds a DediBridge socket transport to the existing `serial`
module in `~/src/dutctl-9e`, without replacing its send/expect engine or its
normal COM-port backend. The original checkout is deliberately untouched.

The adapter uses only the Go standard library. `go test -race ./...` here runs
its socket, binary-data, input-flush, timeout, and disconnect tests. The patch
also adds a module-level configuration/send-expect integration test; the
patched serial module was tested in `../../.local/dutctl`.

## Apply to the existing checkout

Start a separate jj change before modifying it, and dry-run the patch first:

```sh
bridge=$HOME/src/dedibridge
cd "$HOME/src/dutctl-9e"
jj new -m 'feat(serial): add DediBridge socket transport'
patch --dry-run -p1 < "$bridge/integrations/dutctl/serial.patch"
cp "$bridge/integrations/dutctl/dedibridge.go" pkg/module/serial/
cp "$bridge/integrations/dutctl/dedibridge_test.go" pkg/module/serial/
cp "$bridge/LICENSE-MIT" pkg/module/serial/LICENSE-DediBridge-MIT
cp "$bridge/LICENSE-APACHE" pkg/module/serial/LICENSE-DediBridge-APACHE
patch -p1 < "$bridge/integrations/dutctl/serial.patch"
go test -race ./pkg/module/serial ./pkg/dut
```

The patch targets the current alpha.4 serial module. Review/rebase it if that
module changes before installation. No new module registration or Go dependency
is required.

## Configure

Run `dedibridgectl daemon` on the **dutagent host**, selecting a stable USB
serial and a per-device socket. The dutagent account needs permission to access
that socket (by default it is owner-only).

```yaml
version: 1.0.0-alpha.4
devices:
  board:
    desc: Board connected through DediBridge
    cmds:
      serial:
        uses:
          - module: serial
            passthrough: true
            with:
              dedi_socket: /run/user/1000/board-a.sock
              baud: 115200
              delay: 0s
```

```sh
dutctl board serial -t 60s -- expect 'login:' send root expect '# '
```

Configure `dedi_socket` **instead of** `port`. Opening is per Run, not Init, so
a missing daemon does not prevent the agent from starting. `baud` sets the
hardware UART when the session opens. Closing frees the UART lease; concurrent
GPIO RPCs and flashprog remain possible.

The adapter has the exact four-method surface needed by the current engine:
`Read`, `Write`, `ResetInputBuffer`, `Close`. Reads return `(0, nil)` after 100 ms
without data, preserving the engine's cancellation behavior. Commands have a
5 s upper bound. Binary data is base64 framed, writes are acknowledged, and
reset-input discards queued data through a device-side flush barrier. Overflow
or USB/daemon disconnect fails the run rather than being mistaken for an idle
console. The local RX buffer is bounded to 64 KiB.

This adds **serial** support only. Reset/power can be exposed with the shell
module invoking `dedibridgectl --socket ... reset|power|poweroff`; a native GPIO
module is not bundled here.
