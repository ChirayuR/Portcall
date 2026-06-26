use serialport::{ClearBuffer, ErrorKind, SerialPort, SerialPortInfo, SerialPortType, UsbPortInfo};
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_BAUD: u32 = 115_200;
/// How long a read blocks before returning `TimedOut`. Short enough that the
/// reader thread notices a disconnect / shutdown promptly, long enough to not
/// busy-spin.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
const READ_BUF: usize = 1024;

/// Baud rates the auto-detector scans, low → high (standard set + fast links).
const BAUD_CANDIDATES: &[u32] = &[
    9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
];
/// How long to listen at each candidate baud during a scan.
const DETECT_WINDOW: Duration = Duration::from_millis(350);
/// Need at least this many bytes in a window to judge a baud at all.
const DETECT_MIN_BYTES: usize = 8;
/// Stop a listen window early once we have this many bytes — plenty to score,
/// and a hard cap against a fast/flooding link ballooning memory.
const DETECT_MAX_BYTES: usize = 4096;
/// Minimum text-likeness score (0.0–1.0) for a baud to count as a match.
const DETECT_THRESHOLD: f32 = 0.85;

fn main() -> ExitCode {
    // Args: `--all`/`-a` shows phantom slots; the first non-flag arg, if any,
    // is a port path to open directly, skipping discovery + the picker. Direct
    // mode is also what lets us aim portcall at a virtual loopback port (a
    // socat PTY) to test the stream path without real hardware.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_all = args.iter().any(|a| a == "--all" || a == "-a");
    let explicit_path = args.iter().find(|a| !a.starts_with('-')).cloned();

    let port_name = if let Some(path) = explicit_path {
        path
    } else {
        let ports = match serialport::available_ports() {
            Ok(ports) => ports,
            Err(e) => {
                eprintln!("error: failed to enumerate serial ports: {e}");
                return ExitCode::FAILURE;
            }
        };

        // Hide phantom `/dev/ttyS*` slots unless the user asked for everything.
        let visible: Vec<&SerialPortInfo> = ports
            .iter()
            .filter(|p| show_all || !is_phantom(p))
            .collect();

        if visible.is_empty() {
            if ports.is_empty() {
                println!("No serial ports found.");
            } else {
                println!(
                    "No usable serial ports found ({} phantom slot(s) hidden).\n\
                     Plug in a device, or pass --all to see everything.",
                    ports.len()
                );
            }
            return ExitCode::SUCCESS;
        }

        println!("Found {} serial port(s):\n", visible.len());
        for (i, port) in visible.iter().enumerate() {
            println!("  [{}] {}", i + 1, port.port_name);
            println!("      {}", describe(&port.port_type));
        }

        // ---- Slice 2: pick a port, open it, stream RX to stdout ----
        match choose_port(&visible) {
            Ok(Some(name)) => name,
            Ok(None) => {
                eprintln!("\nNothing selected — bye.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("input error: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    let baud = match choose_baud_or_detect(&port_name) {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!("\nNo baud rate — bye.");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("input error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let conn = match Connection::open(&port_name, baud) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to open {port_name} @ {baud}: {e}");
            // The single most common first-run failure on Linux.
            if e.kind() == ErrorKind::Io(io::ErrorKind::PermissionDenied) {
                eprintln!(
                    "hint: add yourself to the 'dialout' group, then log out and back in:\n  \
                     sudo usermod -aG dialout $USER"
                );
            }
            return ExitCode::FAILURE;
        }
    };

    eprintln!("\nConnected to {port_name} @ {baud} baud. Press Ctrl-C to quit.\n");

    // Stream until the reader thread ends (device error/disconnect) or stdout
    // breaks. Serial bytes go to stdout; all our chrome went to stderr above.
    if let Err(e) = conn.pump_to(io::stdout().lock()) {
        eprintln!("\noutput error: {e}");
        return ExitCode::FAILURE;
    }

    eprintln!("\nDisconnected.");
    ExitCode::SUCCESS
}

/// A live serial link: owns the background reader thread and the channel it
/// feeds. This is the seam that becomes the `core` library's `Connection`
/// later (notes: core has no TUI deps). RX-only for now; TX lands in Slice 4
/// via `SerialPort::try_clone`.
struct Connection {
    rx: mpsc::Receiver<Vec<u8>>,
    reader: thread::JoinHandle<()>,
}

impl Connection {
    /// Open `path` at `baud`, then hand the port off to a reader thread.
    fn open(path: &str, baud: u32) -> serialport::Result<Connection> {
        let port = serialport::new(path, baud).timeout(READ_TIMEOUT).open()?;

        // The channel is the producer/consumer queue: reader thread = producer,
        // main thread = consumer (cf. an RTOS message queue).
        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        // `move` transfers ownership of `port` and `tx` into the thread. After
        // this, main can't touch `port` — the compiler guarantees the port has
        // exactly one owner, so there's no data race to reason about.
        let reader = thread::spawn(move || reader_loop(port, tx));

        Ok(Connection { rx, reader })
    }

    /// Block, writing each received chunk to `out`, until the channel closes
    /// (reader thread exits) — then reap the thread.
    fn pump_to<W: Write>(self, mut out: W) -> io::Result<()> {
        // Iterating a `Receiver` yields values until every `Sender` is dropped.
        for chunk in self.rx {
            out.write_all(&chunk)?;
            out.flush()?;
        }
        // Channel drained & closed ⇒ the reader has returned; join to surface
        // any panic and avoid a detached thread.
        let _ = self.reader.join();
        Ok(())
    }
}

/// Producer side: block on the port, push received bytes into the channel.
/// Owns the port outright (moved in), so no locking is needed.
fn reader_loop(mut port: Box<dyn SerialPort>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match port.read(&mut buf) {
            // The port only reaches `read()` after signalling readable, so 0
            // bytes here means EOF — the device closed. (Genuine "no data yet"
            // surfaces as `TimedOut` below.) Stop: closing `tx` ends the stream.
            Ok(0) => return,
            Ok(n) => {
                // `send` fails only if the receiver hung up → nobody's
                // listening, so we're done.
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
            // No data within the timeout window is normal — keep waiting.
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            // Anything else (typically the device unplugged) ends the stream.
            Err(e) => {
                eprintln!("\n[reader] serial read error: {e}");
                return;
            }
        }
    }
}

/// Prompt on stdout, return one trimmed line from stdin. `None` on EOF (Ctrl-D).
fn prompt(label: &str) -> io::Result<Option<String>> {
    print!("{label}");
    io::stdout().flush()?;

    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None); // EOF
    }
    Ok(Some(line.trim().to_string()))
}

/// Ask which listed port to open. Empty input defaults to the first one.
/// Returns the chosen port name, or `None` to quit.
fn choose_port(ports: &[&SerialPortInfo]) -> io::Result<Option<String>> {
    loop {
        let label = if ports.len() == 1 {
            "\nSelect port [1]: ".to_string()
        } else {
            format!("\nSelect port [1-{}]: ", ports.len())
        };

        let Some(input) = prompt(&label)? else {
            return Ok(None);
        };

        let choice = if input.is_empty() {
            1
        } else {
            match input.parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    eprintln!("Not a number: {input:?}");
                    continue;
                }
            }
        };

        if (1..=ports.len()).contains(&choice) {
            return Ok(Some(ports[choice - 1].port_name.clone()));
        }
        eprintln!("Out of range: pick 1-{}.", ports.len());
    }
}

/// Ask for a baud rate. Empty input defaults to `DEFAULT_BAUD`.
fn choose_baud() -> io::Result<Option<u32>> {
    loop {
        let Some(input) = prompt(&format!("Baud rate [{DEFAULT_BAUD}]: "))? else {
            return Ok(None);
        };

        if input.is_empty() {
            return Ok(Some(DEFAULT_BAUD));
        }
        match input.parse::<u32>() {
            Ok(b) if b > 0 => return Ok(Some(b)),
            _ => eprintln!("Enter a positive baud rate (e.g. 9600, 115200)."),
        }
    }
}

/// Decide the baud rate: ask what the device transmits, then auto-detect for
/// ASCII (the device must be sending now) or take a manual baud for binary/raw.
/// Auto-detect failure also falls back to manual entry. `None` = quit (EOF).
fn choose_baud_or_detect(path: &str) -> io::Result<Option<u32>> {
    loop {
        let Some(answer) =
            prompt("\nWhat is the device sending? [A]SCII text / [B]inary or raw (default A): ")?
        else {
            return Ok(None);
        };

        match answer.to_ascii_lowercase().as_str() {
            "" | "a" | "ascii" => {
                eprintln!("Auto-detecting baud — the device must be transmitting ASCII now…");
                if let Some(baud) = detect_baud(path) {
                    eprintln!("auto: {baud} baud detected.");
                    return Ok(Some(baud));
                }
                eprintln!(
                    "Auto-detect failed (no clear signal — is the device sending?). \
                     Falling back to manual entry."
                );
                return choose_baud();
            }
            "b" | "binary" | "raw" => return choose_baud(),
            other => eprintln!("Please answer A or B (got {other:?})."),
        }
    }
}

/// Scan `BAUD_CANDIDATES`, scoring each listen window for ASCII-ness, and return
/// the best-matching baud (or `None` if nothing scored above threshold). Opens
/// its own probe port; the caller reopens for the real connection.
fn detect_baud(path: &str) -> Option<u32> {
    let mut port = match serialport::new(path, BAUD_CANDIDATES[0])
        .timeout(READ_TIMEOUT)
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("auto-baud: can't open {path}: {e}");
            return None;
        }
    };

    let mut best: Option<(u32, f32, Vec<u8>)> = None;
    for &baud in BAUD_CANDIDATES {
        // PTYs reject some rates; a real adapter accepts all standard ones.
        if port.set_baud_rate(baud).is_err() {
            continue;
        }
        // Discard bytes captured at the previous rate / mid-switch garbage.
        let _ = port.clear(ClearBuffer::Input);

        let bytes = read_window(&mut *port, DETECT_WINDOW);
        let score = score_ascii(&bytes);
        eprintln!("  scan {baud:>7}: {:>4} bytes  score {score:.2}", bytes.len());

        if score >= DETECT_THRESHOLD && best.as_ref().is_none_or(|(_, b, _)| score > *b) {
            best = Some((baud, score, bytes));
        }
    }

    best.map(|(baud, _, sample)| {
        eprintln!("  best sample: {:?}", sample_preview(&sample));
        baud
    })
}

/// Read from `port` for up to `window`, returning whatever bytes arrived.
fn read_window<R: io::Read + ?Sized>(port: &mut R, window: Duration) -> Vec<u8> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    while Instant::now() < deadline && out.len() < DETECT_MAX_BYTES {
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
    }
    out
}

/// Rate a sample's "looks like ASCII text" quality from 0.0–1.0. Requires line
/// structure (`\n`/`\r`) — matching the expectation set at the prompt — so a
/// coincidentally-clean window at the wrong baud can't win.
fn score_ascii(bytes: &[u8]) -> f32 {
    if bytes.len() < DETECT_MIN_BYTES {
        return 0.0;
    }
    let textlike = bytes.iter().filter(|&&b| is_textlike(b)).count();
    let ratio = textlike as f32 / bytes.len() as f32;
    let has_line = bytes.iter().any(|&b| b == b'\n' || b == b'\r');
    if has_line { ratio } else { ratio * 0.5 }
}

/// Printable ASCII or common whitespace — the bytes we'd expect from text.
fn is_textlike(b: u8) -> bool {
    matches!(b, 0x20..=0x7E | b'\t' | b'\n' | b'\r')
}

/// A short, single-line, printable preview of a sample for display.
fn sample_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(48)
        .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { '·' })
        .collect()
}

/// Is this a phantom legacy UART slot the kernel always exposes even with no
/// chip behind it? Linux publishes each `/dev/ttySN` slot's UART model in
/// `/sys/class/tty/ttySN/type`; `0` == `PORT_UNKNOWN` == "no hardware" → hide.
///
/// Pure file read — no device `open()`, no ioctl, no `unsafe`. Anything we
/// can't positively identify as phantom is kept (fail open, never hide a real
/// port).
fn is_phantom(port: &SerialPortInfo) -> bool {
    // USB / Bluetooth / PCI presence already proves the port is real; only the
    // metadata-less `Unknown` legacy ports are candidates.
    if !matches!(port.port_type, SerialPortType::Unknown) {
        return false;
    }

    // `let ... else` binds on success or bails on the `None`/non-match path.
    let Some(name) = port.port_name.strip_prefix("/dev/") else {
        return false; // not a Linux /dev path (e.g. Windows COM*)
    };
    if !name.starts_with("ttyS") {
        return false; // only the legacy 8250/16550 family can be phantom
    }

    match std::fs::read_to_string(format!("/sys/class/tty/{name}/type")) {
        Ok(contents) => contents.trim() == "0",
        Err(_) => false, // can't read sysfs → can't prove phantom → keep it
    }
}

/// Turn the port's type into a one-line, human-readable description.
fn describe(port_type: &SerialPortType) -> String {
    match port_type {
        SerialPortType::UsbPort(info) => describe_usb(info),
        SerialPortType::BluetoothPort => "Bluetooth".to_string(),
        SerialPortType::PciPort => "PCI".to_string(),
        SerialPortType::Unknown => "Unknown".to_string(),
    }
}

/// USB ports carry extra metadata (vendor/product strings, VID:PID) that make
/// the list much easier to recognize — surface whatever the OS gave us.
fn describe_usb(info: &UsbPortInfo) -> String {
    // `as_deref()` turns `&Option<String>` into `Option<&str>` so we can fall
    // back to a placeholder without cloning.
    let manufacturer = info.manufacturer.as_deref().unwrap_or("unknown vendor");
    let product = info.product.as_deref().unwrap_or("unknown product");

    format!(
        "USB  {manufacturer} — {product}  (VID:PID {:04x}:{:04x})",
        info.vid, info.pid
    )
}
