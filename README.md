# portcall

> A lightweight terminal serial monitor in Rust that **discovers** available
> ports and lets you **pick one from a list** — instead of making you memorize
> `/dev/ttyUSB0` or `COM4`.

Most serial tools (`picocom`, `minicom`, `cu`, `tio`) make you already *know* the
port path before you connect. portcall flips that around: **discover → pick →
connect.** It can even **auto-detect the baud rate** of a device that's sending
ASCII, so you don't have to guess.

> **Status:** early but working. Port discovery, connect/stream, and auto-baud
> are implemented and verified on hardware. A full-screen TUI is on the roadmap.

## Features

- **Port discovery with metadata** — lists serial ports and classifies each
  (USB / Bluetooth / PCI), showing USB manufacturer, product, and VID:PID so the
  device is recognizable at a glance.
- **Phantom-port filtering (Linux)** — hides the kernel's always-present but
  empty legacy `/dev/ttyS*` UART slots (detected via sysfs), so the list shows
  only ports that are actually there. Pass `--all` to see everything.
- **Auto-baud detection** — for ASCII devices, scans common baud rates
  (9600 → 921600), scores each for readable text, and connects at the best match
  (`auto: 115200 baud detected`). Binary/raw devices take a manually entered baud.
- **Live streaming** — opens the port, reads on a background thread, and streams
  incoming bytes to stdout. Clean shutdown on disconnect.
- **Direct mode** — `portcall /dev/ttyACM0` opens a known path, skipping the
  picker.

## Install

Requires a [Rust toolchain](https://rustup.rs/) (edition 2024).

### Linux build dependency

portcall enumerates ports via **libudev**, so the dev headers are needed at build
time:

```sh
# Debian / Ubuntu / Pop!_OS
sudo apt install libudev-dev pkg-config
```

### Build

```sh
git clone <repo-url> portcall
cd portcall
cargo build --release
# binary at target/release/portcall
```

## Usage

Run it and follow the prompts:

```sh
portcall
```

```
Found 1 serial port(s):

  [1] /dev/ttyACM0
      USB  STMicroelectronics — STM32 STLink  (VID:PID 0483:374b)

Select port [1]: 1
What is the device sending? [A]SCII text / [B]inary or raw (default A): a
Auto-detecting baud — the device must be transmitting ASCII now…
auto: 115200 baud detected.

Connected to /dev/ttyACM0 @ 115200 baud. Press Ctrl-C to quit.
STATUS: Logging active
...
```

Other forms:

```sh
portcall /dev/ttyACM0   # open a specific port directly (skip discovery)
portcall --all          # include phantom/legacy ttyS* slots in the list
```

> **Note:** auto-baud only works while the device is actively transmitting —
> there is no deterministic host-side autobaud for plain UART.

### Permissions (Linux)

Opening a port usually requires membership in the `dialout` group. If you get
`Permission denied`, add yourself and re-login:

```sh
sudo usermod -aG dialout $USER
```

## Platform support

Cross-platform port discovery is provided by the
[`serialport`](https://crates.io/crates/serialport) crate (`COM*` on Windows,
`/dev/tty*` on Linux, `/dev/cu.*` on macOS). Development and verification so far
have been on Linux; the phantom-`ttyS` filter is Linux-specific (no-op elsewhere).

## Roadmap

- Full-screen TUI: navigable port list + scrolling receive view.
- Sending (TX) a line back to the device.
- Reconnect handling and configurable line endings.
- Remember the last-used port and baud.

## How it works

A background **reader thread** owns the open port and pushes received bytes over
an `mpsc` channel to the main thread (producer/consumer). Core concerns (port
discovery, the `Connection`) are kept free of any UI dependency so a TUI can be
layered on without touching them.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
