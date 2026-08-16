//! GitHub Releases backend for the Simplified Chinese community build.
//!
//! The repository, API endpoint, asset naming, and accepted platforms are
//! compile-time policy.  Community builds never consult the upstream npm,
//! GitHub, x.ai, or GCS update sources.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
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
const DOWNLOAD_PROGRESS_TEMPLATE: &str =
    "  下载更新 {bar:30.cyan/dim} {bytes}/{total_bytes} {percent}% ({bytes_per_sec}，剩余 {eta})";
const ONE_CLICK_INSTALLER: &str = "一键安装.cmd";
const COMMAND_SETUP_INSTALLER: &str = "[可选]替换原始启动方式.cmd";
const REQUIRED_PACKAGE_FILES: [&str; 7] = [
    "grok-zh.exe",
    "agent-zh.cmd",
    "rg.exe",
    ONE_CLICK_INSTALLER,
    COMMAND_SETUP_INSTALLER,
    "Install-GrokZh.ps1",
    "INSTALL-WINDOWS.md",
];

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
    let (_, version) = select_latest_release(&releases, channel)?;
    Ok(version.to_string())
}

fn release_asset_name(version: &str) -> Result<String> {
    let parsed = Version::parse(version)
        .with_context(|| format!("invalid community release version: {version}"))?;
    if parsed.to_string() != version || !parsed.build.is_empty() {
        anyhow::bail!("community release version is not canonical: {version}");
    }
    if !(cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") && cfg!(target_env = "gnu")) {
        anyhow::bail!("community self-update currently supports only x86_64-pc-windows-gnu");
    }
    Ok(format!("grok-zh-{version}-windows-x86_64-gnu.zip"))
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

fn select_asset(release: &ApiRelease, version: &str) -> Result<VerifiedAsset> {
    let parsed = parse_release_version(release)
        .ok_or_else(|| anyhow::anyhow!("release is mutable, draft, or has invalid metadata"))?;
    if parsed.to_string() != version || release.tag_name != format!("v{version}") {
        anyhow::bail!("release tag and requested version do not match");
    }
    let name = release_asset_name(version)?;
    let sidecar_name = format!("{name}.sha256");
    let mut actual_names: Vec<&str> = release
        .assets
        .iter()
        .filter(|asset| asset.state == "uploaded")
        .map(|asset| asset.name.as_str())
        .collect();
    actual_names.sort_unstable();
    let mut expected_names = vec![name.as_str(), sidecar_name.as_str()];
    expected_names.sort_unstable();
    if release.assets.len() != 2 || actual_names != expected_names {
        anyhow::bail!(
            "release assets must be exactly {name} and {sidecar_name}; raw executables are not accepted"
        );
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
    if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
        anyhow::bail!("release asset size is outside the accepted range");
    }
    if sidecar.size == 0 || sidecar.size > MAX_SIDECAR_BYTES {
        anyhow::bail!("release checksum sidecar size is outside the accepted range");
    }
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
    progress.finish_and_clear();
    if result.is_err() && created_destination {
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

fn validate_archive_layout(archive: &mut zip::ZipArchive<File>) -> Result<()> {
    if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
        anyhow::bail!("community release ZIP contains an invalid number of entries");
    }

    let mut seen = HashSet::new();
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
        if !enclosed_text.is_ascii() && !is_allowed_unicode_package_name(&enclosed_text) {
            anyhow::bail!(
                "community release ZIP entry name contains unapproved Unicode: {raw_name}"
            );
        }
        let normalized = if enclosed_text.is_ascii() {
            enclosed_text.replace('\\', "/").to_ascii_lowercase()
        } else {
            enclosed_text.to_string()
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
    Ok(())
}

fn read_inner_manifest(archive: &mut zip::ZipArchive<File>) -> Result<HashMap<String, String>> {
    let entry = archive
        .by_name("SHA256SUMS.txt")
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
    let text =
        std::str::from_utf8(&bytes).context("community release SHA256SUMS.txt is not UTF-8")?;
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
        if name.is_empty()
            || name.trim() != name
            || (!name.is_ascii() && !is_allowed_unicode_package_name(name))
            || !REQUIRED_PACKAGE_FILES.contains(&name)
            || name.contains(':')
            || name.contains('/')
            || name.contains('\\')
            || matches!(name, "." | "..")
        {
            anyhow::bail!(
                "community release SHA256SUMS.txt line {} has an unsafe filename",
                line_index + 1
            );
        }
        let normalized = if name.is_ascii() {
            name.to_ascii_lowercase()
        } else {
            name.to_string()
        };
        if !normalized_names.insert(normalized) {
            anyhow::bail!("community release SHA256SUMS.txt contains a duplicate filename");
        }
        hashes.insert(name.to_string(), digest.to_ascii_lowercase());
    }

    for required in REQUIRED_PACKAGE_FILES {
        if !hashes.contains_key(required) {
            anyhow::bail!("community release manifest is missing required file {required}");
        }
    }
    Ok(hashes)
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
/// installer; they are nevertheless required and hashed here so a partial or
/// malformed package can never activate its executable.
pub(crate) fn extract_verified_executable(archive_path: &Path, destination: &Path) -> Result<()> {
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
        validate_archive_layout(&mut archive)?;
        let hashes = read_inner_manifest(&mut archive)?;
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

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn release(tag: &str, prerelease: bool, immutable: bool) -> ApiRelease {
        ApiRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease,
            immutable,
            assets: Vec::new(),
        }
    }

    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "gnu"))]
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

    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "gnu"))]
    fn uploaded_package_assets(version: &str) -> Vec<ApiAsset> {
        let zip_name = format!("grok-zh-{version}-windows-x86_64-gnu.zip");
        vec![
            uploaded_asset(version, zip_name.clone(), 123),
            uploaded_asset(version, format!("{zip_name}.sha256"), 112),
        ]
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

    fn package_entries() -> Vec<(String, Vec<u8>)> {
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
        ];
        let manifest = entries
            .iter()
            .map(|(name, bytes)| format!("{}  {name}", sha256_hex(bytes)))
            .collect::<Vec<_>>()
            .join("\n");
        entries.push(("SHA256SUMS.txt".to_string(), manifest.into_bytes()));
        entries
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
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "gnu"))]
    fn asset_selection_requires_the_exact_zip_only_asset_set() {
        let mut candidate = release("v1.2.3", false, true);
        candidate.assets = uploaded_package_assets("1.2.3");
        let selected = select_asset(&candidate, "1.2.3").unwrap();
        assert_eq!(selected.version, "1.2.3");
        assert_eq!(selected.name, "grok-zh-1.2.3-windows-x86_64-gnu.zip");

        candidate.assets.push(uploaded_asset(
            "1.2.3",
            "grok-zh-1.2.3-windows-x86_64-gnu.exe".to_string(),
            123,
        ));
        assert!(select_asset(&candidate, "1.2.3").is_err());

        candidate.assets.pop();
        candidate.assets[0].browser_download_url = "https://example.com/grok-zh.zip".to_string();
        assert!(select_asset(&candidate, "1.2.3").is_err());
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

        extract_verified_executable(&archive, &candidate).unwrap();
        assert_eq!(std::fs::read(candidate).unwrap(), b"verified executable");
        assert!(!temp.path().join("rg.exe").exists());
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

        let error = extract_verified_executable(&archive, &candidate).unwrap_err();
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
        assert!(extract_verified_executable(&traversal, &candidate).is_err());
        assert!(!candidate.exists());

        let internal_parent = temp.path().join("internal-parent.zip");
        let mut entries = package_entries();
        entries.push(("nested/../escape.txt".to_string(), b"escape".to_vec()));
        write_zip(&internal_parent, &entries);
        assert!(extract_verified_executable(&internal_parent, &candidate).is_err());
        assert!(!candidate.exists());

        let current_segment = temp.path().join("current-segment.zip");
        let mut entries = package_entries();
        entries.push(("./escape.txt".to_string(), b"escape".to_vec()));
        write_zip(&current_segment, &entries);
        assert!(extract_verified_executable(&current_segment, &candidate).is_err());
        assert!(!candidate.exists());

        let duplicate_normalized_entry = temp.path().join("duplicate-normalized-entry.zip");
        let mut entries = package_entries();
        entries.push(("RG.EXE".to_string(), b"duplicate ripgrep".to_vec()));
        write_zip(&duplicate_normalized_entry, &entries);
        let error =
            extract_verified_executable(&duplicate_normalized_entry, &candidate).unwrap_err();
        assert!(error.to_string().contains("duplicate path"), "{error:#}");
        assert!(!candidate.exists());

        let non_ascii_entry = temp.path().join("non-ascii-entry.zip");
        let mut entries = package_entries();
        entries.push(("É.txt".to_string(), b"ambiguous on Windows".to_vec()));
        write_zip(&non_ascii_entry, &entries);
        assert!(extract_verified_executable(&non_ascii_entry, &candidate).is_err());
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
        assert!(extract_verified_executable(&non_ascii_manifest, &candidate).is_err());
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
        assert!(extract_verified_executable(&duplicate_unicode_manifest, &candidate).is_err());
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
            assert!(extract_verified_executable(&incomplete, &candidate).is_err());
            assert!(!candidate.exists());
        }
    }
}
