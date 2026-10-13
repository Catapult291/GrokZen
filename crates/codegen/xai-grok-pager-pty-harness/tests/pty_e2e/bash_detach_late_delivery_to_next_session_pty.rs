//! PTY: a completion left behind by a dead session is handed to the next
//! session that opens, exactly once — and only while it is fresh.
//!
//! A detached task outlives the session that started it, so the watcher that
//! would have reported its completion dies first. What is left is the durable
//! record, which a later session claims at startup. Driving a real detach
//! through this harness cannot exercise that: the harness tears down the
//! pager's process tree, which takes the detached worker with it (a real
//! terminal does not), so the task can never finish after the session is gone.
//! Instead the record that survived such a session is written directly and the
//! rest of the path runs for real: session startup scan, one-shot claim, the
//! reminder in the conversation, and the next turn the model actually runs.
#[allow(unused_imports)]
use super::common::*;

/// Original session of the daemon-style records: it is gone, which is the whole
/// premise of a late delivery.
const DEAD_SESSION: &str = "01a0f0aa-1111-7000-8000-0000000000ff";

fn background_task_root(content: &ContentController) -> PathBuf {
    content.sandbox().grok_home().join("background-tasks")
}

/// Everything the durable record has to say about a task that never got handed
/// over, for an assertion failure.
fn record_diagnostic(root: &Path) -> String {
    let mut out = format!("background-task root: {}\n", root.display());
    let Ok(entries) = std::fs::read_dir(root) else {
        return out + "(missing)\n";
    };
    for entry in entries.flatten() {
        out.push_str(&format!(
            "--- {}\nfiles: {:?}\n",
            entry.file_name().to_string_lossy(),
            std::fs::read_dir(entry.path())
                .map(|dir| dir
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
        ));
    }
    out
}

/// What the conversation and the model actually saw: the session transcript and
/// the last request body, both truncated.
fn conversation_diagnostic(content: &ContentController) -> String {
    let mut out = format!("requests seen: {}\n", content.request_bodies().len());
    if let Some(body) = content.request_bodies().last() {
        let body = body.to_string();
        out.push_str("last request body: ");
        out.push_str(&body[..body.len().min(3000)]);
        out.push('\n');
    }
    for history in session_histories(content) {
        let text = std::fs::read_to_string(&history).unwrap_or_default();
        out.push_str(&format!("{} ({} bytes):\n", history.display(), text.len()));
        out.push_str(&text[..text.len().min(4000)]);
        out.push('\n');
    }
    let path = unified_log_path(content);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    out.push_str(&format!("{} tail:\n", path.display()));
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().rev().take(40).rev() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Every session transcript under the sandbox grok home.
fn session_histories(content: &ContentController) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(cwds) = std::fs::read_dir(content.sandbox().grok_home().join("sessions")) else {
        return files;
    };
    for cwd in cwds.flatten() {
        let Ok(sessions) = std::fs::read_dir(cwd.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let history = session.path().join("chat_history.jsonl");
            if history.is_file() {
                files.push(history);
            }
        }
    }
    files
}

/// A `SystemTime` as serde writes it, `secs` before now.
fn epoch_secs_ago(secs: u64) -> serde_json::Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs();
    json!({"secs_since_epoch": now - secs, "nanos_since_epoch": 0})
}

/// Write the durable pair a dead session would have left behind: a detached
/// spec and the completed state the worker published.
///
/// The shape mirrors what `TaskRegistry` writes, field for field — reading it
/// back is what the startup scan does, so a field it cannot decode is a skipped
/// record, not a test failure.
fn write_orphan_task(content: &ContentController, task_id: &str, description: &str, age_secs: u64) {
    let directory = background_task_root(content).join(task_id);
    std::fs::create_dir_all(&directory).expect("create task dir");
    let output_file = directory.join("output.log");
    std::fs::write(&output_file, "orphaned output\n").expect("write output log");

    let spec = json!({
        "schema_version": 1,
        "task_id": task_id,
        "command": description,
        "cwd": content.home(),
        "env": {},
        "timeout_ms": null,
        "output_byte_limit": 20000,
        "output_file": output_file,
        "output_encoding": null,
        "tool_call_id": "call_orphan",
        "display_command": null,
        "kind": "bash",
        "owner_session_id": DEAD_SESSION,
        "description": description,
        "detach": true,
        "login_shell_capture": true,
        "shell_env_policy": null,
        "persistent_shell": false,
        "search_shadows": {"find_bfs": true, "grep_ugrep": true}
    });
    std::fs::write(
        directory.join("spec.json"),
        serde_json::to_vec(&spec).expect("encode spec"),
    )
    .expect("write spec.json");

    let state = json!({
        "ready": true,
        "worker_pid": null,
        "snapshot": {
            "task_id": task_id,
            "command": description,
            "cwd": content.home(),
            "start_time": epoch_secs_ago(age_secs + 30),
            "end_time": epoch_secs_ago(age_secs),
            "output": "",
            "output_file": output_file,
            "truncated": false,
            "output_total_bytes": 16,
            "exit_code": 0,
            "signal": null,
            "completed": true,
            "kind": "bash",
            "block_waited": false,
            "explicitly_killed": false,
            "kill_result_delivered": false,
            "owner_session_id": DEAD_SESSION,
            "description": description,
            "is_backgrounded": true,
            "detach": true
        }
    });
    std::fs::write(
        directory.join("state.json"),
        serde_json::to_vec(&state).expect("encode state"),
    )
    .expect("write state.json");
}

const FRESH_TASK: &str = "01a0f1bb-2222-7000-8000-000000000001";
const STALE_TASK: &str = "01a0f1bb-3333-7000-8000-000000000002";
const FRESH_DESCRIPTION: &str = "release build detached from its session";
const STALE_DESCRIPTION: &str = "ancient build nobody is waiting for";
/// Wording the reminder leads with, long enough not to collide with the
/// in-session completion messages. Kept to the clause that does not inflect,
/// so it matches whether the reminder lists one task or several.
const LATE_REMINDER_NEEDLE: &str = "finished after the session that started";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn late_delivery_hands_a_dead_sessions_detached_completion_to_the_next_session() {
    let content = ContentController::start().await.expect("start content");
    let project = content.home().join("late-delivery-project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let root = background_task_root(&content);

    // One completion inside the delivery window and one far outside it: the
    // window is what stops a machine with hundreds of stale records from
    // replaying all of them at once.
    write_orphan_task(&content, FRESH_TASK, FRESH_DESCRIPTION, 60);
    write_orphan_task(&content, STALE_TASK, STALE_DESCRIPTION, 3 * 24 * 3600);

    let binary = pager_binary().expect("resolve pager binary");
    let args = ["--yolo", "--trust"];
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &args,
        Some(project.as_path()),
    )
    .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");

    // The pager creates the agent session on the first prompt, and the scan is
    // a session-startup step, so the session has to exist before anything can
    // be claimed.
    content.set_response("FIRST_TURN_SETTLED");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit first prompt");
    harness
        .wait_for_text("FIRST_TURN_SETTLED", Duration::from_secs(45))
        .unwrap_or_else(|_| {
            panic!(
                "the first turn never settled:\n{}",
                harness.screen_contents()
            )
        });

    let fresh_marker = root.join(FRESH_TASK).join("delivery.json");
    let marker =
        wait_for(Duration::from_secs(30), || read_json(&fresh_marker)).unwrap_or_else(|| {
            panic!(
                "the session never claimed the pending completion\n{}",
                record_diagnostic(&root)
            )
        });
    assert_eq!(marker["state"], "delivered");
    assert_eq!(
        marker["late"], true,
        "the completion outlived its session, so it was a late hand-off: {marker}"
    );

    // The stale record is consumed too, so the scan does not keep re-reading it
    // — but it must not reach anybody.
    let stale_marker = root.join(STALE_TASK).join("delivery.json");
    assert!(
        wait_for(Duration::from_secs(30), || stale_marker
            .exists()
            .then_some(()))
        .is_some(),
        "an out-of-window completion must still be marked consumed\n{}",
        record_diagnostic(&root)
    );

    // ...and the model actually reads it, with the provenance it needs to judge
    // a task it has no memory of launching. The reminder is injected at the
    // start of a turn, so the next prompt's request carries it.
    content.set_response("LATE_DELIVERY_SETTLED");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit second prompt");
    harness
        .wait_for_text("LATE_DELIVERY_SETTLED", Duration::from_secs(45))
        .unwrap_or_else(|_| panic!("the turn never settled:\n{}", harness.screen_contents()));
    let delivered = content
        .request_bodies()
        .into_iter()
        .find(|body| body.to_string().contains(LATE_REMINDER_NEEDLE))
        .map(|body| body.to_string())
        .unwrap_or_else(|| {
            panic!(
                "the late-delivery reminder never reached the model\n{}\n{}",
                record_diagnostic(&root),
                conversation_diagnostic(&content),
            )
        });
    assert!(
        delivered.contains(FRESH_DESCRIPTION),
        "the reminder must name the task that finished: {delivered}"
    );
    assert!(
        delivered.contains(&DEAD_SESSION[..8]),
        "the reminder must say which session left it behind: {delivered}"
    );
    assert!(
        !delivered.contains(STALE_DESCRIPTION),
        "a completion outside the window must not be reported: {delivered}"
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
