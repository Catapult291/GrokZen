// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// One distinct prompt per turn, so a picker row and a transcript row can be told apart.
const PROMPTS: [&str; 3] = ["P1TURNSENTINEL", "P2TURNSENTINEL", "P3TURNSENTINEL"];

/// Reply for turn `i`: tall enough that every prompt — including the last — has more than a
/// screenful of content below it, so the picker's preview can anchor each one at the transcript top.
fn tall_reply(i: usize) -> String {
    let mut s = format!("{MOCK_RESPONSE_SENTINEL} R{i}REPLYSENTINEL");
    for line in 0..60 {
        s.push_str(&format!("\nR{i} reply filler {line}"));
    }
    s
}

const PICKER_TITLE: &str = "Rewind to which turn?";
const PICKER_TIMEOUT: Duration = Duration::from_secs(20);
const TURN_TIMEOUT: Duration = Duration::from_secs(45);
/// The rail's 2-column bright tick, in both the regular and the legacy-console form.
const ACTIVE_TICKS: [&str; 2] = ["\u{2501}\u{2501}", "\u{2550}\u{2550}"];

/// The picker's visible rows, top to bottom, as the prompt sentinel each one shows.
/// Rows below the title belong to the overlay, so the transcript above cannot leak in.
fn picker_rows(harness: &PtyHarness) -> Vec<&'static str> {
    let screen = harness.screen_contents();
    let Some(title_row) = screen.lines().position(|line| line.contains(PICKER_TITLE)) else {
        return Vec::new();
    };
    screen
        .lines()
        .skip(title_row + 1)
        .filter_map(|line| {
            PROMPTS
                .iter()
                .find(|prompt| line.contains(**prompt))
                .copied()
        })
        .collect()
}

/// The picker row carrying the cursor band, identified by its background differing from the other
/// rows' (reading the styled screen is the only way to observe the selection from outside).
fn picker_cursor_row(harness: &PtyHarness) -> Option<&'static str> {
    let styled = harness.screen_styled();
    let title_row = styled.iter().position(|line| {
        line.runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .contains(PICKER_TITLE)
    })?;
    let mut rows: Vec<(&'static str, Option<String>)> = Vec::new();
    for line in styled.iter().filter(|line| line.line > title_row) {
        let text: String = line.runs.iter().map(|run| run.text.as_str()).collect();
        let Some(label) = PROMPTS
            .iter()
            .find(|prompt| text.contains(**prompt))
            .copied()
        else {
            continue;
        };
        let bg = line
            .runs
            .iter()
            .find(|run| run.text.contains(label))
            .and_then(|run| run.bg.clone());
        rows.push((label, bg));
    }
    rows.iter()
        .find(|(_, bg)| rows.iter().filter(|(_, other)| other == bg).count() == 1)
        .map(|(label, _)| *label)
}

/// The topmost screen line showing a prompt, as `(row, sentinel)`.
/// The transcript owns the rows above the overlay, so this is its top-anchored prompt.
fn topmost_prompt_row(harness: &PtyHarness) -> Option<(usize, &'static str)> {
    harness
        .screen_contents()
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            PROMPTS
                .iter()
                .find(|prompt| line.contains(**prompt))
                .map(|prompt| (row, *prompt))
        })
}

/// Screen row of the timeline rail's bright (active) tick: the heavy 2-column stroke only the active
/// tick draws, restricted to the rail's columns at the right edge of the transcript pane.
fn active_tick_row(harness: &PtyHarness) -> Option<usize> {
    let rail_col_min = DEFAULT_COLS - 4;
    harness.screen_styled().iter().find_map(|line| {
        let mut col = 0u16;
        for run in &line.runs {
            if col >= rail_col_min && ACTIVE_TICKS.iter().any(|tick| run.text.contains(tick)) {
                return Some(line.line);
            }
            col += unicode_width::UnicodeWidthStr::width(run.text.as_str()) as u16;
        }
        None
    })
}

/// The `/undo` and `/rewind` turn picker borrows the `/fork` picker's shape: rows run oldest first
/// (conversation order), and the row under the cursor is previewed by scrolling its prompt to the
/// top of the transcript, which is also the turn the timeline rail highlights.
///
/// The timeline setting is on so the rail is on screen; its active tick is the turn owning the
/// viewport top row, so it is the outside-the-process witness that the right turn was anchored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn rewind_picker_lists_turns_oldest_first_and_anchors_the_picked_turn() {
    let content = ContentController::start().await.expect("start content");
    // The rail is off by default; the picker preview is what has to drive its highlight.
    seed_ui_config(&content, "show_timeline = true");

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

    // Three committed turns, prompts 0..=2.
    for (i, prompt) in PROMPTS.iter().enumerate() {
        content.set_response(tall_reply(i));
        harness
            .inject_keys(format!("{prompt}\r").as_bytes())
            .expect("submit prompt");
        harness
            .wait_for_text(&format!("R{i} reply filler 59"), TURN_TIMEOUT)
            .expect("turn rendered");
        harness
            .wait_for_turn_idle(Duration::from_secs(20))
            .expect("turn idle");
    }

    // `/undo` opens the same picker `/rewind` does, without the mode confirm behind it.
    harness.inject_keys(b"/undo\r").expect("run /undo");
    harness
        .wait_for_text(PICKER_TITLE, PICKER_TIMEOUT)
        .unwrap_or_else(|e| {
            panic!(
                "/undo must open the picker: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    harness.update(Duration::from_millis(600));

    // Rows run oldest first, the way the conversation (and the `/fork` picker) reads.
    assert_eq!(
        picker_rows(&harness),
        PROMPTS.to_vec(),
        "picker rows must run oldest first\nscreen:\n{}",
        harness.screen_contents()
    );

    // The cursor starts on the newest turn (the least destructive cut) and has anchored it on top.
    assert_eq!(
        picker_cursor_row(&harness),
        Some(PROMPTS[2]),
        "the cursor starts on the newest turn\nscreen:\n{}",
        harness.screen_contents()
    );
    let (row, label) = topmost_prompt_row(&harness).expect("a prompt on screen");
    assert_eq!(
        label,
        PROMPTS[2],
        "the cursor row's prompt must own the transcript top\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        row <= 5,
        "anchored at the top of the transcript pane, got row {row}\nscreen:\n{}",
        harness.screen_contents()
    );
    let active_before = active_tick_row(&harness).unwrap_or_else(|| {
        panic!(
            "the timeline rail must show an active tick\nscreen:\n{}",
            harness.screen_contents()
        )
    });

    // Walking the cursor up two rows drags both the anchored turn and the rail highlight with it.
    for _ in 0..2 {
        harness.inject_keys(keys::K).expect("cursor up");
        harness.update(Duration::from_millis(500));
    }

    assert_eq!(
        picker_cursor_row(&harness),
        Some(PROMPTS[0]),
        "the cursor lands on the oldest turn\nscreen:\n{}",
        harness.screen_contents()
    );
    let (row, label) = topmost_prompt_row(&harness).expect("a prompt on screen");
    assert_eq!(
        label,
        PROMPTS[0],
        "the newly picked turn must own the transcript top\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        row <= 5,
        "anchored at the top of the transcript pane, got row {row}\nscreen:\n{}",
        harness.screen_contents()
    );
    let active_after = active_tick_row(&harness).unwrap_or_else(|| {
        panic!(
            "the timeline rail must still show an active tick\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert_eq!(
        active_before - active_after,
        2,
        "the rail highlight must step up one tick per row crossed ({active_before} -> {active_after})\nscreen:\n{}",
        harness.screen_contents()
    );

    // The anchored preview has scrolled the tail off screen, which is what makes the Esc restore
    // below observable.
    let tail_marker = "R2 reply filler 59";
    assert!(
        !harness.contains_text(tail_marker),
        "the anchored preview must have scrolled the tail off screen\nscreen:\n{}",
        harness.screen_contents()
    );

    // Esc closes the picker without rewinding anything, and puts the transcript back at the tail it
    // was showing before the picker opened: the preview scrolling must not outlive the flow.
    harness.inject_keys(keys::ESC).expect("dismiss picker");
    harness.update(Duration::from_millis(400));
    assert!(
        !harness.contains_text(PICKER_TITLE),
        "Esc must close the picker\nscreen:\n{}",
        harness.screen_contents()
    );
    harness
        .wait_until(
            "Esc returns the transcript to the tail",
            Duration::from_secs(5),
            |h| h.contains_text(tail_marker),
        )
        .unwrap_or_else(|e| panic!("{e}\nscreen:\n{}", harness.screen_contents()));
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
