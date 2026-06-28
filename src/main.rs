use serialport::{ClearBuffer, SerialPort, SerialPortInfo, SerialPortType, UsbPortInfo};
use std::io::{self, Read};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tuie::prelude::*;

/// Set on quit so background threads (reader, scan) can exit without waiting for their timeout.
static QUITTING: AtomicBool = AtomicBool::new(false);

const READ_TIMEOUT: Duration = Duration::from_millis(100);
const READ_BUF: usize = 1024;
const BAUD_CANDIDATES: &[u32] = &[9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600];
const DETECT_WINDOW: Duration = Duration::from_millis(350);
const DETECT_MIN_BYTES: usize = 8;
const DETECT_MAX_BYTES: usize = 4096;
const DETECT_THRESHOLD: f32 = 0.85;

/// A line seen this many times is promoted to the pinned panel and suppressed
/// from the live scroll.
const PIN_THRESHOLD: usize = 3;
/// Maximum pinned entries shown simultaneously.
const MAX_PINS_DISPLAY: usize = 8;
/// Minimum shared byte-prefix length for two lines to be considered the same "field".
const LCP_THRESHOLD: usize = 6;

/// Signal all background threads to exit, then request the TUI to shut down.
fn request_quit(code: u8) {
    QUITTING.store(true, Ordering::Relaxed);
    tuie::quit(code);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_all = args.iter().any(|a| a == "--all" || a == "-a");
    let explicit_path = args.iter().find(|a| !a.starts_with('-')).cloned();

    let ports: Vec<PortEntry> = if let Some(path) = explicit_path {
        vec![PortEntry { name: path, kind: PortKind::Unknown, desc: "direct path".into() }]
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
            .map(|p| {
                let (kind, desc) = classify_port(&p.port_type);
                PortEntry { name: p.port_name.clone(), kind, desc }
            })
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

/// Owns a live serial link: a background reader thread and the channel it writes to.
struct Connection {
    rx: mpsc::Receiver<Vec<u8>>,
    #[allow(dead_code)]
    reader: thread::JoinHandle<()>,
}

impl Connection {
    fn open(path: &str, baud: u32) -> serialport::Result<Self> {
        let port = serialport::new(path, baud).timeout(READ_TIMEOUT).open()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = thread::spawn(move || reader_loop(port, tx));
        Ok(Connection { rx, reader })
    }
}

/// Producer: block on the port and push each received chunk into `tx`.
/// `Ok(0)` = EOF (device closed). `TimedOut` = no data yet (normal, keep looping).
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
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                if QUITTING.load(Ordering::Relaxed) {
                    return;
                }
            }
            Err(e) => {
                // Suppress errors caused by the port being closed on normal exit.
                if !QUITTING.load(Ordering::Relaxed) {
                    eprintln!("\n[reader] serial read error: {e}");
                }
                return;
            }
        }
    }
}

// ---------- Port metadata ----------

/// Broad category of a serial port's physical interface.
#[derive(Clone)]
enum PortKind {
    Usb,
    Bluetooth,
    Pci,
    Unknown,
}

/// A discovered serial port ready for display in the port-picker list.
struct PortEntry {
    name: String,
    kind: PortKind,
    /// Human-readable device description (manufacturer, product, VID:PID, etc.).
    desc: String,
}

// ---------- TUI messages ----------

struct RxBytes(Vec<u8>);
struct ScanUpdate { rate: u32, score: f32 }
struct ScanDone(Option<u32>);

// ---------- Port picker screen ----------

struct PortPickerCtx {
    ports: Vec<PortEntry>,
    selected: usize,
}

/// Virtualized renderer for one port-picker row.
/// Selected row: bold name + coloured type badge.
/// Unselected: dim name + dim badge.
fn render_port(ctx: &mut PortPickerCtx, idx: usize) -> Option<Box<dyn Widget>> {
    let entry = ctx.ports.get(idx)?;
    let sel = idx == ctx.selected;

    let mut s = StyledString::new();

    if sel {
        s.push_span(" ▶ ".bold());
    } else {
        s.push_str("   ");
    }

    let name = entry.name.as_str();
    if sel {
        s.push_span(name.bold());
    } else {
        s.push_span(name.dim());
    }

    s.push_str("  ");

    let badge = match entry.kind {
        PortKind::Usb       => "USB",
        PortKind::Bluetooth => "BT ",
        PortKind::Pci       => "PCI",
        PortKind::Unknown   => "   ",
    };
    match (&entry.kind, sel) {
        (PortKind::Usb,       true)  => s.push_span(badge.cyan().bold()),
        (PortKind::Usb,       false) => s.push_span(badge.cyan().dim()),
        (PortKind::Bluetooth, true)  => s.push_span(badge.blue().bold()),
        (PortKind::Bluetooth, false) => s.push_span(badge.blue().dim()),
        (PortKind::Pci,       true)  => s.push_span(badge.yellow().bold()),
        (PortKind::Pci,       false) => s.push_span(badge.yellow().dim()),
        (PortKind::Unknown,   _)     => s.push_span(badge.dim()),
    }

    Some(Text::new().content(s) as Box<dyn Widget>)
}

/// First TUI screen: interactive port list with a detail panel.
/// Left pane: navigable port list. Right pane: full device info for the selected port.
/// Sets `selected_port` when the user presses Enter (or mouse click); `App` transitions to baud.
struct PortPickerScreen {
    root: Box<Pane>,
    list_id: WidgetId<List>,
    detail_id: WidgetId<Text>,
    port_count: usize,
    selected_port: Option<String>,
}

impl PortPickerScreen {
    fn new(ports: Vec<PortEntry>) -> Box<Self> {
        let port_count = ports.len();
        let mut list_id = WidgetId::EMPTY;
        let mut detail_id = WidgetId::EMPTY;

        let mut list = List::new();
        list.set_renderer(PortPickerCtx { ports, selected: 0 }, render_port);
        list.set_item_count(port_count);

        let detail = Text::new().word_wrap().content("").id(&mut detail_id);

        let split = Split::new(
            SplitPane::new().horizontal().children([
                SplitPaneChild::from(list.id(&mut list_id).flex(1))
                    .title(" portcall "),
                SplitPaneChild::from(detail.flex(2))
                    .title(" device "),
            ]),
        );

        let footer = styled_footer("  ↑↓ / hover  navigate   Enter / click  select   q quit");

        let root = Pane::new()
            .child(split.flex(1))
            .child(footer);

        let mut screen = Box::new(Self { root, list_id, detail_id, port_count, selected_port: None });
        screen.update_detail();
        screen
    }

    fn update_detail(&mut self) {
        let info = {
            let Some(list) = self.root.get_widget_mut(self.list_id) else { return; };
            let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
            ctx.ports.get(ctx.selected).map(|e| (e.name.clone(), e.kind.clone(), e.desc.clone()))
        };

        let content: StyledString = match info {
            None => {
                let mut s = StyledString::new();
                s.push_span("\n  No devices found.".dim());
                s
            }
            Some((name, kind, desc)) => {
                let mut s = StyledString::new();
                s.push_str("\n");
                s.push_span(format!("  {name}\n\n").as_str().bold());
                s.push_span("  Type   ".dim());
                let (kind_str, color) = match kind {
                    PortKind::Usb       => ("USB",       Color::CYAN),
                    PortKind::Bluetooth => ("Bluetooth", Color::BLUE),
                    PortKind::Pci       => ("PCI",       Color::YELLOW),
                    PortKind::Unknown   => ("Unknown",   Color::Foreground),
                };
                s.push_span(kind_str.fg(color).bold());
                s.push_str("\n\n");
                s.push_span(format!("  {desc}\n").as_str().dim());
                s
            }
        };

        if let Some(w) = self.root.get_widget_mut(self.detail_id) {
            w.set_content(content);
        }
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
            request_quit(0);
            return InputResult::Handled;
        }

        // Mouse hover → highlight the item under the cursor.
        // Row 0 = split pane border-top, rows 1+ = list items.
        if matches!(ev.chord.trigger, Trigger::MouseHover) {
            let row = ev.cell().y;
            let n = self.port_count;
            if row >= 1 && n > 0 {
                let item = (row - 1) as usize;
                if item < n {
                    queue.next();
                    let changed = {
                        let Some(list) = self.root.get_widget_mut(self.list_id) else {
                            return InputResult::Handled;
                        };
                        let old = {
                            let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
                            let old = ctx.selected;
                            ctx.selected = item;
                            old
                        };
                        if old != item {
                            list.ensure_visible(item);
                            list.invalidate_all();
                            true
                        } else {
                            false
                        }
                    };
                    if changed {
                        self.update_detail();
                    }
                    return InputResult::Handled;
                }
            }
        }

        // Mouse click → select the item and confirm (same as Enter).
        if let Trigger::MouseDown(MouseButton::Left) = ev.chord.trigger {
            let row = ev.cell().y;
            let n = self.port_count;
            if row >= 1 && n > 0 {
                let item = (row - 1) as usize;
                if item < n {
                    queue.next();
                    let selected = {
                        let Some(list) = self.root.get_widget_mut(self.list_id) else {
                            return InputResult::Handled;
                        };
                        let name = {
                            let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
                            ctx.selected = item;
                            ctx.ports.get(item).map(|e| e.name.clone())
                        };
                        list.invalidate_all();
                        name
                    };
                    self.selected_port = selected;
                    return InputResult::Handled;
                }
            }
        }

        if ev.chord == chord!(Up) || ev.chord == chord!(Down) {
            let up = ev.chord == chord!(Up);
            queue.next();
            let n = self.port_count;
            if n > 0 {
                {
                    let Some(list) = self.root.get_widget_mut(self.list_id) else {
                        return InputResult::Handled;
                    };
                    let s = {
                        let ctx = list.get_context_mut::<PortPickerCtx>().unwrap();
                        ctx.selected = if up {
                            ctx.selected.saturating_sub(1)
                        } else {
                            (ctx.selected + 1).min(n - 1)
                        };
                        ctx.selected
                    };
                    list.ensure_visible(s);
                    list.invalidate_all();
                }
                self.update_detail();
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

/// State machine for the baud-resolution flow.
enum BaudPhase {
    Prompt,
    Scanning { lines: Vec<StyledString> },
    Manual { input: String },
    Done(u32),
}

struct BaudPickerScreen {
    root: Box<Pane>,
    content_id: WidgetId<Text>,
    footer_id: WidgetId<Text>,
    port: String,
    phase: BaudPhase,
    resolved: Option<u32>,
    wants_scan: bool,
    /// Tab-bar selection in the Prompt phase: 0 = Auto-detect, 1 = Manual.
    tab: usize,
}

impl BaudPickerScreen {
    fn new(port: String) -> Box<Self> {
        let mut content_id = WidgetId::EMPTY;
        let mut footer_id = WidgetId::EMPTY;

        let title_str = format!(" portcall — {port} ");

        let content_area = Pane::new()
            .border(Border::SINGLE)
            .title(title_str)
            .child(
                Text::new()
                    .word_wrap()
                    .content(Self::prompt_tabs(0))
                    .id(&mut content_id),
            );

        let footer = Text::new()
            .content(styled_footer_str("  ← → Tab switch   Enter confirm   q quit"))
            .id(&mut footer_id);

        let root = Pane::new()
            .child(content_area.flex(1))
            .child(footer);

        Box::new(Self {
            root, content_id, footer_id,
            port, phase: BaudPhase::Prompt,
            resolved: None, wants_scan: false,
            tab: 0,
        })
    }

    /// Build the tab-bar StyledString for the Prompt phase.
    fn prompt_tabs(selected: usize) -> StyledString {
        let mut s = StyledString::new();
        // Tab 0 — Auto-detect
        if selected == 0 {
            s.push_span("  Auto-detect  ".bold().reverse());
        } else {
            s.push_span("  Auto-detect  ".dim());
        }
        s.push_str("   ");
        // Tab 1 — Manual baud
        if selected == 1 {
            s.push_span("  Manual baud  ".bold().reverse());
        } else {
            s.push_span("  Manual baud  ".dim());
        }
        s.push_str("\n\n");
        if selected == 0 {
            s.push_span("Device must be transmitting ASCII text for auto-detection.".dim());
        } else {
            s.push_span("Type the baud rate and press Enter.".dim());
        }
        s
    }

    fn update_content(&mut self) {
        let text: StyledString = match &self.phase {
            BaudPhase::Prompt => Self::prompt_tabs(self.tab),
            BaudPhase::Scanning { lines } => {
                let mut s = StyledString::new();
                s.push_span("Scanning baud rates…\n\n".bold());
                s.push_span("    baud      score\n".dim());
                for l in lines {
                    s.append(l);
                    s.push_str("\n");
                }
                s
            }
            BaudPhase::Manual { input } => {
                let mut s = StyledString::new();
                s.push_span("Baud rate: ".dim());
                s.push_span(input.as_str().bold());
                s.push_str("▌");
                s
            }
            BaudPhase::Done(baud) => {
                let baud_str = format!("{baud}");
                let mut s = StyledString::new();
                s.push_span("Detected  ".dim());
                s.push_span(baud_str.as_str().cyan().bold());
                s.push_span(" baud.\n\nConnecting…".dim());
                s
            }
        };
        if let Some(w) = self.root.get_widget_mut(self.content_id) {
            w.set_content(text);
        }
    }

    fn update_footer(&mut self) {
        let msg = match &self.phase {
            BaudPhase::Prompt     => "  ← → Tab switch   Enter confirm   q quit",
            BaudPhase::Scanning { .. } => "  Scanning…   q quit",
            BaudPhase::Manual { .. }   => "  0–9 type baud   Backspace   Enter confirm   q quit",
            BaudPhase::Done(_)         => "  Connecting…",
        };
        if let Some(w) = self.root.get_widget_mut(self.footer_id) {
            w.set_content(styled_footer_str(msg));
        }
    }
}

impl DelegateWidget for BaudPickerScreen {
    tuie::delegate_widget!(root);

    fn override_on_event(&mut self, event: &mut WidgetEvent) {
        if let Some(update) = event.take::<ScanUpdate>() {
            if let BaudPhase::Scanning { lines } = &mut self.phase {
                let (filled, empty) = score_bar_parts(update.score);
                let hit = update.score >= DETECT_THRESHOLD;
                let rate_str = format!("  {:>7}  ", update.rate);
                let score_str = format!("  {:.2}", update.score);
                let mut line = StyledString::new();
                line.push_span(rate_str.as_str().dim());
                line.push_span(filled.as_str().cyan());
                line.push_span(empty.as_str().dim());
                if hit {
                    line.push_span(score_str.as_str().green().bold());
                    line.push_span("  ← best".green().bold());
                } else {
                    line.push_span(score_str.as_str().dim());
                }
                lines.push(line);
                self.update_content();
            }
            return;
        }
        if let Some(done) = event.take::<ScanDone>() {
            // Ignore scan result if user already switched to manual entry
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
            request_quit(0);
            return InputResult::Handled;
        }

        match &self.phase {
            BaudPhase::Prompt => {
                // Tab / Right — advance selection
                if ev.chord == chord!(Tab) || ev.chord == chord!(Right) {
                    queue.next();
                    self.tab = (self.tab + 1).min(1);
                    self.update_content();
                    return InputResult::Handled;
                }
                // Left — retreat selection
                if ev.chord == chord!(Left) {
                    queue.next();
                    self.tab = self.tab.saturating_sub(1);
                    self.update_content();
                    return InputResult::Handled;
                }
                // Enter — confirm current tab
                if ev.chord == chord!(Enter) {
                    queue.next();
                    if self.tab == 0 {
                        self.phase = BaudPhase::Scanning { lines: Vec::new() };
                        self.wants_scan = true;
                    } else {
                        self.phase = BaudPhase::Manual { input: String::new() };
                    }
                    self.update_content();
                    self.update_footer();
                    return InputResult::Handled;
                }
                // Letter shortcuts kept as convenience
                if ev.chord == chord!(a) {
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
                // Digit — jump straight to manual entry with that digit pre-filled
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
            BaudPhase::Scanning { .. } => {}
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

/// Background thread: try each candidate baud, score the sample, send progress.
fn scan_baud_thread(port: String, target: WidgetId<BaudPickerScreen>) {
    let mut port_obj = match serialport::new(&port, BAUD_CANDIDATES[0])
        .timeout(READ_TIMEOUT)
        .open()
    {
        Ok(p) => p,
        Err(_) => { tuie::send(target, ScanDone(None)); return; }
    };

    let mut best: Option<u32> = None;
    let mut best_score: f32 = 0.0;

    for &rate in BAUD_CANDIDATES {
        if port_obj.set_baud_rate(rate).is_err() { continue; }
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

/// One completed line in the live scroll, with arrival timestamp.
struct LiveLine {
    text: String,
    ts: Instant,
}

/// Renderer context for the live-scroll `List`.
struct LiveCtx {
    lines: Vec<LiveLine>,
    start_time: Instant,
}

/// Virtualized renderer: one `Text` widget per visible live line.
/// Prefix: dim elapsed timestamp. Body: colour-coded by content keywords.
fn render_live_line(ctx: &mut LiveCtx, index: usize) -> Option<Box<dyn Widget>> {
    let line = ctx.lines.get(index)?;
    let elapsed = line.ts.duration_since(ctx.start_time);
    let secs = elapsed.as_secs();
    let ts = format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        elapsed.subsec_millis()
    );

    let mut s = StyledString::new();
    s.push_span(ts.as_str().dim());
    s.push_str("  ");
    let text_part = line.text.as_str();
    s.push_span(text_part.fg(line_color(text_part)));

    Some(Text::new().content(s) as Box<dyn Widget>)
}

/// Tracks a group of lines that share a common prefix (same "field", varying value).
/// Once `count` reaches `PIN_THRESHOLD` the group is pinned and `latest` updates in place.
struct LineGroup {
    /// Longest common byte-prefix across all lines seen in this group (shrinks as values vary).
    prefix: String,
    /// Most recent full line from this group.
    latest: String,
    count: usize,
    last_seen: Instant,
    pinned: bool,
}

/// Third TUI screen: streams and displays incoming serial data.
///
/// Layout: horizontal Split — live scroll (left, flex 3) | pinned panel (right, flex 1).
/// Title embedded in the live pane's border. Status bar below the Split.
struct RxScreen {
    root: Box<Pane>,
    live_list_id: WidgetId<List>,
    pinned_id: WidgetId<Text>,
    status_id: WidgetId<Text>,
    groups: Vec<LineGroup>,
    partial: Vec<u8>,
    total_bytes: u64,
    start_time: Instant,
    live_count: usize,
}

impl RxScreen {
    fn new(port: &str, baud: u32) -> Box<Self> {
        let mut live_list_id = WidgetId::EMPTY;
        let mut pinned_id = WidgetId::EMPTY;
        let mut status_id = WidgetId::EMPTY;

        let start_time = Instant::now();
        let mut list = List::new();
        list.set_renderer(LiveCtx { lines: Vec::new(), start_time }, render_live_line);

        let pinned = Text::new().word_wrap().content("").id(&mut pinned_id);

        let live_title = format!(" live — {port} @ {baud} baud ");
        let split = Split::new(
            SplitPane::new().horizontal().children([
                SplitPaneChild::from(list.id(&mut live_list_id).flex(3))
                    .title(live_title),
                SplitPaneChild::from(pinned.flex(1))
                    .title(" pinned "),
            ]),
        );

        let status = Text::new().content("").id(&mut status_id);

        let root = Pane::new()
            .child(split.flex(1))
            .child(status);

        let mut screen = Box::new(Self {
            root, live_list_id, pinned_id, status_id,
            groups: Vec::new(),
            partial: Vec::new(),
            total_bytes: 0,
            start_time,
            live_count: 0,
        });
        screen.update_pinned();
        screen.update_status();
        screen
    }

    /// Feed a raw chunk: extract lines, group by LCP, pin recurring groups, refresh panels.
    fn feed_bytes(&mut self, bytes: Vec<u8>) {
        self.total_bytes += bytes.len() as u64;
        self.partial.extend_from_slice(&bytes);

        let now = Instant::now();
        let mut new_live: Vec<LiveLine> = Vec::new();
        let mut newly_pinned_prefixes: Vec<String> = Vec::new();
        let mut pins_changed = false;

        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&raw)
                .trim_end_matches(['\n', '\r'])
                .to_string();
            if text.is_empty() {
                continue;
            }

            // ANSI lines bypass grouping entirely — escape sequences make every
            // frame unique and render as garbage if pinned.
            if text.contains('\x1b') {
                new_live.push(LiveLine { text, ts: now });
                continue;
            }

            // Find the group whose prefix shares the longest common prefix with
            // this line, requiring at least LCP_THRESHOLD bytes to match.
            let best = self.groups
                .iter()
                .enumerate()
                .filter_map(|(i, g)| {
                    let lcp = common_prefix_len(&g.prefix, &text);
                    if lcp >= LCP_THRESHOLD { Some((i, lcp)) } else { None }
                })
                .max_by_key(|&(_, lcp)| lcp)
                .map(|(i, _)| i);

            if let Some(gi) = best {
                let g = &mut self.groups[gi];
                let lcp = common_prefix_len(&g.prefix, &text);
                g.prefix.truncate(lcp); // shrink prefix to actual shared region
                g.latest = text.clone();
                g.count += 1;
                g.last_seen = now;

                if g.pinned {
                    pins_changed = true; // update display; suppress from live
                } else if g.count >= PIN_THRESHOLD {
                    g.pinned = true;
                    newly_pinned_prefixes.push(g.prefix.clone());
                    pins_changed = true;
                } else {
                    new_live.push(LiveLine { text, ts: now });
                }
            } else {
                self.groups.push(LineGroup {
                    prefix: text.clone(),
                    latest: text.clone(),
                    count: 1,
                    last_seen: now,
                    pinned: false,
                });
                new_live.push(LiveLine { text, ts: now });
            }
        }

        let live_count = {
            let Some(list) = self.root.get_widget_mut(self.live_list_id) else {
                return;
            };
            let old_count = self.live_count;
            let was_at_bottom = old_count == 0 || list.get_visible_range().end >= old_count;

            let ctx = list.get_context_mut::<LiveCtx>().expect("live ctx");
            // Purge pre-threshold occurrences of newly pinned groups from live.
            for prefix in &newly_pinned_prefixes {
                ctx.lines.retain(|l| common_prefix_len(&l.text, prefix) < LCP_THRESHOLD);
            }
            ctx.lines.extend(new_live);
            let n = ctx.lines.len();
            list.set_item_count(n);
            if was_at_bottom && n > 0 {
                list.ensure_visible(n - 1);
            }
            if !newly_pinned_prefixes.is_empty() {
                list.invalidate_all();
            }
            n
        };
        self.live_count = live_count;

        if pins_changed {
            self.update_pinned();
        }
        self.update_status();
    }

    /// Rebuild the pinned panel Text from all pinned groups (showing latest value each).
    fn update_pinned(&mut self) {
        let pinned: Vec<(&str, usize)> = self.groups
            .iter()
            .filter(|g| g.pinned)
            .map(|g| (g.latest.as_str(), g.count))
            .collect();

        let content: StyledString = if pinned.is_empty() {
            let mut s = StyledString::new();
            s.push_span("\n  no pins yet\n\n  lines seen ≥3×\n  appear here".dim());
            s
        } else {
            let mut s = StyledString::new();
            let show = pinned.len().min(MAX_PINS_DISPLAY);
            for &(text, count) in &pinned[..show] {
                s.push_span(format!(" ×{count:<3}  ").as_str().cyan().bold());
                s.push_span(format!("{text}\n").as_str().dim());
            }
            if pinned.len() > MAX_PINS_DISPLAY {
                s.push_span(format!("  … {} more\n", pinned.len() - MAX_PINS_DISPLAY).as_str().dim());
            }
            s
        };
        if let Some(w) = self.root.get_widget_mut(self.pinned_id) {
            w.set_content(content);
        }
    }

    /// Rebuild the status bar with live count, byte total, pinned count, and uptime.
    fn update_status(&mut self) {
        let live_str    = format!("{}", self.live_count);
        let bytes_str   = format_bytes(self.total_bytes);
        let pinned_str  = format!("{}", self.groups.iter().filter(|g| g.pinned).count());
        let elapsed_str = format_duration(self.start_time.elapsed().as_secs());

        let mut s = StyledString::new();
        s.push_span("  ".dim());
        s.push_span(live_str.as_str().cyan().bold());
        s.push_span(" live".dim());
        s.push_span("   ".dim());
        s.push_span(bytes_str.as_str().cyan());
        s.push_span("   ".dim());
        s.push_span(pinned_str.as_str().cyan());
        s.push_span(" pinned".dim());
        s.push_span("   ".dim());
        s.push_span(elapsed_str.as_str().dim());
        s.push_span("   ↑↓ scroll   q quit".dim());

        if let Some(w) = self.root.get_widget_mut(self.status_id) {
            w.set_content(s);
        }
    }
}

impl DelegateWidget for RxScreen {
    tuie::delegate_widget!(root);

    fn override_on_event(&mut self, event: &mut WidgetEvent) {
        if let Some(bytes) = event.take::<RxBytes>() {
            self.feed_bytes(bytes.0);
        } else {
            self.get_delegate_mut().on_event(event);
        }
    }

    fn override_on_input(&mut self, queue: &mut InputQueue) -> InputResult {
        if let Some(ev) = queue.peek() {
            if ev.chord == chord!(q) || ev.chord == chord!(Ctrl + c) {
                queue.next();
                request_quit(0);
                return InputResult::Handled;
            }
        }
        self.get_delegate_mut().on_input(queue)
    }
}

// ---------- App — screen manager ----------

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

/// Root widget: drives the port-pick → baud-pick → RX flow.
struct App {
    screen: AppScreen,
    baud_screen_id: WidgetId<BaudPickerScreen>,
}

impl App {
    fn new(ports: Vec<PortEntry>) -> Box<Self> {
        Box::new(Self {
            screen: AppScreen::PortPicking(PortPickerScreen::new(ports)),
            baud_screen_id: WidgetId::EMPTY,
        })
    }

    /// Check every screen's pending-transition flags; act on the first one found.
    fn check_transitions(&mut self) {
        let to_baud: Option<String> =
            if let AppScreen::PortPicking(w) = &mut self.screen {
                w.selected_port.take()
            } else { None };
        if let Some(port) = to_baud {
            self.transition_to_baud(port);
            return;
        }

        let scan_for: Option<String> =
            if let AppScreen::BaudPicking(w) = &mut self.screen {
                if w.wants_scan { w.wants_scan = false; Some(w.port.clone()) } else { None }
            } else { None };
        if let Some(port) = scan_for {
            let id = self.baud_screen_id;
            thread::spawn(move || scan_baud_thread(port, id));
        }

        let to_rx: Option<(String, u32)> =
            if let AppScreen::BaudPicking(w) = &mut self.screen {
                w.resolved.take().map(|baud| (w.port.clone(), baud))
            } else { None };
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
                request_quit(1);
                return;
            }
        };

        let mut rx_id: WidgetId<RxScreen> = WidgetId::EMPTY;
        let rx_screen = RxScreen::new(&port, baud).id(&mut rx_id);
        self.screen = AppScreen::Rx(rx_screen);
        tuie::dirty_layout();

        let Connection { rx, reader: _ } = conn;
        thread::spawn(move || {
            for chunk in rx {
                tuie::send(rx_id, RxBytes(chunk));
            }
        });
    }
}

impl DelegateWidget for App {
    fn get_delegate(&self) -> &dyn Widget { self.screen.as_widget() }
    fn get_delegate_mut(&mut self) -> &mut dyn Widget { self.screen.as_widget_mut() }

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

fn run_tui(ports: Vec<PortEntry>) -> io::Result<ExitCode> {
    tuie::start_tui(App::new(ports))
}

// ---------- Display helpers ----------

/// Build a dim `StyledString` for footer / status hints.
fn styled_footer_str(text: &str) -> StyledString {
    let mut s = StyledString::new();
    s.push_span(text.dim());
    s
}

/// Build a dim `Text` widget for a static footer hint.
fn styled_footer(text: &str) -> Box<Text> {
    Text::new().content(styled_footer_str(text))
}

/// 20-char `█░` progress bar; returns (filled_chars, empty_chars) for independent styling.
fn score_bar_parts(score: f32) -> (String, String) {
    let filled = (score.clamp(0.0, 1.0) * 20.0).round() as usize;
    let empty = 20usize.saturating_sub(filled);
    ("█".repeat(filled), "░".repeat(empty))
}

/// Colour for a live serial line based on keyword scanning.
fn line_color(text: &str) -> Color {
    let lower = text.to_lowercase();
    if lower.contains("error") || lower.contains("err:") || lower.contains("fatal") {
        Color::RED
    } else if lower.contains("warn") {
        Color::YELLOW
    } else if lower.contains("ok") || lower.contains("pass") || lower.contains("success") || lower.contains("done") {
        Color::GREEN
    } else {
        Color::Foreground
    }
}

/// Byte-level common prefix length between two strings.
/// Using bytes (not chars) is correct for ASCII MCU output and avoids UTF-8 splits.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

fn format_bytes(n: u64) -> String {
    if n < 1024 { format!("{n} B") }
    else if n < 1_048_576 { format!("{:.1} KB", n as f64 / 1024.0) }
    else { format!("{:.1} MB", n as f64 / 1_048_576.0) }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m{}s", secs / 60, secs % 60) }
    else { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
}

// ---------- Baud detection helpers ----------

fn read_window<R: io::Read + ?Sized>(port: &mut R, window: Duration) -> Vec<u8> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    while Instant::now() < deadline && out.len() < DETECT_MAX_BYTES {
        if QUITTING.load(Ordering::Relaxed) {
            break;
        }
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
    }
    out
}

fn score_ascii(bytes: &[u8]) -> f32 {
    if bytes.len() < DETECT_MIN_BYTES { return 0.0; }
    let textlike = bytes.iter().filter(|&&b| is_textlike(b)).count();
    let ratio = textlike as f32 / bytes.len() as f32;
    let has_line = bytes.iter().any(|&b| b == b'\n' || b == b'\r');
    if has_line { ratio } else { ratio * 0.5 }
}

fn is_textlike(b: u8) -> bool {
    matches!(b, 0x20..=0x7E | b'\t' | b'\n' | b'\r')
}

// ---------- Port metadata helpers ----------

/// Returns `true` for phantom legacy UART slots (Linux `ttyS*` with `type == 0`).
fn is_phantom(port: &SerialPortInfo) -> bool {
    if !matches!(port.port_type, SerialPortType::Unknown) { return false; }
    let Some(name) = port.port_name.strip_prefix("/dev/") else { return false; };
    if !name.starts_with("ttyS") { return false; }
    match std::fs::read_to_string(format!("/sys/class/tty/{name}/type")) {
        Ok(contents) => contents.trim() == "0",
        Err(_) => false,
    }
}

/// Return the `PortKind` and a human-readable description for a port type.
fn classify_port(port_type: &SerialPortType) -> (PortKind, String) {
    match port_type {
        SerialPortType::UsbPort(info) => (PortKind::Usb, describe_usb(info)),
        SerialPortType::BluetoothPort => (PortKind::Bluetooth, "Bluetooth".to_string()),
        SerialPortType::PciPort       => (PortKind::Pci, "PCI".to_string()),
        SerialPortType::Unknown       => (PortKind::Unknown, "Unknown".to_string()),
    }
}

fn describe_usb(info: &UsbPortInfo) -> String {
    let manufacturer = info.manufacturer.as_deref().unwrap_or("unknown vendor");
    let product      = info.product.as_deref().unwrap_or("unknown product");
    format!(
        "{manufacturer} — {product}  (VID:PID {:04x}:{:04x})",
        info.vid, info.pid
    )
}
