//! GitHub Releases backend for the Simplified Chinese community build.
//!
//! The repository, API endpoint, asset naming, and accepted platforms are
//! compile-time policy.  Community builds never consult the upstream npm,
//! GitHub, x.ai, or GCS update sources.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub(crate) const COMMUNITY_INSTALLER: &str = "community-github";

const API_VERSION: &str = "2026-03-10";
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 4 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 128;
const MAX_UNCOMPRESSED_BYTES: u64 = 768 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_TAR_ZERO_PADDING_BYTES: u64 = 1024 * 1024;
const DOWNLOAD_PROGRESS_TEMPLATE: &str =
    "  下载更新 {bar:30.cyan/dim} {bytes}/{total_bytes} {percent}% ({bytes_per_sec}，剩余 {eta})";
const ONE_CLICK_INSTALLER: &str = "一键安装.cmd";
const COMMAND_SETUP_INSTALLER: &str = "[可选]替换原始启动方式.cmd";
const WINDOWS_REQUIRED_PACKAGE_FILES: [&str; 15] = [
    "grok-zh.exe",
    "agent-zh.cmd",
    "rg.exe",
    ONE_CLICK_INSTALLER,
    COMMAND_SETUP_INSTALLER,
    "Install-GrokZh.ps1",
    "INSTALL-WINDOWS.md",
    "LICENSE-grok-build.txt",
    "BUILD-INFO.txt",
    "licenses/ripgrep/COPYING",
    "licenses/ripgrep/LICENSE-MIT",
    "licenses/ripgrep/UNLICENSE",
    "licenses/project/THIRD-PARTY-NOTICES",
    "licenses/project/THIRD_PARTY_NOTICES.md",
    "licenses/project/NOTICE",
];
// v1.0.5 was published with the current 15-file ZIP layout, but its inner
// manifest predates the license/build-metadata expansion and contains only
// these seven executable/installer files. The outer asset digest still covers
// the complete ZIP. Keep this compatibility profile pinned to that exact
// trusted Release version instead of accepting arbitrary partial manifests.
const WINDOWS_V1_0_5_MANIFEST_FILES: [&str; 7] = [
    "grok-zh.exe",
    "agent-zh.cmd",
    "rg.exe",
    ONE_CLICK_INSTALLER,
    COMMAND_SETUP_INSTALLER,
    "Install-GrokZh.ps1",
    "INSTALL-WINDOWS.md",
];
const WINDOWS_APPROVED_PACKAGE_DIRS: [&str; 3] =
    ["licenses", "licenses/ripgrep", "licenses/project"];
const MACOS_REQUIRED_PACKAGE_FILES: [&str; 9] = [
    "grok-zh",
    "BUILD-INFO.txt",
    "INSTALL-MACOS.md",
    "Install-GrokZh.sh",
    "LICENSE-grok-build.txt",
    "NOTICE-third-party.txt",
    "SOURCE_REV",
    "THIRD-PARTY-NOTICES.txt",
    "THIRD-PARTY-NOTICES-xai-grok-tools.md",
];
const LINUX_REQUIRED_PACKAGE_FILES: [&str; 9] = [
    "grok-zh",
    "BUILD-INFO.txt",
    "INSTALL-LINUX.md",
    "Install-GrokZh.sh",
    "LICENSE-grok-build.txt",
    "NOTICE-third-party.txt",
    "SOURCE_REV",
    "THIRD-PARTY-NOTICES.txt",
    "THIRD-PARTY-NOTICES-xai-grok-tools.md",
];
const INNER_MANIFEST: &str = "SHA256SUMS.txt";

fn is_allowed_unicode_package_name(name: &str) -> bool {
    name == ONE_CLICK_INSTALLER || name == COMMAND_SETUP_INSTALLER
}

fn release_repo() -> &'static str {
    xai_grok_product::COMMUNITY_RELEASE_REPO
}

fn releases_api(page: usize) -> String {
    format!(
        "https://api.github.com/repos/{}/releases?per_page=100&page={page}",
        release_repo()
    )
}

fn release_by_tag_api(version: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/releases/tags/v{version}",
        release_repo()
    )
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    immutable: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    state: String,
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedAsset {
    pub version: String,
    pub name: String,
    download_url: String,
    size: u64,
    sha256: String,
    archive_kind: CommunityArchiveKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommunityArchiveKind {
    WindowsZip,
    MacosTarGz,
    LinuxTarGz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommunityPlatform {
    WindowsX86_64Gnu,
    MacosAarch64,
    LinuxX86_64Gnu,
}

fn current_community_platform() -> Result<CommunityPlatform> {
    if cfg!(all(
        target_os = "windows",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Ok(CommunityPlatform::WindowsX86_64Gnu)
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok(CommunityPlatform::MacosAarch64)
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Ok(CommunityPlatform::LinuxX86_64Gnu)
    } else {
        anyhow::bail!(
            "community self-update supports only x86_64-pc-windows-gnu, aarch64-apple-darwin, and x86_64-unknown-linux-gnu"
        )
    }
}

fn github_client(request_timeout: Duration) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("grok-build-zh-updater"),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(API_VERSION),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .timeout(request_timeout)
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !allowed_github_url(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context("building the community GitHub Releases client")
}

fn api_client() -> Result<reqwest::Client> {
    github_client(Duration::from_secs(30))
}

fn asset_client() -> Result<reqwest::Client> {
    github_client(Duration::from_secs(20 * 60))
}

fn parse_release_version(release: &ApiRelease) -> Option<Version> {
    if release.draft || !release.immutable {
        return None;
    }
    let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
    if !version.build.is_empty() || release.prerelease != !version.pre.is_empty() {
        return None;
    }
    Some(version)
}

fn select_latest_release<'a>(
    releases: &'a [ApiRelease],
    channel: &str,
) -> Result<(&'a ApiRelease, Version)> {
    if !matches!(channel, "stable" | "alpha") {
        anyhow::bail!("unsupported community release channel: {channel}");
    }
    releases
        .iter()
        .filter_map(|release| {
            let version = parse_release_version(release)?;
            if channel == "stable" && !version.pre.is_empty() {
                return None;
            }
            Some((release, version))
        })
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no immutable {channel} release is available in {}",
                release_repo()
            )
        })
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let response = api_client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GitHub Releases API returned {status} for {url}");
    }
    response
        .json::<T>()
        .await
        .with_context(|| format!("parsing GitHub Releases response from {url}"))
}

pub(crate) async fn fetch_latest_version(channel: &str) -> Result<String> {
    crate::ensure_community_updates_enabled()?;
    let mut releases = Vec::new();
    for page in 1.. {
        let page_releases: Vec<ApiRelease> = fetch_json(&releases_api(page)).await?;
        let page_len = page_releases.len();
        releases.extend(page_releases);
        if page_len < 100 {
            break;
        }
    }
    let platform = current_community_platform()?;
    let (_, version) = select_latest_compatible_release(&releases, channel, platform)?;
    Ok(version.to_string())
}

fn canonical_release_version(version: &str) -> Result<Version> {
    let parsed = Version::parse(version)
        .with_context(|| format!("invalid community release version: {version}"))?;
    if parsed.to_string() != version || !parsed.build.is_empty() {
        anyhow::bail!("community release version is not canonical: {version}");
    }
    Ok(parsed)
}

fn release_asset_name_for(platform: CommunityPlatform, version: &str) -> Result<String> {
    canonical_release_version(version)?;
    match platform {
        CommunityPlatform::WindowsX86_64Gnu => {
            Ok(format!("grok-zh-{version}-windows-x86_64-gnu.zip"))
        }
        CommunityPlatform::MacosAarch64 => Ok(format!("grok-zh-{version}-macos-aarch64.tar.gz")),
        CommunityPlatform::LinuxX86_64Gnu => {
            Ok(format!("grok-zh-{version}-linux-x86_64-gnu.tar.gz"))
        }
    }
}

fn release_asset_name(version: &str) -> Result<String> {
    release_asset_name_for(current_community_platform()?, version)
}

fn release_includes_macos_assets(version: &Version) -> bool {
    (version.major, version.minor, version.patch) >= (1, 0, 7)
}

/// Keep v1.0.8 stable on the four-asset Windows/macOS bridge contract so the
/// published v1.0.5 updater can consume it. Linux first ships in the v1.0.8
/// release-candidate line; every stable release from v1.0.9 onward uses the
/// six-asset contract understood by the bridge client.
fn release_includes_linux_assets(version: &Version) -> bool {
    let base = (version.major, version.minor, version.patch);
    base >= (1, 0, 9) || (base == (1, 0, 8) && !version.pre.is_empty())
}

fn expected_release_asset_names(version: &str) -> Result<Vec<String>> {
    let parsed = canonical_release_version(version)?;
    let windows = release_asset_name_for(CommunityPlatform::WindowsX86_64Gnu, version)?;
    let mut names = vec![windows.clone(), format!("{windows}.sha256")];
    if release_includes_macos_assets(&parsed) {
        let macos = release_asset_name_for(CommunityPlatform::MacosAarch64, version)?;
        names.push(macos.clone());
        names.push(format!("{macos}.sha256"));
    }
    if release_includes_linux_assets(&parsed) {
        let linux = release_asset_name_for(CommunityPlatform::LinuxX86_64Gnu, version)?;
        names.push(linux.clone());
        names.push(format!("{linux}.sha256"));
    }
    names.sort_unstable();
    Ok(names)
}

fn parse_sha256_digest(value: &str) -> Result<String> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("release asset digest is not SHA-256"))?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("release asset contains an invalid SHA-256 digest");
    }
    Ok(digest.to_ascii_lowercase())
}

fn select_asset_for_platform(
    release: &ApiRelease,
    version: &str,
    platform: CommunityPlatform,
) -> Result<VerifiedAsset> {
    let parsed = parse_release_version(release)
        .ok_or_else(|| anyhow::anyhow!("release is mutable, draft, or has invalid metadata"))?;
    if parsed.to_string() != version || release.tag_name != format!("v{version}") {
        anyhow::bail!("release tag and requested version do not match");
    }
    let name = release_asset_name_for(platform, version)?;
    if platform == CommunityPlatform::MacosAarch64 && !release_includes_macos_assets(&parsed) {
        anyhow::bail!("release {version} predates macOS community self-update support");
    }
    if platform == CommunityPlatform::LinuxX86_64Gnu && !release_includes_linux_assets(&parsed) {
        anyhow::bail!("release {version} predates Linux community self-update support");
    }
    let sidecar_name = format!("{name}.sha256");
    let mut actual_names: Vec<&str> = release
        .assets
        .iter()
        .filter(|asset| asset.state == "uploaded")
        .map(|asset| asset.name.as_str())
        .collect();
    actual_names.sort_unstable();
    let expected_names = expected_release_asset_names(version)?;
    let expected_name_refs: Vec<&str> = expected_names.iter().map(String::as_str).collect();
    if release.assets.len() != expected_names.len() || actual_names != expected_name_refs {
        anyhow::bail!("release assets do not match the exact approved platform asset set");
    }
    for expected_name in &expected_names {
        let expected = release
            .assets
            .iter()
            .find(|asset| asset.name == *expected_name)
            .ok_or_else(|| anyhow::anyhow!("release is missing {expected_name}"))?;
        let is_sidecar = expected_name.ends_with(".sha256");
        let max_size = if is_sidecar {
            MAX_SIDECAR_BYTES
        } else {
            MAX_ASSET_BYTES
        };
        if expected.size == 0 || expected.size > max_size {
            anyhow::bail!("release asset size is outside the accepted range: {expected_name}");
        }
        let expected_url = format!(
            "https://github.com/{}/releases/download/v{version}/{expected_name}",
            release_repo()
        );
        if expected.browser_download_url != expected_url {
            anyhow::bail!("release asset URL does not match the fixed community repository");
        }
        let digest = expected.digest.as_deref().ok_or_else(|| {
            anyhow::anyhow!("release asset is missing its GitHub SHA-256 digest: {expected_name}")
        })?;
        parse_sha256_digest(digest)?;
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| anyhow::anyhow!("release is missing {name}"))?;
    let sidecar = release
        .assets
        .iter()
        .find(|asset| asset.name == sidecar_name)
        .ok_or_else(|| anyhow::anyhow!("release is missing {sidecar_name}"))?;
    let expected_url = format!(
        "https://github.com/{}/releases/download/v{version}/{name}",
        release_repo()
    );
    if asset.browser_download_url != expected_url {
        anyhow::bail!("release asset URL does not match the fixed community repository");
    }
    let expected_sidecar_url = format!(
        "https://github.com/{}/releases/download/v{version}/{sidecar_name}",
        release_repo()
    );
    if sidecar.browser_download_url != expected_sidecar_url {
        anyhow::bail!("release checksum URL does not match the fixed community repository");
    }
    let sidecar_digest = sidecar
        .digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("release checksum is missing its GitHub SHA-256 digest"))?;
    parse_sha256_digest(sidecar_digest)?;
    let digest = asset
        .digest
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("release asset is missing its GitHub SHA-256 digest"))?;
    Ok(VerifiedAsset {
        version: version.to_string(),
        name,
        download_url: expected_url,
        size: asset.size,
        sha256: parse_sha256_digest(digest)?,
        archive_kind: match platform {
            CommunityPlatform::WindowsX86_64Gnu => CommunityArchiveKind::WindowsZip,
            CommunityPlatform::MacosAarch64 => CommunityArchiveKind::MacosTarGz,
            CommunityPlatform::LinuxX86_64Gnu => CommunityArchiveKind::LinuxTarGz,
        },
    })
}

fn select_asset(release: &ApiRelease, version: &str) -> Result<VerifiedAsset> {
    select_asset_for_platform(release, version, current_community_platform()?)
}

fn select_latest_compatible_release<'a>(
    releases: &'a [ApiRelease],
    channel: &str,
    platform: CommunityPlatform,
) -> Result<(&'a ApiRelease, Version)> {
    let mut compatible = releases
        .iter()
        .filter_map(|release| {
            let version = parse_release_version(release)?;
            if channel == "stable" && !version.pre.is_empty() {
                return None;
            }
            select_asset_for_platform(release, &version.to_string(), platform)
                .ok()
                .map(|_| (release, version))
        })
        .collect::<Vec<_>>();
    if !matches!(channel, "stable" | "alpha") {
        anyhow::bail!("unsupported community release channel: {channel}");
    }
    compatible.sort_by(|(_, a), (_, b)| a.cmp(b));
    compatible.pop().ok_or_else(|| {
        anyhow::anyhow!(
            "no immutable {channel} release compatible with this platform is available in {}",
            release_repo()
        )
    })
}

pub(crate) async fn fetch_asset(version: &str) -> Result<VerifiedAsset> {
    crate::ensure_community_updates_enabled()?;
    let parsed = Version::parse(version)
        .with_context(|| format!("invalid community release version: {version}"))?;
    if !parsed.build.is_empty() {
        anyhow::bail!("community release version must not contain build metadata: {version}");
    }
    if parsed.to_string() != version {
        anyhow::bail!("community release version is not canonical: {version}");
    }
    let url = release_by_tag_api(version);
    let release: ApiRelease = fetch_json(&url).await?;
    select_asset(&release, version)
}

fn allowed_github_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

pub(crate) async fn download_verified(asset: &VerifiedAsset, destination: &Path) -> Result<()> {
    crate::ensure_community_updates_enabled()?;
    let progress = ProgressBar::new(asset.size);
    progress.set_style(
        ProgressStyle::default_bar()
            .template(DOWNLOAD_PROGRESS_TEMPLATE)
            .expect("valid community download progress template"),
    );
    progress.set_position(0);
    let mut created_destination = false;
    let result = async {
        let response = asset_client()?
            .get(&asset.download_url)
            .send()
            .await
            .with_context(|| format!("downloading {}", asset.name))?;
        if !response.status().is_success() {
            anyhow::bail!(
                "GitHub release asset download returned {}",
                response.status()
            );
        }
        if !allowed_github_url(response.url()) {
            anyhow::bail!("GitHub release asset redirected to an untrusted host");
        }
        if let Some(length) = response.content_length()
            && length != asset.size
        {
            anyhow::bail!("GitHub release asset Content-Length does not match its metadata");
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .await
            .with_context(|| format!("creating {}", destination.display()))?;
        created_destination = true;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading GitHub release asset")?;
            written = written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("release asset size overflow"))?;
            if written > asset.size || written > MAX_ASSET_BYTES {
                anyhow::bail!("release asset exceeded its declared size");
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            progress.set_position(written);
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        if written != asset.size {
            anyhow::bail!("release asset was truncated");
        }
        let actual = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual != asset.sha256 {
            anyhow::bail!("release asset SHA-256 does not match GitHub metadata");
        }
        Ok(())
    }
    .await;
    finish_download_progress(&progress, result.is_ok());
    if result.is_err() && created_destination {
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

fn finish_download_progress(progress: &ProgressBar, succeeded: bool) {
    if succeeded {
        progress.finish();
    } else {
        progress.finish_and_clear();
    }
}

fn normalized_package_name(name: &str) -> String {
    if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

fn is_safe_package_relative_path(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains(':')
        && name
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn expected_archive_names(required_files: &[&str]) -> HashSet<String> {
    required_files
        .iter()
        .copied()
        .chain(std::iter::once(INNER_MANIFEST))
        .map(normalized_package_name)
        .collect()
}

fn validate_archive_layout(
    archive: &mut zip::ZipArchive<File>,
    required_files: &[&str],
) -> Result<()> {
    if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
        anyhow::bail!("community release ZIP contains an invalid number of entries");
    }

    let expected_files = expected_archive_names(required_files);
    let approved_dirs: HashSet<String> = WINDOWS_APPROVED_PACKAGE_DIRS
        .iter()
        .copied()
        .map(normalized_package_name)
        .collect();
    let mut seen_files = HashSet::new();
    let mut seen_dirs = HashSet::new();
    let mut total_size = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .with_context(|| format!("reading ZIP entry {index}"))?;
        let raw_name = entry.name().to_string();
        let raw_without_directory_suffix = raw_name.strip_suffix('/').unwrap_or(&raw_name);
        if raw_name.contains('\\')
            || raw_name.contains(':')
            || raw_without_directory_suffix.is_empty()
            || raw_without_directory_suffix.starts_with('/')
            || raw_without_directory_suffix
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            anyhow::bail!("community release ZIP contains an unsafe raw path: {raw_name}");
        }
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("unsafe ZIP entry path: {raw_name}"))?;
        if entry.is_symlink() || (!entry.is_file() && !entry.is_dir()) {
            anyhow::bail!("community release ZIP contains a non-regular entry: {raw_name}");
        }
        if entry.size() > MAX_ENTRY_BYTES {
            anyhow::bail!("community release ZIP entry is too large: {raw_name}");
        }
        total_size = total_size
            .checked_add(entry.size())
            .ok_or_else(|| anyhow::anyhow!("community release ZIP size overflow"))?;
        if total_size > MAX_UNCOMPRESSED_BYTES {
            anyhow::bail!("community release ZIP exceeds the uncompressed size limit");
        }

        let enclosed_text = enclosed.to_string_lossy();
        let enclosed_name = enclosed_text.strip_suffix('/').unwrap_or(&enclosed_text);
        if !enclosed_name.is_ascii() && !is_allowed_unicode_package_name(enclosed_name) {
            anyhow::bail!(
                "community release ZIP entry name contains unapproved Unicode: {raw_name}"
            );
        }
        let normalized = normalized_package_name(&enclosed_name.replace('\\', "/"));
        let seen = if entry.is_dir() {
            if !approved_dirs.contains(&normalized) {
                anyhow::bail!("community release ZIP contains an extra directory: {raw_name}");
            }
            &mut seen_dirs
        } else {
            if !expected_files.contains(&normalized) {
                anyhow::bail!("community release ZIP contains an extra file: {raw_name}");
            }
            &mut seen_files
        };
        if !seen.insert(normalized) {
            anyhow::bail!("community release ZIP contains a duplicate path: {raw_name}");
        }
        if enclosed
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
        {
            anyhow::bail!("community release ZIP must not contain a nested ZIP: {raw_name}");
        }
    }
    if seen_files != expected_files {
        anyhow::bail!("community release ZIP does not contain the exact approved package files");
    }
    Ok(())
}

fn parse_inner_manifest(bytes: &[u8], required_files: &[&str]) -> Result<HashMap<String, String>> {
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        anyhow::bail!("community release SHA256SUMS.txt exceeds the size limit");
    }
    let text =
        std::str::from_utf8(bytes).context("community release SHA256SUMS.txt is not UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut hashes = HashMap::new();
    let mut normalized_names = HashSet::new();
    for (line_index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            anyhow::bail!(
                "community release SHA256SUMS.txt contains an empty line at {}",
                line_index + 1
            );
        }
        let (digest, name) = line.split_once("  ").ok_or_else(|| {
            anyhow::anyhow!(
                "community release SHA256SUMS.txt line {} has an invalid format",
                line_index + 1
            )
        })?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!(
                "community release SHA256SUMS.txt line {} has an invalid digest",
                line_index + 1
            );
        }
        if !is_safe_package_relative_path(name)
            || !required_files.contains(&name)
            || (!name.is_ascii() && !is_allowed_unicode_package_name(name))
        {
            anyhow::bail!(
                "community release SHA256SUMS.txt line {} has an unsafe filename",
                line_index + 1
            );
        }
        if !normalized_names.insert(normalized_package_name(name)) {
            anyhow::bail!("community release SHA256SUMS.txt contains a duplicate filename");
        }
        hashes.insert(name.to_string(), digest.to_ascii_lowercase());
    }

    if hashes.len() != required_files.len() {
        anyhow::bail!("community release manifest does not contain the exact approved file set");
    }
    for required in required_files {
        if !hashes.contains_key(*required) {
            anyhow::bail!("community release manifest is missing required file {required}");
        }
    }
    Ok(hashes)
}

fn read_inner_manifest(
    archive: &mut zip::ZipArchive<File>,
    required_files: &[&str],
) -> Result<HashMap<String, String>> {
    let entry = archive
        .by_name(INNER_MANIFEST)
        .context("community release ZIP is missing SHA256SUMS.txt")?;
    if entry.is_symlink() || !entry.is_file() || entry.size() > MAX_MANIFEST_BYTES {
        anyhow::bail!("community release SHA256SUMS.txt is not a bounded regular file");
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("reading community release SHA256SUMS.txt")?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        anyhow::bail!("community release SHA256SUMS.txt exceeds the size limit");
    }
    parse_inner_manifest(&bytes, required_files)
}

fn hash_manifest_entry(
    archive: &mut zip::ZipArchive<File>,
    name: &str,
    destination: Option<&Path>,
) -> Result<String> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("community release ZIP is missing manifest entry {name}"))?;
    if entry.is_symlink() || !entry.is_file() || entry.size() > MAX_ENTRY_BYTES {
        anyhow::bail!("community release manifest entry is not a bounded regular file: {name}");
    }

    let mut output = match destination {
        Some(path) => Some(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .with_context(|| format!("creating extracted candidate {}", path.display()))?,
        ),
        None => None,
    };
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = entry
            .read(&mut buffer)
            .with_context(|| format!("reading {name} from community release ZIP"))?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("community release entry size overflow"))?;
        if copied > entry.size() || copied > MAX_ENTRY_BYTES {
            anyhow::bail!("community release ZIP entry exceeded its declared size: {name}");
        }
        hasher.update(&buffer[..read]);
        if let Some(output) = output.as_mut() {
            output
                .write_all(&buffer[..read])
                .with_context(|| format!("writing extracted candidate {name}"))?;
        }
    }
    if copied != entry.size() {
        anyhow::bail!("community release ZIP entry was truncated: {name}");
    }
    if let Some(mut output) = output {
        output.flush()?;
        output.sync_all()?;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Validate the complete ZIP package and extract only its verified executable
/// to a new sibling file. Companion files remain managed by the full Windows
/// installer; they are nevertheless required by the exact archive layout and
/// covered by the verified outer asset digest. Every file declared by the
/// version-pinned inner manifest is hashed again here before activation.
fn extract_verified_windows_executable(
    asset: &VerifiedAsset,
    archive_path: &Path,
    destination: &Path,
) -> Result<()> {
    if std::fs::symlink_metadata(destination).is_ok() {
        anyhow::bail!(
            "refusing to overwrite an existing extraction target: {}",
            destination.display()
        );
    }
    let result = (|| {
        let archive_file = File::open(archive_path)
            .with_context(|| format!("opening community release ZIP {}", archive_path.display()))?;
        let mut archive = zip::ZipArchive::new(archive_file)
            .context("opening the downloaded community release as ZIP")?;
        validate_archive_layout(&mut archive, &WINDOWS_REQUIRED_PACKAGE_FILES)?;
        let manifest_files: &[&str] = if asset.version == "1.0.5" {
            &WINDOWS_V1_0_5_MANIFEST_FILES
        } else {
            &WINDOWS_REQUIRED_PACKAGE_FILES
        };
        let hashes = read_inner_manifest(&mut archive, manifest_files)?;
        for (name, expected) in hashes {
            let extracted = (name == "grok-zh.exe").then_some(destination);
            let actual = hash_manifest_entry(&mut archive, &name, extracted)?;
            if actual != expected {
                anyhow::bail!("community release inner SHA-256 mismatch for {name}");
            }
        }
        if !destination.is_file() {
            anyhow::bail!("community release did not produce grok-zh.exe");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

fn normalized_unix_tar_name(raw: &[u8]) -> Result<Option<&str>> {
    if !raw.is_ascii() {
        anyhow::bail!("community release tar path is not ASCII");
    }
    let raw = std::str::from_utf8(raw).context("community release tar path is not UTF-8")?;
    if raw == "." || raw == "./" {
        return Ok(None);
    }
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.bytes().any(|byte| byte.is_ascii_control())
    {
        anyhow::bail!("community release tar contains an unsafe raw path: {raw}");
    }
    let name = raw.strip_prefix("./").unwrap_or(raw);
    if name.is_empty() || name.contains('/') || matches!(name, "." | "..") || name.starts_with("./")
    {
        anyhow::bail!("community release tar contains a nested or unsafe path: {raw}");
    }
    Ok(Some(name))
}

fn hash_tar_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    name: &str,
    destination: Option<&Path>,
) -> Result<String> {
    let declared_size = entry.size();
    if declared_size > MAX_ENTRY_BYTES {
        anyhow::bail!("community release tar entry is too large: {name}");
    }
    let mut output = match destination {
        Some(path) => {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            Some(
                options
                    .open(path)
                    .with_context(|| format!("creating extracted candidate {}", path.display()))?,
            )
        }
        None => None,
    };
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = entry
            .read(&mut buffer)
            .with_context(|| format!("reading {name} from community release tar"))?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("community release tar entry size overflow"))?;
        if copied > declared_size || copied > MAX_ENTRY_BYTES {
            anyhow::bail!("community release tar entry exceeded its declared size: {name}");
        }
        hasher.update(&buffer[..read]);
        if let Some(output) = output.as_mut() {
            output
                .write_all(&buffer[..read])
                .with_context(|| format!("writing extracted candidate {name}"))?;
        }
    }
    if copied != declared_size {
        anyhow::bail!("community release tar entry was truncated: {name}");
    }
    if let Some(mut output) = output {
        output.flush()?;
        output.sync_all()?;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn extract_verified_unix_executable(
    archive_path: &Path,
    destination: &Path,
    required_files: &[&str],
    executable_files: &[&str],
) -> Result<()> {
    if std::fs::symlink_metadata(destination).is_ok() {
        anyhow::bail!(
            "refusing to overwrite an existing extraction target: {}",
            destination.display()
        );
    }
    let result = (|| {
        let archive_file = File::open(archive_path).with_context(|| {
            format!(
                "opening community release tar.gz {}",
                archive_path.display()
            )
        })?;
        // The bufread single-member decoder stops exactly at the first gzip
        // member. After validating the tar stream we inspect its underlying
        // reader, so even an empty concatenated member is rejected.
        let decoder = flate2::bufread::GzDecoder::new(BufReader::new(archive_file));
        let mut archive = tar::Archive::new(decoder);
        let mut entries = archive
            .entries()
            .context("opening the downloaded community release as tar")?
            .raw(true);
        let expected = expected_archive_names(required_files);
        let mut seen = HashSet::new();
        let mut hashes = HashMap::new();
        let mut manifest = None;
        let mut root_seen = false;
        let mut count = 0usize;
        let mut total_size = 0u64;

        for entry_result in &mut entries {
            count = count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("community release tar entry count overflow"))?;
            if count > MAX_ARCHIVE_ENTRIES {
                anyhow::bail!("community release tar contains too many entries");
            }
            let mut entry = entry_result.context("reading community release tar entry")?;
            if entry.header().as_ustar().is_none() {
                anyhow::bail!("community release tar must use the USTAR format");
            }
            let raw_path = entry.header().path_bytes();
            let name = normalized_unix_tar_name(raw_path.as_ref())?;
            let entry_type = entry.header().entry_type();
            if name.is_none() {
                if root_seen || !entry_type.is_dir() || entry.size() != 0 {
                    anyhow::bail!("community release tar contains an invalid root entry");
                }
                root_seen = true;
                continue;
            }
            let name = name.expect("checked above").to_string();
            if !entry_type.is_file() || entry.link_name_bytes().is_some() {
                anyhow::bail!("community release tar contains a non-regular entry: {name}");
            }
            if entry.size() > MAX_ENTRY_BYTES {
                anyhow::bail!("community release tar entry is too large: {name}");
            }
            total_size = total_size
                .checked_add(entry.size())
                .ok_or_else(|| anyhow::anyhow!("community release tar size overflow"))?;
            if total_size > MAX_UNCOMPRESSED_BYTES {
                anyhow::bail!("community release tar exceeds the uncompressed size limit");
            }
            let mode = entry
                .header()
                .mode()
                .with_context(|| format!("reading mode for tar entry {name}"))?;
            let expected_mode = if executable_files.contains(&name.as_str()) {
                0o755
            } else {
                0o644
            };
            if mode & 0o7777 != expected_mode {
                anyhow::bail!("community release tar entry has an unexpected mode: {name}");
            }
            let normalized = normalized_package_name(&name);
            if !expected.contains(&normalized) || !seen.insert(normalized) {
                anyhow::bail!("community release tar contains an extra or duplicate path: {name}");
            }
            if name == INNER_MANIFEST {
                let manifest_size = entry.size();
                if manifest_size > MAX_MANIFEST_BYTES {
                    anyhow::bail!("community release SHA256SUMS.txt exceeds the size limit");
                }
                let mut bytes = Vec::with_capacity(manifest_size as usize);
                entry
                    .take(MAX_MANIFEST_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .context("reading community release SHA256SUMS.txt")?;
                if bytes.len() as u64 != manifest_size {
                    anyhow::bail!("community release SHA256SUMS.txt was truncated");
                }
                manifest = Some(bytes);
            } else {
                let extracted = (name == "grok-zh").then_some(destination);
                hashes.insert(name.clone(), hash_tar_entry(&mut entry, &name, extracted)?);
            }
        }
        drop(entries);
        let mut decoder = archive.into_inner();
        // `tar::Archive::entries` consumes the first zero header and stops. A
        // valid tar must contain a second 512-byte zero header; the remainder
        // may only be bounded zero record padding.
        let mut decoded_tail = [0u8; 8 * 1024];
        let mut padding_bytes = 0u64;
        loop {
            let read = decoder
                .read(&mut decoded_tail)
                .context("validating the end of the community release gzip stream")?;
            if read == 0 {
                break;
            }
            padding_bytes = padding_bytes
                .checked_add(read as u64)
                .ok_or_else(|| anyhow::anyhow!("community release tar padding overflow"))?;
            if padding_bytes > MAX_TAR_ZERO_PADDING_BYTES
                || decoded_tail[..read].iter().any(|byte| *byte != 0)
            {
                anyhow::bail!("community release tar.gz contains trailing decoded data");
            }
        }
        if padding_bytes < 512 {
            anyhow::bail!("community release tar is missing its complete end marker");
        }
        let mut compressed_reader = decoder.into_inner();
        let mut compressed_tail = [0u8; 1];
        if compressed_reader
            .read(&mut compressed_tail)
            .context("validating the end of the community release gzip member")?
            != 0
        {
            anyhow::bail!("community release tar.gz contains a concatenated or trailing stream");
        }
        if !root_seen {
            anyhow::bail!("community release tar is missing its root directory entry");
        }
        if seen != expected {
            anyhow::bail!(
                "community release tar does not contain the exact approved package files"
            );
        }
        let manifest = parse_inner_manifest(
            manifest.as_deref().ok_or_else(|| {
                anyhow::anyhow!("community release tar is missing SHA256SUMS.txt")
            })?,
            required_files,
        )?;
        for (name, expected_hash) in manifest {
            let actual = hashes
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("community release tar is missing {name}"))?;
            if actual != &expected_hash {
                anyhow::bail!("community release inner SHA-256 mismatch for {name}");
            }
        }
        if !destination.is_file() {
            anyhow::bail!("community release did not produce grok-zh");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(unix)]
        File::open(destination)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

fn extract_verified_macos_executable(archive_path: &Path, destination: &Path) -> Result<()> {
    extract_verified_unix_executable(
        archive_path,
        destination,
        &MACOS_REQUIRED_PACKAGE_FILES,
        &["grok-zh", "Install-GrokZh.sh"],
    )
}

fn extract_verified_linux_executable(archive_path: &Path, destination: &Path) -> Result<()> {
    extract_verified_unix_executable(
        archive_path,
        destination,
        &LINUX_REQUIRED_PACKAGE_FILES,
        &["grok-zh", "Install-GrokZh.sh"],
    )
}

pub(crate) fn extract_verified_executable(
    asset: &VerifiedAsset,
    archive_path: &Path,
    destination: &Path,
) -> Result<()> {
    match asset.archive_kind {
        CommunityArchiveKind::WindowsZip => {
            extract_verified_windows_executable(asset, archive_path, destination)
        }
        CommunityArchiveKind::MacosTarGz => {
            extract_verified_macos_executable(archive_path, destination)
        }
        CommunityArchiveKind::LinuxTarGz => {
            extract_verified_linux_executable(archive_path, destination)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[derive(Clone, Debug, Default)]
    struct RecordingTerm {
        contents: std::sync::Arc<std::sync::Mutex<String>>,
    }

    impl RecordingTerm {
        fn contents(&self) -> String {
            self.contents.lock().unwrap().clone()
        }
    }

    impl indicatif::TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            120
        }

        fn move_cursor_up(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn write_line(&self, line: &str) -> std::io::Result<()> {
            let mut contents = self.contents.lock().unwrap();
            contents.push_str(line);
            contents.push('\n');
            Ok(())
        }

        fn write_str(&self, value: &str) -> std::io::Result<()> {
            self.contents.lock().unwrap().push_str(value);
            Ok(())
        }

        fn clear_line(&self) -> std::io::Result<()> {
            self.contents.lock().unwrap().clear();
            Ok(())
        }

        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn release(tag: &str, prerelease: bool, immutable: bool) -> ApiRelease {
        ApiRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease,
            immutable,
            assets: Vec::new(),
        }
    }

    fn uploaded_asset(version: &str, name: String, size: u64) -> ApiAsset {
        ApiAsset {
            browser_download_url: format!(
                "https://github.com/{}/releases/download/v{version}/{name}",
                release_repo()
            ),
            name,
            size,
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
            state: "uploaded".to_string(),
        }
    }

    fn uploaded_package_assets(version: &str) -> Vec<ApiAsset> {
        expected_release_asset_names(version)
            .unwrap()
            .into_iter()
            .map(|name| {
                let size = if name.ends_with(".sha256") { 112 } else { 123 };
                uploaded_asset(version, name, size)
            })
            .collect()
    }

    fn verified_windows_asset(version: &str) -> VerifiedAsset {
        let name = format!("grok-zh-{version}-windows-x86_64-gnu.zip");
        VerifiedAsset {
            version: version.to_string(),
            download_url: format!(
                "https://github.com/{}/releases/download/v{version}/{name}",
                release_repo()
            ),
            name,
            size: 1,
            sha256: "ab".repeat(32),
            archive_kind: CommunityArchiveKind::WindowsZip,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn package_entries_with_manifest(manifest_files: &[&str]) -> Vec<(String, Vec<u8>)> {
        let mut entries = vec![
            ("grok-zh.exe".to_string(), b"verified executable".to_vec()),
            ("agent-zh.cmd".to_string(), b"agent wrapper".to_vec()),
            ("rg.exe".to_string(), b"ripgrep".to_vec()),
            (
                ONE_CLICK_INSTALLER.to_string(),
                b"one-click installer".to_vec(),
            ),
            (
                COMMAND_SETUP_INSTALLER.to_string(),
                b"optional command setup".to_vec(),
            ),
            ("Install-GrokZh.ps1".to_string(), b"installer".to_vec()),
            (
                "INSTALL-WINDOWS.md".to_string(),
                b"installation guide".to_vec(),
            ),
            (
                "LICENSE-grok-build.txt".to_string(),
                b"project license".to_vec(),
            ),
            ("BUILD-INFO.txt".to_string(), b"build metadata".to_vec()),
            (
                "licenses/ripgrep/COPYING".to_string(),
                b"ripgrep copying".to_vec(),
            ),
            (
                "licenses/ripgrep/LICENSE-MIT".to_string(),
                b"ripgrep MIT license".to_vec(),
            ),
            (
                "licenses/ripgrep/UNLICENSE".to_string(),
                b"ripgrep unlicense".to_vec(),
            ),
            (
                "licenses/project/THIRD-PARTY-NOTICES".to_string(),
                b"project notices".to_vec(),
            ),
            (
                "licenses/project/THIRD_PARTY_NOTICES.md".to_string(),
                b"tool notices".to_vec(),
            ),
            (
                "licenses/project/NOTICE".to_string(),
                b"third party notice".to_vec(),
            ),
        ];
        let manifest = entries
            .iter()
            .filter(|(name, _)| manifest_files.contains(&name.as_str()))
            .map(|(name, bytes)| format!("{}  {name}", sha256_hex(bytes)))
            .collect::<Vec<_>>()
            .join("\n");
        entries.push(("SHA256SUMS.txt".to_string(), manifest.into_bytes()));
        entries
    }

    fn package_entries() -> Vec<(String, Vec<u8>)> {
        package_entries_with_manifest(&WINDOWS_REQUIRED_PACKAGE_FILES)
    }

    fn extract_current_windows_executable(archive_path: &Path, destination: &Path) -> Result<()> {
        extract_verified_windows_executable(
            &verified_windows_asset("1.0.6"),
            archive_path,
            destination,
        )
    }

    fn write_zip(path: &Path, entries: &[(String, Vec<u8>)]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn macos_package_entries() -> Vec<(String, Vec<u8>, u32)> {
        let mut entries = vec![
            ("grok-zh".to_string(), b"verified Mach-O".to_vec(), 0o755),
            (
                "BUILD-INFO.txt".to_string(),
                b"unsigned macOS ARM64 build".to_vec(),
                0o644,
            ),
            (
                "INSTALL-MACOS.md".to_string(),
                b"installation guide".to_vec(),
                0o644,
            ),
            (
                "Install-GrokZh.sh".to_string(),
                b"#!/bin/sh\nexit 0\n".to_vec(),
                0o755,
            ),
            (
                "LICENSE-grok-build.txt".to_string(),
                b"license".to_vec(),
                0o644,
            ),
            (
                "NOTICE-third-party.txt".to_string(),
                b"notice".to_vec(),
                0o644,
            ),
            ("SOURCE_REV".to_string(), b"deadbeef".to_vec(), 0o644),
            (
                "THIRD-PARTY-NOTICES.txt".to_string(),
                b"third party".to_vec(),
                0o644,
            ),
            (
                "THIRD-PARTY-NOTICES-xai-grok-tools.md".to_string(),
                b"tools notices".to_vec(),
                0o644,
            ),
        ];
        let manifest = entries
            .iter()
            .map(|(name, bytes, _)| format!("{}  {name}", sha256_hex(bytes)))
            .collect::<Vec<_>>()
            .join("\n");
        entries.push((INNER_MANIFEST.to_string(), manifest.into_bytes(), 0o644));
        entries
    }

    fn linux_package_entries() -> Vec<(String, Vec<u8>, u32)> {
        let mut entries = vec![
            ("grok-zh".to_string(), b"verified ELF".to_vec(), 0o755),
            (
                "BUILD-INFO.txt".to_string(),
                b"Linux x86_64 GNU build".to_vec(),
                0o644,
            ),
            (
                "INSTALL-LINUX.md".to_string(),
                b"installation guide".to_vec(),
                0o644,
            ),
            (
                "Install-GrokZh.sh".to_string(),
                b"#!/bin/sh\nexit 0\n".to_vec(),
                0o755,
            ),
            (
                "LICENSE-grok-build.txt".to_string(),
                b"license".to_vec(),
                0o644,
            ),
            (
                "NOTICE-third-party.txt".to_string(),
                b"notice".to_vec(),
                0o644,
            ),
            ("SOURCE_REV".to_string(), b"deadbeef".to_vec(), 0o644),
            (
                "THIRD-PARTY-NOTICES.txt".to_string(),
                b"third party".to_vec(),
                0o644,
            ),
            (
                "THIRD-PARTY-NOTICES-xai-grok-tools.md".to_string(),
                b"tools notices".to_vec(),
                0o644,
            ),
        ];
        let manifest = entries
            .iter()
            .map(|(name, bytes, _)| format!("{}  {name}", sha256_hex(bytes)))
            .collect::<Vec<_>>()
            .join("\n");
        entries.push((INNER_MANIFEST.to_string(), manifest.into_bytes(), 0o644));
        entries
    }

    fn append_ustar_entry<W: Write>(
        builder: &mut tar::Builder<W>,
        name: &str,
        bytes: &[u8],
        mode: u32,
        entry_type: tar::EntryType,
        link_name: Option<&str>,
    ) {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_size(bytes.len() as u64);
        if let Some(link_name) = link_name {
            header.set_link_name(link_name).unwrap();
        }
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
    }

    fn write_unix_tar(
        path: &Path,
        entries: &[(String, Vec<u8>, u32)],
        extra: Option<(&str, tar::EntryType, Option<&str>)>,
    ) {
        let file = File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        append_ustar_entry(
            &mut builder,
            ".",
            &[],
            0o755,
            tar::EntryType::Directory,
            None,
        );
        for (name, bytes, mode) in entries {
            append_ustar_entry(
                &mut builder,
                name,
                bytes,
                *mode,
                tar::EntryType::Regular,
                None,
            );
        }
        if let Some((name, entry_type, link_name)) = extra {
            append_ustar_entry(&mut builder, name, &[], 0o644, entry_type, link_name);
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn stable_selects_highest_immutable_non_prerelease() {
        let releases = vec![
            release("v1.1.0-alpha.2", true, true),
            release("v1.0.1", false, true),
            release("v1.0.3", false, true),
            release("v1.0.2", false, true),
            release("v1.2.0", false, false),
            release("v1.0.0", false, true),
        ];
        let (_, version) = select_latest_release(&releases, "stable").unwrap();
        assert_eq!(version.to_string(), "1.0.3");
    }

    #[test]
    fn alpha_uses_semver_not_api_order() {
        let releases = vec![
            release("v1.0.0", false, true),
            release("v1.1.0-alpha.1", true, true),
            release("v0.9.9", false, true),
        ];
        let (_, version) = select_latest_release(&releases, "alpha").unwrap();
        assert_eq!(version.to_string(), "1.1.0-alpha.1");
    }

    #[test]
    fn mutable_and_metadata_mismatched_releases_are_rejected() {
        let releases = vec![
            release("v2.0.0", false, false),
            release("v1.0.0-alpha.1", false, true),
            release("1.0.0", false, true),
        ];
        assert!(select_latest_release(&releases, "stable").is_err());
    }

    #[test]
    fn digest_parser_is_strict() {
        let valid = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(parse_sha256_digest(&valid).unwrap(), "ab".repeat(32));
        assert!(parse_sha256_digest(&"ab".repeat(32)).is_err());
        assert!(parse_sha256_digest("sha256:xyz").is_err());
    }

    #[test]
    fn github_url_policy_requires_https_default_port_and_known_hosts() {
        for url in [
            "https://api.github.com/repos/example/releases",
            "https://github.com/example/releases/download/v1/file.zip",
            "https://release-assets.githubusercontent.com/file",
        ] {
            assert!(allowed_github_url(&reqwest::Url::parse(url).unwrap()));
        }
        for url in [
            "http://github.com/file",
            "https://github.com:8443/file",
            "https://example.com/file",
        ] {
            assert!(!allowed_github_url(&reqwest::Url::parse(url).unwrap()));
        }
    }

    #[test]
    fn asset_selection_enforces_the_platform_transition_contract() {
        let mut transition = release("v1.0.6", false, true);
        transition.assets = uploaded_package_assets("1.0.6");
        let selected =
            select_asset_for_platform(&transition, "1.0.6", CommunityPlatform::WindowsX86_64Gnu)
                .unwrap();
        assert_eq!(selected.version, "1.0.6");
        assert_eq!(selected.name, "grok-zh-1.0.6-windows-x86_64-gnu.zip");
        assert!(
            select_asset_for_platform(&transition, "1.0.6", CommunityPlatform::MacosAarch64)
                .is_err()
        );

        let mut candidate = release("v1.0.7-alpha.1", true, true);
        candidate.assets = uploaded_package_assets("1.0.7-alpha.1");
        let windows = select_asset_for_platform(
            &candidate,
            "1.0.7-alpha.1",
            CommunityPlatform::WindowsX86_64Gnu,
        )
        .unwrap();
        assert_eq!(windows.name, "grok-zh-1.0.7-alpha.1-windows-x86_64-gnu.zip");
        let macos =
            select_asset_for_platform(&candidate, "1.0.7-alpha.1", CommunityPlatform::MacosAarch64)
                .unwrap();
        assert_eq!(macos.name, "grok-zh-1.0.7-alpha.1-macos-aarch64.tar.gz");
        assert!(
            select_asset_for_platform(
                &candidate,
                "1.0.7-alpha.1",
                CommunityPlatform::LinuxX86_64Gnu
            )
            .is_err()
        );

        let mut bridge = release("v1.0.8", false, true);
        bridge.assets = uploaded_package_assets("1.0.8");
        assert_eq!(bridge.assets.len(), 4);
        assert!(
            select_asset_for_platform(&bridge, "1.0.8", CommunityPlatform::WindowsX86_64Gnu)
                .is_ok()
        );
        assert!(
            select_asset_for_platform(&bridge, "1.0.8", CommunityPlatform::LinuxX86_64Gnu).is_err()
        );

        let mut linux_candidate = release("v1.0.8-rc.1", true, true);
        linux_candidate.assets = uploaded_package_assets("1.0.8-rc.1");
        assert_eq!(linux_candidate.assets.len(), 6);
        let linux = select_asset_for_platform(
            &linux_candidate,
            "1.0.8-rc.1",
            CommunityPlatform::LinuxX86_64Gnu,
        )
        .unwrap();
        assert_eq!(linux.name, "grok-zh-1.0.8-rc.1-linux-x86_64-gnu.tar.gz");

        let mut next_stable = release("v1.0.9", false, true);
        next_stable.assets = uploaded_package_assets("1.0.9");
        assert_eq!(next_stable.assets.len(), 6);
        assert!(
            select_asset_for_platform(&next_stable, "1.0.9", CommunityPlatform::LinuxX86_64Gnu)
                .is_ok()
        );

        candidate.assets.push(uploaded_asset(
            "1.0.7-alpha.1",
            "grok-zh-1.0.7-alpha.1-windows-x86_64-gnu.exe".to_string(),
            123,
        ));
        assert!(
            select_asset_for_platform(
                &candidate,
                "1.0.7-alpha.1",
                CommunityPlatform::WindowsX86_64Gnu
            )
            .is_err()
        );

        candidate.assets.pop();
        candidate.assets[0].browser_download_url = "https://example.com/grok-zh.zip".to_string();
        assert!(
            select_asset_for_platform(
                &candidate,
                "1.0.7-alpha.1",
                CommunityPlatform::WindowsX86_64Gnu
            )
            .is_err()
        );
    }

    #[test]
    fn latest_release_selection_skips_platform_incompatible_assets() {
        let mut transition = release("v1.0.6", false, true);
        transition.assets = uploaded_package_assets("1.0.6");
        let mut cross_platform = release("v1.0.7", false, true);
        cross_platform.assets = uploaded_package_assets("1.0.7");
        let releases = vec![cross_platform, transition];

        let (_, windows) = select_latest_compatible_release(
            &releases,
            "stable",
            CommunityPlatform::WindowsX86_64Gnu,
        )
        .unwrap();
        assert_eq!(windows.to_string(), "1.0.7");
        let (_, macos) =
            select_latest_compatible_release(&releases, "stable", CommunityPlatform::MacosAarch64)
                .unwrap();
        assert_eq!(macos.to_string(), "1.0.7");

        let only_transition = &releases[1..];
        assert!(
            select_latest_compatible_release(
                only_transition,
                "stable",
                CommunityPlatform::MacosAarch64
            )
            .is_err()
        );
    }

    #[test]
    fn community_download_progress_template_is_valid() {
        assert!(
            ProgressStyle::default_bar()
                .template(DOWNLOAD_PROGRESS_TEMPLATE)
                .is_ok()
        );
    }

    #[test]
    fn completed_community_download_keeps_its_final_progress() {
        use indicatif::ProgressDrawTarget;

        let terminal = RecordingTerm::default();
        let progress = ProgressBar::with_draw_target(
            Some(100),
            ProgressDrawTarget::term_like(Box::new(terminal.clone())),
        );
        progress.set_style(
            ProgressStyle::default_bar()
                .template(DOWNLOAD_PROGRESS_TEMPLATE)
                .unwrap(),
        );
        progress.set_position(100);

        finish_download_progress(&progress, true);

        assert!(terminal.contents().contains("100%"));
    }

    #[test]
    fn failed_community_download_clears_partial_progress() {
        use indicatif::ProgressDrawTarget;

        let terminal = RecordingTerm::default();
        let progress = ProgressBar::with_draw_target(
            Some(100),
            ProgressDrawTarget::term_like(Box::new(terminal.clone())),
        );
        progress.set_style(
            ProgressStyle::default_bar()
                .template(DOWNLOAD_PROGRESS_TEMPLATE)
                .unwrap(),
        );
        progress.set_position(50);
        progress.tick();
        assert!(!terminal.contents().is_empty());

        finish_download_progress(&progress, false);

        assert!(terminal.contents().is_empty());
    }

    #[test]
    fn community_api_urls_follow_the_product_repository_identity() {
        assert_eq!(release_repo(), "JoyElliot/grok-build-Chinese");
        assert_eq!(
            releases_api(2),
            "https://api.github.com/repos/JoyElliot/grok-build-Chinese/releases?per_page=100&page=2"
        );
        assert_eq!(
            release_by_tag_api("1.0.3"),
            "https://api.github.com/repos/JoyElliot/grok-build-Chinese/releases/tags/v1.0.3"
        );
    }

    #[test]
    fn valid_package_zip_extracts_only_the_verified_executable() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.zip");
        let candidate = temp.path().join("candidate.exe");
        write_zip(&archive, &package_entries());

        extract_current_windows_executable(&archive, &candidate).unwrap();
        assert_eq!(std::fs::read(candidate).unwrap(), b"verified executable");
        assert!(!temp.path().join("rg.exe").exists());
    }

    #[test]
    fn published_v1_0_5_legacy_manifest_extracts_verified_executable() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.zip");
        let candidate = temp.path().join("candidate.exe");
        write_zip(
            &archive,
            &package_entries_with_manifest(&WINDOWS_V1_0_5_MANIFEST_FILES),
        );

        extract_verified_executable(&verified_windows_asset("1.0.5"), &archive, &candidate)
            .unwrap();
        assert_eq!(std::fs::read(candidate).unwrap(), b"verified executable");
        assert!(!temp.path().join("rg.exe").exists());
    }

    #[test]
    fn legacy_manifest_is_rejected_for_other_release_versions() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.zip");
        let candidate = temp.path().join("candidate.exe");
        write_zip(
            &archive,
            &package_entries_with_manifest(&WINDOWS_V1_0_5_MANIFEST_FILES),
        );

        let error =
            extract_verified_executable(&verified_windows_asset("1.0.6"), &archive, &candidate)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manifest does not contain the exact approved file set"),
            "{error:#}"
        );
        assert!(!candidate.exists());
    }

    #[test]
    fn published_v1_0_5_legacy_manifest_still_checks_inner_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.zip");
        let candidate = temp.path().join("candidate.exe");
        let mut entries = package_entries_with_manifest(&WINDOWS_V1_0_5_MANIFEST_FILES);
        entries
            .iter_mut()
            .find(|(name, _)| name == "grok-zh.exe")
            .unwrap()
            .1 = b"tampered executable".to_vec();
        write_zip(&archive, &entries);

        let error =
            extract_verified_executable(&verified_windows_asset("1.0.5"), &archive, &candidate)
                .unwrap_err();
        assert!(error.to_string().contains("inner SHA-256 mismatch"));
        assert!(!candidate.exists());
    }

    #[test]
    fn inner_hash_mismatch_fails_closed_and_removes_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.zip");
        let candidate = temp.path().join("candidate.exe");
        let mut entries = package_entries();
        entries
            .iter_mut()
            .find(|(name, _)| name == "grok-zh.exe")
            .unwrap()
            .1 = b"tampered executable".to_vec();
        write_zip(&archive, &entries);

        let error = extract_current_windows_executable(&archive, &candidate).unwrap_err();
        assert!(error.to_string().contains("inner SHA-256 mismatch"));
        assert!(!candidate.exists());
    }

    #[test]
    fn unsafe_or_incomplete_package_zip_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate.exe");

        let traversal = temp.path().join("traversal.zip");
        let mut entries = package_entries();
        entries.push(("../escape.txt".to_string(), b"escape".to_vec()));
        write_zip(&traversal, &entries);
        assert!(extract_current_windows_executable(&traversal, &candidate).is_err());
        assert!(!candidate.exists());

        let internal_parent = temp.path().join("internal-parent.zip");
        let mut entries = package_entries();
        entries.push(("nested/../escape.txt".to_string(), b"escape".to_vec()));
        write_zip(&internal_parent, &entries);
        assert!(extract_current_windows_executable(&internal_parent, &candidate).is_err());
        assert!(!candidate.exists());

        let current_segment = temp.path().join("current-segment.zip");
        let mut entries = package_entries();
        entries.push(("./escape.txt".to_string(), b"escape".to_vec()));
        write_zip(&current_segment, &entries);
        assert!(extract_current_windows_executable(&current_segment, &candidate).is_err());
        assert!(!candidate.exists());

        let duplicate_normalized_entry = temp.path().join("duplicate-normalized-entry.zip");
        let mut entries = package_entries();
        entries.push(("RG.EXE".to_string(), b"duplicate ripgrep".to_vec()));
        write_zip(&duplicate_normalized_entry, &entries);
        let error = extract_current_windows_executable(&duplicate_normalized_entry, &candidate)
            .unwrap_err();
        assert!(error.to_string().contains("duplicate path"), "{error:#}");
        assert!(!candidate.exists());

        let non_ascii_entry = temp.path().join("non-ascii-entry.zip");
        let mut entries = package_entries();
        entries.push(("É.txt".to_string(), b"ambiguous on Windows".to_vec()));
        write_zip(&non_ascii_entry, &entries);
        assert!(extract_current_windows_executable(&non_ascii_entry, &candidate).is_err());
        assert!(!candidate.exists());

        let non_ascii_manifest = temp.path().join("non-ascii-manifest.zip");
        let mut entries = package_entries();
        let manifest = entries
            .iter_mut()
            .find(|(name, _)| name == "SHA256SUMS.txt")
            .unwrap();
        manifest
            .1
            .extend_from_slice(format!("\n{}  É.txt", "00".repeat(32)).as_bytes());
        write_zip(&non_ascii_manifest, &entries);
        assert!(extract_current_windows_executable(&non_ascii_manifest, &candidate).is_err());
        assert!(!candidate.exists());

        let duplicate_unicode_manifest = temp.path().join("duplicate-unicode-manifest.zip");
        let mut entries = package_entries();
        let manifest = entries
            .iter_mut()
            .find(|(name, _)| name == "SHA256SUMS.txt")
            .unwrap();
        manifest
            .1
            .extend_from_slice(format!("\n{}  {ONE_CLICK_INSTALLER}", "00".repeat(32)).as_bytes());
        write_zip(&duplicate_unicode_manifest, &entries);
        assert!(
            extract_current_windows_executable(&duplicate_unicode_manifest, &candidate).is_err()
        );
        assert!(!candidate.exists());

        for (suffix, missing_required) in [
            ("rg", "rg.exe"),
            ("one-click", ONE_CLICK_INSTALLER),
            ("command-setup", COMMAND_SETUP_INSTALLER),
        ] {
            let incomplete = temp.path().join(format!("incomplete-{suffix}.zip"));
            let mut entries = package_entries();
            let manifest = entries
                .iter_mut()
                .find(|(name, _)| name == "SHA256SUMS.txt")
                .unwrap();
            manifest.1 = String::from_utf8(manifest.1.clone())
                .unwrap()
                .lines()
                .filter(|line| !line.ends_with(&format!("  {missing_required}")))
                .collect::<Vec<_>>()
                .join("\n")
                .into_bytes();
            write_zip(&incomplete, &entries);
            assert!(extract_current_windows_executable(&incomplete, &candidate).is_err());
            assert!(!candidate.exists());
        }
    }

    #[test]
    fn valid_macos_tar_extracts_only_the_verified_executable() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let candidate = temp.path().join("candidate");
        write_unix_tar(&archive, &macos_package_entries(), None);

        extract_verified_macos_executable(&archive, &candidate).unwrap();
        assert_eq!(std::fs::read(candidate).unwrap(), b"verified Mach-O");
        assert!(!temp.path().join("BUILD-INFO.txt").exists());
    }

    #[test]
    fn valid_linux_tar_extracts_only_the_verified_executable() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let candidate = temp.path().join("candidate");
        write_unix_tar(&archive, &linux_package_entries(), None);

        extract_verified_linux_executable(&archive, &candidate).unwrap();
        assert_eq!(std::fs::read(candidate).unwrap(), b"verified ELF");
        assert!(!temp.path().join("BUILD-INFO.txt").exists());
    }

    #[test]
    fn linux_tar_rejects_macos_package_layout() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let candidate = temp.path().join("candidate");
        write_unix_tar(&archive, &macos_package_entries(), None);

        let error = extract_verified_linux_executable(&archive, &candidate).unwrap_err();
        assert!(
            error.to_string().contains("extra or duplicate path"),
            "{error:#}"
        );
        assert!(!candidate.exists());
    }

    #[test]
    fn macos_tar_hash_mismatch_fails_closed_and_removes_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let candidate = temp.path().join("candidate");
        let mut entries = macos_package_entries();
        entries
            .iter_mut()
            .find(|(name, _, _)| name == "grok-zh")
            .unwrap()
            .1 = b"tampered Mach-O".to_vec();
        write_unix_tar(&archive, &entries, None);

        let error = extract_verified_macos_executable(&archive, &candidate).unwrap_err();
        assert!(error.to_string().contains("inner SHA-256 mismatch"));
        assert!(!candidate.exists());
    }

    #[test]
    fn macos_tar_rejects_extra_duplicate_and_link_entries() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");
        let entries = macos_package_entries();

        for (suffix, extra) in [
            ("extra", ("extra.txt", tar::EntryType::Regular, None)),
            ("duplicate", ("GROK-ZH", tar::EntryType::Regular, None)),
            (
                "symlink",
                ("replacement", tar::EntryType::Symlink, Some("grok-zh")),
            ),
            (
                "hardlink",
                ("replacement", tar::EntryType::Link, Some("grok-zh")),
            ),
        ] {
            let archive = temp.path().join(format!("{suffix}.tar.gz"));
            write_unix_tar(&archive, &entries, Some(extra));
            assert!(
                extract_verified_macos_executable(&archive, &candidate).is_err(),
                "{suffix} tar should be rejected"
            );
            assert!(!candidate.exists());
        }
    }

    #[test]
    fn macos_tar_rejects_trailing_gzip_payload() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("release.tar.gz");
        let candidate = temp.path().join("candidate");
        write_unix_tar(&archive, &macos_package_entries(), None);

        let mut trailing_encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        trailing_encoder
            .write_all(b"unexpected trailing member")
            .unwrap();
        let trailing = trailing_encoder.finish().unwrap();
        let mut archive_file = OpenOptions::new().append(true).open(&archive).unwrap();
        archive_file.write_all(&trailing).unwrap();
        archive_file.sync_all().unwrap();

        assert!(extract_verified_macos_executable(&archive, &candidate).is_err());
        assert!(!candidate.exists());

        for (suffix, trailing_bytes) in [("empty", Vec::new()), ("zeros", vec![0u8; 512])] {
            let archive = temp.path().join(format!("release-{suffix}.tar.gz"));
            write_unix_tar(&archive, &macos_package_entries(), None);
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&trailing_bytes).unwrap();
            let trailing = encoder.finish().unwrap();
            let mut archive_file = OpenOptions::new().append(true).open(&archive).unwrap();
            archive_file.write_all(&trailing).unwrap();
            archive_file.sync_all().unwrap();

            assert!(
                extract_verified_macos_executable(&archive, &candidate).is_err(),
                "concatenated {suffix} gzip member must be rejected"
            );
            assert!(!candidate.exists());
        }
    }

    #[test]
    fn macos_tar_requires_root_entry_modes_and_complete_end_marker() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");

        let wrong_mode = temp.path().join("wrong-mode.tar.gz");
        let mut entries = macos_package_entries();
        entries
            .iter_mut()
            .find(|(name, _, _)| name == "Install-GrokZh.sh")
            .unwrap()
            .2 = 0o644;
        write_unix_tar(&wrong_mode, &entries, None);
        assert!(extract_verified_macos_executable(&wrong_mode, &candidate).is_err());
        assert!(!candidate.exists());

        let valid = temp.path().join("valid.tar.gz");
        write_unix_tar(&valid, &macos_package_entries(), None);
        let mut decoder = flate2::read::GzDecoder::new(File::open(&valid).unwrap());
        let mut raw_tar = Vec::new();
        decoder.read_to_end(&mut raw_tar).unwrap();
        let last_nonzero = raw_tar.iter().rposition(|byte| *byte != 0).unwrap();
        let truncated_len = (last_nonzero + 512) / 512 * 512;
        raw_tar.truncate(truncated_len);
        let truncated = temp.path().join("missing-end-marker.tar.gz");
        let file = File::create(&truncated).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        encoder.write_all(&raw_tar).unwrap();
        encoder.finish().unwrap();
        assert!(extract_verified_macos_executable(&truncated, &candidate).is_err());
        assert!(!candidate.exists());
    }
}
