use atty::Stream as AttyStream;
use chrono::{TimeZone, Utc};
use clap::{Parser, Subcommand};
use rupico::micropython;
use serde::{Deserialize, Serialize};
use serialport::available_ports;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List available serial ports.
    Ports {
        /// Only show ports that appear to be running MicroPython.
        #[arg(long = "only-micropython")]
        only_micropython: bool,
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
    },

    /// Simple interactive REPL proxy.
    Repl,
}

fn main() {
    if let Err(e) = try_main() {
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

fn try_main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Ports { only_micropython } => {
            cmd_ports(cli.json, *only_micropython)?;
        }
        Command::Ls {
            path,
            recursive,
            long,
        } => {
            let port = require_port(&cli)?;
            let path_str = path.as_deref().unwrap_or("/");
            cmd_ls(&port, path_str, *recursive, *long, cli.json)?;
        }
        Command::Cat { path } => {
            let port = require_port(&cli)?;
            cmd_cat(&port, path)?;
        }
        Command::Init { template, dest } => {
            cmd_init(template, dest.as_deref(), cli.quiet)?;
        }
        Command::Put { local, remote } => {
            let port = require_port(&cli)?;
            cmd_put(&port, local, remote)?;
        }
        Command::Get { remote, local } => {
            let port = require_port(&cli)?;
            cmd_get(&port, remote, local)?;
        }
        Command::Rm { path } => {
            let port = require_port(&cli)?;
            cmd_rm(&port, path)?;
        }
        Command::Mkdir { path } => {
            let port = require_port(&cli)?;
            cmd_mkdir(&port, path)?;
        }
        Command::Run { path } => {
            let port = require_port(&cli)?;
            cmd_run(&port, path, cli.quiet)?;
        }
        Command::RunLocal { path } => {
            let port = require_port(&cli)?;
            cmd_run_local(&port, path, cli.quiet)?;
        }
        Command::RunSnippet { code } => {
            let port = require_port(&cli)?;
            cmd_run_snippet(&port, code, cli.quiet)?;
        }
        Command::FlashMain { local } => {
            let port = require_port(&cli)?;
            cmd_flash_main(&port, local)?;
        }
        Command::RunMain => {
            let port = require_port(&cli)?;
            cmd_run_main(&port)?;
        }
        Command::Stop => {
            let port = require_port(&cli)?;
            cmd_stop(&port, cli.quiet)?;
        }
        Command::SyncToDevice {
            local,
            remote,
            delete,
            dry_run,
            verbose,
            ignore,
        } => {
            let port = require_port(&cli)?;
            let effective_verbose = *verbose && !cli.quiet;
            cmd_sync_to_device(
                &port,
                local,
                remote,
                *delete,
                *dry_run,
                effective_verbose,
                ignore.clone(),
                cli.json,
                None,
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
            let port = require_port(&cli)?;
            let effective_verbose = *verbose && !cli.quiet;
            cmd_sync_from_device(
                &port,
                remote,
                local,
                *delete,
                *dry_run,
                effective_verbose,
                ignore.clone(),
                cli.json,
                None,
            )?;
        }
        Command::Sync {
            from_device,
            delete,
            dry_run,
            verbose,
            ignore,
        } => {
            let port = require_port(&cli)?;
            let effective_verbose = *verbose && !cli.quiet;
            let (workspace_root, cfg) = find_workspace_config()?;
            let mut state = load_workspace_state(&workspace_root);

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());

            let local_root = workspace_root.join(&cfg.local_root);
            let local_root_str = local_root.display().to_string();
            let remote_root = cfg.remote_root;

            if *from_device {
                let last = state.last_sync_from_device;
                cmd_sync_from_device(
                    &port,
                    &remote_root,
                    &local_root_str,
                    *delete,
                    *dry_run,
                    effective_verbose,
                    ignore.clone(),
                    cli.json,
                    last,
                )?;
                if !*dry_run {
                    if let Some(t) = now {
                        state.last_sync_from_device = Some(t);
                        let _ = save_workspace_state(&workspace_root, &state);
                    }
                }
            } else {
                let last = state.last_sync_to_device;
                cmd_sync_to_device(
                    &port,
                    &local_root_str,
                    &remote_root,
                    *delete,
                    *dry_run,
                    effective_verbose,
                    ignore.clone(),
                    cli.json,
                    last,
                )?;
                if !*dry_run {
                    if let Some(t) = now {
                        state.last_sync_to_device = Some(t);
                        let _ = save_workspace_state(&workspace_root, &state);
                    }
                }
            }
        }
        Command::Repl => {
            let port = require_port(&cli)?;
            cmd_repl(&port, cli.quiet)?;
        }
    }

    Ok(())
}

fn require_port(cli: &Cli) -> Result<String, Box<dyn Error>> {
    match &cli.port {
        Some(p) => Ok(p.clone()),
        None => {
            eprintln!("error: --port is required for this command");
            std::process::exit(1);
        }
    }
}

fn with_raw_device<F, T>(port: &str, f: F) -> Result<T, Box<dyn Error>>
where
    F: FnOnce(&mut micropython::MicroPythonDevice) -> micropython::Result<T>,
{
    let mut dev = micropython::MicroPythonDevice::connect(port)?;
    dev.enter_raw_repl()?;
    let result = f(&mut dev);
    let _ = dev.exit_raw_repl();
    Ok(result?)
}

fn cmd_ports(json: bool, only_micropython: bool) -> Result<(), Box<dyn Error>> {
    let ports = available_ports()?;

    if json {
        let mut arr = Vec::new();
        for p in ports {
            let is_mp = is_micropython_port(&p.port_name);
            if only_micropython && !is_mp {
                continue;
            }
            arr.push(serde_json::json!({
                "port": p.port_name,
                "is_micropython": is_mp,
            }));
        }
        let value = serde_json::Value::Array(arr);
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        for p in ports {
            let is_mp = is_micropython_port(&p.port_name);
            if only_micropython && !is_mp {
                continue;
            }
            if is_mp {
                println!("[mp] {}", p.port_name);
            } else {
                println!("{}", p.port_name);
            }
        }
    }

    Ok(())
}

fn is_micropython_port(port: &str) -> bool {
    let res: Result<bool, Box<dyn Error>> = with_raw_device(port, |dev| {
        let out = dev.run_snippet("import sys\nprint(sys.implementation[0])")?;
        Ok(out.stdout.to_lowercase().contains("micropython"))
    });
    res.unwrap_or(false)
}

fn cmd_ls(
    port: &str,
    path: &str,
    recursive: bool,
    long: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        if !recursive {
            let entries = with_raw_device(port, |dev| dev.list_dir(path))?;
            let value = ls_entries_to_json(path, entries);
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            let entries = with_raw_device(port, |dev| {
                let mut out = Vec::<(String, RemoteInfo)>::new();
                collect_remote_entries(dev, path, "", &mut out)?;
                Ok(out)
            })?;
            let value = ls_recursive_entries_to_json(path, &entries);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Ok(())
    } else if !recursive {
        with_raw_device(port, |dev| {
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
        with_raw_device(port, |dev| {
            print_tree(dev, path, 0, long)?;
            Ok(())
        })
    }
}

fn cmd_cat(port: &str, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(port, |dev| {
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

fn cmd_put(port: &str, local: &str, remote: &str) -> Result<(), Box<dyn Error>> {
    let data = fs::read(local)?;
    with_raw_device(port, |dev| {
        dev.write_file(remote, &data)?;
        Ok(())
    })
}

fn cmd_get(port: &str, remote: &str, local: &str) -> Result<(), Box<dyn Error>> {
    let data = with_raw_device(port, |dev| dev.read_file(remote))?;
    fs::write(local, data)?;
    Ok(())
}

fn cmd_rm(port: &str, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(port, |dev| {
        dev.remove(path)?;
        Ok(())
    })
}

fn cmd_mkdir(port: &str, path: &str) -> Result<(), Box<dyn Error>> {
    with_raw_device(port, |dev| {
        dev.mkdir(path)?;
        Ok(())
    })
}

fn cmd_run(port: &str, path: &str, quiet: bool) -> Result<(), Box<dyn Error>> {
    with_raw_device(port, |dev| {
        let res = dev.run_file(path)?;
        let use_color = atty::is(AttyStream::Stdout) && !quiet;
        if quiet {
            // In quiet mode, print raw stdout+stderr without extra labels.
            print!("{}{}", res.stdout, res.stderr);
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
        Ok(())
    })
}

fn cmd_run_local(port: &str, local: &str, quiet: bool) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(local)?;
    const REMOTE_TEMP: &str = "/__rupico_temp__.py";
    with_raw_device(port, |dev| {
        dev.write_text_file(REMOTE_TEMP, &source)?;
        let res = dev.run_file(REMOTE_TEMP)?;
        let use_color = atty::is(AttyStream::Stdout) && !quiet;
        if quiet {
            print!("{}{}", res.stdout, res.stderr);
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
        Ok(())
    })
}

fn cmd_run_snippet(port: &str, code: &str, quiet: bool) -> Result<(), Box<dyn Error>> {
    with_raw_device(port, |dev| {
        let res = dev.run_snippet(code)?;
        let use_color = atty::is(AttyStream::Stdout) && !quiet;
        if quiet {
            print!("{}{}", res.stdout, res.stderr);
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
        Ok(())
    })
}

fn cmd_flash_main(port: &str, local: &str) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(local)?;
    with_raw_device(port, |dev| {
        dev.flash_main_script(&source)?;
        Ok(())
    })
}

fn cmd_run_main(port: &str) -> Result<(), Box<dyn Error>> {
    let mut dev = micropython::MicroPythonDevice::connect(port)?;
    dev.run_main()?;
    Ok(())
}

fn cmd_stop(port: &str, quiet: bool) -> Result<(), Box<dyn Error>> {
    let mut dev = micropython::MicroPythonDevice::connect(port)?;
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

fn cmd_repl(port_path: &str, quiet: bool) -> Result<(), Box<dyn Error>> {
    let mut port = serialport::new(port_path, 115_200)
        .timeout(Duration::from_millis(100))
        .open()?;

    if !quiet {
        println!("Entering REPL on {}. Press Ctrl-C to quit.", port_path);
    }
    port.write_all(b"\r\n")?;
    port.flush()?;

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
        port.write_all(line.as_bytes())?;
        port.flush()?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_sync_to_device(
    port: &str,
    local: &str,
    remote: &str,
    delete: bool,
    dry_run: bool,
    verbose: bool,
    extra_ignores: Vec<String>,
    json: bool,
    last_sync_time: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let local_root = PathBuf::from(local);
    let mut entries = Vec::<(PathBuf, bool)>::new();
    collect_local_entries(&local_root, Path::new(""), &mut entries)?;

    let ignore_patterns = build_ignore_patterns(&local_root, &extra_ignores)?;
    entries.retain(|(rel, _)| !path_is_ignored(rel, &ignore_patterns));

    // Build metadata map for local files/dirs.
    let mut local_files: HashMap<String, LocalInfo> = HashMap::new();
    for (rel, is_dir) in &entries {
        let rel_str = rel_path_to_remote(rel);
        if *is_dir {
            local_files.insert(
                rel_str,
                LocalInfo {
                    is_dir: true,
                    size: 0,
                    modified: None,
                },
            );
        } else {
            let meta = fs::metadata(local_root.join(rel))?;
            let size = meta.len();
            let modified = meta.modified().ok().and_then(system_time_to_unix);
            local_files.insert(
                rel_str,
                LocalInfo {
                    is_dir: false,
                    size,
                    modified,
                },
            );
        }
    }

    let local_paths: HashSet<String> = local_files.keys().cloned().collect();

    let ignore_patterns_clone = ignore_patterns.clone();
    let mut actions: Vec<SyncAction> = Vec::new();

    with_raw_device(port, |dev| {
        let mut remote_entries = Vec::<(String, RemoteInfo)>::new();
        collect_remote_entries(dev, remote, "", &mut remote_entries)?;
        remote_entries.retain(|(rel, _)| {
            let rel_path = PathBuf::from(rel);
            !path_is_ignored(&rel_path, &ignore_patterns_clone)
        });
        let remote_map: HashMap<String, RemoteInfo> = remote_entries.iter().cloned().collect();

        if delete {
            // Remote entries that are not present locally should be deleted
            // from the device. We delete deeper paths first so directories
            // become empty before we try to remove them.
            let mut to_delete: Vec<(String, bool)> = remote_entries
                .iter()
                .filter(|(rel, _)| !local_paths.contains(rel))
                .map(|(rel, info)| (rel.clone(), info.is_dir))
                .collect();

            to_delete.sort_by(|(a, _), (b, _)| {
                let depth_a = a.matches('/').count();
                let depth_b = b.matches('/').count();
                depth_b.cmp(&depth_a)
            });

            for (rel, is_dir) in to_delete {
                let full = join_remote_path(remote, &rel);
                if dry_run {
                    if !json {
                        if is_dir {
                            println!("DRY RUN: would delete remote directory {}", full);
                        } else {
                            println!("DRY RUN: would delete remote file {}", full);
                        }
                    }
                    actions.push(SyncAction {
                        op: if is_dir {
                            "delete_remote_dir".to_string()
                        } else {
                            "delete_remote_file".to_string()
                        },
                        local: None,
                        remote: Some(full.clone()),
                        dry_run: true,
                    });
                } else if is_dir {
                    if verbose && !json {
                        println!("sync-to-device: deleting remote directory {}", full);
                    }
                    dev.rmdir(&full)?;
                    actions.push(SyncAction {
                        op: "delete_remote_dir".to_string(),
                        local: None,
                        remote: Some(full.clone()),
                        dry_run: false,
                    });
                } else {
                    if verbose && !json {
                        println!("sync-to-device: deleting remote file {}", full);
                    }
                    dev.remove(&full)?;
                    actions.push(SyncAction {
                        op: "delete_remote_file".to_string(),
                        local: None,
                        remote: Some(full.clone()),
                        dry_run: false,
                    });
                }
            }
        }

        // Upload/update all local entries.
        for (rel, is_dir) in &entries {
            let rel_remote = rel_path_to_remote(rel);
            let remote_path = join_remote_path(remote, &rel_remote);
            let local_path = local_root.join(rel);
            if *is_dir {
                let _ = dev.mkdir(&remote_path);
            } else {
                let local_info = local_files
                    .get(&rel_remote)
                    .expect("local_files missing entry that exists in entries");
                let remote_info = remote_map.get(&rel_remote);

                // Detect "both changed" conflicts based on last sync time, if
                // we have one and both sides report modification times.
                if let Some(t0) = last_sync_time {
                    if let Some(info) = remote_info {
                        if let (Some(lm), Some(rm)) = (local_info.modified, info.modified) {
                            if lm > t0 && rm > t0 && lm != rm {
                                if !json {
                                    eprintln!(
                                        "sync-to-device: WARNING: both local and remote changed since last sync for {}",
                                        remote_path
                                    );
                                }
                                actions.push(SyncAction {
                                    op: "conflict".to_string(),
                                    local: Some(local_path.display().to_string()),
                                    remote: Some(remote_path.clone()),
                                    dry_run,
                                });
                            }
                        }
                    }
                }

                let should_upload = match remote_info {
                    None => true,
                    Some(info) => {
                        if info.is_dir {
                            true
                        } else {
                            !matches!(
                                (local_info.modified, info.modified),
                                (Some(lm), Some(rm))
                                    if lm == rm && local_info.size == info.size
                            )
                        }
                    }
                };

                if should_upload {
                    if verbose && !json {
                        println!(
                            "sync-to-device: uploading {} -> {}",
                            local_path.display(),
                            remote_path
                        );
                    }
                    let data = fs::read(&local_path).map_err(micropython::MicroPythonError::Io)?;
                    dev.write_file(&remote_path, &data)?;
                    actions.push(SyncAction {
                        op: "upload".to_string(),
                        local: Some(local_path.display().to_string()),
                        remote: Some(remote_path.clone()),
                        dry_run: false,
                    });
                } else {
                    if verbose && !json {
                        println!("sync-to-device: skipping unchanged {}", remote_path);
                    }
                    actions.push(SyncAction {
                        op: "skip_upload".to_string(),
                        local: Some(local_path.display().to_string()),
                        remote: Some(remote_path.clone()),
                        dry_run: false,
                    });
                }
            }
        }
        Ok(())
    })?;

    if json {
        let summary = serde_json::json!({
            "direction": "to_device",
            "local_root": local_root.display().to_string(),
            "remote_root": remote,
            "actions": actions,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_sync_from_device(
    port: &str,
    remote: &str,
    local: &str,
    delete: bool,
    dry_run: bool,
    verbose: bool,
    extra_ignores: Vec<String>,
    json: bool,
    last_sync_time: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let local_root = PathBuf::from(local);

    let ignore_patterns = build_ignore_patterns(&local_root, &extra_ignores)?;
    let ignore_patterns_clone = ignore_patterns.clone();

    // Build metadata map for existing local files/dirs.
    let mut local_entries = Vec::<(PathBuf, bool)>::new();
    collect_local_entries(&local_root, Path::new(""), &mut local_entries)?;
    local_entries.retain(|(rel, _)| !path_is_ignored(rel, &ignore_patterns));
    let mut local_files: HashMap<String, LocalInfo> = HashMap::new();
    for (rel, is_dir) in &local_entries {
        let rel_str = rel_path_to_remote(rel);
        if *is_dir {
            local_files.insert(
                rel_str,
                LocalInfo {
                    is_dir: true,
                    size: 0,
                    modified: None,
                },
            );
        } else {
            let meta = fs::metadata(local_root.join(rel))?;
            let size = meta.len();
            let modified = meta.modified().ok().and_then(system_time_to_unix);
            local_files.insert(
                rel_str,
                LocalInfo {
                    is_dir: false,
                    size,
                    modified,
                },
            );
        }
    }

    let mut actions: Vec<SyncAction> = Vec::new();

    with_raw_device(port, |dev| {
        let mut remote_entries = Vec::<(String, RemoteInfo)>::new();
        collect_remote_entries(dev, remote, "", &mut remote_entries)?;
        remote_entries.retain(|(rel, _)| {
            let rel_path = PathBuf::from(rel);
            !path_is_ignored(&rel_path, &ignore_patterns_clone)
        });
        let remote_map: HashMap<String, RemoteInfo> = remote_entries.iter().cloned().collect();

        if delete {
            // Local entries that are not present on the device should be
            // removed on the host.
            let remote_paths: HashSet<String> = remote_map.keys().cloned().collect();

            let mut to_delete: Vec<(PathBuf, bool)> = local_entries
                .iter()
                .filter(|(rel, _)| {
                    let rel_str = rel_path_to_remote(rel);
                    !remote_paths.contains(&rel_str)
                })
                .map(|(rel, is_dir)| (rel.clone(), *is_dir))
                .collect();

            // Delete deeper paths first so directories are empty before we
            // try to remove them.
            to_delete.sort_by(|(a, _), (b, _)| {
                let depth_a = a.components().count();
                let depth_b = b.components().count();
                depth_b.cmp(&depth_a)
            });

            for (rel, is_dir) in to_delete {
                let full = local_root.join(&rel);
                if dry_run {
                    if !json {
                        if is_dir {
                            println!("DRY RUN: would delete local directory {}", full.display());
                        } else {
                            println!("DRY RUN: would delete local file {}", full.display());
                        }
                    }
                    actions.push(SyncAction {
                        op: if is_dir {
                            "delete_local_dir".to_string()
                        } else {
                            "delete_local_file".to_string()
                        },
                        local: Some(full.display().to_string()),
                        remote: None,
                        dry_run: true,
                    });
                } else if is_dir {
                    if verbose && !json {
                        println!(
                            "sync-from-device: deleting local directory {}",
                            full.display()
                        );
                    }
                    fs::remove_dir(&full).map_err(micropython::MicroPythonError::Io)?;
                    actions.push(SyncAction {
                        op: "delete_local_dir".to_string(),
                        local: Some(full.display().to_string()),
                        remote: None,
                        dry_run: false,
                    });
                } else {
                    if verbose && !json {
                        println!("sync-from-device: deleting local file {}", full.display());
                    }
                    fs::remove_file(&full).map_err(micropython::MicroPythonError::Io)?;
                    actions.push(SyncAction {
                        op: "delete_local_file".to_string(),
                        local: Some(full.display().to_string()),
                        remote: None,
                        dry_run: false,
                    });
                }
            }
        }

        // Ensure base directory exists.
        fs::create_dir_all(&local_root).map_err(micropython::MicroPythonError::Io)?;

        // Download/update only files that differ.
        let mut ordered = remote_entries;
        ordered.sort_by_key(|(rel, _)| rel.matches('/').count());

        for (rel, info) in ordered {
            let rel_pathbuf = PathBuf::from(&rel);
            if path_is_ignored(&rel_pathbuf, &ignore_patterns_clone) {
                if verbose && !json {
                    println!("sync-from-device: ignoring {} due to ignore patterns", rel);
                }
                actions.push(SyncAction {
                    op: "ignore".to_string(),
                    local: Some(local_root.join(&rel).display().to_string()),
                    remote: Some(join_remote_path(remote, &rel)),
                    dry_run: false,
                });
                continue;
            }

            let local_path = local_root.join(&rel);
            if info.is_dir {
                if verbose && !json {
                    println!(
                        "sync-from-device: ensuring directory {}",
                        local_path.display()
                    );
                }
                fs::create_dir_all(&local_path).map_err(micropython::MicroPythonError::Io)?;
                actions.push(SyncAction {
                    op: "ensure_dir".to_string(),
                    local: Some(local_path.display().to_string()),
                    remote: Some(join_remote_path(remote, &rel)),
                    dry_run: false,
                });
            } else {
                let maybe_local_info = local_files.get(&rel);

                // Detect "both changed" conflicts based on last sync time, if
                // we have one and both sides report modification times.
                if let (Some(t0), Some(local_info)) = (last_sync_time, maybe_local_info) {
                    if let (Some(lm), Some(rm)) = (local_info.modified, info.modified) {
                        if lm > t0 && rm > t0 && lm != rm {
                            if !json {
                                eprintln!(
                                    "sync-from-device: WARNING: both local and remote changed since last sync for {}",
                                    local_path.display()
                                );
                            }
                            actions.push(SyncAction {
                                op: "conflict".to_string(),
                                local: Some(local_path.display().to_string()),
                                remote: Some(join_remote_path(remote, &rel)),
                                dry_run,
                            });
                        }
                    }
                }

                let needs_download = match maybe_local_info {
                    None => true,
                    Some(local_info) => {
                        if local_info.is_dir {
                            true
                        } else {
                            !matches!(
                                (local_info.modified, info.modified),
                                (Some(lm), Some(rm))
                                    if lm == rm && local_info.size == info.size
                            )
                        }
                    }
                };

                if needs_download {
                    let remote_path = join_remote_path(remote, &rel);
                    if verbose && !json {
                        println!(
                            "sync-from-device: downloading {} -> {}",
                            remote_path,
                            local_path.display()
                        );
                    }
                    let data = dev.read_file(&remote_path)?;
                    if let Some(parent) = local_path.parent() {
                        fs::create_dir_all(parent).map_err(micropython::MicroPythonError::Io)?;
                    }
                    fs::write(&local_path, data).map_err(micropython::MicroPythonError::Io)?;
                    actions.push(SyncAction {
                        op: "download".to_string(),
                        local: Some(local_path.display().to_string()),
                        remote: Some(remote_path),
                        dry_run: false,
                    });
                } else {
                    if verbose && !json {
                        println!(
                            "sync-from-device: skipping unchanged {}",
                            local_path.display()
                        );
                    }
                    actions.push(SyncAction {
                        op: "skip_download".to_string(),
                        local: Some(local_path.display().to_string()),
                        remote: Some(join_remote_path(remote, &rel)),
                        dry_run: false,
                    });
                }
            }
        }

        Ok(())
    })?;

    if json {
        let summary = serde_json::json!({
            "direction": "from_device",
            "local_root": local_root.display().to_string(),
            "remote_root": remote,
            "actions": actions,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    Ok(())
}

fn join_remote_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{}", name)
    } else if base.ends_with('/') {
        format!("{}{}", base, name)
    } else {
        format!("{}/{}", base, name)
    }
}

fn rel_path_to_remote(rel: &Path) -> String {
    let mut parts = Vec::new();
    for comp in rel.components() {
        parts.push(comp.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
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

fn system_time_to_unix(t: std::time::SystemTime) -> Option<u64> {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => Some(dur.as_secs()),
        Err(_) => None,
    }
}

fn collect_local_entries(
    root: &Path,
    rel: &Path,
    out: &mut Vec<(PathBuf, bool)>,
) -> io::Result<()> {
    let dir = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel.join(&name)
        };

        if file_type.is_dir() {
            out.push((child_rel.clone(), true));
            collect_local_entries(root, &child_rel, out)?;
        } else if file_type.is_file() {
            out.push((child_rel, false));
        }
    }

    Ok(())
}

fn build_ignore_patterns(root: &Path, extra: &[String]) -> io::Result<Vec<String>> {
    let mut patterns: Vec<String> = vec![
        ".git".into(),
        "__pycache__".into(),
        ".venv".into(),
        "target".into(),
    ];

    patterns.extend(extra.iter().cloned());

    let ignore_path = root.join(".rupicoignore");
    if let Ok(contents) = fs::read_to_string(&ignore_path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            patterns.push(line.to_string());
        }
    }

    Ok(patterns)
}

fn path_is_ignored(rel: &Path, patterns: &[String]) -> bool {
    let rel_str = rel_path_to_remote(rel);
    patterns.iter().any(|pat| rel_str.contains(pat))
}

#[derive(Debug, Clone)]
struct LocalInfo {
    is_dir: bool,
    size: u64,
    modified: Option<u64>,
}

#[derive(Debug, Clone)]
struct RemoteInfo {
    is_dir: bool,
    size: u64,
    modified: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SyncAction {
    op: String,
    local: Option<String>,
    remote: Option<String>,
    dry_run: bool,
}

/// Workspace-level configuration loaded from `.rupico.toml` in the project
/// root (or one of its parent directories).
#[derive(Debug, Clone, Deserialize)]
struct WorkspaceConfig {
    /// Local root directory for project files, relative to the workspace
    /// root (where `.rupico.toml` lives).
    local_root: String,
    /// Remote root directory on the device, e.g. `/app`.
    remote_root: String,
}

/// Mutable workspace state stored alongside the config, used for tracking
/// last sync times so we can detect "both changed" conflicts.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct WorkspaceState {
    last_sync_to_device: Option<u64>,
    last_sync_from_device: Option<u64>,
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

/// Locate and load `.rupico.toml`, walking up from the current directory
/// towards the filesystem root. Returns the workspace root directory and the
/// parsed configuration.
fn find_workspace_config() -> Result<(PathBuf, WorkspaceConfig), Box<dyn Error>> {
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join(".rupico.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)?;
            let cfg: WorkspaceConfig = toml::from_str(&text)?;
            return Ok((dir, cfg));
        }
        if !dir.pop() {
            break;
        }
    }
    Err("no .rupico.toml found in current or parent directories".into())
}

fn load_workspace_state(root: &Path) -> WorkspaceState {
    let path = root.join(".rupico-state.json");
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => WorkspaceState::default(),
    }
}

fn save_workspace_state(root: &Path, state: &WorkspaceState) -> io::Result<()> {
    let path = root.join(".rupico-state.json");
    let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    fs::write(path, json)
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
