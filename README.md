# portcall

> A lightweight terminal serial monitor in Rust that **discovers** available
> ports and lets you **pick one from a list** — instead of making you memorize
> `/dev/ttyUSB0` or `COM4`.

Most serial tools (`picocom`, `minicom`, `cu`, `tio`) make you already *know* the
port path before you connect. portcall flips that around: **discover → pick →
connect.** It auto-detects the baud rate of ASCII devices so you don't have to guess.

[![crates.io](https://img.shields.io/crates/v/portcall.svg)](https://crates.io/crates/portcall)

## Features

- **Full-screen TUI** — three-screen flow: port picker → baud picker → live view.
  Mouse hover, click, and keyboard navigation throughout.
- **Port discovery with metadata** — classifies each port (USB / Bluetooth / PCI),
  shows USB manufacturer, product, and VID:PID so the device is recognizable at a glance.
  A detail panel shows full device info for the selected port.
- **Phantom-port filtering (Linux)** — hides empty legacy `/dev/ttyS*` UART slots
  via sysfs. Pass `--all` to see everything.
- **Auto-baud detection** — scans 9600 → 921600, scores each baud for readable
  text structure, and connects at the best match. Manual entry available via tab UI.
- **Live scroll** — timestamped lines with keyword colour-coding
  (errors red, warnings yellow, ok/pass/done green). Smart auto-scroll stays at
  the bottom unless you've scrolled up to read.
- **Smart pinning** — lines sharing a common prefix that repeat 3× or more are
  promoted to a persistent side panel and update in place, keeping the live stream
  clean. ANSI-formatted output bypasses pinning and streams directly.
- **Direct mode** — `portcall /dev/ttyACM0` opens a known path, skipping discovery.

## Install

### From crates.io

```sh
cargo install portcall
```

> **Linux:** portcall enumerates ports via **libudev**. Install the dev headers
> before `cargo install` if you don't already have them:
> ```sh
> sudo apt install libudev-dev pkg-config   # Debian / Ubuntu / Pop!_OS
> ```

### From source

```sh
git clone https://github.com/ChirayuR/Portcall portcall
cd portcall
cargo build --release
# binary at target/release/portcall
```

## Usage

```sh
portcall              # discover ports, pick one, connect
portcall /dev/ttyACM0 # open a specific port directly (skip discovery)
portcall --all        # include phantom/legacy ttyS* slots in the list
```

The TUI walks you through three screens:

**1. Port picker** — select a port with ↑↓, hover, or click. A detail panel on
the right shows the full device description.

**2. Baud picker** — tab between Auto-detect and Manual entry. Auto-detect scans
all common baud rates and connects at the best match.

**3. Live view** — scrolling receive window with timestamps and colour-coded
keywords. Frequently repeated lines are pinned to a side panel and update in place.

> **Note:** auto-baud only works while the device is actively transmitting —
> there is no deterministic host-side autobaud for plain UART.

### Permissions (Linux)

Opening a port usually requires membership in the `dialout` group:

```sh
sudo usermod -aG dialout $USER   # then re-login
```

## Platform support

Cross-platform port discovery via the
[`serialport`](https://crates.io/crates/serialport) crate (`COM*` on Windows,
`/dev/tty*` on Linux, `/dev/cu.*` on macOS). Development and hardware verification
on Linux; the phantom-`ttyS` filter is Linux-specific (no-op elsewhere).

## Roadmap

- TX line — type and send text back to the device.
- Improved baud scoring — line-structure and high-byte-density checks.
- Reconnect handling and configurable line endings.
- Remember last-used port and baud across sessions.

## How it works

A background **reader thread** owns the open serial port and pushes received bytes
over an `mpsc` channel to the UI thread (producer/consumer). The core connection
logic is kept free of TUI dependencies so it can be extracted into a library crate
later without touching the serial logic.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
