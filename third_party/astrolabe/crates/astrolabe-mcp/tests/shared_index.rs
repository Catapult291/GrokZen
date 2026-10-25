//! Shared index across sessions: leader + followers over a private Unix
//! socket (`src/shared_index.rs`). Unix only: Windows always indexes per
//! session.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Short: the socket directory must fit macOS sun_path.
    let dir = std::env::temp_dir().join(format!("as-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.canonicalize().unwrap_or(dir)
}

struct Fixture {
    root: PathBuf,
    sock_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = temp_dir("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("hello.py"),
            "def greet():\n    return 'hi'\n\n\ndef caller():\n    return greet()\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.py"),
            "from hello import greet\n\n\ndef twice():\n    return greet() + greet()\n",
        )
        .unwrap();
        Self {
            root,
            sock_dir: temp_dir("sock"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.sock_dir);
    }
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    log: PathBuf,
    next_id: u64,
}

impl Session {
    fn spawn(fx: &Fixture, name: &str, env: &[(&str, &str)]) -> Self {
        let log = fx.sock_dir.with_file_name(format!(
            "{}-{name}.log",
            fx.sock_dir.file_name().unwrap().to_string_lossy()
        ));
        let stderr = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_astrolabe"));
        cmd.arg(".")
            .current_dir(&fx.root)
            .env("RUST_LOG", "info")
            .env("ASTROLABE_SHARED_INDEX_DIR", &fx.sock_dir)
            .env_remove("ASTROLABE_SHARED_INDEX")
            .env_remove("ASTROLABE_STRUCTURED")
            .env_remove("ASTROLABE_ROOT")
            .env_remove("ASTROLABE_CONTEXT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("start astrolabe");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            log,
            next_id: 0,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let line = json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params});
        let stdin = self.stdin.as_mut().expect("session open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut response = String::new();
        self.stdout.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap_or_else(|e| panic!("bad response {response:?}: {e}"))
    }

    fn call_text(&mut self, tool: &str, args: Value) -> String {
        let response = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        assert_eq!(response.get("error"), None, "{response}");
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Call until the index is ready (cold build / warm load).
    fn call_ready(&mut self, tool: &str, args: Value) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let text = self.call_text(tool, args.clone());
            if !text.contains("索引正在后台构建") || Instant::now() > deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&self, needle: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.log().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Graceful exit: the client closes stdin.
    fn close(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
    }

    fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

const LEADER: &str = "shared index: leader, serving other sessions";
const FOLLOWER: &str = "shared index: following the leader";
const INDEPENDENT: &str = "shared index: indexing independently";
const BUILT: &str = "indexing repository";

fn greet_symbol(s: &mut Session) -> String {
    s.call_ready("find_symbol", json!({"query": "greet"}))
}

#[test]
fn two_followers_forward_to_one_leader() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    assert!(greet_symbol(&mut leader).contains("hello.py"));
    assert!(
        leader.wait_log(LEADER, Duration::from_secs(5)),
        "{}",
        leader.log()
    );

    let mut f1 = Session::spawn(&fx, "f1", &[]);
    let mut f2 = Session::spawn(&fx, "f2", &[("ASTROLABE_SHARED_INDEX", "on")]);
    let a = greet_symbol(&mut f1);
    let b = greet_symbol(&mut f2);
    assert!(a.contains("hello.py") && b.contains("hello.py"), "{a}\n{b}");
    // tools/list is served by the follower itself.
    let listed = f1.rpc("tools/list", json!({}));
    assert!(listed["result"]["tools"]
        .as_array()
        .is_some_and(|t| !t.is_empty()));
    let refs = f2.call_text("find_references", json!({"symbol": "greet"}));
    assert!(refs.contains("other.py"), "{refs}");

    for f in [&f1, &f2] {
        let log = f.log();
        assert!(log.contains(FOLLOWER), "{log}");
        // A follower never starts a watcher or builds an index.
        assert!(
            !log.contains("index build") && !log.contains("watch"),
            "{log}"
        );
    }
    let attached = leader
        .log()
        .matches("shared index: follower attached")
        .count();
    assert_eq!(attached, 2, "{}", leader.log());
    assert!(leader.log().contains(BUILT), "{}", leader.log());
    for f in [&f1, &f2] {
        assert!(
            !f.log().contains(BUILT),
            "follower says it indexes:\n{}",
            f.log()
        );
    }
}

#[test]
fn independent_session_starts_serving_once_it_takes_the_lock() {
    let fx = Fixture::new();
    // A non-sharing holder of the lock (older version / opted out).
    let mut legacy = Session::spawn(&fx, "legacy", &[("ASTROLABE_SHARED_INDEX", "off")]);
    greet_symbol(&mut legacy);
    let mut b = Session::spawn(&fx, "b", &[]);
    greet_symbol(&mut b);
    assert!(b.log().contains(INDEPENDENT), "{}", b.log());
    assert!(!b.log().contains(LEADER), "{}", b.log());
    // The holder exits: the independent session takes the lock and serves.
    legacy.close();
    assert!(
        b.wait_log("took over the indexer lock", Duration::from_secs(5)),
        "{}",
        b.log()
    );
    let mut c = Session::spawn(&fx, "c", &[]);
    assert!(greet_symbol(&mut c).contains("hello.py"));
    assert!(c.log().contains(FOLLOWER), "{}", c.log());
    assert!(!c.log().contains(INDEPENDENT), "{}", c.log());
    assert!(
        b.log().contains("shared index: follower attached"),
        "{}",
        b.log()
    );
}

#[test]
fn toolchain_env_mismatch_falls_back_to_independent() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    let path = format!(
        "{}:/nonexistent-astrolabe-bin",
        std::env::var("PATH").unwrap_or_default()
    );
    let mut other = Session::spawn(&fx, "path", &[("PATH", path.as_str())]);
    assert!(greet_symbol(&mut other).contains("hello.py"));
    let log = other.log();
    assert!(
        log.contains(INDEPENDENT) && log.contains("toolchain env differs"),
        "{log}"
    );
    // Same toolchain env: shares.
    let mut same = Session::spawn(&fx, "same", &[]);
    greet_symbol(&mut same);
    assert!(same.log().contains(FOLLOWER), "{}", same.log());
}

#[test]
fn per_session_state_is_isolated() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    let mut f1 = Session::spawn(&fx, "f1", &[]);
    let mut f2 = Session::spawn(&fx, "f2", &[]);
    greet_symbol(&mut f1);
    greet_symbol(&mut f2);
    assert!(f1.log().contains(FOLLOWER) && f2.log().contains(FOLLOWER));

    let query = json!({"query": "greet"});
    let first = f1.call_text("search_code", query.clone());
    let again = f1.call_text("search_code", query.clone());
    assert!(
        again.contains("同一查询已在本会话"),
        "dedup in the same session:\n{again}"
    );
    // Another session asking the same thing gets the full result.
    let other = f2.call_text("search_code", query.clone());
    assert!(
        !other.contains("同一查询已在本会话"),
        "dedup leaked across sessions:\n{other}"
    );
    assert_eq!(first, other, "same index, same first answer");
    // …and so does the leader's own client.
    let own = leader.call_text("search_code", query);
    assert!(!own.contains("同一查询已在本会话"), "{own}");
}

#[test]
fn leader_kill_hands_over_to_a_follower() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    let mut f1 = Session::spawn(&fx, "f1", &[]);
    let mut f2 = Session::spawn(&fx, "f2", &[]);
    greet_symbol(&mut f1);
    greet_symbol(&mut f2);

    leader.kill9();
    // Both keep answering: one takes the lock and warm-loads, the other
    // reconnects to it.
    let a = greet_symbol(&mut f1);
    let b = greet_symbol(&mut f2);
    assert!(a.contains("hello.py") && b.contains("hello.py"), "{a}\n{b}");
    let (l1, l2) = (f1.log(), f2.log());
    let leaders = [&l1, &l2].iter().filter(|l| l.contains(LEADER)).count();
    assert_eq!(leaders, 1, "exactly one new leader:\n{l1}\n---\n{l2}");
    let (new_leader, survivor) = if l1.contains(LEADER) {
        (&l1, &l2)
    } else {
        (&l2, &l1)
    };
    assert!(new_leader.contains("leader went away"), "{new_leader}");
    assert!(
        survivor.matches(FOLLOWER).count() >= 2,
        "the other follower reconnects:\n{survivor}"
    );
    assert!(!survivor.contains(INDEPENDENT), "{survivor}");
}

#[test]
fn graceful_leader_exit_closes_socket_then_lock() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    let mut f1 = Session::spawn(&fx, "f1", &[]);
    greet_symbol(&mut f1);
    leader.close();
    // The follower takes over without falling back to an independent index.
    let a = greet_symbol(&mut f1);
    assert!(a.contains("hello.py"), "{a}");
    assert!(f1.wait_log(LEADER, Duration::from_secs(5)), "{}", f1.log());
    assert!(!f1.log().contains(INDEPENDENT), "{}", f1.log());
    // A brand-new session follows the new leader.
    let mut f2 = Session::spawn(&fx, "f2", &[]);
    greet_symbol(&mut f2);
    assert!(f2.log().contains(FOLLOWER), "{}", f2.log());
}

#[test]
fn settings_or_version_mismatch_falls_back_to_independent() {
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    // Different answer-shaping settings: refused, indexes on its own.
    let mut odd = Session::spawn(&fx, "odd", &[("ASTROLABE_REINDEX_MS", "1234")]);
    let text = greet_symbol(&mut odd);
    assert!(text.contains("hello.py"), "{text}");
    let log = odd.log();
    assert!(
        log.contains(INDEPENDENT) && log.contains("settings differs"),
        "{log}"
    );
    assert!(
        leader.log().contains("refused a follower"),
        "{}",
        leader.log()
    );
    // Opted out: never touches the socket.
    let mut off = Session::spawn(&fx, "off", &[("ASTROLABE_SHARED_INDEX", "off")]);
    assert!(greet_symbol(&mut off).contains("hello.py"));
    assert!(!off.log().contains("shared index:"), "{}", off.log());
}

#[test]
fn socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let mut leader = Session::spawn(&fx, "l", &[]);
    greet_symbol(&mut leader);
    let dir_mode = std::fs::metadata(&fx.sock_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    let socks: Vec<PathBuf> = std::fs::read_dir(&fx.sock_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert_eq!(socks.len(), 1, "{socks:?}");
    let mode = std::fs::metadata(&socks[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    leader.close();
    assert!(
        !Path::new(&socks[0]).exists(),
        "graceful exit unlinks the socket"
    );
}
