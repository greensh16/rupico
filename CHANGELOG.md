# Changelog

Notable changes to rupico. Versions follow [semantic versioning](https://semver.org);
while the major version is `0`, minor bumps may contain breaking changes.

## 0.3.0

The 0.3 milestone: the desktop app no longer freezes, and a cluster of
correctness defects found by review are fixed.

### Added

- **A REPL in the desktop app.** The bottom dock is now tabbed — **Output** and
  **REPL** — and the REPL runs what you type on the board over the connection
  the rest of the app already holds, so the prompt, the Run button and the file
  tree share the board's globals. Enter submits, Shift-Enter breaks the line,
  Up and Down walk history, and ⌘L (Ctrl-L) jumps to the prompt from anywhere.
  Entries are compiled in interactive mode, so an expression echoes its value
  the way a prompt should: `machine.freq()` prints, `x = 9` does not. Firmware
  without `compile` degrades to running the entry without the echo.
- **`rupico rm --recursive`** removes a directory and everything inside it in
  one round trip, and says what it removed. The GUI's delete dialog offers the
  same as a checkbox. Plain `rm` on a directory now explains which flag it
  needs instead of failing with a bare errno.
- **The desktop app keeps drawing while the board is busy.** Serial I/O moved
  to its own thread: the status bar shows what is running with a Cancel
  button, the sync panel lists each decision as it is made, and a REPL entry
  shows as pending until its result arrives. Cancel interrupts a running
  program with Ctrl-C, or stops a sync at the next file boundary.
- **`MicroPythonDevice::take_remote_warnings`**, for anything the board printed
  to stderr without raising. The GUI shows these in the REPL transcript.

### Fixed

- **A device warning no longer fails the operation it interrupted.** Any
  output on stderr was treated as a raised exception, so a board that logs
  during `os.listdir` could not be listed at all. Only a traceback — or a
  `SomeError:` line — counts as a failure now.
- **rupico's helper programs no longer leave anything in your namespace.**
  Every filesystem operation bound its temporaries (`p`, `f`, `src`, `b`) in
  the same `__main__` your script and the REPL prompt then see, so a script
  whose own first line was `src = open(...)` found someone else's `src`
  already there. Helper programs now run inside a function.
- **`ls -R` is one round trip instead of one per directory**, sharing the
  walk sync has always used — without paying for sync's per-file hashing. The
  GUI file tree uses it too, and no longer stops at four levels deep.
- **Sync reads each local file once**, not once to hash and again to upload.

### Changed

- **`SyncOptions` gained `cancel`** (an `AtomicBool` a caller can set from
  another thread) and **`SyncOutcome` gained `cancelled`**. A cancelled sync
  returns a partial manifest, which must not be saved as a baseline.
- **`list_tree(root, TreeOptions)`** joins `list_tree_hashed`, which stays as
  it was. Hashes and mtimes are each opt-in, because both cost the board
  something.

## 0.2.0

The distribution release: rupico is on crates.io, and the desktop app is now a
real application on each platform rather than a bare executable.

### Added

- **Published on [crates.io](https://crates.io/crates/rupico)** —
  `cargo install rupico`.
- **macOS: the GUI ships as `rupico.app`.** Double-clicking a bare Unix
  executable in Finder launches it through Terminal, so a terminal window
  appeared in front of the app. The release archive now contains a proper
  bundle. It is ad-hoc signed — required for Apple silicon to run it at all —
  but not notarised, so the first launch needs right-click → **Open**.
- **Linux: a `rupico.desktop` entry**, so the GUI appears in application
  launchers.

### Changed

- **The desktop app is behind a `gui` Cargo feature, off by default.** `eframe`
  pulled in roughly 140 of the crate's dependencies, which everyone installing
  the CLI was compiling for no reason. `cargo install rupico` now builds 104
  dependency crates instead of 243. Use `cargo install rupico --features gui`
  for both, or download a release archive, which still contains both binaries.
- **Windows: release builds no longer open a console window** alongside the
  GUI. Debug builds keep it, so panics stay visible while developing.

### Fixed

- The release workflow no longer requests the retired `macos-13` runner, which
  queued forever and silently prevented any release from publishing. Intel
  macOS is now cross-compiled from the Apple-silicon runner, with a check that
  the built binary really is the architecture its filename claims.
- Release jobs have a timeout, so an unschedulable job fails instead of hanging.
- A release is refused if the git tag and `Cargo.toml` version disagree.

## 0.1.0

First release.

- Raw-REPL core: connect, execute, transfer files, soft reboot, interrupt a
  running program. Negotiates MicroPython's raw-paste protocol where available
  and falls back to classic raw REPL where it is not.
- CLI: device discovery, filesystem browsing, upload/download, running scripts,
  and project sync with `.rupico.toml` workspaces, conflict detection, `--json`
  output and meaningful exit codes.
- Desktop app: device file tree, Python editor with syntax highlighting,
  run/stop controls, output dock, and a sync panel.
- Self-update: `rupico update` and the GUI's update dialog, verifying downloads
  against each release's published `SHA256SUMS` before replacing anything.
- Library: `rupico::micropython` for the device protocol, `rupico::sync` for
  directory sync.
- Prebuilt binaries for macOS (Intel and Apple silicon), Linux and Windows.
