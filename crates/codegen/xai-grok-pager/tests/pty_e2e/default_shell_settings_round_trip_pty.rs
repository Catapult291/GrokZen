// Per-test-case module for the `pty_e2e_config_ui` integration test crate.
#![cfg(windows)]

#[allow(unused_imports)]
use super::common::*;

const F2: &[u8] = b"\x1bOQ";
const SETTING_ROW_ZH: &str = "默认 Shell";
const DONE: &str = "DEFAULT_SHELL_PTY_DONE";

fn settings_path(content: &ContentController) -> std::path::PathBuf {
    content.home().join(".grok").join("config.toml")
}

fn spawn_pager(content: &ContentController, overrides: &[(&str, &str)]) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &["--yolo", "--trust", "--no-leader"],
        overrides,
        Some(content.home()),
    )
    .expect("spawn pager")
}

fn wait_for_welcome(harness: &mut PtyHarness) {
    harness
        .wait_for_text("Quit", WELCOME_TIMEOUT)
        .or_else(|_| harness.wait_for_text("退出", Duration::from_secs(1)))
        .expect("welcome text");
}

fn wait_for_settings(harness: &mut PtyHarness) {
    harness
        .wait_for_text("Settings", Duration::from_secs(10))
        .or_else(|_| harness.wait_for_text("设置", Duration::from_secs(1)))
        .expect("settings opened");
}

fn drive_to_agent_session(content: &ContentController, harness: &mut PtyHarness) {
    let sentinel = format!("{MOCK_RESPONSE_SENTINEL} settings prep");
    content.set_response(sentinel.clone());
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit settings prep prompt");
    harness
        .wait_for_text(&sentinel, Duration::from_secs(60))
        .expect("settings prep response");
    harness
        .wait_for_turn_idle(Duration::from_secs(20))
        .expect("settings prep turn idle");
}

fn filter_default_shell(harness: &mut PtyHarness) {
    harness.inject_keys(b"/").expect("start filter");
    for byte in "default shell".as_bytes() {
        harness
            .inject_keys(std::slice::from_ref(byte))
            .expect("filter byte");
        harness.update(Duration::from_millis(35));
    }
    harness
        .wait_for_text(SETTING_ROW_ZH, Duration::from_secs(8))
        .expect("default shell row visible");
    harness.inject_keys(b"\r").expect("commit filter");
    harness.update(Duration::from_millis(300));
}

fn open_default_shell_picker(harness: &mut PtyHarness) {
    harness.inject_keys(F2).expect("F2 open settings");
    wait_for_settings(harness);
    filter_default_shell(harness);
    // The fuzzy search also matches a neighbouring row that mentions Bash,
    // so jump to the final visible setting before opening the picker.
    harness
        .inject_keys(b"G")
        .expect("focus last filtered setting");
    harness.update(Duration::from_millis(200));
    harness.inject_keys(b"\r").expect("open picker");
    harness
        .wait_for_text("PowerShell 7+", Duration::from_secs(8))
        .expect("picker opened");
}

fn choose_picker_row(harness: &mut PtyHarness, choice: &str) {
    let target_idx = match choice {
        "Git Bash" => 0,
        "PowerShell 7+" => 1,
        "Windows PowerShell 5.1" => 2,
        other => panic!("unsupported shell choice {other}"),
    };
    for _ in 0..target_idx {
        harness.inject_keys(keys::DOWN).expect("picker down");
        harness.update(Duration::from_millis(150));
    }
    assert!(
        harness.contains_text(choice),
        "picker must show {choice}\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.inject_keys(b"\r").expect("commit choice");
    harness
        .wait_for_text("重启后生效", Duration::from_secs(8))
        .expect("restart-required toast");
}

fn close_settings(harness: &mut PtyHarness) {
    for _ in 0..4 {
        if !harness.contains_text("Settings") && !harness.contains_text("设置") {
            return;
        }
        harness.inject_keys(keys::ESC).expect("close settings");
        harness.update(Duration::from_millis(250));
    }
}

fn assert_welcome_shows_shell(harness: &mut PtyHarness, choice: &str) {
    wait_for_settings(harness);
    filter_default_shell(harness);
    harness.inject_keys(b"G").expect("focus default shell row");
    assert!(
        harness
            .screen_contents()
            .lines()
            .any(|line| line.contains(SETTING_ROW_ZH) && line.contains(choice)),
        "persisted row must show {choice}\nscreen:\n{}",
        harness.screen_contents()
    );
}

fn enqueue_shell_probe(content: &ContentController, command: String) {
    let args = json!({
        "command": command,
        "description": "record selected shell",
    })
    .to_string();
    let _ = expect_tool_turn(
        content,
        "call_default_shell_probe",
        "run_terminal_command",
        args,
    );
    content.set_response(DONE);
}

async fn run_shell_probe(config_body: &str, command: &str, verify: impl FnOnce(&str)) {
    let content = ContentController::start().await.expect("start content");
    seed_ui_config(&content, config_body);
    let probe = content.home().join("shell-probe.txt");
    let mut harness = spawn_pager(
        &content,
        &[
            ("GROK_ZH_LOCALE", "zh-CN"),
            ("SHELL_MARKER", probe.to_str().unwrap()),
        ],
    );
    wait_for_welcome(&mut harness);
    enqueue_shell_probe(&content, command.to_owned());
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit probe");
    harness
        .wait_for_text(DONE, Duration::from_secs(60))
        .unwrap_or_else(|error| {
            panic!(
                "shell probe failed: {error}\nscreen:\n{}\nconfig:\n{}",
                harness.screen_contents(),
                std::fs::read_to_string(settings_path(&content)).unwrap_or_default()
            )
        });
    harness
        .wait_for_turn_idle(Duration::from_secs(20))
        .expect("probe idle");
    let payload = std::fs::read_to_string(&probe).unwrap_or_default();
    verify(&payload);
    harness.quit().expect("clean quit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn default_shell_settings_persist_across_relaunch() {
    let content = ContentController::start().await.expect("start content");
    seed_ui_config(&content, "default_shell = \"git-bash\"");

    let mut first = spawn_pager(&content, &[("GROK_ZH_LOCALE", "zh-CN")]);
    wait_for_welcome(&mut first);
    drive_to_agent_session(&content, &mut first);
    open_default_shell_picker(&mut first);
    choose_picker_row(&mut first, "PowerShell 7+");
    close_settings(&mut first);
    first.quit().expect("clean quit");

    let saved = std::fs::read_to_string(settings_path(&content)).expect("read saved config");
    assert!(
        saved.contains("default_shell = \"pwsh\""),
        "settings action must persist canonical pwsh value\nconfig:\n{saved}"
    );

    let mut second = spawn_pager(&content, &[("GROK_ZH_LOCALE", "zh-CN")]);
    wait_for_welcome(&mut second);
    drive_to_agent_session(&content, &mut second);
    second
        .inject_keys(F2)
        .expect("open settings after relaunch");
    assert_welcome_shows_shell(&mut second, "PowerShell 7+");
    second.quit().expect("clean quit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn default_shell_config_actually_launches_each_supported_shell() {
    run_shell_probe(
        "default_shell = \"git-bash\"",
        "printf '%s|%s|%s' \"$(cygpath -w /usr/bin/bash)\" \"$BASH_VERSION\" \"$(uname -s)\" > \"$SHELL_MARKER\"",
        |payload| {
            let fields: Vec<&str> = payload.split('|').collect();
            assert!(
                fields
                    .first()
                    .is_some_and(|path| path.to_ascii_lowercase().contains("git"))
                    && fields
                        .first()
                        .is_some_and(|path| path.to_ascii_lowercase().ends_with("bash.exe")),
                "Git Bash probe output: {payload}"
            );
            assert!(!fields.get(1).is_some_and(|version| version.is_empty()));
            assert!(
                fields
                    .get(2)
                    .is_some_and(|system| system.contains("MINGW") || system.contains("MSYS")),
                "Git Bash probe output: {payload}"
            );
        },
    )
    .await;

    const POWERSHELL_PROBE: &str = "$line = [Environment]::GetEnvironmentVariable('SHELL_MARKER'); $path = (Get-Process -Id $PID).MainModule.FileName; $payload = '{0}|{1}|{2}' -f $PSVersionTable.PSEdition, $PSVersionTable.PSVersion, $path; [IO.File]::WriteAllText($line, $payload)";
    run_shell_probe("default_shell = \"pwsh\"", POWERSHELL_PROBE, |payload| {
        let fields: Vec<&str> = payload.split('|').collect();
        assert_eq!(
            fields.first().copied(),
            Some("Core"),
            "PowerShell probe output: {payload}"
        );
        assert_eq!(
            fields.get(1).and_then(|version| version.split('.').next()),
            Some("7"),
            "PowerShell probe output: {payload}"
        );
        assert!(
            fields
                .get(2)
                .is_some_and(|path| path.to_ascii_lowercase().ends_with("pwsh.exe")),
            "PowerShell probe output: {payload}"
        );
    })
    .await;

    run_shell_probe(
        "default_shell = \"powershell\"",
        POWERSHELL_PROBE,
        |payload| {
            let fields: Vec<&str> = payload.split('|').collect();
            assert_eq!(
                fields.first().copied(),
                Some("Desktop"),
                "PowerShell probe output: {payload}"
            );
            assert_eq!(
                fields.get(1).and_then(|version| version.split('.').next()),
                Some("5"),
                "PowerShell probe output: {payload}"
            );
            assert!(
                fields
                    .get(2)
                    .is_some_and(|path| path.to_ascii_lowercase().ends_with("powershell.exe")),
                "PowerShell probe output: {payload}"
            );
        },
    )
    .await;
}
