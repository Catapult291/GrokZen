//! PTY: a `run_terminal_command` call with `encoding: "gbk"` must decode the
//! command's GBK bytes to UTF-8 and leave the pager running.
//!
//! Before the fix the terminal actor finished its source decoder at pipe EOF and
//! then finished it again from the post-exit drain. `encoding_rs` panics on the
//! second finish ("Must not use a decoder that has finished"), which aborted the
//! whole pager and dumped the user onto the launch console with a Rust backtrace.
//!
//! The GBK bytes come from `printf` octal escapes, so the command itself stays
//! free of non-ASCII literals: what the shell emits is exactly the byte pair
//! `D6 D0 CE C4`, which is 中文 only under GBK.
#[allow(unused_imports)]
use super::common::*;

/// Double-click `needle`'s first cell to toggle its fold.
///
/// A model-invoked `run_terminal_command` is an `agent_execute` block, which
/// starts collapsed and stays collapsed on finish (see
/// `agent_execute_does_not_auto_expand_and_preserves_fold_on_finish`); only a
/// user `!` command expands itself. So the decoded bytes are on screen only
/// after the block is opened, and the assertion below must open it first.
fn double_click_text(harness: &mut PtyHarness, needle: &str) {
    let screen = harness.screen_contents();
    let (row, col) = locate_screen_text(&screen, needle)
        .unwrap_or_else(|| panic!("locate {needle:?}; screen:\n{screen}"));
    let click = format!(
        "{}{}",
        sgr_mouse(0, row, col, 'M'),
        sgr_mouse(0, row, col, 'm'),
    );
    for _ in 0..2 {
        harness
            .inject_keys(click.as_bytes())
            .expect("inject SGR click");
        harness.update(Duration::from_millis(100));
    }
}

const DESCRIPTION: &str = "GBK output";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn bash_encoding_gbk_output_renders_and_keeps_the_pager_alive() {
    let content = ContentController::start().await.expect("start content");
    // Folding on double-click only happens in flash mode; pin it so a parallel
    // suite sibling seeding hold/word_select cannot change the behaviour.
    seed_ui_config(&content, "keep_text_selection = \"flash\"");
    let args = json!({
        // ASCII sentinels around the GBK payload separate "the block never
        // rendered" from "it rendered but the CJK line did not survive", and
        // the trailing line keeps the payload off the block's last row.
        "command": "printf 'GBKHEAD\\n\\326\\320\\316\\304\\nGBKTAIL\\n'",
        "description": DESCRIPTION,
        "encoding": "gbk"
    })
    .to_string();
    let _gbk_turn = expect_tool_turn(&content, "call_gbk", "run_terminal_command", args);
    content.set_response("GBK_TURN_SETTLED");

    let binary = pager_binary().expect("resolve pager binary");
    // --yolo --trust keep the card and the folder-trust gate out of the way so
    // the only thing under test is the decoding path.
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
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    harness
        .wait_for_text("GBK_TURN_SETTLED", Duration::from_secs(45))
        .unwrap_or_else(|_| {
            panic!(
                "the GBK turn never settled; screen:\n{}",
                harness.screen_contents()
            )
        });

    // The tool block is collapsed by design; open it so the decoded payload is
    // actually on screen before asserting on it.
    harness.inject_keys(b"\t").expect("focus scrollback");
    harness
        .wait_for_text("Ctrl+e:", Duration::from_secs(10))
        .expect("scrollback owns keys");
    double_click_text(&mut harness, DESCRIPTION);

    let screen = harness.screen_contents();
    for sentinel in ["GBKHEAD", "GBKTAIL"] {
        assert!(
            screen.contains(sentinel),
            "the expanded block must render its output; {sentinel} missing means the \
             block rendered no content at all\nscreen:\n{screen}"
        );
    }
    assert!(
        screen.contains("中文"),
        "GBK output must reach the screen as UTF-8, not as raw bytes\nscreen:\n{screen}"
    );
    assert!(
        !screen.contains('\u{FFFD}'),
        "decoded output must not contain replacement characters\nscreen:\n{screen}"
    );
    // The failure mode this test exists for: the actor's decoder panicked and
    // took the pager with it.
    assert!(
        !screen.contains("panicked") && !screen.contains("decoder that has finished"),
        "the pager must not panic on a decoder that reached its end twice\nscreen:\n{screen}"
    );

    quit_minimal(&mut harness);
}
