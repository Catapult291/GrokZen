use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::KillOnDrop;

/// Spawn a fixture child, detached and with every standard stream discarded.
fn spawn(cmd: &mut Command) -> Child {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::detach_std_command(cmd);
    #[allow(clippy::disallowed_methods)] // test fixture; guarded/reaped by the test
    cmd.spawn().expect("spawn test child")
}

#[cfg(unix)]
fn spawn_sleeper() -> Child {
    let mut cmd = Command::new("sleep");
    cmd.arg("300");
    spawn(&mut cmd)
}

/// `ping` is the cheapest always-present stand-in for `sleep`: 300 one-second
/// loopback pings, killed long before they finish.
#[cfg(windows)]
fn spawn_sleeper() -> Child {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "ping", "-n", "300", "127.0.0.1"]);
    spawn(&mut cmd)
}

#[cfg(unix)]
fn exit_zero_command() -> Command {
    Command::new("true")
}

#[cfg(windows)]
fn exit_zero_command() -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "exit", "0"]);
    cmd
}

/// Zombie-tolerant bounded probe: the contract is that the child stops
/// *running*; whether the corpse is reaped promptly is environmental.
fn assert_stops_running(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !crate::process_not_running(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        crate::process_not_running(pid),
        "{what} still running after drop"
    );
}

#[test]
fn drop_kills_and_reaps_the_child() {
    let guard = KillOnDrop::new(spawn_sleeper());
    let pid = guard.id();
    assert!(!crate::process_not_running(pid), "sanity: sleeper running");

    drop(guard);

    assert_stops_running(pid, "KillOnDrop child");
}

#[test]
fn into_inner_disarms_without_killing() {
    let guard = KillOnDrop::new(spawn_sleeper());
    let pid = guard.id();

    let mut child = guard.into_inner();

    assert!(
        !crate::process_not_running(pid),
        "into_inner must release the child without killing it"
    );
    child.kill().expect("kill released child");
    child.wait().expect("reap released child");
}

#[test]
fn drop_after_in_handle_reap_is_a_no_op() {
    let mut cmd = exit_zero_command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::detach_std_command(&mut cmd);
    #[allow(clippy::disallowed_methods)] // test fixture; reaped through the guard
    let mut guard = KillOnDrop::new(cmd.spawn().expect("spawn exit-zero child"));

    let status = guard.wait().expect("in-handle reap through the guard");
    assert!(status.success(), "exit-zero child succeeds");

    // Drop after the in-handle reap must not panic and must not signal a
    // recycled PID (std's Child::kill refuses already-waited children).
    drop(guard);
}
