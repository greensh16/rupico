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

    /// Execute a Python file already stored on the device.
    ///
    /// This uses `exec` on the contents of the file. It assumes raw
    /// REPL mode is active.
    pub fn run_file(&mut self, path: &str) -> Result<ExecResult> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "p = '{}'\n",
                "with open(p, 'r') as f:\n",
                "    src = f.read()\n",
                "exec(src)\n",
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

        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }

        let trimmed = result.stdout.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let entries: Vec<RemoteEntry> = serde_json::from_str(trimmed).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid JSON from device while listing '{}': {e}; stdout={}",
                path, result.stdout
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
        let escaped = Self::py_escape_single_quoted(root);
        let code = format!(
            concat!(
                "import os, json, binascii\n",
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
                "root = '{}'\n",
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
                "            out.append(dict(p=r, d=True, s=0, h=None))\n",
                "            walk(full, r)\n",
                "        else:\n",
                "            out.append(dict(p=r, d=False, s=st[6], h=fhash(full)))\n",
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
            escaped
        );

        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }

        let trimmed = result.stdout.trim();
        if trimmed.is_empty() {
            return Err(MicroPythonError::Protocol(format!(
                "empty response from device while hashing tree '{root}'"
            )));
        }

        let entries: Option<Vec<RemoteTreeEntry>> = serde_json::from_str(trimmed).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid JSON from device while hashing tree '{}': {e}; stdout={}",
                root, result.stdout
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

        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }

        // `b2a_base64` terminates every chunk with a newline, so strip all
        // whitespace before decoding the concatenated stream.
        let b64: String = result
            .stdout
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if b64.is_empty() {
            return Ok(Vec::new());
        }

        let decoded = B64.decode(&b64).map_err(|e| {
            MicroPythonError::Protocol(format!(
                "invalid base64 from device while reading '{}': {e}; stdout={}",
                path, result.stdout
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
        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }
        Ok(())
    }

    /// Write bytes straight to `path`, truncating it on the first chunk.
    ///
    /// This is the raw transfer used by [`write_file`] to fill its staging
    /// file; callers that need overwrite safety should use `write_file`.
    fn write_file_direct(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let escaped_path = Self::py_escape_single_quoted(path);
        const CHUNK_SIZE: usize = 2048;

        if data.is_empty() {
            // Ensure the file exists and is empty.
            let code = format!(
                concat!("p = '{}'\n", "with open(p, 'wb') as f:\n", "    pass\n",),
                escaped_path
            );
            let result = self.exec_raw_classic(code)?;
            if !result.stderr.is_empty() {
                return Err(MicroPythonError::Remote(result.stderr));
            }
            return Ok(());
        }

        for (i, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
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

            let result = self.exec_raw_classic(code)?;
            if !result.stderr.is_empty() {
                return Err(MicroPythonError::Remote(result.stderr));
            }
        }

        Ok(())
    }

    /// Convenience helper: write a UTF-8 text file to the device.
    pub fn write_text_file(&mut self, path: &str, contents: &str) -> Result<()> {
        self.write_file(path, contents.as_bytes())
    }

    /// Remove a file on the device.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!("import os\n", "p = '{}'\n", "os.remove(p)\n",),
            escaped
        );
        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }
        Ok(())
    }

    /// Create a directory on the device.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!("import os\n", "p = '{}'\n", "os.mkdir(p)\n",),
            escaped
        );
        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }
        Ok(())
    }

    /// Remove an empty directory on the device.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!("import os\n", "p = '{}'\n", "os.rmdir(p)\n",),
            escaped
        );
        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }
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
        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }
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
