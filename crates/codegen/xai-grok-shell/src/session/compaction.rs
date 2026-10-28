//! This module contains the manual `/compact` implementation and checkpoint persistence.
//! These methods form a second `impl SessionActor` block that lives alongside the primary one in `acp_session.rs`.
use super::SessionActor;
use super::is_project_instructions;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::session::helpers::CompactionStateContext;
use crate::session::helpers::compaction_context::CompactionInputs;
use crate::session::helpers::compaction_context::to_system_reminder;
use crate::session::helpers::session_compact::{
    COMPACT_FAILED_PREFIX, CompactOutput, CompactionOutcome, build_compaction_prompt,
    generate_session_compact, is_context_length_error, retain_session_asset_files,
};
use crate::session::persistence::PersistenceMsg;
use agent_client_protocol as acp;
use std::sync::Arc;
use xai_chat_state::compaction_image_context::CompactionImageContext;
use xai_chat_state::compaction_utils::{
    CompactedHistoryInput, CompactionAttempt, build_compacted_history, is_degenerate_summary,
    prepare_conversation_for_verbatim_summarization, sanitize_compacted_history,
    validate_compacted_history,
};
use xai_grok_sampling_types::{ApiBackend, ConversationItem};
/// Prefix on the early-guard failure payloads below; the user-facing normalizer strips it (the renderer prepends its own headline).
const COMPACTION_FAILED_GUARD_PREFIX: &str = "Compaction failed: ";
/// Human-readable "next fire" for a scheduled loop in the compaction reminder.
fn format_loop_next_fire(
    next: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = next.signed_duration_since(now).num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("in {secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("in {mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("in {hours}h");
    }
    next.format("%d %b %H:%M UTC").to_string()
}
/// Reserve for the prompt and generated summary (plus reasoning on Responses/Messages); 32_768 covers p99 of prod output (~20k p95).
const SUMMARY_BUDGET_RESERVE_TOKENS: u64 = 32_768;
/// Context-window percentage at which a memory flush runs ahead of a compaction.
const MEMORY_FLUSH_THRESHOLD_PERCENT: u8 = 85;
#[cfg(test)]
#[path = "compaction_verbatim_input_tests.rs"]
mod verbatim_input_tests;
/// The "always fits" lossy summarization budget (~70% of window, minus tool definitions); shared by the ladder's Lossy step and the cold Lossy start.
fn lossy_input_budget(context_window: u64, tool_tokens: u64) -> u64 {
    (context_window.saturating_mul(7) / 10).saturating_sub(tool_tokens)
}
/// Verbatim-fitted budget: leave room for the summary prompt plus tool-definition prefix.
fn fitted_input_budget(context_window: u64, tool_tokens: u64) -> u64 {
    context_window
        .saturating_sub(SUMMARY_BUDGET_RESERVE_TOKENS)
        .saturating_sub(tool_tokens)
}
/// Input ladder: verbatim, then verbatim fitted, then lossy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum CompactInputStage {
    Verbatim,
    VerbatimFitted,
    Lossy,
}
/// Same image budget then tool-result prune a live turn request uses.
async fn apply_turn_image_budget_and_prune(
    chat_state: &xai_chat_state::ChatStateHandle,
    items: Vec<ConversationItem>,
) -> Vec<ConversationItem> {
    let max_request_bytes = chat_state
        .get_sampling_config()
        .await
        .and_then(|config| config.max_request_bytes);
    let items = xai_chat_state::image_budget::apply_image_budget(items, max_request_bytes).items;
    chat_state.apply_turn_request_pruning(items).await
}
/// Start fitted when the (already image-budgeted and pruned) estimate cannot leave room for tools + summary.
fn start_verbatim_compact_turns(
    turns: Vec<ConversationItem>,
    tool_tokens: u64,
    context_window: u64,
) -> (Vec<ConversationItem>, CompactInputStage) {
    let estimate = xai_chat_state::estimate_conversation_tokens(&turns);
    let budget = fitted_input_budget(context_window, tool_tokens);
    if estimate > budget {
        tracing::info!(
            estimate,
            budget,
            "verbatim compact estimate exceeds fitted budget; starting at verbatim_fitted"
        );
        (
            xai_chat_state::compaction_utils::fit_conversation_to_budget(turns, budget),
            CompactInputStage::VerbatimFitted,
        )
    } else {
        (turns, CompactInputStage::Verbatim)
    }
}
/// Why a compaction attempt failed deterministically.
/// [`SuppressReason::as_str`] is a stable telemetry value (BQ/OTLP/dashboards key off it); don't rename the strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum SuppressReason {
    CreditBlock,
    Size,
    Auth,
    Schema,
    Other,
}
/// Splice the preserved prefix (`conversation[0..prefix_len]`) onto the compacted suffix, dropping the suffix's leading System.
/// If the prefix already has an AGENTS.md item, drop the suffix's re-injected AGENTS.md too (else the model sees it twice).
/// Returns `Err(compacted_history)` unchanged when `prefix_len` is 0 or out of range.
fn preserve_inherited_prefix(
    conversation: &[ConversationItem],
    compacted_history: Vec<ConversationItem>,
    prefix_len: usize,
) -> Result<Vec<ConversationItem>, Vec<ConversationItem>> {
    if prefix_len == 0 {
        return Err(compacted_history);
    }
    let Some(inherited) = conversation.get(..prefix_len) else {
        return Err(compacted_history);
    };
    let drop_reinjected_agents_md = inherited.iter().any(is_project_instructions);
    let mut preserved = inherited.to_vec();
    let child_items = compacted_history
        .into_iter()
        .skip_while(|i| matches!(i, ConversationItem::System(_)))
        .filter(|i| !(drop_reinjected_agents_md && is_project_instructions(i)));
    preserved.extend(child_items);
    Ok(preserved)
}
/// Project the token count a re-pinned (preserved) history would reseed to.
/// The release decision then compares against the same threshold the auto-compact trigger applies next turn.
/// The conversation only grows, so this under-estimates the reseed (a lower bound) and can lean toward preserve.
fn project_preserved_reseed_tokens(
    preserved_estimate: u64,
    tokens_before: u64,
    full_conv_estimate: u64,
) -> u64 {
    let ratio = tokens_before as f64 / full_conv_estimate.max(1) as f64;
    ((preserved_estimate as f64 * ratio).round() as u64).min(tokens_before)
}
impl SessionActor {
    /// Where the transcript would be, without asking the filesystem: callers on a hot path do the `exists()` themselves, off the actor's thread.
    pub(crate) fn transcript_path(&self) -> std::path::PathBuf {
        crate::session::persistence::session_dir(&self.session_info).join("updates.jsonl")
    }
    /// Path to the raw `updates.jsonl` transcript if it exists, else `None`.
    /// `pub(crate)` so the `Transcript`-mode dispatch in `compaction_segments` and transcript-location pointers can both reuse it.
    /// The `path.exists()` guard keeps the pointer safe when a session never wrote one.
    pub(crate) fn get_transcript_path(&self) -> Option<String> {
        let path = self.transcript_path();
        if path.exists() {
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        }
    }
    /// Increment the compaction counter and launch a pre-compaction memory flush.
    ///
    /// The counter is incremented before the flush check so the once-per-cycle guard does not suppress the first eligible flush.
    async fn maybe_pre_compaction_flush(
        self: &Arc<Self>,
        total_tokens: u64,
        context_window: u64,
        trigger: &'static str,
    ) {
        let compaction_count = self
            .compaction
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if !self.agent.borrow().compaction_policy().memory_flush_enabled {
            return;
        }
        let last_flush = self
            .memory
            .last_flush_compaction
            .load(std::sync::atomic::Ordering::Relaxed);
        if xai_grok_memory::flush::should_flush(
            total_tokens,
            context_window,
            MEMORY_FLUSH_THRESHOLD_PERCENT,
            &self.memory.flush_config,
            last_flush,
            compaction_count,
        ) {
            let snapshot = self.snapshot_memory_flush_state().await;
            tokio::task::spawn_local({
                let session = self.clone();
                async move {
                    if session.run_memory_flush(trigger, Some(snapshot)).await {
                        session
                            .memory
                            .last_flush_compaction
                            .store(compaction_count, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    }
    /// Tag the current `session.compact` span with `mode` (and `detail`, for `segments`), the A/B variant key for grouping outcomes in telemetry.
    fn record_compaction_variant(&self) {
        let mode = self.compaction.compaction_mode;
        let span = tracing::Span::current();
        span.record("mode", tracing::field::display(mode));
        if let Some(detail) = mode.segment_detail() {
            span.record("detail", tracing::field::display(detail));
        }
    }
    /// Handle a manual `/compact`: summarize the conversation so the context shrinks.
    #[tracing::instrument(
        name = "session.compact",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            trigger = "manual",
            mode = tracing::field::Empty,
            detail = tracing::field::Empty,
            pre_tokens = tracing::field::Empty,
            post_tokens = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub(crate) async fn run_compact(self: &Arc<Self>) -> Result<(), acp::Error> {
        let (_cancel, _cancel_scope) = self.compaction.cancel.enter();
        self.record_compaction_variant();
        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        tracing::Span::current().record("pre_tokens", total_tokens as i64);
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        self.maybe_pre_compaction_flush(total_tokens, context_window, "pre_compaction")
            .await;
        if let Err(e) = self
            .run_compact_inner(
                None,
                xai_grok_telemetry::events::CompactionTrigger::Manual,
                false,
            )
            .await
        {
            let span = tracing::Span::current();
            span.record("success", false);
            span.record("error", e.to_string().as_str());
            let kind = crate::session::helpers::session_compact::compact_error_kind(&e)
                .unwrap_or(crate::session::helpers::session_compact::CompactErrorKind::Failed);
            let detail =
                Self::user_facing_compact_error(&crate::sampling::error::acp_error_message(&e));
            return Err(e.data(
                crate::session::helpers::session_compact::compact_error_data(kind, &detail),
            ));
        }
        use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
        let tokens_after = self.chat_state_handle.get_total_tokens().await;
        let span = tracing::Span::current();
        span.record("post_tokens", tokens_after as i64);
        span.record("success", true);
        self.send_xai_notification(XaiSessionUpdate::AutoCompactCompleted {
            tokens_before: Some(total_tokens),
            tokens_after,
            elapsed_ms: None,
            summary_preview: None,
        })
        .await;
        self.emit_status_snapshot_detached();
        Ok(())
    }
    async fn emit_compact_cancelled(&self) -> Result<(), acp::Error> {
        Err(crate::session::helpers::session_compact::CompactFailure::cancelled_error())
    }
    /// Map a deterministic failure's error text to a fixed, content-free [`SuppressReason`] for the compaction span.
    fn classify_suppress_reason(error_msg: &str) -> SuppressReason {
        let m = error_msg.to_ascii_lowercase();
        if m.contains("spending-limit")
            || m.contains("spending limit")
            || m.contains("out of credits")
            || m.contains("usage balance exhausted")
            || m.contains("usage limit reached")
        {
            SuppressReason::CreditBlock
        } else if is_context_length_error(&m) {
            SuppressReason::Size
        } else if m.contains("status 401") || m.contains("unauthorized") {
            SuppressReason::Auth
        } else if m.contains("invalid_request_error") {
            SuppressReason::Schema
        } else {
            SuppressReason::Other
        }
    }
    /// Strip the known internal prefixes (wrappers nest), then apply the shared normalize sequence ([`normalize_compact_detail`]).
    fn user_facing_compact_error(raw: &str) -> String {
        use crate::session::helpers::session_compact::normalize_compact_detail;
        const INTERNAL_PREFIXES: &[&str] = &[
            COMPACT_FAILED_PREFIX,
            COMPACTION_FAILED_GUARD_PREFIX,
            xai_grok_compaction::sampler::SAMPLER_BUILD_FAILED_PREFIX,
            xai_grok_compaction::sampler::SAMPLER_START_FAILED_PREFIX,
        ];
        let mut rest = raw.trim();
        loop {
            let Some(prefix) = INTERNAL_PREFIXES.iter().find(|p| {
                rest.as_bytes()
                    .get(..p.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(p.as_bytes()))
            }) else {
                break;
            };
            let Some(stripped) = rest.get(prefix.len()..) else {
                break;
            };
            rest = stripped.trim_start();
        }
        normalize_compact_detail(rest)
    }
    /// Choose the post-compaction history for a forked session.
    /// Re-pin the inherited prefix, or release it when re-pinning would leave the fork over the context window.
    /// A release also sets the sticky flag and records the release span field (this runs within the `run_compact_inner` span).
    async fn resolve_forked_compacted_history(
        &self,
        compacted_history: Vec<ConversationItem>,
        prefix_len: usize,
        tokens_before: u64,
        context_window: u64,
    ) -> Vec<ConversationItem> {
        let full_conv = self.chat_state_handle.get_conversation().await;
        let compacted_len = compacted_history.len();
        let release_candidate = compacted_history.clone();
        match preserve_inherited_prefix(&full_conv, compacted_history, prefix_len) {
            Ok(preserved) => {
                let projected_preserved = project_preserved_reseed_tokens(
                    xai_chat_state::estimate_conversation_tokens(&preserved),
                    tokens_before,
                    xai_chat_state::estimate_conversation_tokens(&full_conv),
                );
                if projected_preserved > context_window {
                    self.compaction
                        .prefix_released
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::Span::current().record("compaction_prefix_released", true);
                    tracing::info!(
                        session_id = %self.session_info.id.0,
                        prefix_len,
                        projected_preserved,
                        "compaction: releasing inherited prefix under pressure"
                    );
                    release_candidate
                } else {
                    tracing::info!(
                        session_id = %self.session_info.id.0,
                        prefix_len,
                        compacted_len,
                        "Preserving inherited prefix across compaction"
                    );
                    preserved
                }
            }
            Err(original) => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    prefix_len,
                    conversation_len = full_conv.len(),
                    "Inherited prefix invalid, using compacted history as-is"
                );
                original
            }
        }
    }
    /// Inner implementation of compaction that supports an optional `auto_continue` payload for the checkpoint.
    #[tracing::instrument(
        name = "session.compact_inner",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            compaction_tokens_before = tracing::field::Empty,
            compaction_tokens_after = tracing::field::Empty,
            compaction_summary_chars = tracing::field::Empty,
            compaction_degenerate_rejections = tracing::field::Empty,
            compaction_input_overflow_rejections = tracing::field::Empty,
            compaction_deterministic_rejections = tracing::field::Empty,
            compaction_transient_rejections = tracing::field::Empty,
            compaction_attempts = tracing::field::Empty,
            compaction_trigger = tracing::field::Empty,
            compaction_trigger_pct = tracing::field::Empty,
            compaction_outcome = tracing::field::Empty,
            compaction_failure_reason = tracing::field::Empty,
            compaction_stop_reason = tracing::field::Empty,
            compaction_ttft_ms = tracing::field::Empty,
            compaction_stream_ms = tracing::field::Empty,
            compaction_delta_count = tracing::field::Empty,
            compaction_itl_max_ms = tracing::field::Empty,
            compaction_prefix_released = tracing::field::Empty,
        )
    )]
    async fn run_compact_inner(
        &self,
        auto_continue: Option<crate::extensions::notification::AutoContinueInfo>,
        trigger: xai_grok_telemetry::events::CompactionTrigger,
        lossy_input: bool,
    ) -> Result<(), acp::Error> {
        let (cancel, _cancel_scope) = self.compaction.cancel.enter();
        let tokens_before = self.chat_state_handle.get_total_tokens().await;
        tracing::Span::current().record("compaction_tokens_before", tokens_before as i64);
        self.signals_handle().record_compaction(tokens_before);
        let trigger_str = match trigger {
            xai_grok_telemetry::events::CompactionTrigger::Manual => "manual",
            xai_grok_telemetry::events::CompactionTrigger::Auto => "auto",
        };
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        {
            let span = tracing::Span::current();
            let trigger_pct = if context_window == 0 {
                0
            } else {
                ((tokens_before as f64 / context_window as f64) * 100.0).round() as i64
            };
            span.record("compaction_trigger_pct", trigger_pct);
            span.record("compaction_trigger", trigger_str);
        }
        let summary_strips_reasoning = sampling_config
            .as_ref()
            .map(|c| c.api_backend == ApiBackend::Messages)
            .unwrap_or(false);
        let model_id = sampling_config.map(|c| c.model).unwrap_or_default();
        let compaction = xai_grok_telemetry::events::CompactionScope::begin(
            xai_grok_telemetry::events::CompactionBeginParams {
                trigger,
                tokens_used: tokens_before,
                context_window,
                model_id: model_id.clone(),
                compaction_mode: match self.compaction.compaction_mode {
                    xai_chat_state::CompactionMode::Summary => {
                        xai_grok_telemetry::events::CompactionModeLabel::Summary
                    }
                    xai_chat_state::CompactionMode::Transcript => {
                        xai_grok_telemetry::events::CompactionModeLabel::Transcript
                    }
                    xai_chat_state::CompactionMode::Segments(_) => {
                        xai_grok_telemetry::events::CompactionModeLabel::Segments
                    }
                },
                is_subagent: self.startup_hints.is_subagent,
            },
        );
        let user_context = self.goal_compaction_user_context();
        let compact_source = trigger_str;
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PreCompact,
            xai_grok_hooks::event::HookPayload::PreCompact {
                source: compact_source.into(),
            },
            None,
        )
        .await;
        let max_retries = 3u32;
        let retry_delay_secs = 3u64;
        let (conv_len, system_message, full_conversation) = tokio::join!(
            self.chat_state_handle.get_conversation_len(),
            self.chat_state_handle.get_system_message(),
            self.chat_state_handle.get_conversation(),
        );
        let assembly_start = std::time::Instant::now();
        let segment_messages = if self.compaction.compaction_mode.writes_segments() {
            xai_chat_state::compaction_utils::prepare_conversation_for_segment(
                full_conversation.clone(),
            )
        } else {
            Vec::new()
        };
        let verbatim_input_enabled = self.compaction.verbatim_input && !lossy_input;
        let mut simplified_messages = if verbatim_input_enabled {
            xai_chat_state::compaction_utils::prepare_conversation_for_verbatim_summarization(
                full_conversation,
                summary_strips_reasoning,
            )
        } else {
            xai_chat_state::compaction_utils::prepare_conversation_for_summarization(
                full_conversation,
            )
        };
        if verbatim_input_enabled {
            simplified_messages =
                apply_turn_image_budget_and_prune(&self.chat_state_handle, simplified_messages)
                    .await;
        }
        let pre_compaction_ms = assembly_start.elapsed().as_millis() as u64;
        if conv_len == 0 {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "Compaction failed: conversation is empty (ChatStateActor may have died)"
            );
            return Err(acp::Error::internal_error().data(format!(
                "{COMPACTION_FAILED_GUARD_PREFIX}conversation is empty"
            )));
        }
        let system_message = match system_message {
            Some(msg) => msg,
            None => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    conversation_len = conv_len,
                    "Compaction failed: no system message in conversation history"
                );
                return Err(acp::Error::internal_error().data(format!(
                    "{COMPACTION_FAILED_GUARD_PREFIX}no system message in conversation history"
                )));
            }
        };
        if simplified_messages.is_empty() {
            tracing::error!(
                session_id = %self.session_info.id.0,
                conversation_len = conv_len,
                "Compaction failed: simplified conversation is empty"
            );
            return Err(acp::Error::internal_error().data(format!(
                "{COMPACTION_FAILED_GUARD_PREFIX}simplified conversation is empty"
            )));
        }
        if !simplified_messages
            .iter()
            .any(|msg| matches!(msg, ConversationItem::System(_)))
        {
            tracing::error!(
                session_id = %self.session_info.id.0,
                conversation_len = conv_len,
                simplified_len = simplified_messages.len(),
                "Compaction failed: no system message in simplified conversation"
            );
            return Err(acp::Error::internal_error().data(format!(
                "{COMPACTION_FAILED_GUARD_PREFIX}no system message in simplified conversation"
            )));
        }
        let sampling_config = self.reconstruct_full_config().await;
        let sampling_client = self.prepare_chat_completion(false).await?;
        let backend_search_active = self.backend_search_active();
        let effective_tool_defs: Vec<xai_grok_sampling_types::ToolDefinition> = self
            .prepare_tool_definitions()
            .await
            .into_iter()
            .filter(|td| !backend_search_active || td.function.name != "web_search")
            .collect();
        let compaction_tool_tokens =
            xai_chat_state::estimate_tool_definitions_tokens(&effective_tool_defs);
        let compaction_tools: Vec<xai_grok_sampling_types::ToolSpec> = effective_tool_defs
            .into_iter()
            .map(xai_grok_sampling_types::ToolSpec::from)
            .collect();
        let compaction_hosted_tools: Vec<xai_grok_sampling_types::HostedTool> =
            self.hosted_tools_for_turn();
        if lossy_input {
            simplified_messages = xai_chat_state::compaction_utils::fit_conversation_to_budget(
                simplified_messages,
                lossy_input_budget(context_window, compaction_tool_tokens),
            );
        }
        tracing::info!(
            num_tools = compaction_tools.len(),
            tool_tokens = compaction_tool_tokens,
            "Running compact with model '{}' (user model: '{}')",
            &sampling_config.model,
            &sampling_config.model
        );
        let mut last_error: Option<acp::Error> = None;
        let mut last_failure_outcome = CompactionOutcome::Failed;
        let mut last_failure_reason: Option<SuppressReason> = None;
        let mut input_stage = if verbatim_input_enabled {
            let (turns, stage) = start_verbatim_compact_turns(
                simplified_messages,
                compaction_tool_tokens,
                context_window,
            );
            simplified_messages = turns;
            stage
        } else {
            CompactInputStage::Lossy
        };
        let use_short_prompt = false;
        let started_at = chrono::Utc::now().to_rfc3339();
        let estimated_input_tokens =
            xai_chat_state::estimate_conversation_tokens(&simplified_messages);
        let wall_clock_budget_secs = self
            .agent
            .borrow()
            .compaction_policy()
            .wall_clock_budget_secs;
        let sampler = crate::session::helpers::full_replace_compaction::ShellCompactionSampler::new(
            use_short_prompt,
            user_context.clone(),
            compaction_tools.clone(),
            compaction_hosted_tools.clone(),
            compaction_tool_tokens,
            sampling_client,
            self.session_info.id.clone(),
            sampling_config.clone(),
            self.inference_idle_timeout,
            wall_clock_budget_secs,
            self.compaction.tool_choice,
            cancel.clone(),
        );
        let observer =
            crate::session::helpers::full_replace_compaction::ShellFullReplaceObserver::new(
                trigger,
                context_window,
                compaction.compaction_id.clone(),
                self.session_info.id.0.to_string(),
                estimated_input_tokens,
                retry_delay_secs,
            );
        let fr_config = xai_grok_compaction::FullReplaceConfig {
            max_attempts: max_retries,
            retry_delay_secs,
            sampling_timeout_secs: 0,
        };
        let mut request_turns = simplified_messages.clone();
        let mut input_overflow_rejections: u32 = 0;
        let mut compact_summary: Option<String> = None;
        while compact_summary.is_none() {
            match xai_grok_compaction::sample_full_replace_summary(
                &sampler,
                &request_turns,
                user_context.as_deref(),
                &fr_config,
                &observer,
            )
            .await
            {
                Ok(summary) => {
                    compact_summary = Some(summary.summary);
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::NothingToCompact) => {
                    last_error = Some(
                        acp::Error::internal_error()
                            .data(format!("{COMPACT_FAILED_PREFIX}nothing to compact")),
                    );
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::EmptyResponse) => {
                    last_failure_outcome = if observer.degenerate_seen() {
                        CompactionOutcome::Degenerate
                    } else {
                        CompactionOutcome::Transient
                    };
                    last_error = Some(acp::Error::internal_error().data(
                        observer.last_error_message().unwrap_or_else(|| {
                            format!("{COMPACT_FAILED_PREFIX}model returned empty response")
                        }),
                    ));
                    break;
                }
                Err(xai_grok_compaction::FullReplaceError::Sampler {
                    message,
                    deterministic,
                    context_overflow,
                }) => {
                    if cancel.is_cancelled()
                        || message.contains(
                            crate::session::helpers::session_compact::COMPACT_CANCELLED_MSG,
                        )
                    {
                        return self.emit_compact_cancelled().await;
                    }
                    if context_overflow {
                        let next_stage = match input_stage {
                            CompactInputStage::Verbatim => Some(CompactInputStage::VerbatimFitted),
                            CompactInputStage::VerbatimFitted => Some(CompactInputStage::Lossy),
                            CompactInputStage::Lossy => None,
                        };
                        if let Some(stage) = next_stage {
                            input_overflow_rejections += 1;
                            xai_grok_telemetry::session_ctx::log_event(
                                xai_grok_telemetry::events::CompactionRetryDegraded {
                                    trigger,
                                    reason: "input_overflow",
                                    from_stage: Some(input_stage.into()),
                                    to_stage: Some(stage.into()),
                                    summary_chars: None,
                                    attempt: observer.attempt_count(),
                                    context_window,
                                    compaction_id: compaction.compaction_id.clone(),
                                },
                            );
                            tracing::warn!(
                                session_id = %self.session_info.id.0,
                                ?stage,
                                error = %message,
                                "Compaction input overflowed deterministically; stepping down the input ladder to avoid an incompactable state"
                            );
                            let conv = self.chat_state_handle.get_conversation().await;
                            request_turns = match stage {
                                CompactInputStage::VerbatimFitted => {
                                    let verbatim = xai_chat_state::compaction_utils::prepare_conversation_for_verbatim_summarization(
                                        conv,
                                        summary_strips_reasoning,
                                    );
                                    let pruned = apply_turn_image_budget_and_prune(
                                            &self.chat_state_handle,
                                            verbatim,
                                        )
                                        .await;
                                    xai_chat_state::compaction_utils::fit_conversation_to_budget(
                                        pruned,
                                        fitted_input_budget(context_window, compaction_tool_tokens),
                                    )
                                }
                                CompactInputStage::Lossy => {
                                    xai_chat_state::compaction_utils::fit_conversation_to_budget(
                                        xai_chat_state::compaction_utils::prepare_conversation_for_summarization(
                                            conv,
                                        ),
                                        lossy_input_budget(context_window, compaction_tool_tokens),
                                    )
                                }
                                CompactInputStage::Verbatim => {
                                    unreachable!("ladder only steps forward")
                                }
                            };
                            input_stage = stage;
                            continue;
                        }
                        last_failure_outcome = CompactionOutcome::Deterministic;
                        last_failure_reason = Some(SuppressReason::Size);
                        last_error = Some(acp::Error::internal_error().data(message));
                        break;
                    }
                    if deterministic {
                        last_failure_outcome = CompactionOutcome::Deterministic;
                        let reason = Self::classify_suppress_reason(&message);
                        last_failure_reason = Some(reason);
                        last_error = Some(acp::Error::internal_error().data(message));
                        break;
                    }
                    last_failure_outcome = CompactionOutcome::Transient;
                    last_error = Some(acp::Error::internal_error().data(message));
                    break;
                }
            }
        }
        let telemetry = observer.into_telemetry();
        if let Some(request_chat_history) = sampler.take_last_attempted_items() {
            self.persist_compaction_request_artifact(
                request_chat_history,
                compaction_tools,
                use_short_prompt,
                &sampling_config.model,
                trigger,
                compact_summary
                    .as_deref()
                    .or(telemetry.last_rejected_summary.as_deref()),
                last_error.as_ref(),
                telemetry.attempts,
                telemetry.attempt_details,
                started_at,
            );
        }
        let compact_output = match compact_summary {
            Some(_) => sampler
                .take_last_success()
                .expect("a successful full-replace sample stashes its CompactOutput"),
            None => {
                let span = tracing::Span::current();
                span.record("compaction_attempts", telemetry.attempts as i64);
                span.record(
                    "compaction_degenerate_rejections",
                    telemetry.degenerate_rejections as i64,
                );
                span.record(
                    "compaction_input_overflow_rejections",
                    input_overflow_rejections as i64,
                );
                span.record(
                    "compaction_deterministic_rejections",
                    telemetry.deterministic_rejections as i64,
                );
                span.record(
                    "compaction_transient_rejections",
                    telemetry.transient_rejections as i64,
                );
                span.record("compaction_outcome", last_failure_outcome.as_ref());
                if let Some(reason) = last_failure_reason {
                    span.record("compaction_failure_reason", reason.as_ref());
                }
                return Err(last_error.unwrap_or_else(|| {
                    acp::Error::internal_error().data("compaction failed: unknown error")
                }));
            }
        };
        let generate_session_compact = compact_output.content.clone();
        let user_message_prefix = self.build_user_message_prefix().await;
        let conversation = self.chat_state_handle.get_conversation().await;
        let (discovered_agents_md, all_skills_for_compaction, _edited_paths, mut state_context) =
            if use_short_prompt {
                let empty_edited: std::collections::BTreeSet<String> = Default::default();
                let ctx = CompactionStateContext::build(
                    &conversation,
                    CompactionInputs {
                        goal_objective: self.goal_objective_for_compaction(),
                        ..Default::default()
                    },
                )
                .await;
                (Vec::<std::path::PathBuf>::new(), vec![], empty_edited, ctx)
            } else {
                let agents_md: Vec<std::path::PathBuf> = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .agents_md_reminded_paths()
                    .await
                    .into_iter()
                    .collect();
                let skills = self.slash_skills_for_resolve().await;
                let edited_paths = self.chat_state_handle.get_agent_edited_paths().await;
                let ctx = {
                    let bridge_tasks = self
                        .agent
                        .borrow()
                        .tool_bridge()
                        .list_background_tasks()
                        .await;
                    let pending_tasks: Vec<_> =
                        bridge_tasks.into_iter().filter(|t| !t.completed).collect();
                    let (execute_tool_name, monitor_tool_name) = if pending_tasks.is_empty() {
                        (None, None)
                    } else {
                        let agent_ref = self.agent.borrow();
                        let bridge = agent_ref.tool_bridge();
                        let empty = serde_json::json!({});
                        let execute = bridge
                            .render_prompt("${{ tools.by_kind.execute }}", &empty)
                            .await
                            .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                        let monitor = bridge
                            .render_prompt("${{ tools.by_kind.monitor }}", &empty)
                            .await
                            .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                        (execute, monitor)
                    };
                    let running_tasks: Vec<_> = pending_tasks
                        .into_iter()
                        .map(|t| {
                            let tool_name = match t.kind {
                                xai_grok_tools::computer::types::TaskKind::Monitor => {
                                    monitor_tool_name.clone()
                                }
                                xai_grok_tools::computer::types::TaskKind::Bash => {
                                    execute_tool_name.clone()
                                }
                            };
                            CompactionStateContext::task_summary(
                                t.task_id, t.command, "running", tool_name,
                            )
                        })
                        .collect();
                    let running_subagents = if let Some(ref event_tx) =
                        self.tool_context.subagent_event_tx
                    {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        use xai_grok_tools::implementations::grok_build::task::types::{
                            SubagentEvent, SubagentListActiveRequest,
                        };
                        let _ =
                            event_tx.send(SubagentEvent::ListActive(SubagentListActiveRequest {
                                parent_session_id: self.session_id_string(),
                                respond_to: tx,
                            }));
                        rx.await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| crate::session::helpers::compaction_context::RunningSubagentSummary {
                            subagent_id: s.subagent_id,
                            subagent_type: s.subagent_type,
                            description: s.description,
                            elapsed_ms: s.elapsed_ms,
                        })
                        .collect()
                    } else {
                        vec![]
                    };
                    let connected_mcp_servers = {
                        use crate::session::helpers::compaction_context::CompactionServerSummary;
                        use xai_grok_tools::implementations::search_tool::{
                            sanitize_description, truncate_description,
                        };
                        self.connected_server_summaries()
                            .into_iter()
                            .map(|s| {
                                let desc = s
                                    .description
                                    .map(|d| truncate_description(&sanitize_description(&d)))
                                    .filter(|d| !d.is_empty());
                                CompactionServerSummary {
                                    name: s.name,
                                    tool_count: s.tool_count,
                                    description: desc,
                                }
                            })
                            .collect()
                    };
                    let todos = {
                        use crate::session::helpers::compaction_context::{
                            TodoSummary, TodoSummaryStatus,
                        };
                        use crate::tools::todo::{TodoState, TodoStatus};
                        use xai_grok_tools::types::resources::State;
                        let bridge = self.agent.borrow().tool_bridge().clone();
                        bridge
                            .read_resource::<State<TodoState>>()
                            .await
                            .map(|s| {
                                s.0.todo_items_with_ids()
                                    .map(|(id, item)| TodoSummary {
                                        id: id.clone(),
                                        content: item.content.clone(),
                                        status: match item.status {
                                            TodoStatus::Pending => TodoSummaryStatus::Pending,
                                            TodoStatus::InProgress => TodoSummaryStatus::InProgress,
                                            TodoStatus::Completed => TodoSummaryStatus::Completed,
                                            TodoStatus::Cancelled => TodoSummaryStatus::Cancelled,
                                        },
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let scheduled_loops = {
                        use crate::session::helpers::compaction_context::ScheduledLoopSummary;
                        let now = chrono::Utc::now();
                        let bridge = self.agent.borrow().tool_bridge().clone();
                        bridge
                        .list_scheduled_tasks()
                        .await
                        .into_iter()
                        .filter(|t| t.pending_fire_at(now).is_some())
                        .map(|t| {
                            let next_fire_at = format_loop_next_fire(
                                t.next_fire_at(),
                                now,
                            );
                            ScheduledLoopSummary {
                                task_id: t.id,
                                interval: xai_grok_tools::implementations::grok_build::scheduler::interval::interval_to_human(
                                    t.interval_secs,
                                ),
                                next_fire_at,
                                prompt: t.prompt,
                                recurring: t.recurring,
                                durable: t.durable,
                            }
                        })
                        .collect()
                    };
                    let workflows = {
                        use crate::session::helpers::compaction_context::WorkflowRunSummary;
                        let tracker = self.workflow_tracker().await;
                        let guard = tracker.lock();
                        guard
                            .list()
                            .into_iter()
                            .filter(|r| !r.status.is_terminal())
                            .map(|r| WorkflowRunSummary {
                                elapsed_ms: guard.elapsed_ms(&r.run_id),
                                name: r.name,
                                run_id: r.run_id,
                                status: r.status.as_ref().to_string(),
                                objective: r.objective,
                                current_phase: r.current_phase,
                                agents_used: r.agents_used,
                                agent_budget: r.agent_budget,
                            })
                            .collect::<Vec<_>>()
                    };
                    let workflow_tool_name = if workflows.is_empty() {
                        None
                    } else {
                        use xai_grok_tools::types::tool::ToolKind;
                        self.agent
                            .borrow()
                            .tool_bridge()
                            .tool_for_kind(ToolKind::Workflow)
                            .await
                            .filter(|s| !s.is_empty())
                    };
                    CompactionStateContext::build(
                        &conversation,
                        CompactionInputs {
                            running_tasks,
                            running_subagents,
                            agent_edited_paths: edited_paths.clone(),
                            connected_mcp_servers,
                            todos,
                            scheduled_loops,
                            workflows,
                            workflow_tool_name,
                            goal_objective: self.goal_objective_for_compaction(),
                            ..Default::default()
                        },
                    )
                    .await
                };
                (agents_md, skills, edited_paths, ctx)
            };
        if self.is_cursor_harness() {
            state_context.images = CompactionImageContext::default();
        }
        let harvested_paths = std::mem::take(&mut state_context.images.attached_paths);
        let (kept, dropped_paths) = if harvested_paths.is_empty() {
            (Vec::new(), 0)
        } else {
            match crate::session::persistence::ensure_owner_only_session_dir(&self.session_info) {
                Ok(session_dir) => {
                    retain_session_asset_files(
                        harvested_paths,
                        &crate::session::image_describe::session_assets_dir(&session_dir),
                    )
                    .await
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "compaction: session dir unavailable; attached image paths dropped"
                    );
                    (Vec::new(), harvested_paths.len())
                }
            }
        };
        state_context.images.attached_paths = kept;
        if dropped_paths > 0 {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                dropped_paths,
                "compaction: dropped attached image paths that are not session asset files"
            );
        }
        use crate::session::helpers::compaction_context::SubagentToolNames;
        let subagent_tool_names: Option<SubagentToolNames> =
            if use_short_prompt || state_context.running_subagents.is_empty() {
                None
            } else {
                let agent_ref = self.agent.borrow();
                let bridge = agent_ref.tool_bridge();
                let empty = serde_json::json!({});
                let poll_name = bridge
                    .render_prompt("${{ tools.by_kind.background_task_action }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                let cancel_name = bridge
                    .render_prompt("${{ tools.by_kind.kill_task_action }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                match (poll_name, cancel_name) {
                    (Some(poll), Some(cancel)) => Some(SubagentToolNames { poll, cancel }),
                    (poll, cancel) => {
                        tracing::warn!(
                            session_id = %self.session_info.id.0,
                            poll_resolved = poll.is_some(),
                            cancel_resolved = cancel.is_some(),
                            "could not resolve subagent tool names, \
                             omitting subagent reminder from compacted conversation"
                        );
                        None
                    }
                }
            };
        use crate::session::helpers::compaction_context::McpToolNames;
        let mcp_tool_names: Option<McpToolNames> =
            if use_short_prompt || state_context.connected_mcp_servers.is_empty() {
                None
            } else {
                let agent_ref = self.agent.borrow();
                let bridge = agent_ref.tool_bridge();
                let empty = serde_json::json!({});
                let search_name = bridge
                    .render_prompt("${{ tools.by_kind.search_tool }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                let call_name = bridge
                    .render_prompt("${{ tools.by_kind.use_tool }}", &empty)
                    .await
                    .filter(|s| !s.is_empty() && !s.contains("by_kind"));
                match (search_name, call_name) {
                    (Some(search), Some(call)) => Some(McpToolNames { search, call }),
                    _ => None,
                }
            };
        let memory_backend_impl = {
            let g = self.memory.storage.borrow();
            g.as_ref()
                .zip(self.memory.backend_params.as_ref())
                .map(|(storage, params)| {
                    crate::session::memory::MemoryBackendImpl::from_session_params(
                        storage.clone(),
                        &crate::session::memory::MemoryBackendParams {
                            search_source:
                                crate::session::memory::MemorySearchSource::CompactionRecovery,
                            ..params.clone()
                        },
                    )
                })
        };
        let memory_opt_out = false;
        let memory_ref: Option<&dyn xai_grok_tools::types::memory_backend::MemoryBackend> =
            if memory_opt_out {
                None
            } else {
                memory_backend_impl
                    .as_ref()
                    .map(|b| b as &dyn xai_grok_tools::types::memory_backend::MemoryBackend)
            };
        let suppress_state_reminder = false;
        let workflow_listing = self.workflow_listing_for_prompt();
        let system_reminder = if suppress_state_reminder {
            workflow_listing.as_deref().map(|listing| {
                let tag = self.reminder_wrapper_tag();
                format!("<{tag}>\n## Available Workflows\n{listing}\n</{tag}>")
            })
        } else {
            to_system_reminder(
                &state_context,
                &discovered_agents_md,
                &all_skills_for_compaction,
                memory_ref,
                subagent_tool_names.as_ref(),
                mcp_tool_names.as_ref(),
                workflow_listing.as_deref(),
            )
            .await
        };
        let v2_memory_context = if self.memory.can_expose_v2() {
            if let Some(storage) = self.memory.storage() {
                let compact_index = self.memory.v2_config.compact_index_enabled;
                match tokio::task::spawn_blocking(move || {
                    crate::session::helpers::memory_context::format_v2_memory_context(
                        &storage,
                        compact_index,
                    )
                })
                .await
                {
                    Ok(Ok(context)) => {
                        let injected_bytes = context.content.len() as u64;
                        self.memory.record_injected_bytes(injected_bytes);
                        crate::session::memory_observation::log_memory_injection(
                            self.session_info.id.to_string(),
                            xai_grok_telemetry::memory_telemetry::MemoryInjectionOutcome::Results,
                            crate::session::memory_observation::MemoryInjectionMetrics {
                                result_count: context
                                    .global_entry_count
                                    .saturating_add(context.workspace_entry_count),
                                injected_bytes,
                                estimated_tokens: xai_token_estimation::estimate_tokens(
                                    &context.content,
                                ),
                                global_entry_count: context.global_entry_count,
                                workspace_entry_count: context.workspace_entry_count,
                                compact_index,
                                ..Default::default()
                            },
                        );
                        Some(context.content)
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            %error,
                            "MEMORY_COMPACTION_RECOVERY: failed to refresh v2 manifests"
                        );
                        crate::session::memory_observation::log_memory_injection(
                            self.session_info.id.to_string(),
                            xai_grok_telemetry::memory_telemetry::MemoryInjectionOutcome::Error,
                            crate::session::memory_observation::MemoryInjectionMetrics {
                                compact_index,
                                ..Default::default()
                            },
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            %error,
                            "MEMORY_COMPACTION_RECOVERY: v2 manifest task failed"
                        );
                        crate::session::memory_observation::log_memory_injection(
                            self.session_info.id.to_string(),
                            xai_grok_telemetry::memory_telemetry::MemoryInjectionOutcome::Error,
                            crate::session::memory_observation::MemoryInjectionMetrics {
                                compact_index,
                                ..Default::default()
                            },
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let system_reminder = match (system_reminder, v2_memory_context) {
            (Some(mut reminder), Some(context)) => {
                reminder.push_str("\n\n");
                reminder.push_str(&context);
                Some(reminder)
            }
            (None, Some(context)) => Some(context),
            (reminder, None) => reminder,
        };
        let system_reminder = {
            let plan_path = {
                let guard = self.plan_mode.lock();
                guard
                    .is_active()
                    .then(|| guard.plan_file_path().to_path_buf())
            };
            if let Some(plan_path) = plan_path {
                let plan_has_content =
                    crate::session::plan_mode::plan_file_has_content(&plan_path).await;
                let template = crate::session::plan_mode::plan_mode_reminder_full_template();
                let wrapper = self.reminder_wrapper_tag();
                let rendered = self
                    .render_plan_template(template, &plan_path, plan_has_content)
                    .await;
                match (system_reminder, rendered) {
                    (Some(mut existing), Some(plan_section)) => {
                        if let Some(pos) = existing.rfind("</system-reminder>") {
                            existing.insert_str(pos, &format!("\n\n{}\n", plan_section));
                        } else {
                            existing.push_str("\n\n");
                            existing.push_str(&plan_section);
                        }
                        Some(existing)
                    }
                    (None, Some(plan_section)) => Some(format!(
                        "<{tag}>\n{body}\n</{tag}>",
                        tag = wrapper,
                        body = plan_section,
                    )),
                    (existing, None) => {
                        tracing::warn!(
                            session_id = %self.session_info.id.0,
                            "compaction: plan mode active but template render failed"
                        );
                        existing
                    }
                }
            } else {
                system_reminder
            }
        };
        let system_reminder = if let Some(goal_section) = self.compaction_goal_section().await {
            use crate::session::acp_session::splice_goal_section;
            match system_reminder {
                Some(existing) => Some(splice_goal_section(&existing, &goal_section)),
                None => {
                    let tag = self.reminder_wrapper_tag();
                    Some(format!("<{tag}>\n{goal_section}\n</{tag}>"))
                }
            }
        } else {
            system_reminder
        };
        if let Some(ref recovery_backend) = memory_backend_impl {
            let n = recovery_backend
                .search_counter
                .load(std::sync::atomic::Ordering::Relaxed);
            if n > 0 {
                self.memory
                    .compaction_recovery_count
                    .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    count = n,
                    "MEMORY_COMPACTION_RECOVERY: {} search(es) performed",
                    n,
                );
            }
        }
        let agents_md_reminder = self.agent.borrow().agents_md_user_reminder();
        let compaction_context = state_context.for_compaction();
        let compaction_state_context: &CompactionStateContext = &compaction_context;
        tracing::debug!(
            session_id = %self.session_info.id.0,
            has_last_user_query = compaction_state_context.last_user_query.is_some(),
            last_turn_image_parts = compaction_state_context.images.last_turn_image_parts.len(),
            has_last_turn_image_files = compaction_state_context
                .images
                .last_turn_image_files
                .is_some(),
            "compaction: last-turn image context"
        );
        let transcript_hint = self.transcript_hint();
        let summary_count = self
            .compaction
            .count
            .load(std::sync::atomic::Ordering::Relaxed);
        let apply_start = std::time::Instant::now();
        let raw_compacted = build_compacted_history(CompactedHistoryInput {
            system_message: system_message.clone(),
            user_message_prefix: user_message_prefix.clone(),
            agents_md_reminder: agents_md_reminder.clone(),
            state_context: compaction_state_context,
            compaction_summary: generate_session_compact.clone(),
            system_reminder: system_reminder.clone(),
            summary_before_recent: use_short_prompt,
            transcript_hint: transcript_hint.clone(),
            summary_count,
        });
        let sanitize_result = sanitize_compacted_history(raw_compacted);
        let compacted_history = if sanitize_result.stripped_tool_call_ids.is_empty() {
            sanitize_result.items
        } else {
            tracing::warn!(
                session_id = %self.session_info.id,
                stripped_count = sanitize_result.stripped_tool_call_ids.len(),
                stripped_ids = ?sanitize_result.stripped_tool_call_ids,
                "compaction: stripped orphaned ToolResults from compacted history"
            );
            sanitize_result.items
        };
        let remaining_violations = validate_compacted_history(&compacted_history);
        let compacted_history = if remaining_violations.is_empty() {
            compacted_history
        } else {
            tracing::error!(
                session_id = %self.session_info.id,
                violation_count = remaining_violations.len(),
                violation_ids = ?remaining_violations,
                "compaction: sanitized history still has invalid ToolResults -- \
                 falling back to minimal compacted history (no recent_messages)"
            );
            build_compacted_history(CompactedHistoryInput {
                system_message,
                user_message_prefix,
                agents_md_reminder,
                state_context: &state_context.for_compaction(),
                compaction_summary: generate_session_compact.clone(),
                system_reminder,
                summary_before_recent: use_short_prompt,
                transcript_hint,
                summary_count,
            })
        };
        let post_compaction_ms = apply_start.elapsed().as_millis() as u64;
        let prompt_index_at_compaction = self.chat_state_handle.get_prompt_index().await;
        let original_user_info = self
            .chat_state_handle
            .get_conversation_item_at(1)
            .await
            .and_then(|item| match item {
                ConversationItem::User(parts) => {
                    parts.content.into_iter().next().and_then(|p| match p {
                        xai_grok_sampling_types::ContentPart::Text { text } => {
                            Some(text.as_ref().to_owned())
                        }
                        _ => None,
                    })
                }
                _ => None,
            });
        if cancel.is_cancelled() {
            return self.emit_compact_cancelled().await;
        }
        let segments_queued = u32::from(
            self.persist_compaction_segment(&segment_messages, &generate_session_compact),
        );
        self.chat_state_handle
            .record_compaction_at(prompt_index_at_compaction);
        self.persist_compaction_checkpoint(
            &compacted_history,
            prompt_index_at_compaction,
            auto_continue,
            original_user_info,
        );
        let prefix_len = if self
            .compaction
            .prefix_released
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0
        } else {
            self.startup_hints.inherited_prefix_len.unwrap_or(0)
        };
        let compacted_history = if prefix_len == 0 {
            compacted_history
        } else {
            self.resolve_forked_compacted_history(
                compacted_history,
                prefix_len,
                tokens_before,
                context_window,
            )
            .await
        };
        self.chat_state_handle
            .replace_conversation_for_compaction(compacted_history);
        self.reseed_active_goal_after_compaction().await;
        let new_len = self.chat_state_handle.get_conversation_len().await;
        self.last_idle_flush_conversation_len
            .store(new_len, std::sync::atomic::Ordering::Relaxed);
        self.memory
            .context_injected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if self.memory.is_enabled() {
            tracing::info!(target: xai_grok_telemetry::memory_log::TARGET, "MEMORY_COMPACT: post-compaction reset, next turn re-checks injection (search only if no block persisted)");
        }
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::PlanState(
                crate::tools::todo::TodoState::default(),
            ));
        self.agent
            .borrow()
            .tool_bridge()
            .on_agents_md_compaction()
            .await;
        self.agent
            .borrow()
            .tool_bridge()
            .on_skill_discovery_compaction()
            .await;
        self.rearm_failed_server_announcements().await;
        if self
            .tool_bridge_handle()
            .toolset()
            .tool_name_for_kind(xai_grok_tools::types::tool::ToolKind::UseTool)
            .is_some()
            && let Some(hint) = self.rendered_mcp_hint().await
        {
            self.push_system_reminder_with_tag(&hint, self.reminder_wrapper_tag());
        }
        self.plan_mode.lock().reset_after_compaction();
        self.persist_plan_mode_state();
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PostCompact,
            xai_grok_hooks::event::HookPayload::PostCompact {
                source: compact_source.into(),
            },
            None,
        )
        .await;
        let tokens_after = self.chat_state_handle.get_total_tokens().await;
        {
            let span = tracing::Span::current();
            span.record("compaction_tokens_after", tokens_after as i64);
            span.record(
                "compaction_summary_chars",
                compact_output.content.chars().count() as i64,
            );
            span.record("compaction_attempts", telemetry.attempts as i64);
            span.record(
                "compaction_degenerate_rejections",
                telemetry.degenerate_rejections as i64,
            );
            span.record(
                "compaction_input_overflow_rejections",
                input_overflow_rejections as i64,
            );
            span.record(
                "compaction_deterministic_rejections",
                telemetry.deterministic_rejections as i64,
            );
            span.record(
                "compaction_transient_rejections",
                telemetry.transient_rejections as i64,
            );
            let stop_reason = compact_output.stop_reason.as_deref().unwrap_or("stop");
            span.record("compaction_stop_reason", stop_reason);
            let outcome = if compact_output.truncated {
                CompactionOutcome::Truncated
            } else {
                CompactionOutcome::Success
            };
            span.record("compaction_outcome", outcome.as_ref());
            span.record("compaction_delta_count", compact_output.delta_count as i64);
            if let Some(ms) = compact_output.ttft_ms {
                span.record("compaction_ttft_ms", ms as i64);
            }
            if let Some(ms) = compact_output.stream_ms {
                span.record("compaction_stream_ms", ms as i64);
            }
            if let Some(ms) = compact_output.itl_max_ms {
                span.record("compaction_itl_max_ms", ms as i64);
            }
        }
        compaction.complete(
            xai_grok_telemetry::events::CompactionCompleteStats {
                tokens_after,
                segments_queued,
                degenerate_retries: telemetry.degenerate_rejections,
                input_overflow_retries: input_overflow_rejections,
            },
            xai_grok_telemetry::events::CompactionTiming {
                model_wait_ms: compact_output.model_wait_ms(),
                pre_compaction_ms: Some(pre_compaction_ms),
                post_compaction_ms: Some(post_compaction_ms),
            },
        );
        Ok(())
    }
    /// The request's token estimate exceeds the failed response's reported context window.
    /// The mid-salvage truncated-complete arm must see overflows even while compaction is suppressed.
    pub(crate) async fn estimate_exceeds_error_context_window(
        &self,
        err: &xai_grok_sampler::SamplingErrorInfo,
    ) -> bool {
        let Some(ref metadata) = err.model_metadata else {
            return false;
        };
        let Some(context_window) = metadata.context_window else {
            return false;
        };
        if context_window == 0 {
            return false;
        }
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        estimated_total > context_window
    }
    /// `created_at` is taken from the caller-supplied `started_at` (captured before the retry loop) rather than `Utc::now()` here.
    /// Transient retries then don't skew the timestamp away from when the call actually started.
    /// Best-effort: send-failures are logged at `warn` and never shown to the user, because the artifact is purely for offline analysis.
    #[allow(clippy::too_many_arguments)]
    fn persist_compaction_request_artifact(
        &self,
        chat_history: Vec<ConversationItem>,
        tools: Vec<xai_grok_sampling_types::ToolSpec>,
        use_short_prompt: bool,
        model: &str,
        trigger: xai_grok_telemetry::events::CompactionTrigger,
        summary: Option<&str>,
        error: Option<&acp::Error>,
        attempts: u32,
        attempt_details: Vec<CompactionAttempt>,
        started_at: String,
    ) {
        use crate::extensions::notification::CompactionRequestFile;
        let request_id = uuid::Uuid::new_v4().to_string();
        let trigger_str = match trigger {
            xai_grok_telemetry::events::CompactionTrigger::Manual => "manual",
            xai_grok_telemetry::events::CompactionTrigger::Auto => "auto",
        };
        let prompt_variant = if use_short_prompt {
            "short"
        } else {
            "detailed"
        };
        let error_str = error.map(|e| {
            e.data
                .as_ref()
                .and_then(|d| d.as_str())
                .unwrap_or("<no error data>")
                .to_owned()
        });
        let artifact = CompactionRequestFile {
            schema_version: 2,
            request_id,
            created_at: started_at,
            trigger: trigger_str.to_owned(),
            prompt_variant: prompt_variant.to_owned(),
            model: model.to_owned(),
            chat_history,
            tools,
            summary: summary.map(str::to_owned),
            error: error_str,
            attempts,
            attempt_details,
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CompactionRequest(artifact))
            .is_err()
        {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                "Failed to send compaction request artifact to persistence channel"
            );
        }
    }
    /// Persist a compaction checkpoint.
    /// Writes the compacted history to a separate file and records a `CompactionCheckpoint` marker in `updates.jsonl`.
    /// `auto_continue` should be `Some` when this compaction was triggered by auto-compact and an auto-continue prompt will follow.
    fn persist_compaction_checkpoint(
        &self,
        compacted_history: &[ConversationItem],
        prompt_index_at_compaction: usize,
        auto_continue: Option<crate::extensions::notification::AutoContinueInfo>,
        original_user_info: Option<String>,
    ) {
        use crate::extensions::notification::{
            CompactionCheckpointFile, CompactionCheckpointInfo, SessionUpdate as XaiSessionUpdate,
        };
        let checkpoint_id = uuid::Uuid::new_v4().to_string();
        let checkpoint_file = format!("compaction_checkpoints/{checkpoint_id}.json");
        let created_at = chrono::Utc::now().to_rfc3339();
        let file_data = CompactionCheckpointFile {
            checkpoint_id: checkpoint_id.clone(),
            prompt_index_at_compaction,
            compacted_history: compacted_history.to_vec(),
            schema_version: 1,
            created_at: created_at.clone(),
            original_user_info,
            reread_file_paths: vec![],
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CompactionCheckpoint(file_data))
            .is_err()
        {
            tracing::warn!("Failed to send compaction checkpoint file to persistence channel");
        }
        let info = CompactionCheckpointInfo {
            checkpoint_id,
            prompt_index_at_compaction,
            checkpoint_file,
            auto_continue,
            schema_version: 1,
            created_at,
        };
        self.persist_xai_update_only(XaiSessionUpdate::CompactionCheckpoint(Box::new(info)));
        tracing::info!(
            prompt_index_at_compaction,
            "Persisted compaction checkpoint"
        );
    }
}
