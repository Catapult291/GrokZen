//! HTTP download backend. Production uses [`UreqFetcher`]; tests inject mocks.

/// Fetch raw bytes for a URL. Implementations must not touch the process-global
/// environment beyond what the caller already configured.
pub trait Fetcher: Send + Sync {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Default HTTPS client used by [`super::ensure_installed`].
#[derive(Debug, Default)]
pub struct UreqFetcher {
    pub timeout_secs: u64,
}

impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let timeout = std::time::Duration::from_secs(if self.timeout_secs == 0 {
            60
        } else {
            self.timeout_secs
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| format!("HTTP GET {url}: {e}"))?;
        // Language-server archives exceed ureq's default 10 MB body limit.
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|e| format!("read body {url}: {e}"))?;
        Ok(bytes)
    }
}
