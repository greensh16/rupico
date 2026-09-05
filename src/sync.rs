//! Directory sync between a host folder and a MicroPython device.
//!
//! This is the engine behind the CLI's `sync`, `sync-to-device` and
//! `sync-from-device` commands and the GUI's sync panel. It lives in the
//! library so both front ends share one implementation — presentation
//! (verbose text, JSON, progress bars) is left to the caller, which receives
//! every decision through a callback as it happens.
//!
//! Change detection is **content-hash based, never mtime**: boards like the
//! Pico have no battery-backed RTC, so their timestamps are meaningless. The
//! remote tree (paths, sizes and sha256 hashes) arrives in a single round
//! trip via [`MicroPythonDevice::list_tree_hashed`].

use crate::micropython::{self, MicroPythonDevice};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Knobs shared by both directions.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Delete entries on the destination that are absent from the source.
    pub delete: bool,
    /// Plan everything but change nothing.
    pub dry_run: bool,
    /// Extra ignore patterns on top of the built-ins and `.rupicoignore`.
    pub ignore: Vec<String>,
    /// Overwrite files that changed on both sides since the last sync.
    /// Without it such files are skipped and reported as conflicts.
    pub force: bool,
}

/// One decision the engine made, in the order it was made.
///
/// `op` is a stable string because it is part of the CLI's `--json` contract.
#[derive(Debug, Clone, Serialize)]
pub struct SyncAction {
    pub op: String,
    pub local: Option<String>,
    pub remote: Option<String>,
    pub dry_run: bool,
    /// Extra detail, such as why a delete was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SyncAction {
    fn new(op: &str, local: Option<String>, remote: Option<String>, dry_run: bool) -> Self {
        Self {
            op: op.to_string(),
            local,
            remote,
            dry_run,
            note: None,
        }
    }

    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// What a sync run produced.
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Every decision, in order.
    pub actions: Vec<SyncAction>,
    /// Content hashes to record as the new last-sync baseline.
    pub manifest: HashMap<String, String>,
    /// Files that changed on both sides and were left untouched.
    pub conflicts: usize,
}

impl SyncOutcome {
    /// Count actions with the given op, for summaries.
    pub fn count(&self, op: &str) -> usize {
        self.actions.iter().filter(|a| a.op == op).count()
    }
}

/// Callback invoked for each action as it happens, so a front end can report
/// progress live rather than only at the end.
pub type Reporter<'a> = &'a mut dyn FnMut(&SyncAction);

fn noop_reporter(_: &SyncAction) {}

// ---------------------------------------------------------------------------
// Workspace configuration
// ---------------------------------------------------------------------------

/// Project-level configuration read from `.rupico.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    /// Local root, relative to the directory holding `.rupico.toml`.
    pub local_root: String,
    /// Remote root on the device, e.g. `/app`.
    pub remote_root: String,
}

/// Mutable state stored beside the config, used to detect conflicts.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub last_sync_to_device: Option<u64>,
    pub last_sync_from_device: Option<u64>,
    /// Content hashes at the last successful sync, keyed by path relative to
    /// the sync roots. Lets "both sides changed" be detected without trusting
    /// any clock.
    #[serde(default)]
    pub files: HashMap<String, String>,
}

/// Locate and load `.rupico.toml`, walking up from `start`.
///
/// Returns the workspace root (the directory holding the file) and the config.
pub fn find_workspace_config(start: &Path) -> io::Result<(PathBuf, WorkspaceConfig)> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".rupico.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)?;
            let cfg: WorkspaceConfig = toml::from_str(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            return Ok((dir, cfg));
        }
        if !dir.pop() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no .rupico.toml found in this or any parent directory",
            ));
        }
    }
}

pub fn load_workspace_state(root: &Path) -> WorkspaceState {
    match fs::read_to_string(root.join(".rupico-state.json")) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => WorkspaceState::default(),
    }
}

/// Write the workspace state, replacing it atomically.
///
/// A torn write silently loses the conflict baseline, so the new contents land
/// in a sibling temp file that is renamed over the old one.
pub fn save_workspace_state(root: &Path, state: &WorkspaceState) -> io::Result<()> {
    let path = root.join(".rupico-state.json");
    let tmp = root.join(".rupico-state.json.tmp");
    let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    fs::write(&tmp, json)?;
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Seconds since the Unix epoch, for stamping the workspace state.
pub fn now_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

// ---------------------------------------------------------------------------
// Ignore rules
// ---------------------------------------------------------------------------

/// Patterns applied before any other rule.
///
/// `.rupico-tmp-*` are staging files from an interrupted `write_file`; they
/// are rupico's own bookkeeping, never project content.
const BUILTIN_IGNORES: &[&str] = &[".git", "__pycache__", ".venv", "target", ".rupico-tmp-*"];

/// Combine built-in patterns, `.rupicoignore` at `root`, and extras.
pub fn build_ignore_patterns(root: &Path, extra: &[String]) -> Vec<String> {
    let mut patterns: Vec<String> = BUILTIN_IGNORES.iter().map(|s| s.to_string()).collect();
    patterns.extend(extra.iter().cloned());
    if let Ok(contents) = fs::read_to_string(root.join(".rupicoignore")) {
        patterns.extend(parse_ignore_file(&contents));
    }
    patterns
}

/// Parse a `.rupicoignore`: one pattern per line, `#` comments and blank
/// lines skipped.
pub fn parse_ignore_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Match one path component against a pattern that may contain `*` (any run
/// of characters, including none) and `?` (exactly one).
///
/// Iterative backtracking rather than recursion, so a pathological pattern
/// cannot blow the stack.
pub fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Decide whether a relative path matches any ignore pattern.
///
/// - A pattern without `/` matches if any path *component* equals it, so
///   `target` ignores `target/` anywhere but not `mytarget/`.
/// - A pattern with `/` matches that relative path and everything inside it.
/// - `*` and `?` are wildcards in either form, so `*.pyc` works. Component
///   matching alone silently un-ignores patterns that relied on the old
///   substring rule.
/// - A leading `/` is stripped: paths are relative to the sync root, so
///   `/build` would otherwise never match.
pub fn path_is_ignored(rel: &Path, patterns: &[String]) -> bool {
    let rel_str = rel_path_to_remote(rel);
    patterns.iter().any(|pat| {
        let pat = pat.trim_end_matches('/').trim_start_matches('/');
        if pat.is_empty() {
            return false;
        }
        if pat.contains('/') {
            if rel_str == pat || rel_str.starts_with(&format!("{pat}/")) {
                return true;
            }
            let depth = pat.matches('/').count() + 1;
            let parts: Vec<&str> = rel_str.splitn(depth + 1, '/').collect();
            if parts.len() >= depth {
                return glob_match(pat, &parts[..depth].join("/"));
            }
            false
        } else {
            rel_str.split('/').any(|comp| glob_match(pat, comp))
        }
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Lowercase hex sha256 of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Render a relative host path as a remote path with `/` separators.
pub fn rel_path_to_remote(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Decide whether a source file must be copied over the destination.
///
/// `dst` is `(is_dir, size, hash)` of the destination entry, if present. When
/// both sides have hashes they are compared directly; otherwise (firmware
/// without sha256) fall back to comparing sizes.
pub fn needs_copy(
    src_size: u64,
    src_hash: Option<&str>,
    dst: Option<(bool, u64, Option<&str>)>,
) -> bool {
    match dst {
        None => true,
        Some((true, _, _)) => true,
        Some((false, dst_size, dst_hash)) => match (src_hash, dst_hash) {
            (Some(a), Some(b)) => a != b,
            _ => src_size != dst_size,
        },
    }
}

/// Fold conflicted files back into the manifest and report how many remain
/// unresolved.
///
/// When the copy was skipped the *previous* baseline is kept: recording the
/// source side would claim a sync that never happened, and dropping the entry
/// is worse still — with no baseline the next run cannot detect the conflict
/// at all and silently overwrites the destination.
fn settle_conflicts(
    manifest: &mut HashMap<String, String>,
    conflicted: &[(String, String)],
    force: bool,
) -> usize {
    if force {
        return 0;
    }
    for (rel, baseline) in conflicted {
        manifest.insert(rel.clone(), baseline.clone());
    }
    conflicted.len()
}

/// Best-effort `mkdir -p` on the device, ignoring "already exists".
fn ensure_remote_dir_all(dev: &mut MicroPythonDevice, path: &str) {
    let mut cur = String::new();
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        cur.push('/');
        cur.push_str(comp);
        let _ = dev.mkdir(&cur);
    }
}

/// Metadata for a host-side entry.
#[derive(Debug, Clone)]
struct LocalInfo {
    is_dir: bool,
    size: u64,
    hash: Option<String>,
}

/// Metadata for a device-side entry.
#[derive(Debug, Clone)]
struct RemoteHashInfo {
    is_dir: bool,
    size: u64,
    hash: Option<String>,
}

/// Walk `root`, collecting `(relative path, is_dir)`.
///
/// Symlinks to files are followed, so a project that links a shared module
/// into place still syncs it. Symlinked *directories* are skipped: a link
/// pointing at an ancestor would make this recursion run until it blows the
/// stack. Skipped entries are reported through `warn` rather than vanishing.
pub fn collect_local_entries(
    root: &Path,
    rel: &Path,
    out: &mut Vec<(PathBuf, bool)>,
    warn: &mut dyn FnMut(String),
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

        // `file_type` describes the link itself, so resolve it to decide what
        // the entry actually is.
        let resolved = if file_type.is_symlink() {
            match fs::metadata(entry.path()) {
                Ok(m) => m,
                Err(e) => {
                    warn(format!(
                        "skipping broken symlink {}: {e}",
                        entry.path().display()
                    ));
                    continue;
                }
            }
        } else {
            entry.metadata()?
        };

        if resolved.is_dir() {
            if file_type.is_symlink() {
                warn(format!(
                    "skipping symlinked directory {} (not followed)",
                    entry.path().display()
                ));
                continue;
            }
            out.push((child_rel.clone(), true));
            collect_local_entries(root, &child_rel, out, warn)?;
        } else if resolved.is_file() {
            out.push((child_rel, false));
        }
    }

    Ok(())
}

/// The host-side index: the walked entries plus their hashed metadata.
type LocalIndex = (Vec<(PathBuf, bool)>, HashMap<String, LocalInfo>);

/// Build the hashed index of a host directory.
fn index_local(
    root: &Path,
    patterns: &[String],
    warn: &mut dyn FnMut(String),
) -> micropython::Result<LocalIndex> {
    let mut entries = Vec::<(PathBuf, bool)>::new();
    if root.exists() {
        collect_local_entries(root, Path::new(""), &mut entries, warn)
            .map_err(micropython::MicroPythonError::Io)?;
    }
    entries.retain(|(rel, _)| !path_is_ignored(rel, patterns));

    let mut files = HashMap::new();
    for (rel, is_dir) in &entries {
        let key = rel_path_to_remote(rel);
        if *is_dir {
            files.insert(
                key,
                LocalInfo {
                    is_dir: true,
                    size: 0,
                    hash: None,
                },
            );
        } else {
            let data = fs::read(root.join(rel)).map_err(micropython::MicroPythonError::Io)?;
            files.insert(
                key,
                LocalInfo {
                    is_dir: false,
                    size: data.len() as u64,
                    hash: Some(sha256_hex(&data)),
                },
            );
        }
    }
    Ok((entries, files))
}

/// Fetch and normalise the device tree.
fn index_remote(
    dev: &mut MicroPythonDevice,
    remote_root: &str,
) -> micropython::Result<Option<Vec<(String, RemoteHashInfo)>>> {
    Ok(dev.list_tree_hashed(remote_root)?.map(|tree| {
        tree.into_iter()
            .map(|e| {
                (
                    e.path,
                    RemoteHashInfo {
                        is_dir: e.is_dir,
                        size: e.size,
                        hash: e.hash,
                    },
                )
            })
            .collect()
    }))
}

// ---------------------------------------------------------------------------
// Engine: host → device
// ---------------------------------------------------------------------------

/// Mirror a host directory onto the device.
///
/// `baseline` is the manifest from the last successful sync, used to detect
/// files that changed on both sides. Pass `None` for a one-off sync with no
/// conflict tracking.
pub fn to_device(
    dev: &mut MicroPythonDevice,
    local_root: &Path,
    remote_root: &str,
    opts: &SyncOptions,
    baseline: Option<&HashMap<String, String>>,
    report: Option<Reporter<'_>>,
) -> micropython::Result<SyncOutcome> {
    let mut noop = noop_reporter;
    let report: Reporter<'_> = match report {
        Some(r) => r,
        None => &mut noop,
    };
    let mut outcome = SyncOutcome::default();
    let mut emit = |outcome: &mut SyncOutcome, action: SyncAction| {
        report(&action);
        outcome.actions.push(action);
    };

    let patterns = build_ignore_patterns(local_root, &opts.ignore);
    let mut warnings = Vec::new();
    let (entries, local_files) = index_local(local_root, &patterns, &mut |w| warnings.push(w))?;
    for w in warnings {
        emit(
            &mut outcome,
            SyncAction::new("warning", None, None, opts.dry_run).with_note(w),
        );
    }

    let local_paths: HashSet<String> = local_files.keys().cloned().collect();

    // The manifest records the source side; after a successful sync both sides
    // match these hashes.
    for (rel, info) in &local_files {
        if let Some(h) = &info.hash {
            outcome.manifest.insert(rel.clone(), h.clone());
        }
    }

    // A missing remote root is fine in this direction: we create it below.
    let mut remote_entries = index_remote(dev, remote_root)?.unwrap_or_default();

    // Sweep staging files stranded by an interrupted transfer (a hard kill
    // skips `write_file`'s own cleanup). They are filtered out by the ignore
    // rules below, so nothing else would ever remove them.
    let stale: Vec<String> = remote_entries
        .iter()
        .filter(|(rel, info)| {
            !info.is_dir
                && rel
                    .rsplit('/')
                    .next()
                    .is_some_and(|n| n.starts_with(".rupico-tmp-"))
        })
        .map(|(rel, _)| rel.clone())
        .collect();
    for rel in &stale {
        let full = micropython::join_remote_path(remote_root, rel);
        if !opts.dry_run {
            let _ = dev.remove(&full);
        }
        emit(
            &mut outcome,
            SyncAction::new("remove_stale_staging", None, Some(full), opts.dry_run),
        );
    }
    remote_entries.retain(|(rel, _)| !stale.contains(rel));

    remote_entries.retain(|(rel, _)| !path_is_ignored(&PathBuf::from(rel), &patterns));
    let remote_map: HashMap<String, RemoteHashInfo> = remote_entries.iter().cloned().collect();

    if opts.delete {
        // Deepest first, so directories are empty before we try to remove them.
        let mut to_delete: Vec<(String, bool)> = remote_entries
            .iter()
            .filter(|(rel, _)| !local_paths.contains(rel))
            .map(|(rel, info)| (rel.clone(), info.is_dir))
            .collect();
        to_delete.sort_by_key(|(rel, _)| std::cmp::Reverse(rel.matches('/').count()));

        for (rel, is_dir) in to_delete {
            let full = micropython::join_remote_path(remote_root, &rel);
            let op = if is_dir {
                "delete_remote_dir"
            } else {
                "delete_remote_file"
            };
            if opts.dry_run {
                emit(&mut outcome, SyncAction::new(op, None, Some(full), true));
                continue;
            }
            let res = if is_dir {
                dev.rmdir(&full)
            } else {
                dev.remove(&full)
            };
            match res {
                Ok(()) => emit(&mut outcome, SyncAction::new(op, None, Some(full), false)),
                Err(e) => {
                    // A directory still holding ignored files is not empty and
                    // `rmdir` fails. That must not abort a sync that has
                    // already deleted other entries.
                    let skip_op = if is_dir {
                        "skip_delete_remote_dir"
                    } else {
                        "skip_delete_remote_file"
                    };
                    emit(
                        &mut outcome,
                        SyncAction::new(skip_op, None, Some(full), false).with_note(e.to_string()),
                    );
                }
            }
        }
    }

    // Make sure the remote root exists so a first sync to a fresh device works.
    if !opts.dry_run {
        ensure_remote_dir_all(dev, remote_root);
    }

    let mut conflicted: Vec<(String, String)> = Vec::new();

    for (rel, is_dir) in &entries {
        let key = rel_path_to_remote(rel);
        let remote_path = micropython::join_remote_path(remote_root, &key);
        let local_path = local_root.join(rel);

        if *is_dir {
            if !opts.dry_run {
                let _ = dev.mkdir(&remote_path);
            }
            continue;
        }

        let local_info = match local_files.get(&key) {
            Some(i) => i,
            None => continue,
        };
        let remote_info = remote_map.get(&key);

        // Both sides diverged from the recorded baseline, and from each other.
        if let Some(baseline) = baseline
            && let Some(h0) = baseline.get(&key)
            && let Some(info) = remote_info
            && let (Some(lh), Some(rh)) = (local_info.hash.as_ref(), info.hash.as_ref())
            && lh != h0
            && rh != h0
            && lh != rh
        {
            emit(
                &mut outcome,
                SyncAction::new(
                    "conflict",
                    Some(local_path.display().to_string()),
                    Some(remote_path.clone()),
                    opts.dry_run,
                ),
            );
            conflicted.push((key.clone(), h0.clone()));
            if !opts.force {
                continue;
            }
        }

        let should_upload = needs_copy(
            local_info.size,
            local_info.hash.as_deref(),
            remote_info.map(|i| (i.is_dir, i.size, i.hash.as_deref())),
        );

        if should_upload {
            if !opts.dry_run {
                let data = fs::read(&local_path).map_err(micropython::MicroPythonError::Io)?;
                dev.write_file(&remote_path, &data)?;
            }
            emit(
                &mut outcome,
                SyncAction::new(
                    "upload",
                    Some(local_path.display().to_string()),
                    Some(remote_path),
                    opts.dry_run,
                ),
            );
        } else {
            emit(
                &mut outcome,
                SyncAction::new(
                    "skip_upload",
                    Some(local_path.display().to_string()),
                    Some(remote_path),
                    false,
                ),
            );
        }
    }

    outcome.conflicts = settle_conflicts(&mut outcome.manifest, &conflicted, opts.force);
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Engine: device → host
// ---------------------------------------------------------------------------

/// Mirror a device directory onto the host.
pub fn from_device(
    dev: &mut MicroPythonDevice,
    remote_root: &str,
    local_root: &Path,
    opts: &SyncOptions,
    baseline: Option<&HashMap<String, String>>,
    report: Option<Reporter<'_>>,
) -> micropython::Result<SyncOutcome> {
    let mut noop = noop_reporter;
    let report: Reporter<'_> = match report {
        Some(r) => r,
        None => &mut noop,
    };
    let mut outcome = SyncOutcome::default();
    let mut emit = |outcome: &mut SyncOutcome, action: SyncAction| {
        report(&action);
        outcome.actions.push(action);
    };

    let base_patterns = build_ignore_patterns(local_root, &opts.ignore);
    let has_local_ignore_file = local_root.join(".rupicoignore").is_file();

    // A missing remote root must be an error in this direction, never an empty
    // listing: with `--delete`, "the device has no files" means "delete every
    // local file", so a typo in the remote path would wipe the host tree.
    let mut remote_entries = index_remote(dev, remote_root)?.ok_or_else(|| {
        micropython::MicroPythonError::Remote(format!(
            "remote path '{remote_root}' does not exist on the device"
        ))
    })?;

    // Ignore rules normally come from `.rupicoignore` at the local root, but on
    // a first download that directory is empty and the only copy lives on the
    // device. Read it from there so the very first sync honours the project's
    // rules instead of dragging down everything they exist to exclude.
    let mut patterns = base_patterns;
    if !has_local_ignore_file
        && remote_entries
            .iter()
            .any(|(rel, info)| !info.is_dir && rel == ".rupicoignore")
    {
        let remote_ignore = micropython::join_remote_path(remote_root, ".rupicoignore");
        match dev.read_text_file(&remote_ignore) {
            Ok(text) => patterns.extend(parse_ignore_file(&text)),
            Err(e) => emit(
                &mut outcome,
                SyncAction::new("warning", None, Some(remote_ignore), opts.dry_run)
                    .with_note(format!("could not read device ignore file: {e}")),
            ),
        }
    }

    let mut warnings = Vec::new();
    let (local_entries, local_files) =
        index_local(local_root, &patterns, &mut |w| warnings.push(w))?;
    for w in warnings {
        emit(
            &mut outcome,
            SyncAction::new("warning", None, None, opts.dry_run).with_note(w),
        );
    }

    remote_entries.retain(|(rel, _)| !path_is_ignored(&PathBuf::from(rel), &patterns));
    let remote_map: HashMap<String, RemoteHashInfo> = remote_entries.iter().cloned().collect();

    for (rel, info) in &remote_entries {
        if !info.is_dir
            && let Some(h) = &info.hash
        {
            outcome.manifest.insert(rel.clone(), h.clone());
        }
    }

    if opts.delete {
        let remote_paths: HashSet<&String> = remote_map.keys().collect();
        let mut to_delete: Vec<(PathBuf, bool)> = local_entries
            .iter()
            .filter(|(rel, _)| !remote_paths.contains(&rel_path_to_remote(rel)))
            .cloned()
            .collect();
        to_delete.sort_by_key(|(rel, _)| std::cmp::Reverse(rel.components().count()));

        for (rel, is_dir) in to_delete {
            let full = local_root.join(&rel);
            let op = if is_dir {
                "delete_local_dir"
            } else {
                "delete_local_file"
            };
            if opts.dry_run {
                emit(
                    &mut outcome,
                    SyncAction::new(op, Some(full.display().to_string()), None, true),
                );
                continue;
            }
            let res = if is_dir {
                fs::remove_dir(&full)
            } else {
                fs::remove_file(&full)
            };
            match res {
                Ok(()) => emit(
                    &mut outcome,
                    SyncAction::new(op, Some(full.display().to_string()), None, false),
                ),
                Err(e) => {
                    let skip_op = if is_dir {
                        "skip_delete_local_dir"
                    } else {
                        "skip_delete_local_file"
                    };
                    emit(
                        &mut outcome,
                        SyncAction::new(skip_op, Some(full.display().to_string()), None, false)
                            .with_note(e.to_string()),
                    );
                }
            }
        }
    }

    if !opts.dry_run {
        fs::create_dir_all(local_root).map_err(micropython::MicroPythonError::Io)?;
    }

    // Shallower paths first, so parent directories exist before their contents.
    let mut ordered = remote_entries;
    ordered.sort_by_key(|(rel, _)| rel.matches('/').count());

    let mut conflicted: Vec<(String, String)> = Vec::new();

    for (rel, info) in ordered {
        let local_path = local_root.join(&rel);
        let remote_path = micropython::join_remote_path(remote_root, &rel);

        if info.is_dir {
            if !opts.dry_run {
                fs::create_dir_all(&local_path).map_err(micropython::MicroPythonError::Io)?;
            }
            emit(
                &mut outcome,
                SyncAction::new(
                    "ensure_dir",
                    Some(local_path.display().to_string()),
                    Some(remote_path),
                    opts.dry_run,
                ),
            );
            continue;
        }

        let local_info = local_files.get(&rel);

        if let Some(baseline) = baseline
            && let Some(h0) = baseline.get(&rel)
            && let Some(li) = local_info
            && let (Some(lh), Some(rh)) = (li.hash.as_ref(), info.hash.as_ref())
            && lh != h0
            && rh != h0
            && lh != rh
        {
            emit(
                &mut outcome,
                SyncAction::new(
                    "conflict",
                    Some(local_path.display().to_string()),
                    Some(remote_path.clone()),
                    opts.dry_run,
                ),
            );
            conflicted.push((rel.clone(), h0.clone()));
            if !opts.force {
                continue;
            }
        }

        let needs_download = needs_copy(
            info.size,
            info.hash.as_deref(),
            local_info.map(|l| (l.is_dir, l.size, l.hash.as_deref())),
        );

        if needs_download {
            if !opts.dry_run {
                let data = dev.read_file(&remote_path)?;
                if let Some(parent) = local_path.parent() {
                    fs::create_dir_all(parent).map_err(micropython::MicroPythonError::Io)?;
                }
                fs::write(&local_path, data).map_err(micropython::MicroPythonError::Io)?;
            }
            emit(
                &mut outcome,
                SyncAction::new(
                    "download",
                    Some(local_path.display().to_string()),
                    Some(remote_path),
                    opts.dry_run,
                ),
            );
        } else {
            emit(
                &mut outcome,
                SyncAction::new(
                    "skip_download",
                    Some(local_path.display().to_string()),
                    Some(remote_path),
                    false,
                ),
            );
        }
    }

    outcome.conflicts = settle_conflicts(&mut outcome.manifest, &conflicted, opts.force);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_copy_when_destination_missing() {
        assert!(needs_copy(10, Some("abc"), None));
    }

    #[test]
    fn needs_copy_when_destination_is_dir() {
        assert!(needs_copy(10, Some("abc"), Some((true, 0, None))));
    }

    #[test]
    fn needs_copy_compares_hashes_when_available() {
        assert!(!needs_copy(10, Some("abc"), Some((false, 10, Some("abc")))));
        assert!(needs_copy(10, Some("abc"), Some((false, 10, Some("def")))));
        // Hash wins even when sizes differ (e.g. stale size metadata).
        assert!(!needs_copy(10, Some("abc"), Some((false, 99, Some("abc")))));
    }

    #[test]
    fn needs_copy_falls_back_to_size_without_hashes() {
        assert!(!needs_copy(10, None, Some((false, 10, None))));
        assert!(needs_copy(10, None, Some((false, 11, None))));
        assert!(needs_copy(10, Some("abc"), Some((false, 11, None))));
    }

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn ignore_matches_path_components_not_substrings() {
        let pats = vec!["target".to_string()];
        assert!(path_is_ignored(Path::new("target"), &pats));
        assert!(path_is_ignored(Path::new("target/debug/foo"), &pats));
        assert!(path_is_ignored(Path::new("sub/target/foo"), &pats));
        assert!(!path_is_ignored(Path::new("mytarget/foo"), &pats));
        assert!(!path_is_ignored(Path::new("targets/foo"), &pats));
    }

    #[test]
    fn ignore_slash_patterns_match_prefix_only() {
        let pats = vec!["lib/vendor".to_string()];
        assert!(path_is_ignored(Path::new("lib/vendor"), &pats));
        assert!(path_is_ignored(Path::new("lib/vendor/x.py"), &pats));
        assert!(!path_is_ignored(Path::new("lib/vendored/x.py"), &pats));
        assert!(!path_is_ignored(Path::new("other/lib/vendor"), &pats));
    }

    #[test]
    fn glob_patterns_match_extensions() {
        let pats = vec!["*.pyc".to_string()];
        assert!(path_is_ignored(Path::new("foo.pyc"), &pats));
        assert!(path_is_ignored(Path::new("pkg/bar.pyc"), &pats));
        assert!(!path_is_ignored(Path::new("foo.py"), &pats));
    }

    #[test]
    fn glob_match_handles_backtracking() {
        assert!(glob_match("*.py", "a.py"));
        assert!(glob_match("*a*b", "xaybzb"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("*.py", "a.pyc"));
    }

    #[test]
    fn ignore_patterns_tolerate_a_leading_slash() {
        let pats = vec!["/build".to_string()];
        assert!(path_is_ignored(Path::new("build"), &pats));
        assert!(path_is_ignored(Path::new("build/out.py"), &pats));
    }

    #[test]
    fn staging_files_are_ignored_by_default() {
        let pats = build_ignore_patterns(Path::new("/nonexistent"), &[]);
        assert!(path_is_ignored(Path::new(".rupico-tmp-main.py"), &pats));
        assert!(path_is_ignored(Path::new("app/.rupico-tmp-x.py"), &pats));
    }

    #[test]
    fn skipped_conflict_keeps_the_previous_baseline() {
        // Regression: dropping the entry left the next run with no baseline,
        // so the conflict went undetected and the destination was silently
        // overwritten. The old baseline must survive instead.
        let mut manifest = HashMap::new();
        manifest.insert("app.py".to_string(), "new-source-hash".to_string());
        let conflicted = vec![("app.py".to_string(), "baseline-hash".to_string())];

        let remaining = settle_conflicts(&mut manifest, &conflicted, false);

        assert_eq!(remaining, 1);
        assert_eq!(
            manifest.get("app.py").map(String::as_str),
            Some("baseline-hash")
        );
    }

    #[test]
    fn forced_conflict_records_the_new_hash_and_reports_nothing_outstanding() {
        let mut manifest = HashMap::new();
        manifest.insert("app.py".to_string(), "new-source-hash".to_string());
        let conflicted = vec![("app.py".to_string(), "baseline-hash".to_string())];

        let remaining = settle_conflicts(&mut manifest, &conflicted, true);

        assert_eq!(remaining, 0, "--force resolves conflicts, so none remain");
        assert_eq!(
            manifest.get("app.py").map(String::as_str),
            Some("new-source-hash")
        );
    }
}
