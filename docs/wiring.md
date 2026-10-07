# Wiring precautions

[← DediBridge](../README.md) · [Pico](boards/pico.md) · [CH32V307](boards/ch32v307.md) · [Blue Pill](boards/blue-pill.md)

## Flash and power

- Use **3.3 V signals**. A 1.8 V flash needs level shifters and a suitable supply.
- Share ground. For a loose flash chip, provide a suitable 3.3 V supply; check
  the board regulator's capacity before powering it from a board's 3V3 pin.
- For in-system programming, **leave programmer 3V3 disconnected** if the
  target already powers the flash. Never tie competing supplies together.
- Ensure the target controller has released the flash bus before programming.
  Firmware releases flash outputs when idle and parks CS with a weak pull-up,
  but **Hi-Z is not electrical isolation or contention protection**.
- Fit appropriate external pull-ups on CS#, WP# and HOLD# as needed. On Pico,
  WP#/HOLD# double as IO2/IO3 for multi-lane reads and are released when idle.
- Flashprog voltage commands are compatibility stubs; they do not switch power
  or change the I/O voltage.

## UART and board control

- UART is **3.3 V TTL**, not RS-232. Connect programmer TX to target RX and
  programmer RX to target TX, with shared ground.
- RESET# and POWER_SW# are open-drain: they pull low or release, never drive
  high. POWER_SW# connects to a motherboard's power-button input, not a power
  supply output. A roughly five-second pulse requests ATX-style power-off.
- For 5 V control lines, use external transistors or level shifting unless the
  complete open-drain electrical design has been checked. An MCU's
  “5 V-tolerant input” rating alone is not an output-mode guarantee.
- Power-state and auxiliary-state pins are digital inputs with **no internal
  pull-ups**. Keep them at defined, safe logic levels; do not leave them floating
  when using state reporting.
- External LEDs need current-limiting resistors. They are optional.
