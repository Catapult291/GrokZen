//! Conversation rewind dispatchers and prompt-entry lookup helpers.

use super::ctx::NO_SESSION_NOTICE;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::state::ScrollbackState;
use crate::views::jump::JumpRestore;
use crate::views::prompt_widget::{PromptWidget, StashedPrompt};
use crate::views::rewind::{RewindMode, RewindPhase, RewindState};

/// User prompt that participates in the shell's prompt numbering.
/// Interjections render as user prompts but the shell never numbers them, so counting them would skew the positional prompt-to-entry mapping.
///
/// Known approximation: an interjection the shell converted into its own `interject-fallback-` turn IS shell-numbered.
/// Its live block (rendered from the interjection broadcast) is flagged `is_interjection` and carries no index.
/// The positional fallback thus under-counts around it until a resume replays it as an indexed prompt.
/// The primary path (explicit `prompt_index` matches) is unaffected.
fn is_indexed_user_prompt(block: &RenderBlock) -> bool {
    matches!(block, RenderBlock::UserPrompt(b) if !b.is_interjection)
}

fn stash_prompt(prompt: &mut PromptWidget) -> Option<StashedPrompt> {
    if prompt.text().is_empty() {
        None
    } else {
        Some(prompt.stash())
    }
}

/// Viewport snapshot a rewind flow restores when it is dismissed (`Esc`), taken before its preview
/// scrolling. The same rule `/jump` and `/fork` follow: cancelling leaves the transcript untouched.
fn capture_rewind_restore(agent: &crate::app::agent_view::AgentView) -> JumpRestore {
    JumpRestore {
        bookmark: agent.scrollback.capture_scroll_bookmark(),
        selected: agent.scrollback.selected(),
        follow_mode: agent.scrollback.is_follow_mode(),
    }
}

pub(in crate::app) fn shell_prompt_index_at(
    scrollback: &ScrollbackState,
    entry_idx: usize,
) -> Option<usize> {
    for idx in (0..=entry_idx).rev() {
        if let Some(e) = scrollback.get(idx)
            && let RenderBlock::UserPrompt(ref block) = e.block
        {
            // A mid-turn interjection belongs to the enclosing turn; keep walking back to that turn's starting prompt
            if block.is_interjection {
                continue;
            }
            if let Some(pi) = block.prompt_index {
                return Some(pi);
            }
            let count = (0..=idx)
                .filter(|&i| {
                    scrollback
                        .get(i)
                        .is_some_and(|e2| is_indexed_user_prompt(&e2.block))
                })
                .count();
            return if count > 0 { Some(count - 1) } else { None };
        }
    }
    None
}

pub(in crate::app) fn find_user_prompt_entry_for_shell_index(
    scrollback: &ScrollbackState,
    target_prompt_index: usize,
) -> Option<usize> {
    for idx in (0..scrollback.len()).rev() {
        if let Some(entry) = scrollback.get(idx)
            && let RenderBlock::UserPrompt(ref block) = entry.block
            && block.prompt_index == Some(target_prompt_index)
        {
            return Some(idx);
        }
    }
    let mut count = 0usize;
    for idx in 0..scrollback.len() {
        if let Some(e) = scrollback.get(idx)
            && is_indexed_user_prompt(&e.block)
        {
            if count == target_prompt_index {
                return Some(idx);
            }
            count += 1;
        }
    }
    None
}

/// Start a rewind flow and fetch its points.
/// `from_cursor` pre-targets the turn under the scrollback cursor (the Esc-Esc path); otherwise the flow always opens the turn picker.
/// `fixed_mode` pins the rewind's mode and skips the mode dialog: the `/undo` path, which never touches files.
fn open_rewind(
    app: &mut AppView,
    from_cursor: bool,
    fixed_mode: Option<RewindMode>,
) -> Vec<Effect> {
    let locale = app.locale.clone();
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        app.show_toast(locale.named_static_text("session.no_active", "No active session"));
        return vec![];
    };

    // Rewind takes input priority over the `/jump` picker; close a lingering one first so it can't reappear (stale) after rewind finishes
    agent.dismiss_jump_picker();

    // Captured before any preview scrolling, so dismissing the flow can put the transcript back.
    let restore = capture_rewind_restore(agent);

    let selected_idx = if from_cursor {
        agent.scrollback.selected()
    } else {
        None
    };
    let selected_shell_idx =
        selected_idx.and_then(|idx| shell_prompt_index_at(&agent.scrollback, idx));

    if agent.session.state.is_busy() {
        let anchor = agent.scrollback.len().saturating_sub(1);
        let draft = stash_prompt(&mut agent.prompt);
        agent.rewind_state = Some(RewindState::new_cancel_offer(
            anchor,
            draft,
            selected_shell_idx,
            fixed_mode,
            restore,
        ));
        return vec![];
    }

    let draft = stash_prompt(&mut agent.prompt);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: selected_idx.unwrap_or(0),
        stashed_draft: draft,
        selected_prompt_index: selected_shell_idx,
        fixed_mode,
        restore,
    });

    vec![Effect::FetchRewindPoints {
        agent_id: id,
        session_id,
    }]
}

pub(super) fn dispatch_rewind(app: &mut AppView) -> Vec<Effect> {
    open_rewind(app, true, None)
}

pub(super) fn dispatch_rewind_show_picker(app: &mut AppView) -> Vec<Effect> {
    open_rewind(app, false, None)
}

/// `/undo`: rewind the conversation only.
/// The mode is pinned up front, so the flow offers no mode dialog and no confirm step.
pub(super) fn dispatch_undo(app: &mut AppView) -> Vec<Effect> {
    open_rewind(app, false, Some(RewindMode::ConversationOnly))
}

pub(super) fn dispatch_rewind_picker_select(app: &mut AppView, prompt_index: usize) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let confirm = app.current_ui.confirm_before_rewind_enabled();
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let point = agent.rewind_points.as_ref().and_then(
        |pts: &Vec<crate::views::rewind::RewindPointInfo>| {
            pts.iter().find(|p| p.prompt_index == prompt_index)
        },
    );
    let preview = point.and_then(|p| p.prompt_preview.clone());
    let num_file_snapshots = point.map_or(0, |p| p.num_file_snapshots);

    let anchor = find_user_prompt_entry_for_shell_index(&agent.scrollback, prompt_index);
    if let Some(entry_idx) = anchor {
        agent.scrollback.set_selected(Some(entry_idx));
    }

    let (draft, fixed_mode, restore) = match agent.rewind_state.take() {
        Some(state) => (state.stashed_draft, state.fixed_mode, state.restore),
        None => (None, None, JumpRestore::none()),
    };
    begin_rewind(
        agent,
        id,
        prompt_index,
        anchor.unwrap_or(0),
        draft,
        preview,
        num_file_snapshots,
        confirm,
        fixed_mode,
        restore,
    )
}

pub(super) fn dispatch_rewind_cancel_offer(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let selected = agent
        .rewind_state
        .as_ref()
        .and_then(|s| s.selected_prompt_index);
    let fixed_mode = agent.rewind_state.as_ref().and_then(|s| s.fixed_mode);
    let restore = agent
        .rewind_state
        .as_ref()
        .map(|s| s.restore)
        .unwrap_or_else(JumpRestore::none);
    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: selected,
        fixed_mode,
        restore,
    });
    let mut effects = vec![Effect::CancelTurn {
        session_id: session_id.clone(),
        cancel_subagents: true,
        trigger: None,
        // The rewind picker owns history via `handle_rewind`; this pre-cancel must not also pop the in-flight prompt
        rewind_prompt_id: None,
    }];
    effects.push(Effect::FetchRewindPoints {
        agent_id: id,
        session_id,
    });
    effects
}

pub(super) fn dispatch_rewind_confirm(
    app: &mut AppView,
    target: usize,
    mode: crate::views::rewind::RewindMode,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let state = agent.rewind_state.take();
    let restore = state.as_ref().map_or_else(JumpRestore::none, |s| s.restore);
    let draft = state.and_then(|s| s.stashed_draft);
    enter_executing(agent, id, target, anchor, draft, mode, restore)
}

/// "Yes, and don't ask again": quiet-persist confirm-before-rewind off, then execute.
/// No settings checkmark toast; success/toast comes from the rewind itself.
pub(super) fn dispatch_rewind_confirm_never_ask(
    app: &mut AppView,
    target: usize,
    mode: crate::views::rewind::RewindMode,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let prev = app.current_ui.confirm_before_rewind_enabled();
    if prev {
        super::settings::setters::set_confirm_before_rewind_inner(app, false);
        super::settings::ui::refresh_open_settings_modals(app);
        effects.push(Effect::PersistSetting {
            key: "confirm_before_rewind",
            value: crate::settings::SettingValue::Bool(false),
            rollback_value: crate::settings::SettingValue::Bool(true),
        });
    }
    effects.extend(dispatch_rewind_confirm(app, target, mode));
    effects
}

pub(super) fn dispatch_rewind_dismiss(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let state = agent.rewind_state.take();
    agent.rewind_points = None;
    if let Some(state) = state {
        // Put the transcript back where the flow opened it, so the preview scrolling (and the
        // cut-point dim) doesn't outlive the flow. `/jump` and `/fork` dismiss the same way.
        agent.restore_jump_viewport(state.restore);
        if let Some(draft) = state.stashed_draft {
            agent.prompt.restore(draft);
        }
    }
    vec![]
}

pub(super) fn dispatch_rewind_dismiss_error(app: &mut AppView) -> Vec<Effect> {
    dispatch_rewind_dismiss(app)
}

/// The single place the inline-edit resubmit gets set: called right before every `Effect::RewindExecute` emission in the rewind flow.
/// If the inline editor is open, the (trimmed) edited text is stashed for `dispatch_rewind_success` to resubmit after the rewind lands.
/// Dismiss / error / empty-points paths never set it, so they need no clearing; the editor stays open there.
fn stash_inline_resubmit_if_editing(agent: &mut crate::app::agent_view::AgentView) {
    if let Some(ref edit) = agent.inline_edit {
        agent.pending_inline_resubmit = Some(edit.textarea.text().trim().to_string());
    }
}

/// Enter `Executing` and emit `RewindExecute` (shared by the confirm rows and immediate
/// execute when confirm-before-rewind is off).
#[allow(clippy::too_many_arguments)]
fn enter_executing(
    agent: &mut crate::app::agent_view::AgentView,
    agent_id: AgentId,
    target: usize,
    anchor: usize,
    draft: Option<StashedPrompt>,
    mode: crate::views::rewind::RewindMode,
    restore: JumpRestore,
) -> Vec<Effect> {
    let Some(session_id) = agent.session.session_id.clone() else {
        if let Some(d) = draft {
            agent.prompt.restore(d);
        }
        agent.rewind_state = None;
        agent.rewind_points = None;
        // The flow ends here without an execute, so it owes the user the viewport it borrowed.
        agent.restore_jump_viewport(restore);
        return vec![];
    };
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Executing {
            target_prompt_index: target,
        },
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        fixed_mode: None,
        // Carried so a failed execute's `Error` phase can still put the viewport back on dismiss.
        restore,
    });
    stash_inline_resubmit_if_editing(agent);
    vec![Effect::RewindExecute {
        agent_id,
        session_id,
        target_prompt_index: target,
        mode,
    }]
}

/// How the flow picks its mode once the target turn is known.
/// `fixed_mode` is always used as-is (`/undo` pins conversation-only and asks nothing).
/// Otherwise `confirm` decides between the mode dialog and an immediate run.
/// With the dialog turned off there is nowhere to pick a mode, so the immediate path uses the
/// protocol default (`All`) — the mode the dialog pre-selects.
#[allow(clippy::too_many_arguments)]
fn begin_rewind(
    agent: &mut crate::app::agent_view::AgentView,
    agent_id: AgentId,
    target: usize,
    anchor: usize,
    draft: Option<StashedPrompt>,
    prompt_preview: Option<String>,
    num_file_snapshots: usize,
    confirm: bool,
    fixed_mode: Option<RewindMode>,
    restore: JumpRestore,
) -> Vec<Effect> {
    if let Some(mode) = fixed_mode {
        return enter_executing(agent, agent_id, target, anchor, draft, mode, restore);
    }
    if confirm {
        agent.rewind_state = Some(RewindState {
            phase: RewindPhase::Confirm {
                target_prompt_index: target,
                active_idx: 0,
                prompt_preview,
                num_file_snapshots,
            },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index: Some(target),
            fixed_mode: None,
            restore,
        });
        return vec![];
    }
    enter_executing(
        agent,
        agent_id,
        target,
        anchor,
        draft,
        crate::views::rewind::RewindMode::All,
        restore,
    )
}

pub(super) fn dispatch_inline_edit_submit(app: &mut AppView) -> Vec<Effect> {
    let locale = app.locale.clone();
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        app.show_toast(locale.named_static_text("session.no_active", "No active session"));
        return vec![];
    };
    let Some(edit) = agent.inline_edit.as_ref() else {
        return vec![];
    };

    // Unchanged/empty edits have nothing to submit: just close the editor.
    let text = edit.textarea.text().trim().to_string();
    if text.is_empty() || text == edit.original.trim() {
        agent.exit_inline_edit();
        return vec![];
    }

    let target = edit.prompt_index;
    let anchor = agent
        .scrollback
        .index_of_id(edit.entry_id)
        .or_else(|| agent.scrollback.selected())
        .unwrap_or(0);
    let draft = stash_prompt(&mut agent.prompt);
    // The editor re-centered the transcript on the edited turn; a dismissed flow puts it back there.
    let restore = capture_rewind_restore(agent);

    if agent.session.state.is_busy() {
        // Mid-turn submit: the same cancel offer `/rewind` raises appears over the still-open editor
        // Confirm cancels the turn and re-enters the flow; dismiss returns to the editor
        agent.rewind_state = Some(RewindState::new_cancel_offer(
            anchor,
            draft,
            Some(target),
            None,
            restore,
        ));
        return vec![];
    }

    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: Some(target),
        fixed_mode: None,
        restore,
    });

    vec![Effect::FetchRewindPoints {
        agent_id: id,
        session_id,
    }]
}

pub(super) fn dispatch_rewind_success(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    response: crate::views::rewind::RewindResponse,
) -> Vec<Effect> {
    let locale = app.locale.clone();
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };

    // Inline-edit resubmit text; taken unconditionally so a failed rewind drops it
    let inline_resubmit = agent.pending_inline_resubmit.take();

    if !response.success {
        let err = response.error.unwrap_or_else(|| "unknown error".into());
        let anchor = agent
            .rewind_state
            .as_ref()
            .map(|s| s.anchor_entry_idx)
            .unwrap_or(0);
        let state = agent.rewind_state.take();
        let restore = state.as_ref().map_or_else(JumpRestore::none, |s| s.restore);
        let draft = state.and_then(|s| s.stashed_draft);
        agent.rewind_state = Some(RewindState {
            phase: RewindPhase::Error { message: err },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index: None,
            fixed_mode: None,
            restore,
        });
        // The inline editor (if any) stays open; dismissing the error returns to editing
        return vec![];
    }

    // The rewind went through: the inline editor's job is done. Close it before the truncation below removes its entry.
    if inline_resubmit.is_some() {
        agent.inline_edit = None;
        agent.scrollback.set_inline_edit_height(None);
    }

    let target = response.target_prompt_index;
    // What the shell actually did; an absent/unknown value means the old conversation-only shape.
    let mode = crate::views::rewind::RewindMode::from_wire(response.mode.as_deref());
    let stashed_draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);

    // Files-only keeps every turn (and its rendered blocks), so there is nothing to truncate,
    // no summary to clear, and no rewound point to resubmit an inline edit from. The editor
    // stays open on that path; the report below is the whole visible effect.
    if !mode.rewinds_conversation() {
        if inline_resubmit.is_none() {
            let msg = locale.named_static_text(mode.reverted_id(), mode.reverted_default());
            if app.screen_mode.is_minimal() {
                agent
                    .scrollback
                    .push_block(RenderBlock::system(msg.to_string()));
            } else {
                agent.show_toast(msg);
            }
        }
        if let Some(draft) = stashed_draft {
            agent.prompt.restore(draft);
        }
        agent.set_active_pane(crate::app::agent_view::ActivePane::Prompt, false);
        agent.rewind_points = None;
        return vec![];
    }

    // The summary describes turns the rewind just removed (the shell clears its persisted copy on the same branch)
    // Bump gen so a late SessionMetaFromDisk hydrate cannot restore the pre-rewind summary.json value into the cleared field
    agent.set_last_turn_summary(None);
    let target_idx = find_user_prompt_entry_for_shell_index(&agent.scrollback, target);
    if let Some(anchor_idx) = target_idx {
        let removed = agent.scrollback.remove_from(anchor_idx);
        // Explicit drop BEFORE the purge: the rewound tail must be freed for the release below to return its pages
        // (Entries and their render caches are potentially most of a long transcript.)
        drop(removed);
        crate::memory_release::release_retained_memory("rewind-truncate");
    }

    // An inline resubmit skips the confirmation; the edited prompt re-appearing at the same spot is self-explanatory
    if inline_resubmit.is_none() {
        let msg = locale.named_static_text(mode.reverted_id(), mode.reverted_default());
        if app.screen_mode.is_minimal() {
            // Minimal has no toast area and can't erase committed lines, so the confirmation stays in scrollback there
            agent
                .scrollback
                .push_block(RenderBlock::system(msg.to_string()));
        } else {
            agent.show_toast(msg);
        }
    }

    if inline_resubmit.is_some() {
        // Restore the full draft before a non-consuming resubmit.
        if let Some(draft) = stashed_draft {
            agent.prompt.restore(draft);
        }
    } else if let Some(ref prompt_text) = response.prompt_text {
        agent.prompt.set_text(prompt_text);
    } else if let Some(draft) = stashed_draft {
        agent.prompt.restore(draft);
    }

    agent.set_active_pane(crate::app::agent_view::ActivePane::Prompt, false);

    agent.rewind_points = None;
    agent.scrollback.goto_bottom();

    if let Some(text) = inline_resubmit {
        if app.active_view == ActiveView::Agent(agent_id) {
            // Resubmit from the rewound point; `consume_input=false` keeps the composer draft, `literal=true` sends slash-lookalike text as a prompt
            // (The transcript is already truncated; running it as a command would swallow the resubmit.)
            return super::prompt::dispatch_send_prompt_inner(
                app, text, /* consume_input */ false, /* literal */ true,
                /* is_follow_up */ false,
            );
        }
        // View switched mid-rewind: fall back to prefilling that composer, appending so an existing draft isn't clobbered
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            if agent.prompt.text().trim().is_empty() {
                agent.prompt.set_text(&text);
            } else {
                agent.prompt.append_text(&format!("\n{text}"));
            }
        }
    }

    vec![]
}

// TaskResult handlers.

pub(super) fn handle_rewind_points_loaded(
    app: &mut AppView,
    agent_id: AgentId,
    points: Vec<crate::views::rewind::RewindPointInfo>,
) -> Vec<Effect> {
    let locale = app.locale.clone();
    let confirm = app.current_ui.confirm_before_rewind_enabled();
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    agent.rewind_points = Some(points.clone());

    let desired_target = agent
        .rewind_state
        .as_ref()
        .and_then(|s| s.selected_prompt_index);
    let (stashed, fixed_mode, restore) = match agent.rewind_state.take() {
        Some(state) => (state.stashed_draft, state.fixed_mode, state.restore),
        None => (None, None, JumpRestore::none()),
    };

    if points.is_empty() {
        if let Some(stashed) = stashed {
            agent.prompt.restore(stashed);
        }
        app.show_toast(
            locale.named_static_text("rewind.no_undoable_prompts", "No undoable prompts"),
        );
        return vec![];
    }

    if let Some(dt) = desired_target {
        let resolved = points
            .iter()
            .find(|p| p.prompt_index == dt)
            .or_else(|| points.iter().max_by_key(|p| p.prompt_index))
            .cloned();

        if let Some(point) = resolved {
            let target = point.prompt_index;
            let preview = point.prompt_preview.clone();
            let num_file_snapshots = point.num_file_snapshots;
            let anchor = find_user_prompt_entry_for_shell_index(&agent.scrollback, target);
            let draft = stashed.or_else(|| stash_prompt(&mut agent.prompt));
            if let Some(entry_idx) = anchor {
                agent.scrollback.set_selected(Some(entry_idx));
            }
            return begin_rewind(
                agent,
                agent_id,
                target,
                anchor.unwrap_or(0),
                draft,
                preview,
                num_file_snapshots,
                confirm,
                fixed_mode,
                restore,
            );
        }
    }

    // Oldest first, like the `/fork` picker's rows, so the picker reads in conversation order.
    let mut sorted = points;
    sorted.sort_by_key(|p| p.prompt_index);
    let draft = stashed.or_else(|| stash_prompt(&mut agent.prompt));
    // The cursor starts on the newest turn — the least destructive cut, and the row the flow landed
    // on before the list was reordered — so its preview anchors that turn at the transcript top.
    let selected = sorted.len() - 1;
    let initial_anchor = sorted
        .last()
        .map(|p| {
            find_user_prompt_entry_for_shell_index(&agent.scrollback, p.prompt_index).unwrap_or(0)
        })
        .unwrap_or(0);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Picker {
            points: sorted,
            selected,
        },
        anchor_entry_idx: initial_anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        fixed_mode,
        restore,
    });
    agent.scrollback.scroll_to_entry_top(initial_anchor);
    vec![]
}

pub(super) fn handle_rewind_execute_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    // A pending inline resubmit dies with its rewind; the editor itself stays open so dismissing the error returns to editing
    agent.pending_inline_resubmit = None;
    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let state = agent.rewind_state.take();
    let restore = state.as_ref().map_or_else(JumpRestore::none, |s| s.restore);
    let draft = state.and_then(|s| s.stashed_draft);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Error { message: error },
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        fixed_mode: None,
        restore,
    });
    vec![]
}
