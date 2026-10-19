//! Idle/end auto-title refresh on `SessionActor`.
//!
//! The first title comes from the fast first-prompt path ([`crate::session::summary::SummaryGenerator`]).
//! This side-call re-titles from the whole conversation whenever the session settles idle, with one last-chance run at session end,
//! so even a single-prompt long-running session gets a summary title instead of the raw first prompt.
//! A manual `/rename` freezes the refresh (`/rename --auto` reopens it).
//! The refresh is best-effort and generation-guarded; persistence never overwrites a manual title.

use super::*;

use super::side_call::AuxCall;
use crate::session::helpers::{session_recap, session_summary, CACHE_MISS_SIDE_MODEL as TITLE_REFRESH_SIDE_MODEL};

/// Upper bound on the title-refresh model call so a hung backend cannot hold the one-at-a-time refresh slot indefinitely.
/// Generous relative to slow aux chains (observed mimo TTFT can exceed 80s); a hung backend still cannot hold the slot forever.
const TITLE_REFRESH_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Titles are 5-10 words; a hard output cap keeps a runaway sample short and cheap.
const TITLE_REFRESH_MAX_OUTPUT_TOKENS: u32 = 256;

impl SessionActor {
    /// Spawn a title refresh after the session settles idle, unless the title is frozen or one is already running.
    /// No-op for subagents and when post-turn side-calls are disabled (title refresh shares the `turn_summary_enabled` gate).
    /// At most one refresh runs at a time and it is left to finish; [`Self::refresh_title`] decides whether the conversation changed since the last success.
    pub(crate) fn maybe_refresh_title(self: &Arc<Self>) {
        if !self.title_refresh_enabled || self.startup_hints.is_subagent {
            return;
        }
        if self.title_refresh_frozen.get() {
            return;
        }
        // One refresh at a time: a whole-conversation title doesn't need the very latest turn.
        // The check is `is_finished` (not just `is_some`) so a panicked task can't wedge the slot shut.
        if self
            .title_refresh_task
            .borrow()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let generation = self.title_refresh_generation.get();
        let actor = self.clone();
        let task = tokio::task::spawn_local(async move {
            actor.refresh_title(generation).await;
            if actor.title_refresh_generation.get() == generation {
                *actor.title_refresh_task.borrow_mut() = None;
            }
        });
        *self.title_refresh_task.borrow_mut() = Some(task);
    }

    /// Last-chance refresh at session end: the idle trigger may never have run (quit mid-turn) or its model call may have failed.
    /// Awaits an in-flight refresh first (it already describes the settled conversation), then runs one synchronously when the conversation still has no success recorded for its current shape.
    /// Called from `fire_session_end_hooks`, so guards plus the model-call timeout bound how long session teardown can wait.
    pub(crate) async fn finalize_title_refresh(&self) {
        if !self.title_refresh_enabled
            || self.startup_hints.is_subagent
            || self.title_refresh_frozen.get()
        {
            return;
        }
        // Take the handle out before awaiting: the task's completion closure also borrows the slot.
        if let Some(task) = self.title_refresh_task.borrow_mut().take() {
            let _ = task.await;
        }
        if self.title_refresh_frozen.get() {
            return;
        }
        self.refresh_title(self.title_refresh_generation.get())
            .await;
    }

    /// A manual rename freezes the auto refresh (so a racing in-flight refresh can't flip the title and no later refresh fights the user's title).
    /// `/rename --auto` reopens it; both directions clear the success marker so the next idle/end refresh runs against the renamed conversation.
    pub(crate) fn on_title_renamed(&self, manual: bool) {
        self.abort_title_refresh();
        self.title_refresh_frozen.set(manual);
        self.title_refresh_done_turns.set(None);
    }

    /// Bumps the generation so a task that finishes after the abort neither persists its result nor counts as the idle/end attempt.
    /// Called on rename, rewind (the snapshot is now stale) and shutdown (free the actor `Arc`); a new prompt deliberately does not abort it.
    pub(crate) fn abort_title_refresh(&self) {
        self.title_refresh_generation
            .set(self.title_refresh_generation.get().wrapping_add(1));
        if let Some(task) = self.title_refresh_task.borrow_mut().take() {
            task.abort();
        }
    }

    /// `generation` is the spawn-time token.
    /// Only a stale attempt, one whose generation was bumped by an abort (rename, rewind, shutdown), bails without committing.
    /// A failed model call commits nothing, so the next idle/end event retries.
    async fn refresh_title(&self, generation: u64) {
        let conversation = self.chat_state_handle.get_conversation().await;
        let turns = session_recap::main_turn_count(&conversation);

        // Nothing human to title yet (prefix/system items only), already titled for this turn shape, or pinned by a manual rename.
        if turns == 0
            || self.title_refresh_done_turns.get() == Some(turns)
            || self.title_refresh_frozen.get()
        {
            return;
        }

        let title = self.generate_refreshed_title(conversation).await;

        // No await past here, so an abort can only land before the commit.
        if self.title_refresh_generation.get() != generation {
            return;
        }

        if let Some(title) = title {
            self.title_refresh_done_turns.set(Some(turns));
            tracing::info!(turns, chars = title.len(), "session title refreshed");
            // The persistence actor overwrites an auto title but never a manual `/rename` (checked under the summary lock)
            // It notifies clients only when the write lands
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::RegenerateTitle(title));
        } else {
            tracing::warn!(
                turns,
                "title refresh failed; will retry on the next idle/end"
            );
        }
    }

    /// One tool-free model call producing a cleaned whole-conversation title, or `None` on any setup/model failure or empty output.
    ///
    /// Prefers [`TITLE_REFRESH_SIDE_MODEL`] so the main conversation's cache pool is not displaced by a 0%-hit title body.
    /// When the side model is missing from the catalog (or already the session model), falls back to the parent-cached path.
    async fn generate_refreshed_title(
        &self,
        conversation: Vec<ConversationItem>,
    ) -> Option<String> {
        self.refresh_token_if_expired().await;
        let session_config = self.reconstruct_full_config().await;

        let side_cfg = if self.models_manager.model_in_catalog(TITLE_REFRESH_SIDE_MODEL)
            && session_config.model != TITLE_REFRESH_SIDE_MODEL
        {
            self.resolve_aux_sampler_config(TITLE_REFRESH_SIDE_MODEL)
                .await
                .map(|mut cfg| {
                    crate::agent::config::stamp_session_local_sampler_fields(
                        &mut cfg,
                        &session_config,
                        self.client_identifier.clone(),
                        session_config.max_retries,
                    );
                    cfg
                })
        } else {
            None
        };

        // Isolated side model: do not share the parent `prompt_cache_key` (different KV pool; sharing would only add noise).
        let shared_parent_prefix = side_cfg.is_none();
        let (client, model, context_window, reasoning_effort, strip_reasoning): (
            xai_grok_sampler::SamplingClient,
            String,
            u64,
            Option<xai_grok_sampling_types::ReasoningEffort>,
            bool,
        ) = if let Some(cfg) = side_cfg {
            let model = cfg.model.clone();
            let context_window = cfg.context_window;
            let reasoning_effort = cfg.reasoning_effort;
            // Not sharing the parent prefix: strip reasoning to shrink the whole-conversation replay.
            let strip_reasoning = true;
            match xai_grok_sampler::SamplingClient::new(cfg) {
                Ok(c) => (c, model, context_window, reasoning_effort, strip_reasoning),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "title refresh: failed to prepare side sampling client"
                    );
                    return None;
                }
            }
        } else {
            let setup = match self.prepare_side_call().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "title refresh: failed to prepare sampling client"
                    );
                    return None;
                }
            };
            (
                setup.client,
                setup.model,
                setup.context_window,
                setup.reasoning_effort,
                setup.strip_reasoning,
            )
        };

        let instruction = session_summary::title_refresh_instruction(self.reminder_wrapper_tag());
        let items = session_recap::budget_instruction_items(
            conversation,
            instruction,
            strip_reasoning,
            context_window,
        );
        // Deliberately tool-free: unlike the turn summary, the request carries no tools
        // The model then can't spend the call on a tool invocation that would leave empty text and waste the attempt
        let conv_id = format!("title-refresh-{}", uuid::Uuid::new_v4());
        let req_id = format!("xai-title-refresh-{}", uuid::Uuid::new_v4());
        let request = if shared_parent_prefix {
            // Parent path keeps the shared prompt-cache prefix for the common same-model case.
            let mut request = self.parent_cached_request(AuxCall {
                items,
                tools: Vec::new(),
                hosted_tools: Vec::new(),
                model,
                reasoning_effort,
                backend: client.api_backend(),
                conv_id,
                req_id,
            });
            request.max_output_tokens = Some(TITLE_REFRESH_MAX_OUTPUT_TOKENS);
            request
        } else {
            // Side model: unique pck, tool-free, capped output.
            ConversationRequest {
                items,
                tools: Vec::new(),
                hosted_tools: Vec::new(),
                model: Some(model),
                reasoning_effort,
                x_grok_conv_id: Some(conv_id),
                x_grok_req_id: Some(req_id),
                x_grok_session_id: Some(self.session_info.id.to_string()),
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                prompt_cache_key: Some(format!("title-refresh-{}", uuid::Uuid::new_v4())),
                max_output_tokens: Some(TITLE_REFRESH_MAX_OUTPUT_TOKENS),
                length_policy: xai_grok_sampling_types::LengthPolicy::Fail,
                ..Default::default()
            }
        };

        let response = match tokio::time::timeout(
            TITLE_REFRESH_MODEL_TIMEOUT,
            client.conversation_collect(request),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "title refresh: model call failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = TITLE_REFRESH_MODEL_TIMEOUT.as_secs(),
                    "title refresh: model call timed out"
                );
                return None;
            }
        };
        super::side_call::log_prompt_cache_usage("title_refresh", client.api_backend(), &response);
        let title = session_summary::clean_title_text(&response.assistant_text());
        if title.is_empty() {
            tracing::debug!("title refresh: model returned empty title");
            return None;
        }
        Some(title)
    }
}
