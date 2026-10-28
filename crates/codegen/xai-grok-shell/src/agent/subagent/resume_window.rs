//! Resume admission for a copied child transcript.
//!
//! Spawn injects the system prompt, AGENTS.md, and the resume task before the
//! first compact can run, so a copy that fills the window is not safe.

/// Spawn injects prelude tokens before the first compact can shrink history.
const RESUME_WINDOW_FIT_PERCENT: u8 = 95;

/// Child context window used for resume admission.
pub(super) struct ResumeWindowPolicy {
    pub context_window: u64,
}

impl ResumeWindowPolicy {
    pub(super) fn token_limit(&self) -> u64 {
        self.context_window * u64::from(RESUME_WINDOW_FIT_PERCENT) / 100
    }

    pub(super) fn fits(&self, estimated_tokens: u64) -> bool {
        self.context_window != 0 && estimated_tokens <= self.token_limit()
    }
}

/// Leading items to preserve across compaction on resume: the System head only, so the resumed body (the child's own work) stays compactable.
/// Returns 0 when there's no leading System; the spawn path then inserts one and bumps the prefix to 1.
pub(super) fn resume_inherited_prefix_len(
    conversation: &[xai_grok_sampling_types::conversation::ConversationItem],
) -> usize {
    use xai_grok_sampling_types::conversation::ConversationItem;

    conversation
        .iter()
        .take_while(|item| matches!(item, ConversationItem::System(_)))
        .count()
}

#[cfg(test)]
#[path = "resume_window_tests.rs"]
mod tests;
