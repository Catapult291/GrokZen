//! Token budgeting for tool output.
//!
//! Every tool takes a token ceiling and must stop cleanly at it rather than
//! truncating mid-structure. Truncate by rank: drop whole lower-ranked items
//! and say how many were dropped, so the agent knows the list was cut and can
//! ask for more.
//!
//! Estimation: `chars / 4` is the cheap approximation the prior art used and
//! it is good enough for budgeting, but it under-counts CJK and dense symbol
//! text. Keep the estimator behind this type so it can be swapped for a real
//! tokenizer without touching callers.

/// Rough token count for a string.
///
/// This deliberately counts Unicode scalar values rather than UTF-8 bytes:
/// the approximation is "characters divided by four". It still under-counts
/// CJK text, where a real tokenizer commonly emits more tokens per character.
/// Keep all callers going through this function so the approximation can be
/// replaced without changing truncation logic.
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Spend is tracked in characters and rounded up to tokens once, over the
/// running total. Rounding every item up on its own over-charges lists of
/// short lines: ten 5-character lines (50 chars, 13 tokens) would cost 20.
pub struct TokenBudget {
    limit: usize,
    used_chars: usize,
}

impl TokenBudget {
    pub fn new(limit: usize) -> Self {
        TokenBudget {
            limit,
            used_chars: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used_chars.div_ceil(4))
    }

    pub fn would_exceed(&self, s: &str) -> bool {
        self.used_chars
            .saturating_add(s.chars().count())
            .div_ceil(4)
            > self.limit
    }

    /// Charge `s` to the budget if it fits. Returns false when it does not,
    /// leaving the budget untouched.
    pub fn try_add(&mut self, s: &str) -> bool {
        if self.would_exceed(s) {
            return false;
        }
        self.used_chars += s.chars().count();
        true
    }
}

/// Keep the longest prefix of ranked `items` that fits in `limit`.
///
/// `render` must produce the complete representation that will be emitted for
/// one item, including any separators charged to that item. Once an item does
/// not fit, it and every lower-ranked item are omitted: items are never split
/// and smaller low-ranked items never leapfrog a higher-ranked one.
///
/// The second tuple element is the number omitted, allowing callers to append
/// an explicit "N more items omitted due to budget" marker.
pub fn truncate_ranked<T, F>(items: &[T], limit: usize, mut render: F) -> (Vec<&T>, usize)
where
    F: FnMut(&T) -> String,
{
    let mut budget = TokenBudget::new(limit);
    let mut kept = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let rendered = render(item);
        if !budget.try_add(&rendered) {
            return (kept, items.len() - index);
        }
        kept.push(item);
    }

    (kept, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_refuses_overflow_without_charging() {
        let mut b = TokenBudget::new(10);
        assert!(b.try_add("12345678")); // 2 tokens
        let before = b.remaining();
        assert!(!b.try_add(&"x".repeat(100)));
        assert_eq!(b.remaining(), before);
    }

    #[test]
    fn ranked_truncation_drops_whole_tail_items() {
        let items = ["1111", "22222222", "3"];
        let (kept, omitted) = truncate_ranked(&items, 2, |item| item.to_string());

        assert_eq!(kept, vec![&"1111"]);
        assert_eq!(omitted, 2);
    }

    #[test]
    fn zero_budget_is_safe_and_reports_all_nonempty_items_omitted() {
        let items = ["first", "second"];
        let (kept, omitted) = truncate_ranked(&items, 0, |item| item.to_string());

        assert!(kept.is_empty());
        assert_eq!(omitted, items.len());
    }

    #[test]
    fn short_items_are_not_rounded_up_one_by_one() {
        // Ten 5-char lines (incl. newline) = 50 chars = 13 tokens in total;
        // per-item rounding would charge 20 and cut the list at 10 tokens.
        let items: Vec<String> = (0..10).map(|i| format!("Li{i:02}\n")).collect();
        let (kept, omitted) = truncate_ranked(&items, 13, |item| item.clone());
        assert_eq!((kept.len(), omitted), (10, 0));
        let (kept, omitted) = truncate_ranked(&items, 12, |item| item.clone());
        assert_eq!((kept.len(), omitted), (9, 1));
    }

    #[test]
    fn estimator_counts_characters_not_utf8_bytes() {
        assert_eq!(estimate_tokens("中文😀a"), 1);
    }
}
