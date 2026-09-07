//! Independently versioned, display-only announcement translations.
//!
//! Official announcement loads trigger a parallel check; this worker adds no
//! polling timer or dependency to official requests, authentication, or startup.
//! Consumers render immutable snapshots; all IO stays in the worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;

const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 4096;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
const MAX_CACHE_BYTES: usize = MAX_CATALOG_BYTES * 2 + MAX_MANIFEST_BYTES;
const MAX_ENTRIES: usize = 512;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(8);
const CACHE_IO_TIMEOUT: Duration = Duration::from_secs(2);
const BUNDLED_CATALOG: &str =
    include_str!("../../../../../community/announcements/catalogs/1.json");

/// Only display text can be mapped. IDs, severities and link targets have no
/// representation in the translation format.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TranslationField {
    Title,
    Message,
    CtaLabel,
    CtaCaption,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    version: u64,
    sha256: String,
}

impl Manifest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported catalog schema"
        );
        ensure!(self.version > 0, "catalog version must be positive");
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid catalog digest"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    schema_version: u32,
    version: u64,
    locale: String,
    entries: Vec<TranslationEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranslationEntry {
    field: TranslationField,
    source: String,
    translation: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedCatalog {
    manifest: Manifest,
    catalog_json: String,
}

/// One validated snapshot. Its original JSON is retained for digest-checked
/// persistence; render-time lookups never parse JSON, lock, or touch the disk.
#[derive(Debug)]
pub struct TranslationCatalog {
    manifest: Manifest,
    catalog_json: String,
    entries: BTreeMap<TranslationField, BTreeMap<String, String>>,
}

impl TranslationCatalog {
    pub fn bundled() -> Arc<Self> {
        static BUNDLED: OnceLock<Arc<TranslationCatalog>> = OnceLock::new();
        Arc::clone(BUNDLED.get_or_init(|| {
            let manifest = Manifest {
                schema_version: SCHEMA_VERSION,
                version: 1,
                sha256: digest(BUNDLED_CATALOG.as_bytes()),
            };
            let catalog =
                Self::parse(manifest, BUNDLED_CATALOG.as_bytes()).unwrap_or_else(|error| {
                    // A bad bundled catalog must not prevent the application from
                    // starting. The catalog validation check catches this in CI.
                    tracing::error!(%error, "invalid bundled announcement translations");
                    Self {
                        manifest: Manifest {
                            schema_version: SCHEMA_VERSION,
                            version: 0,
                            sha256: String::new(),
                        },
                        catalog_json: String::new(),
                        entries: BTreeMap::new(),
                    }
                });
            Arc::new(catalog)
        }))
    }

    pub fn version(&self) -> u64 {
        self.manifest.version
    }

    /// Exact source text, without case folding, trimming or pattern matching.
    pub fn lookup(&self, field: TranslationField, source: &str) -> Option<&str> {
        self.entries.get(&field)?.get(source).map(String::as_str)
    }

    fn parse(manifest: Manifest, bytes: &[u8]) -> Result<Self> {
        manifest.validate()?;
        ensure!(
            bytes.len() <= MAX_CATALOG_BYTES,
            "catalog exceeds size limit"
        );
        ensure!(digest(bytes) == manifest.sha256, "catalog digest mismatch");
        let document: CatalogDocument = serde_json::from_slice(bytes)?;
        ensure!(
            document.schema_version == SCHEMA_VERSION,
            "unsupported catalog schema"
        );
        ensure!(
            document.version == manifest.version,
            "catalog version mismatch"
        );
        ensure!(document.locale == "zh-CN", "unsupported catalog locale");
        ensure!(
            document.entries.len() <= MAX_ENTRIES,
            "too many translations"
        );
        let mut entries = BTreeMap::<TranslationField, BTreeMap<String, String>>::new();
        for entry in document.entries {
            validate_text(entry.field, &entry.source)?;
            validate_text(entry.field, &entry.translation)?;
            ensure!(
                entries
                    .entry(entry.field)
                    .or_default()
                    .insert(entry.source, entry.translation)
                    .is_none(),
                "duplicate source text for translation field"
            );
        }
        Ok(Self {
            manifest,
            catalog_json: std::str::from_utf8(bytes)?.to_owned(),
            entries,
        })
    }

    fn cache_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(&CachedCatalog {
            manifest: self.manifest.clone(),
            catalog_json: self.catalog_json.clone(),
        })?;
        ensure!(
            bytes.len() <= MAX_CACHE_BYTES,
            "catalog cache exceeds size limit"
        );
        Ok(bytes)
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_text(field: TranslationField, text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
        "invalid translation text length"
    );
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && !(field == TranslationField::Message && c == '\n')),
        "translation text contains control characters"
    );
    Ok(())
}

/// Owns the worker for exactly one TUI lifetime. Dropping it cancels outstanding
/// network IO, including early exits before the first frame is displayed.
pub struct TranslationUpdates {
    receiver: watch::Receiver<Arc<TranslationCatalog>>,
    task: tokio::task::JoinHandle<()>,
}

impl TranslationUpdates {
    /// Returns immediately; cache reads, TLS setup and HTTP happen off the
    /// caller's path. HTTP waits for an official announcement load signal;
    /// `network_enabled = false` still permits offline cache use.
    pub fn start(network_enabled: bool, load_started: watch::Receiver<bool>) -> Self {
        let (sender, receiver) = watch::channel(TranslationCatalog::bundled());
        let task = tokio::spawn(async move {
            let cache_path = xai_dirs::resolve_grok_home()
                .map(|home| home.join("cache/grok-zh/announcement-translations.json"));
            run_worker(sender, cache_path, network_enabled, load_started).await;
        });
        Self { receiver, task }
    }

    pub fn current(&self) -> Arc<TranslationCatalog> {
        Arc::clone(&self.receiver.borrow())
    }

    /// A closed worker yields None once; consumers should then remove this arm
    /// from their select loop to avoid a busy loop.
    pub async fn changed(&mut self) -> Option<Arc<TranslationCatalog>> {
        self.receiver.changed().await.ok()?;
        Some(Arc::clone(&self.receiver.borrow_and_update()))
    }
}

impl Drop for TranslationUpdates {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_worker(
    sender: watch::Sender<Arc<TranslationCatalog>>,
    cache_path: Option<PathBuf>,
    network_enabled: bool,
    load_started: watch::Receiver<bool>,
) {
    let mut current = TranslationCatalog::bundled();
    if let Some(path) = cache_path.as_deref()
        && let Ok(Ok(cached)) = tokio::time::timeout(CACHE_IO_TIMEOUT, read_cache(path)).await
        && (cached.version() > current.version()
            || (cached.version() == current.version()
                && cached.manifest.sha256 == current.manifest.sha256))
    {
        current = Arc::new(cached);
        sender.send_replace(Arc::clone(&current));
    }
    if !network_enabled || sender.is_closed() {
        return;
    }
    refresh_loop(
        sender,
        current,
        cache_path,
        RefreshSource {
            client: None,
            base_url: xai_grok_product::COMMUNITY_ANNOUNCEMENTS_BASE_URL.to_owned(),
            timeout: REFRESH_TIMEOUT,
        },
        load_started,
    )
    .await;
}

struct RefreshSource {
    client: Option<reqwest::Client>,
    base_url: String,
    timeout: Duration,
}

async fn refresh_loop(
    sender: watch::Sender<Arc<TranslationCatalog>>,
    mut current: Arc<TranslationCatalog>,
    cache_path: Option<PathBuf>,
    mut source: RefreshSource,
    mut load_started: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            () = sender.closed() => return,
            changed = load_started.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        // A signal sent during cache reads remains pending. Certificate setup
        // is lazy as well, so starting a worker alone cannot start a request.
        let check = async {
            if source.client.is_none() {
                // OS certificate roots may involve synchronous IO.
                match tokio::task::spawn_blocking(build_client).await {
                    Ok(Ok(client)) => source.client = Some(client),
                    error => {
                        tracing::debug!(?error, "announcement translation client unavailable");
                        return;
                    }
                }
            }
            let client = source.client.as_ref().expect("client initialized above");
            let refresh = refresh_catalog(client, &source.base_url, &current);
            match tokio::time::timeout(source.timeout, refresh).await {
                Ok(Ok(Some(catalog))) => {
                    current = Arc::new(catalog);
                    // Display does not wait for a slow or unwritable cache disk.
                    sender.send_replace(Arc::clone(&current));
                    if let Some(path) = cache_path.as_deref() {
                        match tokio::time::timeout(CACHE_IO_TIMEOUT, write_cache(path, &current))
                            .await
                        {
                            Ok(Ok(())) => {}
                            error => {
                                tracing::debug!(
                                    ?error,
                                    "announcement translation cache write skipped"
                                )
                            }
                        }
                    }
                }
                Ok(Ok(None)) => {}
                error => tracing::debug!(
                    ?error,
                    "announcement translation refresh skipped; keeping current catalog"
                ),
            }
        };
        tokio::select! {
            () = sender.closed() => return,
            () = check => {}
        }
        // Coalesce loads that arrived during this check (including cache IO).
        // Errors retry only when another official load starts, never on a timer.
        load_started.borrow_and_update();
    }
}

fn build_client() -> reqwest::Result<reqwest::Client> {
    xai_grok_extra_ca::build_reqwest_client(|builder| {
        builder
            .user_agent("grok-build-zh-announcement-translations")
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
    })
}

async fn read_response(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response.status().is_success(),
        "unexpected catalog response status"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= max as u64),
        "catalog response exceeds size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= max.saturating_sub(bytes.len()),
            "catalog response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn refresh_catalog(
    client: &reqwest::Client,
    base_url: &str,
    current: &TranslationCatalog,
) -> Result<Option<TranslationCatalog>> {
    let bytes = read_response(
        client,
        &format!("{base_url}/manifest.json"),
        MAX_MANIFEST_BYTES,
    )
    .await?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    if manifest.version <= current.version() {
        if manifest.version == current.version() {
            ensure!(
                manifest.sha256 == current.manifest.sha256,
                "published catalog version was reused"
            );
        }
        // An older CDN response must not roll back a newer cached snapshot.
        return Ok(None);
    }
    let url = format!("{base_url}/catalogs/{}.json", manifest.version);
    let bytes = read_response(client, &url, MAX_CATALOG_BYTES).await?;
    TranslationCatalog::parse(manifest, &bytes).map(Some)
}

async fn read_cache(path: &Path) -> Result<TranslationCatalog> {
    let metadata = tokio::fs::metadata(path).await?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_CACHE_BYTES as u64,
        "invalid catalog cache file"
    );
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_CACHE_BYTES,
        "catalog cache exceeds size limit"
    );
    let cache: CachedCatalog = serde_json::from_slice(&bytes)?;
    TranslationCatalog::parse(cache.manifest, cache.catalog_json.as_bytes())
}

async fn write_cache(path: &Path, catalog: &TranslationCatalog) -> Result<()> {
    let path = path.to_owned();
    let bytes = catalog.cache_bytes()?;
    // Keep the entire atomic write and cleanup in one blocking job. Dropping
    // the async waiter on timeout/exit must not skip temporary-file cleanup.
    // As with tokio::fs, an already-started disk operation may finish later.
    tokio::task::spawn_blocking(move || write_cache_bytes(&path, &bytes))
        .await
        .context("announcement cache writer failed")?
}

fn write_cache_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().context("catalog cache has no parent")?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .context("catalog cache has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("could not atomically replace announcement translation cache")
}

#[cfg(test)]
mod tests;
