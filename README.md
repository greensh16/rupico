# rupico

[![CI](https://github.com/greensh16/rupico/actions/workflows/ci.yml/badge.svg)](https://github.com/greensh16/rupico/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/greensh16/rupico?label=release)](https://github.com/greensh16/rupico/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/greensh16/rupico/total)](https://github.com/greensh16/rupico/releases)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-blue)](https://github.com/greensh16/rupico/releases/latest)
[![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-dea584)](https://www.rust-lang.org)
[![License](https://img.shields.io/github/license/greensh16/rupico)](LICENSE)

A Rust library, CLI and desktop app for working with MicroPython boards — the
Raspberry Pi Pico and anything else that speaks the raw REPL over serial.

The loop it exists for: **edit code on your machine, put it on the board, run
it, see the output.**

```bash
rupico ports                                   # find the board
rupico -p /dev/cu.usbmodem101 sync             # push your project
rupico -p /dev/cu.usbmodem101 run /main.py     # run it
```

📖 **[Full documentation is in the wiki.](https://github.com/greensh16/rupico/wiki)**

---

## What you get

- **A reliable raw-REPL core** — connect, execute, transfer files, soft reboot,
  interrupt a runaway program. Uses MicroPython's fast raw-paste protocol where
  the firmware supports it, and falls back where it doesn't.
- **A CLI** — device discovery, filesystem browsing, upload/download, running
  scripts, and project sync. `--json` output and meaningful exit codes for
  scripting.
- **A desktop app** — file tree, Python editor with syntax highlighting,
  run/stop, output pane, and a sync panel.
- **A Rust library** — `rupico::micropython` for the device protocol,
  `rupico::sync` for directory sync. Both front ends are built on exactly these.

## Requirements

- Rust **1.95 or newer** (enforced via `rust-version` in `Cargo.toml`).
- A board flashed with MicroPython.
- A USB cable that carries data — charge-only cables are a common cause of "no
  ports found".

## Install

Prebuilt binaries for macOS (Intel and Apple silicon), Linux and Windows are
attached to each [release](https://github.com/greensh16/rupico/releases).
Download, unpack, and put `rupico` on your `PATH`.

The desktop app ships as `rupico.app` on macOS (drag it to `/Applications`) and
as `rupico_gui` plus a `.desktop` entry on Linux. On macOS the app is ad-hoc
signed but not notarised, so the first launch needs right-click → **Open** —
see [The GUI](https://github.com/greensh16/rupico/wiki/The-GUI).

Or install the CLI from crates.io:

```bash
cargo install rupico                    # CLI only
cargo install rupico --features gui     # CLI plus the desktop app
```

The desktop app is behind the `gui` feature because it pulls in a graphics
stack that more than doubles the dependency count; the downloadable release
archives above already contain both binaries.

To build from source:

```bash
git clone https://github.com/greensh16/rupico
cd rupico
cargo run -- --help                       # the CLI
cargo run --features gui --bin rupico_gui # the desktop app
```

## Quick start

```bash
rupico ports
# [mp] /dev/cu.usbmodem101

P=/dev/cu.usbmodem101

rupico -p $P ls /                          # browse the device
rupico -p $P run-snippet "print('hi')"     # run a snippet
rupico -p $P put ./blink.py /blink.py      # upload a file
rupico -p $P run-local ./blink.py          # run a local file without keeping it
rupico -p $P flash-main ./blink.py         # make it the boot program
```

On macOS use the `/dev/cu.*` port, not `/dev/tty.*` — the latter blocks waiting
for carrier detect.

Port detection is **passive**: ports are classified by USB vendor ID and
nothing is ever written to them, so unrelated serial hardware is left alone.
`rupico ports --probe` is the explicit opt-in that does write.

→ [Getting Started](https://github.com/greensh16/rupico/wiki/Getting-Started)
· [CLI Reference](https://github.com/greensh16/rupico/wiki/CLI-Reference)

## Syncing a project

Drop a `.rupico.toml` in your project root:

```toml
local_root = "src"
remote_root = "/app"
```

```bash
rupico -p $P sync --dry-run   # see what would change
rupico -p $P sync             # upload  src/ -> /app
rupico -p $P sync --from-device
```

Only files whose contents actually differ are copied — comparison is by sha256,
never by timestamp, because a Pico has no battery-backed clock. A file edited on
both sides since the last sync is left untouched and reported as a conflict.

→ [Syncing](https://github.com/greensh16/rupico/wiki/Syncing)

## Using it from Rust

```rust
use rupico::micropython::{MicroPythonDevice, Result};

fn main() -> Result<()> {
    let mut dev = MicroPythonDevice::connect("/dev/cu.usbmodem101")?;
    dev.enter_raw_repl()?;
    let res = dev.run_snippet("print('hello from the board')")?;
    println!("{}", res.stdout);
    dev.exit_raw_repl()
}
```

→ [Library API](https://github.com/greensh16/rupico/wiki/Library-API)

## Staying up to date

```bash
rupico update --check    # is there a newer release?
rupico update            # download, verify and replace this executable
```

Downloads are verified against the release's published `SHA256SUMS` before
anything is replaced, and the update refuses to proceed if that check cannot be
made. The desktop app has the same feature behind the version button on its
toolbar. Nothing is checked automatically — there is no background phone-home.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | rupico or the device failed |
| `2` | A workspace `sync` found conflicting changes and skipped them |
| `3` | Code run on the device raised |

## Documentation

| Page | |
|---|---|
| [Getting Started](https://github.com/greensh16/rupico/wiki/Getting-Started) | Install, find your board, first commands |
| [CLI Reference](https://github.com/greensh16/rupico/wiki/CLI-Reference) | Every command and flag |
| [Syncing](https://github.com/greensh16/rupico/wiki/Syncing) | Workspace config, ignore rules, conflicts |
| [The GUI](https://github.com/greensh16/rupico/wiki/The-GUI) | The desktop app |
| [How It Works](https://github.com/greensh16/rupico/wiki/How-It-Works) | Raw REPL, chunking, staged writes, hashing |
| [Library API](https://github.com/greensh16/rupico/wiki/Library-API) | Calling rupico from Rust |
| [Troubleshooting](https://github.com/greensh16/rupico/wiki/Troubleshooting) | When something goes wrong |
| [Development](https://github.com/greensh16/rupico/wiki/Development) | Building, testing, contributing |

## Development

```bash
cargo test --all-features           # no hardware required
cargo clippy --all-targets --all-features
cargo fmt
```

Tests run against a scripted in-memory serial port that emulates the raw REPL,
so the protocol layer is covered without a board attached. Device-side Python
snippets can only be verified against real hardware.

→ [Development](https://github.com/greensh16/rupico/wiki/Development)

## Status

The CLI is the supported interface and the right tool for bulk transfers. The
GUI is usable but still experimental — its device I/O runs on the UI thread, so
the window briefly freezes during long operations.

## Changelog

See [CHANGELOG.md](CHANGELOG.md).

## License

GPL-3.0. See [LICENSE](LICENSE).
