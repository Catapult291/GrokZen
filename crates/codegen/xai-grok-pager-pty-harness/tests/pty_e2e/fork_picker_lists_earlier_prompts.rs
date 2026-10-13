// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Two distinct prompts so the transcript proves which turn each assertion is about.
const FIRST_PROMPT: &str = "FIRSTPROMPTSENTINEL";
const SECOND_PROMPT: &str = "SECONDPROMPTSENTINEL";

/// The picker's "keep everything" row, which the cursor starts on.
const CURRENT_ROW: &str = "Keep the whole conversation (current state)";
const ROWS: [&str; 2] = [SECOND_PROMPT, CURRENT_ROW];

const PICKER_TITLE: &str = "Fork before which prompt?";
const CURSOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Text of the picker row carrying the cursor, identified by the cursor row's bold style.
/// The `→` gutter belongs to the current-state row's meaning, not to whichever row is selected, so it
/// cannot identify the cursor; the renderer does bold the selected row even when its background matches
/// the terminal theme in a PTY capture.
fn fork_picker_cursor_row(harness: &PtyHarness) -> Option<String> {
    for line in harness.screen_styled() {
        let text: String = line.runs.iter().map(|run| run.text.as_str()).collect();
        let Some(label) = ROWS.iter().find(|label| text.contains(**label)) else {
            continue;
        };
        if line
            .runs
            .iter()
            .any(|run| run.text.contains(*label) && run.bold)
        {
            return Some((*label).to_string());
        }
    }
    None
}

fn expect_cursor_row(harness: &mut PtyHarness, expected: &str, step: &str) {
    let outcome = harness.wait_until(
        &format!("{step}: cursor on {expected:?}"),
        CURSOR_TIMEOUT,
        |h| fork_picker_cursor_row(h).as_deref() == Some(expected),
    );
    outcome.unwrap_or_else(|e| {
        panic!(
            "{e}\nrows: {:#?}\nscreen:\n{}",
            harness.screen_styled(),
            harness.screen_contents()
        )
    });
}

/// `/fork` opens the fork-point picker over the session's earlier prompts; picking one forks the session
/// the way it stood **before** that prompt and returns its text to the composer (pi-style branch-and-edit).
///
/// The cut is asserted through the transcript: the child keeps the first turn and must not contain the
/// second turn's reply, while the dropped prompt text exists only in the composer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn fork_picker_lists_earlier_prompts_and_prefills_the_pick() {
    let content = ContentController::start().await.expect("start content");
    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Two committed turns: prompt 0 ("first") and prompt 1 ("second").
    for (prompt, reply) in [
        (FIRST_PROMPT, "reply to FIRSTPROMPTSENTINEL"),
        (SECOND_PROMPT, "reply to SECONDPROMPTSENTINEL"),
    ] {
        content.set_response(format!("{MOCK_RESPONSE_SENTINEL} {reply}"));
        harness
            .inject_keys(format!("{prompt}\r").as_bytes())
            .expect("submit prompt");
        harness
            .wait_for_text(reply, Duration::from_secs(30))
            .expect("turn rendered");
        harness
            .wait_for_turn_idle(Duration::from_secs(15))
            .expect("turn idle");
    }

    // `/fork` opens the picker with the cursor on the current-state row, so a bare Enter still forks
    // the whole conversation.
    harness.inject_keys(b"/fork\r").expect("run /fork");
    harness
        .wait_for_text(PICKER_TITLE, CURSOR_TIMEOUT)
        .expect("fork picker opens");
    expect_cursor_row(&mut harness, CURRENT_ROW, "initial cursor");

    // Esc closes it without forking.
    harness.inject_keys(keys::ESC).expect("dismiss fork picker");
    harness.update(Duration::from_millis(300));
    assert!(
        !harness.contains_text(PICKER_TITLE),
        "Esc must close the fork picker\nscreen:\n{}",
        harness.screen_contents()
    );

    // Reopen and move the cursor onto the earlier prompt: `k` leaves the current-state row.
    harness.inject_keys(b"/fork\r").expect("run /fork again");
    harness
        .wait_for_text(PICKER_TITLE, CURSOR_TIMEOUT)
        .expect("fork picker reopens");
    harness.inject_keys(keys::K).expect("cursor up");
    expect_cursor_row(&mut harness, SECOND_PROMPT, "after k");

    // Enter forks before the second prompt.
    harness
        .inject_keys(keys::ENTER)
        .expect("fork at the picked row");
    harness
        .wait_for_text("forked from", Duration::from_secs(30))
        .expect("child session loads and names its parent");

    let screen = harness.screen_contents();
    assert!(
        screen.contains("reply to FIRSTPROMPTSENTINEL"),
        "the fork keeps the turns before the picked prompt\nscreen:\n{screen}"
    );
    assert!(
        !screen.contains("reply to SECONDPROMPTSENTINEL"),
        "the fork must cut before the picked prompt\nscreen:\n{screen}"
    );
    assert!(
        screen.contains(SECOND_PROMPT),
        "the dropped prompt returns to the composer for rewriting\nscreen:\n{screen}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
