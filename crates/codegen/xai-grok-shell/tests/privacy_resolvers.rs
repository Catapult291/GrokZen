//! Privacy hard-off regression tests for telemetry / trace-upload resolvers.
//!
//! Integration tests (not `#[cfg(test)]` unit tests) so they compile against the
//! normal shell library.
//!
//! These drive the **shipped** `Config::resolve_telemetry_mode` /
//! `resolve_trace_upload` entry points with env, config, and remote settings
//! that would re-enable product telemetry on upstream Grok Build.
//!
//! 移植自 gork-build 0050-privacy-contract-tests，按 zh 1.0.12 API 适配
//! （feedback 门控从 `resolve_feedback()` 改为 `feature(Feature::Feedback)`）。

use serial_test::serial;
use xai_grok_config_types::RemoteSettings;
use xai_grok_shell::agent::config::{Config, Feature, TelemetryMode};

#[test]
#[serial]
fn privacy_build_telemetry_mode_ignores_env_config_and_remote() {
    assert!(
        xai_grok_version::research_data_collection_forbidden(),
        "this fork must lock research collection off"
    );
    // SAFETY: #[serial]
    unsafe {
        std::env::set_var("GROK_TELEMETRY_ENABLED", "1");
    }
    let mut cfg = Config::default();
    cfg.features.telemetry = Some(TelemetryMode::Enabled);
    cfg.requirements.telemetry.pin(
        TelemetryMode::Enabled,
        xai_grok_shell::config::RequirementSource::Unknown,
    );
    cfg.remote_settings = Some(RemoteSettings {
        telemetry_enabled: Some(true),
        telemetry_mode: Some("enabled".into()),
        ..Default::default()
    });
    let r = cfg.resolve_telemetry_mode();
    assert!(
        r.value.is_disabled(),
        "privacy hard-off must win over env/config/remote: mode={:?}",
        r.value
    );
    unsafe {
        std::env::remove_var("GROK_TELEMETRY_ENABLED");
    }
}

#[test]
#[serial]
fn privacy_build_trace_upload_ignores_env_config_and_remote() {
    assert!(xai_grok_version::research_data_collection_forbidden());
    // SAFETY: #[serial]
    unsafe {
        std::env::set_var("GROK_TELEMETRY_ENABLED", "1");
        std::env::set_var("GROK_TELEMETRY_TRACE_UPLOAD", "1");
    }
    let mut cfg = Config::default();
    cfg.features.telemetry = Some(TelemetryMode::Enabled);
    cfg.telemetry.trace_upload = Some(true);
    cfg.requirements
        .trace_upload
        .pin(true, xai_grok_shell::config::RequirementSource::Unknown);
    cfg.remote_settings = Some(RemoteSettings {
        telemetry_enabled: Some(true),
        telemetry_mode: Some("enabled".into()),
        trace_upload_enabled: Some(true),
        ..Default::default()
    });
    let r = cfg.resolve_trace_upload();
    assert!(
        !r.value,
        "privacy hard-off must win over env/config/remote for trace upload"
    );
    assert!(!cfg.is_trace_upload_enabled());
    unsafe {
        std::env::remove_var("GROK_TELEMETRY_ENABLED");
        std::env::remove_var("GROK_TELEMETRY_TRACE_UPLOAD");
    }
}

#[test]
#[serial]
fn privacy_build_feedback_ignores_remote_and_defaults_off() {
    assert!(xai_grok_version::research_data_collection_forbidden());
    unsafe {
        std::env::remove_var("GROK_FEEDBACK_ENABLED");
    }
    // `[features] feedback = true` via the real config loader (config tier ON).
    let raw: toml::Value = toml::from_str("[features]\nfeedback = true\n").unwrap();
    let mut cfg = Config::new_from_toml_cfg(&raw).expect("config should parse");
    cfg.remote_settings = Some(RemoteSettings {
        feedback_enabled: Some(true),
        ..Default::default()
    });
    assert!(
        !cfg.feature(Feature::Feedback).value,
        "privacy hard-off must default feedback off and ignore remote/config"
    );
    unsafe {
        std::env::set_var("GROK_FEEDBACK_ENABLED", "1");
    }
    let on = Config::default().feature(Feature::Feedback);
    assert!(
        on.value,
        "explicit GROK_FEEDBACK_ENABLED=1 may still opt in: {:?}",
        on.value
    );
    unsafe {
        std::env::remove_var("GROK_FEEDBACK_ENABLED");
    }
}
