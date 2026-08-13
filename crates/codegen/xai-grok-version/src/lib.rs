//! Installed grok CLI version, lockstepped with shipping binaries.

use std::cmp::Ordering;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use semver::Version;

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

pub const VERSION: &str = match option_env!("GROK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// [`TEST_VERSION_ENV`] override first, then [`VERSION`]. Trimmed so
/// non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| VERSION.to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// A release version understood by this distribution.
///
/// Standard SemVer remains fully supported.  The Chinese community build also
/// permits a stable fourth numeric component (`A.B.C.N`) as its revision of
/// the corresponding upstream `A.B.C` release.  Revision zero is represented
/// canonically as `A.B.C`, never as `A.B.C.0`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReleaseVersion {
    semver: Version,
    revision: Option<NonZeroU64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseVersionParseError {
    input: String,
}

impl fmt::Display for ReleaseVersionParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid release version {:?}; expected A.B.C, A.B.C-prerelease, or A.B.C.N with N > 0",
            self.input
        )
    }
}

impl std::error::Error for ReleaseVersionParseError {}

impl ReleaseVersion {
    pub fn parse(input: &str) -> Result<Self, ReleaseVersionParseError> {
        input.parse()
    }

    pub fn is_prerelease(&self) -> bool {
        !self.semver.pre.is_empty()
    }

    pub fn revision(&self) -> u64 {
        self.revision.map(NonZeroU64::get).unwrap_or(0)
    }

    pub fn as_semver(&self) -> &Version {
        &self.semver
    }
}

impl From<Version> for ReleaseVersion {
    fn from(semver: Version) -> Self {
        Self {
            semver,
            revision: None,
        }
    }
}

impl FromStr for ReleaseVersion {
    type Err = ReleaseVersionParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || ReleaseVersionParseError {
            input: input.to_string(),
        };
        if input.is_empty() || input.trim() != input {
            return Err(invalid());
        }

        if let Ok(semver) = Version::parse(input) {
            return Ok(Self {
                semver,
                revision: None,
            });
        }

        // Cargo and the semver crate intentionally reject four numeric core
        // components.  Accept exactly one additional *stable* revision here;
        // pre-release/build suffixes remain standard three-component SemVer.
        let components: Vec<&str> = input.split('.').collect();
        if components.len() != 4
            || components.iter().any(|part| {
                part.is_empty()
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                    || (part.len() > 1 && part.starts_with('0'))
            })
        {
            return Err(invalid());
        }
        let revision = components[3]
            .parse::<u64>()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(invalid)?;
        let semver = Version::parse(&components[..3].join(".")).map_err(|_| invalid())?;
        Ok(Self {
            semver,
            revision: Some(revision),
        })
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(revision) = self.revision {
            write!(
                formatter,
                "{}.{}.{}.{}",
                self.semver.major, self.semver.minor, self.semver.patch, revision
            )
        } else {
            self.semver.fmt(formatter)
        }
    }
}

impl Ord for ReleaseVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = (self.semver.major, self.semver.minor, self.semver.patch).cmp(&(
            other.semver.major,
            other.semver.minor,
            other.semver.patch,
        ));
        if core != Ordering::Equal {
            return core;
        }

        match (self.is_prerelease(), other.is_prerelease()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (true, true) => self.semver.cmp(&other.semver),
            (false, false) => self
                .revision()
                .cmp(&other.revision())
                .then_with(|| self.semver.cmp(&other.semver)),
        }
    }
}

impl PartialOrd for ReleaseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn installed_release_version() -> Result<ReleaseVersion, ReleaseVersionParseError> {
    ReleaseVersion::parse(&installed())
}

/// Parse versions at an updater source boundary. Community builds accept the
/// fourth numeric revision; official sources remain strict SemVer.
pub fn parse_distribution_version(
    input: &str,
    community_build: bool,
) -> Result<ReleaseVersion, ReleaseVersionParseError> {
    let parsed = ReleaseVersion::parse(input)?;
    if !community_build && parsed.revision().ne(&0) {
        return Err(ReleaseVersionParseError {
            input: input.to_string(),
        });
    }
    Ok(parsed)
}

/// Format the compiled version with a channel label for user-facing display.
///
/// `channel_label` is a pre-formatted suffix such as `" [alpha]"`, `" [stable]"`,
/// or `""` (empty when no cached pointer is available). Obtain it from
/// `xai_grok_update::channel_label()`.
///
/// Example: `"0.2.5 [stable]"` or `"0.2.5 [alpha]"`.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", VERSION, channel_label)
}

/// Format a version-with-commit string with a channel label.
///
/// Same semantics as [`display_version`] but for the full
/// `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_accepts_semver_and_numeric_community_revisions() {
        for (input, revision, prerelease) in [
            ("1.0.0", 0, false),
            ("1.0.0.1", 1, false),
            ("1.0.0.12", 12, false),
            ("1.0.0-alpha.2", 0, true),
            ("1.0.0-zh.ci.17", 0, true),
            ("1.0.0-zh.preview.10", 0, true),
        ] {
            let parsed = ReleaseVersion::parse(input).unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(parsed.revision(), revision);
            assert_eq!(parsed.is_prerelease(), prerelease);
        }
    }

    #[test]
    fn release_version_rejects_ambiguous_or_noncanonical_revisions() {
        for input in [
            "",
            " 1.0.0",
            "1.0.0 ",
            "v1.0.0",
            "1",
            "1.0",
            "1.0.0.0",
            "1.0.0.01",
            "1.0.0.1.2",
            "1.0.0.1-alpha.1",
            "1.0.0.18446744073709551616",
        ] {
            assert!(ReleaseVersion::parse(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn distribution_parser_keeps_official_sources_strict_semver() {
        assert!(parse_distribution_version("1.0.0", false).is_ok());
        assert!(parse_distribution_version("1.0.0-alpha.1", false).is_ok());
        assert!(parse_distribution_version("1.0.0.1", false).is_err());
        assert!(parse_distribution_version("1.0.0.1", true).is_ok());
    }

    #[test]
    fn release_version_orders_upstream_base_revision_and_prerelease() {
        let v = |input| ReleaseVersion::parse(input).unwrap();
        assert!(v("1.0.0-zh.ci.17") < v("1.0.0"));
        assert!(v("1.0.0") < v("1.0.0.1"));
        assert!(v("1.0.0.1") < v("1.0.0.2"));
        assert!(v("1.0.0.999") < v("1.0.1"));
        assert!(v("1.0.1-alpha.2") < v("1.0.1"));
        assert!(v("1.0.1-alpha.1") < v("1.0.1-alpha.2"));
        // Keep the semver crate's existing total ordering for build metadata.
        assert!(v("1.0.0+abc") < v("1.0.0+xyz"));
        assert!(v("1.0.0+xyz") < v("1.0.0.1"));
    }

    /// Display formatting invariant matrix — verifies label appending
    /// works correctly across all label states (alpha, stable, empty).
    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            // (version_with_commit,    label,        expected_suffix)
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (vwc, label, expected) in cases {
            assert_eq!(
                display_version_with_commit(vwc, label),
                *expected,
                "display_version_with_commit({:?}, {:?})",
                vwc,
                label,
            );
        }
        // display_version uses compiled VERSION — just verify the label appends
        assert_eq!(display_version(""), VERSION);
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }
}
