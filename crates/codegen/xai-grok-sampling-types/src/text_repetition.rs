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

/// 思考（reasoning）通道的护栏子配置。思考文本天然重复率高（列表、重述），阈值比正文宽松，
/// 仅在明显洪泛（同 token 长串、整句无限循环）时触发。serde default 全覆盖：老配置文件与
/// 远端 settings 缺失该子对象时按默认值解析，零迁移；`enforce_on_reasoning=false` 是一键
/// 关闭思考通道护栏的逃生口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextRepetitionReasoningPolicy {
    /// 思考流是否也受护栏约束（默认启用；线上已出现思考流死循环事故）。
    #[serde(default = "default_reasoning_enforce_on")]
    pub enforce_on_reasoning: bool,
    /// 思考流 D1: 同 token 连续次数上限（默认 128，比正文的 64 宽松一倍）。
    #[serde(default = "default_reasoning_max_identical_token_run")]
    pub max_identical_token_run: u32,
    /// 思考流 D2: 同块重复次数阈值（默认 16，比正文的 10 宽松）。
    #[serde(default = "default_reasoning_chunk_repeat_threshold")]
    pub chunk_repeat_threshold: u32,
}

fn default_reasoning_enforce_on() -> bool {
    TextRepetitionReasoningPolicy::DEFAULT_ENFORCE_ON_REASONING
}

fn default_reasoning_max_identical_token_run() -> u32 {
    TextRepetitionReasoningPolicy::DEFAULT_MAX_IDENTICAL_TOKEN_RUN
}

fn default_reasoning_chunk_repeat_threshold() -> u32 {
    TextRepetitionReasoningPolicy::DEFAULT_CHUNK_REPEAT_THRESHOLD
}

impl Default for TextRepetitionReasoningPolicy {
    fn default() -> Self {
        Self {
            enforce_on_reasoning: Self::DEFAULT_ENFORCE_ON_REASONING,
            max_identical_token_run: Self::DEFAULT_MAX_IDENTICAL_TOKEN_RUN,
            chunk_repeat_threshold: Self::DEFAULT_CHUNK_REPEAT_THRESHOLD,
        }
    }
}

impl TextRepetitionReasoningPolicy {
    pub const DEFAULT_ENFORCE_ON_REASONING: bool = true;
    pub const DEFAULT_MAX_IDENTICAL_TOKEN_RUN: u32 = 128;
    pub const DEFAULT_CHUNK_REPEAT_THRESHOLD: u32 = 16;

    /// 越界值同正文一样钳制回默认，不做独立区间：思考阈值与正文共用同一合法范围。
    pub fn sanitized(self) -> Self {
        Self {
            enforce_on_reasoning: self.enforce_on_reasoning,
            max_identical_token_run: TextRepetitionGuardPolicy::clamp_max_identical_token_run(
                self.max_identical_token_run,
            ),
            chunk_repeat_threshold: TextRepetitionGuardPolicy::clamp_chunk_repeat_threshold(
                self.chunk_repeat_threshold,
            ),
        }
    }
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
    /// 思考（reasoning）通道的护栏子配置：独立阈值 + 总开关，正文通道配置不动。
    #[serde(default)]
    pub reasoning: TextRepetitionReasoningPolicy,
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
            reasoning: TextRepetitionReasoningPolicy::default(),
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
            reasoning: self.reasoning.sanitized(),
        }
    }

    /// 思考通道守卫实际使用的正文策略形态：D1/D2 阈值换成思考子配置，chunk_size 与
    /// 历史窗沿用正文配置（两个 guard 实例状态互不污染，检测实现共用）。
    pub fn reasoning_guard_policy(self) -> Self {
        Self {
            max_identical_token_run: self.reasoning.max_identical_token_run,
            chunk_size: self.chunk_size,
            chunk_repeat_threshold: self.reasoning.chunk_repeat_threshold,
            history_limit: self.history_limit,
            reasoning: self.reasoning,
        }
    }
}
