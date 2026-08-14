pub mod auto_update;
#[cfg(feature = "community-build")]
mod community_release;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;

/// User-facing reason returned when the selected distribution source is not
/// enabled. Community builds never use the official xAI update sources.
#[cfg(feature = "community-build")]
pub const UPDATE_DISABLED_REASON: &str = "此版本未启用所选的更新来源。";
#[cfg(not(feature = "community-build"))]
pub const UPDATE_DISABLED_REASON: &str =
    "The selected update source is disabled for this distribution.";

/// Compile-time distribution policy. Keeping this in one leaf crate prevents
/// an upstream updater refactor from accidentally re-enabling official sources.
pub const fn updates_enabled() -> bool {
    if cfg!(feature = "community-build") {
        community_updates_enabled()
    } else {
        true
    }
}

/// Effective default for `[cli].auto_update` in the selected distribution.
/// Official builds retain their upstream default; the community build is
/// opt-in while still allowing metadata-only availability checks.
pub const fn default_auto_update_enabled() -> bool {
    if cfg!(feature = "community-build") {
        xai_grok_product::AUTO_UPDATE_DEFAULT_ENABLED
    } else {
        true
    }
}

/// The upstream updater implementation only contains official xAI backends.
/// Community builds keep them unavailable even after their own updater exists.
pub const fn official_update_sources_allowed() -> bool {
    !cfg!(feature = "community-build") || xai_grok_product::OFFICIAL_UPDATE_SOURCES_ALLOWED
}

/// Whether this binary is the community distribution with its fixed Releases
/// backend enabled.
pub const fn community_updates_enabled() -> bool {
    cfg!(feature = "community-build")
        && xai_grok_product::AUTO_UPDATE_ENABLED
        && cfg!(all(
            target_os = "windows",
            target_arch = "x86_64",
            target_env = "gnu"
        ))
}

pub(crate) fn ensure_updates_enabled() -> anyhow::Result<()> {
    if !updates_enabled() || !official_update_sources_allowed() {
        anyhow::bail!(UPDATE_DISABLED_REASON);
    }
    Ok(())
}

pub(crate) fn ensure_community_updates_enabled() -> anyhow::Result<()> {
    if !community_updates_enabled() {
        anyhow::bail!(UPDATE_DISABLED_REASON);
    }
    Ok(())
}

/// Gate the backend selected at compile time. Official helper entry points keep
/// using [`ensure_updates_enabled`] so community builds cannot reach them.
pub(crate) fn ensure_selected_updates_enabled() -> anyhow::Result<()> {
    if cfg!(feature = "community-build") {
        ensure_community_updates_enabled()
    } else {
        ensure_updates_enabled()
    }
}

#[cfg(all(test, feature = "community-build"))]
mod community_build_tests {
    use super::*;

    #[test]
    fn community_build_enables_only_its_fixed_release_source() {
        let supported_target = cfg!(all(
            target_os = "windows",
            target_arch = "x86_64",
            target_env = "gnu"
        ));
        assert_eq!(updates_enabled(), supported_target);
        assert!(!default_auto_update_enabled());
        assert_eq!(community_updates_enabled(), supported_target);
        assert!(!official_update_sources_allowed());
        assert_eq!(ensure_community_updates_enabled().is_ok(), supported_target);
        assert_eq!(
            ensure_updates_enabled().unwrap_err().to_string(),
            UPDATE_DISABLED_REASON
        );
    }

    #[tokio::test]
    async fn official_download_is_blocked_and_fixed_installer_is_selected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("must-not-exist");
        let error =
            auto_update::download_silent("http://127.0.0.1:1/must-not-be-requested", &destination)
                .await
                .expect_err("community download must be disabled");
        assert_eq!(error.to_string(), UPDATE_DISABLED_REASON);
        assert!(!destination.exists());
        let expected_installer =
            community_updates_enabled().then_some(community_release::COMMUNITY_INSTALLER);
        assert_eq!(auto_update::get_installer().await, expected_installer);
    }
}
