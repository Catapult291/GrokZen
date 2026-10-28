//! Compaction configuration and runtime state for the session actor.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// Holder count (not a bool): nested compact scopes share one token.
/// The first `enter` installs a token; nested enters reuse it; `in_flight` stays true until the last scope drops.
/// A normal turn stop is a no-op when idle.
#[derive(Default)]
pub(crate) struct CompactCancelGate {
    token: RefCell<tokio_util::sync::CancellationToken>,
    holders: AtomicUsize,
}

/// Decrements the holder count when a compact scope ends.
pub(crate) struct CompactCancelScope<'a>(&'a CompactCancelGate);

impl Drop for CompactCancelScope<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

impl CompactCancelGate {
    /// Start or join a compact scope.
    /// Nested callers share one token, including a token already cancelled by stop, so overlapping compactions both observe the same abort.
    /// A later independent enter after holders drain installs a fresh token.
    pub(crate) fn enter(&self) -> (tokio_util::sync::CancellationToken, CompactCancelScope<'_>) {
        let prev = self.holders.fetch_add(1, Ordering::AcqRel);
        let token = if prev == 0 {
            let token = tokio_util::sync::CancellationToken::new();
            self.token.replace(token.clone());
            token
        } else {
            self.token.borrow().clone()
        };
        (token, CompactCancelScope(self))
    }

    fn end(&self) {
        self.holders.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn request_cancel(&self) {
        if self.holders.load(Ordering::Acquire) > 0 {
            self.token.borrow().cancel();
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.holders.load(Ordering::Acquire) > 0 && self.token.borrow().is_cancelled()
    }
}

pub(crate) struct CompactionConfig {
    /// Locks the context window when `GROK_DEBUG_CONTEXT_WINDOW` is set.
    pub context_window_override: Option<std::num::NonZeroU64>,
    /// Shared with `SessionHandle::context_window_selection`.
    pub context_window_selection: Arc<AtomicU64>,
    pub count: AtomicU64,
    /// The resolved mode; `Segments` carries its detail level inline.
    pub compaction_mode: xai_chat_state::CompactionMode,
    /// When `true`, feed the summarizer the verbatim conversation instead of the lossy rewrite (the retry loop may still fall back).
    pub verbatim_input: bool,
    pub tool_choice: crate::util::config::CompactionToolChoice,
    /// Sticky once a forked session releases its inherited prefix under compaction pressure (see `run_compact_inner`), so it stops re-pinning it.
    pub prefix_released: AtomicBool,
    /// User/stop cancel for the current compact generation.
    pub cancel: CompactCancelGate,
}

#[cfg(test)]
mod compact_cancel_gate_tests {
    use super::*;

    #[test]
    fn request_cancel_trips_shared_token() {
        let gate = CompactCancelGate::default();
        let (token, _scope) = gate.enter();
        assert!(!token.is_cancelled());
        gate.request_cancel();
        assert!(token.is_cancelled());
        assert!(gate.is_cancelled());
    }

    #[test]
    fn request_cancel_is_noop_when_idle() {
        let gate = CompactCancelGate::default();
        gate.request_cancel();
        let (token, _scope) = gate.enter();
        assert!(!token.is_cancelled());
        assert!(!gate.is_cancelled());
    }

    #[test]
    fn nested_enter_keeps_in_flight_after_inner_drop() {
        let gate = CompactCancelGate::default();
        let (outer_tok, outer) = gate.enter();
        let (inner_tok, inner) = gate.enter();
        gate.request_cancel();
        assert!(outer_tok.is_cancelled());
        assert!(inner_tok.is_cancelled());
        drop(inner);
        assert!(gate.is_cancelled());
        drop(outer);
        assert!(!gate.is_cancelled());
        let (next, _scope) = gate.enter();
        assert!(!next.is_cancelled());
    }

    #[test]
    fn join_while_cancelled_reuses_cancelled_token() {
        let gate = CompactCancelGate::default();
        let (_outer, outer) = gate.enter();
        gate.request_cancel();
        let (joined, joined_scope) = gate.enter();
        assert!(
            joined.is_cancelled(),
            "nested enter during stop must keep sharing the cancelled token"
        );
        drop(joined_scope);
        drop(outer);
        let (next, _scope) = gate.enter();
        assert!(
            !next.is_cancelled(),
            "fresh enter after scopes drain must not inherit the prior stop"
        );
    }
}
