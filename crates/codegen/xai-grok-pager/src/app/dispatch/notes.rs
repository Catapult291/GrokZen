//! Remember-note, btw, and recap dispatchers.

use super::ctx::{NO_SESSION_NOTICE, with_active_agent};
use agent_client_protocol as acp;

use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::{AgentView, PromptInputMode};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{SessionEvent, ToolCallBlock};

fn note_static(
    locale: &crate::locale::LocaleContext,
    id: &str,
    english: &'static str,
) -> &'static str {
    locale.named_static_text(id, english)
}

fn note_text(locale: &crate::locale::LocaleContext, id: &str, english: &str) -> String {
    locale.named_text(id, english).into_owned()
}

use std::sync::atomic::{AtomicU64, Ordering};
use xai_grok_feedback::{
    FeedbackDraftStore, FeedbackSource, FeedbackTaxonomy, derive_title, structured_feedback,
};

/// Monotonic counter for correlating async rewrite responses with the modal that requested them.
/// It prevents stale results from populating a different note's review modal when the user closes and re-opens quickly.
static REWRITE_NONCE: AtomicU64 = AtomicU64::new(0);

fn next_rewrite_nonce() -> u64 {
    REWRITE_NONCE.fetch_add(1, Ordering::Relaxed)
}


/// Enter remember mode: visual change to prompt bar (remember accent, `#` prefix).
/// No side effects: the user types a memory note and presses Enter to send.
pub(super) fn dispatch_enter_remember_mode(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        agent.prompt_input_mode = PromptInputMode::Remember;
        agent.prompt.set_text("");
    });
    vec![]
}
/// The `#` composer path. Nothing else records the note, so this records it.
pub(super) fn dispatch_send_remember_note(app: &mut AppView, text: String) -> Vec<Effect> {
    send_remember_note(app, text, true)
}

/// The `/remember <text>` path. `dispatch_send_prompt_inner` already recorded the typed command.
pub(super) fn dispatch_send_remember_note_from_command(
    app: &mut AppView,
    text: String,
) -> Vec<Effect> {
    send_remember_note(app, text, false)
}

/// Send a raw remember note for LLM-powered rewriting via `x.ai/memory/rewrite`.
/// Clears remember mode and prompts the LLM to reformat the note with session context.
/// Falls back to direct `SaveMemoryNote` when no session is available.
fn send_remember_note(app: &mut AppView, text: String, record_in_history: bool) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.prompt_input_mode = PromptInputMode::Normal;
    agent.prompt.set_text("");
    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        let message = note_static(
            agent.scrollback.locale(),
            "memory_note.error.empty",
            "Please provide a memory note.",
        )
        .to_string();
        agent.scrollback.push_block(RenderBlock::system(message));
        return vec![];
    }

    agent.note_draft_consumed();
    if record_in_history {
        // The note is stored without the `#`; recall decodes a prefix back into its mode, which would turn `# Context` into a note
        agent.record_prompt_in_history(&trimmed);
    }

    let cwd = agent.session.cwd.clone();

    let Some(session_id) = agent.session.session_id.clone() else {
        // No session: open the modal with raw content only (no LLM rewrite)
        agent.active_modal = Some(ActiveModal::RememberNoteReview {
            raw_content: trimmed.clone(),
            enhanced_content: None, // No session, so no LLM rewrite and Tab is disabled
            showing_enhanced: false,
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            cwd,
            agent_id: id,
            rewrite_nonce: Default::default(), // no rewrite in flight, nonce unused
        });
        return vec![];
    };

    // Open modal with raw content, LLM rewrite in flight.
    let nonce = next_rewrite_nonce();
    agent.active_modal = Some(ActiveModal::RememberNoteReview {
        raw_content: trimmed.clone(),
        enhanced_content: None,
        showing_enhanced: false,
        scroll: 0,
        window: crate::views::modal_window::ModalWindowState::new(),
        cached_lines: None,
        cwd: cwd.clone(),
        agent_id: id,
        rewrite_nonce: nonce,
    });

    let context_summary = extract_session_context(agent);

    vec![Effect::RewriteMemoryNote {
        agent_id: id,
        session_id,
        raw_text: trimmed,
        context_summary,
        nonce,
    }]
}

/// Save the currently displayed remember note from the review modal.
pub(super) fn dispatch_save_remember_note_from_modal(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let (content, cwd) = if let Some(ActiveModal::RememberNoteReview {
        ref raw_content,
        ref enhanced_content,
        showing_enhanced,
        ref cwd,
        ..
    }) = agent.active_modal
    {
        let text = if showing_enhanced {
            enhanced_content.as_deref().unwrap_or(raw_content)
        } else {
            raw_content
        };
        (text.trim().to_string(), cwd.clone())
    } else {
        return vec![];
    };
    let pinned_mode = agent
        .session
        .session_id
        .as_ref()
        .map(|_| agent.memory_mode.unwrap_or_default());

    agent.active_modal = None;
    let message = note_static(
        agent.scrollback.locale(),
        "memory_note.saving",
        "Saving memory note...",
    )
    .to_string();
    agent.scrollback.push_block(RenderBlock::system(message));

    vec![Effect::SaveMemoryNote {
        agent_id: id,
        text: content,
        cwd,
        pinned_mode,
    }]
}

/// Extract session context for the LLM memory rewrite request.
/// File paths from recent tool calls (Read, Edit, ListDir)
fn extract_session_context(agent: &AgentView) -> String {
    let mut user_prompts: Vec<String> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();

    // Walk scrollback entries in reverse to collect recent context.
    let len = agent.scrollback.len();
    for i in (0..len).rev() {
        let Some(entry) = agent.scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            RenderBlock::UserPrompt(prompt) => {
                if user_prompts.len() < 5 {
                    let text = if prompt.text.len() > 200 {
                        let end = prompt
                            .text
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        match prompt.text.get(..end) {
                            Some(prefix) => format!("{prefix}..."),
                            None => prompt.text.clone(),
                        }
                    } else {
                        prompt.text.clone()
                    };
                    user_prompts.push(text);
                }
            }
            RenderBlock::ToolCall(tc) => {
                if file_paths.len() < 20 {
                    match tc {
                        ToolCallBlock::Read(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::Edit(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::ListDir(b) => {
                            file_paths.push(b.path.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if user_prompts.len() >= 5 && file_paths.len() >= 20 {
            break;
        }
    }

    let mut parts: Vec<String> = Vec::new();

    // CWD
    parts.push(format!("CWD: {}", agent.session.cwd.display()));

    // Git branch
    if let Some(ref branch) = agent.current_branch {
        parts.push(format!("Branch: {branch}"));
    }

    // Recent prompts (chronological order)
    if !user_prompts.is_empty() {
        user_prompts.reverse();
        parts.push("Recent prompts:".to_string());
        for p in &user_prompts {
            parts.push(format!("- {p}"));
        }
    }

    // Recent file paths (deduplicated, preserving first-seen order)
    if !file_paths.is_empty() {
        let mut seen = std::collections::HashSet::new();
        file_paths.retain(|p| seen.insert(p.clone()));
        parts.push("Recent files:".to_string());
        for p in &file_paths {
            parts.push(format!("- {p}"));
        }
    }

    parts.join("\n")
}

/// Send a /btw side question.
/// Bypasses the prompt queue, so it works even while the agent is mid-turn.
/// Fires an ACP ext method and shows a loading overlay.
pub(super) fn dispatch_send_btw(
    app: &mut AppView,
    question: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    // Drop deletes staged and session files. A side question does not keep them.
    let images = BtwImages::new(images);
    // Slash submit strips image chips before the command runs. Put `[Image]`
    // back so the panel title shows the attachment.
    let question = question_with_image_chips(&question, images.as_slice());
    let no_session = note_static(
        app.locale.as_ref(),
        "session.no_active",
        "No active session",
    );
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let minimal = app.screen_mode.is_minimal();
    let (session_id, minimal_request_id, cwd) = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let Some(session_id) = agent.session.session_id.clone() else {
            if minimal {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(no_session));
            } else {
                agent.show_toast(no_session);
            }
            return vec![];
        };
        let cwd = std::path::PathBuf::from(&agent.session.cwd);

        // Composer clearing belongs to the submit funnel: `dispatch_send_prompt_inner` clears it when `consume_input` is set
        // Draft-preserving callers (the palette, an edited queue row) keep theirs
        let minimal_request_id = if minimal {
            Some(crate::minimal_api::start_minimal_btw(
                agent,
                question.clone(),
            ))
        } else {
            agent.clear_btw_owned_selection();
            agent.btw_state = Some(crate::views::btw_overlay::BtwOverlayState::Loading {
                question: question.clone(),
            });
            // Prompt keeps focus while the answer is in flight (panel focuses on Done).
            agent.btw_focused = false;
            None
        };
        (session_id, minimal_request_id, cwd)
    };

    vec![Effect::SendBtw {
        agent_id: id,
        session_id,
        question,
        images: images.into_inner(),
        cwd,
        minimal_request_id,
    }]
}

/// Owns `/btw` attachments until they are handed to the send effect.
/// Drop unlinks staged temps and session copies.
struct BtwImages(Vec<crate::prompt_images::PastedImage>);

impl BtwImages {
    fn new(images: Vec<crate::prompt_images::PastedImage>) -> Self {
        Self(images)
    }

    fn as_slice(&self) -> &[crate::prompt_images::PastedImage] {
        &self.0
    }

    fn into_inner(mut self) -> Vec<crate::prompt_images::PastedImage> {
        std::mem::take(&mut self.0)
    }
}

/// Prefix a single `[Image]` chip. The `/btw` title does not number attachments.
/// Text-only questions are returned unchanged so newlines survive.
fn question_with_image_chips(
    question: &str,
    images: &[crate::prompt_images::PastedImage],
) -> String {
    if images.is_empty() {
        return question.to_string();
    }
    let question = strip_numbered_image_chips(question.trim());
    if question.contains("[Image]") {
        return question;
    }
    if question.is_empty() {
        "[Image]".to_string()
    } else {
        format!("[Image] {question}")
    }
}

/// Remove `[Image #N]` tokens without collapsing newlines.
fn strip_numbered_image_chips(question: &str) -> String {
    let mut out = String::new();
    let mut rest = question;
    while let Some(start) = rest.find("[Image #") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "[Image #".len()..];
        let Some(end) = after.find(']') else {
            out.push_str(&rest[start..]);
            return out;
        };
        rest = &after[end + 1..];
        if out.ends_with(' ') && rest.starts_with(' ') {
            rest = rest.trim_start_matches(' ');
        }
    }
    out.push_str(rest);
    out
}

impl Drop for BtwImages {
    fn drop(&mut self) {
        crate::prompt_images::drain_and_cleanup(
            crate::prompt_images::SessionPathPolicy::Delete,
            &mut self.0,
        );
    }
}

/// Side-question payload cap. Matches the per-image send limit so one screenshot
/// under 50MB is never dropped; extra images past this total are omitted.
const BTW_IMAGE_AGGREGATE_CAP: usize = 50_000_000;

pub(crate) struct BtwImageEncode {
    pub blocks: Option<Vec<acp::ContentBlock>>,
    /// Every attachment that did not encode: the aggregate-cap drops plus the unreadable ones.
    pub omitted: usize,
    /// Attachments the aggregate cap dropped before any load was attempted.
    pub omitted_by_cap: usize,
    /// Display numbers of attachments whose bytes could not be loaded (a subset of `omitted`).
    pub skipped_display_numbers: Vec<usize>,
}

/// Encode composer images into `x.ai/btw` content blocks.
/// `blocks: None` keeps the text-only wire. `omitted` is how many attachments did not encode.
/// Caller unlinks the image files after this returns.
pub(crate) fn encode_btw_images(
    question: &str,
    images: &[crate::prompt_images::PastedImage],
    cwd: &std::path::Path,
) -> BtwImageEncode {
    if images.is_empty() {
        return BtwImageEncode {
            blocks: None,
            omitted: 0,
            omitted_by_cap: 0,
            skipped_display_numbers: Vec::new(),
        };
    }
    let kept = images_within_aggregate_cap(images);
    let omitted_by_cap = images.len() - kept.len();
    if kept.is_empty() {
        return BtwImageEncode {
            blocks: None,
            omitted: images.len(),
            omitted_by_cap,
            skipped_display_numbers: Vec::new(),
        };
    }
    let build = crate::prompt_images::build_content_blocks_with_workspace_report_ref(
        question.to_string(),
        &kept,
        Some(cwd),
    );
    let produced = build
        .blocks
        .iter()
        .filter(|block| matches!(block, acp::ContentBlock::Image(_)))
        .count();
    let omitted = omitted_by_cap + build.skipped_display_numbers.len();
    BtwImageEncode {
        blocks: (produced > 0).then_some(build.blocks),
        omitted,
        omitted_by_cap,
        skipped_display_numbers: build.skipped_display_numbers,
    }
}

fn images_within_aggregate_cap(
    images: &[crate::prompt_images::PastedImage],
) -> Vec<crate::prompt_images::PastedImage> {
    let mut kept = Vec::new();
    let mut total = 0usize;
    for image in images {
        let size = image.byte_len;
        if total.saturating_add(size) > BTW_IMAGE_AGGREGATE_CAP {
            tracing::warn!(
                bytes = size,
                total,
                cap = BTW_IMAGE_AGGREGATE_CAP,
                "skipping /btw image: aggregate payload cap"
            );
            continue;
        }
        total += size;
        kept.push(image.clone());
    }
    kept
}

/// Toast when a manual `/recap` produces no summary.
/// Empty sessions get a clear empty-state message; anything else (model failure, empty summary, etc.) keeps the generic failure toast.
pub(crate) fn recap_unavailable_toast(has_user_messages: bool) -> &'static str {
    if has_user_messages {
        "Couldn't generate recap"
    } else {
        "No messages yet"
    }
}

pub(crate) fn recap_unavailable_toast_with_locale(
    locale: &crate::locale::LocaleContext,
    has_user_messages: bool,
) -> &'static str {
    if has_user_messages {
        note_static(locale, "recap.unavailable", "Couldn't generate recap")
    } else {
        note_static(locale, "recap.no_messages", "No messages yet")
    }
}

/// Whether scrollback already has a user prompt. Scans entries (not
/// `turn_count`) so it stays correct during `begin_batch`/`end_batch` session
/// load, when `push` defers `rebuild_turns` and `turn_count` can stay 0 while
/// replayed prompts are already present.
pub(crate) fn scrollback_has_user_messages(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    scrollback
        .iter_entries()
        .any(|(_, entry)| entry.block.is_user_prompt())
}

/// Request a session recap.
/// Bypasses the prompt queue, so it works even while the agent is mid-turn.
/// The auto path is best-effort and silently no-ops without an active session.
pub(super) fn dispatch_send_recap(app: &mut AppView, auto: bool) -> Vec<Effect> {
    let locale = app.locale.clone();
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    // The shell is authoritative (remote settings, config, env)
    // Skip client requests entirely when the feature is off so we never hit `x.ai/recap`
    if !app.session_recap_available {
        if !auto {
            agent.show_toast(note_static(
                locale.as_ref(),
                "recap.disabled",
                "Session recap is not enabled",
            ));
        }
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        if !auto {
            agent.show_toast(note_static(
                locale.as_ref(),
                "session.no_active",
                "No active session",
            ));
        }
        return vec![];
    };

    if !auto {
        agent.prompt.set_text("");
        // Nothing to summarize yet: show a clear empty-state toast instead of a spinner that ends in "Couldn't generate recap"
        // Skip the short-circuit while session replay is still loading (prompts may not have arrived yet)
        // Prefer an entry scan over `turn_count()` so mid-batch resume (deferred `rebuild_turns`) still sees history
        if !agent.session.loading_replay && !scrollback_has_user_messages(&agent.scrollback) {
            agent.show_toast(recap_unavailable_toast_with_locale(locale.as_ref(), false));
            return vec![];
        }
        // Show an immediate loading block with the animated "running" sidebar so the user has feedback that a recap is being generated
        // The `SessionRecap` handler fills this entry in and stops the animation
        // Reuse an existing in-flight loading block instead of stacking spinners when `/recap` is pressed repeatedly
        let already_loading = agent.pending_recap_entry.is_some_and(|eid| {
            agent
                .scrollback
                .get_by_id(eid)
                .is_some_and(|entry| entry.is_running)
        });
        if !already_loading {
            let entry_id =
                agent
                    .scrollback
                    .push(crate::scrollback::entry::ScrollbackEntry::running(
                        RenderBlock::session_event(SessionEvent::Recap {
                            summary: String::new(),
                            auto: false,
                        }),
                    ));
            agent.pending_recap_entry = Some(entry_id);
        }
    } else {
        // Retry backoff only: do not consume the away period on dispatch
        // The shell often no-ops auto recap until at least 3 min since the last main turn
        // mark_recap_shown runs when any SessionRecap arrives (auto or manual `/recap`)
        app.notification_service
            .focus_tracker
            .note_auto_recap_attempt();
    }

    vec![Effect::SendRecap { session_id, auto }]
}

// TaskResult handlers.

pub(super) fn handle_memory_note_saved(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<(), String>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        match result {
            Ok(()) => {
                let message = note_text(
                    agent.scrollback.locale(),
                    "memory_note.saved_v2",
                    "Memory note saved",
                );
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(message));
            }
            Err(error) => {
                let message = note_text(
                    agent.scrollback.locale(),
                    "memory_note.error.save",
                    "Couldn't save memory note: {error}",
                )
                .replace("{error}", &error);
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(message));
            }
        }
    }
    vec![]
}

/// Localize only image notices generated by the client. The model request and
/// unknown diagnostics remain untouched; the notice channel does not carry replies.
fn localized_btw_image_notice(notice: &str, locale: &crate::locale::LocaleContext) -> String {
    const ALL: &str = "Attached images were not included (too large or could not be loaded).";
    const SUFFIX: &str = " attached image(s) were not included (over the 50MB side-question limit or could not be loaded).";
    if notice == ALL {
        return locale
            .named_text("btw.images.omitted_all", ALL)
            .into_owned();
    }
    if let Some(count) = notice.strip_suffix(SUFFIX)
        && !count.is_empty()
        && count.bytes().all(|b| b.is_ascii_digit())
    {
        return locale.named_text(
            "btw.images.omitted_some",
            "{count} attached image(s) were not included (over the 50MB side-question limit or could not be loaded).",
        ).replace("{count}", count);
    }
    notice.to_owned()
}

pub(super) fn handle_btw_response(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<String, String>,
    minimal_request_id: Option<uuid::Uuid>,
    image_notice: Option<String>,
    skipped_image_numbers: &[usize],
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        use crate::views::btw_overlay::BtwOverlayState;
        let image_notice = image_notice
            .as_deref()
            .map(|notice| localized_btw_image_notice(notice, agent.scrollback.locale()));
        if let Some(request_id) = minimal_request_id {
            // A dismissed or stale request ignores the answer. Don't toast either.
            if crate::minimal_api::finish_minimal_btw(agent, request_id, result) {
                app.pending_image_notices.extend(image_notice);
                app.pending_image_notices
                    .extend(agent.skipped_image_send_notice(skipped_image_numbers));
            }
            return vec![];
        }
        // Cap drops and read failures are disjoint sets; the flush shows them as one message.
        app.pending_image_notices.extend(image_notice);
        app.pending_image_notices
            .extend(agent.skipped_image_send_notice(skipped_image_numbers));
        let question = match &agent.btw_state {
            Some(BtwOverlayState::Loading { question }) => question.clone(),
            _ => String::new(),
        };
        match result {
            Ok(response) => {
                // Answer arrived: show it (until Esc) and focus the panel so Up/Down scroll it until the user returns to the prompt
                agent.btw_state = Some(BtwOverlayState::done(question, response));
                agent.btw_focused = true;
            }
            Err(error) => {
                // Error stays until Esc; nothing to scroll, keep prompt focus.
                agent.btw_state = Some(BtwOverlayState::Error { question, error });
                agent.btw_focused = false;
            }
        }
    }
    vec![]
}
