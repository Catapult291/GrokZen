//! GitHub Releases backend for the Simplified Chinese community build.
//!
//! The repository, API endpoint, asset naming, and accepted platforms are
//! compile-time policy.  Community builds never consult the upstream npm,
//! GitHub, x.ai, or GCS update sources.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::redirect::Policy;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub(crate) const COMMUNITY_INSTALLER: &str = "community-github";

const RELEASES_API: &str =
    "https://api.github.com/repos/ljy6-6-6/grok-build-Chinese/releases?per_page=100";
const RELEASE_BY_TAG_API: &str =
    "https://api.github.com/repos/ljy6-6-6/grok-build-Chinese/releases/tags/";
const API_VERSION: &str = "2026-03-10";
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

fn release_repo() -> &'static str {
    xai_grok_product::COMMUNITY_RELEASE_REPO
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

fn api_client() -> Result<reqwest::Client> {
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
        .timeout(Duration::from_secs(30))
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

fn parse_release_version(release: &ApiRelease) -> Option<semver::Version> {
    if release.draft || !release.immutable {
        return None;
    }
    let version = semver::Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
    if !version.build.is_empty() || release.prerelease != !version.pre.is_empty() {
        return None;
    }
    Some(version)
}

fn select_latest_release<'a>(
    releases: &'a [ApiRelease],
    channel: &str,
) -> Result<(&'a ApiRelease, semver::Version)> {
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
    let releases: Vec<ApiRelease> = fetch_json(RELEASES_API).await?;
    let (_, version) = select_latest_release(&releases, channel)?;
    Ok(version.to_string())
}

fn release_asset_name(version: &str) -> Result<String> {
    semver::Version::parse(version)
        .with_context(|| format!("invalid community release version: {version}"))?;
    if !(cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") && cfg!(target_env = "gnu")) {
        anyhow::bail!("community self-update currently supports only x86_64-pc-windows-gnu");
    }
    Ok(format!("grok-zh-{version}-windows-x86_64-gnu.exe"))
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
    let matches: Vec<&ApiAsset> = release
        .assets
        .iter()
        .filter(|asset| asset.name == name && asset.state == "uploaded")
        .collect();
    let [asset] = matches.as_slice() else {
        anyhow::bail!("release must contain exactly one uploaded asset named {name}");
    };
    if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
        anyhow::bail!("release asset size is outside the accepted range");
    }
    let expected_url = format!(
        "https://github.com/{}/releases/download/v{version}/{name}",
        release_repo()
    );
    if asset.browser_download_url != expected_url {
        anyhow::bail!("release asset URL does not match the fixed community repository");
    }
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
    semver::Version::parse(version)
        .with_context(|| format!("invalid community release version: {version}"))?;
    let url = format!("{RELEASE_BY_TAG_API}v{version}");
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
    let result = async {
        let response = api_client()?
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
    if result.is_err() {
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

#[cfg(test)]
mod tests {
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
    fn uploaded_asset(version: &str) -> ApiAsset {
        let name = format!("grok-zh-{version}-windows-x86_64-gnu.exe");
        ApiAsset {
            browser_download_url: format!(
                "https://github.com/{}/releases/download/v{version}/{name}",
                release_repo()
            ),
            name,
            size: 123,
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
            state: "uploaded".to_string(),
        }
    }

    #[test]
    fn stable_selects_highest_immutable_non_prerelease() {
        let releases = vec![
            release("v1.1.0-alpha.2", true, true),
            release("v1.0.1", false, true),
            release("v1.2.0", false, false),
            release("v1.0.0", false, true),
        ];
        let (_, version) = select_latest_release(&releases, "stable").unwrap();
        assert_eq!(version.to_string(), "1.0.1");
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
            "https://github.com/example/releases/download/v1/file.exe",
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
    fn asset_selection_requires_one_exact_fork_owned_asset() {
        let mut candidate = release("v1.2.3", false, true);
        candidate.assets.push(uploaded_asset("1.2.3"));
        let selected = select_asset(&candidate, "1.2.3").unwrap();
        assert_eq!(selected.version, "1.2.3");
        assert_eq!(selected.name, "grok-zh-1.2.3-windows-x86_64-gnu.exe");

        candidate.assets.push(uploaded_asset("1.2.3"));
        assert!(select_asset(&candidate, "1.2.3").is_err());

        candidate.assets.pop();
        candidate.assets[0].browser_download_url = "https://example.com/grok-zh.exe".to_string();
        assert!(select_asset(&candidate, "1.2.3").is_err());
    }
}
