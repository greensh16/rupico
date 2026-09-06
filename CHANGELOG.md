# Changelog

Notable changes to rupico. Versions follow [semantic versioning](https://semver.org);
while the major version is `0`, minor bumps may contain breaking changes.

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
