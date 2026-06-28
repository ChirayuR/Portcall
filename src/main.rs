use serialport::{ClearBuffer, SerialPort, SerialPortInfo, SerialPortType, UsbPortInfo};
use std::io::{self, Read};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tuie::prelude::*;

/// How long a port read blocks before returning `TimedOut`. Short enough that the
/// reader thread notices a disconnect promptly; long enough not to busy-spin.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
const READ_BUF: usize = 1024;

/// Baud rates scanned during auto-detection, from slowest to fastest.
const BAUD_CANDIDATES: &[u32] = &[9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600];
/// How long to listen at each candidate baud during auto-detection.
const DETECT_WINDOW: Duration = Duration::from_millis(350);
/// Minimum bytes received in a window to score a baud rate at all.
const DETECT_MIN_BYTES: usize = 8;
/// Hard cap on bytes per window — prevents a fast/flooding link from bloating memory.
const DETECT_MAX_BYTES: usize = 4096;
/// Minimum text-likeness ratio (0–1) for a baud rate to count as a match.
const DETECT_THRESHOLD: f32 = 0.85;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_all = args.iter().any(|a| a == "--all" || a == "-a");
    let explicit_path = args.iter().find(|a| !a.starts_with('-')).cloned();

    let ports: Vec<PortEntry> = if let Some(path) = explicit_path {
        // Direct-path mode: skip discovery and show a single-port picker.
        vec![PortEntry { name: path, desc: "direct path".into() }]
    } else {
        let raw = match serialport::available_ports() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: failed to enumerate serial ports: {e}");
                return ExitCode::FAILURE;
            }
        };
        let visible: Vec<&SerialPortInfo> =
            raw.iter().filter(|p| show_all || !is_phantom(p)).collect();
        if visible.is_empty() {
            if raw.is_empty() {
                println!("No serial ports found.");
            } else {
                println!(
                    "No usable serial ports found ({} phantom slot(s) hidden).\n\
                     Plug in a device, or pass --all to see everything.",
                    raw.len()
                );
            }
            return ExitCode::SUCCESS;
        }
        visible
            .iter()
            .map(|p| PortEntry { name: p.port_name.clone(), desc: describe(&p.port_type) })
            .collect()
    };

    match run_tui(ports) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tui error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------- Serial connection ----------

/// Owns a live serial link: a background reader thread pushing received bytes into
/// a channel, and the handle for that thread. This is the future `core` library
/// seam — no TUI deps allowed here.
struct Connection {
    /// Receive end of the reader → consumer channel. Iterate to drain chunks.
    rx: mpsc::Receiver<Vec<u8>>,
    #[allow(dead_code)]
    reader: thread::JoinHandle<()>,
}

impl Connection {
    /// Open `path` at `baud` and hand the port off to a background reader thread.
    ///
    /// The reader pushes each received chunk into the returned channel; the caller
    /// drains it at its own pace. Dropping `Connection` detaches the reader thread
    /// but leaves it running until the port closes or EOF.
    fn open(path: &str, baud: u32) -> serialport::Result<Self> {
        let port = serialport::new(path, baud).timeout(READ_TIMEOUT).open()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = thread::spawn(move || reader_loop(port, tx));
        Ok(Connection { rx, reader })
    }
}

/// Producer: block on the serial port and push each received chunk into `tx`.
///
/// Exits when `Ok(0)` (device closed/EOF), a send error (receiver hung up), or a
/// non-timeout read error (typically a disconnect). `TimedOut` is normal — no data
/// yet — so it just loops. Serialport only returns `Ok(0)` after signalling readable,
/// so 0 bytes truly means EOF rather than "no data".
fn reader_loop(mut port: Box<dyn SerialPort>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match port.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("\n[reader] serial read error: {e}");
                return;
            }
        }
    }
}

// ---------- Port metadata ----------

/// A discovered serial port with a human-readable description for display in the
/// port-picker list.
struct PortEntry {
    /// OS path to the port (e.g. `/dev/ttyACM0`, `COM3`).
    name: String,
    /// One-line description: type + USB manufacturer/product/VID:PID if available.
    desc: String,
}

// ---------- TUI messages (sent between threads via tuie::send) ----------

/// Chunk of raw bytes received from the serial port; forwarded to [`RxScreen`].
struct RxBytes(Vec<u8>);

/// Progress update from the baud-detection background thread: one baud rate tried
/// with its ASCII-likeness score.
struct ScanUpdate {
    rate: u32,
    score: f32,
}

/// Final result from the baud-detection background thread: the best-scoring baud
/// that exceeded [`DETECT_THRESHOLD`], or `None` if no rate matched.
struct ScanDone(Option<u32>);

// ---------- Port picker screen ----------

/// Renderer context for the port-picker [`List`]: the available ports and which
/// one is currently highlighted.
struct PortPickerCtx {
    ports: Vec<PortEntry>,
    selected: usize,
}

/// Virtualized `List` renderer for the port picker. Prefixes the selected row with
/// `"> "` so it visually stands out; all others get two spaces.
fn render_port(ctx: &mut PortPickerCtx, idx: usize) -> Option<Box<dyn Widget>> {
    let entry = ctx.ports.get(idx)?;
    let prefix = if idx == ctx.selected { "> " } else { "  " };
    let line = format!("{}{:<22}  {}", prefix, entry.name, entry.desc);
    Some(Text::new().content(line) as Box<dyn Widget>)
}

/// TUI screen that lets the user pick a port with arrow keys and Enter.
///
/// Layout: title bar → bordered scrollable port list (flex) → footer hint.
/// Sets [`selected_port`] when the user confirms; [`App`] detects this and
/// transitions to [`BaudPickerScreen`].
struct PortPickerScreen {
    root: Box<Pane>,
    list_id: WidgetId<List>,
    port_count: usize,
    /// Set to the chosen port name on Enter; `None` until the user confirms.
    selected_port: Option<String>,
}

impl PortPickerScreen {
    fn new(ports: Vec<PortEntry>) -> Box<Self> {
        let port_count = ports.len();
        let mut list_id = WidgetId::EMPTY;

        let mut list = List::new();
        list.set_renderer(PortPickerCtx { ports, selected: 0 }, render_port);
        list.set_item_count(port_count);

        let title = Text::new().content("portcall — select a port");
        let footer = Text::new().content("↑↓ navigate   Enter select   q quit");

        let content = Pane::new()
            .border(Border::SINGLE)
            .child(list.id(&mut list_id).flex(1));

        let root = Pane::new()
            .child(title)
            .child(content.flex(1))
            .child(footer);

        Box::new(Self { root, list_id, port_count, selected_port: None })
    }
}

impl DelegateWidget for PortPickerScreen {
    tuie::delegate_widget!(root);

    fn override_on_input(&mut self, queue: &mut InputQueue) -> InputResult {
        let Some(ev) = queue.peek() else {
            return self.get_delegate_mut().on_input(queue);
        };

        if ev.chord == chord!(q) || ev.chord == chord!(Ctrl + c) {
            queue.next();
            tuie::quit(0);
            return InputResult::Handled;
        }

        if ev.chord == chord!(Up) || ev.chord == chord!(Down) {
            let up = ev.chord == chord!(Up);
            queue.next();
            let n = self.port_count;
            if n == 0 {
                return InputResult::Handled;
            }
            {
                let Some(list) = self.root.get_widget_mut(self.list_id) else {
                    return InputResult::Handled;
                };
                let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
                ctx.selected = if up {
                    ctx.selected.saturating_sub(1)
                } else {
                    (ctx.selected + 1).min(n - 1)
                };
                let s = ctx.selected;
                list.ensure_visible(s);
                list.invalidate_all();
            }
            return InputResult::Handled;
        }

        if ev.chord == chord!(Enter) {
            queue.next();
            let selected = {
                let Some(list) = self.root.get_widget_mut(self.list_id) else {
                    return InputResult::Handled;
                };
                let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
                ctx.ports.get(ctx.selected).map(|e| e.name.clone())
            };
            self.selected_port = selected;
            return InputResult::Handled;
        }

        self.get_delegate_mut().on_input(queue)
    }
}

// ---------- Baud picker screen ----------

/// State machine for the baud-selection flow.
enum BaudPhase {
    /// Initial state: show the A/B/digit prompt.
    Prompt,
    /// Auto-detection running; accumulates one line per scanned baud.
    Scanning { lines: Vec<String> },
    /// Manual entry: user is typing a baud rate.
    Manual { input: String },
    /// Baud resolved (auto or manual); [`App`] will transition to [`RxScreen`].
    Done(u32),
}

/// TUI screen that resolves a baud rate — either by scanning or manual entry.
///
/// Layout: title bar → bordered content area (flex) → footer hint.
/// Starts in [`BaudPhase::Prompt`]; transitions via user input or incoming
/// [`ScanUpdate`]/[`ScanDone`] messages from [`scan_baud_thread`].
struct BaudPickerScreen {
    root: Box<Pane>,
    content_id: WidgetId<Text>,
    footer_id: WidgetId<Text>,
    /// Port path this screen is resolving a baud for.
    port: String,
    phase: BaudPhase,
    /// Set to the confirmed baud rate; [`App`] reads and acts on this.
    resolved: Option<u32>,
    /// Signals [`App`] to spawn [`scan_baud_thread`] on the next tick.
    wants_scan: bool,
}

impl BaudPickerScreen {
    fn new(port: String) -> Box<Self> {
        let mut content_id = WidgetId::EMPTY;
        let mut footer_id = WidgetId::EMPTY;

        let title = Text::new().content(format!("portcall — {port}"));

        let content_text = Text::new()
            .word_wrap()
            .content(BaudPickerScreen::prompt_text());

        let content_area = Pane::new()
            .border(Border::SINGLE)
            .child(content_text.id(&mut content_id));

        let footer = Text::new().content("A auto-detect   B manual baud   q quit");

        let root = Pane::new()
            .child(title)
            .child(content_area.flex(1))
            .child(footer.id(&mut footer_id));

        Box::new(Self {
            root,
            content_id,
            footer_id,
            port,
            phase: BaudPhase::Prompt,
            resolved: None,
            wants_scan: false,
        })
    }

    fn prompt_text() -> &'static str {
        "Device must be transmitting ASCII text for auto-detection.\n\
         \n\
         Press A  to auto-detect baud rate\n\
         Press B  to enter baud rate manually\n\
         Or start typing the baud rate directly."
    }

    /// Sync the content-area [`Text`] widget with the current [`BaudPhase`].
    fn update_content(&mut self) {
        let text = match &self.phase {
            BaudPhase::Prompt => Self::prompt_text().to_string(),
            BaudPhase::Scanning { lines } => {
                let mut s = "Scanning baud rates…\n\n".to_string();
                for l in lines {
                    s.push_str(l);
                    s.push('\n');
                }
                s
            }
            BaudPhase::Manual { input } => {
                format!("Baud rate: {}▌", input)
            }
            BaudPhase::Done(baud) => {
                format!("auto: {} baud detected.\n\nConnecting…", baud)
            }
        };
        if let Some(w) = self.root.get_widget_mut(self.content_id) {
            w.set_content(text);
        }
    }

    /// Sync the footer hint [`Text`] widget with the current [`BaudPhase`].
    fn update_footer(&mut self) {
        let text = match &self.phase {
            BaudPhase::Prompt => "A auto-detect   B manual baud   q quit",
            BaudPhase::Scanning { .. } => "Scanning…   q quit",
            BaudPhase::Manual { .. } => {
                "0–9 type baud   Backspace delete   Enter confirm   q quit"
            }
            BaudPhase::Done(_) => "Connecting…",
        };
        if let Some(w) = self.root.get_widget_mut(self.footer_id) {
            w.set_content(text);
        }
    }
}

impl DelegateWidget for BaudPickerScreen {
    tuie::delegate_widget!(root);

    fn override_on_event(&mut self, event: &mut WidgetEvent) {
        if let Some(update) = event.take::<ScanUpdate>() {
            if let BaudPhase::Scanning { lines } = &mut self.phase {
                let mark = if update.score >= DETECT_THRESHOLD { "  ← best" } else { "" };
                lines.push(format!("  {:>7}  {:.2}{}", update.rate, update.score, mark));
                self.update_content();
            }
            return;
        }
        if let Some(done) = event.take::<ScanDone>() {
            // Only act if still scanning; if user switched to manual, ignore result.
            if matches!(self.phase, BaudPhase::Scanning { .. }) {
                match done.0 {
                    Some(baud) => {
                        self.phase = BaudPhase::Done(baud);
                        self.resolved = Some(baud);
                    }
                    None => {
                        self.phase = BaudPhase::Manual { input: String::new() };
                    }
                }
                self.update_content();
                self.update_footer();
            }
            return;
        }
        self.get_delegate_mut().on_event(event);
    }

    fn override_on_input(&mut self, queue: &mut InputQueue) -> InputResult {
        let Some(ev) = queue.peek() else {
            return self.get_delegate_mut().on_input(queue);
        };

        if ev.chord == chord!(q) || ev.chord == chord!(Ctrl + c) {
            queue.next();
            tuie::quit(0);
            return InputResult::Handled;
        }

        match &self.phase {
            BaudPhase::Prompt => {
                if ev.chord == chord!(a) || ev.chord == chord!(Enter) {
                    queue.next();
                    self.phase = BaudPhase::Scanning { lines: Vec::new() };
                    self.wants_scan = true;
                    self.update_content();
                    self.update_footer();
                    return InputResult::Handled;
                }
                if ev.chord == chord!(b) {
                    queue.next();
                    self.phase = BaudPhase::Manual { input: String::new() };
                    self.update_content();
                    self.update_footer();
                    return InputResult::Handled;
                }
                // A digit switches directly to manual entry with that digit pre-filled.
                if let Trigger::Key(Key::Char(c)) = ev.chord.trigger {
                    if c.is_ascii_digit() {
                        queue.next();
                        self.phase = BaudPhase::Manual { input: c.to_string() };
                        self.update_content();
                        self.update_footer();
                        return InputResult::Handled;
                    }
                }
            }
            BaudPhase::Scanning { .. } => {
                // No input accepted during scanning (other than quit, handled above).
            }
            BaudPhase::Manual { .. } => {
                if let Trigger::Key(Key::Char(c)) = ev.chord.trigger {
                    if c.is_ascii_digit() {
                        queue.next();
                        if let BaudPhase::Manual { input } = &mut self.phase {
                            input.push(c);
                        }
                        self.update_content();
                        return InputResult::Handled;
                    }
                }
                if ev.chord == chord!(Backspace) {
                    queue.next();
                    if let BaudPhase::Manual { input } = &mut self.phase {
                        input.pop();
                    }
                    self.update_content();
                    return InputResult::Handled;
                }
                if ev.chord == chord!(Enter) {
                    queue.next();
                    let baud = if let BaudPhase::Manual { input } = &self.phase {
                        input.parse::<u32>().ok().filter(|&b| b > 0)
                    } else {
                        None
                    };
                    if let Some(b) = baud {
                        self.phase = BaudPhase::Done(b);
                        self.resolved = Some(b);
                        self.update_content();
                        self.update_footer();
                    }
                    return InputResult::Handled;
                }
            }
            BaudPhase::Done(_) => {}
        }

        self.get_delegate_mut().on_input(queue)
    }
}

/// Background thread: scan [`BAUD_CANDIDATES`] in order, send a [`ScanUpdate`]
/// after each, then send [`ScanDone`] with the best-scoring baud (or `None`).
///
/// Uses the same listen-and-score logic as the earlier CLI auto-baud detector, but
/// sends progress via [`tuie::send`] so the [`BaudPickerScreen`] can display it live.
fn scan_baud_thread(port: String, target: WidgetId<BaudPickerScreen>) {
    let mut port_obj = match serialport::new(&port, BAUD_CANDIDATES[0])
        .timeout(READ_TIMEOUT)
        .open()
    {
        Ok(p) => p,
        Err(_) => {
            tuie::send(target, ScanDone(None));
            return;
        }
    };

    let mut best: Option<u32> = None;
    let mut best_score: f32 = 0.0;

    for &rate in BAUD_CANDIDATES {
        if port_obj.set_baud_rate(rate).is_err() {
            continue;
        }
        let _ = port_obj.clear(ClearBuffer::Input);
        let bytes = read_window(&mut *port_obj, DETECT_WINDOW);
        let score = score_ascii(&bytes);
        tuie::send(target, ScanUpdate { rate, score });
        if score >= DETECT_THRESHOLD && score > best_score {
            best = Some(rate);
            best_score = score;
        }
    }

    tuie::send(target, ScanDone(best));
}

// ---------- RX screen ----------

/// Line buffer for incoming serial data.
///
/// `feed` appends raw bytes and emits a completed `String` for each `\n` found,
/// trimming the trailing `\n`/`\r`. The partial line between the last `\n` and EOF
/// is buffered until more data arrives. Line-based buffering is intentional: it sets
/// up the future auto-tab aggregation of recurring lines.
#[derive(Default)]
struct RxLines {
    lines: Vec<String>,
    partial: Vec<u8>,
}

impl RxLines {
    /// Append `bytes`, emitting a completed line into `self.lines` for each `\n`.
    fn feed(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line);
            self.lines.push(text.trim_end_matches(['\n', '\r']).to_string());
        }
    }
}

/// Virtualized `List` renderer for the RX view: returns one `Text` widget per
/// completed line. tuie only calls this for rows currently in the viewport.
fn render_rx_line(ctx: &mut RxLines, index: usize) -> Option<Box<dyn Widget>> {
    let line = ctx.lines.get(index)?;
    Some(Text::new().content(line.clone()) as Box<dyn Widget>)
}

/// TUI screen that streams and displays incoming serial data.
///
/// Layout: title bar (port + baud) → bordered scrolling line list (flex) → status
/// bar (line count + scroll hint). Receives [`RxBytes`] messages from a forwarder
/// thread that drains the [`Connection`] channel.
struct RxScreen {
    root: Box<Pane>,
    list_id: WidgetId<List>,
    status_id: WidgetId<Text>,
    line_count: usize,
}

impl RxScreen {
    fn new(port: &str, baud: u32) -> Box<Self> {
        let mut list_id = WidgetId::EMPTY;
        let mut status_id = WidgetId::EMPTY;

        let title = Text::new().content(format!("portcall — {port} @ {baud} baud"));
        let status = Text::new().content("0 lines   ↑↓ scroll   q quit");

        let mut list = List::new();
        list.set_renderer(RxLines::default(), render_rx_line);

        let content = Pane::new()
            .border(Border::SINGLE)
            .child(list.id(&mut list_id).flex(1));

        let root = Pane::new()
            .child(title)
            .child(content.flex(1))
            .child(status.id(&mut status_id));

        Box::new(Self { root, list_id, status_id, line_count: 0 })
    }

    /// Feed a received chunk into the line buffer, update the list count, and keep
    /// the viewport pinned to the latest line.
    fn append(&mut self, bytes: Vec<u8>) {
        let count = {
            let Some(list) = self.root.get_widget_mut(self.list_id) else { return };
            let ctx = list.get_context_mut::<RxLines>().expect("rx context");
            ctx.feed(&bytes);
            let n = ctx.lines.len();
            list.set_item_count(n);
            if n > 0 {
                list.ensure_visible(n - 1);
            }
            n
        };
        if count != self.line_count {
            self.line_count = count;
            self.update_status();
        }
    }

    /// Refresh the status bar text with the current line count.
    fn update_status(&mut self) {
        let n = self.line_count;
        let text = format!(
            "{} line{}   ↑↓ scroll   q quit",
            n,
            if n == 1 { "" } else { "s" }
        );
        if let Some(w) = self.root.get_widget_mut(self.status_id) {
            w.set_content(text);
        }
    }
}

impl DelegateWidget for RxScreen {
    tuie::delegate_widget!(root);

    fn override_on_event(&mut self, event: &mut WidgetEvent) {
        if let Some(bytes) = event.take::<RxBytes>() {
            self.append(bytes.0);
        } else {
            self.get_delegate_mut().on_event(event);
        }
    }

    fn override_on_input(&mut self, queue: &mut InputQueue) -> InputResult {
        if let Some(ev) = queue.peek() {
            if ev.chord == chord!(q) || ev.chord == chord!(Ctrl + c) {
                queue.next();
                tuie::quit(0);
                return InputResult::Handled;
            }
        }
        self.get_delegate_mut().on_input(queue)
    }
}

// ---------- App — screen manager ----------

/// The currently visible TUI screen.
enum AppScreen {
    PortPicking(Box<PortPickerScreen>),
    BaudPicking(Box<BaudPickerScreen>),
    Rx(Box<RxScreen>),
}

impl AppScreen {
    fn as_widget(&self) -> &dyn Widget {
        match self {
            Self::PortPicking(w) => &**w,
            Self::BaudPicking(w) => &**w,
            Self::Rx(w) => &**w,
        }
    }
    fn as_widget_mut(&mut self) -> &mut dyn Widget {
        match self {
            Self::PortPicking(w) => &mut **w,
            Self::BaudPicking(w) => &mut **w,
            Self::Rx(w) => &mut **w,
        }
    }
}

/// Root widget: drives the three-screen flow (port pick → baud pick → rx view).
///
/// Implements [`DelegateWidget`] and forwards all widget operations to whichever
/// [`AppScreen`] is active. Because tuie resolves messages by matching the root
/// widget's identity against the target [`WidgetId`], the effective identity changes
/// with each screen swap — messages sent to a previous screen's ID are silently
/// dropped once the screen transitions, which is the desired behaviour.
struct App {
    screen: AppScreen,
    /// Captured ID of the current [`BaudPickerScreen`]; used by [`scan_baud_thread`]
    /// to send progress back. Stale after the baud screen is replaced.
    baud_screen_id: WidgetId<BaudPickerScreen>,
}

impl App {
    fn new(ports: Vec<PortEntry>) -> Box<Self> {
        Box::new(Self {
            screen: AppScreen::PortPicking(PortPickerScreen::new(ports)),
            baud_screen_id: WidgetId::EMPTY,
        })
    }

    /// Check every screen's pending-transition flags and act on the first one found.
    ///
    /// Called after every `on_event` and `on_input` delegation so that transitions
    /// triggered by either path (background messages or keyboard) are handled promptly.
    fn check_transitions(&mut self) {
        // Port picker → baud picker
        let to_baud: Option<String> =
            if let AppScreen::PortPicking(w) = &mut self.screen {
                w.selected_port.take()
            } else {
                None
            };
        if let Some(port) = to_baud {
            self.transition_to_baud(port);
            return;
        }

        // Baud picker: spawn scan thread once on request
        let scan_for: Option<String> =
            if let AppScreen::BaudPicking(w) = &mut self.screen {
                if w.wants_scan {
                    w.wants_scan = false;
                    Some(w.port.clone())
                } else {
                    None
                }
            } else {
                None
            };
        if let Some(port) = scan_for {
            let id = self.baud_screen_id;
            thread::spawn(move || scan_baud_thread(port, id));
        }

        // Baud picker → rx view
        let to_rx: Option<(String, u32)> =
            if let AppScreen::BaudPicking(w) = &mut self.screen {
                w.resolved.take().map(|baud| (w.port.clone(), baud))
            } else {
                None
            };
        if let Some((port, baud)) = to_rx {
            self.transition_to_rx(port, baud);
        }
    }

    fn transition_to_baud(&mut self, port: String) {
        let mut baud_id = WidgetId::EMPTY;
        let baud_screen = BaudPickerScreen::new(port).id(&mut baud_id);
        self.baud_screen_id = baud_id;
        self.screen = AppScreen::BaudPicking(baud_screen);
        tuie::dirty_layout();
    }

    fn transition_to_rx(&mut self, port: String, baud: u32) {
        let conn = match Connection::open(&port, baud) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to open {port} @ {baud}: {e}");
                tuie::quit(1);
                return;
            }
        };

        let mut rx_id: WidgetId<RxScreen> = WidgetId::EMPTY;
        let rx_screen = RxScreen::new(&port, baud).id(&mut rx_id);

        self.screen = AppScreen::Rx(rx_screen);
        tuie::dirty_layout();

        // Forwarder: drain the reader channel and push each chunk to the RX screen.
        let Connection { rx, reader: _ } = conn;
        thread::spawn(move || {
            for chunk in rx {
                tuie::send(rx_id, RxBytes(chunk));
            }
        });
    }
}

impl DelegateWidget for App {
    fn get_delegate(&self) -> &dyn Widget {
        self.screen.as_widget()
    }
    fn get_delegate_mut(&mut self) -> &mut dyn Widget {
        self.screen.as_widget_mut()
    }

    fn override_on_event(&mut self, event: &mut WidgetEvent) {
        self.screen.as_widget_mut().on_event(event);
        self.check_transitions();
    }

    fn override_on_input(&mut self, queue: &mut InputQueue) -> InputResult {
        let result = self.screen.as_widget_mut().on_input(queue);
        self.check_transitions();
        result
    }
}

// ---------- Entry point ----------

/// Start the TUI with the given port list and block until the user quits.
fn run_tui(ports: Vec<PortEntry>) -> io::Result<ExitCode> {
    tuie::start_tui(App::new(ports))
}

// ---------- Baud detection helpers ----------

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

/// Rate a byte slice's "looks like ASCII text" quality from 0.0–1.0.
///
/// Requires line structure (`\n`/`\r`) in the sample: a high ratio of printable
/// bytes at the wrong baud can still score near 1.0, but line endings are far less
/// likely to appear by coincidence and so gate the final score.
fn score_ascii(bytes: &[u8]) -> f32 {
    if bytes.len() < DETECT_MIN_BYTES {
        return 0.0;
    }
    let textlike = bytes.iter().filter(|&&b| is_textlike(b)).count();
    let ratio = textlike as f32 / bytes.len() as f32;
    let has_line = bytes.iter().any(|&b| b == b'\n' || b == b'\r');
    if has_line { ratio } else { ratio * 0.5 }
}

/// Returns `true` for printable ASCII and common whitespace characters.
fn is_textlike(b: u8) -> bool {
    matches!(b, 0x20..=0x7E | b'\t' | b'\n' | b'\r')
}

// ---------- Port metadata helpers ----------

/// Returns `true` if `port` is a phantom legacy UART slot the kernel always
/// exposes even with no chip behind it.
///
/// Linux publishes each `/dev/ttySN` slot's UART model in
/// `/sys/class/tty/ttySN/type`; `0` == `PORT_UNKNOWN` == "no hardware" → phantom.
/// Non-`Unknown` port types (USB, Bluetooth, PCI) are never phantom. Fails open:
/// anything we can't positively identify as phantom is kept.
fn is_phantom(port: &SerialPortInfo) -> bool {
    if !matches!(port.port_type, SerialPortType::Unknown) {
        return false;
    }
    let Some(name) = port.port_name.strip_prefix("/dev/") else {
        return false;
    };
    if !name.starts_with("ttyS") {
        return false;
    }
    match std::fs::read_to_string(format!("/sys/class/tty/{name}/type")) {
        Ok(contents) => contents.trim() == "0",
        Err(_) => false,
    }
}

/// One-line, human-readable description of a port type.
fn describe(port_type: &SerialPortType) -> String {
    match port_type {
        SerialPortType::UsbPort(info) => describe_usb(info),
        SerialPortType::BluetoothPort => "Bluetooth".to_string(),
        SerialPortType::PciPort => "PCI".to_string(),
        SerialPortType::Unknown => "Unknown".to_string(),
    }
}

/// Formats USB port metadata: vendor/product strings and VID:PID.
fn describe_usb(info: &UsbPortInfo) -> String {
    let manufacturer = info.manufacturer.as_deref().unwrap_or("unknown vendor");
    let product = info.product.as_deref().unwrap_or("unknown product");
    format!(
        "USB  {manufacturer} — {product}  (VID:PID {:04x}:{:04x})",
        info.vid, info.pid
    )
}
