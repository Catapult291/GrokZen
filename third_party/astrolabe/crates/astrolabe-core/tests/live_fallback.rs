//! S4 end-to-end checks of the language-server fallback against real clangd.
//!
//! Ignored by default: they need clangd. Point `ASTROLABE_TEST_CLANGD` at the
//! binary (or have `clangd` on `PATH`) and run
//! `cargo test -p astrolabe-core --test live_fallback -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrolabe_core::lsp::pool::{LspPool, LspPoolConfig, SpawnServer};
use astrolabe_core::lsp::transport::{LspSession, LspTransport, StdioSpawn};
use astrolabe_core::lsp::{LanguageServer, Position, ServerSpec};
use astrolabe_core::types::{Language, RelPath};

const B_H: &str = "#pragma once\nint helper(int x);\n";
const B_CPP: &str = "#include \"b.h\"\nint helper(int x) { return x + 1; }\n";
const A_CPP: &str = "#include \"b.h\"\nint use_one() { return helper(1); }\nint use_two() { return helper(2) + helper(3); }\n";

fn clangd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ASTROLABE_TEST_CLANGD") {
        return Some(PathBuf::from(p));
    }
    let out = std::process::Command::new("which")
        .arg("clangd")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn spec(command: PathBuf) -> ServerSpec {
    ServerSpec {
        language: Language::Cpp,
        command,
        args: Vec::new(),
        install_hint: String::new(),
    }
}

/// `include/b.h`, `src/b.cpp`, `src/a.cpp`, and a database whose
/// directories are relative (`"."`), so `#include "b.h"` only resolves
/// through its `-Iinclude`.
fn relative_db_project(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("astrolabe-s4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("include")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("include/b.h"), B_H).unwrap();
    std::fs::write(root.join("src/b.cpp"), B_CPP).unwrap();
    std::fs::write(root.join("src/a.cpp"), A_CPP).unwrap();
    let db = r#"[
 {"directory": ".", "file": "src/a.cpp", "arguments": ["c++", "-std=c++17", "-Iinclude", "-c", "src/a.cpp"]},
 {"directory": ".", "file": "src/b.cpp", "arguments": ["c++", "-std=c++17", "-Iinclude", "-c", "src/b.cpp"]}
]"#;
    std::fs::write(root.join("compile_commands.json"), db).unwrap();
    root.canonicalize().unwrap()
}

/// References to `helper` from its definition in `src/b.cpp`, polled until
/// the background index answers (or 20 s pass).
fn helper_refs(server: &dyn LanguageServer) -> Vec<String> {
    let at = Position {
        line: 1,
        character: 4,
    };
    let until = Instant::now() + Duration::from_secs(20);
    let mut last = Vec::new();
    while Instant::now() < until {
        if let Ok(locs) = server.references(&RelPath::new("src/b.cpp"), at) {
            last = locs
                .iter()
                .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
                .collect();
            last.sort();
            if last.iter().any(|p| p.starts_with("src/a.cpp")) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}

fn defs_from_a(server: &dyn LanguageServer) -> Vec<String> {
    let at = Position {
        line: 1,
        character: 23,
    };
    server
        .definition(&RelPath::new("src/a.cpp"), at)
        .map(|v| {
            v.iter()
                .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "needs clangd"]
fn relative_compile_db_before_and_after() {
    let Some(cmd) = clangd() else {
        eprintln!("skip: no clangd");
        return;
    };
    // Before: clangd reads the repo's database as is.
    let root = relative_db_project("before");
    let raw = LspTransport::connect(&spec(cmd.clone()), &root, Duration::from_secs(60)).unwrap();
    let before = LspSession::new(raw, root.clone(), Duration::from_secs(30));
    let before_defs = defs_from_a(&before);
    let before_refs = helper_refs(&before);
    before.shutdown().unwrap();
    // After: the production spawner (absolutized copy + --compile-commands-dir).
    let root2 = relative_db_project("after");
    let after = StdioSpawn::new(&root2).spawn(&spec(cmd), &root2).unwrap();
    let after_defs = defs_from_a(after.as_ref());
    let after_refs = helper_refs(after.as_ref());
    after.shutdown().unwrap();
    eprintln!("before: definition from a.cpp -> {before_defs:?}; references -> {before_refs:?}");
    eprintln!("after:  definition from a.cpp -> {after_defs:?}; references -> {after_refs:?}");
    assert!(
        after_refs
            .iter()
            .filter(|p| p.starts_with("src/a.cpp"))
            .count()
            >= 3
    );
    assert!(after_defs
        .iter()
        .any(|p| p.starts_with("include/b.h") || p.starts_with("src/b.cpp")));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}

fn child_pids_named(name: &str) -> Vec<i32> {
    let me = std::process::id().to_string();
    let out = std::process::Command::new("pgrep")
        .args(["-P", &me, name])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
#[ignore = "needs clangd"]
fn killed_server_is_restarted_and_request_resent() {
    let Some(cmd) = clangd() else {
        eprintln!("skip: no clangd");
        return;
    };
    let root = relative_db_project("crash");
    struct One(PathBuf);
    impl astrolabe_core::lsp::pool::DiscoverServers for One {
        fn discover(
            &self,
            language: Language,
        ) -> Result<ServerSpec, astrolabe_core::lsp::LspError> {
            let mut s = spec(self.0.clone());
            s.language = language;
            Ok(s)
        }
    }
    let config = LspPoolConfig {
        monitor_interval: Duration::ZERO,
        ..LspPoolConfig::default()
    };
    let pool = LspPool::from_discovery_and_transport(
        One(cmd),
        StdioSpawn::new(&root),
        root.clone(),
        config,
    );
    let guard = pool.acquire(Language::Cpp).unwrap();
    let warm = defs_from_a(&guard);
    let pids = child_pids_named("clangd");
    assert_eq!(pids.len(), 1, "{pids:?}");
    unsafe_kill(pids[0]);
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    let after = defs_from_a(&guard);
    let took = t.elapsed();
    let new_pids = child_pids_named("clangd");
    eprintln!(
        "warm: {warm:?}; killed pid {}; after kill: {after:?} in {took:?}; new pid {new_pids:?}",
        pids[0]
    );
    assert!(!after.is_empty(), "request re-sent to the restarted server");
    assert_eq!(new_pids.len(), 1);
    assert_ne!(new_pids[0], pids[0]);
    drop(guard);
    pool.shutdown();
    let _ = std::fs::remove_dir_all(Path::new(&root));
}

fn unsafe_kill(pid: i32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}
