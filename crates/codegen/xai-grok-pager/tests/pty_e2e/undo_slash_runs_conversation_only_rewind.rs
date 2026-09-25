// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Copy the `/undo` row advertises; it is the row's own description, not `/rewind`'s.
const UNDO_DESCRIPTION: &str = "Rewind the conversation to an earlier turn";

/// Picker title before any turn is chosen (see `rewind.picker.title`).
const REWIND_PICKER_TITLE: &str = "Rewind to which turn?";

/// Toast the client renders for a conversation-only rewind (see `rewind.reverted.conversation`).
const REVERTED_CONVERSATION: &str = "Reverted conversation";

/// Toast for the conversation-and-files half. `assert!(!contains_text)` distinguishes it from the
/// conversation-only toast, which is a substring of this one.
const REVERTED_ALL: &str = "Reverted conversation and files";

/// `/undo` is its own command, not a `/rewind` alias: the slash menu lists it with the
/// conversation-only description, and running it rolls back the conversation alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn undo_slash_runs_conversation_only_rewind() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} undo probe."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        Some(content.home()),
    )
    .expect("spawn pager with content");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    // One completed turn, so `/undo` has a rewind point to offer.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn rendered");
    harness.update(Duration::from_millis(800));

    // Typing the command opens the slash menu; only an advertised command renders its own description.
    inject_keys_paced(&mut harness, b"/undo");
    harness
        .wait_for_text(UNDO_DESCRIPTION, Duration::from_secs(15))
        .unwrap_or_else(|e| {
            panic!(
                "/undo must be advertised as its own command: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    harness.inject_keys(b"\r").expect("run /undo");
    harness
        .wait_for_text(REWIND_PICKER_TITLE, Duration::from_secs(20))
        .unwrap_or_else(|e| {
            panic!(
                "/undo must open the rewind picker: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    harness.inject_keys(b"\r").expect("pick the turn");
    harness
        .wait_for_text(REVERTED_CONVERSATION, Duration::from_secs(30))
        .unwrap_or_else(|e| {
            panic!(
                "the rewind must report a conversation rewind: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    assert!(
        !harness.contains_text(REVERTED_ALL),
        "`/undo` must not revert files (that is `/rewind`'s job)\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text(MOCK_RESPONSE_SENTINEL),
        "the rewound turn must leave the transcript\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
