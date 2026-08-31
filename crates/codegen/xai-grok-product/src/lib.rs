//! Product identity for the unofficial Simplified Chinese community build.
//!
//! Keep distribution identity separate from UI localization. Protocol names,
//! server endpoints, model IDs, tool names, and wire fields must not depend on
//! this crate.

#![forbid(unsafe_code)]

/// Compile-time privacy switch, mirrored from `xai-grok-version`'s `privacy`
/// feature (default-on in this fork). Every privacy-policy constant below and
/// the display name follow it, so a single feature bit drives the whole
/// distribution: telemetry hard-off, research-upload forbiddance, retention
/// lock, and vendor-update refusal.
pub const PRIVACY_BUILD: bool = xai_grok_version::PRIVACY_BUILD;

/// Stable machine-readable identity used by packaging and release metadata.
pub const PRODUCT_ID: &str = "grok-build-zh";
/// Human-readable product name for client-owned UI chrome.
/// The privacy-build suffix mirrors `PRIVACY_BUILD` so the shipped binary
/// surfaces its hardened posture in `--version`, welcome copy, and the
/// settings UI.
pub const DISPLAY_NAME: &str = if PRIVACY_BUILD {
    "Grok Build 中文社区版（隐私构建）"
} else {
    "Grok Build 中文社区版"
};
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

/// Repository that owns every update accepted by the community distribution.
pub const COMMUNITY_RELEASE_REPO: &str = "JoyElliot/grok-build-Chinese";
/// Canonical download page for the community distribution.
pub const COMMUNITY_RELEASES_URL: &str = "https://github.com/JoyElliot/grok-build-Chinese/releases";
/// The community updater uses immutable GitHub Releases from the repository
/// above. Release ZIPs are selected by an exact platform-specific name and
/// verified against GitHub metadata plus the package's inner hashes before
/// activation.
pub const AUTO_UPDATE_ENABLED: bool = true;
/// Default for the user-controlled `[cli].auto_update` setting.
///
/// Availability checks remain enabled so the welcome screen can offer an
/// update, but community builds do not download or install in the background
/// until the user explicitly opts in.
pub const AUTO_UPDATE_DEFAULT_ENABLED: bool = false;
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
    fn privacy_build_suffixes_display_name() {
        assert_eq!(
            PRIVACY_BUILD,
            xai_grok_version::PRIVACY_BUILD,
            "product policy must mirror the compile-time privacy switch"
        );
        assert_eq!(
            DISPLAY_NAME,
            if PRIVACY_BUILD {
                "Grok Build 中文社区版（隐私构建）"
            } else {
                "Grok Build 中文社区版"
            }
        );
        assert!(
            !PRIVACY_BUILD || DISPLAY_NAME.contains("隐私构建"),
            "privacy builds must advertise the hardened posture"
        );
    }

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
    fn updater_uses_only_the_community_release_source() {
        assert!(AUTO_UPDATE_ENABLED);
        assert!(!AUTO_UPDATE_DEFAULT_ENABLED);
        assert_eq!(COMMUNITY_RELEASE_REPO, "JoyElliot/grok-build-Chinese");
        assert_eq!(
            COMMUNITY_RELEASES_URL,
            "https://github.com/JoyElliot/grok-build-Chinese/releases"
        );
        assert!(!OFFICIAL_UPDATE_SOURCES_ALLOWED);
        assert!(!OFFICIAL_CHANGELOG_SOURCE_ALLOWED);
    }
}
