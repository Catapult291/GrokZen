//! Keyboard actions on cross-session rows: the pane must reach them even though
//! they live in `foreign_tasks` rather than this session's `bg_tasks`.

use super::test_fixtures::make_agent;
use super::{AgentPane, AgentView, AppRenderParams, BannerSlotParams};
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::agent::{BgTaskState, BgTaskStatus, ForeignTaskState};
use crate::app::app_view::InputOutcome;
use crate::scrollback::render::ScratchBuffer;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn task(task_id: &str, status: BgTaskStatus) -> BgTaskState {
    BgTaskState {
        task_id: task_id.into(),
        tool_call_id: format!("call-{task_id}"),
        command: "cargo build --release".into(),
        description: None,
        cwd: "/tmp".into(),
        output_file: "/tmp/out".into(),
        status,
        start_time: std::time::SystemTime::now(),
        end_time: None,
        exit_code: None,
        signal: None,
        stdout: String::new(),
        stdout_line_count: 0,
        truncated: false,
        pending_kill: false,
        kill_requested_at: None,
        scrollback_entry_id: None,
        is_monitor: false,
        restored_from_replay: false,
        detach: true,
    }
}

/// An agent with the tasks pane open and one cross-session row in it.
fn agent_with_foreign_row(status: BgTaskStatus) -> AgentView {
    let mut agent = make_agent();
    agent.foreign_tasks.insert(
        "fg-1".into(),
        ForeignTaskState {
            task: task("fg-1", status),
            owner_short: "01a0e7b8".into(),
        },
    );
    agent.tasks.overlay.visible = true;
    agent.tasks.overlay.focused = true;
    agent.active_pane = AgentPane::Tasks;
    agent
}

/// One real frame, so the list layout runs and the pane holds a selection.
fn draw(agent: &mut AgentView, registry: &ActionRegistry) {
    let area = Rect::new(0, 0, 80, 30);
    let bundle = crate::app::bundle::BundleState::default();
    let mut buf = Buffer::empty(area);
    let mut scratch = ScratchBuffer::new();
    agent.last_terminal_size = (80, 30);
    agent.draw(
        area,
        &mut buf,
        registry,
        &mut scratch,
        None,
        false,
        BannerSlotParams::none(),
        &bundle,
        false,
        false,
        &mut Vec::new(),
        AppRenderParams::default(),
    );
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

/// `x` on a running cross-session row must send the kill. Before the fix the
/// handler only looked in `bg_tasks`, so the press vanished without a trace.
#[test]
fn x_key_reaches_a_running_foreign_row() {
    let registry = ActionRegistry::defaults();
    let mut agent = agent_with_foreign_row(BgTaskStatus::Running);
    draw(&mut agent, &registry);

    // The layout auto-selects the first row, which is the group header.
    assert_eq!(agent.tasks.selected_task_id(), None, "expected the header");
    assert!(matches!(
        agent.handle_input(&key(KeyCode::Down), &registry),
        InputOutcome::Changed
    ));
    assert_eq!(
        agent.tasks.selected_task_id(),
        Some("fg-1"),
        "the cross-session row must be the selected row"
    );
    let outcome = agent.handle_input(&key(KeyCode::Char('x')), &registry);
    assert!(
        matches!(
            &outcome,
            InputOutcome::Action(Action::KillBgTask(id)) if id == "fg-1"
        ),
        "x on a cross-session row must reach it, got {outcome:?}"
    );
}

/// A finished cross-session row answers the same key by clearing its record;
/// the pane only ever emits one action and the dispatch picks the meaning.
#[test]
fn x_key_reaches_a_finished_foreign_row() {
    let registry = ActionRegistry::defaults();
    let mut agent = agent_with_foreign_row(BgTaskStatus::Done);
    draw(&mut agent, &registry);

    // Finished rows are hidden until `h` reveals them, same as this session's.
    assert!(matches!(
        agent.handle_input(&key(KeyCode::Char('h')), &registry),
        InputOutcome::Changed
    ));
    draw(&mut agent, &registry);

    assert_eq!(agent.tasks.selected_task_id(), None, "expected the header");
    assert!(matches!(
        agent.handle_input(&key(KeyCode::Down), &registry),
        InputOutcome::Changed
    ));
    assert_eq!(
        agent.tasks.selected_task_id(),
        Some("fg-1"),
        "the finished cross-session row must be selectable"
    );
    let outcome = agent.handle_input(&key(KeyCode::Char('x')), &registry);
    assert!(
        matches!(
            &outcome,
            InputOutcome::Action(Action::KillBgTask(id)) if id == "fg-1"
        ),
        "x on a finished cross-session row must clear it, got {outcome:?}"
    );
}
