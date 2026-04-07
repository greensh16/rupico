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

/// Result of executing code in raw REPL mode.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
}

/// High-level handle to a MicroPython board speaking the raw REPL protocol over serial.
pub struct MicroPythonDevice {
    port: Box<dyn SerialPort>,
    read_timeout: Duration,
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
            read_timeout,
            rx_buf: Vec::new(),
            raw_paste_supported: None,
        })
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

        // Request raw REPL.
        self.port.write_all(&[CTRL_A])?;
        self.port.flush()?;

        let mut buf = [0u8; 256];
        let mut collected = Vec::new();
        let deadline = Instant::now() + self.read_timeout;

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
                "import uos, ujson\n",
                "p = '{}'\n",
                "ents = []\n",
                "for name in uos.listdir(p):\n",
                "    if p.endswith('/'):\n",
                "        full = p + name\n",
                "    else:\n",
                "        full = p + '/' + name\n",
                "    try:\n",
                "        st = uos.stat(full)\n",
                "        mode = st[0]\n",
                "        size = st[6]\n",
                "        is_dir = (mode & 0x4000) != 0\n",
                "        mtime = st[8] if len(st) > 8 else None\n",
                "    except OSError:\n",
                "        size = 0\n",
                "        is_dir = False\n",
                "        mtime = None\n",
                "    ents.append(dict(name=name, is_dir=is_dir, size=size, modified=mtime))\n",
                "print(ujson.dumps(ents))\n",
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

    /// Read a file as raw bytes from the device.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let escaped = Self::py_escape_single_quoted(path);
        let code = format!(
            concat!(
                "import ubinascii\n",
                "p = '{}'\n",
                "with open(p, 'rb') as f:\n",
                "    data = f.read()\n",
                "print(ubinascii.b2a_base64(data).decode(), end='')\n",
            ),
            escaped
        );

        let result = self.exec_raw_classic(code)?;
        if !result.stderr.is_empty() {
            return Err(MicroPythonError::Remote(result.stderr));
        }

        let b64 = result.stdout.trim();
        if b64.is_empty() {
            return Ok(Vec::new());
        }

        let decoded = B64.decode(b64).map_err(|e| {
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
    /// For robustness on small devices, this writes in chunks rather than
    /// constructing one huge base64 string on the MicroPython side.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
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
                    "import ubinascii\n",
                    "p = '{}'\n",
                    "b = '{}'\n",
                    "raw = ubinascii.a2b_base64(b)\n",
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
            concat!("import uos\n", "p = '{}'\n", "uos.remove(p)\n",),
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
            concat!("import uos\n", "p = '{}'\n", "uos.mkdir(p)\n",),
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
            concat!("import uos\n", "p = '{}'\n", "uos.rmdir(p)\n",),
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
                "import uos\n",
                "src = '{}'\n",
                "dst = '{}'\n",
                "uos.rename(src, dst)\n",
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

        // Classic raw-REPL: write code followed by Ctrl-D to end, then read
        // two 0x04-delimited blocks for stdout and stderr respectively.
        self.port.write_all(text.as_bytes())?;
        self.port.write_all(&[CTRL_D])?;
        self.port.flush()?;

        let raw_stdout = self.read_until_sentinel(CTRL_D)?;
        let raw_stderr = self.read_until_sentinel(CTRL_D)?;

        let stdout = Self::strip_ok_banner(raw_stdout)?;
        let stderr = String::from_utf8(raw_stderr)?;

        Ok(ExecResult { stdout, stderr })
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

        let header = self.read_exact_with_timeout(2)?;
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
        let win_bytes = self.read_exact_with_timeout(2)?;
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
                let ack = self.read_exact_with_timeout(1)?;
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

        let stdout = Self::strip_ok_banner(raw_stdout)?;
        let stderr = String::from_utf8(raw_stderr)?;

        Ok(Some(ExecResult { stdout, stderr }))
    }

    /// Read bytes from the serial port until we encounter the given
    /// sentinel byte, returning everything before it. Any bytes after the
    /// sentinel are kept in the internal buffer for future reads.
    fn read_until_sentinel(&mut self, sentinel: u8) -> Result<Vec<u8>> {
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + self.read_timeout;

        loop {
            if let Some(pos) = self.rx_buf.iter().position(|b| *b == sentinel) {
                let before = self.rx_buf[..pos].to_vec();
                // Keep everything after the sentinel in the buffer.
                let remaining = self.rx_buf.split_off(pos + 1);
                self.rx_buf = remaining;
                return Ok(before);
            }

            if Instant::now() >= deadline {
                return Err(MicroPythonError::ExecTimeout);
            }

            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    self.rx_buf.extend_from_slice(&buf[..n]);
                }
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read exactly `n` bytes from the serial port (ignoring any existing
    /// contents of `rx_buf`), or return `ExecTimeout` if that many bytes are
    /// not received before `read_timeout` elapses.
    fn read_exact_with_timeout(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + self.read_timeout;

        while out.len() < n {
            if Instant::now() >= deadline {
                return Err(MicroPythonError::ExecTimeout);
            }

            let want = std::cmp::min(buf.len(), n - out.len());
            match self.port.read(&mut buf[..want]) {
                Ok(m) if m > 0 => {
                    out.extend_from_slice(&buf[..m]);
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
    fn base64_roundtrip_works_with_b64_engine() {
        let data = b"hello world";
        let encoded = B64.encode(data);
        let decoded = B64.decode(&encoded).expect("base64 decode failed");
        assert_eq!(&decoded, data);
    }
}
