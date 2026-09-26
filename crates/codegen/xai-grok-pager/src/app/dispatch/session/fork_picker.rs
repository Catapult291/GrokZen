//! `/fork` fork-point picker dispatchers.
//!
//! Rows come from the rendered transcript (one per turn), so opening the picker costs no request and
//! works while the session is still replaying. Picking a row hands the resolved
//! [`ForkCut`] to the worktree question in `dispatch::session::fork`, which owns the fork itself.

use super::fork::resolve_fork_after_cut;
use crate::app::actions::Effect;
use crate::app::app_view::{ActiveView, AppView};
use crate::app::dispatch::rewind::shell_prompt_index_at;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::state::ScrollbackState;
use crate::slash::commands::fork::ForkCut;
use crate::views::fork_picker::{ForkPickerState, ForkPointRow};
use crate::views::jump::JumpRestore;

/// Every forkable point the transcript still renders, oldest first.
///
/// Prompt `0` is excluded: forking before the first prompt would copy an empty conversation, which is
/// what `/new` is for, and the shell's `targetPromptIndex` has no value for "keep no prompt".
pub(in crate::app::dispatch) fn fork_point_rows(scrollback: &ScrollbackState) -> Vec<ForkPointRow> {
    let mut rows: Vec<ForkPointRow> = Vec::new();
    for entry in scrollback.timeline_entries() {
        let Some(idx) = scrollback.index_of_id(entry.prompt_entry_id) else {
            continue;
        };
        let Some(prompt_index) = shell_prompt_index_at(scrollback, idx) else {
            continue;
        };
        if prompt_index == 0 {
            continue;
        }
        // A turn's prompt block is the only place the full text (for the child's composer) lives.
        let text = match scrollback.get(idx).map(|e| &e.block) {
            Some(RenderBlock::UserPrompt(block)) => block.text.clone(),
            _ => String::new(),
        };
        if text.is_empty()
            || rows.iter().any(
                |row| matches!(row, ForkPointRow::Before { prompt_index: seen, .. } if *seen == prompt_index),
            )
        {
            continue;
        }
        rows.push(ForkPointRow::Before {
            prompt_index,
            entry_id: entry.prompt_entry_id,
            preview: entry.preview,
            text,
        });
    }
    rows
}

/// The cut for `/fork --at <position>`, or `None` when the session has no such prompt.
///
/// The position is the prompt's index in the shell's numbering (the same numbering `/rewind` reports).
/// A prompt the transcript no longer renders (dropped by compaction) still cuts, but carries no
/// pre-filled text.
pub(in crate::app::dispatch) fn resolve_cut_for_at_prompt(
    scrollback: &ScrollbackState,
    position: usize,
) -> Option<ForkCut> {
    let rows = fork_point_rows(scrollback);
    if let Some(ForkPointRow::Before {
        prompt_index, text, ..
    }) = rows.iter().find(
        |row| matches!(row, ForkPointRow::Before { prompt_index, .. } if *prompt_index == position),
    ) {
        return Some(ForkCut {
            target_prompt_index: prompt_index.checked_sub(1),
            prefill: (!text.is_empty()).then(|| text.clone()),
        });
    }
    // No rendered prompt at that index: accept it while it is still within the prompts we know about
    // (a gap left by compaction), reject positions past the newest one.
    let newest = rows.iter().rev().find_map(|row| match row {
        ForkPointRow::Before { prompt_index, .. } => Some(*prompt_index),
        ForkPointRow::Current => None,
    });
    match newest {
        Some(newest) if position > newest => None,
        _ => Some(ForkCut {
            target_prompt_index: position.checked_sub(1),
            prefill: None,
        }),
    }
}

/// Open the fork-point picker on the active agent.
///
/// A session with no earlier prompt has nothing to pick, so the picker is skipped and the fork
/// proceeds exactly as it did before the picker existed.
pub(in crate::app::dispatch) fn open_fork_picker(
    app: &mut AppView,
    worktree_override: Option<bool>,
    directive: Option<String>,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    // `/fork` takes the input slot from a lingering `/jump` picker, the same way `/rewind` does.
    agent.dismiss_jump_picker();
    if agent.fork_picker_slot_taken() {
        return vec![];
    }

    let mut rows = fork_point_rows(&agent.scrollback);
    if rows.is_empty() {
        return resolve_fork_after_cut(app, worktree_override, directive, ForkCut::default());
    }
    // Default cursor keeps the whole conversation, i.e. `/fork` as it behaved before the picker.
    rows.push(ForkPointRow::Current);
    let selected = rows.len() - 1;
    let restore = JumpRestore {
        bookmark: agent.scrollback.capture_scroll_bookmark(),
        selected: agent.scrollback.selected(),
        follow_mode: agent.scrollback.is_follow_mode(),
    };
    agent.fork_picker_state = Some(ForkPickerState {
        rows,
        selected,
        restore,
        worktree_override,
        directive,
    });
    agent.sync_fork_picker_preview();
    vec![]
}

/// Enter on a row: fork with that row's cut.
pub(in crate::app::dispatch) fn dispatch_fork_picker_select(
    app: &mut AppView,
    row_idx: usize,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(state) = app
        .agents
        .get_mut(&id)
        .and_then(|agent| agent.fork_picker_state.take())
    else {
        return vec![];
    };
    let Some(row) = state.rows.get(row_idx) else {
        return vec![];
    };
    let (target_prompt_index, prefill) = row.cut();
    resolve_fork_after_cut(
        app,
        state.worktree_override,
        state.directive,
        ForkCut {
            target_prompt_index,
            prefill,
        },
    )
}

/// Esc: close the picker and put the viewport back where it was.
pub(in crate::app::dispatch) fn dispatch_fork_picker_dismiss(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    agent.dismiss_fork_picker();
    vec![]
}
