use serialport::SerialPort;
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use thiserror::Error;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

const CTRL_A: u8 = 0x01; // enter raw REPL
const CTRL_B: u8 = 0x02; // exit raw REPL
const CTRL_C: u8 = 0x03; // interrupt
const CTRL_D: u8 = 0x04; // end of code / soft reboot depending on mode

#[derive(Debug, Error)]
pub enum MicroPythonError {
    #[error("serial error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("handshake with device timed out (entering raw REPL)")]
    HandshakeTimeout,

    #[error("execution timed out while waiting for raw REPL result")]
    ExecTimeout,

    #[error("remote error: {0}")]
    Remote(String),

    #[error("protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, MicroPythonError>;

/// A single file or directory entry reported by the remote filesystem.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Optional modification time in seconds since the Unix epoch, if
    /// reported by the device. Some ports may omit or zero this field.
    pub modified: Option<u64>,
}

/// A file or directory in a recursive, content-hashed listing of the
/// remote filesystem, as returned by [`MicroPythonDevice::list_tree_hashed`].
///
/// Paths are relative to the listing root and use `/` separators. `hash` is
/// the lowercase hex sha256 of the file contents, or `None` for directories
/// and for devices whose firmware lacks a sha256 implementation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoteTreeEntry {
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "d")]
    pub is_dir: bool,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "h")]
    pub hash: Option<String>,
    /// Device mtime, only when [`TreeOptions::mtimes`] asked for it. The
    /// board usually has no clock, so this is for display, never for diffing.
    #[serde(rename = "m", default)]
    pub modified: Option<u64>,
}

/// What a tree walk should collect beyond names, types and sizes.
///
/// Both extras cost something on the device — hashes read every file, mtimes
/// widen the JSON that a board with ~192 KB of RAM has to build — so each
/// caller asks for only what it will use.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeOptions {
    /// sha256 of every file, for content-based diffing.
    pub hashes: bool,
    /// Modification times, where the filesystem reports them.
    pub mtimes: bool,
}

impl TreeOptions {
    /// Names, types and sizes only: no file is read, nothing is hashed.
    pub fn metadata_only() -> Self {
        Self::default()
    }

    /// What sync needs: a content hash per file.
    pub fn hashed() -> Self {
        Self {
            hashes: true,
            mtimes: false,
        }
    }
}

/// A writer that can interrupt the device from another thread.
///
/// Obtained from [`MicroPythonDevice::interrupt_handle`]. The interrupted
/// exec returns normally on the owning thread, with `KeyboardInterrupt` in
/// its stderr — the connection stays in raw REPL and usable.
pub struct InterruptHandle {
    port: Box<dyn SerialPort>,
}

impl InterruptHandle {
    /// Send Ctrl-C to whatever is running on the device.
    pub fn interrupt(&mut self) -> Result<()> {
        self.port.write_all(&[CTRL_C])?;
        self.port.flush()?;
        Ok(())
    }
}

/// What a recursive delete removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub files: usize,
    pub dirs: usize,
}

/// Result of executing code in raw REPL mode.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
}

/// High-level handle to a MicroPython board speaking the raw REPL protocol over serial.
pub struct MicroPythonDevice {
    port: Box<dyn SerialPort>,
    /// Timeout used while entering raw REPL. Always finite so a dead or
    /// non-MicroPython device fails fast.
    handshake_timeout: Duration,
    /// Idle timeout for reads during execution and file transfers. The
    /// deadline resets whenever data arrives, so long transfers do not time
    /// out as long as the device keeps sending. `None` means wait forever.
    read_timeout: Option<Duration>,
    /// Buffered bytes that have been read from the serial port but not yet
    /// consumed by the protocol parser.
    rx_buf: Vec<u8>,
    /// Whether this connection has successfully negotiated raw-paste support.
    ///
    /// - `None` means we have not yet attempted to use raw-paste.
    /// - `Some(true)` means the device supports raw-paste and we will try to
    ///   use it for subsequent execs.
    /// - `Some(false)` means the device does not support raw-paste and we
    ///   should always fall back to classic raw-REPL execution.
    raw_paste_supported: Option<bool>,
    /// Device-side stderr from helper snippets that did not look like a
    /// raised exception. Kept rather than dropped so a front end can show
    /// the board's own warnings; see `take_remote_warnings`.
    remote_warnings: Vec<String>,
}

/// How many non-fatal device warnings to keep before dropping the oldest.
/// A board printing on every helper call must not grow this without bound.
const MAX_REMOTE_WARNINGS: usize = 16;

/// Does device-side stderr describe a raised exception, or is it just
/// something the board printed?
///
/// Raw REPL puts *everything* a snippet sends to stderr into the same frame,
/// so treating any stderr at all as failure turned a board that warns during
/// `os.listdir` into a failed `ls`. MicroPython prints a traceback for every
/// uncaught exception, and that — plus a bare `SomeError:` line for the
/// firmware that prints one without a traceback — is the signal to key on.
fn stderr_is_fatal(stderr: &str) -> bool {
    if stderr.contains("Traceback (most recent call last)") {
        return true;
    }
    stderr.lines().any(|line| {
        let line = line.trim();
        let Some((head, _)) = line.split_once(':') else {
            return line == "KeyboardInterrupt";
        };
        !head.is_empty()
            && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && (head.ends_with("Error")
                || head.ends_with("Exception")
                || head == "KeyboardInterrupt")
    })
}

impl MicroPythonDevice {
    /// Escape a Rust string so it can be safely embedded inside a single-
    /// quoted Python string literal.
    fn py_escape_single_quoted(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 8);
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out
    }

    /// Wrap a helper program so it leaves nothing in the user's namespace.
    ///
    /// Raw REPL execs at module level, so every temporary a helper binds —
    /// `p`, `f`, `src`, even the modules it imports — lands in the same
    /// `__main__` that the user's script and the REPL prompt then see. A
    /// script whose own first line is `src = open(...)` would find someone
    /// else's `src` already there, left by whatever rupico did last.
    ///
    /// Running the body inside a function makes all of it function-local. The
    /// one name that remains is deleted whether the body raised or not.
    fn scoped(body: &str) -> String {
        let mut out = String::with_capacity(body.len() + 96);
        out.push_str("def _rupico_op():\n");
        for line in body.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("try:\n    _rupico_op()\nfinally:\n    del _rupico_op\n");
        out
    }

    /// Run one filesystem helper program and take its stdout.
    ///
    /// Every helper goes through here, so scoping and the "did the device
    /// actually raise?" question are each decided in exactly one place. User
    /// code does *not*: `run_file` and the REPL must execute at module level,
    /// where the names a script defines are supposed to stick.
    fn run_helper(&mut self, body: String) -> Result<String> {
        let result = self.exec_raw_classic(Self::scoped(&body))?;
        self.check_remote(result)
    }

    /// Unwrap a helper snippet's result: stdout on success, `Remote` only if
    /// the device actually raised.
    ///
    /// Anything else the board printed to stderr is a warning, and is kept
    /// for `take_remote_warnings` rather than failing the operation.
    fn check_remote(&mut self, result: ExecResult) -> Result<String> {
        if stderr_is_fatal(&result.stderr) {
            return Err(MicroPythonError::Remote(result.stderr));
        }
        if !result.stderr.trim().is_empty() {
            if self.remote_warnings.len() == MAX_REMOTE_WARNINGS {
                self.remote_warnings.remove(0);
            }
            self.remote_warnings
                .push(result.stderr.trim_end().to_string());
        }
        Ok(result.stdout)
    }

    /// Take everything the device has printed to stderr without raising.
    ///
    /// Draining rather than copying, so a caller that polls this between
    /// operations reports each warning once.
    pub fn take_remote_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.remote_warnings)
    }

    /// Open a serial port and construct a `MicroPythonDevice` with explicit
    /// baud rate and read timeout.
    pub fn open(path: &str, baud_rate: u32, read_timeout: Duration) -> Result<Self> {
        let port = serialport::new(path, baud_rate)
            // Short OS-level timeout; we implement our own deadline on top.
            .timeout(Duration::from_millis(200))
            .open()?;

        Ok(Self {
            port,
            handshake_timeout: read_timeout,
            read_timeout: Some(read_timeout),
            rx_buf: Vec::new(),
            raw_paste_supported: None,
            remote_warnings: Vec::new(),
        })
    }

    /// Build a device around an already-constructed port.
    ///
    /// Exists so tests can drive the protocol over a scripted in-memory port
    /// instead of real hardware; there is no other way to exercise the raw
    /// REPL framing without a board attached.
    #[cfg(test)]
    pub(crate) fn from_port(port: Box<dyn SerialPort>, read_timeout: Duration) -> Self {
        Self {
            port,
            handshake_timeout: read_timeout,
            read_timeout: Some(read_timeout),
            rx_buf: Vec::new(),
            raw_paste_supported: None,
            remote_warnings: Vec::new(),
        }
    }

    /// Set the idle read timeout used during execution and file transfers.
    /// `None` disables the timeout entirely (wait forever), which is useful
    /// for running long-lived programs. The raw-REPL handshake timeout is
    /// unaffected and stays finite.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Convenience constructor that uses sensible defaults for typical
    /// MicroPython boards (115200 baud, ~3s read timeout).
    pub fn connect(path: &str) -> Result<Self> {
        const DEFAULT_BAUD: u32 = 115_200;
        const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(3);
        Self::open(path, DEFAULT_BAUD, DEFAULT_READ_TIMEOUT)
    }

    /// A second handle on this port, for interrupting from another thread.
    ///
    /// Raw REPL has no out-of-band channel: stopping a running program means
    /// writing Ctrl-C to the port, and the thread that owns the device is
    /// precisely the one blocked waiting for that program's output. This
    /// hands a *writer* to another thread so a UI can stay responsive; it
    /// deliberately exposes nothing but the interrupt.
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        Ok(InterruptHandle {
            port: self.port.try_clone()?,
        })
    }

    /// Send Ctrl-C to interrupt any running program.
    pub fn interrupt(&mut self) -> Result<()> {
        self.port.write_all(&[CTRL_C])?;
        self.port.flush()?;
        Ok(())
    }

    /// Enter raw REPL mode.
    ///
    /// This sends a couple of interrupts, then Ctrl-A and waits for the
    /// `raw REPL; CTRL-B to exit` banner and a `>` prompt.
    pub fn enter_raw_repl(&mut self) -> Result<()> {
        // Try to stop anything currently running.
        self.port.write_all(&[CTRL_C, CTRL_C])?;
        self.port.flush()?;
        std::thread::sleep(Duration::from_millis(100));

        // Drain any stale output (boot messages, leftover program output,
        // KeyboardInterrupt tracebacks) so it cannot confuse the banner
        // detection below.
        //
        // The drain is bounded by `handshake_timeout`: a board running a
        // program that survives the interrupts above (for example a bare
        // `except:` around a printing loop) streams forever, and an
        // unbounded drain would hang every command with no way out.
        let mut scratch = [0u8; 256];
        let drain_deadline = Instant::now() + self.handshake_timeout;
        while Instant::now() < drain_deadline {
            match self.port.read(&mut scratch) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
        self.rx_buf.clear();

        // Request raw REPL.
        self.port.write_all(&[CTRL_A])?;
        self.port.flush()?;

        let mut buf = [0u8; 256];
        let mut collected = Vec::new();
        let deadline = Instant::now() + self.handshake_timeout;

        while Instant::now() < deadline {
            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    collected.extend_from_slice(&buf[..n]);

                    // Heuristic: once we've seen "raw REPL" and a trailing '>' prompt, assume we're in.
                    let has_banner = collected.windows(8).any(|w| w == b"raw REPL");
                    if has_banner && collected.ends_with(b">") {
                        return Ok(());
                    }
                }
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            }
        }

        Err(MicroPythonError::HandshakeTimeout)
    }

    /// Exit raw REPL back to the friendly REPL.
    pub fn exit_raw_repl(&mut self) -> Result<()> {
        self.port.write_all(&[CTRL_B])?;
        self.port.flush()?;
        Ok(())
    }

    /// Best-effort recovery routine after a suspected protocol desync or
    /// error. It clears any buffered bytes, sends interrupts, and then
    /// attempts to re-enter raw REPL.
    pub fn recover(&mut self) -> Result<()> {
        self.rx_buf.clear();
        self.port.write_all(&[CTRL_C, CTRL_C])?;
        self.port.flush()?;
        std::thread::sleep(Duration::from_millis(100));
        self.enter_raw_repl()
    }

    /// Perform a soft reboot so that `boot.py` / `main.py` run, if present.
    ///
    /// This attempts to return to the friendly REPL, sends a couple of
    /// interrupts, and then issues Ctrl-D to trigger the soft reset.
    pub fn soft_reboot(&mut self) -> Result<()> {
        // Ignore errors here; soft reboot is best-effort.
        let _ = self.exit_raw_repl();
        self.port.write_all(&[CTRL_C, CTRL_C])?;
        self.port.flush()?;
        std::thread::sleep(Duration::from_millis(50));
        self.port.write_all(&[CTRL_D])?;
        self.port.flush()?;
        Ok(())
    }

    /// Send interrupts to stop any currently running user program.
    ///
    /// This does not change REPL mode (raw vs friendly); callers may
    /// wish to follow this with `enter_raw_repl` or `recover`.
    pub fn stop_current_program(&mut self) -> Result<()> {
        self.rx_buf.clear();
        self.port.write_all(&[CTRL_C, CTRL_C])?;
        self.port.flush()?;
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    /// Convenience wrapper: run a small snippet of Python code in raw
    /// REPL mode. This is just an alias for `exec_raw` but documents the
    /// intended use.
    pub fn run_snippet<S: AsRef<str>>(&mut self, code: S) -> Result<ExecResult> {
        self.exec_raw(code)
    }

    /// Run one entry typed at an interactive prompt.
    ///
    /// Raw REPL compiles whatever it is sent in `exec` mode, so a bare
    /// expression runs but its value is thrown away: `run_snippet("1 + 1")`
    /// prints nothing, which is not what someone typing at a prompt expects.
    /// Compiling in `single` mode instead is what makes a prompt a prompt —
    /// the device echoes the value of each expression statement and stays
    /// quiet for assignments and for `None`, which is the result of most calls
    /// worth making on a board.
    ///
    /// Names bind in the device's `__main__` globals, so state carries across
    /// calls for as long as the raw-REPL session lives. The wrapper's own
    /// temporaries are `_rupico_`-prefixed because they are visible to a
    /// `dir()` typed at the prompt, so they should at least be obviously ours.
    pub fn run_repl_entry(&mut self, source: &str) -> Result<ExecResult> {
        self.exec_raw(Self::repl_snippet(source))
    }

    /// Build the device-side program for one REPL entry.
    ///
    /// Split out from `run_repl_entry` so the escaping — the part that breaks
    /// badly on a quote or a newline — can be tested without a board.
    fn repl_snippet(source: &str) -> String {
        let escaped = Self::py_escape_single_quoted(source);
        format!(
            concat!(
                "_rupico_src = '{}'\n",
                "try:\n",
                "    _rupico_code = compile(_rupico_src, '<repl>', 'single')\n",
                // Deliberately broad. A build without `compile`, or without
                // `single` mode, must degrade to a plain exec rather than
                // fail the entry — and a genuine syntax error in the entry is
                // not swallowed, because the fallback re-raises it.
                "except:\n",
                "    _rupico_code = None\n",
                "if _rupico_code is None:\n",
                "    exec(_rupico_src)\n",
                "else:\n",
                "    exec(_rupico_code)\n",
            ),
            escaped,
        )
    }

    /// Execute a Python file already stored on the device.
    ///
    /// This uses `exec` on the contents of the file. It assumes raw
    /// REPL mode is active.
    ///
    /// The reader's temporaries are `_rupico_`-prefixed and deleted before
    /// the script runs: they share the namespace the script then executes in,
    /// so plain names like `p`, `f` or `src` would shadow the script's own —
    /// a script whose first line is `src = open(...)` should not find someone
    /// else's `src` already bound.
    pub fn run_file(&mut self, path: &str) -> Result<ExecResult> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "_rupico_f = open('{}', 'r')\n",
                "try:\n",
                "    _rupico_src = _rupico_f.read()\n",
                "finally:\n",
                "    _rupico_f.close()\n",
                "    del _rupico_f\n",
                // `finally`, so a script that raises still leaves nothing of
                // ours behind for the next run to trip over.
                "try:\n",
                "    exec(_rupico_src)\n",
                "finally:\n",
                "    del _rupico_src\n",
            ),
            escaped,
        );
        self.exec_raw(code)
    }

    /// Flash the given source text as `main.py` on the device so that it
    /// will run on the next soft reboot.
    pub fn flash_main_script(&mut self, source: &str) -> Result<()> {
        self.write_text_file("/main.py", source)
    }

    /// Trigger execution of `boot.py` / `main.py` via soft reboot.
    pub fn run_main(&mut self) -> Result<()> {
        self.soft_reboot()
    }

    /// List the entries in a directory on the device.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "import os, json\n",
                "p = '{}'\n",
                "ents = []\n",
                "for name in os.listdir(p):\n",
                "    if p.endswith('/'):\n",
                "        full = p + name\n",
                "    else:\n",
                "        full = p + '/' + name\n",
                "    try:\n",
                "        st = os.stat(full)\n",
                "        mode = st[0]\n",
                "        size = st[6]\n",
                "        is_dir = (mode & 0x4000) != 0\n",
                "        mtime = st[8] if len(st) > 8 else None\n",
                "    except OSError:\n",
                "        size = 0\n",
                "        is_dir = False\n",
                "        mtime = None\n",
                "    ents.append(dict(name=name, is_dir=is_dir, size=size, modified=mtime))\n",
                "print(json.dumps(ents))\n",
            ),
            escaped
        );

        let stdout = self.run_helper(code)?;

        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let entries: Vec<RemoteEntry> = serde_json::from_str(trimmed).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid JSON from device while listing '{}': {e}; stdout={}",
                path, stdout
            ))
        })?;

        Ok(entries)
    }

    /// Recursively list a directory tree on the device, computing a sha256
    /// content hash for every file in a single round trip.
    ///
    /// This is the workhorse for sync: one exec walks the whole tree, so it
    /// is far faster than a `list_dir` call per directory, and content
    /// hashes allow reliable change detection without trusting the device
    /// clock (which is often unset on boards without a battery-backed RTC).
    ///
    /// If the firmware has no sha256 implementation (neither `hashlib` nor
    /// `uhashlib`), entries are returned with `hash: None` and callers
    /// should fall back to size-based comparison.
    ///
    /// Returns `Ok(None)` when `root` does not exist on the device, which is
    /// deliberately distinct from `Ok(Some(vec![]))` for an existing but
    /// empty directory. Callers that mirror the device onto the host **must**
    /// treat the two differently: a missing root that reads as "empty" would
    /// make a `--delete` sync erase every local file. Uploading to a missing
    /// root is fine (it gets created), so only that direction should treat
    /// `None` as an empty tree.
    ///
    /// Individual unreadable entries are skipped rather than aborting the
    /// walk, but a failure never truncates the listing silently: every
    /// directory that can be listed is listed.
    pub fn list_tree_hashed(&mut self, root: &str) -> Result<Option<Vec<RemoteTreeEntry>>> {
        self.list_tree(root, TreeOptions::hashed())
    }

    /// Recursively list a directory tree on the device in a single round trip,
    /// collecting the extras named by `opts`.
    ///
    /// `list_tree_hashed` is this with hashing on. Hashing is what makes the
    /// walk expensive — every file is read on the device — so a caller that
    /// only wants to *show* a tree (`ls -R`, the GUI's file rail) should ask
    /// for [`TreeOptions::metadata_only`] and get the same single round trip
    /// for the cost of a `stat` per entry.
    ///
    /// The `None` return and the skip-don't-abort behaviour are exactly as
    /// documented on [`Self::list_tree_hashed`].
    pub fn list_tree(
        &mut self,
        root: &str,
        opts: TreeOptions,
    ) -> Result<Option<Vec<RemoteTreeEntry>>> {
        let escaped = Self::py_escape_single_quoted(root);
        // Turning hashing off is a matter of never finding a sha256: `fhash`
        // then returns `None` for every file without opening it.
        let sha_setup = if opts.hashes {
            concat!(
                "_sha = None\n",
                "try:\n",
                "    import hashlib\n",
                "    _sha = getattr(hashlib, 'sha256', None)\n",
                "except ImportError:\n",
                "    pass\n",
                "if _sha is None:\n",
                "    try:\n",
                "        import uhashlib\n",
                "        _sha = getattr(uhashlib, 'sha256', None)\n",
                "    except ImportError:\n",
                "        pass\n",
            )
        } else {
            "_sha = None\n"
        };
        // The key is left out entirely rather than sent as null: it is per
        // entry, and the device has to hold the whole JSON in RAM.
        let (mt_dir, mt_file) = if opts.mtimes {
            (", m=None", ", m=(st[8] if len(st) > 8 else None)")
        } else {
            ("", "")
        };
        let code = format!(
            concat!(
                "import os, json, binascii\n",
                "{sha_setup}",
                "root = '{root}'\n",
                "out = []\n",
                "def fhash(p):\n",
                "    if _sha is None:\n",
                "        return None\n",
                // An unreadable file must not abort the walk: report it with
                // no hash so the caller falls back to a size comparison.
                "    try:\n",
                "        h = _sha()\n",
                "        f = open(p, 'rb')\n",
                "        try:\n",
                "            while True:\n",
                "                b = f.read(1024)\n",
                "                if not b:\n",
                "                    break\n",
                "                h.update(b)\n",
                "        finally:\n",
                "            f.close()\n",
                "        return binascii.hexlify(h.digest()).decode()\n",
                "    except OSError:\n",
                "        return None\n",
                "def walk(d, rel):\n",
                // A directory we cannot list is skipped on its own rather
                // than unwinding and truncating everything after it.
                "    try:\n",
                "        names = os.listdir(d)\n",
                "    except OSError:\n",
                "        return\n",
                "    for name in names:\n",
                "        full = (d + name) if d.endswith('/') else (d + '/' + name)\n",
                "        r = (rel + '/' + name) if rel else name\n",
                "        try:\n",
                "            st = os.stat(full)\n",
                "        except OSError:\n",
                "            continue\n",
                "        if st[0] & 0x4000:\n",
                "            out.append(dict(p=r, d=True, s=0, h=None{mt_dir}))\n",
                "            walk(full, r)\n",
                "        else:\n",
                "            out.append(dict(p=r, d=False, s=st[6], h=fhash(full){mt_file}))\n",
                // `null` marks a root that does not exist, so the host can
                // tell "missing" apart from "empty".
                "_missing = False\n",
                "try:\n",
                "    os.stat(root)\n",
                "except OSError:\n",
                "    _missing = True\n",
                "if _missing:\n",
                "    print('null')\n",
                "else:\n",
                "    walk(root, '')\n",
                "    print(json.dumps(out))\n",
            ),
            sha_setup = sha_setup,
            root = escaped,
            mt_dir = mt_dir,
            mt_file = mt_file,
        );

        let stdout = self.run_helper(code)?;

        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err(MicroPythonError::Protocol(format!(
                "empty response from device while hashing tree '{root}'"
            )));
        }

        let entries: Option<Vec<RemoteTreeEntry>> = serde_json::from_str(trimmed).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid JSON from device while hashing tree '{}': {e}; stdout={}",
                root, stdout
            ))
        })?;

        Ok(entries)
    }

    /// Read a file as raw bytes from the device.
    ///
    /// The file is encoded and streamed in small chunks on the device rather
    /// than being slurped into one buffer: a board with ~192 KB of RAM cannot
    /// hold a large file *and* its base64 expansion at once, so reading whole
    /// files at once fails with `MemoryError` well before the flash fills up.
    /// This still costs only one round trip.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let escaped = Self::py_escape_single_quoted(path);
        // Must be a multiple of 3 so each chunk encodes without padding and
        // the concatenated output is still valid base64.
        const READ_CHUNK: usize = 1536;
        let code = format!(
            concat!(
                "import binascii\n",
                "p = '{}'\n",
                "f = open(p, 'rb')\n",
                "try:\n",
                "    while True:\n",
                "        b = f.read({})\n",
                "        if not b:\n",
                "            break\n",
                "        print(binascii.b2a_base64(b).decode(), end='')\n",
                "finally:\n",
                "    f.close()\n",
            ),
            escaped, READ_CHUNK
        );

        let stdout = self.run_helper(code)?;

        // `b2a_base64` terminates every chunk with a newline, so strip all
        // whitespace before decoding the concatenated stream.
        let b64: String = stdout.chars().filter(|c| !c.is_whitespace()).collect();
        if b64.is_empty() {
            return Ok(Vec::new());
        }

        let decoded = B64.decode(&b64).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid base64 from device while reading '{}': {e}; stdout={}",
                path, stdout
            ))
        })?;

        Ok(decoded)
    }

    /// Convenience helper: read a UTF-8 text file from the device.
    pub fn read_text_file(&mut self, path: &str) -> Result<String> {
        let bytes = self.read_file(path)?;
        String::from_utf8(bytes).map_err(MicroPythonError::Utf8)
    }

    /// Write raw bytes to a file on the device, overwriting if it exists.
    ///
    /// The data is staged in a sibling temporary file and moved into place
    /// only once every chunk has landed, so an interrupted transfer leaves
    /// the previous contents intact. Writing directly would truncate the
    /// target on the first chunk, and a timeout partway through a
    /// `flash-main` would leave a half-written `main.py` that the board then
    /// tries to run at the next reset.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let staging = Self::staging_path_for(path);

        match self.write_file_direct(&staging, data) {
            Ok(()) => {}
            Err(e) => {
                // Don't leave the staging file behind on a failed transfer.
                let _ = self.remove(&staging);
                return Err(e);
            }
        }

        match self.replace_with(&staging, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = self.remove(&staging);
                Err(e)
            }
        }
    }

    /// Build the staging path used by [`write_file`]. It sits in the same
    /// directory as the target so the final rename stays within one
    /// filesystem.
    fn staging_path_for(path: &str) -> String {
        match path.rfind('/') {
            Some(0) => format!("/.rupico-tmp-{}", &path[1..]),
            Some(i) => format!("{}/.rupico-tmp-{}", &path[..i], &path[i + 1..]),
            None => format!(".rupico-tmp-{path}"),
        }
    }

    /// Move `from` onto `to`, replacing `to` if it already exists.
    ///
    /// `os.rename` refuses an existing destination on FAT filesystems, so the
    /// old file is removed first.
    fn replace_with(&mut self, from: &str, to: &str) -> Result<()> {
        let from_escaped = Self::py_escape_single_quoted(from);
        let to_escaped = Self::py_escape_single_quoted(to);
        let code = format!(
            concat!(
                "import os\n",
                "src = '{}'\n",
                "dst = '{}'\n",
                "try:\n",
                "    os.remove(dst)\n",
                "except OSError:\n",
                "    pass\n",
                "os.rename(src, dst)\n",
            ),
            from_escaped, to_escaped
        );
        self.run_helper(code)?;
        Ok(())
    }

    /// True when a device-side traceback is an out-of-memory failure.
    fn is_remote_memory_error(err: &MicroPythonError) -> bool {
        matches!(err, MicroPythonError::Remote(msg) if msg.contains("MemoryError"))
    }

    /// Write bytes straight to `path`, truncating it on the first chunk.
    ///
    /// This is the raw transfer used by [`write_file`] to fill its staging
    /// file; callers that need overwrite safety should use `write_file`.
    ///
    /// A chunk costs the board about 4/3 its size as a base64 string literal
    /// inside the snippet, plus the decoded bytes, and the literal is
    /// allocated while the snippet is still being *compiled*. Entering the
    /// raw REPL interrupts a running program but never frees what it
    /// allocated, so a board that was mid-program has a heap that is both
    /// smaller and more fragmented than an idle one, and that literal is the
    /// allocation that fails. When it does, halve the chunk and start the
    /// file over rather than surfacing a `MemoryError` the user cannot act
    /// on. Restarting (instead of resuming) is deliberate: the first chunk
    /// truncates, so a fresh pass cannot append onto a partial write.
    fn write_file_direct(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let escaped_path = Self::py_escape_single_quoted(path);
        // Largest first; each retry is a full re-send, so keep the ladder short.
        const CHUNK_SIZES: [usize; 4] = [2048, 1024, 512, 256];

        if data.is_empty() {
            // Ensure the file exists and is empty.
            let code = format!(
                concat!("p = '{}'\n", "with open(p, 'wb') as f:\n", "    pass\n",),
                escaped_path
            );
            self.run_helper(code)?;
            return Ok(());
        }

        let mut last_err: Option<MicroPythonError> = None;
        for (attempt, &chunk_size) in CHUNK_SIZES.iter().enumerate() {
            if attempt > 0 {
                // The collect needs its own frame: a `gc.collect()` at the top
                // of the failing snippet would never run, because compiling
                // that snippet is what ran out of memory.
                let _ = self.exec_raw_classic("import gc\ngc.collect()\n");
            }

            match self.write_chunks(&escaped_path, data, chunk_size) {
                Ok(()) => return Ok(()),
                Err(e) if Self::is_remote_memory_error(&e) => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            MicroPythonError::Protocol("write retry loop made no attempt".to_string())
        }))
    }

    /// Send `data` to an already-escaped `path` as base64 chunks of
    /// `chunk_size` bytes. The first chunk truncates the file.
    fn write_chunks(&mut self, escaped_path: &str, data: &[u8], chunk_size: usize) -> Result<()> {
        for (i, chunk) in data.chunks(chunk_size).enumerate() {
            let mode = if i == 0 { "wb" } else { "ab" };
            let b64 = B64.encode(chunk);
            let code = format!(
                concat!(
                    "import binascii\n",
                    "p = '{}'\n",
                    "b = '{}'\n",
                    "raw = binascii.a2b_base64(b)\n",
                    "with open(p, '{}') as f:\n",
                    "    f.write(raw)\n",
                ),
                escaped_path, b64, mode
            );

            self.run_helper(code)?;
        }

        Ok(())
    }

    /// Convenience helper: write a UTF-8 text file to the device.
    pub fn write_text_file(&mut self, path: &str, contents: &str) -> Result<()> {
        self.write_file(path, contents.as_bytes())
    }

    /// Remove a file on the device.
    ///
    /// Refuses a directory with a message that names the way out, rather than
    /// letting `os.remove` fail with a bare errno the user has to decode.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "import os\n",
                "p = '{}'\n",
                "if os.stat(p)[0] & 0x4000:\n",
                "    raise OSError('is a directory, remove it recursively: ' + p)\n",
                "os.remove(p)\n",
            ),
            escaped
        );
        self.run_helper(code)?;
        Ok(())
    }

    /// Remove a file, or a directory and everything inside it.
    ///
    /// One round trip: the walk, the unlinks and the rmdirs all happen on the
    /// device. Directories are removed deepest-first, because a board's
    /// `os.rmdir` only takes empty ones.
    ///
    /// The filesystem root is emptied but not itself removed — `rmdir('/')`
    /// cannot succeed, and failing *after* deleting everything would be a
    /// confusing way to report a job that was actually done.
    pub fn remove_tree(&mut self, path: &str) -> Result<RemoveOutcome> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "import os, json\n",
                "root = '{}'\n",
                "nf = 0\n",
                "nd = 0\n",
                // A missing path raises here, before anything is deleted.
                "if os.stat(root)[0] & 0x4000:\n",
                "    stack = [root]\n",
                "    dirs = []\n",
                "    while stack:\n",
                "        d = stack.pop()\n",
                "        dirs.append(d)\n",
                "        for name in os.listdir(d):\n",
                "            full = (d + name) if d.endswith('/') else (d + '/' + name)\n",
                "            if os.stat(full)[0] & 0x4000:\n",
                "                stack.append(full)\n",
                "            else:\n",
                "                os.remove(full)\n",
                "                nf += 1\n",
                // A parent is always appended before its children, so popping
                // from the end always empties a directory before removing it.
                "    while dirs:\n",
                "        d = dirs.pop()\n",
                "        if d != '/':\n",
                "            os.rmdir(d)\n",
                "            nd += 1\n",
                "else:\n",
                "    os.remove(root)\n",
                "    nf += 1\n",
                "print(json.dumps([nf, nd]))\n",
            ),
            escaped
        );
        let stdout = self.run_helper(code)?;

        let counts: (usize, usize) = serde_json::from_str(stdout.trim()).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid JSON from device while removing '{path}': {e}; stdout={stdout}"
            ))
        })?;
        Ok(RemoveOutcome {
            files: counts.0,
            dirs: counts.1,
        })
    }

    /// Create a directory on the device.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!("import os\n", "p = '{}'\n", "os.mkdir(p)\n",),
            escaped
        );
        self.run_helper(code)?;
        Ok(())
    }

    /// Remove an empty directory on the device.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!("import os\n", "p = '{}'\n", "os.rmdir(p)\n",),
            escaped
        );
        self.run_helper(code)?;
        Ok(())
    }

    /// Rename a file or directory on the device.
    pub fn rename(&mut self, old_path: &str, new_path: &str) -> Result<()> {
        let old_escaped = Self::py_escape_single_quoted(old_path);
        let new_escaped = Self::py_escape_single_quoted(new_path);
        let code = format!(
            concat!(
                "import os\n",
                "src = '{}'\n",
                "dst = '{}'\n",
                "os.rename(src, dst)\n",
            ),
            old_escaped, new_escaped
        );
        self.run_helper(code)?;
        Ok(())
    }

    /// Execute a snippet of Python code in raw REPL mode and return split
    /// stdout and stderr according to the raw-REPL framing.
    ///
    /// Normally this uses the classic raw-REPL protocol:
    ///
    ///   OK\n<stdout bytes>\x04<stderr bytes>\x04
    ///
    /// On newer MicroPython builds that support it we instead use the
    /// "raw-paste" protocol, which streams the code with built-in flow
    /// control for higher throughput. In that case the framing for stdout
    /// and stderr is the same but there is no leading `OK` line.
    ///
    /// # Binary output caveat
    ///
    /// The raw-REPL protocol delimits stdout and stderr with a literal
    /// 0x04 byte. If the executed code prints a raw 0x04 itself, the
    /// framing desyncs and the results will be garbled. This is inherent
    /// to the protocol; binary data should be transported base64-encoded
    /// (as the file helpers in this module do).
    pub fn exec_raw<S: AsRef<str>>(&mut self, code: S) -> Result<ExecResult> {
        let mut text = code.as_ref().to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }

        // Clear any buffered bytes from previous operations so we start
        // parsing from a clean frame boundary.
        self.rx_buf.clear();

        // First, try to use raw-paste. If it succeeds we are done; if it is
        // not supported on this device we fall back to the classic path.
        if let Some(result) = self.try_exec_raw_paste(text.as_bytes())? {
            return Ok(result);
        }

        self.exec_raw_classic(text)
    }

    /// Execute code using the classic raw REPL protocol only, without
    /// attempting raw-paste negotiation. This is useful for operations that
    /// are known to behave well with the original protocol (such as
    /// filesystem helpers) or when debugging device-specific raw-paste
    /// issues.
    pub fn exec_raw_classic<S: AsRef<str>>(&mut self, code: S) -> Result<ExecResult> {
        let mut text = code.as_ref().to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }

        // Clear any buffered bytes from previous operations so we start
        // parsing from a clean frame boundary.
        self.rx_buf.clear();

        self.port.write_all(text.as_bytes())?;
        self.port.write_all(&[CTRL_D])?;
        self.port.flush()?;

        let raw_stdout = self.read_until_sentinel(CTRL_D)?;
        let raw_stderr = self.read_until_sentinel(CTRL_D)?;

        let stdout = Self::strip_ok_banner(raw_stdout)?;
        let stderr = String::from_utf8(raw_stderr)?;

        Ok(ExecResult { stdout, stderr })
    }

    /// Attempt to execute code using the MicroPython "raw-paste" protocol.
    ///
    /// If the connected device does not support this extension, or if the
    /// negotiation fails, this returns `Ok(None)` and leaves the device in
    /// raw-REPL mode so that the caller can fall back to the classic
    /// `exec_raw` path.
    fn try_exec_raw_paste(&mut self, code: &[u8]) -> Result<Option<ExecResult>> {
        // Respect a previous negative probe to avoid re-negotiating on every
        // call for devices that don't implement raw-paste.
        if matches!(self.raw_paste_supported, Some(false)) {
            return Ok(None);
        }

        // Send the raw-paste initiation sequence. The device will respond with
        // either:
        //   - b"R\x00" : understands but does not support raw-paste.
        //   - b"R\x01" : supports raw-paste and is now in that mode.
        //   - b"ra"    : does not understand raw-paste; the remaining
        //                "w REPL; CTRL-B to exit\r\n>" banner should be
        //                discarded and we should fall back.
        self.port.write_all(&[0x05, b'A', 0x01])?;
        self.port.flush()?;

        // The handshake is a fixed-size exchange and must always be bounded,
        // even under `--timeout 0`: firmware without raw-paste support treats
        // the bytes above as source text and answers nothing at all, so an
        // unbounded read here would hang instead of falling back to classic
        // raw REPL.
        let header = self.read_exact_within(2, Some(self.handshake_timeout))?;
        match header.as_slice() {
            b"R\x00" => {
                // Device knows about raw-paste but this port/firmware does not
                // support it. Mark as unavailable and fall back.
                self.raw_paste_supported = Some(false);
                return Ok(None);
            }
            b"R\x01" => {
                // Proceed below.
            }
            b"ra" => {
                // Read and discard the remainder of the raw-REPL banner so the
                // caller can safely fall back to classic execution.
                let _ = self.read_until_sentinel(b'>')?;
                self.raw_paste_supported = Some(false);
                return Ok(None);
            }
            other => {
                return Err(MicroPythonError::Protocol(format!(
                    "unexpected raw-paste handshake response: {:?}",
                    other
                )));
            }
        }

        // At this point raw-paste is active on the device.
        self.raw_paste_supported = Some(true);

        // Next the device sends a 2-byte little-endian window-size increment
        // used for flow control. See the official MicroPython raw-REPL
        // documentation for details.
        let win_bytes = self.read_exact_within(2, Some(self.handshake_timeout))?;
        if win_bytes.len() != 2 {
            return Err(MicroPythonError::Protocol(
                "short window size from device in raw-paste handshake".into(),
            ));
        }
        let window_inc = u16::from_le_bytes([win_bytes[0], win_bytes[1]]) as usize;
        if window_inc == 0 {
            return Err(MicroPythonError::Protocol(
                "zero window size from device in raw-paste handshake".into(),
            ));
        }

        let mut remaining = window_inc;
        let mut offset: usize = 0;
        let mut sent_end = false;

        // Stream the code respecting the flow-control window. When the device
        // sends 0x01 we may send another `window_inc` bytes; when it sends
        // 0x04 it is asking us to stop sending and to reply with our own
        // 0x04 terminator.
        while offset < code.len() {
            if remaining == 0 || self.port.bytes_to_read().unwrap_or(0) > 0 {
                // Either the window is exhausted or the device has something
                // to say (like a window update or early-termination request).
                let ack = self.read_exact_within(1, self.read_timeout)?;
                if ack.is_empty() {
                    return Err(MicroPythonError::ExecTimeout);
                }
                match ack[0] {
                    0x01 => {
                        remaining += window_inc;
                    }
                    CTRL_D => {
                        // Device wants to end data reception early.
                        self.port.write_all(&[CTRL_D])?;
                        self.port.flush()?;
                        sent_end = true;
                        break;
                    }
                    other => {
                        return Err(MicroPythonError::Protocol(format!(
                            "unexpected flow-control byte during raw-paste: {:#04x}",
                            other
                        )));
                    }
                }
            }

            if offset < code.len() && remaining > 0 {
                let to_send = remaining.min(code.len() - offset);
                let chunk = &code[offset..offset + to_send];
                self.port.write_all(chunk)?;
                self.port.flush()?;
                remaining -= to_send;
                offset += to_send;
            }
        }

        if !sent_end {
            // Signal end-of-code to the device.
            self.port.write_all(&[CTRL_D])?;
            self.port.flush()?;
        }

        // Read until the device signals that it has finished compiling and is
        // now executing the code. Any bytes that arrive before the sentinel
        // are treated as part of stdout and kept in `rx_buf` by
        // `read_until_sentinel`.
        let _ = self.read_until_sentinel(CTRL_D)?;

        // Now collect stdout and stderr using the usual 0x04 framing.
        let raw_stdout = self.read_until_sentinel(CTRL_D)?;
        let raw_stderr = self.read_until_sentinel(CTRL_D)?;

        // No `strip_ok_banner` here: raw-paste has no `OK` banner, so every
        // byte of this frame is the program's own output. Stripping it would
        // silently eat a leading "OK" — `print("OKAY")` would come back as
        // "AY", and `print("OK")` as nothing at all.
        let stdout = String::from_utf8(raw_stdout)?;
        let stderr = String::from_utf8(raw_stderr)?;

        Ok(Some(ExecResult { stdout, stderr }))
    }

    /// Read bytes from the serial port until we encounter the given
    /// sentinel byte, returning everything before it. Any bytes after the
    /// sentinel are kept in the internal buffer for future reads.
    fn read_until_sentinel(&mut self, sentinel: u8) -> Result<Vec<u8>> {
        let mut buf = [0u8; 256];
        // Idle deadline: reset whenever data arrives so long-running
        // transfers and programs are not cut off while still producing
        // output. `None` means no timeout at all.
        let mut deadline = self.read_timeout.map(|t| Instant::now() + t);

        loop {
            if let Some(pos) = self.rx_buf.iter().position(|b| *b == sentinel) {
                let before = self.rx_buf[..pos].to_vec();
                // Keep everything after the sentinel in the buffer.
                let remaining = self.rx_buf.split_off(pos + 1);
                self.rx_buf = remaining;
                return Ok(before);
            }

            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return Err(MicroPythonError::ExecTimeout);
            }

            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    self.rx_buf.extend_from_slice(&buf[..n]);
                    deadline = self.read_timeout.map(|t| Instant::now() + t);
                }
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read exactly `n` bytes from the serial port (ignoring any existing
    /// contents of `rx_buf`), or return `ExecTimeout` if that many bytes are
    /// not received before `timeout` elapses.
    ///
    /// `timeout` is passed explicitly rather than always taken from
    /// `read_timeout` so that fixed-size protocol exchanges can stay bounded
    /// even when the caller has disabled the execution timeout entirely.
    /// `None` waits forever.
    fn read_exact_within(&mut self, n: usize, timeout: Option<Duration>) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut buf = [0u8; 64];
        let mut deadline = timeout.map(|t| Instant::now() + t);

        while out.len() < n {
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return Err(MicroPythonError::ExecTimeout);
            }

            let want = std::cmp::min(buf.len(), n - out.len());
            match self.port.read(&mut buf[..want]) {
                Ok(m) if m > 0 => {
                    out.extend_from_slice(&buf[..m]);
                    deadline = timeout.map(|t| Instant::now() + t);
                }
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            }
        }

        Ok(out)
    }

    /// Strip the leading `OK` protocol line (and anything before it, such
    /// as leftover prompts) from the stdout stream, if present.
    fn strip_ok_banner(bytes: Vec<u8>) -> Result<String> {
        let text = String::from_utf8(bytes)?;

        // Some MicroPython builds emit `OK` immediately followed by output
        // on the same line (for example, `OK[]`). In that case we treat the
        // leading `OK` (and an optional following newline or space) as the
        // banner and keep the remainder.
        if let Some(mut rest) = text.strip_prefix("OK") {
            if let Some(stripped) = rest.strip_prefix("\r\n") {
                rest = stripped;
            } else if let Some(stripped) = rest.strip_prefix('\n') {
                rest = stripped;
            }
            if let Some(stripped) = rest.strip_prefix(' ') {
                rest = stripped;
            }
            return Ok(rest.to_string());
        }

        let lines = text.lines();
        let mut saw_ok = false;
        let mut kept: Vec<&str> = Vec::new();

        for line in lines {
            if !saw_ok {
                if line.trim() == "OK" {
                    saw_ok = true;
                }
                // Skip everything up to and including the first `OK` line.
                continue;
            } else {
                kept.push(line);
            }
        }

        if saw_ok {
            Ok(kept.join("\n"))
        } else {
            // Fallback: no OK line detected, return the original text.
            Ok(text)
        }
    }
}

/// USB vendor IDs commonly used by MicroPython boards: Raspberry Pi (Pico)
/// and the generic MicroPython/pyboard VID.
pub const MICROPYTHON_USB_VIDS: &[u16] = &[0x2E8A, 0xF055];

/// Passive detection: classify a port by its USB vendor ID without opening or
/// writing to it. Non-USB ports return `false`.
///
/// Lives in the library so the CLI's `ports` command and the GUI's port picker
/// agree on what looks like a board — they used to disagree, and the GUI would
/// leave a plainly-identifiable Pico unselected in a list of Bluetooth ports.
pub fn vid_looks_micropython(port_type: &serialport::SerialPortType) -> bool {
    match port_type {
        serialport::SerialPortType::UsbPort(info) => MICROPYTHON_USB_VIDS.contains(&info.vid),
        _ => false,
    }
}

/// Join a base remote path and a name component into a single remote path.
///
/// Handles the root `/` special case so that `join_remote_path("/", "main.py")`
/// produces `"/main.py"` rather than `"//main.py"`.
/// The directory part of a tree-relative path, `""` for a top-level entry.
///
/// Tree walks report paths relative to their root (`lib/thing/mod.py`), and
/// both front ends have to regroup those into directories to display them.
pub fn remote_parent(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// The final component of a path.
pub fn remote_leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn join_remote_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{}", name)
    } else if base.ends_with('/') {
        format!("{}{}", base, name)
    } else {
        format!("{}/{}", base, name)
    }
}

/// A scripted in-memory serial port that emulates just enough of the raw REPL
/// to drive `MicroPythonDevice` without hardware.
///
/// Hardware testing caught a bug that no unit test could (a conflicted file
/// losing its baseline across successive syncs), which is exactly the class of
/// defect this harness exists to catch earlier. It models the parts of the
/// protocol the host actually depends on: the raw-REPL banner, raw-paste
/// negotiation, and the `\x04`-delimited stdout/stderr framing.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use serialport::{
        ClearBuffer, DataBits, FlowControl, Parity, Result as SerialResult, SerialPort, StopBits,
    };
    use std::sync::{Arc, Mutex};

    /// How the fake device answers the raw-paste probe.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Paste {
        /// Firmware supports raw-paste (`R\x01`), the modern path.
        Supported,
        /// Firmware knows the command but declines it (`R\x00`).
        Declined,
    }

    #[derive(Default)]
    pub(crate) struct Shared {
        /// Everything the host has written, for assertions.
        pub written: Vec<u8>,
        /// Bytes queued for the host to read.
        outbox: Vec<u8>,
        /// Canned `(stdout, stderr)` frames, consumed one per exec.
        replies: Vec<(Vec<u8>, Vec<u8>)>,
        paste: Option<Paste>,
        /// True once the host has finished streaming code and sent its
        /// terminating `\x04`, i.e. a reply may now be released.
        in_paste_body: bool,
    }

    #[derive(Clone)]
    pub(crate) struct FakePort {
        shared: Arc<Mutex<Shared>>,
    }

    impl FakePort {
        pub(crate) fn new(paste: Option<Paste>, replies: Vec<(&str, &str)>) -> Self {
            let shared = Shared {
                replies: replies
                    .into_iter()
                    .map(|(o, e)| (o.as_bytes().to_vec(), e.as_bytes().to_vec()))
                    .collect(),
                paste,
                ..Default::default()
            };
            Self {
                shared: Arc::new(Mutex::new(shared)),
            }
        }

        /// Bytes the host has written, as text (lossy) for assertions.
        pub(crate) fn written_text(&self) -> String {
            String::from_utf8_lossy(&self.shared.lock().unwrap().written).into_owned()
        }
    }

    impl Shared {
        /// Queue the next canned reply using the classic framing
        /// (`OK<stdout>\x04<stderr>\x04`) or the raw-paste framing, which has
        /// a leading `\x04` acknowledgement and no `OK` banner.
        fn push_reply(&mut self, raw_paste: bool) {
            if self.replies.is_empty() {
                return;
            }
            let (out, err) = self.replies.remove(0);
            if raw_paste {
                self.outbox.push(0x04); // end-of-paste acknowledgement
            } else {
                self.outbox.extend_from_slice(b"OK");
            }
            self.outbox.extend_from_slice(&out);
            self.outbox.push(0x04);
            self.outbox.extend_from_slice(&err);
            self.outbox.push(0x04);
        }
    }

    impl std::io::Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut s = self.shared.lock().unwrap();
            if s.outbox.is_empty() {
                // Mirrors a real port with a short OS-level timeout.
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no data"));
            }
            let n = buf.len().min(s.outbox.len());
            buf[..n].copy_from_slice(&s.outbox[..n]);
            s.outbox.drain(..n);
            Ok(n)
        }
    }

    impl std::io::Write for FakePort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut s = self.shared.lock().unwrap();
            s.written.extend_from_slice(buf);

            let mut i = 0;
            while i < buf.len() {
                let b = buf[i];
                // Raw-paste probe: 0x05 'A' 0x01
                if b == 0x05 && buf.len() >= i + 3 && &buf[i + 1..i + 3] == b"A\x01" {
                    match s.paste {
                        Some(Paste::Supported) => {
                            s.outbox.extend_from_slice(b"R\x01");
                            // Window large enough that the host never needs to
                            // wait for a flow-control credit mid-stream.
                            s.outbox.extend_from_slice(&[0x00, 0x40]);
                            s.in_paste_body = true;
                        }
                        Some(Paste::Declined) => s.outbox.extend_from_slice(b"R\x00"),
                        None => {
                            // Firmware that never heard of raw-paste echoes the
                            // tail of the banner instead.
                            s.outbox.extend_from_slice(b"raw REPL; CTRL-B to exit\r\n>");
                        }
                    }
                    i += 3;
                    continue;
                }
                if b == 0x01 {
                    // CTRL-A: enter raw REPL.
                    s.outbox.extend_from_slice(b"raw REPL; CTRL-B to exit\r\n>");
                } else if b == 0x04 {
                    // CTRL-D terminates the code, in either protocol.
                    let raw_paste = s.in_paste_body;
                    s.in_paste_body = false;
                    s.push_reply(raw_paste);
                }
                i += 1;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for FakePort {
        fn name(&self) -> Option<String> {
            Some("fake".to_string())
        }
        fn baud_rate(&self) -> SerialResult<u32> {
            Ok(115_200)
        }
        fn data_bits(&self) -> SerialResult<DataBits> {
            Ok(DataBits::Eight)
        }
        fn flow_control(&self) -> SerialResult<FlowControl> {
            Ok(FlowControl::None)
        }
        fn parity(&self) -> SerialResult<Parity> {
            Ok(Parity::None)
        }
        fn stop_bits(&self) -> SerialResult<StopBits> {
            Ok(StopBits::One)
        }
        fn timeout(&self) -> Duration {
            Duration::from_millis(200)
        }
        fn set_baud_rate(&mut self, _: u32) -> SerialResult<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _: DataBits) -> SerialResult<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _: FlowControl) -> SerialResult<()> {
            Ok(())
        }
        fn set_parity(&mut self, _: Parity) -> SerialResult<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _: StopBits) -> SerialResult<()> {
            Ok(())
        }
        fn set_timeout(&mut self, _: Duration) -> SerialResult<()> {
            Ok(())
        }
        fn write_request_to_send(&mut self, _: bool) -> SerialResult<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _: bool) -> SerialResult<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> SerialResult<bool> {
            Ok(true)
        }
        fn read_data_set_ready(&mut self) -> SerialResult<bool> {
            Ok(true)
        }
        fn read_ring_indicator(&mut self) -> SerialResult<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> SerialResult<bool> {
            Ok(true)
        }
        fn bytes_to_read(&self) -> SerialResult<u32> {
            Ok(self.shared.lock().unwrap().outbox.len() as u32)
        }
        fn bytes_to_write(&self) -> SerialResult<u32> {
            Ok(0)
        }
        fn clear(&self, _: ClearBuffer) -> SerialResult<()> {
            Ok(())
        }
        fn try_clone(&self) -> SerialResult<Box<dyn SerialPort>> {
            Ok(Box::new(self.clone()))
        }
        fn set_break(&self) -> SerialResult<()> {
            Ok(())
        }
        fn clear_break(&self) -> SerialResult<()> {
            Ok(())
        }
    }

    /// Build a device wired to a fake port, already in raw REPL.
    pub(crate) fn device(
        paste: Option<Paste>,
        replies: Vec<(&str, &str)>,
    ) -> (MicroPythonDevice, FakePort) {
        let port = FakePort::new(paste, replies);
        let mut dev = MicroPythonDevice::from_port(Box::new(port.clone()), Duration::from_secs(2));
        dev.enter_raw_repl().expect("fake device enters raw REPL");
        (dev, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ok_banner_strips_ok_line_and_keeps_rest() {
        let input = b"garbage prefix\nOK\nline1\nline2\n".to_vec();
        let out = MicroPythonDevice::strip_ok_banner(input).expect("strip_ok_banner failed");
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn strip_ok_banner_without_ok_returns_original() {
        let s = "no ok here\njust text\n";
        let out = MicroPythonDevice::strip_ok_banner(s.as_bytes().to_vec())
            .expect("strip_ok_banner failed");
        assert_eq!(out, s);
    }

    #[test]
    fn remote_entry_deserializes_with_modified() {
        let json = r#"[{"name":"main.py","is_dir":false,"size":123,"modified":1733550000}]"#;
        let entries: Vec<RemoteEntry> = serde_json::from_str(json).expect("JSON parse failed");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "main.py");
        assert!(!e.is_dir);
        assert_eq!(e.size, 123);
        assert_eq!(e.modified, Some(1_733_550_000));
    }

    #[test]
    fn remote_tree_entry_deserializes_with_and_without_hash() {
        let json = r#"[
            {"p":"main.py","d":false,"s":10,"h":"ab12"},
            {"p":"lib","d":true,"s":0,"h":null},
            {"p":"lib/x.py","d":false,"s":5,"h":null}
        ]"#;
        let entries: Vec<RemoteTreeEntry> = serde_json::from_str(json).expect("JSON parse failed");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "main.py");
        assert_eq!(entries[0].hash.as_deref(), Some("ab12"));
        assert!(entries[1].is_dir);
        assert!(entries[2].hash.is_none());
    }

    use super::fake::{self, Paste};

    #[test]
    fn removing_a_tree_reports_what_it_removed() {
        let (mut dev, port) = fake::device(Some(Paste::Declined), vec![("[7, 2]\n", "")]);
        let outcome = dev.remove_tree("/lib").expect("remove succeeds");
        assert_eq!(
            outcome,
            RemoveOutcome { files: 7, dirs: 2 },
            "the caller can say what was deleted"
        );

        let sent = port.written_text();
        assert!(sent.contains("os.rmdir(d)"), "directories go too:\n{sent}");
        assert!(
            sent.contains("if d != '/':"),
            "the filesystem root cannot be rmdir'd, so emptying it must not \
             fail after the work is done:\n{sent}"
        );
    }

    #[test]
    fn removing_a_directory_without_recursion_says_so() {
        // `os.remove` on a directory fails with a bare errno; the user needs
        // to be told which flag fixes it.
        let (mut dev, port) = fake::device(Some(Paste::Declined), vec![("", "")]);
        dev.remove("/lib").expect("the fake device raises nothing");
        assert!(
            port.written_text()
                .contains("is a directory, remove it recursively"),
            "the refusal must name the way out"
        );
    }

    #[test]
    fn a_metadata_only_tree_walk_neither_hashes_nor_widens_the_payload() {
        // `ls -R` used to cost a round trip per directory. It now shares
        // sync's single-round-trip walk, but must not pay for sync's hashing
        // (a full read of every file on the board) to do it.
        let (mut dev, port) = fake::device(Some(Paste::Declined), vec![("[]", "")]);
        dev.list_tree("/", TreeOptions::metadata_only())
            .expect("walk succeeds");
        let sent = port.written_text();
        assert!(
            !sent.contains("hashlib"),
            "no hashing was asked for:\n{sent}"
        );
        assert!(
            !sent.contains("m=None"),
            "no mtimes were asked for:\n{sent}"
        );

        let (mut dev, port) = fake::device(Some(Paste::Declined), vec![("[]", "")]);
        dev.list_tree(
            "/",
            TreeOptions {
                hashes: false,
                mtimes: true,
            },
        )
        .expect("walk succeeds");
        assert!(port.written_text().contains("m=(st[8]"));

        let (mut dev, port) = fake::device(Some(Paste::Declined), vec![("[]", "")]);
        dev.list_tree_hashed("/").expect("walk succeeds");
        let sent = port.written_text();
        assert!(sent.contains("uhashlib"), "sync still hashes:\n{sent}");
        assert!(!sent.contains("m=None"), "sync never wants mtimes:\n{sent}");
    }

    #[test]
    fn a_tree_entry_parses_with_and_without_an_mtime() {
        let json = r#"[{"p":"a.py","d":false,"s":3,"h":null,"m":1733550000},
                       {"p":"b.py","d":false,"s":3,"h":null}]"#;
        let entries: Vec<RemoteTreeEntry> = serde_json::from_str(json).expect("JSON parse failed");
        assert_eq!(entries[0].modified, Some(1_733_550_000));
        assert_eq!(entries[1].modified, None);
    }

    #[test]
    fn running_a_file_leaves_no_bindings_for_the_script_to_trip_over() {
        // The defect: the reader bound `p`, `f` and `src` in the very
        // namespace the script then ran in.
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("", "")]);
        dev.run_file("/main.py").expect("exec succeeds");

        let sent = port.written_text();
        for leaked in ["\np = '", "\nf = ", "\nsrc = ", " as f:"] {
            assert!(
                !sent.contains(leaked),
                "run_file must not bind {leaked:?}:\n{sent}"
            );
        }
        assert!(
            sent.contains("del _rupico_src"),
            "temporaries are cleaned up"
        );
        assert!(sent.contains("_rupico_f.close()"), "the file is closed");
    }

    #[test]
    fn device_stderr_is_only_fatal_when_it_is_a_raised_exception() {
        assert!(stderr_is_fatal(
            "Traceback (most recent call last):\n  File \"<stdin>\", line 1\nOSError: [Errno 2]"
        ));
        assert!(stderr_is_fatal("OSError: [Errno 2] ENOENT"));
        assert!(stderr_is_fatal("KeyboardInterrupt"));
        assert!(stderr_is_fatal("MemoryError: memory allocation failed"));

        // A board that prints its own diagnostics is not a failure.
        assert!(!stderr_is_fatal(""));
        assert!(!stderr_is_fatal("\n  \n"));
        assert!(!stderr_is_fatal("WARNING: low battery\n"));
        assert!(!stderr_is_fatal("wifi: associated\n"));
        assert!(!stderr_is_fatal("note: Error handling enabled\n"));
    }

    #[test]
    fn a_device_warning_does_not_fail_a_listing() {
        // The defect: any stderr at all failed the operation, so a board that
        // logs during `os.listdir` could not be listed at all.
        let (mut dev, _port) = fake::device(
            Some(Paste::Declined),
            vec![(
                r#"[{"name":"main.py","is_dir":false,"size":12,"modified":null}]"#,
                "wifi: reassociating\n",
            )],
        );
        let entries = dev
            .list_dir("/")
            .expect("a warning must not fail the listing");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            dev.take_remote_warnings(),
            vec!["wifi: reassociating".to_string()],
            "the warning is kept for the caller to show"
        );
        assert!(
            dev.take_remote_warnings().is_empty(),
            "taking the warnings drains them"
        );
    }

    #[test]
    fn a_device_traceback_still_fails_the_listing() {
        let (mut dev, _port) = fake::device(
            Some(Paste::Declined),
            vec![(
                "",
                "Traceback (most recent call last):\n  File \"<stdin>\", line 3\nOSError: [Errno 2] ENOENT\n",
            )],
        );
        let err = dev
            .list_dir("/nope")
            .expect_err("a raise must fail the listing");
        assert!(matches!(err, MicroPythonError::Remote(_)));
    }

    #[test]
    fn a_repl_entry_compiles_interactively_and_can_fall_back() {
        // A prompt that cannot echo `machine.freq()` is not a prompt, so the
        // entry has to be compiled in `single` mode — but firmware without
        // `compile` must still run it, via the plain-exec fallback.
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("42\n", "")]);
        let res = dev.run_repl_entry("6 * 7").expect("exec succeeds");
        assert_eq!(res.stdout, "42\n");

        let sent = port.written_text();
        assert!(
            sent.contains("compile(_rupico_src, '<repl>', 'single')"),
            "the entry must be compiled interactively:\n{sent}"
        );
        assert!(
            sent.contains("exec(_rupico_src)"),
            "a build without compile() must still run the entry:\n{sent}"
        );
    }

    #[test]
    fn a_repl_entry_survives_quotes_and_newlines() {
        // The entry is spliced into a single-quoted Python literal, so an
        // apostrophe or a multi-line block would otherwise end the string
        // early and run something the user never typed.
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("", "")]);
        dev.run_repl_entry("for i in range(2):\n    print('it\\'s fine')")
            .expect("exec succeeds");

        let sent = port.written_text();
        // The handshake bytes share a line with the code, so match the
        // assignment where it starts rather than at a line boundary.
        let start = sent
            .find("_rupico_src = ")
            .expect("entry is sent as a literal");
        let literal = sent[start..].lines().next().expect("literal is one line");
        assert!(
            literal.contains("\\n") && !literal.contains("print('it's"),
            "quotes and newlines must stay escaped: {literal}"
        );
        assert!(literal.ends_with('\''), "the literal must close: {literal}");
    }

    #[test]
    fn raw_paste_does_not_strip_a_leading_ok_from_program_output() {
        // The bug this harness was built for: raw-paste has no `OK` banner,
        // so running stdout through `strip_ok_banner` ate real output.
        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("OKAY\n", "")]);
        let res = dev.run_snippet("print('OKAY')").expect("exec succeeds");
        assert_eq!(res.stdout, "OKAY\n");

        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("OK\n", "")]);
        let res = dev.run_snippet("print('OK')").expect("exec succeeds");
        assert_eq!(res.stdout, "OK\n", "output of exactly 'OK' must survive");
    }

    #[test]
    fn classic_protocol_still_strips_its_banner_but_keeps_program_output() {
        // Firmware that declines raw-paste falls back to classic framing,
        // where the `OK` really is a banner and must come off — without
        // taking a program's own leading "OK" with it.
        let (mut dev, _port) = fake::device(Some(Paste::Declined), vec![("OKAY\n", "")]);
        let res = dev.run_snippet("print('OKAY')").expect("exec succeeds");
        assert_eq!(res.stdout, "OKAY\n");
    }

    #[test]
    fn raw_paste_probe_falls_back_when_firmware_does_not_understand_it() {
        // Oldest firmware answers the probe with the raw-REPL banner instead
        // of `R\x00`/`R\x01`; the host must recover and use classic framing.
        let (mut dev, _port) = fake::device(None, vec![("hello\n", "")]);
        let res = dev.run_snippet("print('hello')").expect("exec succeeds");
        assert_eq!(res.stdout, "hello\n");
    }

    #[test]
    fn exec_splits_stdout_and_stderr_frames() {
        let (mut dev, _port) =
            fake::device(Some(Paste::Supported), vec![("out\n", "Traceback: boom\n")]);
        let res = dev.run_snippet("boom()").expect("exec succeeds");
        assert_eq!(res.stdout, "out\n");
        assert_eq!(res.stderr, "Traceback: boom\n");
    }

    #[test]
    fn missing_root_and_empty_dir_are_distinct_over_the_wire() {
        // The distinction that stops `sync-from-device --delete` from
        // mistaking a bad path for "the device has no files".
        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("null", "")]);
        assert!(
            dev.list_tree_hashed("/nope")
                .expect("call succeeds")
                .is_none()
        );

        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("[]", "")]);
        let tree = dev
            .list_tree_hashed("/empty")
            .expect("call succeeds")
            .expect("existing dir yields Some");
        assert!(tree.is_empty());
    }

    #[test]
    fn recover_reenters_raw_repl_after_a_desync() {
        // The GUI leans on this: after a protocol error it resynchronises
        // rather than leaving a connection that still looks healthy.
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("after\n", "")]);
        let before = port.written_text().matches('\u{1}').count();

        dev.recover().expect("recover re-enters raw REPL");

        let after = port.written_text().matches('\u{1}').count();
        assert!(after > before, "recover must re-issue CTRL-A");

        // The connection is usable again afterwards.
        let res = dev
            .run_snippet("print('after')")
            .expect("exec after recover");
        assert_eq!(res.stdout, "after\n");
    }

    #[test]
    fn write_file_stages_then_renames_into_place() {
        // Two execs: the chunk write, then the rename.
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("", ""), ("", "")]);
        dev.write_file("/main.py", b"print('hi')")
            .expect("write succeeds");

        let sent = port.written_text();
        assert!(
            sent.contains(".rupico-tmp-main.py"),
            "payload should land in a staging file first"
        );
        assert!(
            sent.contains("os.rename"),
            "staging file should be renamed into place"
        );
        let staged_at = sent.find(".rupico-tmp-main.py").unwrap();
        let renamed_at = sent.find("os.rename").unwrap();
        assert!(
            staged_at < renamed_at,
            "the rename must come after the data is written"
        );
    }

    #[test]
    fn read_file_reassembles_multiple_base64_chunks() {
        // `b2a_base64` terminates each chunk with a newline; the host must
        // strip that before decoding the concatenated stream.
        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("YWJj\nZGVm\n", "")]);
        let data = dev.read_file("/x.bin").expect("read succeeds");
        assert_eq!(data, b"abcdef");
    }

    #[test]
    fn read_file_handles_an_empty_file() {
        let (mut dev, _port) = fake::device(Some(Paste::Supported), vec![("", "")]);
        assert!(
            dev.read_file("/empty.txt")
                .expect("read succeeds")
                .is_empty()
        );
    }

    #[test]
    fn device_stderr_becomes_a_remote_error() {
        let (mut dev, _port) =
            fake::device(Some(Paste::Supported), vec![("", "OSError: ENOENT\n")]);
        let err = dev
            .list_dir("/missing")
            .expect_err("should surface the error");
        assert!(matches!(err, MicroPythonError::Remote(_)));
    }

    // The fake device replies with canned frames, so it cannot observe the
    // Python that runs *on* the board. These tests assert on the program the
    // host actually transmits, which is the only way to guard the embedded
    // snippets short of real hardware.

    #[test]
    fn tree_walk_program_reports_a_missing_root_distinctly() {
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("null", "")]);
        let _ = dev.list_tree_hashed("/app");
        let sent = port.written_text();

        assert!(
            sent.contains("print('null')"),
            "a missing root must be signalled as null, not as an empty list"
        );
        assert!(
            sent.contains("print(json.dumps(out))"),
            "an existing root must emit the real listing"
        );
    }

    #[test]
    fn tree_walk_program_guards_every_failure_point() {
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("[]", "")]);
        let _ = dev.list_tree_hashed("/app");
        let sent = port.written_text();

        // An unreadable file or directory must skip itself rather than unwind
        // and leave a truncated listing that reads as complete.
        assert!(
            sent.contains("names = os.listdir(d)"),
            "the recursive listdir must be bound inside a guard"
        );
        assert_eq!(
            sent.matches("except OSError:").count(),
            4,
            "listdir, stat, hashing and the root probe each need their own guard"
        );
    }

    #[test]
    fn read_program_streams_in_chunks_rather_than_slurping_the_file() {
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("", "")]);
        let _ = dev.read_file("/big.bin");
        let sent = port.written_text();

        assert!(
            !sent.contains("f.read()\n"),
            "reading the whole file at once exhausts RAM on a small board"
        );
        assert!(
            sent.contains("b = f.read(1536)"),
            "expected a chunked read loop"
        );
        // Chunks must divide by 3 or the concatenated base64 gains interior
        // padding and no longer decodes.
        assert_eq!(1536 % 3, 0);
    }

    #[test]
    fn write_backs_off_to_smaller_chunks_after_a_device_memory_error() {
        // A board whose heap is still occupied by an interrupted program
        // cannot allocate the 2733-byte base64 literal a 2048-byte chunk
        // compiles to. Halving and re-sending is what turns that into a
        // completed transfer instead of a "Sync failed" dialog.
        let data = vec![b'x'; 3000];
        let (mut dev, port) = fake::device(
            Some(Paste::Supported),
            vec![
                (
                    "",
                    "MemoryError: memory allocation failed, allocating 2733 bytes\n",
                ),
                ("", ""), // gc.collect()
                ("", ""), // three 1024-byte chunks
                ("", ""),
                ("", ""),
            ],
        );

        dev.write_file_direct("/big.bin", &data)
            .expect("a memory error should be retried, not surfaced");

        let sent = port.written_text();
        assert!(
            sent.contains("import gc"),
            "expected a collect between attempts"
        );
        assert_eq!(
            sent.matches("b = '").count(),
            4,
            "expected one failed 2048-byte chunk then three 1024-byte ones, got: {sent}"
        );
        // The retry must truncate again rather than append onto the partial file.
        assert_eq!(sent.matches("'wb'").count(), 2);
    }

    #[test]
    fn write_does_not_retry_errors_that_a_smaller_chunk_cannot_fix() {
        let (mut dev, port) = fake::device(
            Some(Paste::Supported),
            vec![("", "OSError: [Errno 2] ENOENT\n"), ("", "")],
        );

        let err = dev
            .write_file_direct("/nodir/f.bin", &vec![b'x'; 3000])
            .expect_err("a missing directory must fail immediately");
        assert!(matches!(err, MicroPythonError::Remote(_)));
        assert_eq!(
            port.written_text().matches("b = '").count(),
            1,
            "a non-memory failure must not re-send the file"
        );
    }

    #[test]
    fn paths_are_escaped_before_being_embedded_in_python() {
        let (mut dev, port) = fake::device(Some(Paste::Supported), vec![("", "")]);
        let _ = dev.read_file("/it's/a\\path.py");
        let sent = port.written_text();

        assert!(
            sent.contains("p = '/it\\'s/a\\\\path.py'"),
            "quotes and backslashes must be escaped, got: {sent}"
        );
    }

    #[test]
    fn list_tree_response_distinguishes_missing_root_from_empty_dir() {
        // `null` means the root does not exist; `[]` means it exists and is
        // empty. Conflating the two makes `--delete` wipe the local tree.
        let missing: Option<Vec<RemoteTreeEntry>> =
            serde_json::from_str("null").expect("null should parse");
        assert!(missing.is_none());

        let empty: Option<Vec<RemoteTreeEntry>> =
            serde_json::from_str("[]").expect("[] should parse");
        assert_eq!(empty.expect("empty dir is Some").len(), 0);
    }

    #[test]
    fn staging_path_is_a_sibling_of_the_target() {
        assert_eq!(
            MicroPythonDevice::staging_path_for("/main.py"),
            "/.rupico-tmp-main.py"
        );
        assert_eq!(
            MicroPythonDevice::staging_path_for("/app/lib/util.py"),
            "/app/lib/.rupico-tmp-util.py"
        );
        assert_eq!(
            MicroPythonDevice::staging_path_for("bare.py"),
            ".rupico-tmp-bare.py"
        );
    }

    #[test]
    fn strip_ok_banner_is_only_for_the_classic_protocol() {
        // Documents why `try_exec_raw_paste` must not call this: raw-paste
        // has no banner, so every byte is the program's own output.
        assert_eq!(
            MicroPythonDevice::strip_ok_banner(b"OKAY\n".to_vec()).unwrap(),
            "AY\n"
        );
    }

    #[test]
    fn join_remote_path_handles_root_and_nested() {
        assert_eq!(join_remote_path("/", "main.py"), "/main.py");
        assert_eq!(join_remote_path("/app", "main.py"), "/app/main.py");
        assert_eq!(join_remote_path("/app/", "main.py"), "/app/main.py");
    }

    #[test]
    fn base64_roundtrip_works_with_b64_engine() {
        let data = b"hello world";
        let encoded = B64.encode(data);
        let decoded = B64.decode(&encoded).expect("base64 decode failed");
        assert_eq!(&decoded, data);
    }
}
