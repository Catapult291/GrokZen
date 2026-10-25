//! Session-gated language-server installation.
//!
//! Installation is never a side effect of discovery, indexing, or a precise
//! query. The MCP catalog no longer exposes an install tool: `get_languages`
//! reports Ready / needs_install / AST-only, and the operator installs the
//! named server locally. [`LanguageServerInstaller::install`] remains the
//! gated entrypoint for a future installer.
//!
//! # Status
//!
//! The artifact downloader / checksum verifier is not merged yet. [`StubInstaller`]
//! implements the prompt contract (plan + gated install entrypoint) and returns
//! [`InstallError::NotImplemented`] from `install`. Replace the stub when the
//! core installer lands — the MCP tool should keep calling this trait.
//!
//! See `docs/RFC-P0-language-matrix.md` §2–§3.

use std::path::PathBuf;

use crate::types::Language;

use super::discovery::{Discovery, ProbeResult, PROBED_LANGUAGES};
use super::router::{install_hint_for, primary_server_name};

/// How version is chosen for an install request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionPolicy {
    /// Resolve the latest supported upstream release (RFC default).
    Latest,
    /// Pin an explicit upstream version; must not float to latest.
    Pinned(String),
}

impl VersionPolicy {
    pub fn from_pin(pin: Option<&str>) -> Self {
        match pin.map(str::trim).filter(|s| !s.is_empty()) {
            Some(v) => VersionPolicy::Pinned(v.to_string()),
            None => VersionPolicy::Latest,
        }
    }

    pub fn label(&self) -> String {
        match self {
            VersionPolicy::Latest => "latest".to_string(),
            VersionPolicy::Pinned(v) => format!("pinned:{v}"),
        }
    }
}

/// Readiness of a language for precise (LSP) tools vs AST/graph-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspStatus {
    /// A server binary is on PATH (or override) and can be started.
    Ready {
        server_name: String,
        command: PathBuf,
    },
    /// Language has an install path; binary is missing. Session must confirm
    /// before [`LanguageServerInstaller::install`].
    NeedsInstall {
        server_name: String,
        install_hint: String,
    },
    /// Indexed/parsed, but no language-server probe/install path is wired yet.
    AstOnly { reason: String },
}

impl LspStatus {
    /// Stable token for MCP / `get_languages` rows: `Ready` / `needs_install` / `AST-only`.
    pub fn token(&self) -> &'static str {
        match self {
            LspStatus::Ready { .. } => "Ready",
            LspStatus::NeedsInstall { .. } => "needs_install",
            LspStatus::AstOnly { .. } => "AST-only",
        }
    }

    /// One-line suffix after LOC counts in `get_languages`.
    pub fn summary_suffix(&self) -> String {
        match self {
            LspStatus::Ready {
                server_name,
                command,
            } => format!(" — Ready ({server_name} @ {})", command.display()),
            LspStatus::NeedsInstall {
                server_name,
                install_hint: _,
            } => format!(" — needs_install ({server_name}; install this language server locally)"),
            LspStatus::AstOnly { reason } => format!(" — AST-only ({reason})"),
        }
    }
}

/// Plan returned when `confirm_install` is false (or when already ready).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPlan {
    pub language: Language,
    pub server_name: String,
    pub version_policy: VersionPolicy,
    /// Approximate download size in bytes when known; `None` until the
    /// release catalog lands.
    pub approximate_size_bytes: Option<u64>,
    pub install_hint: String,
    pub status: LspStatus,
}

impl InstallPlan {
    /// Human-readable body for MCP (no download occurs for this rendering).
    pub fn render(&self) -> String {
        let size = match self.approximate_size_bytes {
            Some(n) => format!("~{n} bytes"),
            None => "size unknown (catalog not wired)".to_string(),
        };
        let mut body = format!(
            "ensure_language_server plan\n\
             language: {}\n\
             server: {}\n\
             version_policy: {}\n\
             approximate_size: {size}\n\
             status: {}\n",
            self.language.name(),
            self.server_name,
            self.version_policy.label(),
            self.status.token(),
        );
        match &self.status {
            LspStatus::Ready { command, .. } => {
                body.push_str(&format!(
                    "already available at {}; no download needed.\n",
                    command.display()
                ));
            }
            LspStatus::NeedsInstall { .. } => {
                body.push_str(
                    "needs_install: no download performed.\n\
                     next_step: ask the user for permission, then call again with \
                     confirm_install=true.\n",
                );
                body.push_str(&format!("install_hint: {}\n", self.install_hint));
            }
            LspStatus::AstOnly { reason } => {
                body.push_str(&format!(
                    "AST-only: cannot install via this tool yet ({reason}).\n"
                ));
            }
        }
        body
    }
}

/// Record of a completed (or attempted) install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallRecord {
    pub language: Language,
    pub server_name: String,
    pub version: String,
    pub command: Option<PathBuf>,
    pub note: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InstallError {
    #[error("unknown language `{0}` for language-server install")]
    UnknownLanguage(String),
    #[error("language `{0}` is AST-only: {1}")]
    AstOnly(String, String),
    #[error("install declined or confirm_install not set")]
    NotConfirmed,
    /// Core artifact installer not merged yet. MCP still ships the prompt contract.
    #[error(
        "TODO: language-server installer not merged yet; plan accepted but no download performed \
         (server={0}, version_policy={1})"
    )]
    NotImplemented(String, String),
    #[error("install failed: {0}")]
    Failed(String),
}

/// Session-gated installer. Implementations must not download in [`Self::plan`].
pub trait LanguageServerInstaller: Send + Sync {
    /// Build a plan from local discovery. Never downloads.
    fn plan(
        &self,
        language: Language,
        version_pin: Option<&str>,
    ) -> Result<InstallPlan, InstallError>;

    /// Perform the install for a previously returned plan. Only called when the
    /// session has confirmed (`confirm_install=true` in MCP).
    fn install(&self, plan: &InstallPlan) -> Result<InstallRecord, InstallError>;
}

/// Probe-backed stub installer. Plans are real; `install` is TODO.
#[derive(Clone, Debug)]
pub struct StubInstaller {
    discovery: Discovery,
}

impl Default for StubInstaller {
    fn default() -> Self {
        Self::new(Discovery::new(""))
    }
}

impl StubInstaller {
    pub fn new(discovery: Discovery) -> Self {
        Self { discovery }
    }

    pub fn from_process() -> Self {
        Self::new(Discovery::from_process())
    }

    /// Map a probe row into [`LspStatus`]. Languages without candidates are
    /// [`LspStatus::AstOnly`] so `get_languages` can say so when new langs are
    /// added to the enum before discovery/install are wired.
    pub fn status_for(&self, language: Language) -> LspStatus {
        status_from_probe(&self.discovery.probe(language))
    }

    /// Status rows for every currently probed language (order = [`PROBED_LANGUAGES`]).
    pub fn probed_statuses(&self) -> Vec<(Language, LspStatus)> {
        PROBED_LANGUAGES
            .iter()
            .copied()
            .map(|language| (language, self.status_for(language)))
            .collect()
    }
}

impl LanguageServerInstaller for StubInstaller {
    fn plan(
        &self,
        language: Language,
        version_pin: Option<&str>,
    ) -> Result<InstallPlan, InstallError> {
        let version_policy = VersionPolicy::from_pin(version_pin);
        let probe = self.discovery.probe(language);
        let status = status_from_probe(&probe);
        let server_name = primary_server_name(language).to_string();
        Ok(InstallPlan {
            language,
            server_name,
            version_policy,
            approximate_size_bytes: None,
            install_hint: probe.install_hint,
            status,
        })
    }

    fn install(&self, plan: &InstallPlan) -> Result<InstallRecord, InstallError> {
        match &plan.status {
            LspStatus::Ready { command, .. } => Ok(InstallRecord {
                language: plan.language,
                server_name: plan.server_name.clone(),
                version: plan.version_policy.label(),
                command: Some(command.clone()),
                note: "already installed; nothing downloaded".into(),
            }),
            LspStatus::AstOnly { reason } => Err(InstallError::AstOnly(
                plan.language.name().to_string(),
                reason.clone(),
            )),
            LspStatus::NeedsInstall { .. } => {
                // Prefer the artifact installer when a catalog spec exists.
                if let Some(mut spec) = super::discovery::catalog_install_spec(plan.language) {
                    if let VersionPolicy::Pinned(v) = &plan.version_policy {
                        spec.version = v.clone();
                    }
                    match super::installer::ensure_installed(&spec) {
                        Ok(command) => Ok(InstallRecord {
                            language: plan.language,
                            server_name: plan.server_name.clone(),
                            version: spec.version,
                            command: Some(command),
                            note: "installed via astrolabe lsp installer".into(),
                        }),
                        Err(err) => Err(InstallError::Failed(err.to_string())),
                    }
                } else {
                    Err(InstallError::NotImplemented(
                        plan.server_name.clone(),
                        plan.version_policy.label(),
                    ))
                }
            }
        }
    }
}

fn status_from_probe(probe: &ProbeResult) -> LspStatus {
    let server_name = primary_server_name(probe.language).to_string();
    match &probe.spec {
        Some(spec) => LspStatus::Ready {
            server_name,
            command: spec.command.clone(),
        },
        None => {
            // Every language in PROBED_LANGUAGES has candidates. If a future
            // language is added to the enum without discovery candidates,
            // treat empty-hint probes as AST-only.
            if probe.install_hint.is_empty()
                || probe.install_hint.contains("not wired")
                || probe.install_hint.contains("AST-only")
            {
                LspStatus::AstOnly {
                    reason: if probe.install_hint.is_empty() {
                        "language server probe not wired".into()
                    } else {
                        probe.install_hint.clone()
                    },
                }
            } else {
                LspStatus::NeedsInstall {
                    server_name,
                    install_hint: probe.install_hint.clone(),
                }
            }
        }
    }
}

/// Parse a user/MCP language token into [`Language`].
pub fn parse_language_name(raw: &str) -> Result<Language, InstallError> {
    let trimmed = raw.trim();
    // jsx is an alias Language::from_name does not cover.
    if trimmed.eq_ignore_ascii_case("jsx") {
        return Ok(Language::JavaScript);
    }
    Language::from_name(trimmed)
        .ok_or_else(|| InstallError::UnknownLanguage(trimmed.to_ascii_lowercase()))
}

/// Fallback hint text (same sources as discovery / router).
pub fn default_install_hint(language: Language) -> String {
    install_hint_for(language).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::discovery::Discovery;
    use std::path::PathBuf;

    fn empty_path_discovery() -> Discovery {
        Discovery::new("")
    }

    #[test]
    fn plan_without_binary_is_needs_install_and_does_not_download() {
        let installer = StubInstaller::new(empty_path_discovery());
        let plan = installer.plan(Language::Go, None).unwrap();
        assert_eq!(plan.server_name, "gopls");
        assert_eq!(plan.version_policy, VersionPolicy::Latest);
        assert!(matches!(plan.status, LspStatus::NeedsInstall { .. }));
        assert!(plan.render().contains("needs_install"));
        assert!(plan.render().contains("no download performed"));
        assert!(plan.approximate_size_bytes.is_none());
    }

    #[test]
    fn confirm_install_hits_stub_todo() {
        let installer = StubInstaller::new(empty_path_discovery());
        let plan = installer.plan(Language::Rust, None).unwrap();
        let err = installer.install(&plan).unwrap_err();
        match err {
            InstallError::NotImplemented(server, policy) => {
                assert_eq!(server, "rust-analyzer");
                assert_eq!(policy, "latest");
            }
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    #[test]
    fn pinned_version_policy() {
        let policy = VersionPolicy::from_pin(Some("1.2.3"));
        assert_eq!(policy, VersionPolicy::Pinned("1.2.3".into()));
        assert_eq!(policy.label(), "pinned:1.2.3");
    }

    #[test]
    fn parse_language_name_accepts_aliases() {
        assert_eq!(parse_language_name("Go").unwrap(), Language::Go);
        assert_eq!(parse_language_name("ts").unwrap(), Language::TypeScript);
        assert!(parse_language_name("cobol").is_err());
    }

    #[test]
    fn status_token_strings_match_contract() {
        assert_eq!(
            LspStatus::Ready {
                server_name: "gopls".into(),
                command: PathBuf::from("/usr/bin/gopls"),
            }
            .token(),
            "Ready"
        );
        assert_eq!(
            LspStatus::NeedsInstall {
                server_name: "gopls".into(),
                install_hint: "x".into(),
            }
            .token(),
            "needs_install"
        );
        assert_eq!(
            LspStatus::AstOnly {
                reason: "not wired".into(),
            }
            .token(),
            "AST-only"
        );
    }
}
