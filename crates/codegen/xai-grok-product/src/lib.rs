//! Product identity for the unofficial Simplified Chinese community build.
//!
//! Keep distribution identity separate from UI localization. Protocol names,
//! server endpoints, model IDs, tool names, and wire fields must not depend on
//! this crate.

#![forbid(unsafe_code)]

/// Stable machine-readable identity used by packaging and release metadata.
pub const PRODUCT_ID: &str = "grok-build-zh";
/// Human-readable product name for client-owned UI chrome.
pub const DISPLAY_NAME: &str = "Grok Build 中文社区版";
/// Command and executable stem for the community distribution.
pub const CLI_NAME: &str = "grok-zh";
/// Shared per-user data directory, relative to the user's home directory.
///
/// The official and Simplified Chinese executables intentionally use the same
/// sessions, credentials, configuration, plugins, caches, and local state.
pub const DATA_DIR_NAME: &str = ".grok";
/// Shared user-data override used by both the official and Chinese executables.
pub const HOME_ENV: &str = "GROK_HOME";
/// Distribution-specific UI locale override.
pub const LOCALE_ENV: &str = "GROK_ZH_LOCALE";
/// Default UI locale for this distribution.
pub const DEFAULT_UI_LOCALE: &str = "zh-CN";

/// The community updater remains disabled until a signed, fork-owned manifest
/// and artifact source are configured. It must never fall back to xAI sources.
pub const AUTO_UPDATE_ENABLED: bool = false;
/// Whether the official npm/GitHub/CDN/GCS update sources may be consulted.
pub const OFFICIAL_UPDATE_SOURCES_ALLOWED: bool = false;
/// Whether release notes may be fetched from the official xAI changelog CDN.
pub const OFFICIAL_CHANGELOG_SOURCE_ALLOWED: bool = false;

/// Executable filename for the current platform.
pub const fn executable_name() -> &'static str {
    if cfg!(windows) {
        "grok-zh.exe"
    } else {
        CLI_NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_ui_identity_uses_the_shared_official_data_home() {
        assert_eq!(PRODUCT_ID, "grok-build-zh");
        assert_eq!(DATA_DIR_NAME, ".grok");
        assert_eq!(HOME_ENV, "GROK_HOME");
        assert_eq!(LOCALE_ENV, "GROK_ZH_LOCALE");
        assert_ne!(
            executable_name(),
            if cfg!(windows) { "grok.exe" } else { "grok" }
        );
    }

    #[test]
    fn updater_is_fail_closed_until_a_trusted_source_exists() {
        assert!(!AUTO_UPDATE_ENABLED);
        assert!(!OFFICIAL_UPDATE_SOURCES_ALLOWED);
        assert!(!OFFICIAL_CHANGELOG_SOURCE_ALLOWED);
    }
}
