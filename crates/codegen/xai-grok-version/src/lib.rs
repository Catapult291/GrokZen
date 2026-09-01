//! Installed grok CLI version, kept in sync with the shipping binaries.
//!
//! **隐私构建（privacy build）**是中文社区版的可选加固形态：不做产品遥测、
//! 不上传研究轨迹、不向官方源上报。模型推理仍使用用户自己的凭据访问 Grok
//! API —— 这是 agent 正常工作所需的唯一网络路径。

use std::sync::OnceLock;

use semver::Version;

/// 编译期隐私开关。由 `privacy` cargo feature（本 fork 默认开启）驱动，而非
/// 直接硬编码 `true` 字面量，避免上游版本字符串的编辑与策略位冲突。
pub const PRIVACY_BUILD: bool = cfg!(feature = "privacy");

/// `true` 时研究遥测、Mixpanel、GCS 会话轨迹等非推理上传必须保持关闭。
/// 当 [`PRIVACY_BUILD`] 为 `true` 时恒为 `true`。
#[inline]
pub fn research_data_collection_forbidden() -> bool {
    PRIVACY_BUILD
}

/// `true` 时编码数据保留锁定为 **opt-out**（没有任何 UI/API 路径可以把账号
/// 切回共享/训练保留）。
#[inline]
pub fn coding_data_retention_locked_opt_out() -> bool {
    PRIVACY_BUILD
}

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

pub const VERSION: &str = match option_env!("GROK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The release pipeline always injects `GROK_VERSION`; without it the build is from source.
pub const IS_DEV_BUILD: bool = option_env!("GROK_VERSION").is_none();

/// Runtime-injected `"<version> (<shortcommit>)"` string.
/// Only the release binary stamps the commit in its own build.rs and injects it here at startup, so the lib crates don't recompile on every commit.
static FULL_VERSION: OnceLock<&'static str> = OnceLock::new();

/// Inject the binary's stamped `"<version> (<shortcommit>)"` string.
/// Idempotent: the first set wins, repeats are ignored.
pub fn set_full_version(v: &'static str) {
    let _ = FULL_VERSION.set(v);
}

/// The injected version-with-commit string, or plain [`VERSION`] when no binary has called [`set_full_version`] (e.g. lib tests, dev harnesses).
pub fn full_version() -> &'static str {
    FULL_VERSION.get().copied().unwrap_or(VERSION)
}

/// Returns the [`TEST_VERSION_ENV`] override when set, otherwise [`VERSION`].
/// The env value is trimmed so non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| VERSION.to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// Formats the compiled version with a channel label for user-facing display, e.g. `"0.2.5 [stable]"`.
/// `channel_label` is pre-formatted by `xai_grok_update::channel_label()`: `" [alpha]"`, `" [stable]"`, or `""` when no pointer is cached.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", VERSION, channel_label)
}

/// Like [`display_version`], but for the full `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 隐私构建策略常量 —— resolver 与 updater 依赖的编译期硬开关。
    /// 本 fork 发布时这些必须保持为真。
    #[test]
    fn privacy_build_locks_research_and_retention_policy() {
        assert!(
            PRIVACY_BUILD,
            "privacy build must ship with the `privacy` feature (PRIVACY_BUILD)"
        );
        assert!(
            research_data_collection_forbidden(),
            "research_data_collection_forbidden must follow PRIVACY_BUILD"
        );
        assert!(
            coding_data_retention_locked_opt_out(),
            "coding data retention must be locked opt-out under PRIVACY_BUILD"
        );
    }

    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (version, label, expected) in cases {
            assert_eq!(display_version_with_commit(version, label), *expected);
        }
        assert_eq!(display_version(""), VERSION);
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }

    #[test]
    fn strict_semver_rejects_four_part_versions() {
        assert!(Version::parse("1.0.3").is_ok());
        assert!(Version::parse("1.0.3-alpha.1").is_ok());
        assert!(Version::parse("1.0.0.1").is_err());
    }

    #[test]
    fn full_version_falls_back_then_first_set_wins() {
        assert_eq!(full_version(), VERSION);
        set_full_version("first (aaaaaaa)");
        assert_eq!(full_version(), "first (aaaaaaa)");
        set_full_version("second (bbbbbbb)");
        assert_eq!(full_version(), "first (aaaaaaa)");
    }
}
