use chrono::{TimeZone, Utc};
use clap::{Parser, Subcommand};
use rupico::micropython;
use rupico::micropython::join_remote_path;
use rupico::micropython::vid_looks_micropython;
use rupico::sync;
use rupico::update;
use serialport::available_ports;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "rupico",
    version,
    about = "Rust MicroPython helper for boards like the Pico"
)]
struct Cli {
    /// Serial port path for commands that talk to a device, e.g. /dev/cu.usbmodemXXXX.
    #[arg(short, long, global = true)]
    port: Option<String>,

    /// Suppress non-essential output (suitable for scripting/CI). Errors and
    /// primary command results are still printed.
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

    /// Emit machine-readable JSON for supported commands (for example, `ports`
    /// and `ls`). Other commands may ignore this flag for now.
    #[arg(long, global = true)]
    json: bool,

    /// Serial baud rate for device connections.
    #[arg(long, global = true, default_value_t = 115_200)]
    baud: u32,

    /// Idle read timeout in seconds for device operations. The timer resets
    /// whenever the device sends data, so long transfers are safe. Use 0 to
    /// wait forever (useful for long-running programs with `run`).
    #[arg(long = "timeout", global = true, default_value_t = 3)]
    timeout_secs: u64,

    #[command(subcommand)]
    command: Command,
}

/// Connection parameters shared by all device-facing commands.
struct DeviceOpts {
    port: String,
    baud: u32,
    /// Idle read timeout; `None` means wait forever.
    read_timeout: Option<Duration>,
}

#[derive(Subcommand)]
enum Command {
    /// List available serial ports.
    Ports {
        /// Only show ports that appear to be running MicroPython.
        #[arg(long = "only-micropython")]
        only_micropython: bool,
        /// Actively probe every serial port by opening it and running a
        /// small identification snippet. This finds boards behind generic
        /// USB-serial bridges (CP210x, CH340, FTDI) that the vendor ID
        /// cannot identify, but it writes a few bytes to each port, so
        /// only pass it when you know what else is plugged in. Without this
        /// flag detection is passive (USB vendor ID only) and never writes
        /// to any port.
        #[arg(long)]
        probe: bool,
    },

    /// List files in a directory on the device.
    Ls {
        /// Directory path on the device (default: "/").
        path: Option<String>,
        /// Recursively list subdirectories (like ls -R).
        #[arg(short = 'R', long)]
        recursive: bool,
        /// Show more information (size and modified time) per entry.
        #[arg(short = 'l', long)]
        long: bool,
    },

    /// Print a file from the device to stdout.
    Cat {
        /// Remote path on the device.
        path: String,
    },

    /// Upload a local file to the device.
    Put {
        /// Local path on the host.
        local: String,
        /// Remote path on the device.
        remote: String,
    },

    /// Download a file from the device to the host.
    Get {
        /// Remote path on the device.
        remote: String,
        /// Local path to write on the host.
        local: String,
    },

    /// Remove a file on the device.
    Rm {
        /// Remote path on the device.
        path: String,
    },

    /// Create a directory on the device.
    Mkdir {
        /// Directory path on the device.
        path: String,
    },

    /// Initialize a local project from a template (e.g. `blink`, `button`, `uart`).
    Init {
        /// Template name to use (currently `blink`, `button`, `uart`).
        template: String,
        /// Optional target directory. Defaults to the template name.
        dest: Option<String>,
    },

    /// Execute a Python file that is already stored on the device.
    Run {
        /// Remote path to a Python file on the device.
        path: String,
    },

    /// Execute a local Python file by uploading it to a temporary path and
    /// running it on the device.
    RunLocal {
        /// Local path to a Python file on the host.
        path: String,
    },

    /// Execute an inline Python snippet on the device.
    RunSnippet {
        /// Python code to execute.
        code: String,
    },

    /// Upload a local script as `main.py` on the device.
    FlashMain {
        /// Local path to a Python script to flash as main.py.
        local: String,
    },

    /// Soft reboot the device so that `boot.py` / `main.py` run.
    RunMain,

    /// Send Ctrl-C to stop any currently running user program on the device.
    Stop,

    /// Recursively sync a local directory to a device directory (upload only).
    SyncToDevice {
        /// Local directory on the host.
        local: String,
        /// Target directory on the device.
        remote: String,
        /// Delete remote files/dirs that are not present locally (mirror mode).
        #[arg(long)]
        delete: bool,
        /// Do not actually delete, just print what would be deleted.
        #[arg(long)]
        dry_run: bool,
        /// Verbose output about sync decisions (uploads, skips, deletions).
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Additional ignore patterns (matched on local relative paths), on
        /// top of built-in ignores like `.git` and `__pycache__`.
        #[arg(long = "ignore")]
        ignore: Vec<String>,
    },

    /// Recursively sync a device directory to a local directory (download only).
    SyncFromDevice {
        /// Source directory on the device.
        remote: String,
        /// Target directory on the host.
        local: String,
        /// Delete local files/dirs that are not present on the device (mirror mode).
        #[arg(long)]
        delete: bool,
        /// Do not actually delete, just print what would be deleted.
        #[arg(long)]
        dry_run: bool,
        /// Verbose output about sync decisions (downloads, skips, deletions).
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Additional ignore patterns (matched on local relative paths), on
        /// top of built-in ignores like `.git` and `__pycache__`.
        #[arg(long = "ignore")]
        ignore: Vec<String>,
    },

    /// Sync using workspace configuration from .rupico.toml.
    Sync {
        /// If set, sync from device → local (download). By default syncs
        /// local → device (upload).
        #[arg(long = "from-device")]
        from_device: bool,
        /// Delete files/dirs on the target side that are not present on the
        /// source side (mirror mode).
        #[arg(long)]
        delete: bool,
        /// Do not actually modify anything, just report what would change.
        #[arg(long)]
        dry_run: bool,
        /// Verbose output about sync decisions (uploads/downloads, skips, deletions).
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Additional ignore patterns (matched on local relative paths), on
        /// top of built-in ignores like `.git` and `__pycache__`.
        #[arg(long = "ignore")]
        ignore: Vec<String>,
        /// Overwrite files that changed on both sides since the last sync.
        /// Without this, conflicting files are left untouched and the command
        /// exits non-zero.
        #[arg(long)]
        force: bool,
    },

    /// Simple interactive REPL proxy.
    Repl,

    /// Update rupico to the latest published release.
    ///
    /// Downloads the release archive for this platform, verifies it against
    /// the release's SHA256SUMS, and replaces this executable.
    Update {
        /// Only report whether a newer version exists; install nothing.
        #[arg(long)]
        check: bool,
    },
}

/// Process exit code used when a sync finds files that changed on both sides.
const EXIT_CONFLICT: i32 = 2;

/// Process exit code used when code run on the device raised an exception.
const EXIT_DEVICE_ERROR: i32 = 3;

fn main() {
    match try_main() {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(e) => report_error_and_exit(e),
    }
}

fn report_error_and_exit(e: Box<dyn Error>) -> ! {
    {
        if let Some(mp) = e.downcast_ref::<micropython::MicroPythonError>() {
            use rupico::micropython::MicroPythonError;
            match mp {
                MicroPythonError::Remote(s) => {
                    eprintln!("device error:\n{s}");
                }
                MicroPythonError::Protocol(s) => {
                    eprintln!("internal protocol error in rupico: {s}");
                }
                MicroPythonError::HandshakeTimeout => {
                    eprintln!(
                        "timed out entering raw REPL. Is the device connected and running MicroPython?",
                    );
                }
                MicroPythonError::ExecTimeout => {
                    eprintln!("timed out waiting for the device to finish executing code.");
                }
                other => {
                    eprintln!("error: {other}");
                }
            }
        } else {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn try_main() -> Result<i32, Box<dyn Error>> {
    let cli = Cli::parse();
    let mut exit_code = 0;

    match &cli.command {
        Command::Ports {
            only_micropython,
            probe,
        } => {
            cmd_ports(&cli, *only_micropython, *probe)?;
        }
        Command::Ls {
            path,
            recursive,
            long,
        } => {
            let opts = device_opts(&cli)?;
            let path_str = path.as_deref().unwrap_or("/");
            cmd_ls(&opts, path_str, *recursive, *long, cli.json)?;
        }
        Command::Cat { path } => {
            let opts = device_opts(&cli)?;
            cmd_cat(&opts, path)?;
        }
        Command::Init { template, dest } => {
            cmd_init(template, dest.as_deref(), cli.quiet)?;
        }
        Command::Put { local, remote } => {
            let opts = device_opts(&cli)?;
            cmd_put(&opts, local, remote)?;
        }
        Command::Get { remote, local } => {
            let opts = device_opts(&cli)?;
            cmd_get(&opts, remote, local)?;
        }
        Command::Rm { path } => {
            let opts = device_opts(&cli)?;
            cmd_rm(&opts, path)?;
        }
        Command::Mkdir { path } => {
            let opts = device_opts(&cli)?;
            cmd_mkdir(&opts, path)?;
        }
        Command::Run { path } => {
            let opts = device_opts(&cli)?;
            exit_code = cmd_run(&opts, path, cli.quiet)?;
        }
        Command::RunLocal { path } => {
            let opts = device_opts(&cli)?;
            exit_code = cmd_run_local(&opts, path, cli.quiet)?;
        }
        Command::RunSnippet { code } => {
            let opts = device_opts(&cli)?;
            exit_code = cmd_run_snippet(&opts, code, cli.quiet)?;
        }
        Command::FlashMain { local } => {
            let opts = device_opts(&cli)?;
            cmd_flash_main(&opts, local)?;
        }
        Command::RunMain => {
            let opts = device_opts(&cli)?;
            cmd_run_main(&opts)?;
        }
        Command::Stop => {
            let opts = device_opts(&cli)?;
            cmd_stop(&opts, cli.quiet)?;
        }
        Command::SyncToDevice {
            local,
            remote,
            delete,
            dry_run,
            verbose,
            ignore,
        } => {
            let opts = device_opts(&cli)?;
            let sync_opts = sync::SyncOptions {
                delete: *delete,
                dry_run: *dry_run,
                ignore: ignore.clone(),
                // The low-level commands keep no baseline, so they never
                // detect conflicts and `force` is irrelevant to them.
                force: true,
            };
            run_sync(
                &opts,
                Path::new(local),
                remote,
                false,
                &sync_opts,
                None,
                cli.quiet,
                cli.json,
                *verbose && !cli.quiet,
            )?;
        }
        Command::SyncFromDevice {
            remote,
            local,
            delete,
            dry_run,
            verbose,
            ignore,
        } => {
            let opts = device_opts(&cli)?;
            let sync_opts = sync::SyncOptions {
                delete: *delete,
                dry_run: *dry_run,
                ignore: ignore.clone(),
                force: true,
            };
            run_sync(
                &opts,
                Path::new(local),
                remote,
                true,
                &sync_opts,
                None,
                cli.quiet,
                cli.json,
                *verbose && !cli.quiet,
            )?;
        }
        Command::Sync {
            from_device,
            delete,
            dry_run,
            verbose,
            ignore,
            force,
        } => {
            let opts = device_opts(&cli)?;
            let sync_opts = sync::SyncOptions {
                delete: *delete,
                dry_run: *dry_run,
                ignore: ignore.clone(),
                force: *force,
            };

            let cwd = std::env::current_dir()?;
            let (workspace_root, cfg) = sync::find_workspace_config(&cwd)?;
            let mut state = sync::load_workspace_state(&workspace_root);
            let local_root = workspace_root.join(&cfg.local_root);

            // Content-hash manifest from the last successful sync, used for
            // conflict detection.
            let baseline = state.files.clone();

            let outcome = run_sync(
                &opts,
                &local_root,
                &cfg.remote_root,
                *from_device,
                &sync_opts,
                Some(&baseline),
                cli.quiet,
                cli.json,
                *verbose && !cli.quiet,
            )?;

            if !*dry_run {
                state.files = outcome.manifest.clone();
                if let Some(t) = sync::now_secs() {
                    if *from_device {
                        state.last_sync_from_device = Some(t);
                    } else {
                        state.last_sync_to_device = Some(t);
                    }
                }
                sync::save_workspace_state(&workspace_root, &state)?;
            }

            if outcome.conflicts > 0 {
                eprintln!(
                    "{} file(s) changed on both sides and were left untouched. \
                     Inspect them, then re-run with --force to overwrite, or copy \
                     the side you want across by hand.",
                    outcome.conflicts
                );
                exit_code = EXIT_CONFLICT;
            }
        }
        Command::Repl => {
            let opts = device_opts(&cli)?;
            cmd_repl(&opts, cli.quiet)?;
        }
        Command::Update { check } => {
            exit_code = cmd_update(*check, cli.quiet)?;
        }
    }

    Ok(exit_code)
}

/// Build connection options from CLI flags, failing with a proper error
/// (rather than exiting) when `--port` is missing.
fn device_opts(cli: &Cli) -> Result<DeviceOpts, Box<dyn Error>> {
    let port = match &cli.port {
        Some(p) => p.clone(),
        None => return Err("--port is required for this command".into()),
    };
    let read_timeout = if cli.timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(cli.timeout_secs))
    };
    Ok(DeviceOpts {
        port,
        baud: cli.baud,
        read_timeout,
    })
}

/// Timeout used for the raw-REPL handshake. Kept finite even when the exec
/// timeout is disabled so a dead device still fails fast.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

fn open_device(opts: &DeviceOpts) -> micropython::Result<micropython::MicroPythonDevice> {
    let mut dev = micropython::MicroPythonDevice::open(&opts.port, opts.baud, HANDSHAKE_TIMEOUT)?;
    dev.set_read_timeout(opts.read_timeout);
    Ok(dev)
}

fn with_raw_device<F, T>(opts: &DeviceOpts, f: F) -> Result<T, Box<dyn Error>>
where
    F: FnOnce(&mut micropython::MicroPythonDevice) -> micropython::Result<T>,
{
    let mut dev = open_device(opts)?;
    dev.enter_raw_repl()?;
    let result = f(&mut dev);
    let _ = dev.exit_raw_repl();
    Ok(result?)
}

fn cmd_ports(cli: &Cli, only_micropython: bool, probe: bool) -> Result<(), Box<dyn Error>> {
    let ports = available_ports()?;

    let mut results: Vec<(String, bool)> = Vec::new();
    for p in &ports {
        let candidate = vid_looks_micropython(&p.port_type);
        // Passive by default: classify by USB vendor ID and never write to a
        // port, so unrelated serial hardware (CNC controllers, printers,
        // UPSes, ...) is left alone.
        //
        // `--probe` is the explicit opt-in to writing, and it has to widen
        // the search rather than narrow it: probing only VID matches could
        // just demote a port, leaving a board on a generic USB-serial bridge
        // undetectable under every flag combination.
        let is_mp = if probe {
            candidate || is_micropython_port(cli, &p.port_name)
        } else {
            candidate
        };
        if only_micropython && !is_mp {
            continue;
        }
        results.push((p.port_name.clone(), is_mp));
    }

    if cli.json {
        let arr: Vec<serde_json::Value> = results
            .iter()
            .map(|(name, is_mp)| {
                serde_json::json!({
                    "port": name,
                    "is_micropython": is_mp,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr))?
        );
    } else {
        for (name, is_mp) in results {
            if is_mp {
                println!("[mp] {}", name);
            } else {
                println!("{}", name);
            }
        }
    }

    Ok(())
}

fn is_micropython_port(cli: &Cli, port: &str) -> bool {
    let opts = DeviceOpts {
        port: port.to_string(),
        baud: cli.baud,
        read_timeout: Some(HANDSHAKE_TIMEOUT),
    };
    let res: Result<bool, Box<dyn Error>> = with_raw_device(&opts, |dev| {
        let out = dev.run_snippet("import sys\nprint(sys.implementation[0])")?;
        Ok(out.stdout.to_lowercase().contains("micropython"))
    });
    res.unwrap_or(false)
}

fn cmd_ls(
    opts: &DeviceOpts,
    path: &str,
    recursive: bool,
    long: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        if !recursive {
            let entries = with_raw_device(opts, |dev| dev.list_dir(path))?;
            let value = ls_entries_to_json(path, entries);
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            let entries = with_raw_device(opts, |dev| {
                let mut out = Vec::<(String, RemoteInfo)>::new();
                collect_remote_entries(dev, path, "", &mut out)?;
                Ok(out)
            })?;
            let value = ls_recursive_entries_to_json(path, &entries);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Ok(())
    } else if !recursive {
        with_raw_device(opts, |dev| {
            let entries = dev.list_dir(path)?;
            for e in entries {
                if long {
                    let mtime = e
                        .modified
                        .map(format_mtime)
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{}\t{}\t{}\t{}",
                        if e.is_dir { "d" } else { "-" },
                        e.size,
                        mtime,
                        e.name
                    );
                } else {
                    println!(
                        "{}\t{}\t{}",
                        if e.is_dir { "d" } else { "-" },
                        e.size,
                        e.name
                    );
                }
            }
            Ok(())
        })
    } else {
        with_raw_device(opts, |dev| {
            print_tree(dev, path, 0, long)?;
            Ok(())
        })
    }
}

fn cmd_cat(opts: &DeviceOpts, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(opts, |dev| {
        let contents = dev.read_text_file(path)?;
        print!("{}", contents);
        io::stdout().flush()?;
        Ok(())
    })
}

fn cmd_init(template: &str, dest: Option<&str>, quiet: bool) -> Result<(), Box<dyn Error>> {
    match template {
        "blink" => cmd_init_blink(dest, quiet),
        "button" => cmd_init_button(dest, quiet),
        "uart" => cmd_init_uart(dest, quiet),
        other => Err(format!(
            "unknown template '{other}'. Supported templates: 'blink', 'button', 'uart'"
        )
        .into()),
    }
}

fn init_template_common(
    default_dir: &str,
    dest: Option<&str>,
    filename: &str,
    contents: &str,
    quiet: bool,
) -> Result<(), Box<dyn Error>> {
    let dir_name = dest.unwrap_or(default_dir);
    let dest_path = PathBuf::from(dir_name);

    if dest_path.exists() {
        let meta = fs::metadata(&dest_path)?;
        if !meta.is_dir() {
            return Err(format!("{} exists and is not a directory", dest_path.display()).into());
        }
        if fs::read_dir(&dest_path)?.next().is_some() {
            return Err(format!(
                "directory {} already exists and is not empty",
                dest_path.display()
            )
            .into());
        }
    } else {
        fs::create_dir_all(&dest_path)?;
    }

    let file_path = dest_path.join(filename);
    fs::write(&file_path, contents)?;

    if !quiet {
        println!(
            "Initialized '{}' template in {}",
            default_dir,
            dest_path.display()
        );
    }

    Ok(())
}

fn cmd_init_blink(dest: Option<&str>, quiet: bool) -> Result<(), Box<dyn Error>> {
    init_template_common("blink", dest, "blink.py", BLINK_TEMPLATE_BLINK_PY, quiet)
}

fn cmd_init_button(dest: Option<&str>, quiet: bool) -> Result<(), Box<dyn Error>> {
    init_template_common(
        "button",
        dest,
        "button.py",
        BUTTON_TEMPLATE_BUTTON_PY,
        quiet,
    )
}

fn cmd_init_uart(dest: Option<&str>, quiet: bool) -> Result<(), Box<dyn Error>> {
    init_template_common("uart", dest, "uart.py", UART_TEMPLATE_UART_PY, quiet)
}

fn cmd_put(opts: &DeviceOpts, local: &str, remote: &str) -> Result<(), Box<dyn Error>> {
    let data = fs::read(local)?;
    with_raw_device(opts, |dev| {
        dev.write_file(remote, &data)?;
        Ok(())
    })
}

fn cmd_get(opts: &DeviceOpts, remote: &str, local: &str) -> Result<(), Box<dyn Error>> {
    let data = with_raw_device(opts, |dev| dev.read_file(remote))?;
    fs::write(local, data)?;
    Ok(())
}

fn cmd_rm(opts: &DeviceOpts, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(opts, |dev| {
        dev.remove(path)?;
        Ok(())
    })
}

fn cmd_mkdir(opts: &DeviceOpts, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(opts, |dev| {
        dev.mkdir(path)?;
        Ok(())
    })
}

/// Check for, and optionally install, a newer release.
///
/// This is the only command that touches the network, and it does so only
/// when explicitly invoked — there is no background check.
fn cmd_update(check_only: bool, quiet: bool) -> Result<i32, Box<dyn Error>> {
    let current = update::current_version();

    let outcome = update::check(update::REPO)?;
    let Some(outcome) = outcome else {
        println!("No releases have been published yet (installed: {current}).");
        return Ok(0);
    };

    let release = match outcome {
        update::Check::UpToDate { current } => {
            println!("rupico {current} is the latest release.");
            return Ok(0);
        }
        update::Check::Available { current, release } => {
            println!(
                "A new release is available: {current} -> {}",
                release.version
            );
            if !quiet {
                println!("{}", release.html_url);
            }
            release
        }
    };

    if check_only {
        println!("Run `rupico update` to install it.");
        return Ok(0);
    }

    let binary = update::running_binary_name()?;
    if !quiet {
        println!(
            "Downloading and verifying {}...",
            update::asset_name(&release.version, update::target_triple())
        );
    }
    update::install(&release)?;

    println!("Updated {binary} to {}.", release.version);
    if !quiet && binary == "rupico" {
        println!("The desktop app updates separately: run `rupico_gui` and use Check for updates.");
    }
    Ok(0)
}

/// Print one sync decision in human-readable form.
///
/// The engine reports each action as it happens, so this runs live rather than
/// after the fact.
fn print_sync_action(dir: &str, action: &sync::SyncAction, verbose: bool) {
    let arrow = |a: &sync::SyncAction| {
        format!(
            "{} -> {}",
            a.local.as_deref().unwrap_or("?"),
            a.remote.as_deref().unwrap_or("?")
        )
    };
    let target = |a: &sync::SyncAction| {
        a.remote
            .as_deref()
            .or(a.local.as_deref())
            .unwrap_or("?")
            .to_string()
    };

    // Warnings and conflicts always surface; the rest only with -v.
    match action.op.as_str() {
        "warning" => {
            eprintln!("warning: {}", action.note.as_deref().unwrap_or(""));
            return;
        }
        "conflict" => {
            eprintln!(
                "{dir}: WARNING: both local and remote changed since last sync for {} ({})",
                target(action),
                if action.dry_run { "dry run" } else { "skipped" }
            );
            return;
        }
        op if op.starts_with("skip_delete_") => {
            eprintln!(
                "{dir}: WARNING: could not delete {} (skipped): {}",
                target(action),
                action.note.as_deref().unwrap_or("")
            );
            return;
        }
        _ => {}
    }

    if action.dry_run {
        let what = match action.op.as_str() {
            "upload" => format!("would upload {}", arrow(action)),
            "download" => format!("would download {}", arrow(action)),
            "delete_remote_file" => format!("would delete remote file {}", target(action)),
            "delete_remote_dir" => format!("would delete remote directory {}", target(action)),
            "delete_local_file" => format!("would delete local file {}", target(action)),
            "delete_local_dir" => format!("would delete local directory {}", target(action)),
            "remove_stale_staging" => {
                format!("would remove stale staging file {}", target(action))
            }
            _ => return,
        };
        println!("DRY RUN: {what}");
        return;
    }

    if !verbose {
        return;
    }
    let what = match action.op.as_str() {
        "upload" => format!("uploading {}", arrow(action)),
        "download" => format!("downloading {}", arrow(action)),
        "skip_upload" | "skip_download" => format!("skipping unchanged {}", target(action)),
        "delete_remote_file" | "delete_local_file" => format!("deleting {}", target(action)),
        "delete_remote_dir" | "delete_local_dir" => {
            format!("deleting directory {}", target(action))
        }
        "ensure_dir" => format!("ensuring directory {}", target(action)),
        "remove_stale_staging" => format!("removing stale staging file {}", target(action)),
        _ => return,
    };
    println!("{dir}: {what}");
}

/// Emit the `--json` summary for a completed sync.
fn print_sync_json(
    direction: &str,
    local_root: &str,
    remote_root: &str,
    outcome: &sync::SyncOutcome,
) -> Result<(), Box<dyn Error>> {
    let summary = serde_json::json!({
        "direction": direction,
        "local_root": local_root,
        "remote_root": remote_root,
        "actions": outcome.actions,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// Run one sync in either direction, printing progress as it goes.
#[allow(clippy::too_many_arguments)]
fn run_sync(
    opts: &DeviceOpts,
    local_root: &Path,
    remote_root: &str,
    from_device: bool,
    sync_opts: &sync::SyncOptions,
    baseline: Option<&HashMap<String, String>>,
    quiet: bool,
    json: bool,
    verbose: bool,
) -> Result<sync::SyncOutcome, Box<dyn Error>> {
    let dir = if from_device {
        "sync-from-device"
    } else {
        "sync-to-device"
    };
    // In JSON mode the machine-readable summary is the output; live text would
    // corrupt it.
    let chatty = !json && !quiet;
    let mut report = |a: &sync::SyncAction| {
        if chatty {
            print_sync_action(dir, a, verbose);
        }
    };

    let outcome = with_raw_device(opts, |dev| {
        if from_device {
            sync::from_device(
                dev,
                remote_root,
                local_root,
                sync_opts,
                baseline,
                Some(&mut report),
            )
        } else {
            sync::to_device(
                dev,
                local_root,
                remote_root,
                sync_opts,
                baseline,
                Some(&mut report),
            )
        }
    })?;

    if json {
        let direction = if from_device {
            "from_device"
        } else {
            "to_device"
        };
        print_sync_json(
            direction,
            &local_root.display().to_string(),
            remote_root,
            &outcome,
        )?;
    }

    Ok(outcome)
}

/// Print an execution result's stdout/stderr, with labeled (and colorized,
/// when stdout is a TTY) sections unless `quiet` is set.
///
/// Returns the process exit code to use: code that raised on the device is
/// reported as a failure so a script can tell a clean run from a traceback.
fn print_exec_result(res: &micropython::ExecResult, quiet: bool) -> io::Result<i32> {
    let use_color = io::stdout().is_terminal() && !quiet;
    if quiet {
        // Quiet mode is the scripting mode, so keep the streams separate:
        // folding the device's stderr into stdout corrupts captured output.
        print!("{}", res.stdout);
        io::stdout().flush()?;
        eprint!("{}", res.stderr);
    } else if use_color {
        println!("\x1b[32m--- stdout ---\x1b[0m");
        print!("{}", res.stdout);
        println!("\x1b[31m--- stderr ---\x1b[0m");
        print!("\x1b[31m{}\x1b[0m", res.stderr);
    } else {
        println!("--- stdout ---");
        print!("{}", res.stdout);
        println!("--- stderr ---");
        print!("{}", res.stderr);
    }
    io::stdout().flush()?;

    if res.stderr.trim().is_empty() {
        Ok(0)
    } else {
        Ok(EXIT_DEVICE_ERROR)
    }
}

fn cmd_run(opts: &DeviceOpts, path: &str, quiet: bool) -> Result<i32, Box<dyn Error>> {
    with_raw_device(opts, |dev| {
        let res = dev.run_file(path)?;
        Ok(print_exec_result(&res, quiet)?)
    })
}

fn cmd_run_local(opts: &DeviceOpts, local: &str, quiet: bool) -> Result<i32, Box<dyn Error>> {
    let source = fs::read_to_string(local)?;
    // Include the pid so two rupico processes driving the same board do not
    // overwrite each other's staged script.
    let remote_temp = format!("/__rupico_temp_{}__.py", std::process::id());
    with_raw_device(opts, |dev| {
        dev.write_text_file(&remote_temp, &source)?;
        let res = dev.run_file(&remote_temp);
        // Best-effort cleanup: remove the temp file regardless of whether
        // execution succeeded so we don't leave stale files on flash storage.
        let _ = dev.remove(&remote_temp);
        let res = res?;
        Ok(print_exec_result(&res, quiet)?)
    })
}

fn cmd_run_snippet(opts: &DeviceOpts, code: &str, quiet: bool) -> Result<i32, Box<dyn Error>> {
    with_raw_device(opts, |dev| {
        let res = dev.run_snippet(code)?;
        Ok(print_exec_result(&res, quiet)?)
    })
}

fn cmd_flash_main(opts: &DeviceOpts, local: &str) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(local)?;
    with_raw_device(opts, |dev| {
        dev.flash_main_script(&source)?;
        Ok(())
    })
}

fn cmd_run_main(opts: &DeviceOpts) -> Result<(), Box<dyn Error>> {
    let mut dev = open_device(opts)?;
    dev.run_main()?;
    Ok(())
}

fn cmd_stop(opts: &DeviceOpts, quiet: bool) -> Result<(), Box<dyn Error>> {
    let mut dev = open_device(opts)?;
    dev.stop_current_program()?;
    if !quiet {
        eprintln!("Sent Ctrl-C to stop current program on the device.");
    }
    Ok(())
}

fn print_tree(
    dev: &mut micropython::MicroPythonDevice,
    path: &str,
    depth: usize,
    long: bool,
) -> micropython::Result<()> {
    let indent = "  ".repeat(depth);
    println!("{}{}:", indent, path);
    let entries = dev.list_dir(path)?;
    for e in &entries {
        if long {
            let mtime = e
                .modified
                .map(format_mtime)
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{}  {} {} {} {}",
                indent,
                if e.is_dir { "d" } else { "-" },
                e.size,
                mtime,
                e.name
            );
        } else {
            println!(
                "{}  {} {}",
                indent,
                if e.is_dir { "d" } else { "-" },
                e.name
            );
        }
    }
    for e in entries {
        if e.is_dir {
            let child = join_remote_path(path, &e.name);
            print_tree(dev, &child, depth + 1, long)?;
        }
    }
    Ok(())
}

fn cmd_repl(opts: &DeviceOpts, quiet: bool) -> Result<(), Box<dyn Error>> {
    let mut port = serialport::new(&opts.port, opts.baud)
        .timeout(Duration::from_millis(100))
        .open()?;

    if !quiet {
        println!(
            "Entering REPL on {}. Ctrl-C interrupts the running program on the device; press Ctrl-D to exit.",
            opts.port
        );
    }
    port.write_all(b"\r\n")?;
    port.flush()?;

    // Forward Ctrl-C to the device (to interrupt whatever is running there)
    // instead of letting it kill this process. Exit the proxy with Ctrl-D
    // (EOF on stdin).
    let int_port = std::sync::Mutex::new(port.try_clone()?);
    ctrlc::set_handler(move || {
        if let Ok(mut p) = int_port.lock() {
            // If the board went away, Ctrl-C would otherwise be a silent
            // no-op and the session would just look wedged.
            if let Err(e) = p.write_all(&[0x03]).and_then(|()| p.flush()) {
                eprintln!("\nrupico: could not forward Ctrl-C to the device: {e}");
                eprintln!("rupico: press Ctrl-D to exit the REPL proxy.");
            }
        }
    })?;

    let mut reader = port.try_clone()?;

    // Thread to continuously read from device and print to stdout.
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    print!("{}", text);
                    let _ = io::stdout().flush();
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => {
                    eprintln!("REPL read error: {}", e);
                    break;
                }
            }
        }
    });

    // Main thread: read from stdin and forward to device.
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdin.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // MicroPython's REPL expects \r to execute a line.
        let line = line.replace('\n', "\r");
        port.write_all(line.as_bytes())?;
        port.flush()?;
    }

    Ok(())
}

fn format_mtime(epoch: u64) -> String {
    match Utc.timestamp_opt(epoch as i64, 0).single() {
        Some(dt) => dt.to_rfc3339(),
        None => epoch.to_string(),
    }
}

fn ls_entries_to_json(path: &str, entries: Vec<micropython::RemoteEntry>) -> serde_json::Value {
    let mut arr = Vec::new();
    for e in entries {
        let full = join_remote_path(path, &e.name);
        arr.push(serde_json::json!({
            "path": full,
            "name": e.name,
            "is_dir": e.is_dir,
            "size": e.size,
            "modified": e.modified,
        }));
    }
    serde_json::Value::Array(arr)
}

fn ls_recursive_entries_to_json(root: &str, entries: &[(String, RemoteInfo)]) -> serde_json::Value {
    let mut arr = Vec::new();
    for (rel, info) in entries {
        let full = join_remote_path(root, rel);
        let name = rel.rsplit('/').next().unwrap_or(rel.as_str()).to_string();
        arr.push(serde_json::json!({
            "path": full,
            "name": name,
            "is_dir": info.is_dir,
            "size": info.size,
            "modified": info.modified,
        }));
    }
    serde_json::Value::Array(arr)
}

/// Remote entry metadata used by `ls -R` (mtime-based display only).
#[derive(Debug, Clone)]
struct RemoteInfo {
    is_dir: bool,
    size: u64,
    modified: Option<u64>,
}

fn collect_remote_entries(
    dev: &mut micropython::MicroPythonDevice,
    remote_root: &str,
    rel: &str,
    out: &mut Vec<(String, RemoteInfo)>,
) -> micropython::Result<()> {
    let current = if rel.is_empty() {
        remote_root.to_string()
    } else {
        join_remote_path(remote_root, rel)
    };

    let entries = dev.list_dir(&current)?;
    for e in entries {
        let child_rel = if rel.is_empty() {
            e.name.clone()
        } else {
            format!("{}/{}", rel, e.name)
        };
        let info = RemoteInfo {
            is_dir: e.is_dir,
            size: e.size,
            modified: e.modified,
        };
        out.push((child_rel.clone(), info.clone()));
        if info.is_dir {
            collect_remote_entries(dev, remote_root, &child_rel, out)?;
        }
    }

    Ok(())
}

const BLINK_TEMPLATE_BLINK_PY: &str = r#"from machine import Pin
import time

# Use the on-board LED if available (e.g. Raspberry Pi Pico), otherwise
# adjust the pin name/number for your board.
led = Pin("LED", Pin.OUT)

while True:
    led.toggle()
    time.sleep(0.5)
"#;

const BUTTON_TEMPLATE_BUTTON_PY: &str = r#"from machine import Pin
import time

# Simple button example: prints when the button is pressed.
# Adjust pin name/number for your board if needed.
button = Pin(14, Pin.IN, Pin.PULL_UP)

print("Press the button (active low)...")

while True:
    if not button.value():
        print("button pressed")
        while not button.value():
            time.sleep(0.01)
    time.sleep(0.01)
"#;

const UART_TEMPLATE_UART_PY: &str = r#"from machine import UART, Pin
import time

# Simple UART echo example. Adjust UART ID, baudrate and pins for your board.
uart = UART(0, baudrate=115200, tx=Pin(0), rx=Pin(1))

print("UART echo example. Type in the REPL and see it echoed back.")

while True:
    if uart.any():
        data = uart.read()
        if data:
            uart.write(data)
    time.sleep(0.01)
"#;
