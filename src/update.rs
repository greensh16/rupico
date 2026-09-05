//! Self-update: check GitHub Releases and replace the running executable.
//!
//! Shared by the CLI's `update` command and the GUI's update dialog.
//!
//! # Trust model
//!
//! This downloads code and then runs it, so the checks are deliberately
//! strict and **fail closed**:
//!
//! - Everything is fetched over HTTPS from `api.github.com` and
//!   `objects.githubusercontent.com`; no plaintext fallback.
//! - The release must publish a `SHA256SUMS` asset. If it is missing, or does
//!   not list the archive we downloaded, the update is refused rather than
//!   installed unverified.
//! - The archive's sha256 must match that entry exactly.
//! - Only the *running* executable is replaced, and only after the new binary
//!   has been extracted and its checksum verified.
//!
//! Nothing here happens automatically. A user has to ask for it — there is no
//! background check, and no network traffic unless one of these functions is
//! called.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Repository that releases are published from.
pub const REPO: &str = "greensh16/rupico";

/// Cap on any single download. A release archive is a few MB; this stops a
/// hostile or broken endpoint from streaming until the disk fills.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Network timeout for the whole check, in seconds.
const HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Debug)]
pub enum UpdateError {
    /// Network or HTTP failure.
    Http(String),
    /// The GitHub response was not what we expected.
    Api(String),
    /// No release, or no asset, for this platform.
    NoAsset(String),
    /// Checksum missing or mismatched — the update was refused.
    Verification(String),
    /// Unpacking the archive failed.
    Archive(String),
    /// Replacing the executable failed.
    Install(String),
    Io(std::io::Error),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(m) => write!(f, "network error: {m}"),
            Self::Api(m) => write!(f, "unexpected response from GitHub: {m}"),
            Self::NoAsset(m) => write!(f, "{m}"),
            Self::Verification(m) => write!(f, "refusing to install: {m}"),
            Self::Archive(m) => write!(f, "could not unpack the download: {m}"),
            Self::Install(m) => write!(f, "could not replace the executable: {m}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The target triple this binary was built for, matching the names the
/// release workflow uses for its archives.
pub fn target_triple() -> &'static str {
    // Kept in step with the matrix in .github/workflows/release.yml.
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else {
        "unsupported"
    }
}

/// Archive extension used for this platform by the release workflow.
fn archive_extension() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

/// Asset filename the release workflow produces for a given version.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("rupico-{version}-{target}.{}", archive_extension())
}

#[derive(Debug, Clone, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

/// A published release, as far as the updater cares.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    /// Tag with any leading `v` removed.
    pub version: String,
    pub html_url: String,
    assets: Vec<ApiAsset>,
}

impl Release {
    fn url_of(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
    }
}

/// What a check found.
#[derive(Debug, Clone)]
pub enum Check {
    /// Already on the newest published release.
    UpToDate { current: String },
    /// A newer release is available.
    Available { current: String, release: Release },
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
        .build()
        .into()
}

fn user_agent() -> String {
    format!("rupico/{}", current_version())
}

/// Fetch the latest published release.
///
/// Returns `Ok(None)` when the project has no releases yet, which is a normal
/// state rather than an error.
pub fn fetch_latest(repo: &str) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut resp = match agent()
        .get(&url)
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        // A project with no releases yet answers 404 here, which is a normal
        // state rather than a failure.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(UpdateError::Http(e.to_string())),
    };

    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    let api: ApiRelease =
        serde_json::from_str(&body).map_err(|e| UpdateError::Api(e.to_string()))?;

    Ok(Some(Release {
        version: api.tag_name.trim_start_matches('v').to_string(),
        tag: api.tag_name,
        html_url: api.html_url,
        assets: api.assets,
    }))
}

/// Compare two version strings, tolerating a leading `v` and a two-part form.
///
/// Uses real semver rather than a hand-rolled comparison, because getting this
/// wrong means either never updating or silently downgrading.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        // If either side is unparseable, refuse to claim an update is newer.
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<semver::Version> {
    let v = v.trim().trim_start_matches('v');
    if let Ok(parsed) = semver::Version::parse(v) {
        return Some(parsed);
    }
    // Accept a two-part tag such as `0.1` as `0.1.0`.
    let core = v.split(['-', '+']).next().unwrap_or(v);
    if core.split('.').count() == 2 {
        let rest = &v[core.len()..];
        return semver::Version::parse(&format!("{core}.0{rest}")).ok();
    }
    None
}

/// Check whether a newer release than the running binary exists.
pub fn check(repo: &str) -> Result<Option<Check>> {
    let current = current_version().to_string();
    let Some(release) = fetch_latest(repo)? else {
        return Ok(None);
    };
    if is_newer(&current, &release.version) {
        Ok(Some(Check::Available { current, release }))
    } else {
        Ok(Some(Check::UpToDate { current }))
    }
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", user_agent())
        .call()
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    resp.body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .map_err(|e| UpdateError::Http(e.to_string()))
}

/// Find `file`'s expected digest in a `SHA256SUMS` listing.
///
/// Accepts the `<hash>  <name>` layout `sha256sum` produces, with or without a
/// leading `./` on the name.
pub fn expected_digest(sums: &str, file: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*').trim_start_matches("./");
        if name == file && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Extract one named executable from a release archive.
fn extract_binary(archive: &[u8], bin_name: &str, into: &Path) -> Result<PathBuf> {
    let out = into.join(bin_name);

    if cfg!(windows) {
        let reader = std::io::Cursor::new(archive);
        let mut zip =
            zip::ZipArchive::new(reader).map_err(|e| UpdateError::Archive(e.to_string()))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| UpdateError::Archive(e.to_string()))?;
            let matches = entry
                .name()
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|n| n == bin_name);
            if matches {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                std::fs::write(&out, bytes)?;
                return Ok(out);
            }
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(decoder);
        for entry in tar
            .entries()
            .map_err(|e| UpdateError::Archive(e.to_string()))?
        {
            let mut entry = entry.map_err(|e| UpdateError::Archive(e.to_string()))?;
            let path = entry
                .path()
                .map_err(|e| UpdateError::Archive(e.to_string()))?
                .into_owned();
            if path.file_name().and_then(|n| n.to_str()) == Some(bin_name) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                std::fs::write(&out, &bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
                }
                return Ok(out);
            }
        }
    }

    Err(UpdateError::Archive(format!(
        "the archive does not contain {bin_name}"
    )))
}

/// The file name of the running executable, without any `.exe` suffix.
pub fn running_binary_name() -> Result<String> {
    let exe = std::env::current_exe()?;
    let name = exe
        .file_stem()
        .and_then(|n| n.to_str())
        .ok_or_else(|| UpdateError::Install("cannot determine this executable's name".into()))?;
    Ok(name.to_string())
}

/// Download, verify and install `release`, replacing the running executable.
///
/// Only the running binary is replaced. If you use both `rupico` and
/// `rupico_gui`, update each one.
pub fn install(release: &Release) -> Result<()> {
    let target = target_triple();
    if target == "unsupported" {
        return Err(UpdateError::NoAsset(format!(
            "no prebuilt binaries are published for {}-{}; \
             build from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        )));
    }

    let archive_name = asset_name(&release.version, target);
    let archive_url = release.url_of(&archive_name).ok_or_else(|| {
        UpdateError::NoAsset(format!(
            "release {} has no asset named {archive_name}",
            release.tag
        ))
    })?;

    // Fail closed: without a checksum list we do not install anything.
    let sums_url = release.url_of("SHA256SUMS").ok_or_else(|| {
        UpdateError::Verification(format!(
            "release {} publishes no SHA256SUMS to verify against",
            release.tag
        ))
    })?;

    let sums = String::from_utf8(download(sums_url)?)
        .map_err(|e| UpdateError::Verification(format!("SHA256SUMS is not text: {e}")))?;
    let expected = expected_digest(&sums, &archive_name).ok_or_else(|| {
        UpdateError::Verification(format!("SHA256SUMS does not list {archive_name}"))
    })?;

    let archive = download(archive_url)?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        return Err(UpdateError::Verification(format!(
            "checksum mismatch for {archive_name}\n  expected {expected}\n  got      {actual}"
        )));
    }

    let bin_name = running_binary_name()?;
    let staging = tempdir()?;
    let new_binary = extract_binary(&archive, &bin_name, staging.path())?;

    // `self_replace` handles the platform-specific dance, notably Windows,
    // where a running executable cannot simply be overwritten.
    self_replace::self_replace(&new_binary).map_err(|e| {
        UpdateError::Install(format!(
            "{e}. If rupico was installed system-wide you may need elevated \
             permissions, or to update it the way you installed it."
        ))
    })?;

    Ok(())
}

/// A directory that deletes itself when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> Result<TempDir> {
    let mut dir = std::env::temp_dir();
    // Enough to avoid collisions between concurrent runs without pulling in a
    // random-number dependency.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.push(format!("rupico-update-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(TempDir(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.9.0", "1.0.0"));
        assert!(is_newer("0.1.0", "v0.2.0"), "a leading v is tolerated");
        assert!(is_newer("0.1.0", "0.2"), "a two-part tag is tolerated");
    }

    #[test]
    fn same_or_older_versions_are_not_offered() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "v0.1"), "0.1 and 0.1.0 are the same");
    }

    #[test]
    fn a_prerelease_does_not_supersede_its_release() {
        // 0.2.0-rc1 sorts before 0.2.0 in semver, and must not be offered as
        // an upgrade from it.
        assert!(!is_newer("0.2.0", "0.2.0-rc1"));
        assert!(is_newer("0.2.0-rc1", "0.2.0"));
    }

    #[test]
    fn unparseable_versions_never_claim_to_be_newer() {
        // Fail closed: a malformed tag must not trigger a binary replacement.
        assert!(!is_newer("0.1.0", "not-a-version"));
        assert!(!is_newer("not-a-version", "0.2.0"));
        assert!(!is_newer("0.1.0", ""));
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        let name = asset_name("0.1.0", "x86_64-unknown-linux-gnu");
        let expected = if cfg!(windows) {
            "rupico-0.1.0-x86_64-unknown-linux-gnu.zip"
        } else {
            "rupico-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        };
        assert_eq!(name, expected);
    }

    #[test]
    fn digest_is_found_in_a_sha256sums_listing() {
        let sums = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  rupico-0.1.0-aarch64-apple-darwin.tar.gz
d1e2f3a4b5c6978899aabbccddeeff00112233445566778899aabbccddeeff00  rupico-0.1.0-x86_64-pc-windows-msvc.zip
";
        assert_eq!(
            expected_digest(sums, "rupico-0.1.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert!(expected_digest(sums, "rupico-9.9.9-nope.tar.gz").is_none());
    }

    #[test]
    fn digest_lookup_tolerates_sha256sum_path_forms() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for line in [
            format!("{hash}  ./rupico-0.1.0-x.tar.gz"),
            format!("{hash} *rupico-0.1.0-x.tar.gz"),
            format!("{hash}\trupico-0.1.0-x.tar.gz"),
        ] {
            assert_eq!(
                expected_digest(&line, "rupico-0.1.0-x.tar.gz").as_deref(),
                Some(hash),
                "failed for {line:?}"
            );
        }
    }

    #[test]
    fn a_malformed_digest_line_is_rejected() {
        // Anything that is not 64 hex characters must not be accepted as a
        // digest, or verification could be bypassed by a crafted listing.
        assert!(expected_digest("nothex  rupico.tar.gz", "rupico.tar.gz").is_none());
        assert!(expected_digest("  rupico.tar.gz", "rupico.tar.gz").is_none());
        assert!(
            expected_digest(
                "zzz0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  rupico.tar.gz",
                "rupico.tar.gz"
            )
            .is_none()
        );
    }

    /// A realistic `releases/latest` payload, including fields we do not read,
    /// to confirm the deserializer tolerates everything GitHub actually sends.
    const FIXTURE: &str = r#"{
      "url": "https://api.github.com/repos/greensh16/rupico/releases/1",
      "id": 1,
      "tag_name": "v0.2.0",
      "target_commitish": "main",
      "name": "v0.2.0",
      "draft": false,
      "prerelease": false,
      "created_at": "2026-01-01T00:00:00Z",
      "published_at": "2026-01-01T00:00:00Z",
      "html_url": "https://github.com/greensh16/rupico/releases/tag/v0.2.0",
      "body": "notes",
      "assets": [
        {
          "id": 10,
          "name": "rupico-0.2.0-aarch64-apple-darwin.tar.gz",
          "content_type": "application/gzip",
          "size": 5551234,
          "download_count": 3,
          "browser_download_url": "https://github.com/greensh16/rupico/releases/download/v0.2.0/rupico-0.2.0-aarch64-apple-darwin.tar.gz"
        },
        {
          "id": 11,
          "name": "SHA256SUMS",
          "content_type": "text/plain",
          "size": 400,
          "download_count": 1,
          "browser_download_url": "https://github.com/greensh16/rupico/releases/download/v0.2.0/SHA256SUMS"
        }
      ]
    }"#;

    #[test]
    fn a_real_github_payload_deserializes() {
        let api: ApiRelease = serde_json::from_str(FIXTURE).expect("payload should parse");
        let release = Release {
            version: api.tag_name.trim_start_matches('v').to_string(),
            tag: api.tag_name,
            html_url: api.html_url,
            assets: api.assets,
        };

        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.version, "0.2.0");
        assert!(is_newer("0.1.0", &release.version));
        assert!(
            release.url_of("SHA256SUMS").is_some(),
            "the checksum asset must be findable, or install refuses"
        );
        assert!(
            release
                .url_of("rupico-0.2.0-aarch64-apple-darwin.tar.gz")
                .is_some()
        );
        assert!(release.url_of("rupico-0.2.0-nonexistent.tar.gz").is_none());
    }

    #[test]
    fn a_release_without_checksums_is_refused() {
        // Fail closed: no SHA256SUMS asset means no install, however valid the
        // archive looks.
        let api: ApiRelease = serde_json::from_str(FIXTURE).unwrap();
        let stripped = Release {
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            html_url: api.html_url,
            assets: api
                .assets
                .into_iter()
                .filter(|a| a.name != "SHA256SUMS")
                .collect(),
        };
        assert!(stripped.url_of("SHA256SUMS").is_none());

        let err = install(&stripped).expect_err("must refuse without checksums");
        assert!(
            matches!(err, UpdateError::Verification(_) | UpdateError::NoAsset(_)),
            "expected a refusal, got {err:?}"
        );
    }

    #[test]
    fn this_build_has_a_known_target_triple() {
        // If this fails on a platform we ship for, the release matrix and
        // `target_triple` have drifted apart.
        let t = target_triple();
        assert!(
            t != "unsupported",
            "no release target mapped for this platform"
        );
        assert!(asset_name(current_version(), t).starts_with("rupico-"));
    }
}
