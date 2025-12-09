# rupico Roadmap
High-level plan for building a Rust-based MicroPython GUI tool for boards like the Raspberry Pi Pico.

## Milestone 0: Solid core library
Goal: a reliable core that hides serial + raw-REPL details.

You already have:
- Connect to a board over serial.
- Enter and exit raw REPL.
- Execute code and parse stdout and stderr.

Refinements:
- ~~Harden `MicroPythonDevice`:~~
  - ~~Add reconnection logic after timeouts (interrupt, re-enter raw REPL).~~
  - ~~Clarify error types: distinguish protocol desync vs device not responding, general timeouts vs exec timeouts.~~
- ~~Define a clean public API:~~
  - ~~`connect(path) -> MicroPythonDevice`.~~
  - ~~`enter_raw_repl()`, `exit_raw_repl()`.~~
  - ~~`exec_raw(code) -> ExecResult { stdout, stderr }`.~~
  - ~~`soft_reboot()` for running `boot.py` and `main.py`.~~

## Milestone 1: File system operations on the Pico
Goal: read, write, list, and delete files via raw REPL, exposed as simple Rust methods.

Core operations to implement on top of `exec_raw`:
- ~~File and directory listing:~~
  - ~~`list_dir(path: &str) -> Result<Vec<RemoteEntry>>`.~~
  - ~~`RemoteEntry { name, is_dir, size, modified: Option<…> }`.~~
  - ~~Use `uos.listdir()` and optionally `uos.stat()` in the Python snippets.~~
- ~~Read a file:~~
  - ~~`read_file(path: &str) -> Result<Vec<u8>>` (or `String` for text).~~
  - ~~Use Python snippets that read the file and base64-encode content for transport.~~
- ~~Write or overwrite a file:~~
  - ~~`write_file(path: &str, data: &[u8])`.~~
  - ~~Send base64-encoded chunks to Python that decodes and writes them.~~
  - ~~Later, consider a raw-paste style uploader for better performance.~~
  - ~~Note: current implementation chunks base64 writes for robustness; a true raw-paste uploader is still future work.~~
  - Raw-paste transport for `exec_raw` is now implemented; chunked file writes
    run over raw-paste when supported, so a separate dedicated uploader is less
    critical.
- ~~Delete and mkdir:~~
  - ~~`remove(path: &str)` and `mkdir(path: &str)` using `uos.remove` and `uos.mkdir`.~~
- ~~High-level helpers:~~
  - ~~`put_script(local_source: &str, remote_path: &str)`.~~
  - ~~`get_script(remote_path: &str) -> String`.~~

Testing:
- ~~Unit tests with a fake MicroPython device that feeds canned raw-REPL responses.~~ (partially covered by unit tests for framing and JSON/base64 helpers)
- Manual tests against a real Pico.
Deliverable: full Pico filesystem control from a CLI, no GUI yet.

## Milestone 2: Script execution model
Goal: clear separation between temporary runs and running stored files.

APIs:
- ~~Run an in-memory script:~~
  - ~~`run_snippet(source: &str) -> ExecResult`.~~
  - ~~Uses raw REPL; does not persist to device.~~
- ~~Run a file already on device:~~
  - ~~`run_file(path: &str) -> ExecResult`.~~
  - Implementation examples: `exec(open(path).read(), {})` or `import foo; foo.main()`.
- ~~Run as main program:~~
  - ~~`flash_as_main(local_path: &str)` uploads a script as `main.py`.~~
  - ~~`run_main()` soft-reboots so MicroPython runs `boot.py` and `main.py`.~~
- ~~Interrupt and stop:~~
  - ~~`stop_current_program()` sends Ctrl-C a few times and cleanly re-enters raw REPL.~~

Deliverable: upload a script, run it, capture stdout and stderr, and stop it when needed.

## Milestone 3: CLI front-end
Goal: a `rupico` CLI that exercises the core library and is useful for debugging and power users.

Suggested subcommands:
- ~~`rupico ports` to list serial ports and detect Pico-like boards.~~
- ~~`rupico ls <dir>` to list device directories.~~
- ~~`rupico cat <remote-path>` to print a device file.~~
- ~~`rupico put <local-path> <remote-path>` to upload a file.~~
- ~~`rupico get <remote-path> <local-path>` to download a file.~~
- ~~`rupico rm <remote-path>` to delete a file.~~
- ~~`rupico run <remote-path>` to run a file on the device.~~
- ~~`rupico run-snippet "<code>"` to run quick one-off code.~~
- ~~`rupico repl` for an interactive REPL proxy between stdin/stdout and the device.~~

Benefits:
- Stabilizes core functionality and error messages.
- Provides a non-GUI way to use rupico for scripting and CI.

## Milestone 4: GUI foundation
Goal: minimal but functional desktop app that can connect, edit a script, upload it, run it, and show output.

Architecture:
- ~~Split into:~~
  - ~~`rupico-core`: library with MicroPython and filesystem logic.~~
  - ~~`rupico-gui`: GUI binary depending on `rupico-core`.~~
- ~~Use `eframe` or `egui` for the UI.~~

Initial layout:
- ~~Top bar:~~
  - ~~Serial port dropdown.~~
  - ~~Connect and disconnect buttons.~~
  - ~~Connection status indicator.~~
- ~~Main area:~~
  - Left: placeholder for future file browser.
  - ~~Right top: code editor (multi-line text area) for a single script.~~
  - ~~Right bottom: console output (read-only text area with a clear button).~~

Basic interactions:
- ~~Connect:~~
  - ~~Open serial port and enter raw REPL using `rupico-core`.~~
  - Optionally show device info such as implementation and version.
- ~~Run script:~~
  - ~~Take editor contents and upload to a temporary path, for example `/tmp/rupico_temp.py`.~~
  - ~~Call `run_file` on that path.~~
  - ~~Append stdout and stderr to console.~~
- Background tasks:
  - Use a worker thread or async tasks for serial I/O to keep the UI responsive.

Deliverable: an IDE-like window that can edit and run a single script on the Pico.

## Milestone 5: Device file browser and direct editing
Goal: view, open, edit, create, and delete files on the Pico directly from the GUI.

Features:
- ~~File tree view:~~
  - ~~On refresh or connect, call `list_dir("/")` recursively up to a safe depth.~~
  - ~~Display a tree in the left pane with files and directories.~~
- ~~Open and save:~~
  - ~~Open from device reads a file and loads it into the editor.~~
  - ~~Save to device writes the editor contents back to the same path.~~
- Create, rename, and delete:
  - ~~Create new file: prompt for path and open an empty buffer.~~
  - ~~Delete: confirm and call `remove(path)`, then refresh tree.~~ (implemented in `rupico_gui` with a confirmation dialog and tree refresh)
  - ~~Rename: allow renaming files and directories from the GUI.~~ (implemented in `rupico_gui` with a rename dialog that updates the tree and any open tabs)
- ~~Multiple open files (optional but useful):~~
  - ~~Tabbed editor with each tab mapping to a remote path.~~
  - ~~Track dirty state per tab.~~

Deliverable: full device file management from the GUI. (Achieved: GUI file tree + multi-tab editor with full CRUD on device files.)

## Milestone 6: Improved UX for running scripts
Goal: make running scripts obvious and friendly, especially for beginners.

Ideas:
- Run current file vs run as main:
  - ~~Run current file: save if dirty, then `run_file(path)`.~~ (CLI has `run` and `run-local` for this flow; GUI `Run` saves the active tab if bound to a path and then calls `run_file`.)
  - ~~Flash as `main.py`: copy current buffer to `main.py`, then soft-reboot.~~ (CLI has `flash-main` + `run-main`.)
- Output panel:
  - ~~Separate sections or colors for stdout and stderr.~~ (CLI prints labeled stdout/stderr sections, with colors when writing to a TTY; GUI labels stdout/stderr separately.)
  - ~~Clear error indicators when stderr is non-empty.~~ (GUI shows a red warning label when stderr is non-empty.)
- ~~Stop button:~~
  - ~~Send Ctrl-C to stop the current program.~~ (CLI has `rupico stop`; GUI has a `Stop` button wired to `stop_current_program`.)
  - ~~Show a "Program stopped" message and re-enter raw REPL.~~ (GUI shows "Program stopped" in the output and status bar and re-enters raw REPL.)
- ~~Status bar:~~
  - ~~Show current device and port.~~ (GUI bottom bar shows current port and connection status.)
  - ~~Show last operation message and simple progress indication.~~ (GUI bottom bar shows the last status message, e.g. "Running file ...", "Saved ...", "Run finished with errors".)
## Milestone 7: Quality, tests, and packaging
Goal: make rupico feel like a polished, reliable tool.

Tasks:
- Automated tests:
  - ~~Unit tests for raw-REPL framing and parsing (stdout and stderr separation).~~
  - Unit tests for file transfer helpers using a fake transport.
  - ~~Optional integration tests that talk to a real device.~~ (partially covered by CLI smoke tests that exercise the binary)
- Configuration and persistence:
  - Remember last selected serial port.
  - Remember window layout, open files, and font settings.
  - ~~Store simple config (for example, JSON or TOML) in a user config directory.~~ (CLI now uses `.rupico.toml` and `.rupico-state.json` for workspace and
    last-sync configuration; GUI-specific configuration is still future work.)
- Packaging:
  - Build a bundled macOS app using a suitable utility.
  - Later, add Windows and Linux builds.

## Milestone 8: Future enhancements
Ideas for later versions that are not required for the initial release:

- GUI REPL console:
  - An input box that sends a line at a time to the friendly REPL and displays responses.
- ~~Device discovery:~~
  - ~~Auto-detect MicroPython devices by probing ports and running a small identification snippet.~~ (CLI `ports` probes ports and reports `is_micropython`, with `--only-micropython` and JSON output)
- ~~Templates and examples:~~
  - ~~Starter templates for common Pico programs such as blinking an LED or reading a button.~~ (CLI `init blink`, `init button`, and `init uart` create example programs.)
- Workspace and sync:
  - ~~A local project folder that mirrors some portion of the device filesystem.~~ (CLI `sync` plus `.rupico.toml` define a workspace-local project root.)
  - ~~Commands to sync local to device and device to local.~~
  - Note: current CLI has `sync-to-device` / `sync-from-device` with `--delete`, `--dry-run`, and ignore patterns (built-ins, `.rupicoignore`, and `--ignore`),
    plus a higher-level `sync` subcommand driven by `.rupico.toml` and
    `.rupico-state.json` with basic conflict detection.
- Firmware flashing:
  - Integrate UF2 flashing with user guidance for BOOTSEL mode.

## Summary
Following these milestones, you first stabilize a core library and CLI, then layer on a cross-platform GUI that can:
- Browse and manage files on a MicroPython device.
- Edit scripts and save them directly to the device.
- Run scripts and display stdout and stderr.
- Provide a friendly experience similar to Mu, but implemented entirely in Rust.