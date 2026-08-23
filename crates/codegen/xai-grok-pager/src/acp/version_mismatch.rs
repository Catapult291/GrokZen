//! Decode `x.ai/leader/version_mismatch` into toast/log text.
//!
//! Versions are scrubbed before the usability check so control-only values are
//! rejected instead of toasted. The full banner then goes through
//! `sanitize_toast_message` so chrome glyphs (`⚠`) get ConHost fallback, not
//! just the interpolated versions. Wire `message` is ignored; the restart hint
//! is client-owned.

use std::borrow::Cow;

use crate::glyphs::sanitize_toast_message;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionMismatchParams<'a> {
    #[serde(borrow)]
    client_version: Option<Cow<'a, str>>,
    #[serde(borrow)]
    leader_version: Option<Cow<'a, str>>,
}

/// ASCII marker that survives `sanitize_toast_message` glyph fallback
/// (`⚠` → `!` on legacy ConHost).
const VERSION_MISMATCH_MARKER: &str = "Version mismatch:";
const VERSION_MISMATCH_MARKER_ZH: &str = "版本不一致：";

pub(crate) fn version_mismatch_banner(params: &str) -> Option<String> {
    version_mismatch_banner_with_locale(params, None)
}

pub(crate) fn version_mismatch_banner_with_locale(
    params: &str,
    locale: Option<&crate::locale::LocaleContext>,
) -> Option<String> {
    let parsed: VersionMismatchParams<'_> = serde_json::from_str(params).ok()?;
    let client = sanitize_toast_message(parsed.client_version.as_deref()?);
    let leader = sanitize_toast_message(parsed.leader_version.as_deref()?);
    if client.chars().all(char::is_whitespace) || leader.chars().all(char::is_whitespace) {
        return None;
    }
    let template = locale
        .map(|locale| {
            locale.named_text(
                "acp.version_mismatch",
                "⚠ Version mismatch: client {client_version}, leader {leader_version}. Restart grok to match",
            )
        })
        .unwrap_or_else(|| {
            Cow::Borrowed(
                "⚠ Version mismatch: client {client_version}, leader {leader_version}. Restart grok to match",
            )
        });
    let banner = template
        .replace("{client_version}", &client)
        .replace("{leader_version}", &leader);
    Some(sanitize_toast_message(&banner).into_owned())
}

pub(crate) fn is_version_mismatch_banner(msg: &str) -> bool {
    msg.contains(VERSION_MISMATCH_MARKER) || msg.contains(VERSION_MISMATCH_MARKER_ZH)
}

#[cfg(test)]
#[path = "version_mismatch_tests.rs"]
mod tests;
