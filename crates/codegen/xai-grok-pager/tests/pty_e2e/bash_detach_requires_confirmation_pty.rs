//! PTY: `detach: true` on a background `run_terminal_command` call must reach
//! the user, and only a confirmed detach may outlive the session.
//!
//! A detached process keeps running with no session left to stop it, so the
//! access is deliberately given no auto-approve shortcut: even in
//! always-approve mode the card opens, while an ordinary command in the same
//! session still runs without one. The confirmation is the whole reason the
//! durable worker exists, so these tests pin both the prompt and its outcomes.
//!
//! `Ctrl+C` is the card's cancel (the only one). It resolves the request as
//! `Cancelled`, which the session turns into a cancelled turn rather than a
//! rejected tool call — so there is no follow-up model turn to wait for, and
//! the assertion that matters is the one about the command not running.
#[allow(unused_imports)]
use super::common::*;

/// Text of the permission card's reject row, in the locale the pager renders.
///
/// The pager is localized, so the hard-coded English needle never appears in a
/// zh-CN run. Both catalog spellings are accepted: the row is what these tests
/// are about, and its wording is a presentation detail.
fn reject_row_visible(screen: &str) -> bool {
    const EN: &str = "No, reject";
    const ZH: &str = "否，拒绝";
    screen.contains(EN) || screen.contains(ZH)
}

/// Block until the reject row is on screen in any supported locale.
fn wait_for_reject_row(harness: &mut PtyHarness, timeout: Duration) -> Result<(), ()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if reject_row_visible(&harness.screen_contents()) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        harness.update(Duration::from_millis(100));
    }
}

/// Marker a rejected detach must never create.
fn marker_path(content: &ContentController, name: &str) -> PathBuf {
    content.home().join(name)
}

/// Poll `cond` until it holds or `timeout` elapses.
///
/// `common::wait_until` is `#[cfg(unix)]`, and the survival assertion below is
/// about files rather than processes, so it does not need a Unix-only probe.
fn poll_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Shell form of `path` for the command line: the pager's shell on Windows is
/// Git Bash, which reads `C:/…` but not `C:\…`.
fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn bash_detach_prompts_under_always_approve_and_reject_does_not_run() {
    let content = ContentController::start().await.expect("start content");
    let marker = marker_path(&content, "detach_rejected.marker");

    // Control: an ordinary command in the same session is auto-approved, so no
    // card can be up when that turn settles.
    //
    // One user turn is one model call *per tool round-trip*: a tool call does not
    // end the turn, so each turn needs a scripted tool call AND a scripted text
    // reply to close it. Registering only the two tool calls made the second one
    // land inside the first turn, which is how the detach ran before its own
    // prompt was ever submitted. Foreground expectations are consumed FIFO.
    let plain = json!({
        "command": "echo plain-control",
        "description": "control"
    })
    .to_string();
    let _plain_call = expect_tool_turn(&content, "call_plain", "run_terminal_command", plain);
    let _plain_done = content.expect_agent_turn("plain turn closed", "PLAIN_TURN_SETTLED");

    let detach = json!({
        "command": format!("echo ran > {}", shell_path(&marker)),
        "description": "detach that must be rejected",
        "is_background": true,
        "detach": true
    })
    .to_string();
    let _detach_call = expect_tool_turn(&content, "call_detach", "run_terminal_command", detach);

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
        .expect("welcome");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit control prompt");
    harness
        .wait_for_text("PLAIN_TURN_SETTLED", Duration::from_secs(45))
        .unwrap_or_else(|_| {
            panic!(
                "the control turn never settled; screen:\n{}",
                harness.screen_contents()
            )
        });
    assert!(
        !reject_row_visible(&harness.screen_contents()),
        "always-approve must still auto-approve a plain command: no card may be open\nscreen:\n{}",
        harness.screen_contents()
    );

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit detach prompt");
    wait_for_reject_row(&mut harness, Duration::from_secs(45)).unwrap_or_else(|_| {
        panic!(
            "a detached background command must open a confirmation card even in \
                 always-approve mode; screen:\n{}",
            harness.screen_contents()
        )
    });

    // Ctrl+C is the card's only cancel. The request resolves as `Cancelled`,
    // which the session turns into a cancelled turn, so nothing further is
    // scripted for it — the property under test is that the command never ran.
    harness
        .inject_keys(b"\x03")
        .expect("reject the detach request");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while reject_row_visible(&harness.screen_contents()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the detach card stayed open after Ctrl+C; screen:\n{}",
            harness.screen_contents()
        );
        harness.update(Duration::from_millis(100));
    }

    assert!(
        !poll_until(Duration::from_secs(10), || marker.exists()),
        "a rejected detach must not run the command; marker {} exists",
        marker.display()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn bash_detach_confirmed_task_outlives_the_session() {
    let content = ContentController::start().await.expect("start content");
    let heartbeat = marker_path(&content, "detach_heartbeat.txt");
    let ticks = shell_path(&heartbeat);

    // A bounded loop: it outlasts the test by far, but cannot leak a ~68-year
    // sleep on a regression. Each tick appends a line, so growth is observable
    // after the pager is gone.
    let command = format!(
        "i=0; while [ $i -lt 2000 ]; do echo tick >> '{ticks}'; i=$((i+1)); sleep 0.2; done"
    );
    let detach = json!({
        "command": command,
        "description": "detach that must survive the session",
        "is_background": true,
        "detach": true
    })
    .to_string();
    let _detach_turn =
        expect_tool_turn(&content, "call_detach_keep", "run_terminal_command", detach);
    content.set_response("DETACH_KEEP_SETTLED");

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
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit detach prompt");
    wait_for_reject_row(&mut harness, Duration::from_secs(45)).unwrap_or_else(|_| {
        panic!(
            "the detach card must open; screen:\n{}",
            harness.screen_contents()
        )
    });

    // The first option is an allow option (the always-approve shortcut, or the
    // session allow row), so `1` confirms.
    harness.inject_keys(b"1").expect("confirm the detach");

    assert!(
        poll_until(Duration::from_secs(30), || heartbeat.exists()),
        "a confirmed detach must run the command; screen:\n{}",
        harness.screen_contents()
    );

    // The durable record is what a later session rediscovers, and it is also
    // where teardown reads the flag that spares the task.
    let spec = wait_for_detached_spec(&content, Duration::from_secs(15));
    let spec: serde_json::Value = serde_json::from_str(&spec).expect("decode spec.json");
    assert_eq!(
        spec["detach"], true,
        "the durable spec must record the confirmed detach: {spec}"
    );

    let before = heartbeat_len(&heartbeat);
    quit_minimal(&mut harness);
    assert!(
        before > 0,
        "the heartbeat must have been written before the pager exited"
    );

    // The pager is gone; a detached task keeps ticking.
    let grew = poll_until(Duration::from_secs(20), || {
        heartbeat_len(&heartbeat) > before
    });
    assert!(
        grew,
        "a confirmed detach must outlive the session: {} stopped growing after the pager exited",
        heartbeat.display()
    );

    // Clean up the survivor through the same durable record teardown uses, so
    // the test cannot leak a background loop.
    kill_detached_task(&content);
}

/// Length of the heartbeat file, or 0 when it does not exist yet.
fn heartbeat_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Read the first durable `spec.json` under the sandbox's background-task root.
fn wait_for_detached_spec(content: &ContentController, timeout: Duration) -> String {
    let root = content.sandbox().grok_home().join("background-tasks");
    let read = |root: &Path| -> Option<String> {
        std::fs::read_dir(root)
            .ok()?
            .flatten()
            .find_map(|entry| std::fs::read_to_string(entry.path().join("spec.json")).ok())
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(spec) = read(&root) {
            return spec;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "no durable background task record under {} after {timeout:?}",
                root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ask the durable worker to stop, mirroring the kill path a later session uses.
fn kill_detached_task(content: &ContentController) {
    let root = content.sandbox().grok_home().join("background-tasks");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = std::fs::write(entry.path().join("kill.request"), b"1");
    }
}
