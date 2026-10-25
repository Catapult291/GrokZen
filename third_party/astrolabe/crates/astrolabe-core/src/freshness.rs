//! Query-time freshness barrier: a pure decision layer with no I/O.
//!
//! This module sits in a four-layer defense in depth:
//!
//! 1. **Event stream** — the watcher records filesystem events and a
//!    watermark of the last completed verification.
//! 2. **Conditional safety** — the indexer refuses to treat a file as
//!    unchanged when metadata cannot prove it.
//! 3. **This barrier** — before a tool answers, decide whether the index
//!    is fresh enough, whether a small suspect set can be repaired inline
//!    (`apply_changeset`), or whether the answer must be degraded and
//!    marked stale.
//! 4. **Model fallback** — the agent can re-query after a degraded
//!    answer; this module does not talk to the model.
//!
//! The barrier is a complete backstop for **positive** results (a hit
//! must not be served from a stale graph). Residual risk on **negative**
//! results — a file rewritten in place with identical mtime and length,
//! so the event stream never saw a change — is owned by conditional
//! safety, not by this function. Inputs are primitives (watermark age, a
//! distrust flag, suspect paths) so this crate stays decoupled from
//! `watch`.

use std::time::Duration;

use crate::types::RelPath;

/// Default freshness window: a watermark no older than this is treated as
/// current. The MCP layer may override the value it passes into
/// [`barrier_decision`]; this constant is the documented default.
pub const FRESH_WINDOW: Duration = Duration::from_secs(5);

/// Default cap on suspects repaired inline at query time. The MCP layer
/// may override this via `ASTROLABE_BARRIER_MAX`; this constant is the
/// documented default.
pub const BARRIER_MAX_INLINE: usize = 64;

/// Decision produced by the query-time freshness barrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BarrierDecision {
    /// Watermark is fresh and there are no suspects: answer directly.
    Fresh,
    /// Suspect count is within `max_inline`: the caller should run
    /// `apply_changeset` on these paths (sorted, unique) and then answer.
    Repair(Vec<RelPath>),
    /// Too many suspects, or the event stream is distrusted: skip inline
    /// repair, kick a background reindex, and annotate the result as stale.
    Degrade { suspects: usize },
}

/// Pure decision: given watermark age (time since the last completed
/// verification), a distrust flag, the suspect path set, and the inline
/// cap, produce a [`BarrierDecision`].
///
/// Rules, in order:
/// - `distrusted == true` → always [`BarrierDecision::Degrade`] (the
///   event stream has declared itself untrustworthy; point-repair is
///   meaningless).
/// - `watermark_age <= fresh_window` and no unique suspects →
///   [`BarrierDecision::Fresh`].
/// - unique suspects `len <= max_inline` → [`BarrierDecision::Repair`]
///   with paths sorted and deduplicated.
/// - otherwise → [`BarrierDecision::Degrade`] with the unique count.
#[must_use]
pub fn barrier_decision(
    watermark_age: Duration,
    fresh_window: Duration,
    distrusted: bool,
    suspects: &[RelPath],
    max_inline: usize,
) -> BarrierDecision {
    let unique = unique_sorted(suspects);
    if distrusted {
        return BarrierDecision::Degrade {
            suspects: unique.len(),
        };
    }
    if watermark_age <= fresh_window && unique.is_empty() {
        return BarrierDecision::Fresh;
    }
    if unique.len() <= max_inline {
        return BarrierDecision::Repair(unique);
    }
    BarrierDecision::Degrade {
        suspects: unique.len(),
    }
}

/// Staleness note appended to a degraded tool result. Wording matches
/// existing Chinese tool-result notes in the MCP server (sentence +
/// `confidence: unknown`).
#[must_use]
pub fn staleness_note(suspects: usize) -> String {
    format!(
        "索引可能过期：{suspects} 个疑点文件未内联修复，已触发后台重索引。本结果须按 stale 解读。confidence: unknown"
    )
}

fn unique_sorted(suspects: &[RelPath]) -> Vec<RelPath> {
    let mut unique: Vec<RelPath> = suspects.to_vec();
    unique.sort();
    unique.dedup();
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s)
    }

    fn decide(
        age: Duration,
        window: Duration,
        distrusted: bool,
        paths: &[&str],
        max_inline: usize,
    ) -> BarrierDecision {
        let suspects: Vec<RelPath> = paths.iter().copied().map(p).collect();
        barrier_decision(age, window, distrusted, &suspects, max_inline)
    }

    #[test]
    fn fresh_zero_age_no_suspects() {
        assert_eq!(
            decide(Duration::ZERO, FRESH_WINDOW, false, &[], BARRIER_MAX_INLINE),
            BarrierDecision::Fresh
        );
    }

    #[test]
    fn fresh_at_window_boundary_no_suspects() {
        assert_eq!(
            decide(FRESH_WINDOW, FRESH_WINDOW, false, &[], BARRIER_MAX_INLINE),
            BarrierDecision::Fresh
        );
        assert_eq!(
            decide(
                Duration::from_millis(4_999),
                FRESH_WINDOW,
                false,
                &[],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Fresh
        );
    }

    #[test]
    fn repair_single_suspect() {
        assert_eq!(
            decide(
                Duration::ZERO,
                FRESH_WINDOW,
                false,
                &["src/lib.rs"],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Repair(vec![p("src/lib.rs")])
        );
    }

    #[test]
    fn repair_exactly_max_inline() {
        let paths: Vec<String> = (0..BARRIER_MAX_INLINE)
            .map(|i| format!("f{i}.rs"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut expected: Vec<RelPath> = refs.iter().copied().map(p).collect();
        expected.sort();
        match decide(
            Duration::ZERO,
            FRESH_WINDOW,
            false,
            &refs,
            BARRIER_MAX_INLINE,
        ) {
            BarrierDecision::Repair(got) => assert_eq!(got, expected),
            other => panic!("expected Repair, got {other:?}"),
        }
    }

    #[test]
    fn repair_sorts_and_dedups() {
        assert_eq!(
            decide(
                Duration::ZERO,
                FRESH_WINDOW,
                false,
                &["c.rs", "a.rs", "b.rs", "a.rs", "c.rs"],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Repair(vec![p("a.rs"), p("b.rs"), p("c.rs")])
        );
    }

    #[test]
    fn repair_stale_watermark_few_suspects() {
        // Watermark age does not block Repair once suspects are non-empty.
        assert_eq!(
            decide(
                FRESH_WINDOW + Duration::from_secs(1),
                FRESH_WINDOW,
                false,
                &["z.rs", "a.rs"],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Repair(vec![p("a.rs"), p("z.rs")])
        );
    }

    #[test]
    fn repair_stale_watermark_empty_suspects() {
        // Not Fresh (watermark past the window); unique len 0 <= max_inline.
        assert_eq!(
            decide(
                FRESH_WINDOW + Duration::from_nanos(1),
                FRESH_WINDOW,
                false,
                &[],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Repair(Vec::new())
        );
    }

    #[test]
    fn degrade_one_over_max_inline() {
        let paths: Vec<String> = (0..=BARRIER_MAX_INLINE)
            .map(|i| format!("f{i}.rs"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert_eq!(
            decide(
                Duration::ZERO,
                FRESH_WINDOW,
                false,
                &refs,
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Degrade {
                suspects: BARRIER_MAX_INLINE + 1
            }
        );
    }

    #[test]
    fn degrade_well_over_cap_counts_unique() {
        let mut paths: Vec<String> = (0..100).map(|i| format!("x{i}.rs")).collect();
        paths.extend((0..20).map(|i| format!("x{i}.rs")));
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert_eq!(
            decide(Duration::ZERO, FRESH_WINDOW, false, &refs, 10),
            BarrierDecision::Degrade { suspects: 100 }
        );
    }

    #[test]
    fn distrusted_overrides_fresh() {
        assert_eq!(
            decide(Duration::ZERO, FRESH_WINDOW, true, &[], BARRIER_MAX_INLINE),
            BarrierDecision::Degrade { suspects: 0 }
        );
        assert_eq!(
            decide(FRESH_WINDOW, FRESH_WINDOW, true, &[], 0),
            BarrierDecision::Degrade { suspects: 0 }
        );
    }

    #[test]
    fn distrusted_overrides_repair() {
        assert_eq!(
            decide(
                Duration::ZERO,
                FRESH_WINDOW,
                true,
                &["src/a.rs"],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Degrade { suspects: 1 }
        );
        assert_eq!(
            decide(
                Duration::ZERO,
                FRESH_WINDOW,
                true,
                &["b.rs", "a.rs", "b.rs"],
                BARRIER_MAX_INLINE
            ),
            BarrierDecision::Degrade { suspects: 2 }
        );
    }

    #[test]
    fn staleness_note_includes_suspect_count() {
        let zero = staleness_note(0);
        assert!(zero.contains('0'), "{zero}");
        assert!(zero.contains("confidence: unknown"), "{zero}");

        let seven = staleness_note(7);
        assert!(seven.contains('7'), "{seven}");
        assert!(seven.contains("7 个疑点"), "{seven}");
        assert!(seven.contains("confidence: unknown"), "{seven}");
    }
}
