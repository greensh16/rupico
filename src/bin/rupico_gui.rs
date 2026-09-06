// On Windows, a console-subsystem binary opens a console window alongside the
// GUI. Detach it for real builds, but keep it in debug builds so panics and
// backtraces remain visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop UI for `rupico`.
//!
//! The layout is IDE-shaped: a slim toolbar carries the connection and run
//! controls, the device filesystem sits in a left rail, editor tabs fill the
//! centre, and program output docks along the bottom.
//!
//! Device I/O still runs on the UI thread, so a long transfer briefly blocks
//! the window; the CLI remains the right tool for bulk work.

use eframe::egui;
use egui::text::LayoutJob;
use rupico::micropython::{
    ExecResult, MicroPythonDevice, MicroPythonError, Result as MpResult, join_remote_path,
    vid_looks_micropython,
};
use rupico::sync;
use rupico::update;
use serialport::available_ports;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Semantic colours resolved for the active light/dark theme.
///
/// Kept in one place so the editor, the tree and the output dock cannot drift
/// apart, and so the whole palette flips with the system theme.
#[derive(Clone, Copy)]
struct Palette {
    accent: egui::Color32,
    ok: egui::Color32,
    warn: egui::Color32,
    err: egui::Color32,
    dim: egui::Color32,
    rail: egui::Color32,
    editor_bg: egui::Color32,
    /// Hairline between panels. Without it the rails bleed into the editor.
    divider: egui::Color32,
    gutter: egui::Color32,
    // Syntax
    keyword: egui::Color32,
    string: egui::Color32,
    comment: egui::Color32,
    number: egui::Color32,
    decorator: egui::Color32,
    ident: egui::Color32,
}

impl Palette {
    fn for_theme(dark: bool) -> Self {
        if dark {
            Self {
                accent: egui::Color32::from_rgb(0x6E, 0xA8, 0xFE),
                ok: egui::Color32::from_rgb(0x5B, 0xC8, 0x8A),
                warn: egui::Color32::from_rgb(0xE0, 0xB1, 0x54),
                err: egui::Color32::from_rgb(0xEB, 0x6F, 0x6F),
                dim: egui::Color32::from_rgb(0x8A, 0x91, 0x9E),
                rail: egui::Color32::from_rgb(0x1E, 0x22, 0x29),
                editor_bg: egui::Color32::from_rgb(0x14, 0x17, 0x1C),
                divider: egui::Color32::from_rgb(0x2E, 0x34, 0x3E),
                gutter: egui::Color32::from_rgb(0x4A, 0x51, 0x5C),
                keyword: egui::Color32::from_rgb(0xC5, 0x8A, 0xF0),
                string: egui::Color32::from_rgb(0x8F, 0xD0, 0x84),
                comment: egui::Color32::from_rgb(0x6B, 0x73, 0x80),
                number: egui::Color32::from_rgb(0xE5, 0xA5, 0x6B),
                decorator: egui::Color32::from_rgb(0x5F, 0xC2, 0xC8),
                ident: egui::Color32::from_rgb(0xD5, 0xDA, 0xE2),
            }
        } else {
            Self {
                accent: egui::Color32::from_rgb(0x1B, 0x63, 0xC8),
                ok: egui::Color32::from_rgb(0x1E, 0x7D, 0x4F),
                warn: egui::Color32::from_rgb(0x9A, 0x6B, 0x0A),
                err: egui::Color32::from_rgb(0xC0, 0x35, 0x2B),
                dim: egui::Color32::from_rgb(0x6A, 0x71, 0x7C),
                rail: egui::Color32::from_rgb(0xE6, 0xE9, 0xEF),
                editor_bg: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
                divider: egui::Color32::from_rgb(0xC6, 0xCC, 0xD6),
                gutter: egui::Color32::from_rgb(0xA8, 0xAF, 0xBA),
                keyword: egui::Color32::from_rgb(0x8B, 0x2D, 0xB8),
                string: egui::Color32::from_rgb(0x1E, 0x6B, 0x33),
                comment: egui::Color32::from_rgb(0x8A, 0x91, 0x9E),
                number: egui::Color32::from_rgb(0xA8, 0x55, 0x10),
                decorator: egui::Color32::from_rgb(0x0F, 0x6E, 0x74),
                ident: egui::Color32::from_rgb(0x24, 0x29, 0x31),
            }
        }
    }
}

/// Font size used everywhere code is shown, so the editor gutter lines up with
/// the text beside it.
const CODE_SIZE: f32 = 13.0;

fn code_font() -> egui::FontId {
    egui::FontId::monospace(CODE_SIZE)
}

/// Apply spacing and rounding once per frame.
///
/// egui's defaults are cramped; widening the item spacing and softening the
/// corners is most of what stops the window looking like a debug overlay.
fn apply_style(ctx: &egui::Context) {
    // egui keeps a separate style per theme, so mutate both rather than only
    // whichever one happens to be active right now.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(9.0, 5.0);
        style.spacing.indent = 16.0;
        for w in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
        ] {
            w.corner_radius = egui::CornerRadius::same(5);
        }
    });
}

/// Symbols used in button labels.
///
/// egui's default font covers far less than you would guess: `→`, `⇄`, `⌫`,
/// `⌄` and `⌃` are all absent and render as empty boxes. These constants are
/// the single source of truth so `every_ui_symbol_has_a_glyph` can check the
/// whole set, rather than the boxes being spotted by eye after the fact.
mod sym {
    pub const RUN: &str = "▶  Run";
    pub const STOP: &str = "⏹  Stop";
    pub const FLASH: &str = "⚡  Flash";
    pub const REBOOT: &str = "⟲  Reboot";
    pub const SYNC: &str = "Sync";
    pub const CONNECTED_DOT: &str = "⏺";
    pub const DISCONNECTED_DOT: &str = "⏹";
    pub const REFRESH: &str = "⟳";
    pub const ADD: &str = "+";
    pub const CLOSE: &str = "×";
    pub const DIRTY: &str = "●";
    pub const WARN: &str = "⚠";
}

// ---------------------------------------------------------------------------
// Python syntax highlighting
// ---------------------------------------------------------------------------

/// Python keywords worth colouring. Deliberately a plain list rather than a
/// dependency: highlighting a MicroPython script needs nothing more, and a
/// syntect-based highlighter would dwarf the rest of the binary.
const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Build a coloured layout for one chunk of Python source.
///
/// A single forward pass over the characters — no regex, no parser. It handles
/// the cases that actually change readability: comments, string literals
/// (including triple-quoted and escapes), numbers, decorators and keywords.
fn highlight_python(text: &str, pal: &Palette) -> LayoutJob {
    let mut job = LayoutJob::default();
    // Code scrolls horizontally rather than wrapping mid-statement.
    job.wrap.max_width = f32::INFINITY;

    let fmt = |color: egui::Color32| egui::TextFormat {
        font_id: code_font(),
        color,
        ..Default::default()
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let push = |job: &mut LayoutJob, range: &[char], color: egui::Color32| {
        job.append(&range.iter().collect::<String>(), 0.0, fmt(color));
    };

    while i < chars.len() {
        let c = chars[i];

        // Comment: runs to end of line.
        if c == '#' {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            push(&mut job, &chars[start..i], pal.comment);
            continue;
        }

        // String literal, single or triple quoted.
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            let triple = i + 2 < chars.len() && chars[i + 1] == quote && chars[i + 2] == quote;
            if triple {
                i += 3;
                while i < chars.len() {
                    if chars[i] == quote
                        && i + 2 < chars.len()
                        && chars[i + 1] == quote
                        && chars[i + 2] == quote
                    {
                        i += 3;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 1;
                while i < chars.len() {
                    // A backslash escapes the next character, so an escaped
                    // quote does not end the literal.
                    if chars[i] == '\\' {
                        i = (i + 2).min(chars.len());
                        continue;
                    }
                    if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    // An unterminated single-quoted string ends at the newline
                    // rather than swallowing the rest of the file.
                    if chars[i] == '\n' {
                        break;
                    }
                    i += 1;
                }
            }
            push(&mut job, &chars[start..i], pal.string);
            continue;
        }

        // Decorator.
        if c == '@' {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            push(&mut job, &chars[start..i], pal.decorator);
            continue;
        }

        // Number.
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            push(&mut job, &chars[start..i], pal.number);
            continue;
        }

        // Word: keyword or identifier.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let color = if PY_KEYWORDS.contains(&word.as_str()) {
                pal.keyword
            } else {
                pal.ident
            };
            job.append(&word, 0.0, fmt(color));
            continue;
        }

        // Everything else: operators, punctuation, whitespace.
        let start = i;
        i += 1;
        push(&mut job, &chars[start..i], pal.ident);
    }

    job
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One open buffer.
struct EditorTab {
    /// Remote path this buffer is bound to, if it came from (or was saved to)
    /// the device.
    path: Option<String>,
    text: String,
    dirty: bool,
}

/// Seed for the first-run buffer.
///
/// An empty editor on launch gives no hint of what the tool is for; a tiny
/// runnable example does, and it makes the syntax colours visible before the
/// user has connected anything.
const STARTER_SNIPPET: &str = r#"# Connect a board above, then press Run (Cmd-R).
from machine import Pin
import time

led = Pin("LED", Pin.OUT)

for _ in range(10):
    led.toggle()
    time.sleep(0.5)

print("done")
"#;

impl EditorTab {
    fn untitled() -> Self {
        Self {
            path: None,
            text: String::new(),
            dirty: false,
        }
    }

    /// The buffer shown on first launch.
    fn starter() -> Self {
        Self {
            path: None,
            text: STARTER_SNIPPET.to_string(),
            dirty: false,
        }
    }

    fn from_remote(path: String, text: String) -> Self {
        Self {
            path: Some(path),
            text,
            dirty: false,
        }
    }

    /// Short name for the tab strip.
    fn title(&self) -> String {
        match &self.path {
            Some(p) => p.rsplit('/').next().unwrap_or(p).to_string(),
            None => "untitled".to_string(),
        }
    }
}

/// A node in the cached view of the device filesystem.
struct RemoteNode {
    name: String,
    path: String,
    is_dir: bool,
    children: Vec<RemoteNode>,
}

/// Which stream the output dock is showing.
#[derive(PartialEq, Eq, Clone, Copy)]
enum OutputFilter {
    All,
    Stdout,
    Stderr,
}

/// Something the tree asked for this frame.
///
/// Collected rather than acted on inline, because the recursive render only
/// has `&RemoteNode` and cannot also hold `&mut GuiApp`.
enum TreeAction {
    Select(String, bool),
    Open(String),
    StartRename(String),
    Delete(String, bool),
    NewFileIn(String),
}

/// State of the sync panel.
///
/// Sync is an occasional, deliberate action rather than something done every
/// few seconds, so it lives in its own window instead of taking permanent
/// space in the main layout.
struct SyncPanel {
    open: bool,
    /// Host folder to sync. Remembered between launches.
    local_dir: Option<PathBuf>,
    /// Device folder to mirror it onto.
    remote_dir: String,
    /// Delete entries on the destination that are absent from the source.
    delete: bool,
    /// Download instead of upload.
    from_device: bool,
    /// Result of the last run, kept on screen so a preview can be read before
    /// committing to it.
    last: Option<sync::SyncOutcome>,
    /// Whether `last` came from a dry run.
    last_was_preview: bool,
}

impl Default for SyncPanel {
    fn default() -> Self {
        Self {
            open: false,
            local_dir: None,
            remote_dir: "/".to_string(),
            delete: false,
            from_device: false,
            last: None,
            last_was_preview: false,
        }
    }
}

/// Settings persisted between launches.
///
/// eframe's own persistence is behind a feature that pulls in extra
/// dependencies; a single small JSON file does the same job here.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Prefs {
    #[serde(default)]
    sync_local_dir: Option<PathBuf>,
    #[serde(default)]
    sync_remote_dir: Option<String>,
    #[serde(default)]
    last_port: Option<String>,
}

impl Prefs {
    fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/rupico/gui.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        // Best effort: failing to remember a folder is not worth an error.
        if let Some(path) = Self::path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(text) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(path, text);
            }
        }
    }
}

/// Result of a background update job.
enum UpdateMsg {
    Checked(Result<Option<update::Check>, String>),
    Installed(Result<String, String>),
}

/// State of the update dialog.
///
/// Unlike device I/O, the update check runs on a worker thread: a network call
/// on the UI thread would freeze the window for up to the HTTP timeout.
#[derive(Default)]
struct UpdatePanel {
    open: bool,
    busy: bool,
    rx: Option<std::sync::mpsc::Receiver<UpdateMsg>>,
    /// Message and whether it is an error, for colouring.
    status: Option<(String, bool)>,
    /// Set once a check has found something newer.
    available: Option<update::Release>,
}

struct GuiApp {
    // Connection
    available_ports: Vec<PortEntry>,
    selected_port: Option<String>,
    device: Option<MicroPythonDevice>,
    connection_error: Option<String>,

    // Device filesystem
    remote_tree: Vec<RemoteNode>,
    selected_remote_path: Option<String>,
    selected_remote_is_dir: bool,

    // Editor
    tabs: Vec<EditorTab>,
    active_tab: usize,

    // Output dock
    last_output: Option<ExecResult>,
    output_open: bool,
    output_filter: OutputFilter,

    // Transient interaction state
    last_status: Option<String>,
    /// Inline rename in the tree: (original path, edited leaf name).
    renaming: Option<(String, String)>,
    /// Inline "new file" in the tree: (parent directory, edited name).
    creating: Option<(String, String)>,
    confirm_delete: Option<(String, bool)>,
    sync_panel: SyncPanel,
    update_panel: UpdatePanel,
}

impl Default for GuiApp {
    fn default() -> Self {
        let prefs = Prefs::load();
        let available_ports = list_ports();
        // A port the user chose last time wins, as long as it is still here.
        let selected_port = prefs
            .last_port
            .filter(|p| available_ports.iter().any(|e| &e.name == p))
            .or_else(|| default_port(&available_ports));
        let sync_panel = SyncPanel {
            local_dir: prefs.sync_local_dir,
            remote_dir: prefs.sync_remote_dir.unwrap_or_else(|| "/".to_string()),
            ..SyncPanel::default()
        };
        Self {
            available_ports,
            selected_port,
            device: None,
            connection_error: None,
            remote_tree: Vec::new(),
            selected_remote_path: None,
            selected_remote_is_dir: false,
            tabs: vec![EditorTab::starter()],
            active_tab: 0,
            last_output: None,
            output_open: false,
            output_filter: OutputFilter::All,
            last_status: None,
            renaming: None,
            creating: None,
            confirm_delete: None,
            sync_panel,
            update_panel: UpdatePanel::default(),
        }
    }
}

/// A serial port as offered in the picker.
#[derive(Clone)]
struct PortEntry {
    name: String,
    /// Looks like a MicroPython board by USB vendor ID. Passive: nothing is
    /// ever written to a port to decide this.
    is_board: bool,
}

/// Enumerate serial ports, boards first.
///
/// A typical Mac lists Bluetooth, debug-console and headset ports alongside
/// the board, so an unsorted list buries the one port the user wants.
fn list_ports() -> Vec<PortEntry> {
    let mut ports: Vec<PortEntry> = available_ports()
        .map(|list| {
            list.into_iter()
                .map(|p| PortEntry {
                    is_board: vid_looks_micropython(&p.port_type),
                    name: p.port_name,
                })
                .collect()
        })
        .unwrap_or_default();
    ports.sort_by(|a, b| {
        b.is_board
            .cmp(&a.is_board)
            .then_with(|| a.name.cmp(&b.name))
    });
    ports
}

/// Pick the port to start on: the first that looks like a board, or the only
/// port if there is exactly one. Otherwise leave it unset rather than guess.
///
/// macOS exposes every USB serial device twice, as `/dev/cu.*` and
/// `/dev/tty.*`. The `cu` ("callout") node is the one to open — `tty` blocks
/// waiting for carrier detect — so prefer it explicitly rather than relying on
/// the two sorting into a lucky order.
fn default_port(ports: &[PortEntry]) -> Option<String> {
    let boards = || ports.iter().filter(|p| p.is_board);
    boards()
        .find(|p| is_callout_node(&p.name))
        .or_else(|| boards().next())
        .or_else(|| {
            if ports.len() == 1 {
                ports.first()
            } else {
                None
            }
        })
        .map(|p| p.name.clone())
}

/// Whether this is the macOS "callout" node for a device.
fn is_callout_node(name: &str) -> bool {
    name.rsplit('/')
        .next()
        .is_some_and(|leaf| leaf.starts_with("cu."))
}

/// Trim the noisy `/dev/` prefix for display.
fn short_port(port: &str) -> String {
    port.rsplit('/').next().unwrap_or(port).to_string()
}

/// The directory containing `path`, as a remote path.
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

// ---------------------------------------------------------------------------
// Device operations
// ---------------------------------------------------------------------------

impl GuiApp {
    /// Report a failed device operation and put the connection back into a
    /// known state.
    ///
    /// A raw-REPL failure usually means the protocol desynced partway through
    /// a frame. Leaving the handle open would make every later operation fail
    /// in confusing ways against a connection that still looks healthy, so we
    /// re-interrupt and re-enter raw REPL. If even that fails the handle is
    /// dropped, so the user gets an honest "disconnected" instead of a dead
    /// connection that still shows as connected.
    fn fail_device_op(&mut self, what: &str, e: MicroPythonError) {
        self.connection_error = Some(format!("{what}: {e}"));

        let recovered = matches!(self.device.as_mut().map(|d| d.recover()), Some(Ok(())));
        if recovered {
            self.last_status = Some(format!("{what} (connection resynchronised)"));
        } else {
            self.device = None;
            self.last_status = Some(format!("{what} (disconnected)"));
        }
    }

    fn ensure_connected(&mut self) {
        if self.device.is_some() {
            return;
        }

        let port = match self.selected_port.clone() {
            Some(p) => p,
            None => {
                self.connection_error = Some("No port selected".to_string());
                return;
            }
        };

        match MicroPythonDevice::connect(&port) {
            Ok(mut dev) => {
                if let Err(e) = dev.enter_raw_repl() {
                    self.connection_error = Some(format!("Failed to enter raw REPL: {e}"));
                    self.last_status = Some("Failed to enter raw REPL".to_string());
                    return;
                }
                self.connection_error = None;
                self.last_status = Some(format!("Connected to {}", short_port(&port)));
                self.device = Some(dev);
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to connect: {e}"));
                self.last_status = Some("Failed to connect".to_string());
            }
        }
    }

    fn connect_and_list(&mut self) {
        self.ensure_connected();
        if self.device.is_some() {
            self.refresh_remote_tree();
        }
    }

    fn disconnect(&mut self) {
        if let Some(mut dev) = self.device.take() {
            let _ = dev.exit_raw_repl();
        }
        self.remote_tree.clear();
        self.last_status = Some("Disconnected".to_string());
    }

    fn stop_program(&mut self) {
        self.ensure_connected();
        let stopped = match self.device.as_mut() {
            Some(d) => d.stop_current_program().and_then(|()| d.enter_raw_repl()),
            None => return,
        };

        if let Err(e) = stopped {
            self.fail_device_op("Failed to stop program", e);
            return;
        }
        self.last_status = Some("Program stopped".to_string());
    }

    fn flash_active_as_main(&mut self) {
        self.ensure_connected();
        let text = self.tabs[self.active_tab].text.clone();
        let flashed = match self.device.as_mut() {
            Some(d) => d.flash_main_script(&text),
            None => return,
        };

        if let Err(e) = flashed {
            self.fail_device_op("Failed to flash main.py", e);
            return;
        }
        self.last_status = Some("Flashed active tab as main.py".to_string());
        self.refresh_remote_tree();
    }

    fn run_main_script(&mut self) {
        // The soft reboot leaves the board outside raw REPL, so the handle is
        // dropped deliberately rather than kept in a state we cannot use.
        self.ensure_connected();
        let mut dev = match self.device.take() {
            Some(d) => d,
            None => {
                self.last_status = Some("No device connected".to_string());
                return;
            }
        };

        match dev.run_main() {
            Ok(()) => {
                self.last_status = Some("Soft reboot triggered".to_string());
                self.set_output(ExecResult {
                    stdout: "Soft reboot triggered; boot.py / main.py should run on the device.\n\
                             Reconnect to regain the raw REPL.\n"
                        .to_string(),
                    stderr: String::new(),
                });
                self.remote_tree.clear();
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to run main.py: {e}"));
                self.last_status = Some("Failed to run main.py".to_string());
            }
        }
    }

    fn run_current_script(&mut self) {
        self.ensure_connected();

        let path = self.tabs[self.active_tab].path.clone();
        let text = self.tabs[self.active_tab].text.clone();
        let dirty = self.tabs[self.active_tab].dirty;

        // Each device call is scoped so the borrow ends before any error
        // handling, which needs `&mut self` to resynchronise the connection.
        if let (Some(remote), true) = (path.clone(), dirty) {
            let saved = match self.device.as_mut() {
                Some(d) => d.write_text_file(&remote, &text),
                None => return,
            };
            if let Err(e) = saved {
                self.fail_device_op("Failed to save before run", e);
                return;
            }
            self.tabs[self.active_tab].dirty = false;
        }

        let result = match self.device.as_mut() {
            Some(d) => match &path {
                Some(remote) => d.run_file(remote),
                None => d.run_snippet(&text),
            },
            None => return,
        };

        match result {
            Ok(res) => {
                self.last_status = Some(if res.stderr.trim().is_empty() {
                    "Run finished".to_string()
                } else {
                    "Run raised an exception".to_string()
                });
                self.set_output(res);
            }
            Err(e) => self.fail_device_op("Execution error", e),
        }
    }

    /// Show a result in the output dock, opening it if it was collapsed.
    fn set_output(&mut self, res: ExecResult) {
        self.last_output = Some(res);
        self.output_open = true;
    }

    fn refresh_remote_tree(&mut self) {
        self.ensure_connected();
        let result = match self.device.as_mut() {
            Some(d) => build_remote_tree(d, "/", 0, 4),
            None => return,
        };

        match result {
            Ok(nodes) => {
                self.remote_tree = nodes;
                self.last_status = Some("Refreshed device files".to_string());
            }
            Err(e) => {
                self.remote_tree.clear();
                self.fail_device_op("Failed to list device files", e);
            }
        }
    }

    fn open_path(&mut self, path: String) {
        self.ensure_connected();
        let result = match self.device.as_mut() {
            Some(d) => d.read_text_file(&path),
            None => return,
        };

        match result {
            Ok(text) => {
                // Focus an existing tab for this path rather than duplicating.
                if let Some(idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.path.as_deref() == Some(path.as_str()))
                {
                    self.tabs[idx].text = text;
                    self.tabs[idx].dirty = false;
                    self.active_tab = idx;
                } else {
                    // Reuse a pristine untitled tab instead of stacking up
                    // empty buffers.
                    let only_pristine_scratch = self.tabs.len() == 1
                        && self.tabs[0].path.is_none()
                        && !self.tabs[0].dirty
                        && (self.tabs[0].text.is_empty() || self.tabs[0].text == STARTER_SNIPPET);
                    if only_pristine_scratch {
                        self.tabs.clear();
                    }
                    self.tabs.push(EditorTab::from_remote(path.clone(), text));
                    self.active_tab = self.tabs.len() - 1;
                }
                self.connection_error = None;
                self.last_status = Some(format!("Opened {path}"));
            }
            Err(e) => self.fail_device_op("Failed to open file", e),
        }
    }

    fn save_current(&mut self) {
        self.ensure_connected();

        let path = match self.tabs[self.active_tab].path.clone() {
            Some(p) => p,
            None => {
                // An untitled buffer has nowhere to go yet; point at the tree
                // rather than inventing a path.
                self.connection_error = Some(
                    "This buffer has no device path yet — create the file from the tree first"
                        .to_string(),
                );
                return;
            }
        };

        let text = self.tabs[self.active_tab].text.clone();
        let result = match self.device.as_mut() {
            Some(d) => d.write_text_file(&path, &text),
            None => return,
        };

        match result {
            Ok(()) => {
                self.tabs[self.active_tab].dirty = false;
                self.connection_error = None;
                self.last_status = Some(format!("Saved {path}"));
            }
            Err(e) => self.fail_device_op("Failed to save file", e),
        }
    }

    fn create_file(&mut self, path: String) {
        self.ensure_connected();
        let result = match self.device.as_mut() {
            Some(d) => d.write_text_file(&path, ""),
            None => return,
        };

        match result {
            Ok(()) => {
                self.connection_error = None;
                self.last_status = Some(format!("Created {path}"));
                self.refresh_remote_tree();
                self.tabs.push(EditorTab::from_remote(path, String::new()));
                self.active_tab = self.tabs.len() - 1;
            }
            Err(e) => self.fail_device_op("Failed to create file", e),
        }
    }

    fn delete_path(&mut self, path: &str, is_dir: bool) {
        self.ensure_connected();
        let res = match self.device.as_mut() {
            Some(d) => {
                if is_dir {
                    d.rmdir(path)
                } else {
                    d.remove(path)
                }
            }
            None => return,
        };

        match res {
            Ok(()) => {
                self.last_status = Some(format!("Deleted {path}"));
                // Keep the buffer but unbind it: the file is gone, and its
                // contents may be the only surviving copy.
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(path) {
                        tab.path = None;
                        tab.dirty = true;
                    }
                }
                if self.selected_remote_path.as_deref() == Some(path) {
                    self.selected_remote_path = None;
                }
                self.refresh_remote_tree();
            }
            Err(e) => self.fail_device_op(&format!("Failed to delete {path}"), e),
        }
    }

    fn rename_path(&mut self, old_path: &str, new_path: &str) {
        self.ensure_connected();
        let result = match self.device.as_mut() {
            Some(d) => d.rename(old_path, new_path),
            None => return,
        };

        match result {
            Ok(()) => {
                self.last_status = Some(format!("Renamed to {new_path}"));
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(old_path) {
                        tab.path = Some(new_path.to_string());
                    }
                }
                if self.selected_remote_path.as_deref() == Some(old_path) {
                    self.selected_remote_path = Some(new_path.to_string());
                }
                self.refresh_remote_tree();
            }
            Err(e) => self.fail_device_op(&format!("Failed to rename {old_path}"), e),
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl GuiApp {
    /// Keyboard shortcuts, consumed before any widget can swallow the key.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (save, run, close_tab, toggle_output, new_tab) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::R),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::W),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::J),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::N),
            )
        });

        if save {
            self.save_current();
        }
        if run {
            self.run_current_script();
        }
        if close_tab {
            self.close_tab(self.active_tab);
        }
        if toggle_output {
            self.output_open = !self.output_open;
        }
        if new_tab {
            self.tabs.push(EditorTab::untitled());
            self.active_tab = self.tabs.len() - 1;
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.tabs.push(EditorTab::untitled());
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            // Colour carries the connection state, so it reads without
            // parsing any text.
            let (dot, tint) = if self.device.is_some() {
                (sym::CONNECTED_DOT, pal.ok)
            } else {
                (sym::DISCONNECTED_DOT, pal.dim)
            };
            ui.colored_label(tint, dot);

            let label = self
                .selected_port
                .as_deref()
                .map(short_port)
                .unwrap_or_else(|| "no port".to_string());

            egui::ComboBox::from_id_salt("port")
                .width(165.0)
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if self.available_ports.is_empty() {
                        ui.label(egui::RichText::new("No serial ports found").color(pal.dim));
                    }
                    for port in self.available_ports.clone() {
                        let selected = self.selected_port.as_deref() == Some(port.name.as_str());
                        let label = if port.is_board {
                            egui::RichText::new(format!("{}  ·  board", short_port(&port.name)))
                                .color(pal.ok)
                        } else {
                            egui::RichText::new(short_port(&port.name)).color(pal.dim)
                        };
                        if ui.selectable_label(selected, label).clicked() {
                            self.selected_port = Some(port.name);
                        }
                    }
                    ui.separator();
                    if ui.button("Rescan ports").clicked() {
                        self.available_ports = list_ports();
                        if self.selected_port.is_none() {
                            self.selected_port = default_port(&self.available_ports);
                        }
                    }
                });

            if self.device.is_some() {
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            } else if ui.button("Connect").clicked() {
                self.connect_and_list();
            }

            ui.separator();

            let usable = self.device.is_some() || self.selected_port.is_some();
            if ui
                .add_enabled(usable, egui::Button::new(sym::RUN))
                .on_hover_text("Run the active tab on the device   ⌘R")
                .clicked()
            {
                self.run_current_script();
            }
            if ui
                .add_enabled(usable, egui::Button::new(sym::STOP))
                .on_hover_text("Interrupt whatever is running on the device")
                .clicked()
            {
                self.stop_program();
            }
            if ui
                .add_enabled(usable, egui::Button::new(sym::FLASH))
                .on_hover_text("Write the active tab to the device as main.py")
                .clicked()
            {
                self.flash_active_as_main();
            }
            if ui
                .add_enabled(usable, egui::Button::new(sym::REBOOT))
                .on_hover_text("Soft reboot so boot.py / main.py run")
                .clicked()
            {
                self.run_main_script();
            }

            ui.separator();

            if ui
                .add_enabled(usable, egui::Button::new(sym::SYNC))
                .on_hover_text("Mirror a local folder to or from the device")
                .clicked()
            {
                self.sync_panel.open = true;
            }

            // Version doubles as the way in to the update dialog, so it is
            // always visible without taking a toolbar slot of its own.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(format!("v{}", update::current_version()))
                    .on_hover_text("Check for updates")
                    .clicked()
                {
                    self.update_panel.open = true;
                }
            });
        });
    }

    fn file_rail(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("DEVICE")
                    .small()
                    .strong()
                    .color(pal.dim),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(sym::REFRESH)
                    .on_hover_text("Refresh device files")
                    .clicked()
                {
                    self.refresh_remote_tree();
                }
                let can_add = self.device.is_some();
                if ui
                    .add_enabled(can_add, egui::Button::new(sym::ADD).small())
                    .on_hover_text("New file at the device root")
                    .clicked()
                {
                    self.creating = Some(("/".to_string(), String::new()));
                }
            });
        });
        ui.separator();

        if self.remote_tree.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(if self.device.is_some() {
                        "No files on device"
                    } else {
                        "Not connected"
                    })
                    .color(pal.dim),
                );
                ui.add_space(6.0);
                if self.device.is_none() && ui.button("Connect").clicked() {
                    self.connect_and_list();
                }
            });
            return;
        }

        let mut actions: Vec<TreeAction> = Vec::new();
        let selected = self.selected_remote_path.clone();
        let rename_path = self.renaming.as_ref().map(|(p, _)| p.clone());

        egui::ScrollArea::both().show(ui, |ui| {
            // A pending "new file" at the root gets an inline row above the
            // tree, so creating a file never opens a dialog.
            if let Some((parent, buf)) = self.creating.as_mut()
                && parent == "/"
            {
                inline_name_field(ui, buf, "new file name");
            }

            for node in &self.remote_tree {
                show_node(
                    ui,
                    node,
                    selected.as_deref(),
                    rename_path.as_deref(),
                    self.renaming.as_mut().map(|(_, b)| b),
                    self.creating.as_mut(),
                    &mut actions,
                    pal,
                );
            }
        });

        for action in actions {
            match action {
                TreeAction::Select(path, is_dir) => {
                    self.selected_remote_path = Some(path);
                    self.selected_remote_is_dir = is_dir;
                }
                TreeAction::Open(path) => self.open_path(path),
                TreeAction::StartRename(path) => {
                    let leaf = path.rsplit('/').next().unwrap_or(&path).to_string();
                    self.creating = None;
                    self.renaming = Some((path, leaf));
                }
                TreeAction::Delete(path, is_dir) => self.confirm_delete = Some((path, is_dir)),
                TreeAction::NewFileIn(dir) => {
                    self.renaming = None;
                    self.creating = Some((dir, String::new()));
                }
            }
        }
    }

    fn commit_rename(&mut self) {
        if let Some((old_path, leaf)) = self.renaming.take() {
            let leaf = leaf.trim();
            if leaf.is_empty() {
                return;
            }
            let new_path = join_remote_path(&parent_of(&old_path), leaf);
            if new_path != old_path {
                self.rename_path(&old_path, &new_path);
            }
        }
    }

    fn commit_create(&mut self) {
        if let Some((parent, name)) = self.creating.take() {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            self.create_file(join_remote_path(&parent, name));
        }
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        egui::ScrollArea::horizontal()
            .id_salt("tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut to_close: Option<usize> = None;
                    for idx in 0..self.tabs.len() {
                        let active = idx == self.active_tab;
                        let title = self.tabs[idx].title();
                        // A dot rather than an asterisk: quieter, and it lines
                        // up with the connection indicator's vocabulary.
                        let label = if self.tabs[idx].dirty {
                            format!("{title}  {}", sym::DIRTY)
                        } else {
                            title
                        };
                        let text = if active {
                            egui::RichText::new(label).color(pal.ident).strong()
                        } else {
                            egui::RichText::new(label).color(pal.dim)
                        };

                        if ui.selectable_label(active, text).clicked() {
                            self.active_tab = idx;
                        }
                        if ui
                            .small_button(sym::CLOSE)
                            .on_hover_text("Close tab   ⌘W")
                            .clicked()
                        {
                            to_close = Some(idx);
                        }
                        ui.add_space(4.0);
                    }
                    if ui
                        .small_button(sym::ADD)
                        .on_hover_text("New buffer   ⌘N")
                        .clicked()
                    {
                        self.tabs.push(EditorTab::untitled());
                        self.active_tab = self.tabs.len() - 1;
                    }
                    if let Some(idx) = to_close {
                        self.close_tab(idx);
                    }
                });
            });
    }

    fn editor(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let pal_copy = *pal;
        let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            ui.fonts_mut(|f| f.layout_job(highlight_python(buf.as_str(), &pal_copy)))
        };

        let line_count = self.tabs[self.active_tab].text.lines().count().max(1);

        egui::Frame::new()
            .fill(pal.editor_bg)
            .inner_margin(egui::Margin::symmetric(0, 6))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .id_salt("editor_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            // Gutter: same font and size as the editor, so the
                            // rows line up with no manual offsetting.
                            ui.add_space(8.0);
                            let gutter: String = (1..=line_count)
                                .map(|n| format!("{n:>4}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            ui.label(
                                egui::RichText::new(gutter)
                                    .font(code_font())
                                    .color(pal.gutter),
                            );
                            ui.add_space(8.0);

                            let tab = &mut self.tabs[self.active_tab];
                            let response = ui.add(
                                egui::TextEdit::multiline(&mut tab.text)
                                    .font(code_font())
                                    .code_editor()
                                    .desired_width(f32::INFINITY)
                                    .frame(egui::Frame::NONE)
                                    .layouter(&mut layouter),
                            );
                            if response.changed() {
                                tab.dirty = true;
                            }
                        });
                    });
            });
    }

    fn output_dock(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let has_err = self
            .last_output
            .as_ref()
            .is_some_and(|o| !o.stderr.trim().is_empty());

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("OUTPUT")
                    .small()
                    .strong()
                    .color(pal.dim),
            );
            ui.add_space(4.0);
            ui.selectable_value(&mut self.output_filter, OutputFilter::All, "All");
            ui.selectable_value(&mut self.output_filter, OutputFilter::Stdout, "stdout");
            let stderr_label = if has_err {
                egui::RichText::new("stderr").color(pal.err)
            } else {
                egui::RichText::new("stderr")
            };
            ui.selectable_value(&mut self.output_filter, OutputFilter::Stderr, stderr_label);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Words, not symbols: egui's default font has no glyph for
                // ⌫ / ⌄ / ⌃, so those drew as empty boxes.
                if ui
                    .small_button("Hide")
                    .on_hover_text("Hide the output dock   ⌘J")
                    .clicked()
                {
                    self.output_open = false;
                }
                if ui
                    .small_button("Clear")
                    .on_hover_text("Clear the output")
                    .clicked()
                {
                    self.last_output = None;
                }
            });
        });
        ui.separator();

        egui::ScrollArea::both()
            .id_salt("output_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let Some(res) = &self.last_output else {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Nothing has run yet").color(pal.dim));
                    return;
                };

                let show_out =
                    self.output_filter != OutputFilter::Stderr && !res.stdout.trim().is_empty();
                let show_err =
                    self.output_filter != OutputFilter::Stdout && !res.stderr.trim().is_empty();

                if show_out {
                    ui.label(
                        egui::RichText::new(res.stdout.trim_end())
                            .font(code_font())
                            .color(pal.ident),
                    );
                }
                if show_err {
                    ui.label(
                        egui::RichText::new(res.stderr.trim_end())
                            .font(code_font())
                            .color(pal.err),
                    );
                }
                if !show_out && !show_err {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("(nothing on this stream)").color(pal.dim));
                }
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            let (dot, tint, state) = if self.device.is_some() {
                (sym::CONNECTED_DOT, pal.ok, "Connected")
            } else {
                (sym::DISCONNECTED_DOT, pal.dim, "Disconnected")
            };
            ui.colored_label(tint, dot);
            ui.label(egui::RichText::new(state).color(pal.dim).small());
            ui.label(egui::RichText::new("·").color(pal.dim).small());

            let file = self.tabs[self.active_tab]
                .path
                .clone()
                .unwrap_or_else(|| "untitled".to_string());
            ui.label(egui::RichText::new(file).color(pal.dim).small());
            if self.tabs[self.active_tab].dirty {
                ui.label(egui::RichText::new("unsaved").color(pal.warn).small());
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.output_open
                    && ui
                        .small_button("Show output")
                        .on_hover_text("Show the output dock   ⌘J")
                        .clicked()
                {
                    self.output_open = true;
                }
                if let Some(msg) = &self.last_status {
                    ui.label(egui::RichText::new(msg).color(pal.dim).small());
                }
            });
        });
    }

    /// The inline error banner above the editor.
    fn error_banner(&mut self, ui: &mut egui::Ui, pal: &Palette, err: String) {
        egui::Frame::new()
            .fill(pal.err.gamma_multiply(0.18))
            .inner_margin(egui::Margin::symmetric(9, 6))
            .corner_radius(egui::CornerRadius::same(5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(pal.err, sym::WARN);
                    ui.colored_label(pal.err, err);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(sym::CLOSE)
                            .on_hover_text("Dismiss")
                            .clicked()
                        {
                            self.connection_error = None;
                        }
                    });
                });
            });
        ui.add_space(5.0);
    }
}

impl GuiApp {
    fn save_prefs(&self) {
        Prefs {
            sync_local_dir: self.sync_panel.local_dir.clone(),
            sync_remote_dir: Some(self.sync_panel.remote_dir.clone()),
            last_port: self.selected_port.clone(),
        }
        .save();
    }

    /// Run a sync in the direction and mode the panel is configured for.
    ///
    /// `preview` maps to a dry run: the engine plans everything and touches
    /// nothing, so the action list can be read before committing.
    fn run_sync(&mut self, preview: bool) {
        let Some(local) = self.sync_panel.local_dir.clone() else {
            self.connection_error = Some("Choose a local folder first".to_string());
            return;
        };
        let remote = self.sync_panel.remote_dir.trim().to_string();
        if remote.is_empty() {
            self.connection_error = Some("Enter a device folder".to_string());
            return;
        }

        self.ensure_connected();
        let opts = sync::SyncOptions {
            delete: self.sync_panel.delete,
            dry_run: preview,
            ignore: Vec::new(),
            // The panel keeps no last-sync baseline, so there is nothing to
            // detect conflicts against and every copy is unconditional. The
            // CLI's workspace `sync` is the mode that tracks conflicts.
            force: true,
        };
        let from_device = self.sync_panel.from_device;

        let result = match self.device.as_mut() {
            Some(dev) => {
                if from_device {
                    sync::from_device(dev, &remote, &local, &opts, None, None)
                } else {
                    sync::to_device(dev, &local, &remote, &opts, None, None)
                }
            }
            None => return,
        };

        match result {
            Ok(outcome) => {
                let copied = outcome.count(if from_device { "download" } else { "upload" });
                let deleted = outcome.count("delete_remote_file")
                    + outcome.count("delete_remote_dir")
                    + outcome.count("delete_local_file")
                    + outcome.count("delete_local_dir");
                self.last_status = Some(if preview {
                    format!("Preview: {copied} to copy, {deleted} to delete")
                } else {
                    format!("Synced: {copied} copied, {deleted} deleted")
                });
                self.sync_panel.last_was_preview = preview;
                self.sync_panel.last = Some(outcome);
                self.connection_error = None;
                if !preview {
                    self.refresh_remote_tree();
                }
                self.save_prefs();
            }
            Err(e) => self.fail_device_op("Sync failed", e),
        }
    }

    fn sync_window(&mut self, ctx: &egui::Context, pal: &Palette) {
        let mut open = self.sync_panel.open;
        egui::Window::new("Sync")
            .open(&mut open)
            .resizable(true)
            .default_size([520.0, 420.0])
            .show(ctx, |ui| {
                ui.add_space(2.0);

                // --- direction -------------------------------------------
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("DIRECTION")
                            .small()
                            .strong()
                            .color(pal.dim),
                    );
                    ui.selectable_value(&mut self.sync_panel.from_device, false, "Upload to Pico");
                    ui.selectable_value(
                        &mut self.sync_panel.from_device,
                        true,
                        "Download from Pico",
                    );
                });
                ui.add_space(6.0);

                // --- folders ---------------------------------------------
                ui.horizontal(|ui| {
                    if ui.button("Choose folder…").clicked()
                        && let Some(dir) = rfd::FileDialog::new()
                            .set_title("Local folder to sync")
                            .pick_folder()
                    {
                        self.sync_panel.local_dir = Some(dir);
                        self.sync_panel.last = None;
                        self.save_prefs();
                    }
                    let label = match &self.sync_panel.local_dir {
                        Some(d) => d.display().to_string(),
                        None => "no folder chosen".to_string(),
                    };
                    ui.label(egui::RichText::new(label).font(code_font()).color(
                        if self.sync_panel.local_dir.is_some() {
                            pal.ident
                        } else {
                            pal.dim
                        },
                    ));
                });

                ui.horizontal(|ui| {
                    ui.label("Device folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.sync_panel.remote_dir)
                            .font(code_font())
                            .desired_width(200.0)
                            .hint_text("/app"),
                    );
                });
                ui.add_space(4.0);

                ui.checkbox(
                    &mut self.sync_panel.delete,
                    "Delete files on the destination that are not on the source",
                );
                ui.add_space(8.0);

                // --- actions ---------------------------------------------
                let ready = self.sync_panel.local_dir.is_some()
                    && !self.sync_panel.remote_dir.trim().is_empty();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(ready, egui::Button::new("Preview"))
                        .on_hover_text("Show what would change, without changing anything")
                        .clicked()
                    {
                        self.run_sync(true);
                    }
                    let sync_label = if self.sync_panel.from_device {
                        "Download now"
                    } else {
                        "Upload now"
                    };
                    if ui
                        .add_enabled(ready, egui::Button::new(sync_label).fill(pal.accent))
                        .clicked()
                    {
                        self.run_sync(false);
                    }
                });

                ui.add_space(8.0);
                ui.separator();

                // --- results ---------------------------------------------
                let Some(outcome) = &self.sync_panel.last else {
                    ui.add_space(8.0);
                    let hint = if ready {
                        "Press Preview to see what would change."
                    } else {
                        "Choose a local folder and a device folder to begin."
                    };
                    ui.label(egui::RichText::new(hint).color(pal.dim));
                    return;
                };

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if self.sync_panel.last_was_preview {
                        "Preview — nothing has been changed"
                    } else {
                        "Completed"
                    })
                    .small()
                    .strong()
                    .color(if self.sync_panel.last_was_preview {
                        pal.warn
                    } else {
                        pal.ok
                    }),
                );
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .id_salt("sync_results")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let interesting = outcome
                            .actions
                            .iter()
                            .filter(|a| !a.op.starts_with("skip_") && a.op != "ensure_dir");
                        let mut any = false;
                        for action in interesting {
                            any = true;
                            let (verb, color) = match action.op.as_str() {
                                "upload" => ("upload", pal.ok),
                                "download" => ("download", pal.ok),
                                op if op.starts_with("delete_") => ("delete", pal.err),
                                "remove_stale_staging" => ("clean up", pal.dim),
                                "warning" => ("warning", pal.warn),
                                other => (other, pal.dim),
                            };
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(verb).small().color(color));
                                let target = action
                                    .remote
                                    .as_deref()
                                    .or(action.local.as_deref())
                                    .unwrap_or("");
                                ui.label(
                                    egui::RichText::new(target)
                                        .font(code_font())
                                        .color(pal.ident),
                                );
                            });
                            if let Some(note) = &action.note {
                                ui.label(egui::RichText::new(note).small().color(pal.dim));
                            }
                        }
                        if !any {
                            ui.label(
                                egui::RichText::new("Everything is already up to date.")
                                    .color(pal.dim),
                            );
                        }
                    });
            });
        self.sync_panel.open = open;
    }
}

impl GuiApp {
    /// Start a background update check.
    ///
    /// The worker owns the network call; the UI thread only polls for the
    /// result, so the window keeps redrawing while it runs.
    fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.update_panel.busy {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::check(update::REPO).map_err(|e| e.to_string());
            let _ = tx.send(UpdateMsg::Checked(result));
            // Wake the UI so it notices without waiting for the next event.
            ctx.request_repaint();
        });
        self.update_panel.busy = true;
        self.update_panel.status = Some(("Checking for updates…".to_string(), false));
        self.update_panel.available = None;
        self.update_panel.rx = Some(rx);
    }

    /// Download, verify and install the release found by a previous check.
    fn start_update_install(&mut self, ctx: &egui::Context) {
        let Some(release) = self.update_panel.available.clone() else {
            return;
        };
        if self.update_panel.busy {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let version = release.version.clone();
            let result = update::install(&release)
                .map(|()| version)
                .map_err(|e| e.to_string());
            let _ = tx.send(UpdateMsg::Installed(result));
            ctx.request_repaint();
        });
        self.update_panel.busy = true;
        self.update_panel.status = Some(("Downloading and verifying…".to_string(), false));
        self.update_panel.rx = Some(rx);
    }

    /// Collect any finished update job. Called once per frame.
    fn poll_update(&mut self) {
        let Some(rx) = &self.update_panel.rx else {
            return;
        };
        let Ok(msg) = rx.try_recv() else {
            return;
        };
        self.update_panel.rx = None;
        self.update_panel.busy = false;

        match msg {
            UpdateMsg::Checked(Ok(None)) => {
                self.update_panel.status =
                    Some(("No releases have been published yet.".to_string(), false));
            }
            UpdateMsg::Checked(Ok(Some(update::Check::UpToDate { current }))) => {
                self.update_panel.status =
                    Some((format!("rupico {current} is the latest release."), false));
            }
            UpdateMsg::Checked(Ok(Some(update::Check::Available { current, release }))) => {
                self.update_panel.status = Some((
                    format!(
                        "Version {} is available (you have {current}).",
                        release.version
                    ),
                    false,
                ));
                self.update_panel.available = Some(release);
            }
            UpdateMsg::Checked(Err(e)) => {
                self.update_panel.status = Some((e, true));
            }
            UpdateMsg::Installed(Ok(version)) => {
                self.update_panel.available = None;
                self.update_panel.status = Some((
                    format!("Updated to {version}. Restart rupico to use it."),
                    false,
                ));
            }
            UpdateMsg::Installed(Err(e)) => {
                self.update_panel.status = Some((e, true));
            }
        }
    }

    fn update_window(&mut self, ctx: &egui::Context, pal: &Palette) {
        let mut open = self.update_panel.open;
        egui::Window::new("Updates")
            .open(&mut open)
            .resizable(false)
            .default_size([420.0, 200.0])
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Installed").small().color(pal.dim));
                    ui.label(
                        egui::RichText::new(update::current_version())
                            .font(code_font())
                            .color(pal.ident),
                    );
                });
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.update_panel.busy,
                            egui::Button::new("Check for updates"),
                        )
                        .clicked()
                    {
                        self.start_update_check(ctx);
                    }
                    if self.update_panel.available.is_some()
                        && ui
                            .add_enabled(
                                !self.update_panel.busy,
                                egui::Button::new("Download and install").fill(pal.accent),
                            )
                            .clicked()
                    {
                        self.start_update_install(ctx);
                    }
                    if self.update_panel.busy {
                        ui.spinner();
                    }
                });

                if let Some((msg, is_error)) = &self.update_panel.status {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(msg).color(if *is_error {
                        pal.err
                    } else {
                        pal.ident
                    }));
                }

                if let Some(release) = &self.update_panel.available {
                    ui.add_space(4.0);
                    ui.hyperlink_to(
                        egui::RichText::new("Release notes").small(),
                        release.html_url.clone(),
                    );
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "The download is checked against the release's published SHA256SUMS \
                         before anything is replaced. Only this application is updated — the \
                         rupico command-line tool updates separately with `rupico update`.",
                    )
                    .small()
                    .color(pal.dim),
                );
            });
        self.update_panel.open = open;
    }
}

/// A focused single-line field used for inline create/rename in the tree.
fn inline_name_field(ui: &mut egui::Ui, buf: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        let resp = ui.add(
            egui::TextEdit::singleline(buf)
                .desired_width(150.0)
                .hint_text(hint),
        );
        // Focus follows the field for as long as it exists, so the user can
        // simply type after choosing "New file" or "Rename".
        resp.request_focus();
    });
}

#[allow(clippy::too_many_arguments)]
fn show_node(
    ui: &mut egui::Ui,
    node: &RemoteNode,
    selected: Option<&str>,
    rename_path: Option<&str>,
    mut rename_buf: Option<&mut String>,
    mut creating: Option<&mut (String, String)>,
    actions: &mut Vec<TreeAction>,
    pal: &Palette,
) {
    let is_selected = selected == Some(node.path.as_str());

    // Inline rename replaces the row's label in place, so renaming never opens
    // a dialog.
    if rename_path == Some(node.path.as_str())
        && let Some(buf) = rename_buf.as_deref_mut()
    {
        inline_name_field(ui, buf, "new name");
        return;
    }

    if node.is_dir {
        let resp =
            egui::CollapsingHeader::new(egui::RichText::new(&node.name).color(pal.ident).strong())
                .id_salt(&node.path)
                // Collapsed by default. Expanding every top-level directory pushed
                // the root's own files (main.py, config.py) below a long list of
                // module files.
                .default_open(false)
                .show(ui, |ui| {
                    // A pending "new file" inside this directory shows as a row here.
                    if let Some((parent, buf)) = creating.as_deref_mut()
                        && parent == &node.path
                    {
                        inline_name_field(ui, buf, "new file name");
                    }
                    for child in &node.children {
                        show_node(
                            ui,
                            child,
                            selected,
                            rename_path,
                            rename_buf.as_deref_mut(),
                            creating.as_deref_mut(),
                            actions,
                            pal,
                        );
                    }
                });

        if resp.header_response.clicked() {
            actions.push(TreeAction::Select(node.path.clone(), true));
        }
        resp.header_response.context_menu(|ui| {
            if ui.button("New file here").clicked() {
                actions.push(TreeAction::NewFileIn(node.path.clone()));
                ui.close();
            }
            if ui.button("Rename…").clicked() {
                actions.push(TreeAction::StartRename(node.path.clone()));
                ui.close();
            }
            ui.separator();
            if ui.button("Delete").clicked() {
                actions.push(TreeAction::Delete(node.path.clone(), true));
                ui.close();
            }
        });
    } else {
        let color = if is_selected { pal.accent } else { pal.ident };
        let resp = ui.selectable_label(is_selected, egui::RichText::new(&node.name).color(color));

        if resp.clicked() {
            actions.push(TreeAction::Select(node.path.clone(), false));
        }
        // Double-click opens, which is what a file tree is expected to do.
        if resp.double_clicked() {
            actions.push(TreeAction::Open(node.path.clone()));
        }
        resp.context_menu(|ui| {
            if ui.button("Open").clicked() {
                actions.push(TreeAction::Open(node.path.clone()));
                ui.close();
            }
            if ui.button("Rename…").clicked() {
                actions.push(TreeAction::StartRename(node.path.clone()));
                ui.close();
            }
            ui.separator();
            if ui.button("Delete").clicked() {
                actions.push(TreeAction::Delete(node.path.clone(), false));
                ui.close();
            }
        });
    }
}

fn build_remote_tree(
    dev: &mut MicroPythonDevice,
    path: &str,
    depth: usize,
    max_depth: usize,
) -> MpResult<Vec<RemoteNode>> {
    let mut nodes = Vec::new();
    let entries = dev.list_dir(path)?;

    for e in entries {
        let full = join_remote_path(path, &e.name);
        let children = if e.is_dir && depth < max_depth {
            build_remote_tree(dev, &full, depth + 1, max_depth)?
        } else {
            Vec::new()
        };
        nodes.push(RemoteNode {
            name: e.name,
            path: full,
            is_dir: e.is_dir,
            children,
        });
    }

    // Directories first, then files, each alphabetically.
    nodes.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(nodes)
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        apply_style(&ctx);
        let pal = Palette::for_theme(ctx.theme() == egui::Theme::Dark);

        // There is always at least one tab, and the active index always points
        // at a live one — every `self.tabs[self.active_tab]` below relies on it.
        if self.tabs.is_empty() {
            self.tabs.push(EditorTab::untitled());
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);

        self.handle_shortcuts(&ctx);

        // Commit or cancel any inline tree edit.
        let (enter, escape) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if enter {
            self.commit_rename();
            self.commit_create();
        }
        if escape {
            self.renaming = None;
            self.creating = None;
        }

        // Outline each rail so the panels read as distinct surfaces instead
        // of one flat sheet; in light mode the fills alone are too close.
        let rail_frame = |pal: &Palette| {
            egui::Frame::new()
                .fill(pal.rail)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .stroke(egui::Stroke::new(1.0, pal.divider))
        };

        egui::Panel::top("toolbar")
            .frame(rail_frame(&pal))
            .show(ui, |ui| self.toolbar(ui, &pal));

        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(pal.rail)
                    .inner_margin(egui::Margin::symmetric(10, 3))
                    .stroke(egui::Stroke::new(1.0, pal.divider)),
            )
            .show(ui, |ui| self.status_bar(ui, &pal));

        if self.output_open {
            egui::Panel::bottom("output")
                .resizable(true)
                .default_size(170.0)
                .frame(rail_frame(&pal))
                .show(ui, |ui| self.output_dock(ui, &pal));
        }

        egui::Panel::left("files")
            .resizable(true)
            .default_size(225.0)
            .frame(
                egui::Frame::new()
                    .fill(pal.rail)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .stroke(egui::Stroke::new(1.0, pal.divider)),
            )
            .show(ui, |ui| self.file_rail(ui, &pal));

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(2.0);
            self.tab_strip(ui, &pal);
            ui.add_space(2.0);
            if let Some(err) = self.connection_error.clone() {
                self.error_banner(ui, &pal, err);
            }
            self.editor(ui, &pal);
        });

        self.poll_update();

        if self.sync_panel.open {
            self.sync_window(&ctx, &pal);
        }

        if self.update_panel.open {
            self.update_window(&ctx, &pal);
        }

        if let Some((path, is_dir)) = self.confirm_delete.clone() {
            let modal = egui::Modal::new(egui::Id::new("confirm_delete")).show(&ctx, |ui| {
                ui.set_width(330.0);
                ui.heading("Delete from device?");
                ui.add_space(8.0);
                ui.label(egui::RichText::new(&path).font(code_font()));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if is_dir {
                        "The directory must already be empty."
                    } else {
                        "This cannot be undone."
                    })
                    .color(pal.dim),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let danger = egui::Button::new(
                            egui::RichText::new("Delete").color(egui::Color32::WHITE),
                        )
                        .fill(pal.err);
                        if ui.add(danger).clicked() {
                            self.confirm_delete = None;
                            self.delete_path(&path, is_dir);
                        }
                    });
                });
            });
            if modal.should_close() {
                self.confirm_delete = None;
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("rupico"),
        ..Default::default()
    };

    eframe::run_native(
        "rupico",
        options,
        Box::new(|_cc| Ok(Box::new(GuiApp::default()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decompose a highlighted job into `(text, colour)` runs so assertions
    /// can talk about what was coloured rather than about layout internals.
    fn runs(src: &str) -> Vec<(String, egui::Color32)> {
        let pal = Palette::for_theme(true);
        let job = highlight_python(src, &pal);
        job.sections
            .iter()
            .map(|s| {
                // `byte_range` is in `ByteIndex`, a newtype over usize.
                let range = s.byte_range.start.0..s.byte_range.end.0;
                (job.text[range].to_string(), s.format.color)
            })
            .collect()
    }

    /// The concatenated runs must reproduce the input exactly — a highlighter
    /// that drops or duplicates a character would corrupt what the user sees.
    fn assert_lossless(src: &str) {
        let joined: String = runs(src).into_iter().map(|(t, _)| t).collect();
        assert_eq!(joined, src, "highlighting must preserve the source text");
    }

    /// Colour applied at the first occurrence of `needle`.
    ///
    /// Looked up by byte offset rather than by matching a whole run:
    /// `LayoutJob::append` coalesces neighbouring runs that share a format, so
    /// "blink" in `def blink():` is not a run of its own.
    fn color_of(src: &str, needle: &str) -> egui::Color32 {
        let at = src
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} does not occur in {src:?}"));
        let pal = Palette::for_theme(true);
        let job = highlight_python(src, &pal);
        job.sections
            .iter()
            .find(|s| (s.byte_range.start.0..s.byte_range.end.0).contains(&at))
            .unwrap_or_else(|| panic!("no section covers byte {at} of {src:?}"))
            .format
            .color
    }

    /// Characters confirmed to render in egui's default font by looking at
    /// the running app. Anything outside this set is guilty until seen.
    ///
    /// `Fonts::has_glyph` is not usable for this: it reports `▶`, `⚡`, `●`
    /// and `⚠` as missing even though they render fine, because it does not
    /// consult the emoji fallback that supplies them.
    const VISUALLY_CONFIRMED: &[char] = &[
        '▶', '⏹', '⚡', '⟲', '⏺', '⟳', '+', '×', '●', '⚠', '▼', '▲',
        // Seen rendering in the status bar ("Connected · untitled"), the
        // context menu ("Rename…") and the sync panel ("Choose folder…").
        '·', '…',
    ];

    #[test]
    fn ui_labels_use_only_confirmed_glyphs() {
        // Regression: ⇄, →, ⌫, ⌄ and ⌃ are all absent from egui's default
        // font and shipped as empty boxes — three separate times, because
        // each check only covered the symbols someone had remembered to
        // register. This scans the source instead, so a symbol dropped into
        // any widget label is caught whether or not it was registered.
        let src = include_str!("rupico_gui.rs");
        let constructors = [
            "Button::new(",
            ".button(",
            ".small_button(",
            "selectable_value(",
            "RichText::new(",
            "colored_label(",
            "ui.label(",
        ];

        let mut offenders: Vec<(usize, char, &str)> = Vec::new();
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if !constructors.iter().any(|c| line.contains(c)) {
                continue;
            }
            for c in line.chars() {
                if !c.is_ascii() && !VISUALLY_CONFIRMED.contains(&c) {
                    offenders.push((n + 1, c, line.trim()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "unconfirmed symbols in UI labels: {offenders:#?}\n\
             Check each renders in the real window, then add it to \
             VISUALLY_CONFIRMED."
        );
    }

    #[test]
    fn highlighting_never_alters_the_source() {
        for src in [
            "",
            "x = 1\n",
            "# just a comment",
            "s = 'unterminated\nnext = 2\n",
            "t = \"\"\"triple\nspanning\"\"\"\n",
            "q = 'it\\'s escaped'\n",
            "@decorator\ndef f():\n    return 0x1F\n",
            "unicode = 'héllo — ünïcode'\n",
        ] {
            assert_lossless(src);
        }
    }

    #[test]
    fn keywords_and_identifiers_are_distinguished() {
        let pal = Palette::for_theme(true);
        let src = "def blink():\n    pass\n";
        assert_eq!(color_of(src, "def"), pal.keyword);
        assert_eq!(color_of(src, "blink"), pal.ident);
        assert_eq!(color_of(src, "pass"), pal.keyword);
    }

    #[test]
    fn a_word_containing_a_keyword_is_not_a_keyword() {
        let pal = Palette::for_theme(true);
        // "format" contains "for"; naive substring matching would miscolour it.
        assert_eq!(color_of("format = 1", "format"), pal.ident);
        assert_eq!(color_of("is_ready = 1", "is_ready"), pal.ident);
    }

    #[test]
    fn comments_strings_numbers_and_decorators_are_coloured() {
        let pal = Palette::for_theme(true);
        assert_eq!(color_of("x = 1  # note", "# note"), pal.comment);
        assert_eq!(color_of("s = 'hi'", "'hi'"), pal.string);
        assert_eq!(color_of("n = 42", "42"), pal.number);
        assert_eq!(
            color_of("@micropython.native", "@micropython.native"),
            pal.decorator
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let pal = Palette::for_theme(true);
        let src = r"s = 'it\'s fine' + x";
        assert_eq!(color_of(src, r"'it\'s fine'"), pal.string);
        // The tail after the literal is code again, not string.
        assert_eq!(color_of(src, "x"), pal.ident);
    }

    #[test]
    fn an_unterminated_string_stops_at_the_newline() {
        let pal = Palette::for_theme(true);
        // Otherwise one stray quote would paint the rest of the file green.
        let src = "s = 'oops\nkeyword = None\n";
        assert_eq!(color_of(src, "None"), pal.keyword);
    }

    #[test]
    fn a_keyword_inside_a_comment_or_string_stays_uncoloured() {
        let pal = Palette::for_theme(true);
        assert_eq!(color_of("# def not code", "# def not code"), pal.comment);
        assert_eq!(color_of("s = 'return me'", "'return me'"), pal.string);
    }

    #[test]
    fn parent_of_handles_root_and_nesting() {
        assert_eq!(parent_of("/main.py"), "/");
        assert_eq!(parent_of("/app/lib/util.py"), "/app/lib");
        assert_eq!(parent_of("bare.py"), "/");
    }

    #[test]
    fn tab_titles_use_the_leaf_name() {
        assert_eq!(EditorTab::untitled().title(), "untitled");
        assert_eq!(
            EditorTab::from_remote("/sensors/bme280.py".into(), String::new()).title(),
            "bme280.py"
        );
    }

    fn port(name: &str, is_board: bool) -> PortEntry {
        PortEntry {
            name: name.to_string(),
            is_board,
        }
    }

    #[test]
    fn default_port_prefers_a_board_over_other_hardware() {
        let ports = vec![
            port("/dev/cu.Bluetooth-Incoming-Port", false),
            port("/dev/cu.usbmodem101", true),
        ];
        assert_eq!(default_port(&ports).as_deref(), Some("/dev/cu.usbmodem101"));
    }

    #[test]
    fn default_port_prefers_the_callout_node_over_its_tty_twin() {
        // macOS lists both; opening the tty node can block on carrier detect.
        let ports = vec![
            port("/dev/tty.usbmodem101", true),
            port("/dev/cu.usbmodem101", true),
        ];
        assert_eq!(default_port(&ports).as_deref(), Some("/dev/cu.usbmodem101"));
    }

    #[test]
    fn default_port_falls_back_to_a_lone_port_but_never_guesses() {
        assert_eq!(
            default_port(&[port("/dev/cu.usbserial", false)]).as_deref(),
            Some("/dev/cu.usbserial")
        );
        // Several non-board ports and no board: pick none rather than poke
        // whichever happened to sort first.
        let ambiguous = vec![
            port("/dev/cu.Bluetooth-Incoming-Port", false),
            port("/dev/cu.debug-console", false),
        ];
        assert_eq!(default_port(&ambiguous), None);
        assert_eq!(default_port(&[]), None);
    }

    #[test]
    fn port_labels_drop_the_dev_prefix() {
        assert_eq!(short_port("/dev/cu.usbmodem1101"), "cu.usbmodem1101");
        assert_eq!(short_port("COM3"), "COM3");
    }
}
