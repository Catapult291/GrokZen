//! Final rendering: host-side truncation and policy notes.
//!
//! The engine already renders the body; this layer only enforces the host's
//! byte ceiling and appends the notes a model needs to interpret the result
//! (truncation, hidden files). Metadata stays at the top of the engine's own
//! text, so a tail truncation never removes `next_offset`.

/// Append-free finalisation of one tool result.
pub fn finalize(text: String, hidden: usize, is_error: bool, budget_bytes: usize) -> String {
    let mut text = text.trim_end().to_string();
    if is_error && text.is_empty() {
        text = "(no result)".to_string();
    }
    if hidden > 0 {
        text.push_str(&format!(
            "\n\nnote: {hidden} result line(s) hidden by the workspace read policy (DenyReadGlobs)."
        ));
    }
    truncate(text, budget_bytes)
}

/// Cut `text` to `budget_bytes` on a character boundary and say so. Keeps a
/// margin for the notice itself so the notice is never the thing that spills.
pub fn truncate(text: String, budget_bytes: usize) -> String {
    if budget_bytes == 0 || text.len() <= budget_bytes {
        return text;
    }
    const NOTICE_RESERVE: usize = 200;
    let limit = budget_bytes.saturating_sub(NOTICE_RESERVE);
    let mut cut = limit.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let omitted = text.len().saturating_sub(cut);
    let mut out = text[..cut].to_string();
    out.push_str(&format!(
        "\n\n… output truncated by host ({omitted} bytes omitted); narrow the query with path/glob, \
         or continue with offset= from the last line above."
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_passes_through() {
        assert_eq!(finalize("ok".into(), 0, false, 1000), "ok");
    }

    #[test]
    fn hidden_lines_are_reported() {
        let out = finalize("hit".into(), 2, false, 1000);
        assert!(out.contains("2 result line(s) hidden"));
    }

    #[test]
    fn truncation_reports_the_omitted_size() {
        let long = "x".repeat(500);
        let out = truncate(long, 300);
        assert!(out.contains("output truncated by host"));
        assert!(out.len() <= 320);
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let long = "命中".repeat(200);
        let out = truncate(long, 301);
        assert!(out.contains("output truncated"));
        // Must be valid UTF-8 (the construction would panic on a bad cut).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn zero_budget_means_no_truncation() {
        let text = "y".repeat(100);
        assert_eq!(truncate(text.clone(), 0), text);
    }
}
