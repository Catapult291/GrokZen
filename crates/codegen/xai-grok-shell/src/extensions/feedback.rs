//! `x.ai/feedback`, `x.ai/feedback/dismiss`, `x.ai/btw`, and `x.ai/review/*` extension handlers.
//!
//! - `feedback` and `feedback/dismiss`: persist user ratings and text locally and forward to cli-chat-proxy.
//! - `btw`: dispatch a side question to the active session via `SessionCommand::SideQuestion` and return the answer.
//! - `review/comment` and `review/comment/delete`: record inline code review events to cloud storage.

use std::sync::Arc;

use agent_client_protocol as acp;
use prod_mc_cli_chat_proxy_types::feedback_types::{
    FeedbackImage, FeedbackSubmission, validate_feedback_images,
};

use super::{ExtResult, btw, parse_params, review};
use crate::agent::MvpAgent;
use crate::session::persistence::{LocalFeedbackEntry, UserFeedbackEntry};
use crate::session::{
    ClientFeedbackInput, FeedbackContext, FeedbackOutcome, FeedbackRequestDismiss,
    FeedbackResponse, SessionCommand,
};

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/btw" => {
            tracing::info!("handling /btw side question");
            btw::handle_btw(agent, args).await
        }
        "x.ai/feedback" | "x.ai/feedback/dismiss" => {
            tracing::info!("handling user feedback");
            handle_feedback(agent, args).await
        }
        m if m.starts_with("x.ai/review") => {
            tracing::info!("handling review comment");
            review::handle_review(agent, args).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

/// Shown for every `x.ai/feedback*` request while the feature flag is off
pub const FEEDBACK_DISABLED_MESSAGE: &str = "Feedback is disabled. To enable, set \
GROK_FEEDBACK_ENABLED=true or [features] feedback = true in config.toml.";

fn apply_feedback_context(submission: &mut FeedbackSubmission, ctx: Option<FeedbackContext>) {
    let Some(ctx) = ctx else {
        submission.reasoning_effort = None;
        return;
    };
    submission.tool_outcomes = ctx.tool_outcomes;
    submission.session_cwd = Some(ctx.session_cwd);
    submission.compaction_count = Some(ctx.compaction_count);
    submission.context_window_usage = Some(ctx.context_window_usage);
    submission.context_tokens_used = Some(ctx.context_tokens_used);
    submission.context_window_tokens = Some(ctx.context_window_tokens);
    submission.reasoning_effort = ctx.reasoning_effort.map(|effort| effort.to_string());
    if let Some(rated_id) = ctx.model_id {
        if submission.resolved_model_id.as_ref() != Some(&rated_id)
            && submission.model_id.as_ref() != Some(&rated_id)
        {
            submission.model_id = Some(rated_id);
            submission.resolved_model_id = None;
        }
        submission.model_fingerprint = ctx.model_fingerprint;
    }
}

async fn handle_feedback(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if !agent.cfg.borrow().is_feedback_enabled() {
        return Err(acp::Error::internal_error().data(FEEDBACK_DISABLED_MESSAGE));
    }

    match args.method.as_ref() {
        "x.ai/feedback" => {
            let mut feedback_input = parse_legacy_feedback(args)?;
            let params: serde_json::Value = serde_json::from_str(args.params.get())
                .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
            feedback_input.request_trace_upload_token = params
                .get("request_trace_upload_token")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            validate_images(&feedback_input.images)?;

            let session_id = acp::SessionId::new(feedback_input.session_id.clone());
            let session_handle = agent.resident_handle(&session_id);

            let (model_id, model_metadata) = if let Some(ref session) = session_handle {
                let (tx1, rx1) = tokio::sync::oneshot::channel();
                let _ = session
                    .cmd_tx
                    .send(SessionCommand::GetCurrentModel { responds_to: tx1 });
                let model_id = rx1.await.ok().map(|c| c.id);
                let model_metadata = session.get_model_metadata().await;
                (model_id, model_metadata)
            } else {
                let sampling_config = agent.sampling_config.borrow().clone();
                (Some(sampling_config.model.clone()), Default::default())
            };

            let turn_number = feedback_input.turn_number;

            let mut submission = feedback_input.take_submission(
                model_id,
                model_metadata.resolved_model_id,
                model_metadata.model_fingerprint,
                turn_number,
            );
            let turn_number = submission.turn_number;

            // Enrich with session context for Slack notifications (best-effort).
            let ctx = if let Some(ref session_handle) = session_handle {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = session_handle
                    .cmd_tx
                    .send(SessionCommand::GetFeedbackContext {
                        turn_number,
                        responds_to: tx,
                    });
                rx.await.ok()
            } else {
                None
            };
            apply_feedback_context(&mut submission, ctx);

            // Track rating in session signals
            if let (Some(session_handle), Some(rating_value)) =
                (&session_handle, feedback_input.rating_value)
            {
                use prod_mc_cli_chat_proxy_types::feedback_types::RatingType;
                let (is_positive, is_negative) = match feedback_input.rating_type {
                    // Thumbs: -1 is down, 0 is neutral, 1 is up
                    Some(RatingType::Thumbs) | None => (rating_value > 0, rating_value < 0),
                    // Stars (1-5): >= 4 positive, <= 2 negative, 3 neutral
                    Some(RatingType::Stars) => (rating_value >= 4, rating_value <= 2),
                    // NPS (0-10): 9-10 promoter, 0-6 detractor, 7-8 passive
                    Some(RatingType::Nps) => (rating_value >= 9, rating_value <= 6),
                };
                if is_positive {
                    session_handle.signals_handle.record_positive_rating();
                } else if is_negative {
                    session_handle.signals_handle.record_negative_rating();
                }
            }

            // Log feedback type for debugging
            if feedback_input.is_solicited() {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    request_id = ?feedback_input.request_id(),
                    turn_number = ?turn_number,
                    "Solicited feedback received (response to feedback request)"
                );
            } else {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    turn_number = ?turn_number,
                    "Spontaneous user feedback received"
                );
            }

            let telemetry_enabled = agent.product_analytics_enabled();
            let client = agent.feedback_client();
            if client.is_none() {
                tracing::warn!(
                    "no feedback client available (missing proxy credentials); feedback saved locally only"
                );
            }
            // Read the live feedback.user config; the session-actor path uses its spawn-time snapshot
            // Both dedupe through the same process-wide identity cache, so a stable config resolves identically either way
            // Clone out so the RefCell borrow doesn't span an await.
            let user_cfg = agent.cfg.borrow().feedback.user.clone();
            let author_identity =
                crate::util::user_identity::cached_identity(user_cfg.as_ref()).await;
            let outcome = crate::session::feedback_manager::submit_feedback_workflow(
                &mut submission,
                client.as_ref(),
                session_handle.as_ref().map(|h| &h.persistence_tx),
                crate::session::feedback_manager::SubmitFeedbackOptions {
                    solicited: feedback_input.is_solicited(),
                    telemetry_enabled,
                    author_identity,
                },
            )
            .await;

            let response_outcome = match &outcome {
                crate::session::feedback_manager::SubmitOutcome::Submitted => {
                    tracing::info!("feedback submitted to proxy successfully");
                    Some(crate::session::FeedbackOutcome::Submitted)
                }
                crate::session::feedback_manager::SubmitOutcome::LocalOnly => {
                    tracing::warn!("feedback saved locally only (no proxy client)");
                    Some(crate::session::FeedbackOutcome::LocalOnly)
                }
                crate::session::feedback_manager::SubmitOutcome::Failed(error) => {
                    tracing::error!(%error, "feedback submission to proxy failed");
                    return Err(acp::Error::internal_error()
                        .data(format!("Feedback submission failed: {error}")));
                }
            };
            let value = serde_json::to_value(FeedbackResponse {
                success: true,
                outcome: response_outcome,
                // The one-shot trace-upload grant was removed with the trace upload handler.
                trace_upload_token: None,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        "x.ai/feedback/dismiss" => dismiss_request(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

fn parse_legacy_feedback(args: &acp::ExtRequest) -> Result<ClientFeedbackInput, acp::Error> {
    match serde_json::from_str::<ClientFeedbackInput>(args.params.get()) {
        Ok(input) => Ok(input),
        Err(_) => {
            let simple: crate::session::FeedbackRequest = parse_params(args)?;
            Ok(ClientFeedbackInput {
                session_id: simple.session_id,
                client_type: prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
                rating_type: None,
                rating_value: None,
                feedback_text: Some(simple.feedback_text),
                images: vec![],
                feedback_categories: vec![],
                context_type: None,
                turn_number: None,
                request_id: None,
                client_version: None,
                metadata: None,
                terminal_info: None,
                request_trace_upload_token: false,
            })
        }
    }
}

fn validate_images(images: &[FeedbackImage]) -> Result<(), acp::Error> {
    validate_feedback_images(images)
        .map_err(|error| acp::Error::invalid_params().data(format!("feedback images: {error}")))
}

/// Tells the proxy that the user dismissed a solicited feedback request.
async fn dismiss_request(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let input: FeedbackRequestDismiss = parse_params(args)?;
    tracing::info!(
        session_id = %input.session_id,
        request_id = %input.request_id,
        "Feedback request dismissed by user"
    );

    // `submit_feedback_workflow` checks the same flag before its "responded" event
    if agent.product_analytics_enabled() {
        xai_grok_telemetry::event_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "dismissed",
            appearance_id = %input.request_id,
            has_feedback_text = false,
            is_solicited = true,
        );
    }

    persist_dismissal(agent, &input);

    let client = agent
        .feedback_client()
        .ok_or_else(|| acp::Error::internal_error().data("No credentials for feedback"))?;
    let feedback_url = agent.cfg.borrow().endpoints.resolve_feedback_base_url();
    match client.dismiss_request(&input.request_id).await {
        Ok(response) => {
            tracing::info!(
                request_id = %response.request_id,
                status = %response.status,
                %feedback_url,
                "Feedback request dismissed"
            );
            super::to_raw_response(&response)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                request_id = %input.request_id,
                %feedback_url,
                "Failed to dismiss feedback request"
            );
            Err(acp::Error::internal_error()
                .data(format!("Failed to dismiss feedback request: {error}")))
        }
    }
}

/// The persistence actor flushes this entry before the storage `CopyFile`.
fn persist_dismissal(agent: &MvpAgent, input: &FeedbackRequestDismiss) {
    let session_id = acp::SessionId::new(input.session_id.clone());
    if let Some(session_handle) = agent.resident_handle(&session_id) {
        session_handle.persist_feedback(LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: input.session_id.clone(),
            turn_number: None,
            solicited: true,
            request_id: Some(input.request_id.clone()),
            dismissed: true,
            submission: None,
        }));
    }
}
