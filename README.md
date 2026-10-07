# DediBridge

Turn a Pico, CH32V307 devboard, or Blue Pill into an SF600-compatible SPI flash
programmer, with a UART bridge and reset/power-button control over the same USB
cable. Use [flashprog](https://flashprog.org/) or
[rflasher](https://github.com/ArthurHeymans/rflasher) to read, write, and erase flash.

Successor to [DediPico](https://github.com/ArthurHeymans/dedipico) and
[dedich32](https://github.com/ArthurHeymans/dedich32).

## Pick your programmer

| Board guide · wiring & flashing | USB | SPI reads |
|---|---|---|
| [Raspberry Pi Pico](docs/boards/pico.md) | Full speed | single / dual / quad |
| [CH32V307 devboard](docs/boards/ch32v307.md) | High speed | single |
| [STM32 Blue Pill](docs/boards/blue-pill.md) · proof of concept | Full speed | single |

## Use it

Flash your board using its guide above, then:

```sh
flashprog -p dediprog --flash-name
flashprog -p dediprog -r dump.bin
flashprog -p dediprog -w image.bin
```

**3.3 V I/O only; no voltage switching or electrical isolation.** Don’t connect
competing power supplies or drive a flash bus still owned by the target.
See [wiring precautions](docs/wiring.md) before connecting anything.

Combined firmware still needs [hardware validation](docs/hardware-validation.md).

- [UART, board control & USB permissions](docs/usage.md)
- [Build from source & development](docs/building.md)
- [Socket API](docs/socket-protocol.md) · [dutctl integration](integrations/dutctl/README.md)
- [Implementation notes](docs/implementation.md) · [licensing & attribution](NOTICE.md)
