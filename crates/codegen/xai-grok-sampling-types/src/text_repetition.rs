//! Text-repetition guard: policy, trip payload, and detector vocabulary shared across layers.
//!
//! The detector itself lives in `xai-grok-sampler::text_repetition`; this module stays pure data
//! (no I/O) so the shell's config layer and the sampler can both name the types.
//! Mirrors [`crate::doom_loop`]: the policy is the one struct the TOML `[text_repetition_guard]`
//! table, the remote settings object, and the per-request sampler config all share.

use serde::{Deserialize, Serialize};

/// Which sub-detector fired. Serialized as the wire/telemetry label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRepetitionTrigger {
    /// D1: one token repeated consecutively past the run cap (plus the tail-window double check).
    IdenticalRun,
    /// D2: one fixed-size chunk recurring within a short average distance (Gemini-style periodicity).
    ChunkPeriodicity,
}

impl TextRepetitionTrigger {
    /// Stable label used in `signals.json`, `events.jsonl`, and the UI copy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdenticalRun => "identical_run",
            Self::ChunkPeriodicity => "chunk_periodicity",
        }
    }
}

impl std::str::FromStr for TextRepetitionTrigger {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "identical_run" => Ok(Self::IdenticalRun),
            "chunk_periodicity" => Ok(Self::ChunkPeriodicity),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for TextRepetitionTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One guard trip: which detector fired, how much assistant text had been generated, and the
/// reading that reached the threshold (run length or chunk repeat count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRepetitionTrip {
    pub trigger: TextRepetitionTrigger,
    /// Assistant characters fed to the guard before the trip (whole turns, not just the tail window).
    pub generated_chars: u64,
    /// The observed value that fired: identical-token run length, or same-chunk repeat count.
    pub threshold: u32,
}

/// Thresholds for the client-side text-repetition guard.
/// One type serves the local `[text_repetition_guard]` TOML table and the remote
/// `text_repetition_guard` JSON object; every field defaults per-field so a partial object parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextRepetitionGuardPolicy {
    /// D1: trip when one token repeats this many times consecutively (default 64).
    #[serde(default = "default_max_identical_token_run")]
    pub max_identical_token_run: u32,
    /// D2: chunk size in characters (default 50).
    #[serde(default = "default_chunk_size")]
    pub chunk_size: u32,
    /// D2: how many times one chunk must recur inside the average-distance bound (default 10).
    #[serde(default = "default_chunk_repeat_threshold")]
    pub chunk_repeat_threshold: u32,
    /// D2: streaming history window in characters; older text is truncated from the front (default 5000).
    #[serde(default = "default_history_limit")]
    pub history_limit: u32,
}

fn default_max_identical_token_run() -> u32 {
    TextRepetitionGuardPolicy::DEFAULT_MAX_IDENTICAL_TOKEN_RUN
}

fn default_chunk_size() -> u32 {
    TextRepetitionGuardPolicy::DEFAULT_CHUNK_SIZE
}

fn default_chunk_repeat_threshold() -> u32 {
    TextRepetitionGuardPolicy::DEFAULT_CHUNK_REPEAT_THRESHOLD
}

fn default_history_limit() -> u32 {
    TextRepetitionGuardPolicy::DEFAULT_HISTORY_LIMIT
}

impl Default for TextRepetitionGuardPolicy {
    fn default() -> Self {
        Self {
            max_identical_token_run: Self::DEFAULT_MAX_IDENTICAL_TOKEN_RUN,
            chunk_size: Self::DEFAULT_CHUNK_SIZE,
            chunk_repeat_threshold: Self::DEFAULT_CHUNK_REPEAT_THRESHOLD,
            history_limit: Self::DEFAULT_HISTORY_LIMIT,
        }
    }
}

impl TextRepetitionGuardPolicy {
    pub const MAX_IDENTICAL_TOKEN_RUN_RANGE: std::ops::RangeInclusive<u32> = 8..=4096;
    pub const CHUNK_SIZE_RANGE: std::ops::RangeInclusive<u32> = 16..=256;
    pub const CHUNK_REPEAT_THRESHOLD_RANGE: std::ops::RangeInclusive<u32> = 4..=64;
    pub const HISTORY_LIMIT_RANGE: std::ops::RangeInclusive<u32> = 250..=50_000;

    pub const DEFAULT_MAX_IDENTICAL_TOKEN_RUN: u32 = 64;
    pub const DEFAULT_CHUNK_SIZE: u32 = 50;
    pub const DEFAULT_CHUNK_REPEAT_THRESHOLD: u32 = 10;
    pub const DEFAULT_HISTORY_LIMIT: u32 = 5000;

    /// Clamp a configured `max_identical_token_run` into [`Self::MAX_IDENTICAL_TOKEN_RUN_RANGE`].
    pub fn clamp_max_identical_token_run(value: u32) -> u32 {
        value.clamp(
            *Self::MAX_IDENTICAL_TOKEN_RUN_RANGE.start(),
            *Self::MAX_IDENTICAL_TOKEN_RUN_RANGE.end(),
        )
    }

    /// Clamp a configured `chunk_size` into [`Self::CHUNK_SIZE_RANGE`].
    pub fn clamp_chunk_size(value: u32) -> u32 {
        value.clamp(
            *Self::CHUNK_SIZE_RANGE.start(),
            *Self::CHUNK_SIZE_RANGE.end(),
        )
    }

    /// Clamp a configured `chunk_repeat_threshold` into [`Self::CHUNK_REPEAT_THRESHOLD_RANGE`].
    pub fn clamp_chunk_repeat_threshold(value: u32) -> u32 {
        value.clamp(
            *Self::CHUNK_REPEAT_THRESHOLD_RANGE.start(),
            *Self::CHUNK_REPEAT_THRESHOLD_RANGE.end(),
        )
    }

    /// Clamp a configured `history_limit` into [`Self::HISTORY_LIMIT_RANGE`].
    pub fn clamp_history_limit(value: u32) -> u32 {
        value.clamp(
            *Self::HISTORY_LIMIT_RANGE.start(),
            *Self::HISTORY_LIMIT_RANGE.end(),
        )
    }

    /// Out-of-range values fail closed to the default, mirroring the doom-loop window clamp.
    pub fn sanitized(self) -> Self {
        Self {
            max_identical_token_run: Self::clamp_max_identical_token_run(
                self.max_identical_token_run,
            ),
            chunk_size: Self::clamp_chunk_size(self.chunk_size),
            chunk_repeat_threshold: Self::clamp_chunk_repeat_threshold(self.chunk_repeat_threshold),
            history_limit: Self::clamp_history_limit(self.history_limit),
        }
    }
}
