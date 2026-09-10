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
    self, ExecResult, InterruptHandle, MicroPythonDevice, Result as MpResult, TreeOptions,
    join_remote_path, remote_leaf, remote_parent, vid_looks_micropython,
};
use rupico::sync;
use rupico::update;
use serialport::available_ports;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

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
    pub const REPL: &str = "REPL";
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

/// Which view the bottom dock is showing.
///
/// Output and the REPL are the same kind of thing — a transcript of what the
/// board said — so they share one resizable dock rather than competing for
/// vertical space.
#[derive(PartialEq, Eq, Clone, Copy)]
enum DockTab {
    Output,
    Repl,
}

/// One exchange in the REPL scrollback.
struct ReplEntry {
    /// What the user submitted, or `None` for a line rupico itself printed
    /// (a connection failure, say) so notes cannot be mistaken for input.
    source: Option<String>,
    stdout: String,
    stderr: String,
    /// The entry is on the board and its result has not come back yet. The
    /// prompt echoes immediately, so this marks the gap.
    pending: bool,
}

/// Scrollback and input state for the REPL dock.
///
/// Entries run through `MicroPythonDevice::run_repl_entry` on the existing
/// raw-REPL connection, so the prompt shares the board's globals with the Run
/// button and the file tree keeps working between commands. It is not a
/// terminal emulator: output arrives when the entry finishes, not as it is
/// printed.
#[derive(Default)]
struct ReplPanel {
    input: String,
    entries: Vec<ReplEntry>,
    /// Submitted entries, oldest first, for Up/Down recall.
    history: Vec<String>,
    /// Where Up/Down currently sits in `history`. `None` is the live edit,
    /// which is what Down comes back to.
    history_pos: Option<usize>,
    /// Set when the input should take keyboard focus on the next frame.
    focus_input: bool,
}

/// How much scrollback to keep.
///
/// A session that leaves a sensor loop printing can produce entries without
/// bound; the oldest ones are the ones nobody scrolls back to.
const MAX_REPL_ENTRIES: usize = 400;

impl ReplPanel {
    /// Id of the input field, needed before the widget is built so Enter and
    /// the arrow keys can be claimed only while it has focus.
    fn input_id() -> egui::Id {
        egui::Id::new("repl_input")
    }

    fn push(&mut self, entry: ReplEntry) {
        self.entries.push(entry);
        if self.entries.len() > MAX_REPL_ENTRIES {
            let excess = self.entries.len() - MAX_REPL_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Remember a submitted entry, skipping an immediate repeat so holding
    /// Up walks distinct commands.
    fn remember(&mut self, source: &str) {
        if self.history.last().map(String::as_str) != Some(source) {
            self.history.push(source.to_string());
        }
        self.history_pos = None;
    }

    /// Step back through history, oldest-ward.
    fn recall_older(&mut self) {
        let pos = match self.history_pos {
            None => self.history.len().checked_sub(1),
            Some(0) => Some(0),
            Some(i) => Some(i - 1),
        };
        if let Some(i) = pos {
            self.history_pos = Some(i);
            self.input = self.history[i].clone();
        }
    }

    /// Step forward through history; past the newest entry is the empty line
    /// the user was typing before they started recalling.
    fn recall_newer(&mut self) {
        match self.history_pos {
            Some(i) if i + 1 < self.history.len() => {
                self.history_pos = Some(i + 1);
                self.input = self.history[i + 1].clone();
            }
            Some(_) => {
                self.history_pos = None;
                self.input.clear();
            }
            None => {}
        }
    }
}

/// Take a keypress with *no* modifiers at all out of the event queue.
///
/// `InputState::consume_key(Modifiers::NONE, ..)` is not this: it matches
/// modifiers logically and ignores Shift, so it claims Shift-Enter too — which
/// submitted the REPL entry instead of breaking the line, and claimed
/// Shift-Up instead of extending the selection.
fn take_bare_key(input: &mut egui::InputState, key: egui::Key) -> bool {
    let mut hit = false;
    input.events.retain(|event| {
        let bare = matches!(
            event,
            egui::Event::Key {
                key: pressed_key,
                pressed: true,
                modifiers,
                ..
            } if *pressed_key == key && modifiers.matches_exact(egui::Modifiers::NONE)
        );
        hit |= bare;
        !bare
    });
    hit
}

/// Render a submitted entry the way an interactive prompt would, so a pasted
/// block reads as one submission rather than several.
fn prompt_block(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 4);
    for (i, line) in source.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(if i == 0 { ">>> " } else { "... " });
        out.push_str(line);
    }
    out
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
    /// Skip files and directories whose name starts with `.`.
    skip_hidden: bool,
    /// Skip Markdown files, which are documentation the board never runs.
    skip_markdown: bool,
    /// Download instead of upload.
    from_device: bool,
    /// Result of the last run, kept on screen so a preview can be read before
    /// committing to it.
    last: Option<sync::SyncOutcome>,
    /// Whether `last` came from a dry run.
    last_was_preview: bool,
    /// Decisions from the run in flight, as they arrive.
    live: Vec<sync::SyncAction>,
}

/// Ignore pattern matching any component starting with a dot.
const HIDDEN_IGNORE: &str = ".*";
/// Ignore pattern for Markdown files.
const MARKDOWN_IGNORE: &str = "*.md";

impl SyncPanel {
    /// Ignore patterns for the panel's exclusion checkboxes.
    ///
    /// These layer on top of the engine's built-ins rather than filtering
    /// separately, so an excluded file is also protected from a `delete`
    /// pass: the engine drops ignored entries from both sides before
    /// comparing them, so they are never seen as "absent from the source".
    fn ignore_patterns(&self) -> Vec<String> {
        let mut pats = Vec::new();
        if self.skip_hidden {
            pats.push(HIDDEN_IGNORE.to_string());
        }
        if self.skip_markdown {
            pats.push(MARKDOWN_IGNORE.to_string());
        }
        pats
    }
}

impl Default for SyncPanel {
    fn default() -> Self {
        Self {
            open: false,
            local_dir: None,
            remote_dir: "/".to_string(),
            delete: false,
            // Both default off: the panel remembers its settings, and an
            // exclusion that switched itself on would quietly change what an
            // existing setup syncs.
            skip_hidden: false,
            skip_markdown: false,
            from_device: false,
            last: None,
            last_was_preview: false,
            live: Vec::new(),
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
    sync_skip_hidden: bool,
    #[serde(default)]
    sync_skip_markdown: bool,
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

// ---------------------------------------------------------------------------
// Device worker
// ---------------------------------------------------------------------------

/// Work the UI hands to the device thread.
///
/// Every variant is one user action, and the thread runs them in the order
/// they were sent — so a queued job that needs a connection can simply be
/// sent behind `Connect`. Stopping a running program is deliberately *not*
/// here: it has to reach a device whose thread is blocked reading, so it goes
/// down the interrupt handle instead.
enum Job {
    Connect {
        port: String,
    },
    Disconnect,
    /// Put the connection back in raw REPL after an interrupt.
    Resync,
    RefreshTree,
    Open {
        path: String,
    },
    Save {
        path: String,
        text: String,
    },
    Create {
        path: String,
    },
    Delete {
        path: String,
        is_dir: bool,
        recursive: bool,
    },
    Rename {
        old: String,
        new: String,
    },
    RunScript {
        path: Option<String>,
        text: String,
        save_first: bool,
    },
    RunRepl {
        source: String,
    },
    Flash {
        text: String,
    },
    RunMain,
    Sync {
        local: PathBuf,
        remote: String,
        opts: sync::SyncOptions,
        from_device: bool,
        preview: bool,
    },
    Shutdown,
}

/// What kind of work a job is, which decides what cancelling it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// Runs user code on the board. Only Ctrl-C stops it.
    Exec,
    /// A long loop the worker can be asked to leave early.
    Cancellable,
    /// Short enough that cancelling is meaningless.
    Quick,
}

impl Job {
    /// What the status bar says while this runs.
    fn label(&self) -> String {
        match self {
            Job::Connect { port } => format!("Connecting to {}", short_port(port)),
            Job::Disconnect => "Disconnecting".to_string(),
            Job::Resync => "Resynchronising".to_string(),
            Job::RefreshTree => "Listing device files".to_string(),
            Job::Open { path } => format!("Opening {path}"),
            Job::Save { path, .. } => format!("Saving {path}"),
            Job::Create { path } => format!("Creating {path}"),
            Job::Delete { path, .. } => format!("Deleting {path}"),
            Job::Rename { old, .. } => format!("Renaming {old}"),
            Job::RunScript { path, .. } => match path {
                Some(p) => format!("Running {p}"),
                None => "Running buffer".to_string(),
            },
            Job::RunRepl { .. } => "Running REPL entry".to_string(),
            Job::Flash { .. } => "Flashing main.py".to_string(),
            Job::RunMain => "Rebooting".to_string(),
            Job::Sync { preview: true, .. } => "Previewing sync".to_string(),
            Job::Sync { .. } => "Syncing".to_string(),
            Job::Shutdown => "Closing".to_string(),
        }
    }

    fn kind(&self) -> JobKind {
        match self {
            Job::RunScript { .. } | Job::RunRepl { .. } | Job::RunMain => JobKind::Exec,
            Job::Sync { .. } => JobKind::Cancellable,
            _ => JobKind::Quick,
        }
    }
}

/// What the device thread reports back.
enum Update {
    Started(String, JobKind),
    Finished,
    Connected(String),
    Disconnected,
    Tree(Vec<RemoteNode>),
    Opened {
        path: String,
        text: String,
    },
    Saved {
        path: String,
    },
    Created {
        path: String,
    },
    Deleted {
        path: String,
    },
    Renamed {
        old: String,
        new: String,
    },
    Output(ExecResult),
    Repl {
        source: String,
        stdout: String,
        stderr: String,
    },
    Status(String),
    SyncProgress(sync::SyncAction),
    SyncDone {
        outcome: Box<sync::SyncOutcome>,
        preview: bool,
        from_device: bool,
    },
    /// Something the board printed to stderr without raising.
    Notice(String),
    Failed {
        what: String,
        message: String,
        connected: bool,
    },
}

/// The UI's end of the device thread.
struct DeviceLink {
    jobs: mpsc::Sender<Job>,
    updates: mpsc::Receiver<Update>,
    /// Asks the running job to stop at its next safe boundary.
    cancel: Arc<AtomicBool>,
    /// A writer for Ctrl-C, usable while the worker is blocked mid-exec.
    interrupt: Arc<Mutex<Option<InterruptHandle>>>,
}

impl DeviceLink {
    fn spawn(ctx: egui::Context) -> Self {
        let (jobs, job_rx) = mpsc::channel();
        let (update_tx, updates) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(Mutex::new(None));

        let worker = DeviceWorker {
            device: None,
            updates: update_tx,
            cancel: Arc::clone(&cancel),
            interrupt: Arc::clone(&interrupt),
            ctx,
        };
        std::thread::Builder::new()
            .name("rupico-device".to_string())
            .spawn(move || worker.run(job_rx))
            .expect("spawn the device thread");

        Self {
            jobs,
            updates,
            cancel,
            interrupt,
        }
    }

    /// Queue a job. A closed channel means the worker is gone, which only
    /// happens as the app exits.
    fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Write Ctrl-C to the board from *this* thread.
    ///
    /// The point of the second handle: the worker is usually blocked reading
    /// the output of exactly the program the user wants to stop.
    fn interrupt(&self) -> std::result::Result<(), String> {
        let mut guard = self.interrupt.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(handle) => handle.interrupt().map_err(|e| e.to_string()),
            None => Err("Not connected".to_string()),
        }
    }
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        // Give the worker the chance to leave the board in the friendly REPL.
        // Not joined: the process is on its way out, and a join could block
        // behind a transfer that is still running.
        self.send(Job::Shutdown);
    }
}

/// The device thread: owns the connection, and is the only place that talks
/// to the board.
struct DeviceWorker {
    device: Option<MicroPythonDevice>,
    updates: mpsc::Sender<Update>,
    cancel: Arc<AtomicBool>,
    interrupt: Arc<Mutex<Option<InterruptHandle>>>,
    ctx: egui::Context,
}

impl DeviceWorker {
    fn send(&self, update: Update) {
        if self.updates.send(update).is_ok() {
            // An idle egui window redraws only on input, so without this the
            // update would sit in the channel until the user moved the mouse.
            self.ctx.request_repaint();
        }
    }

    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            if matches!(job, Job::Shutdown) {
                break;
            }
            // The flag belongs to the job that is about to run; a cancel that
            // arrived while nothing was running must not kill the next thing
            // the user asks for.
            self.cancel.store(false, Ordering::Relaxed);
            self.send(Update::Started(job.label(), job.kind()));
            self.run_job(job);
            self.report_device_notices();
            self.send(Update::Finished);
        }
        self.disconnect();
    }

    fn run_job(&mut self, job: Job) {
        match job {
            Job::Connect { port } => self.connect(&port),
            Job::Disconnect => {
                self.disconnect();
                self.send(Update::Disconnected);
            }
            Job::Resync => {
                if let Some(dev) = self.device.as_mut()
                    && dev.recover().is_err()
                {
                    self.drop_device();
                    self.send(Update::Disconnected);
                }
            }
            Job::RefreshTree => self.refresh_tree(),
            Job::Open { path } => {
                if let Some(text) = self.attempt(&format!("Failed to open {path}"), |dev| {
                    dev.read_text_file(&path)
                }) {
                    self.send(Update::Opened { path, text });
                }
            }
            Job::Save { path, text } => {
                if self
                    .attempt(&format!("Failed to save {path}"), |dev| {
                        dev.write_text_file(&path, &text)
                    })
                    .is_some()
                {
                    self.send(Update::Saved { path });
                }
            }
            Job::Create { path } => {
                if self
                    .attempt(&format!("Failed to create {path}"), |dev| {
                        dev.write_text_file(&path, "")
                    })
                    .is_some()
                {
                    self.send(Update::Created { path });
                    self.refresh_tree();
                }
            }
            Job::Delete {
                path,
                is_dir,
                recursive,
            } => {
                let what = format!("Failed to delete {path}");
                let done = if recursive {
                    self.attempt(&what, |dev| {
                        dev.remove_tree(&path).map(|o| {
                            format!("Deleted {} file(s) and {} folder(s)", o.files, o.dirs)
                        })
                    })
                } else if is_dir {
                    self.attempt(&what, |dev| dev.rmdir(&path).map(|()| String::new()))
                } else {
                    self.attempt(&what, |dev| dev.remove(&path).map(|()| String::new()))
                };
                if let Some(note) = done {
                    if !note.is_empty() {
                        self.send(Update::Status(note));
                    }
                    self.send(Update::Deleted { path });
                    self.refresh_tree();
                }
            }
            Job::Rename { old, new } => {
                if self
                    .attempt(&format!("Failed to rename {old}"), |dev| {
                        dev.rename(&old, &new)
                    })
                    .is_some()
                {
                    self.send(Update::Renamed { old, new });
                    self.refresh_tree();
                }
            }
            Job::RunScript {
                path,
                text,
                save_first,
            } => self.run_script(path, text, save_first),
            Job::RunRepl { source } => {
                match self.attempt("REPL error", |dev| dev.run_repl_entry(&source)) {
                    Some(res) => self.send(Update::Repl {
                        source,
                        stdout: res.stdout,
                        stderr: res.stderr,
                    }),
                    None => self.send(Update::Repl {
                        source,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                }
            }
            Job::Flash { text } => {
                if self
                    .attempt("Failed to flash main.py", |dev| {
                        dev.flash_main_script(&text)
                    })
                    .is_some()
                {
                    self.send(Update::Status("Flashed active tab as main.py".to_string()));
                    self.refresh_tree();
                }
            }
            Job::RunMain => self.run_main(),
            Job::Sync {
                local,
                remote,
                opts,
                from_device,
                preview,
            } => self.run_sync(local, remote, opts, from_device, preview),
            Job::Shutdown => {}
        }
    }

    /// Run one device call, reporting failure and putting the connection back
    /// into a known state.
    ///
    /// A raw-REPL failure usually means the protocol desynced partway through
    /// a frame. Leaving the handle open would make every later operation fail
    /// in confusing ways against a connection that still looks healthy, so we
    /// re-interrupt and re-enter raw REPL — and if even that fails, the handle
    /// is dropped so the UI shows an honest "disconnected".
    fn attempt<T>(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut MicroPythonDevice) -> MpResult<T>,
    ) -> Option<T> {
        let result = match self.device.as_mut() {
            Some(dev) => f(dev),
            None => {
                self.send(Update::Failed {
                    what: what.to_string(),
                    message: "not connected".to_string(),
                    connected: false,
                });
                return None;
            }
        };

        match result {
            Ok(value) => Some(value),
            Err(e) => {
                let message = e.to_string();
                let recovered = matches!(self.device.as_mut().map(|d| d.recover()), Some(Ok(())));
                if !recovered {
                    self.drop_device();
                }
                self.send(Update::Failed {
                    what: what.to_string(),
                    message,
                    connected: recovered,
                });
                None
            }
        }
    }

    fn connect(&mut self, port: &str) {
        self.disconnect();

        let mut dev = match MicroPythonDevice::connect(port) {
            Ok(mut d) => {
                // The 3 s default suited a UI that blocked on every call: it
                // bounded how long the window could freeze. Nothing freezes
                // now, and the deadline resets whenever the board sends a
                // byte, so a generous idle limit just means a sleepy program
                // finishes instead of failing. It stays finite so a pulled
                // cable still surfaces as an error rather than a wedged job,
                // and Stop interrupts anything longer.
                d.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                d
            }
            Err(e) => {
                self.send(Update::Failed {
                    what: "Failed to connect".to_string(),
                    message: e.to_string(),
                    connected: false,
                });
                return;
            }
        };
        if let Err(e) = dev.enter_raw_repl() {
            self.send(Update::Failed {
                what: "Failed to enter raw REPL".to_string(),
                message: e.to_string(),
                connected: false,
            });
            return;
        }

        match dev.interrupt_handle() {
            Ok(handle) => *self.interrupt.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle),
            // Worth saying out loud: without it, Stop cannot reach a board
            // that is already running something.
            Err(e) => self.send(Update::Notice(format!(
                "Stop will not work on this port: {e}"
            ))),
        }

        self.device = Some(dev);
        self.send(Update::Connected(port.to_string()));
        self.refresh_tree();
    }

    /// Close the connection, leaving the board in the friendly REPL.
    fn disconnect(&mut self) {
        if let Some(mut dev) = self.device.take() {
            let _ = dev.exit_raw_repl();
        }
        *self.interrupt.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Drop a connection that is past saving, without trying to talk on it.
    fn drop_device(&mut self) {
        self.device = None;
        *self.interrupt.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn refresh_tree(&mut self) {
        // Metadata only: the rail shows names, and hashing would read every
        // file on the board to draw them.
        let entries = self.attempt("Failed to list device files", |dev| {
            dev.list_tree("/", TreeOptions::metadata_only())
        });
        match entries {
            Some(Some(entries)) => self.send(Update::Tree(tree_from_entries(&entries, "/"))),
            Some(None) => self.send(Update::Tree(Vec::new())),
            None => self.send(Update::Tree(Vec::new())),
        }
    }

    fn run_script(&mut self, path: Option<String>, text: String, save_first: bool) {
        if save_first && let Some(remote) = path.clone() {
            if self
                .attempt("Failed to save before run", |dev| {
                    dev.write_text_file(&remote, &text)
                })
                .is_none()
            {
                return;
            }
            self.send(Update::Saved { path: remote });
        }

        let result = match &path {
            Some(remote) => {
                let remote = remote.clone();
                self.attempt("Execution error", move |dev| dev.run_file(&remote))
            }
            None => self.attempt("Execution error", |dev| dev.run_snippet(&text)),
        };
        if let Some(res) = result {
            self.send(Update::Status(
                if res.stderr.trim().is_empty() {
                    "Run finished"
                } else {
                    "Run raised an exception"
                }
                .to_string(),
            ));
            self.send(Update::Output(res));
        }
    }

    fn run_main(&mut self) {
        // The soft reboot leaves the board outside raw REPL, so the handle is
        // dropped deliberately rather than kept in a state we cannot use.
        let Some(mut dev) = self.device.take() else {
            self.send(Update::Failed {
                what: "Failed to run main.py".to_string(),
                message: "not connected".to_string(),
                connected: false,
            });
            return;
        };
        let outcome = dev.run_main();
        self.drop_device();

        match outcome {
            Ok(()) => {
                self.send(Update::Output(ExecResult {
                    stdout: "Soft reboot triggered; boot.py / main.py should run on the device.\n\
                             Reconnect to regain the raw REPL.\n"
                        .to_string(),
                    stderr: String::new(),
                }));
                self.send(Update::Status("Soft reboot triggered".to_string()));
                self.send(Update::Disconnected);
                self.send(Update::Tree(Vec::new()));
            }
            Err(e) => {
                self.send(Update::Failed {
                    what: "Failed to run main.py".to_string(),
                    message: e.to_string(),
                    connected: false,
                });
                self.send(Update::Disconnected);
            }
        }
    }

    fn run_sync(
        &mut self,
        local: PathBuf,
        remote: String,
        opts: sync::SyncOptions,
        from_device: bool,
        preview: bool,
    ) {
        let updates = self.updates.clone();
        let ctx = self.ctx.clone();
        // Each decision is reported as it is made, so a long sync shows its
        // progress instead of one silent pause and a summary.
        let mut report = move |action: &sync::SyncAction| {
            let _ = updates.send(Update::SyncProgress(action.clone()));
            ctx.request_repaint();
        };

        let outcome = {
            let result = match self.device.as_mut() {
                Some(dev) => {
                    if from_device {
                        sync::from_device(dev, &remote, &local, &opts, None, Some(&mut report))
                    } else {
                        sync::to_device(dev, &local, &remote, &opts, None, Some(&mut report))
                    }
                }
                None => {
                    self.send(Update::Failed {
                        what: "Sync failed".to_string(),
                        message: "not connected".to_string(),
                        connected: false,
                    });
                    return;
                }
            };
            match result {
                Ok(o) => o,
                Err(e) => {
                    let message = e.to_string();
                    let recovered =
                        matches!(self.device.as_mut().map(|d| d.recover()), Some(Ok(())));
                    if !recovered {
                        self.drop_device();
                    }
                    self.send(Update::Failed {
                        what: "Sync failed".to_string(),
                        message,
                        connected: recovered,
                    });
                    return;
                }
            }
        };

        let cancelled = outcome.cancelled;
        self.send(Update::SyncDone {
            outcome: Box::new(outcome),
            preview,
            from_device,
        });
        if cancelled {
            self.send(Update::Status("Sync cancelled".to_string()));
        }
        if !preview {
            self.refresh_tree();
        }
    }

    /// Forward anything the board printed to stderr without raising.
    fn report_device_notices(&mut self) {
        let notices = match self.device.as_mut() {
            Some(dev) => dev.take_remote_warnings(),
            None => return,
        };
        for notice in notices {
            self.send(Update::Notice(notice));
        }
    }
}

/// One line of live sync progress.
fn sync_action_line(action: &sync::SyncAction) -> String {
    let target = action
        .remote
        .as_deref()
        .or(action.local.as_deref())
        .unwrap_or("");
    format!("{} {}", action.op, target)
}

/// Build the display tree from one flat walk.
///
/// Entries arrive relative to `root`, a parent always before its children.
fn tree_from_entries(entries: &[micropython::RemoteTreeEntry], root: &str) -> Vec<RemoteNode> {
    fn children(
        entries: &[micropython::RemoteTreeEntry],
        root: &str,
        parent: &str,
    ) -> Vec<RemoteNode> {
        let mut nodes: Vec<RemoteNode> = entries
            .iter()
            .filter(|e| remote_parent(&e.path) == parent)
            .map(|e| RemoteNode {
                name: remote_leaf(&e.path).to_string(),
                path: join_remote_path(root, &e.path),
                is_dir: e.is_dir,
                children: if e.is_dir {
                    children(entries, root, &e.path)
                } else {
                    Vec::new()
                },
            })
            .collect();
        // Directories first, then files, each alphabetically.
        nodes.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        nodes
    }

    children(entries, root, "")
}

struct GuiApp {
    // Connection
    available_ports: Vec<PortEntry>,
    selected_port: Option<String>,
    /// The device thread. All serial I/O happens over there, so a transfer
    /// never stops this one from drawing.
    link: DeviceLink,
    connected: bool,
    /// A connect is queued but has not reported back yet, so a second action
    /// does not queue a second one.
    connecting: bool,
    /// Label of the job in flight, and what cancelling it would mean.
    busy: Option<(String, JobKind)>,
    connection_error: Option<String>,

    // Device filesystem
    remote_tree: Vec<RemoteNode>,
    selected_remote_path: Option<String>,
    selected_remote_is_dir: bool,

    // Editor
    tabs: Vec<EditorTab>,
    active_tab: usize,

    // Bottom dock: program output and the REPL
    last_output: Option<ExecResult>,
    output_open: bool,
    output_filter: OutputFilter,
    dock_tab: DockTab,
    repl: ReplPanel,

    // Transient interaction state
    last_status: Option<String>,
    /// Inline rename in the tree: (original path, edited leaf name).
    renaming: Option<(String, String)>,
    /// Inline "new file" in the tree: (parent directory, edited name).
    creating: Option<(String, String)>,
    /// Pending delete: (path, is_dir, delete contents too).
    confirm_delete: Option<(String, bool, bool)>,
    sync_panel: SyncPanel,
    update_panel: UpdatePanel,
}

impl GuiApp {
    /// Build the app and start its device thread.
    ///
    /// The thread needs the `Context` so it can wake an idle window when an
    /// update arrives; that is why this is not `Default`.
    fn new(ctx: &egui::Context) -> Self {
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
            skip_hidden: prefs.sync_skip_hidden,
            skip_markdown: prefs.sync_skip_markdown,
            ..SyncPanel::default()
        };
        Self {
            available_ports,
            selected_port,
            link: DeviceLink::spawn(ctx.clone()),
            connected: false,
            connecting: false,
            busy: None,
            connection_error: None,
            remote_tree: Vec::new(),
            selected_remote_path: None,
            selected_remote_is_dir: false,
            tabs: vec![EditorTab::starter()],
            active_tab: 0,
            last_output: None,
            output_open: false,
            output_filter: OutputFilter::All,
            dock_tab: DockTab::Output,
            repl: ReplPanel::default(),
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
    /// Queue a connect ahead of the job about to be sent, if one is needed.
    ///
    /// Jobs run in order, so an action taken while disconnected simply lands
    /// behind the connect it implies.
    fn ensure_connected(&mut self) {
        if self.connected || self.connecting {
            return;
        }
        let Some(port) = self.selected_port.clone() else {
            self.connection_error = Some("No port selected".to_string());
            return;
        };
        self.connecting = true;
        self.link.send(Job::Connect { port });
    }

    /// Send a job, connecting first if that has not happened yet.
    ///
    /// Returns whether the job was actually queued: a caller that has already
    /// shown something on screen (the REPL echoes its entry immediately) has
    /// to undo that when there is no connection to send it to.
    fn device_job(&mut self, job: Job) -> bool {
        self.ensure_connected();
        if !self.connected && !self.connecting {
            // No port selected; `ensure_connected` has already said so.
            return false;
        }
        self.link.send(job);
        true
    }

    fn connect_and_list(&mut self) {
        // Connecting refreshes the tree on the worker side.
        self.ensure_connected();
    }

    fn disconnect(&mut self) {
        self.link.send(Job::Disconnect);
    }

    /// Interrupt whatever the board is running.
    ///
    /// Goes down the interrupt handle rather than the job queue: the worker
    /// is usually blocked reading the output of the very program being
    /// stopped, so a queued job would not be looked at until it finished.
    fn stop_program(&mut self) {
        match self.link.interrupt() {
            Ok(()) => {
                self.last_status = Some("Sent Ctrl-C to the board".to_string());
                self.link.send(Job::Resync);
            }
            Err(e) => {
                self.connection_error = Some(format!("Could not interrupt the device: {e}"));
            }
        }
    }

    /// Stop the job in flight, in whatever way that job can be stopped.
    fn cancel_current(&mut self) {
        match self.busy.as_ref().map(|(_, kind)| *kind) {
            // Board-side code only stops for Ctrl-C.
            Some(JobKind::Exec) => self.stop_program(),
            Some(JobKind::Cancellable) => {
                self.link.request_cancel();
                self.last_status = Some("Stopping after the current file".to_string());
            }
            _ => {}
        }
    }

    fn flash_active_as_main(&mut self) {
        let text = self.tabs[self.active_tab].text.clone();
        self.device_job(Job::Flash { text });
    }

    fn run_main_script(&mut self) {
        self.device_job(Job::RunMain);
    }

    fn run_current_script(&mut self) {
        let tab = &self.tabs[self.active_tab];
        let job = Job::RunScript {
            path: tab.path.clone(),
            text: tab.text.clone(),
            save_first: tab.path.is_some() && tab.dirty,
        };
        self.device_job(job);
    }

    /// Show a result in the output dock, opening it if it was collapsed.
    fn set_output(&mut self, res: ExecResult) {
        self.last_output = Some(res);
        self.output_open = true;
        self.dock_tab = DockTab::Output;
    }

    /// Bring the REPL up and put the caret in it.
    fn open_repl(&mut self) {
        self.output_open = true;
        self.dock_tab = DockTab::Repl;
        self.repl.focus_input = true;
    }

    /// Send whatever is in the REPL input to the board.
    fn submit_repl(&mut self) {
        let source = self.repl.input.trim_end().to_string();
        self.repl.input.clear();
        self.repl.history_pos = None;
        if source.trim().is_empty() {
            return;
        }
        self.repl.remember(&source);
        // Echo the entry immediately: the result arrives later, and a prompt
        // that swallowed the line until then would feel broken.
        self.repl.push(ReplEntry {
            source: Some(source.clone()),
            stdout: String::new(),
            stderr: String::new(),
            pending: true,
        });
        if !self.device_job(Job::RunRepl { source }) {
            let why = self
                .connection_error
                .clone()
                .unwrap_or_else(|| "No device connected".to_string());
            self.settle_pending_repl(&why);
        }
    }

    fn refresh_remote_tree(&mut self) {
        self.device_job(Job::RefreshTree);
    }

    fn open_path(&mut self, path: String) {
        self.device_job(Job::Open { path });
    }

    fn save_current(&mut self) {
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
        self.device_job(Job::Save { path, text });
    }

    fn create_file(&mut self, path: String) {
        self.device_job(Job::Create { path });
    }

    fn delete_path(&mut self, path: &str, is_dir: bool, recursive: bool) {
        self.device_job(Job::Delete {
            path: path.to_string(),
            is_dir,
            recursive,
        });
    }

    fn rename_path(&mut self, old_path: &str, new_path: &str) {
        self.device_job(Job::Rename {
            old: old_path.to_string(),
            new: new_path.to_string(),
        });
    }

    /// Apply everything the device thread has reported since the last frame.
    fn poll_device(&mut self) {
        while let Ok(update) = self.link.updates.try_recv() {
            self.apply_update(update);
        }
    }

    fn apply_update(&mut self, update: Update) {
        match update {
            Update::Started(label, kind) => self.busy = Some((label, kind)),
            Update::Finished => self.busy = None,
            Update::Connected(port) => {
                self.connected = true;
                self.connecting = false;
                self.connection_error = None;
                self.last_status = Some(format!("Connected to {}", short_port(&port)));
            }
            Update::Disconnected => {
                self.connected = false;
                self.connecting = false;
                self.remote_tree.clear();
                self.last_status = Some("Disconnected".to_string());
            }
            Update::Tree(nodes) => self.remote_tree = nodes,
            Update::Opened { path, text } => {
                self.adopt_opened_file(path, text);
            }
            Update::Saved { path } => {
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(path.as_str()) {
                        tab.dirty = false;
                    }
                }
                self.connection_error = None;
                self.last_status = Some(format!("Saved {path}"));
            }
            Update::Created { path } => {
                self.connection_error = None;
                self.last_status = Some(format!("Created {path}"));
                self.tabs.push(EditorTab::from_remote(path, String::new()));
                self.active_tab = self.tabs.len() - 1;
            }
            Update::Deleted { path } => {
                self.last_status = Some(format!("Deleted {path}"));
                // Keep the buffer but unbind it: the file is gone, and its
                // contents may be the only surviving copy.
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(path.as_str()) {
                        tab.path = None;
                        tab.dirty = true;
                    }
                }
                if self.selected_remote_path.as_deref() == Some(path.as_str()) {
                    self.selected_remote_path = None;
                }
            }
            Update::Renamed { old, new } => {
                self.last_status = Some(format!("Renamed to {new}"));
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(old.as_str()) {
                        tab.path = Some(new.clone());
                    }
                }
                if self.selected_remote_path.as_deref() == Some(old.as_str()) {
                    self.selected_remote_path = Some(new);
                }
            }
            Update::Output(res) => self.set_output(res),
            Update::Repl {
                source,
                stdout,
                stderr,
            } => self.settle_repl_entry(&source, stdout, stderr),
            Update::Status(text) => self.last_status = Some(text),
            Update::SyncProgress(action) => self.sync_panel.live.push(action),
            Update::SyncDone {
                outcome,
                preview,
                from_device,
            } => {
                let copied = outcome.count(if from_device { "download" } else { "upload" });
                let deleted = outcome.count("delete_remote_file")
                    + outcome.count("delete_remote_dir")
                    + outcome.count("delete_local_file")
                    + outcome.count("delete_local_dir");
                self.last_status = Some(if outcome.cancelled {
                    format!("Sync cancelled after {copied} copied, {deleted} deleted")
                } else if preview {
                    format!("Preview: {copied} to copy, {deleted} to delete")
                } else {
                    format!("Synced: {copied} copied, {deleted} deleted")
                });
                self.sync_panel.last_was_preview = preview;
                self.sync_panel.last = Some(*outcome);
                self.sync_panel.live.clear();
                self.connection_error = None;
                self.save_prefs();
            }
            Update::Notice(text) => {
                // The board said something without raising. Keep it where a
                // session's other output lives rather than dropping it.
                self.repl.push(ReplEntry {
                    source: None,
                    stdout: format!("device: {text}"),
                    stderr: String::new(),
                    pending: false,
                });
                self.last_status = Some(format!("Device: {text}"));
            }
            Update::Failed {
                what,
                message,
                connected,
            } => {
                self.connection_error = Some(format!("{what}: {message}"));
                self.connected = connected;
                self.connecting = false;
                if !connected {
                    self.remote_tree.clear();
                }
                self.last_status = Some(if connected {
                    format!("{what} (connection resynchronised)")
                } else {
                    format!("{what} (disconnected)")
                });
                // A REPL entry still waiting on the board would otherwise sit
                // there marked pending for ever.
                self.settle_pending_repl(&message);
            }
        }
    }

    /// Put an opened file in front of the user.
    fn adopt_opened_file(&mut self, path: String, text: String) {
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
            // Reuse a pristine untitled tab instead of stacking up empty
            // buffers.
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

    /// Fill in the result of the REPL entry that was waiting for it.
    fn settle_repl_entry(&mut self, source: &str, stdout: String, stderr: String) {
        self.last_status = Some(if stderr.trim().is_empty() {
            "REPL entry finished".to_string()
        } else {
            "REPL entry raised an exception".to_string()
        });
        if let Some(entry) = self
            .repl
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.pending && e.source.as_deref() == Some(source))
        {
            entry.stdout = stdout;
            entry.stderr = stderr;
            entry.pending = false;
        }
    }

    /// Close off any REPL entry left waiting when something went wrong.
    fn settle_pending_repl(&mut self, message: &str) {
        for entry in self.repl.entries.iter_mut().filter(|e| e.pending) {
            entry.pending = false;
            if entry.stderr.trim().is_empty() {
                entry.stderr = message.to_string();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl GuiApp {
    /// Keyboard shortcuts, consumed before any widget can swallow the key.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (save, run, close_tab, toggle_output, new_tab, repl) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::R),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::W),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::J),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::N),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::L),
            )
        });

        // The device shortcuts mirror the toolbar buttons, including being
        // unavailable while a job is in flight.
        let idle = self.busy.is_none();
        if save && idle {
            self.save_current();
        }
        if run && idle {
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
        if repl {
            self.open_repl();
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
        // A job already in flight disables the buttons that would queue
        // another one — except Stop, which exists precisely for then.
        let idle = self.busy.is_none();
        ui.horizontal(|ui| {
            // Colour carries the connection state, so it reads without
            // parsing any text.
            let (dot, tint) = if self.connected {
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

            if self.connected {
                if ui
                    .add_enabled(idle, egui::Button::new("Disconnect"))
                    .clicked()
                {
                    self.disconnect();
                }
            } else if ui
                .add_enabled(idle && !self.connecting, egui::Button::new("Connect"))
                .clicked()
            {
                self.connect_and_list();
            }

            ui.separator();

            let usable = (self.connected || self.selected_port.is_some()) && idle;
            if ui
                .add_enabled(usable, egui::Button::new(sym::RUN))
                .on_hover_text("Run the active tab on the device   ⌘R")
                .clicked()
            {
                self.run_current_script();
            }
            if ui
                .add_enabled(self.connected, egui::Button::new(sym::STOP))
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
                .add_enabled(usable, egui::Button::new(sym::REPL))
                .on_hover_text("Type Python straight at the board   ⌘L")
                .clicked()
            {
                self.open_repl();
            }
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
                let can_add = self.connected;
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
                    egui::RichText::new(if self.connected {
                        "No files on device"
                    } else {
                        "Not connected"
                    })
                    .color(pal.dim),
                );
                ui.add_space(6.0);
                if !self.connected && ui.button("Connect").clicked() {
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
                TreeAction::Delete(path, is_dir) => {
                    self.confirm_delete = Some((path, is_dir, false))
                }
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

    /// The bottom dock: a tab strip over the output transcript and the REPL.
    fn dock(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let has_err = self
            .last_output
            .as_ref()
            .is_some_and(|o| !o.stderr.trim().is_empty());
        let on_repl = self.dock_tab == DockTab::Repl;

        ui.horizontal(|ui| {
            let repl_clicked = {
                ui.selectable_value(&mut self.dock_tab, DockTab::Output, "Output");
                ui.selectable_value(&mut self.dock_tab, DockTab::Repl, sym::REPL)
                    .clicked()
            };
            // Switching to the REPL by hand should leave the caret ready, the
            // same as arriving there by shortcut.
            if repl_clicked {
                self.repl.focus_input = true;
            }

            if !on_repl {
                ui.add_space(4.0);
                ui.selectable_value(&mut self.output_filter, OutputFilter::All, "All");
                ui.selectable_value(&mut self.output_filter, OutputFilter::Stdout, "stdout");
                let stderr_label = if has_err {
                    egui::RichText::new("stderr").color(pal.err)
                } else {
                    egui::RichText::new("stderr")
                };
                ui.selectable_value(&mut self.output_filter, OutputFilter::Stderr, stderr_label);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Words, not symbols: egui's default font has no glyph for
                // ⌫ / ⌄ / ⌃, so those drew as empty boxes.
                if ui
                    .small_button("Hide")
                    .on_hover_text("Hide the dock   ⌘J")
                    .clicked()
                {
                    self.output_open = false;
                }
                let clear = if on_repl {
                    ui.small_button("Clear")
                        .on_hover_text("Clear the REPL transcript (history is kept)")
                } else {
                    ui.small_button("Clear").on_hover_text("Clear the output")
                };
                if clear.clicked() {
                    if on_repl {
                        self.repl.entries.clear();
                    } else {
                        self.last_output = None;
                    }
                }
            });
        });
        ui.separator();

        match self.dock_tab {
            DockTab::Output => self.output_view(ui, pal),
            DockTab::Repl => self.repl_view(ui, pal),
        }
    }

    /// Transcript of the last run.
    fn output_view(&mut self, ui: &mut egui::Ui, pal: &Palette) {
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

    /// The REPL: a transcript above, a prompt pinned to the bottom.
    fn repl_view(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        // The prompt is a panel inside the dock so it keeps its place while
        // the transcript takes whatever height is left.
        egui::Panel::bottom("repl_prompt")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 4)))
            .show(ui, |ui| self.repl_prompt(ui, pal));

        egui::ScrollArea::both()
            .id_salt("repl_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.repl.entries.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "Type Python and press Enter to run it on the board. \
                             Shift-Enter for a new line, Up and Down for history.",
                        )
                        .color(pal.dim),
                    );
                    return;
                }

                for entry in &self.repl.entries {
                    if let Some(source) = &entry.source {
                        ui.label(
                            egui::RichText::new(prompt_block(source))
                                .font(code_font())
                                .color(pal.accent),
                        );
                    }
                    if entry.pending {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(10.0));
                            ui.label(
                                egui::RichText::new("running on the board")
                                    .font(code_font())
                                    .color(pal.dim),
                            );
                        });
                    }
                    if !entry.stdout.trim().is_empty() {
                        ui.label(
                            egui::RichText::new(entry.stdout.trim_end())
                                .font(code_font())
                                .color(pal.ident),
                        );
                    }
                    if !entry.stderr.trim().is_empty() {
                        ui.label(
                            egui::RichText::new(entry.stderr.trim_end())
                                .font(code_font())
                                .color(pal.err),
                        );
                    }
                    ui.add_space(3.0);
                }
            });
    }

    /// The input line, and the keys that only mean something while it has
    /// focus.
    fn repl_prompt(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let id = ReplPanel::input_id();
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        // A multi-line entry needs the arrow keys for the caret, so history
        // recall only claims them while the buffer is a single line.
        let one_line = !self.repl.input.contains('\n');

        // Claimed before the field is built, so the text edit never sees them:
        // otherwise Enter would insert a newline as well as submitting.
        let (submit, older, newer) = if focused {
            ui.ctx().input_mut(|i| {
                (
                    take_bare_key(i, egui::Key::Enter),
                    one_line && take_bare_key(i, egui::Key::ArrowUp),
                    one_line && take_bare_key(i, egui::Key::ArrowDown),
                )
            })
        } else {
            (false, false, false)
        };

        if older {
            self.repl.recall_older();
        }
        if newer {
            self.repl.recall_newer();
        }

        ui.horizontal_top(|ui| {
            ui.label(
                egui::RichText::new(">>>")
                    .font(code_font())
                    .color(pal.accent),
            );
            let field = egui::TextEdit::multiline(&mut self.repl.input)
                .id(id)
                .font(code_font())
                .desired_rows(1)
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .hint_text("print('hello')");
            let response = ui.add(field);
            if self.repl.focus_input {
                response.request_focus();
                self.repl.focus_input = false;
            }
        });

        if submit {
            self.submit_repl();
            // Submitting should not cost the caret; the next entry usually
            // follows straight on.
            self.repl.focus_input = true;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            let (dot, tint, state) = if self.connected {
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
                // While the device thread is busy the window keeps drawing,
                // so it has to say what it is waiting for — and offer the way
                // out that suits the job.
                if let Some((label, kind)) = self.busy.clone() {
                    let stoppable = matches!(kind, JobKind::Exec | JobKind::Cancellable);
                    if stoppable
                        && ui
                            .small_button("Cancel")
                            .on_hover_text(match kind {
                                JobKind::Exec => "Interrupt the program on the board",
                                _ => "Stop after the current file",
                            })
                            .clicked()
                    {
                        self.cancel_current();
                    }
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(egui::RichText::new(label).color(pal.accent).small());
                } else if let Some(msg) = &self.last_status {
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
            sync_skip_hidden: self.sync_panel.skip_hidden,
            sync_skip_markdown: self.sync_panel.skip_markdown,
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

        let opts = sync::SyncOptions {
            delete: self.sync_panel.delete,
            dry_run: preview,
            ignore: self.sync_panel.ignore_patterns(),
            // The panel keeps no last-sync baseline, so there is nothing to
            // detect conflicts against and every copy is unconditional. The
            // CLI's workspace `sync` is the mode that tracks conflicts.
            force: true,
            // Shared with the worker so Cancel can stop a long sync between
            // files instead of the window sitting there until it ends.
            cancel: Some(Arc::clone(&self.link.cancel)),
        };

        self.sync_panel.live.clear();
        self.sync_panel.last = None;
        let job = Job::Sync {
            local,
            remote,
            opts,
            from_device: self.sync_panel.from_device,
            preview,
        };
        self.device_job(job);
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
                if ui
                    .checkbox(
                        &mut self.sync_panel.skip_hidden,
                        "Skip files and folders starting with a dot",
                    )
                    .on_hover_text("Excludes .git, .vscode, .DS_Store and the like")
                    .changed()
                {
                    self.sync_panel.last = None;
                    self.save_prefs();
                }
                if ui
                    .checkbox(&mut self.sync_panel.skip_markdown, "Skip Markdown files")
                    .on_hover_text("Excludes README.md and other .md documentation")
                    .changed()
                {
                    self.sync_panel.last = None;
                    self.save_prefs();
                }
                ui.add_space(8.0);

                // --- actions ---------------------------------------------
                let syncing = matches!(self.busy, Some((_, JobKind::Cancellable)));
                let ready = self.sync_panel.local_dir.is_some()
                    && !self.sync_panel.remote_dir.trim().is_empty()
                    && !syncing;
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
                    if syncing {
                        ui.add(egui::Spinner::new().size(14.0));
                        if ui.button("Stop").clicked() {
                            self.cancel_current();
                        }
                    }
                });

                ui.add_space(8.0);
                ui.separator();

                // --- results ---------------------------------------------
                // A sync in flight reports each decision as it makes it, so
                // the window shows the work rather than a frozen pane.
                if syncing {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} decisions so far",
                            self.sync_panel.live.len()
                        ))
                        .small()
                        .strong()
                        .color(pal.accent),
                    );
                    egui::ScrollArea::vertical()
                        .id_salt("sync_live")
                        .max_height(220.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for action in &self.sync_panel.live {
                                ui.label(
                                    egui::RichText::new(sync_action_line(action))
                                        .font(code_font())
                                        .color(pal.dim),
                                );
                            }
                        });
                    return;
                }

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

        // Everything the device thread has done since the last frame.
        self.poll_device();

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
                .show(ui, |ui| self.dock(ui, &pal));
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

        if let Some((path, is_dir, recursive)) = self.confirm_delete.clone() {
            let modal = egui::Modal::new(egui::Id::new("confirm_delete")).show(&ctx, |ui| {
                ui.set_width(330.0);
                ui.heading("Delete from device?");
                ui.add_space(8.0);
                ui.label(egui::RichText::new(&path).font(code_font()));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if is_dir {
                        "An empty directory unless you delete its contents too."
                    } else {
                        "This cannot be undone."
                    })
                    .color(pal.dim),
                );
                if is_dir {
                    ui.add_space(6.0);
                    let mut recurse = recursive;
                    if ui
                        .checkbox(&mut recurse, "Delete everything inside it")
                        .changed()
                    {
                        self.confirm_delete = Some((path.clone(), is_dir, recurse));
                    }
                }
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
                            self.delete_path(&path, is_dir, recursive);
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
        Box::new(|cc| Ok(Box::new(GuiApp::new(&cc.egui_ctx)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_checkboxes_exclude_only_what_they_name() {
        use rupico::sync::path_is_ignored;
        use std::path::Path;

        let mut panel = SyncPanel {
            skip_hidden: true,
            skip_markdown: true,
            ..SyncPanel::default()
        };
        let pats = panel.ignore_patterns();

        // Dotted names, at the root and nested, files and directories.
        assert!(path_is_ignored(Path::new(".DS_Store"), &pats));
        assert!(path_is_ignored(Path::new(".vscode"), &pats));
        assert!(path_is_ignored(Path::new(".vscode/settings.json"), &pats));
        assert!(path_is_ignored(Path::new("lib/.cache/x.py"), &pats));
        assert!(path_is_ignored(Path::new("README.md"), &pats));
        assert!(path_is_ignored(Path::new("docs/guide.md"), &pats));

        // A dot inside a name is not a dot at the start of one, and `.md`
        // must not match a file that merely contains those letters.
        assert!(!path_is_ignored(Path::new("main.py"), &pats));
        assert!(!path_is_ignored(Path::new("my.config.py"), &pats));
        assert!(!path_is_ignored(Path::new("lib/weather.py"), &pats));
        assert!(!path_is_ignored(Path::new("md.py"), &pats));
        assert!(!path_is_ignored(Path::new("notes.markdown"), &pats));

        // Each checkbox acts alone.
        panel.skip_markdown = false;
        let hidden_only = panel.ignore_patterns();
        assert!(path_is_ignored(Path::new(".env"), &hidden_only));
        assert!(!path_is_ignored(Path::new("README.md"), &hidden_only));

        panel.skip_hidden = false;
        panel.skip_markdown = true;
        let md_only = panel.ignore_patterns();
        assert!(path_is_ignored(Path::new("README.md"), &md_only));
        assert!(!path_is_ignored(Path::new(".env"), &md_only));
    }

    #[test]
    fn no_skip_options_means_no_extra_patterns() {
        // Both off must leave the engine's built-ins exactly as they were.
        assert!(SyncPanel::default().ignore_patterns().is_empty());
    }

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

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn only_a_bare_keypress_is_claimed_from_the_repl_input() {
        // Regression: `consume_key(Modifiers::NONE, ..)` matches modifiers
        // *logically*, which ignores Shift — so Shift-Enter submitted the
        // entry instead of breaking the line, and never reached the field.
        let mut input = egui::InputState::default();
        input.events = vec![
            key_event(egui::Key::Enter, egui::Modifiers::SHIFT),
            key_event(egui::Key::ArrowUp, egui::Modifiers::SHIFT),
        ];
        assert!(!take_bare_key(&mut input, egui::Key::Enter));
        assert!(!take_bare_key(&mut input, egui::Key::ArrowUp));
        assert_eq!(input.events.len(), 2, "modified keys stay for the field");

        input.events = vec![key_event(egui::Key::Enter, egui::Modifiers::NONE)];
        assert!(take_bare_key(&mut input, egui::Key::Enter));
        assert!(
            input.events.is_empty(),
            "a claimed key must not reach the field"
        );
    }

    fn tree_entry(path: &str, is_dir: bool) -> micropython::RemoteTreeEntry {
        serde_json::from_value(serde_json::json!({
            "p": path, "d": is_dir, "s": 0, "h": null
        }))
        .expect("entry parses")
    }

    #[test]
    fn the_file_rail_is_built_from_one_flat_walk() {
        // The rail used to cost a `list_dir` round trip per directory. It now
        // shares the single-round-trip walk, so the flat result has to be
        // regrouped into the tree the rail draws.
        let entries = vec![
            tree_entry("main.py", false),
            tree_entry("lib", true),
            tree_entry("lib/mod.py", false),
            tree_entry("lib/deep", true),
            tree_entry("lib/deep/x.py", false),
        ];
        let tree = tree_from_entries(&entries, "/");

        // Directories first, then files, each alphabetically.
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "lib");
        assert!(tree[0].is_dir);
        assert_eq!(tree[1].name, "main.py");
        assert_eq!(tree[1].path, "/main.py");

        let lib = &tree[0];
        assert_eq!(lib.children.len(), 2, "one flat walk, all depths");
        assert_eq!(lib.children[0].name, "deep");
        assert_eq!(lib.children[0].children[0].path, "/lib/deep/x.py");
        assert_eq!(lib.children[1].path, "/lib/mod.py");
    }

    #[test]
    fn tree_paths_hang_off_the_root_they_were_walked_from() {
        let tree = tree_from_entries(&[tree_entry("mod.py", false)], "/lib");
        assert_eq!(tree[0].path, "/lib/mod.py");
    }

    #[test]
    fn only_board_side_work_is_stopped_with_ctrl_c() {
        // Cancelling has to mean different things: a program on the board
        // only stops for Ctrl-C, while a sync can be asked to stop between
        // files, and a one-shot listing is not worth interrupting at all.
        assert_eq!(Job::RunRepl { source: "x".into() }.kind(), JobKind::Exec);
        assert_eq!(
            Job::RunScript {
                path: None,
                text: String::new(),
                save_first: false
            }
            .kind(),
            JobKind::Exec
        );
        assert_eq!(Job::RunMain.kind(), JobKind::Exec);
        assert_eq!(
            Job::Sync {
                local: PathBuf::from("/tmp"),
                remote: "/".into(),
                opts: sync::SyncOptions::default(),
                from_device: false,
                preview: false,
            }
            .kind(),
            JobKind::Cancellable
        );
        assert_eq!(Job::RefreshTree.kind(), JobKind::Quick);
    }

    /// An app instance with a device thread that will never be given work.
    fn headless_app() -> GuiApp {
        GuiApp::new(&egui::Context::default())
    }

    #[test]
    fn a_failure_never_leaves_a_repl_entry_waiting_for_ever() {
        // The prompt echoes an entry the moment it is submitted, so anything
        // that stops its result from arriving has to close the entry off.
        let mut app = headless_app();
        app.repl.push(ReplEntry {
            source: Some("machine.freq()".to_string()),
            stdout: String::new(),
            stderr: String::new(),
            pending: true,
        });

        app.apply_update(Update::Failed {
            what: "REPL error".to_string(),
            message: "execution timed out".to_string(),
            connected: false,
        });

        let entry = app.repl.entries.last().expect("the entry is still there");
        assert!(!entry.pending, "no entry may stay pending after a failure");
        assert_eq!(entry.stderr, "execution timed out");
        assert!(!app.connected, "a dropped connection shows as disconnected");
        assert!(app.remote_tree.is_empty(), "a stale tree is not kept");
    }

    #[test]
    fn a_result_fills_in_the_entry_that_was_waiting_for_it() {
        let mut app = headless_app();
        app.repl.push(ReplEntry {
            source: Some("6 * 7".to_string()),
            stdout: String::new(),
            stderr: String::new(),
            pending: true,
        });

        app.apply_update(Update::Repl {
            source: "6 * 7".to_string(),
            stdout: "42\n".to_string(),
            stderr: String::new(),
        });

        let entry = app.repl.entries.last().expect("entry");
        assert!(!entry.pending);
        assert_eq!(entry.stdout, "42\n");
        assert_eq!(
            app.repl.entries.len(),
            1,
            "the echo is filled in, not duplicated"
        );
    }

    #[test]
    fn an_opened_file_reuses_its_tab_rather_than_stacking_up_copies() {
        let mut app = headless_app();
        // The starter buffer is scratch, so the first opened file takes it.
        app.apply_update(Update::Opened {
            path: "/main.py".to_string(),
            text: "print(1)\n".to_string(),
        });
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].path.as_deref(), Some("/main.py"));

        app.apply_update(Update::Opened {
            path: "/lib/mod.py".to_string(),
            text: "X = 1\n".to_string(),
        });
        assert_eq!(app.tabs.len(), 2);

        // Opening the same file again refreshes its tab instead of adding one.
        app.apply_update(Update::Opened {
            path: "/main.py".to_string(),
            text: "print(2)\n".to_string(),
        });
        assert_eq!(app.tabs.len(), 2, "no duplicate tab for the same path");
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.tabs[0].text, "print(2)\n");
        assert!(!app.tabs[0].dirty, "a freshly loaded buffer is clean");
    }

    #[test]
    fn a_deleted_file_keeps_its_buffer_but_loses_its_path() {
        // The device copy is gone, so the open buffer may be the only one
        // left; it must not be silently re-saveable to a file that no longer
        // exists either.
        let mut app = headless_app();
        app.apply_update(Update::Opened {
            path: "/doomed.py".to_string(),
            text: "keep me".to_string(),
        });

        app.apply_update(Update::Deleted {
            path: "/doomed.py".to_string(),
        });

        assert_eq!(app.tabs[0].text, "keep me");
        assert_eq!(app.tabs[0].path, None);
        assert!(app.tabs[0].dirty);
    }

    #[test]
    fn a_repl_entry_with_nowhere_to_go_does_not_spin_for_ever() {
        // The prompt echoes the entry before the job is sent, so a submit
        // with no port selected has to close its own entry off.
        let mut app = headless_app();
        app.selected_port = None;
        app.repl.input = "1 + 1".to_string();

        app.submit_repl();

        let entry = app.repl.entries.last().expect("the entry was echoed");
        assert!(!entry.pending, "nothing will ever answer it");
        assert!(!entry.stderr.is_empty(), "and it says why");
    }

    #[test]
    fn the_ui_tracks_what_the_device_thread_is_doing() {
        let mut app = headless_app();
        assert!(app.busy.is_none());

        app.apply_update(Update::Started("Syncing".to_string(), JobKind::Cancellable));
        assert!(matches!(app.busy, Some((_, JobKind::Cancellable))));

        app.apply_update(Update::Finished);
        assert!(app.busy.is_none(), "the buttons come back when work ends");
    }

    #[test]
    fn a_prompt_block_marks_continuation_lines() {
        // A pasted block has to read as one submission, or the transcript
        // looks like several separate commands.
        assert_eq!(prompt_block("1 + 1"), ">>> 1 + 1");
        assert_eq!(
            prompt_block("for i in range(2):\n    print(i)"),
            ">>> for i in range(2):\n...     print(i)"
        );
        assert_eq!(prompt_block(""), "");
    }

    #[test]
    fn repl_history_walks_back_and_returns_to_a_blank_line() {
        let mut repl = ReplPanel::default();
        for entry in ["a = 1", "print(a)"] {
            repl.input = entry.to_string();
            repl.remember(entry);
            repl.input.clear();
        }

        repl.recall_older();
        assert_eq!(repl.input, "print(a)");
        repl.recall_older();
        assert_eq!(repl.input, "a = 1");
        // Past the oldest entry, Up holds rather than wrapping around to the
        // newest, which would silently re-run something else.
        repl.recall_older();
        assert_eq!(repl.input, "a = 1");

        repl.recall_newer();
        assert_eq!(repl.input, "print(a)");
        // Down past the newest entry comes back to an empty prompt.
        repl.recall_newer();
        assert_eq!(repl.input, "");
        repl.recall_newer();
        assert_eq!(repl.input, "");
    }

    #[test]
    fn repl_history_skips_an_immediate_repeat() {
        let mut repl = ReplPanel::default();
        repl.remember("print(1)");
        repl.remember("print(1)");
        repl.remember("print(2)");
        repl.remember("print(1)");
        assert_eq!(repl.history, vec!["print(1)", "print(2)", "print(1)"]);
    }

    #[test]
    fn repl_scrollback_is_bounded() {
        // A loop left printing must not grow the transcript without limit.
        let mut repl = ReplPanel::default();
        for i in 0..MAX_REPL_ENTRIES + 25 {
            repl.push(ReplEntry {
                source: Some(format!("print({i})")),
                stdout: String::new(),
                stderr: String::new(),
                pending: false,
            });
        }
        assert_eq!(repl.entries.len(), MAX_REPL_ENTRIES);
        // The oldest go, not the newest.
        assert_eq!(
            repl.entries.last().and_then(|e| e.source.clone()),
            Some(format!("print({})", MAX_REPL_ENTRIES + 24))
        );
        assert_eq!(
            repl.entries.first().and_then(|e| e.source.clone()),
            Some("print(25)".to_string())
        );
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
