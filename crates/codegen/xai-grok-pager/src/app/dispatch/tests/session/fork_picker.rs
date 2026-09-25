//! `/fork` fork-point picker tests: row construction, the picker's own input, and how a picked row
//! reaches the fork effect.

use super::*;

/// A fork-eligible app (git repo, live session) whose transcript shows `prompts` as user prompts.
/// Each prompt carries an explicit `prompt_index`, mirroring what a replay writes.
fn fork_test_app_with_prompts(prompts: &[&str]) -> AppView {
    let mut app = fork_test_app();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    for (index, text) in prompts.iter().enumerate() {
        let mut block = RenderBlock::user_prompt(*text);
        if let RenderBlock::UserPrompt(prompt) = &mut block {
            prompt.prompt_index = Some(index);
        }
        agent.scrollback.push_block(block);
        agent
            .scrollback
            .push_block(RenderBlock::agent_message(format!("answer {index}")));
    }
    app
}

fn rows(app: &AppView) -> Vec<crate::views::fork_picker::ForkPointRow> {
    app.agents[&AgentId(0)]
        .fork_picker_state
        .as_ref()
        .expect("picker open")
        .rows
        .clone()
}

#[test]
fn open_picker_lists_every_earlier_prompt_plus_the_current_state_row() {
    let mut app = fork_test_app_with_prompts(&["first", "second", "third"]);
    let effects = dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);

    assert!(effects.is_empty(), "the picker only resolves on selection");
    assert!(
        app.agents.values().all(|a| a.question_view.is_none()),
        "the worktree question waits for the fork point"
    );
    let rows = rows(&app);
    // Prompt 0 is not forkable ("before the first prompt" is an empty session, i.e. `/new`).
    assert_eq!(
        rows.len(),
        3,
        "prompts 1 and 2 plus the current-state row: {rows:?}"
    );
    assert!(matches!(
        &rows[0],
        crate::views::fork_picker::ForkPointRow::Before { prompt_index: 1, text, .. }
            if text == "second"
    ));
    assert!(matches!(
        &rows[1],
        crate::views::fork_picker::ForkPointRow::Before { prompt_index: 2, text, .. }
            if text == "third"
    ));
    assert!(matches!(
        rows[2],
        crate::views::fork_picker::ForkPointRow::Current
    ));

    // Default cursor is the current-state row, so Enter alone still forks the whole conversation.
    let state = app.agents[&AgentId(0)].fork_picker_state.as_ref().unwrap();
    assert_eq!(state.selected, rows.len() - 1);
}

#[test]
fn single_prompt_session_skips_the_picker_entirely() {
    // Nothing to pick: the picker would be a one-row modal for the behaviour `/fork` always had.
    let mut app = fork_test_app_with_prompts(&["only"]);
    let effects = dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);
    assert!(app.agents[&AgentId(0)].fork_picker_state.is_none());
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ForkSession {
                target_prompt_index: None,
                ..
            }]
        ),
        "got {effects:?}"
    );
}

#[test]
fn picking_an_earlier_prompt_cuts_the_fork_there_and_prefills_the_child() {
    let mut app = fork_test_app_with_prompts(&["first", "second", "third"]);
    dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);

    // Row 0 is "second" (prompt index 1): the fork keeps prompt 0 only, so the wire value is 0.
    let effects = dispatch(Action::ForkPointSelect(0), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ForkSession {
                target_prompt_index: Some(0),
                ..
            }]
        ),
        "got {effects:?}"
    );
    assert!(app.agents[&AgentId(0)].fork_picker_state.is_none());
    let child = app.agents.get(&AgentId(1)).expect("fork placeholder");
    assert_eq!(
        child.prompt.text(),
        "second",
        "the dropped prompt returns to the composer so it can be rewritten"
    );
}

#[test]
fn picking_the_current_state_row_forks_the_whole_conversation() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);
    let last = rows(&app).len() - 1;

    let effects = dispatch(Action::ForkPointSelect(last), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ForkSession {
                target_prompt_index: None,
                ..
            }]
        ),
        "got {effects:?}"
    );
    let child = app.agents.get(&AgentId(1)).expect("fork placeholder");
    assert_eq!(child.prompt.text(), "", "nothing to rewrite");
}

#[test]
fn picker_selection_keeps_the_worktree_flag() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    dispatch(Action::Fork(fork_args(Some(true), None)), &mut app);
    assert_eq!(rows(&app).len(), 2, "prompt 1 + current state");

    let effects = dispatch(Action::ForkPointSelect(0), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::CreateWorktreeSession {
                target_prompt_index: Some(0),
                load_session_id: Some(parent),
                ..
            }] if parent == "test-session"
        ),
        "a worktree fork carries the fork point into the worktree resume: {effects:?}"
    );
}

#[test]
fn picker_dismiss_closes_it_without_forking() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);
    assert!(app.agents[&AgentId(0)].fork_picker_state.is_some());

    let effects = dispatch(Action::ForkPointDismiss, &mut app);

    assert!(effects.is_empty());
    assert!(app.agents[&AgentId(0)].fork_picker_state.is_none());
    assert_eq!(app.agents.len(), 1, "no fork happened");
}

#[test]
fn selecting_a_stale_row_after_dismiss_forks_nothing() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);
    dispatch(Action::ForkPointDismiss, &mut app);

    let effects = dispatch(Action::ForkPointSelect(0), &mut app);

    assert!(effects.is_empty(), "got {effects:?}");
    assert_eq!(app.agents.len(), 1);
}

#[test]
fn at_flag_skips_the_picker_and_cuts_before_the_one_based_position() {
    let mut app = fork_test_app_with_prompts(&["first", "second", "third"]);
    let effects = dispatch(Action::Fork(fork_args_at(Some(false), 2)), &mut app);

    assert!(app.agents[&AgentId(0)].fork_picker_state.is_none());
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ForkSession {
                target_prompt_index: Some(0),
                ..
            }]
        ),
        "`--at 2` forks before the second prompt, keeping only prompt 0: {effects:?}"
    );
    assert_eq!(app.agents[&AgentId(1)].prompt.text(), "second");
}

#[test]
fn at_two_marks_the_parent_with_the_second_prompt_position() {
    let mut app = fork_test_app_with_prompts(&["first", "second", "third"]);
    dispatch(Action::Fork(fork_args_at(Some(false), 2)), &mut app);

    let marker = last_system_text(&app, AgentId(0));
    assert_eq!(marker, "Forked before prompt #2", "got: {marker}");
}

#[test]
fn at_flag_matches_the_selected_picker_row() {
    let mut via_at = fork_test_app_with_prompts(&["first", "second", "third"]);
    let at_effects = dispatch(Action::Fork(fork_args_at(Some(false), 2)), &mut via_at);

    let mut via_picker = fork_test_app_with_prompts(&["first", "second", "third"]);
    dispatch(Action::Fork(fork_args(Some(false), None)), &mut via_picker);
    let picker_effects = dispatch(Action::ForkPointSelect(0), &mut via_picker);

    assert!(matches!(
        at_effects.as_slice(),
        [Effect::ForkSession {
            target_prompt_index: Some(0),
            ..
        }]
    ));
    assert_eq!(at_effects.len(), picker_effects.len());
    assert!(matches!(
        picker_effects.as_slice(),
        [Effect::ForkSession {
            target_prompt_index: Some(0),
            ..
        }]
    ));
    assert_eq!(via_at.agents[&AgentId(1)].prompt.text(), "second");
    assert_eq!(via_picker.agents[&AgentId(1)].prompt.text(), "second");
}

#[test]
fn at_flag_with_directive_prefers_the_directive_over_prompt_prefill() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    let args = crate::slash::commands::fork::ForkArgs {
        worktree_override: Some(false),
        directive: Some("try another design".into()),
        at_prompt: Some(2),
    };
    dispatch(Action::Fork(args), &mut app);
    let child = app.agents.get(&AgentId(1)).expect("fork child");
    assert_eq!(child.prompt.text(), "");
    assert_eq!(
        child.pending_first_prompt.as_deref(),
        Some("try another design")
    );
}

#[test]
fn at_flag_rejects_position_one_when_it_would_copy_an_empty_conversation() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    let effects = dispatch(Action::Fork(fork_args_at(Some(false), 1)), &mut app);

    assert!(effects.is_empty());
    assert_eq!(app.agents.len(), 1);
    assert!(app.agents[&AgentId(0)].toast.is_some());
}

#[test]
fn at_flag_rejects_every_position_when_only_prompt_zero_exists() {
    for position in [1, 7] {
        let mut app = fork_test_app_with_prompts(&["only"]);
        let effects = dispatch(Action::Fork(fork_args_at(Some(false), position)), &mut app);
        assert!(effects.is_empty(), "--at {position} must not fork");
        assert_eq!(app.agents.len(), 1);
        assert!(app.agents[&AgentId(0)].toast.is_some());
    }
}

#[test]
fn at_flag_beyond_the_newest_prompt_toasts_and_forks_nothing() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    let effects = dispatch(Action::Fork(fork_args_at(Some(false), 7)), &mut app);

    assert!(effects.is_empty());
    assert_eq!(app.agents.len(), 1);
    let toast = app.agents[&AgentId(0)]
        .toast
        .as_ref()
        .expect("toast should be set");
    assert!(toast.0.contains("prompt #7"), "got: {}", toast.0);
}

#[test]
fn at_flag_inside_a_compaction_gap_still_cuts_without_a_prefill() {
    // Only prompt 3 is rendered (earlier ones were compacted away), but the index is still within the
    // session's prompt range, so the cut is honoured without a text to pre-fill.
    let mut app = fork_test_app();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let mut block = RenderBlock::user_prompt("recent");
        if let RenderBlock::UserPrompt(prompt) = &mut block {
            prompt.prompt_index = Some(3);
        }
        agent.scrollback.push_block(block);
    }

    let effects = dispatch(Action::Fork(fork_args_at(Some(false), 2)), &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ForkSession {
                target_prompt_index: Some(0),
                ..
            }]
        ),
        "`--at 2` is inside the gap and keeps prompt 0: {effects:?}"
    );
    assert_eq!(app.agents[&AgentId(1)].prompt.text(), "");
}

#[test]
fn worktree_question_modal_carries_the_picked_cut() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    app.fork_worktree_mode = crate::app::app_view::WorktreeMode::Ask;
    dispatch(Action::Fork(fork_args(None, None)), &mut app);

    // Row 0 is "second" (prompt index 1): the worktree question must be asked for that same cut.
    let effects = dispatch(Action::ForkPointSelect(0), &mut app);

    assert!(effects.is_empty(), "the modal decides first: {effects:?}");
    let qv = app.agents[&AgentId(0)]
        .question_view
        .as_ref()
        .expect("modal must be open");
    match qv.local_kind.as_ref().expect("local_kind must be set") {
        crate::views::question_view::LocalQuestionKind::Fork { directive, cut } => {
            assert!(directive.is_none());
            assert_eq!(
                cut,
                &crate::slash::commands::fork::ForkCut {
                    target_prompt_index: Some(0),
                    prefill: Some("second".into()),
                }
            );
        }
        other => panic!("expected Fork, got {other:?}"),
    }
}

#[test]
fn picker_refuses_to_open_over_an_open_rewind() {
    let mut app = fork_test_app_with_prompts(&["first", "second"]);
    app.agents.get_mut(&AgentId(0)).unwrap().rewind_state =
        Some(crate::views::rewind::RewindState {
            phase: crate::views::rewind::RewindPhase::Loading,
            anchor_entry_idx: 0,
            stashed_draft: None,
            selected_prompt_index: None,
            fixed_mode: None,
            restore: crate::views::jump::JumpRestore::none(),
        });

    let effects = dispatch(Action::Fork(fork_args(Some(false), None)), &mut app);

    assert!(app.agents[&AgentId(0)].fork_picker_state.is_none());
    assert!(effects.is_empty(), "got {effects:?}");
}
