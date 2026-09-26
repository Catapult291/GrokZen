// Per-test-case module for the `pty_e2e` integration test crate.
//
// User report: once an answer has finished streaming, wheeling up from the transcript tail no longer
// offers the ▼ jump-to-bottom arrow, so there is no one-click way back to the newest content.
// The arrow's contract is "not at the tail and there is content below the viewport" — turn state is
// not part of it. This test pins the whole gesture on the real binary:
//   finished turn → wheel up → ▼ on screen → click it → viewport back at the tail.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

/// Transcript height: 240 one-row markers dwarf the 50-row PTY, so the wheel-up can never clamp at the top.
const MARKER_COUNT: usize = 240;

/// Rows wheeled up to leave the tail. Under the forced-wheel env each SGR report moves exactly one row.
const UP_EVENTS: usize = 6;

/// The follow indicator, drawn on its own row between the scrollback and the prompt.
const DOWN_INDICATOR: &str = "▼";

/// The ▼ must own its row: an arrow sharing a row with transcript text is the reported defect (the glyph
/// lands inside a line of the answer, which then scrolls past with a cursor-like block wedged in it).
fn assert_arrow_owns_its_row(screen: &str, arrow_row: u16) {
    let row = screen.lines().nth(arrow_row as usize).unwrap_or_default();
    let rest = row.replacen(DOWN_INDICATOR, " ", 1);
    assert!(
        rest.trim().is_empty(),
        "the ▼ shares row {arrow_row} with other content: {row:?}\nscreen:\n{screen}"
    );
}

/// Deterministic wheel pacing: one report is one row, so the parked position and the click target are stable.
fn forced_wheel_env() -> [(&'static str, &'static str); 3] {
    [
        ("TERM_PROGRAM", "zed"),
        ("GROK_SCROLL_MODE", "wheel"),
        ("GROK_SCROLL_LINES", "1"),
    ]
}

/// **Idle transcript: the ▼ must appear on wheel-up and click must return to the tail.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn follow_indicator_click_returns_to_tail_after_turn_end() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(marker_response(MOCK_RESPONSE_SENTINEL, MARKER_COUNT));
    let env = forced_wheel_env();

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env,
    )
    .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(&marker_line(MARKER_COUNT - 1), Duration::from_secs(60))
        .expect("response finished streaming");

    // The whole point of the report: the turn is OVER when the user wheels up.
    harness
        .wait_for_turn_idle(Duration::from_secs(30))
        .expect("turn reaches the idle prompt state");
    harness.update(Duration::from_millis(300));

    // Setup guards: bottom-pinned at the tail, transcript taller than the viewport.
    assert!(
        marker_screen_row(&harness, &marker_line(0)).is_none(),
        "setup: {} already visible → transcript not taller than the screen\nscreen:\n{}",
        marker_line(0),
        harness.screen_contents()
    );
    let top_before = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "setup: no marker visible after the turn ended\nscreen:\n{}",
            harness.screen_contents()
        )
    });

    // Wheel up to leave the tail, exactly as the reporter does.
    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        UP_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        Duration::ZERO,
    );
    harness.update(Duration::from_millis(800));

    let top_parked = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "no marker visible after the up-burst\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert!(
        top_parked < top_before,
        "wheel-up did not move the viewport off the tail: topmost visible marker {} → \
         {}\nscreen:\n{}",
        marker_line(top_before),
        marker_line(top_parked),
        harness.screen_contents()
    );

    let screen = harness.screen_contents();
    let (arrow_row, arrow_col) = locate_screen_text(&screen, DOWN_INDICATOR).unwrap_or_else(|| {
        panic!(
            "the ▼ jump-to-bottom arrow must stay available on an idle scrolled-up \
             transcript\nscreen:\n{screen}"
        )
    });
    assert_arrow_owns_its_row(&screen, arrow_row);

    // Click it (SGR left press and release at its cell).
    let click = format!(
        "{}{}",
        sgr_mouse(0, arrow_row, arrow_col, 'M'),
        sgr_mouse(0, arrow_row, arrow_col, 'm')
    );
    harness.inject_keys(click.as_bytes()).expect("click ▼");
    harness.update(Duration::from_millis(800));

    let screen = harness.screen_contents();
    assert!(
        screen.contains(&marker_line(MARKER_COUNT - 1)),
        "the click must land back on the transcript tail ({})\nscreen:\n{screen}",
        marker_line(MARKER_COUNT - 1)
    );
    assert!(
        marker_screen_row(&harness, &marker_line(0)).is_none(),
        "the click must not overshoot past the tail\nscreen:\n{screen}"
    );

    harness.quit().expect("clean quit");
}

/// **Compact prompt (`[ui] compact_mode = true`): the same gesture must still offer the ▼ on a row of its own.**
///
/// Compact prompts drop the one-row gap between the scrollback and the prompt box, so the layout owns no row
/// below the scrollback to draw the arrow on: that row is the box's top border, and the prompt chrome painted
/// afterwards covers the arrow. The arrow must instead take a row out of the scrollback itself — which renders
/// one row shorter — so it lands on blank space directly above the box, clickable, and never over a line of
/// the answer (painting there left the glyph wedged inside transcript text).
/// `compact_mode` is the trigger; the reporter's config carried it among plain layout settings
/// (a config sweep confirmed the rest — `show_timeline`, `screen_mode`, `simple_mode`, `page_flip_on_send` —
/// leaves the arrow untouched).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn follow_indicator_survives_a_compact_prompt_without_a_gap_row() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(marker_response(MOCK_RESPONSE_SENTINEL, MARKER_COUNT));
    let grok_home = content.sandbox().grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create grok home");
    std::fs::write(grok_home.join("config.toml"), "[ui]\ncompact_mode = true\n")
        .expect("write compact-mode config");
    let env = forced_wheel_env();

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env,
    )
    .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(&marker_line(MARKER_COUNT - 1), Duration::from_secs(60))
        .expect("response finished streaming");
    harness
        .wait_for_turn_idle(Duration::from_secs(30))
        .expect("turn reaches the idle prompt state");
    harness.update(Duration::from_millis(300));

    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        UP_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        Duration::ZERO,
    );
    harness.update(Duration::from_millis(800));

    let screen = harness.screen_contents();
    let (arrow_row, arrow_col) = locate_screen_text(&screen, DOWN_INDICATOR).unwrap_or_else(|| {
        panic!(
            "the ▼ jump-to-bottom arrow must survive a prompt with no reserved gap row\
             \nscreen:\n{screen}"
        )
    });
    // Setup guard: this run really is the compact shape (no gap row between the transcript and the box).
    let (box_row, _) = locate_screen_text(&screen, "╭")
        .unwrap_or_else(|| panic!("compact run must render a prompt box\nscreen:\n{screen}"));
    assert!(
        box_row == arrow_row + 1,
        "setup: expected the arrow on the row reserved above the box top \
         (box row {box_row}, arrow row {arrow_row})\nscreen:\n{screen}"
    );
    assert_arrow_owns_its_row(&screen, arrow_row);

    // Click it (SGR left press and release at its cell).
    let click = format!(
        "{}{}",
        sgr_mouse(0, arrow_row, arrow_col, 'M'),
        sgr_mouse(0, arrow_row, arrow_col, 'm')
    );
    harness.inject_keys(click.as_bytes()).expect("click ▼");
    harness.update(Duration::from_millis(800));

    let screen = harness.screen_contents();
    assert!(
        screen.contains(&marker_line(MARKER_COUNT - 1)),
        "the click must land back on the transcript tail ({})\nscreen:\n{screen}",
        marker_line(MARKER_COUNT - 1)
    );

    harness.quit().expect("clean quit");
}
