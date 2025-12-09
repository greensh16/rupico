# rupico

Rust MicroPython helper for boards like the Raspberry Pi Pico.

`rupico` is a small Rust tool and library that makes it easy to talk to
MicroPython boards over a serial connection. It focuses on:

- A **reliable raw REPL core** (connect, exec code, soft reboot, stop programs).
- A **CLI** for everyday tasks:
  - Discover devices, browse the device filesystem, upload/download files.
  - Run scripts (already on the device or from local sources).
  - Sync a local project tree with a directory on the device.
  - Initialize starter examples.
- A **workspace config** (`.rupico.toml`) for opinionated project↔device sync.

The code is structured as:

- A reusable library: `rupico::micropython::MicroPythonDevice`.
- A CLI binary: `rupico` (in `src/main.rs`).
- An experimental GUI binary (currently disabled on some macOS versions due to upstream windowing/runtime issues; the CLI is the supported interface).

---

## Requirements

- A recent Rust toolchain (`rustup` recommended).
- A MicroPython-capable board (e.g. Raspberry Pi Pico) flashed with MicroPython.
- A serial port device for the board, e.g. on macOS something like
  `/dev/cu.usbmodemXXXX`.

---

## Installation

Clone the repo and install the CLI from source:

```bash
cargo install --path .
```

This builds and installs the `rupico` binary into `~/.cargo/bin`.

From inside the repository you can also run it directly with:

```bash
cargo run -- --help
```

---

## CLI overview

The CLI is defined in `src/main.rs` and exposed as the `rupico` binary.

### Global flags

These apply to all subcommands:

- `-p, --port <PORT>`
  - Serial port path for commands that talk to a device.
  - Required for most commands except `ports` and a few purely local ones
    (like `init`).
- `-q, --quiet`
  - Suppress non-essential output (handy for scripting / CI).
  - Errors and primary results are still printed.
- `--json`
  - Emit machine-readable JSON for supported commands.
  - Currently used by: `ports`, `ls`, and the sync commands.

### Listing ports

List available serial ports and detect MicroPython devices:

```bash
rupico ports
rupico ports --only-micropython
rupico ports --json
```

- Text mode prints one port per line.
- MicroPython-looking ports are marked as `[mp] /dev/...`.
- `--only-micropython` filters to just those.
- JSON mode prints an array of objects like:

```json
[
  { "port": "/dev/cu.usbmodem1234", "is_micropython": true },
  { "port": "/dev/cu.usbserial5678", "is_micropython": false }
]
```

### Device filesystem commands

All of these require `--port`.

#### List directories: `ls`

```bash
rupico --port /dev/cu.usbmodemXXXX ls               # list "/"
rupico --port /dev/cu.usbmodemXXXX ls /app         # list a directory
rupico --port /dev/cu.usbmodemXXXX ls -R /         # recursive tree
rupico --port /dev/cu.usbmodemXXXX ls -l /         # long listing
rupico --port /dev/cu.usbmodemXXXX ls --json /app  # JSON output
```

Flags:

- `-R, --recursive` – recursively list subdirectories.
- `-l, --long` – show type, size, and modification time.
- `--json` – JSON representation instead of text.

JSON example (non-recursive):

```json
[
  {
    "path": "/main.py",
    "name": "main.py",
    "is_dir": false,
    "size": 123,
    "modified": 1733550000
  }
]
```

#### View and manage files

- Print a file:

  ```bash
  rupico --port /dev/cu.usbmodemXXXX cat /main.py
  ```

- Upload a file (host → device):

  ```bash
  rupico --port /dev/cu.usbmodemXXXX put ./blink.py /blink.py
  ```

- Download a file (device → host):

  ```bash
  rupico --port /dev/cu.usbmodemXXXX get /blink.py ./blink.py
  ```

- Remove a file:

  ```bash
  rupico --port /dev/cu.usbmodemXXXX rm /old.py
  ```

- Create a directory:

  ```bash
  rupico --port /dev/cu.usbmodemXXXX mkdir /app
  ```

Under the hood these use the `micropython` library to run small Python snippets
on the device.

### Running code on the device

There are several ways to execute Python code.

#### Run a file already on the device: `run`

```bash
rupico --port /dev/cu.usbmodemXXXX run /main.py
```

- In normal mode, the CLI prints two sections:
  - `--- stdout ---`
  - `--- stderr ---`
- If stdout is a TTY and you did not pass `--quiet`, these headings and stderr
  are colorized.
- With `--quiet`, it just prints `stdout` followed by `stderr` with no labels.

#### Run a local file without permanently uploading: `run-local`

```bash
rupico --port /dev/cu.usbmodemXXXX run-local ./script.py
```

- Reads `script.py` from your machine.
- Uploads it to a temporary path on the device.
- Executes it, then shows stdout/stderr as with `run`.

This is a good workflow when you want to edit code locally but run it on the
board.

#### Run a small snippet: `run-snippet`

```bash
rupico --port /dev/cu.usbmodemXXXX run-snippet "import sys; print(sys.implementation)"
```

Runs the inline code under raw REPL. Output formatting is the same as for
`run` and `run-local`.

#### Run as `main.py` / soft reboot

Two commands help with the classic `main.py` flow:

- `flash-main` – upload a local script as `main.py`:

  ```bash
  rupico --port /dev/cu.usbmodemXXXX flash-main ./main.py
  ```

- `run-main` – soft reboot so `boot.py` / `main.py` run:

  ```bash
  rupico --port /dev/cu.usbmodemXXXX run-main
  ```

### Stopping a running program

If a user program is stuck or running in a loop, you can stop it:

```bash
rupico --port /dev/cu.usbmodemXXXX stop
```

This sends a couple of Ctrl-C bytes over serial. In non-quiet mode it prints a
single line to stderr indicating that the stop signal was sent.

### Interactive REPL proxy

`rupico` can act as a very simple REPL bridge between your terminal and the
board:

```bash
rupico --port /dev/cu.usbmodemXXXX repl
```

- It opens the port, sends a newline, and then:
  - Spawns a thread to continuously read from the device and print to stdout.
  - Reads lines from stdin and forwards them to the device.
- Exit by closing stdin (Ctrl-D) or by killing the process.

This is intended as a bare-bones helper, not a full-featured editor.

---

## Project templates

`rupico` can scaffold small example projects for common patterns using the
`init` command. These templates are purely local (no device required).

```bash
# Create a new directory ./blink with blink.py inside
rupico init blink

# Create a blink example in ./my-blink instead
rupico init blink my-blink

# Button + UART templates
rupico init button
rupico init uart
```

Available templates:

- `blink` – blink an on-board LED.
- `button` – poll a button and print when pressed.
- `uart` – basic UART echo loop.

Each template:

- Ensures the target directory either doesn’t exist (and is created) or exists
  but is empty.
- Writes a single `.py` file into that directory.

---

## Syncing projects

`rupico` has two layers of sync support:

1. Low-level "one-off" sync commands where you manually specify paths.
2. A higher-level workspace concept driven by `.rupico.toml`.

### 1. Direct sync commands

These commands mirror a directory tree in one direction.

#### Local → device: `sync-to-device`

```bash
rupico --port /dev/cu.usbmodemXXXX sync-to-device \
  --local ./src \
  --remote /app
```

Flags:

- `--delete` – delete remote files/dirs that are not present locally
  (mirror mode).
- `--dry-run` – show what would be changed, but do not actually modify the
  device.
- `-v, --verbose` – print detailed decisions (uploads, skips, deletions).
- `--ignore PATTERN` – additional ignore patterns to skip files and dirs.
- `--json` – emit a structured summary.

Sync decisions are based on:

- Directory recursion on both sides.
- A simple diff that considers both file size and (when available) modification
  time.

JSON output is a summary of actions, e.g.:

```json
{
  "direction": "to_device",
  "local_root": "/path/to/src",
  "remote_root": "/app",
  "actions": [
    {
      "op": "upload",
      "local": "/path/to/src/main.py",
      "remote": "/app/main.py",
      "dry_run": false
    },
    {
      "op": "skip_upload",
      "local": "/path/to/src/util.py",
      "remote": "/app/util.py",
      "dry_run": false
    }
  ]
}
```

#### Device → local: `sync-from-device`

```bash
rupico --port /dev/cu.usbmodemXXXX sync-from-device \
  --remote /app \
  --local ./src
```

Flags are symmetric to `sync-to-device`:

- `--delete` – remove local files/dirs that are not on the device.
- `--dry-run`, `-v/--verbose`, `--ignore`, `--json` – same semantics.

JSON summary looks like:

```json
{
  "direction": "from_device",
  "local_root": "/path/to/src",
  "remote_root": "/app",
  "actions": [
    {
      "op": "download",
      "local": "/path/to/src/main.py",
      "remote": "/app/main.py",
      "dry_run": false
    },
    {
      "op": "skip_download",
      "local": "/path/to/src/util.py",
      "remote": "/app/util.py",
      "dry_run": false
    }
  ]
}
```

#### Ignore patterns

Sync respects a few sources of ignore patterns:

1. Built-in patterns:
   - `.git`
   - `__pycache__`
   - `.venv`
   - `target`
2. `.rupicoignore` at the local root of the sync
   - Simple line-based list; blank lines and `#` comments are ignored.
   - Currently treated as plain substring matches on the relative path.
3. Extra patterns from `--ignore PATTERN`, which can be passed multiple times.

Ignored paths are skipped on both upload and download. In `sync-from-device`,
ignored entries appear in JSON output with `op = "ignore"`.

### 2. Workspace sync with `.rupico.toml`

For a more opinionated workflow, you can define a project-level config file.

Create `.rupico.toml` in your project root:

```toml
# Local directory (relative to this file) to treat as the project root
local_root = "src"

# Remote directory on the device that should mirror the project
remote_root = "/app"
```

Then use the `sync` subcommand instead of the low-level ones:

```bash
# Upload src → /app according to .rupico.toml
rupico --port /dev/cu.usbmodemXXXX sync

# Download /app → src according to .rupico.toml
rupico --port /dev/cu.usbmodemXXXX sync --from-device
```

Behavior:

- `rupico` walks up from the current directory until it finds a `.rupico.toml`.
- Uses its directory as the workspace root.
- Applies the same flags as the direct sync commands:
  - `--delete`, `--dry-run`, `-v/--verbose`, `--ignore`, `--json`.
- Ignores are based on the derived local root (e.g. `<workspace>/src`).

#### Last-sync tracking & conflict warnings

The workspace sync mode maintains a small state file
`<workspace>/.rupico-state.json` that records the last successful sync times
(Unix seconds) in each direction:

```json
{
  "last_sync_to_device": 1733551000,
  "last_sync_from_device": 1733552000
}
```

This allows `rupico sync` to detect potential conflicts where both sides have
changed since the last sync:

- If both local and remote have modification times **after** the last sync in
  that direction, and those times differ, the CLI:
  - Prints a warning on stderr in text mode.
  - Emits a JSON action with `op = "conflict"` in `--json` mode.

Example warning (upload direction):

```text
sync-to-device: WARNING: both local and remote changed since last sync for /app/main.py
```

Corresponding JSON action:

```json
{
  "op": "conflict",
  "local": "/path/to/workspace/src/main.py",
  "remote": "/app/main.py",
  "dry_run": false
}
```

**Important:** `rupico` does not auto-resolve conflicts. It only detects and
surfaces them so you can inspect and decide whether to overwrite or pull the
other side.

- Direct `sync-to-device` / `sync-from-device` subcommands do **not** use
  last-sync tracking and therefore do not emit conflict warnings.
- Workspace `sync` does.

---

## Library usage (Rust)

If you want to embed `rupico` functionality into your own Rust code, you can
use the `micropython` module directly.

Very rough sketch (error handling omitted for brevity):

```rust
use rupico::micropython::{MicroPythonDevice, Result};
use std::time::Duration;

fn example() -> Result<()> {
    let mut dev = MicroPythonDevice::open("/dev/cu.usbmodemXXXX", 115_200, Duration::from_secs(3))?;

    dev.enter_raw_repl()?;
    let res = dev.run_snippet("print('hello from device')")?;
    println!("stdout: {}", res.stdout);
    println!("stderr: {}", res.stderr);
    dev.exit_raw_repl()?;

    Ok(())
}
```

Key capabilities of `MicroPythonDevice`:

- Connection & REPL control:
  - `connect(path: &str)` – convenience constructor.
  - `enter_raw_repl()`, `exit_raw_repl()`.
  - `soft_reboot()` and `run_main()`.
  - `stop_current_program()` and `interrupt()`.
- File operations:
  - `list_dir(path) -> Vec<RemoteEntry>`
  - `read_file(path) -> Vec<u8>` / `read_text_file(path) -> String`
  - `write_file(path, &[u8])` / `write_text_file(path, &str)`
  - `remove(path)`, `mkdir(path)`, `rmdir(path)`
- Execution helpers:
  - `exec_raw(code) -> ExecResult { stdout, stderr }`
  - `run_snippet(source)`, `run_file(path)`, `flash_main_script(source)`

The library code hides the raw REPL framing, handles basic timeouts, and uses a
chunked / raw-paste-aware upload path for better performance on large files.

---

## GUI

There is an experimental GUI binary based on `eframe`/`egui` in
`src/bin/rupico_gui.rs`. Functionally it has roughly the same concepts as the
CLI (ports, file browser, editor, run/stop, flash main, run main), but **on
some recent macOS versions the underlying windowing stack (`winit`/`objc2`) can
panic at startup**.

Because this crash happens entirely inside the upstream GUI dependencies before
any `rupico` code runs, the GUI is considered unstable on those platforms. The
**CLI described above is the supported and recommended way to use `rupico`**
until the upstream stack is updated.

---

## Development & testing

From the repository root:

- Run tests:

  ```bash
  cargo test
  ```

- Build and run the CLI:

  ```bash
  cargo run -- --help
  ```

Bugs, suggestions, and contributions are welcome.
