pub mod auto_update;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;

/// User-facing reason returned by every update entry point until the community
/// distribution has a signed, fork-owned manifest and artifact source.
pub const UPDATE_DISABLED_REASON: &str =
    "The community build updater is disabled until a signed community update source is configured.";

/// Compile-time distribution policy. Keeping this in one leaf crate prevents
/// an upstream updater refactor from accidentally re-enabling official sources.
pub const fn updates_enabled() -> bool {
    !cfg!(feature = "community-build") || xai_grok_product::AUTO_UPDATE_ENABLED
}

/// The upstream updater implementation only contains official xAI backends.
/// Community builds keep them unavailable even after their own updater exists.
pub const fn official_update_sources_allowed() -> bool {
    !cfg!(feature = "community-build") || xai_grok_product::OFFICIAL_UPDATE_SOURCES_ALLOWED
}

pub(crate) fn ensure_updates_enabled() -> anyhow::Result<()> {
    if !updates_enabled() || !official_update_sources_allowed() {
        anyhow::bail!(UPDATE_DISABLED_REASON);
    }
    Ok(())
}

#[cfg(all(test, feature = "community-build"))]
mod community_build_tests {
    use super::*;

    #[test]
    fn community_build_fails_closed_before_using_official_sources() {
        assert!(!updates_enabled());
        assert!(!official_update_sources_allowed());
        assert_eq!(
            ensure_updates_enabled().unwrap_err().to_string(),
            UPDATE_DISABLED_REASON
        );
    }

    #[tokio::test]
    async fn community_entry_points_do_not_download_or_mutate_channel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("must-not-exist");
        let error =
            auto_update::download_silent("http://127.0.0.1:1/must-not-be-requested", &destination)
                .await
                .expect_err("community download must be disabled");
        assert_eq!(error.to_string(), UPDATE_DISABLED_REASON);
        assert!(!destination.exists());

        let mut config = UpdateConfig {
            proxy_base_url: "http://127.0.0.1:1".to_string(),
            auth_scope: "test".to_string(),
            deployment_key: None,
            alpha_test_key: None,
            channel: "stable".to_string(),
            npm_registry: None,
        };
        auto_update::apply_channel_switch(Some("alpha"), &mut config).await;
        assert_eq!(config.channel, "stable");
    }
}
