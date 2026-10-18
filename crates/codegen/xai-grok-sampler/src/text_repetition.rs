//! Client-side streaming text-repetition guard: the sibling of the server-reported doom-loop check.
//!
//! Two independent sub-detectors; the first trip wins ([`TextRepetitionGuard::on_text_chunk`]):
//!
//! * **D1 `identical_run`** — one whitespace/punctuation-delimited token repeated consecutively
//!   past `max_identical_token_run`, plus a tail-anchored double check: once at least 4,000
//!   characters have been generated, the last 8 completed tokens being identical also trips. The
//!   double check catches floods whose run counter kept getting reset mid-stream; the full-window
//!   gate keeps short replies and the run-cap boundary honest.
//! * **D2 `chunk_periodicity`** — the Gemini `LoopDetectionService` content-loop algorithm:
//!   fixed `chunk_size`-character windows hashed and counted, tripping when one window recurs
//!   `chunk_repeat_threshold` times with average distance ≤ 5×chunk and at most
//!   `threshold / 2` distinct periods between occurrences (which lets long shared-prefix lists
//!   through).
//!
//! False-positive guards follow the reference's `checkContentLoop`: any markdown shape in a fed
//! piece (table / list / heading / blockquote / divider / code fence) resets tracking first, and
//! dividers and code-block content are never judged at all — so structured content that repeats
//! naturally cannot accumulate a verdict, and a wall of `===` lines reads as a divider rather
//! than a token flood. Only model-authored message text is ever fed in — reasoning, tool
//! arguments, tool results, and user text never reach this type.
//!
//! State is bounded: the D2 history is a `history_limit`-character tail window plus a
//! chunk-position table; D1 keeps the last token, an in-progress token, and a short tail ring.
//! No whole-message retention.

use std::collections::{HashMap, VecDeque};

use xai_grok_sampling_types::{
    TextRepetitionGuardPolicy, TextRepetitionTrigger, TextRepetitionTrip,
};

/// The tail-anchored double check only arms once this many characters have been generated; below
/// it the primary run counter is the sole D1 verdict (keeps `max_identical_token_run - 1`
/// sequences non-tripping at any token length under the window).
const TAIL_WINDOW_CHARS: u64 = 4_000;
/// Identical completed tokens required at the tail for the double check.
const TAIL_IDENTICAL_TOKENS: usize = 8;
/// Force-flush bound for one whitespace-free token, so an unbroken symbol stream stays bounded.
const MAX_TOKEN_BYTES: usize = 1024;

/// FNV-1a 64 over chunk chars; identity is re-verified against the history on lookup, so a
/// collision can only cost a dropped sample, never a false positive.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash_chars(chars: &[char]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &ch in chars {
        hash ^= ch as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Streaming detector for one sampling attempt. Feed every message-channel text delta in order;
/// the first trip is returned and latches the guard (later feeds are ignored).
#[derive(Debug)]
pub struct TextRepetitionGuard {
    policy: TextRepetitionGuardPolicy,
    /// Total assistant characters ever fed (not truncated with the D2 window); reported on a trip.
    generated_chars: u64,
    tripped: bool,

    // ── D1 state ───────────────────────────────────────────────────────────
    /// Token that ended the current consecutive run.
    last_token: Option<Box<str>>,
    /// Length of the current consecutive-same-token run (0 = nothing seen yet).
    token_run: u32,
    /// In-progress token carried across chunk boundaries, so a token split by the stream still
    /// counts once (the incident's `<br>\n` deltas arrive in arbitrary slices).
    pending_token: String,
    /// Last `TAIL_IDENTICAL_TOKENS` completed tokens for the tail-anchored double check.
    tail_tokens: VecDeque<Box<str>>,

    // ── D2 state (Gemini `LoopDetectionService` shape, char-indexed) ────────
    /// Rolling character history; truncated from the front past `history_limit`.
    history: Vec<char>,
    /// FNV of each `chunk_size`-character window → the character positions where it occurred.
    chunk_positions: HashMap<u64, Vec<usize>>,
    /// Next history position to slice a window from (the sliding-window cursor).
    next_index: usize,
    /// Fence parity: content is not judged while inside a ``` code block.
    in_code_block: bool,
}

impl TextRepetitionGuard {
    pub fn new(policy: TextRepetitionGuardPolicy) -> Self {
        Self {
            policy: policy.sanitized(),
            generated_chars: 0,
            tripped: false,
            last_token: None,
            token_run: 0,
            pending_token: String::new(),
            tail_tokens: VecDeque::with_capacity(TAIL_IDENTICAL_TOKENS),
            history: Vec::new(),
            chunk_positions: HashMap::new(),
            next_index: 0,
            in_code_block: false,
        }
    }

    /// The policy this guard enforces (for telemetry beside a trip).
    pub fn policy(&self) -> TextRepetitionGuardPolicy {
        self.policy
    }

    /// Feed one message-channel text delta, in stream order. Returns the first trip, if any.
    pub fn on_text_chunk(&mut self, text: &str) -> Option<TextRepetitionTrip> {
        if self.tripped || text.is_empty() {
            return None;
        }
        self.generated_chars = self.generated_chars.saturating_add(text.len() as u64);

        // Reference order: element-shape reset first, then fence parity, then skip judging.
        let fence_count = text.matches("```").count();
        let shape_reset = fence_count > 0
            || table_like(text)
            || list_like(text)
            || heading_like(text)
            || blockquote_like(text)
            || divider_like(text);
        if shape_reset {
            self.reset_tracking();
        }
        let was_in_code_block = self.in_code_block;
        if fence_count % 2 == 1 {
            self.in_code_block = !self.in_code_block;
        }
        if was_in_code_block || self.in_code_block || fence_count > 0 || divider_like(text) {
            // Code blocks and dividers are legal to repeat; neither detector sees them.
            // A piece that itself contains a fence marker is skipped too: a whole block arriving
            // in one delta must not be judged between its fences (parity alone would miss it).
            // (A divider also skips the feed: a wall of `===` lines is structured content.)
            return None;
        }

        let trip = self.check_identical_run(text);
        if trip.is_some() {
            self.tripped = true;
            return trip;
        }
        let trip = self.check_chunk_periodicity(text);
        if trip.is_some() {
            self.tripped = true;
        }
        trip
    }

    /// D1: incremental whitespace tokenization into the consecutive-run counter, then the tail
    /// double check. Tokens are whitespace-separated (the incident's `<br>` flood is one token);
    /// a token longer than [`MAX_TOKEN_BYTES`] is force-flushed so an unbroken symbol stream
    /// stays bounded and still reads as repeating tokens.
    fn check_identical_run(&mut self, text: &str) -> Option<TextRepetitionTrip> {
        let cap = self.policy.max_identical_token_run;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !self.pending_token.is_empty() {
                    let token = std::mem::take(&mut self.pending_token);
                    if let Some(trip) = self.note_completed_token(&token, cap) {
                        return Some(trip);
                    }
                }
            } else {
                self.pending_token.push(ch);
                if self.pending_token.len() >= MAX_TOKEN_BYTES {
                    let token = std::mem::take(&mut self.pending_token);
                    if let Some(trip) = self.note_completed_token(&token, cap) {
                        return Some(trip);
                    }
                }
            }
        }
        None
    }

    /// Fold one completed token into the run counter / tail ring; trips at the configured cap or,
    /// past the full-window gate, when the last completed tokens are identical.
    fn note_completed_token(&mut self, token: &str, cap: u32) -> Option<TextRepetitionTrip> {
        if self.last_token.as_deref() == Some(token) {
            self.token_run = self.token_run.saturating_add(1);
        } else {
            self.last_token = Some(token.into());
            self.token_run = 1;
        }
        if self.token_run >= cap {
            return Some(TextRepetitionTrip {
                trigger: TextRepetitionTrigger::IdenticalRun,
                generated_chars: self.generated_chars,
                threshold: self.token_run,
            });
        }
        self.tail_tokens.push_back(token.into());
        while self.tail_tokens.len() > TAIL_IDENTICAL_TOKENS {
            self.tail_tokens.pop_front();
        }
        let tail_identical = self.tail_tokens.len() == TAIL_IDENTICAL_TOKENS
            && self.tail_tokens.front().is_some_and(|first| {
                self.tail_tokens
                    .iter()
                    .all(|t| first.as_ref() == t.as_ref())
            });
        if self.generated_chars >= TAIL_WINDOW_CHARS && tail_identical {
            return Some(TextRepetitionTrip {
                trigger: TextRepetitionTrigger::IdenticalRun,
                generated_chars: self.generated_chars,
                threshold: self.token_run.max(TAIL_IDENTICAL_TOKENS as u32),
            });
        }
        None
    }

    /// D2: append to the char history, keep it inside the window, slide unprocessed chunk starts.
    /// Whitespace-only pieces are still fed to D1 above (they complete a token) but never enter
    /// the D2 history: a run of blank lines is legal text, and would otherwise read as one
    /// 50-character window repeating at stride 1.
    fn check_chunk_periodicity(&mut self, text: &str) -> Option<TextRepetitionTrip> {
        if !text.chars().any(|ch| !ch.is_whitespace()) {
            return None;
        }
        self.history.extend(text.chars());
        self.truncate_history();

        let chunk_size = self.policy.chunk_size as usize;
        loop {
            // Hash first and release the history borrow before the `&mut self` judge call.
            let Some(hash) = self
                .history
                .get(self.next_index..self.next_index + chunk_size)
                .map(hash_chars)
            else {
                return None;
            };
            if self.chunk_triggers(hash) {
                return Some(TextRepetitionTrip {
                    trigger: TextRepetitionTrigger::ChunkPeriodicity,
                    generated_chars: self.generated_chars,
                    threshold: self.policy.chunk_repeat_threshold,
                });
            }
            self.next_index += 1;
        }
    }

    /// Judge the window starting at `next_index`; records it for future comparisons when it
    /// does not trip. Identity is verified against the history so a hash collision is dropped.
    fn chunk_triggers(&mut self, hash: u64) -> bool {
        let chunk_size = self.policy.chunk_size as usize;
        let threshold = self.policy.chunk_repeat_threshold as usize;
        let position = self.next_index;

        // An all-whitespace window is never recorded: a wall of blank lines renders as nothing,
        // and without this a run of ~60+ blanks would read as one window repeating at stride 1.
        if self
            .history
            .get(position..position + chunk_size)
            .is_some_and(|window| window.iter().all(|ch| ch.is_whitespace()))
        {
            return false;
        }

        let recent: Vec<usize> = {
            // `chunk_positions` and `history` are disjoint fields: comparing inside the entry
            // borrow is a legal split borrow, and the collision case returns without recording.
            let positions = self.chunk_positions.entry(hash).or_default();
            if let Some(&first) = positions.first()
                && self.history.get(first..first + chunk_size)
                    != self.history.get(position..position + chunk_size)
            {
                // Collision: the reference compares actual content and refuses to mix windows.
                return false;
            }
            positions.push(position);
            if positions.len() < threshold {
                return false;
            }
            positions
                .iter()
                .skip(positions.len() - threshold)
                .copied()
                .collect()
        };

        let (Some(&first), Some(&last)) = (recent.first(), recent.last()) else {
            return false;
        };
        let gaps = (threshold - 1) as u64;
        if gaps == 0 {
            return false;
        }
        let average_distance = last.saturating_sub(first) as f64 / gaps as f64;
        if average_distance > chunk_size as f64 * 5.0 {
            return false;
        }
        // Distinct periods between consecutive occurrences: a true loop has few; a list of
        // distinct items sharing a prefix has many. The reference allows `threshold / 2`.
        let max_unique_periods = threshold / 2;
        let mut distinct_periods: Vec<&[char]> = Vec::new();
        for pair in recent.windows(2) {
            let (Some(&from), Some(&to)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let Some(period) = self.history.get(from..to) else {
                return false;
            };
            if distinct_periods.contains(&period) {
                continue;
            }
            distinct_periods.push(period);
            if distinct_periods.len() > max_unique_periods {
                return false;
            }
        }
        true
    }

    /// Drop the oldest characters once the window is exceeded, shifting every stored index the
    /// same way the reference's `truncateAndUpdate` does.
    fn truncate_history(&mut self) {
        let limit = self.policy.history_limit as usize;
        if self.history.len() <= limit {
            return;
        }
        let cut = self.history.len() - limit;
        self.history.drain(..cut);
        self.next_index = self.next_index.saturating_sub(cut);
        let history_len = self.history.len();
        self.chunk_positions.retain(|_, positions| {
            for p in positions.iter_mut() {
                *p = p.saturating_sub(cut);
            }
            positions.retain(|p| *p < history_len);
            !positions.is_empty()
        });
    }

    /// Clear both detectors' tracking at an element boundary (markdown shape / code fence).
    /// Fence parity itself is state that must survive — the caller flips it separately.
    fn reset_tracking(&mut self) {
        self.history.clear();
        self.chunk_positions.clear();
        self.next_index = 0;
        self.last_token = None;
        self.token_run = 0;
        self.pending_token.clear();
        self.tail_tokens.clear();
    }
}

// ── Markdown-shape guards (ported from Gemini's `checkContentLoop` regexes) ──────────────────

fn table_like(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('|') {
            return true;
        }
        trimmed
            .chars()
            .take_while(|c| matches!(c, '|' | '+' | '-'))
            .count()
            >= 3
    })
}

fn list_like(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        match chars.next() {
            Some('*' | '-' | '+') => chars.next().is_some_and(|c| c.is_whitespace()),
            _ => {
                let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
                digits > 0
                    && digits <= 4
                    && trimmed.chars().nth(digits) == Some('.')
                    && trimmed
                        .chars()
                        .nth(digits + 1)
                        .is_some_and(|c| c.is_whitespace())
            }
        }
    })
}

fn heading_like(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        hashes > 0
            && trimmed
                .chars()
                .nth(hashes)
                .is_some_and(|c| c.is_whitespace())
    })
}

fn blockquote_like(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('>') && trimmed.chars().nth(1).is_some_and(|c| c.is_whitespace())
    })
}

/// Text that is nothing but divider characters and whitespace (`===`, `---`, `***`, box drawing…).
/// Whitespace is allowed inside so a multi-line wall of dividers still classifies as a divider
/// rather than accumulating as periodic text.
fn divider_like(text: &str) -> bool {
    let mut saw_divider_char = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if "+-_=*\u{2500}\u{2501}\u{2502}\u{2503}\u{250c}\u{2510}\u{2514}\u{2518}".contains(ch) {
            saw_divider_char = true;
            continue;
        }
        return false;
    }
    saw_divider_char
}

#[cfg(test)]
#[path = "text_repetition_tests.rs"]
mod tests;
