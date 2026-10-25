//! Download, verify, and cache language-server binaries under
//! `~/.astrolabe/servers/{name}/{version}/`.
//!
//! ## Policy (RFC P0)
//!
//! - Discovery never calls [`ensure_installed`] unless
//!   [`AutoInstallMode::On`] (`ASTROLABE_AUTO_INSTALL=on`).
//! - Default mode is [`AutoInstallMode::Prompt`]: surface
//!   [`NeedsInstall`] / [`InstallHint`] and wait for an explicit session decision.
//! - Checksums are mandatory for downloaded artifacts; a missing or mismatched
//!   digest fails the install.
//! - `ASTROLABE_OFFLINE=1` refuses network downloads.
//! - `ASTROLABE_MIRROR_GITHUB` rewrites `https://github.com/` URLs.

mod background;
mod extract;
mod fetch;
mod lock;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::types::Language;

pub use background::{BackgroundInstall, Installs, INSTALL_PENDING, RETRY_FAILED_AFTER};
pub use extract::ArchiveFormat;
pub use fetch::{Fetcher, UreqFetcher};

/// Environment variable selecting install behavior.
pub const ENV_AUTO_INSTALL: &str = "ASTROLABE_AUTO_INSTALL";
/// Prefix rewritten onto `https://github.com/` download URLs.
pub const ENV_MIRROR_GITHUB: &str = "ASTROLABE_MIRROR_GITHUB";
/// When truthy (`1`/`true`/`yes`/`on`), downloads are refused.
pub const ENV_OFFLINE: &str = "ASTROLABE_OFFLINE";

/// How discovery may react when a server binary is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AutoInstallMode {
    /// Default. Do not download; return structured [`NeedsInstall`].
    #[default]
    Prompt,
    /// Never download; unavailable stays unavailable.
    Off,
    /// Download via [`ensure_installed`] once PATH and cache miss.
    On,
}

impl AutoInstallMode {
    /// Parse `ASTROLABE_AUTO_INSTALL`. Unknown / empty → [`Prompt`].
    pub fn from_env() -> Self {
        match std::env::var(ENV_AUTO_INSTALL)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "0" | "false" | "no" => AutoInstallMode::Off,
            "on" | "1" | "true" | "yes" => AutoInstallMode::On,
            "prompt" | "" => AutoInstallMode::Prompt,
            _ => AutoInstallMode::Prompt,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AutoInstallMode::Prompt => "prompt",
            AutoInstallMode::Off => "off",
            AutoInstallMode::On => "on",
        }
    }
}

/// Artifact description for [`ensure_installed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSpec {
    /// Directory name under `servers/` (e.g. `clangd`).
    pub name: String,
    /// Version directory name (e.g. `18.1.3`). Must be a single path segment.
    pub version: String,
    /// Download URL. May be rewritten by [`ENV_MIRROR_GITHUB`].
    pub url: String,
    /// Expected SHA-256 of the **downloaded archive bytes** (lowercase hex).
    pub sha256: String,
    pub archive: ArchiveFormat,
    /// Path of the runnable binary relative to the version directory after extract.
    pub binary_relative: PathBuf,
}

impl InstallSpec {
    pub fn version_dir(&self, cache_root: &Path) -> PathBuf {
        cache_root.join(&self.name).join(&self.version)
    }

    pub fn binary_path(&self, cache_root: &Path) -> PathBuf {
        self.version_dir(cache_root).join(&self.binary_relative)
    }

    pub fn marker_path(&self, cache_root: &Path) -> PathBuf {
        self.version_dir(cache_root).join(".installed")
    }
}

/// Human- and MCP-facing install guidance when a server is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallHint {
    pub language: Language,
    pub server_name: String,
    /// One-line / multi-line manual install instructions.
    pub message: String,
    /// Whether [`ensure_installed`] has a catalog entry for this server.
    pub auto_installable: bool,
    pub mode: AutoInstallMode,
    /// Where a successful install would land (`…/servers/{name}`).
    pub cache_dir: PathBuf,
}

impl InstallHint {
    pub fn summary(&self) -> String {
        let auto = if self.auto_installable {
            format!(
                " Auto-install is available when {ENV_AUTO_INSTALL}=on (current mode: {}).",
                self.mode.as_str()
            )
        } else {
            String::new()
        };
        format!("{}{}", self.message, auto)
    }
}

/// Structured "needs install" payload for MCP / session prompts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsInstall {
    pub hint: InstallHint,
    /// Spec to pass to [`ensure_installed`] after the session permits it.
    pub spec: Option<InstallSpec>,
}

impl NeedsInstall {
    pub fn message(&self) -> String {
        self.hint.summary()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("offline mode ({ENV_OFFLINE}) refuses download of {0}")]
    Offline(String),
    #[error("checksum missing for {0}; refusing unverified install")]
    MissingChecksum(String),
    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("download failed for {0}: {1}")]
    Download(String, String),
    #[error("extract failed for {0}: {1}")]
    Extract(String, String),
    #[error("install lock busy for {0}: {1}")]
    Lock(String, String),
    #[error("io error during install of {0}: {1}")]
    Io(String, #[source] io::Error),
    #[error("invalid install spec for {0}: {1}")]
    InvalidSpec(String, String),
}

/// Runtime knobs for [`ensure_installed_with`]. Tests inject a mock [`Fetcher`]
/// and a temp `cache_root` so no real network or home directory is touched.
pub struct InstallContext<'a> {
    pub cache_root: PathBuf,
    pub fetcher: &'a dyn Fetcher,
    pub offline: bool,
    pub mirror_github: Option<String>,
    pub lock_timeout: Duration,
}

impl<'a> InstallContext<'a> {
    /// Production defaults from process env + [`UreqFetcher`].
    pub fn from_env(fetcher: &'a dyn Fetcher) -> Self {
        Self {
            cache_root: default_servers_dir(),
            fetcher,
            offline: env_truthy(ENV_OFFLINE),
            mirror_github: std::env::var(ENV_MIRROR_GITHUB)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            lock_timeout: Duration::from_secs(120),
        }
    }
}

/// `~/.astrolabe/servers` (or `$HOME` override when home is missing).
pub fn default_servers_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".astrolabe").join("servers")
}

/// Ensure `spec` is present under `ctx.cache_root`, downloading if needed.
///
/// Returns the absolute path of the runnable binary. Never consults
/// [`AutoInstallMode`] — callers (discovery) decide whether to invoke this.
pub fn ensure_installed(spec: &InstallSpec) -> Result<PathBuf, InstallError> {
    let fetcher = UreqFetcher::default();
    ensure_installed_with(spec, &InstallContext::from_env(&fetcher))
}

/// Injectable variant used by unit tests and by discovery with a custom cache.
pub fn ensure_installed_with(
    spec: &InstallSpec,
    ctx: &InstallContext<'_>,
) -> Result<PathBuf, InstallError> {
    validate_spec(spec)?;
    let dest = spec.binary_path(&ctx.cache_root);
    if is_runnable(&dest) && spec.marker_path(&ctx.cache_root).is_file() {
        return Ok(dest);
    }

    if ctx.offline {
        return Err(InstallError::Offline(spec.name.clone()));
    }
    if spec.sha256.trim().is_empty() {
        return Err(InstallError::MissingChecksum(spec.name.clone()));
    }

    let version_dir = spec.version_dir(&ctx.cache_root);
    fs::create_dir_all(version_dir.parent().unwrap_or(Path::new(".")))
        .map_err(|e| InstallError::Io(spec.name.clone(), e))?;

    let lock_path = version_dir
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".{}.install.lock", spec.version));
    let _guard = lock::acquire(&lock_path, ctx.lock_timeout)
        .map_err(|e| InstallError::Lock(spec.name.clone(), e.to_string()))?;

    // Re-check after lock: another process may have finished.
    if is_runnable(&dest) && spec.marker_path(&ctx.cache_root).is_file() {
        return Ok(dest);
    }

    let url = rewrite_mirror(&spec.url, ctx.mirror_github.as_deref());
    let bytes = ctx
        .fetcher
        .fetch(&url)
        .map_err(|e| InstallError::Download(spec.name.clone(), e))?;

    let actual = hex::encode(Sha256::digest(&bytes));
    let expected = spec.sha256.trim().to_ascii_lowercase();
    if actual != expected {
        return Err(InstallError::ChecksumMismatch {
            name: spec.name.clone(),
            expected,
            actual,
        });
    }

    let staging = unique_staging(&version_dir)?;
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    fs::create_dir_all(&staging).map_err(|e| InstallError::Io(spec.name.clone(), e))?;

    extract::extract(spec.archive, &bytes, &staging, &spec.binary_relative).map_err(|e| {
        let _ = fs::remove_dir_all(&staging);
        InstallError::Extract(spec.name.clone(), e)
    })?;

    // Replace destination atomically when possible.
    if version_dir.exists() {
        let backup = version_dir.with_extension(format!(
            "bak-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = fs::rename(&version_dir, &backup);
        let _ = fs::remove_dir_all(&backup);
    }
    fs::rename(&staging, &version_dir).map_err(|e| {
        let _ = fs::remove_dir_all(&staging);
        InstallError::Io(spec.name.clone(), e)
    })?;

    write_marker(spec, &ctx.cache_root)?;

    let final_bin = spec.binary_path(&ctx.cache_root);
    if !is_runnable(&final_bin) {
        return Err(InstallError::Extract(
            spec.name.clone(),
            format!(
                "binary missing or not executable after extract: {}",
                final_bin.display()
            ),
        ));
    }
    Ok(final_bin)
}

fn validate_spec(spec: &InstallSpec) -> Result<(), InstallError> {
    if spec.name.is_empty()
        || spec.name.contains('/')
        || spec.name.contains('\\')
        || spec.name.contains("..")
    {
        return Err(InstallError::InvalidSpec(
            spec.name.clone(),
            "name must be a single path segment".into(),
        ));
    }
    if spec.version.is_empty()
        || spec.version.contains('/')
        || spec.version.contains('\\')
        || spec.version.contains("..")
    {
        return Err(InstallError::InvalidSpec(
            spec.name.clone(),
            "version must be a single path segment".into(),
        ));
    }
    if spec.binary_relative.as_os_str().is_empty()
        || spec
            .binary_relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(InstallError::InvalidSpec(
            spec.name.clone(),
            "binary_relative must be a relative path without ..".into(),
        ));
    }
    Ok(())
}

fn write_marker(spec: &InstallSpec, cache_root: &Path) -> Result<(), InstallError> {
    let path = spec.marker_path(cache_root);
    let mut f = fs::File::create(&path).map_err(|e| InstallError::Io(spec.name.clone(), e))?;
    writeln!(
        f,
        "name={}\nversion={}\nsha256={}\nurl={}",
        spec.name, spec.version, spec.sha256, spec.url
    )
    .map_err(|e| InstallError::Io(spec.name.clone(), e))?;
    Ok(())
}

fn unique_staging(version_dir: &Path) -> Result<PathBuf, InstallError> {
    let parent = version_dir.parent().unwrap_or(Path::new("."));
    let name = version_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pkg");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(parent.join(format!(".{name}.staging-{stamp}")))
}

pub(crate) fn rewrite_mirror(url: &str, mirror: Option<&str>) -> String {
    let Some(mirror) = mirror.filter(|m| !m.is_empty()) else {
        return url.to_string();
    };
    const GH: &str = "https://github.com/";
    if let Some(rest) = url.strip_prefix(GH) {
        let mirror = mirror.trim_end_matches('/');
        return format!("{mirror}/{rest}");
    }
    url.to_string()
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Look under `cache_root/{server_name}/*/` for a runnable `binary_relative`.
/// Prefers lexicographically greatest version directory (newest-looking pin).
pub fn find_cached_binary(
    cache_root: &Path,
    server_name: &str,
    binary_relative: &Path,
) -> Option<PathBuf> {
    let root = cache_root.join(server_name);
    let mut versions: Vec<PathBuf> = fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            !name.starts_with('.')
        })
        .collect();
    versions.sort();
    for dir in versions.into_iter().rev() {
        let candidate = dir.join(binary_relative);
        let marker = dir.join(".installed");
        if is_runnable(&candidate) && marker.is_file() {
            return Some(candidate);
        }
        // Accept runnable binary even without marker (manual drop-in).
        if is_runnable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Build an [`InstallHint`] / [`NeedsInstall`] for MCP and discovery.
pub fn needs_install_for(
    language: Language,
    message: impl Into<String>,
    server_name: impl Into<String>,
    spec: Option<InstallSpec>,
    mode: AutoInstallMode,
    cache_root: &Path,
) -> NeedsInstall {
    let server_name = server_name.into();
    let cache_dir = cache_root.join(&server_name);
    NeedsInstall {
        hint: InstallHint {
            language,
            server_name,
            message: message.into(),
            auto_installable: spec.is_some(),
            mode,
            cache_dir,
        },
        spec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use flate2::write::GzEncoder;
    use flate2::Compression;

    struct MockFetcher {
        responses: Mutex<HashMap<String, Result<Vec<u8>, String>>>,
        hits: Mutex<Vec<String>>,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                hits: Mutex::new(Vec::new()),
            }
        }

        fn with(url: &str, body: Vec<u8>) -> Self {
            let m = Self::new();
            m.responses
                .lock()
                .unwrap()
                .insert(url.to_string(), Ok(body));
            m
        }
    }

    impl Fetcher for MockFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
            self.hits.lock().unwrap().push(url.to_string());
            self.responses
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("unexpected url {url}")))
        }
    }

    fn make_targz_with_bin(rel: &str, contents: &[u8]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let enc = GzEncoder::new(&mut raw, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, rel, contents).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        raw
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn auto_install_mode_parses() {
        assert_eq!(AutoInstallMode::default(), AutoInstallMode::Prompt);
        assert_eq!(AutoInstallMode::Prompt.as_str(), "prompt");
    }

    #[test]
    fn mirror_rewrites_github_only() {
        assert_eq!(
            rewrite_mirror(
                "https://github.com/foo/bar/releases/x",
                Some("https://ghproxy.com/")
            ),
            "https://ghproxy.com/foo/bar/releases/x"
        );
        assert_eq!(
            rewrite_mirror("https://example.com/x", Some("https://ghproxy.com/")),
            "https://example.com/x"
        );
    }

    #[test]
    fn ensure_installed_downloads_verifies_and_extracts() {
        let dir = scratch();
        let archive = make_targz_with_bin("bin/demo-ls", b"#!/bin/sh\nexit 0\n");
        let sha = hex::encode(Sha256::digest(&archive));
        let url = "https://example.test/demo-ls.tar.gz";
        let fetcher = MockFetcher::with(url, archive);
        let spec = InstallSpec {
            name: "demo-ls".into(),
            version: "1.0.0".into(),
            url: url.into(),
            sha256: sha,
            archive: ArchiveFormat::TarGz,
            binary_relative: PathBuf::from("bin/demo-ls"),
        };
        let ctx = InstallContext {
            cache_root: dir.path().to_path_buf(),
            fetcher: &fetcher,
            offline: false,
            mirror_github: None,
            lock_timeout: Duration::from_secs(5),
        };
        let bin = ensure_installed_with(&spec, &ctx).unwrap();
        assert!(bin.is_file());
        assert!(is_runnable(&bin));
        assert!(spec.marker_path(dir.path()).is_file());
        // Second call hits cache — fetcher must not be contacted again.
        let before = fetcher.hits.lock().unwrap().len();
        let again = ensure_installed_with(&spec, &ctx).unwrap();
        assert_eq!(bin, again);
        assert_eq!(fetcher.hits.lock().unwrap().len(), before);
    }

    #[test]
    fn checksum_mismatch_refuses_install() {
        let dir = scratch();
        let archive = make_targz_with_bin("bin/x", b"x");
        let url = "https://example.test/x.tar.gz";
        let fetcher = MockFetcher::with(url, archive);
        let spec = InstallSpec {
            name: "x".into(),
            version: "1".into(),
            url: url.into(),
            sha256: "0".repeat(64),
            archive: ArchiveFormat::TarGz,
            binary_relative: PathBuf::from("bin/x"),
        };
        let ctx = InstallContext {
            cache_root: dir.path().to_path_buf(),
            fetcher: &fetcher,
            offline: false,
            mirror_github: None,
            lock_timeout: Duration::from_secs(5),
        };
        let err = ensure_installed_with(&spec, &ctx).unwrap_err();
        assert!(matches!(err, InstallError::ChecksumMismatch { .. }));
        assert!(!spec.version_dir(dir.path()).exists());
    }

    #[test]
    fn offline_refuses_download() {
        let dir = scratch();
        let fetcher = MockFetcher::new();
        let spec = InstallSpec {
            name: "x".into(),
            version: "1".into(),
            url: "https://example.test/x".into(),
            sha256: "ab".into(),
            archive: ArchiveFormat::Raw,
            binary_relative: PathBuf::from("x"),
        };
        let ctx = InstallContext {
            cache_root: dir.path().to_path_buf(),
            fetcher: &fetcher,
            offline: true,
            mirror_github: None,
            lock_timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            ensure_installed_with(&spec, &ctx).unwrap_err(),
            InstallError::Offline(_)
        ));
        assert!(fetcher.hits.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_checksum_refuses() {
        let dir = scratch();
        let fetcher = MockFetcher::new();
        let spec = InstallSpec {
            name: "x".into(),
            version: "1".into(),
            url: "https://example.test/x".into(),
            sha256: "  ".into(),
            archive: ArchiveFormat::Raw,
            binary_relative: PathBuf::from("x"),
        };
        let ctx = InstallContext {
            cache_root: dir.path().to_path_buf(),
            fetcher: &fetcher,
            offline: false,
            mirror_github: None,
            lock_timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            ensure_installed_with(&spec, &ctx).unwrap_err(),
            InstallError::MissingChecksum(_)
        ));
    }

    #[test]
    fn zip_extract_works() {
        let dir = scratch();
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            zip.start_file("tool", opts).unwrap();
            zip.write_all(b"#!/bin/sh\n").unwrap();
            zip.finish().unwrap();
        }
        let archive = cursor.into_inner();
        let sha = hex::encode(Sha256::digest(&archive));
        let url = "https://example.test/tool.zip";
        let fetcher = MockFetcher::with(url, archive);
        let spec = InstallSpec {
            name: "tool".into(),
            version: "2".into(),
            url: url.into(),
            sha256: sha,
            archive: ArchiveFormat::Zip,
            binary_relative: PathBuf::from("tool"),
        };
        let ctx = InstallContext {
            cache_root: dir.path().to_path_buf(),
            fetcher: &fetcher,
            offline: false,
            mirror_github: None,
            lock_timeout: Duration::from_secs(5),
        };
        let bin = ensure_installed_with(&spec, &ctx).unwrap();
        assert_eq!(fs::read(&bin).unwrap(), b"#!/bin/sh\n");
    }

    #[test]
    fn find_cached_binary_prefers_newest_version() {
        let dir = scratch();
        let v1 = dir.path().join("clangd").join("1.0.0");
        let v2 = dir.path().join("clangd").join("2.0.0");
        for (root, body) in [(&v1, b"v1"), (&v2, b"v2")] {
            fs::create_dir_all(root).unwrap();
            let bin = root.join("clangd");
            fs::write(&bin, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut p = fs::metadata(&bin).unwrap().permissions();
                p.set_mode(0o755);
                fs::set_permissions(&bin, p).unwrap();
            }
            fs::write(root.join(".installed"), "ok").unwrap();
        }
        let found = find_cached_binary(dir.path(), "clangd", Path::new("clangd")).unwrap();
        assert_eq!(fs::read(&found).unwrap(), b"v2");
    }

    #[test]
    fn needs_install_summary_mentions_mode() {
        let n = needs_install_for(
            Language::Cpp,
            "install clangd",
            "clangd",
            Some(InstallSpec {
                name: "clangd".into(),
                version: "1".into(),
                url: "https://example.test/x".into(),
                sha256: "aa".into(),
                archive: ArchiveFormat::Raw,
                binary_relative: PathBuf::from("clangd"),
            }),
            AutoInstallMode::Prompt,
            Path::new("/tmp/servers"),
        );
        assert!(n.message().contains("ASTROLABE_AUTO_INSTALL=on"));
        assert!(n.hint.auto_installable);
        assert_eq!(n.hint.language, Language::Cpp);
    }

    #[test]
    fn concurrent_lock_serializes_install() {
        let dir = scratch();
        let archive = make_targz_with_bin("bin/y", b"y");
        let sha = hex::encode(Sha256::digest(&archive));
        let url = "https://example.test/y.tar.gz";
        let fetcher = Arc::new(MockFetcher::with(url, archive));
        let spec = Arc::new(InstallSpec {
            name: "y".into(),
            version: "1".into(),
            url: url.into(),
            sha256: sha,
            archive: ArchiveFormat::TarGz,
            binary_relative: PathBuf::from("bin/y"),
        });
        let root = dir.path().to_path_buf();
        let f1 = fetcher.clone();
        let s1 = spec.clone();
        let r1 = root.clone();
        let t1 = std::thread::spawn(move || {
            let ctx = InstallContext {
                cache_root: r1,
                fetcher: f1.as_ref(),
                offline: false,
                mirror_github: None,
                lock_timeout: Duration::from_secs(60),
            };
            ensure_installed_with(&s1, &ctx)
        });
        let f2 = fetcher.clone();
        let s2 = spec.clone();
        let r2 = root.clone();
        let t2 = std::thread::spawn(move || {
            let ctx = InstallContext {
                cache_root: r2,
                fetcher: f2.as_ref(),
                offline: false,
                mirror_github: None,
                lock_timeout: Duration::from_secs(60),
            };
            ensure_installed_with(&s2, &ctx)
        });
        let a = t1.join().unwrap().unwrap();
        let b = t2.join().unwrap().unwrap();
        assert_eq!(a, b);
        // At most two fetches if both raced before marker; usually one.
        assert!(fetcher.hits.lock().unwrap().len() <= 2);
    }
}
