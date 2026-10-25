//! End-to-end checks against a real language server.
//!
//! Ignored by default: they need `rust-analyzer` on `PATH` and take tens of
//! seconds. Run with `cargo test -p astrolabe-core --test live_lsp -- --ignored`.
//!
//! These exist because every bug this layer has had so far was invisible to
//! the fakes: a cold index answering `[]`, progress tokens going transiently
//! quiet mid-startup, a workspace root that never reached the spawner. None
//! of those reproduce without a real server.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrolabe_core::lsp::pool::{LspPool, LspPoolConfig};
use astrolabe_core::lsp::queries::{find_references, SymbolAt};
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::{LanguageServer, ServerSpec};
use astrolabe_core::types::{Confidence, Language, RelPath};

const LIB_RS: &str = r#"pub fn target_symbol(x: i32) -> i32 {
    x + 1
}

pub fn caller_one() -> i32 {
    target_symbol(1)
}

pub fn caller_two() -> i32 {
    target_symbol(2) + target_symbol(3)
}
"#;

/// Definition plus three call sites.
const EXPECTED_REFERENCES: usize = 4;

fn tiny_crate() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "astrolabe-live-lsp-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("create crate dir");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    std::fs::write(root.join("src/lib.rs"), LIB_RS).expect("write source");
    root
}

fn rust_analyzer() -> Option<ServerSpec> {
    let found = std::process::Command::new("which")
        .arg("rust-analyzer")
        .output()
        .ok()?;
    if !found.status.success() {
        return None;
    }
    let path = String::from_utf8(found.stdout).ok()?.trim().to_owned();
    Some(ServerSpec {
        language: Language::Rust,
        command: PathBuf::from(path),
        args: Vec::new(),
        install_hint: "rustup component add rust-analyzer".into(),
    })
}

fn pool_for(root: &Path, spec: ServerSpec) -> LspPool {
    struct Fixed(ServerSpec);
    impl astrolabe_core::lsp::pool::DiscoverServers for Fixed {
        fn discover(
            &self,
            _language: Language,
        ) -> Result<ServerSpec, astrolabe_core::lsp::LspError> {
            Ok(self.0.clone())
        }
    }
    LspPool::from_discovery_and_transport(
        Fixed(spec),
        StdioSpawn::new(root).with_timeout(Duration::from_secs(60)),
        root,
        LspPoolConfig::default(),
    )
}

#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn acquire_returns_a_server_that_has_finished_indexing() {
    let Some(spec) = rust_analyzer() else {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    };
    let root = tiny_crate();
    let pool = pool_for(&root, spec);

    let started = Instant::now();
    let server = pool.acquire(Language::Rust).expect("server starts");
    let waited = started.elapsed();

    // The contract `acquire` owes its callers: by the time it returns, the
    // server's answers mean something. Returning a cold server pushes the
    // readiness problem onto every call site, and the first one to forget it
    // reports "no references" for a symbol that has plenty.
    assert!(
        !server.is_busy(),
        "acquire returned while the server was still indexing (waited {waited:?})"
    );
}

#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn references_are_exact_and_complete_against_a_real_server() {
    let Some(spec) = rust_analyzer() else {
        eprintln!("skipping: rust-analyzer not on PATH");
        return;
    };
    let root = tiny_crate();
    let pool = pool_for(&root, spec);
    let server = pool.acquire(Language::Rust).expect("server starts");

    let result = find_references(
        Some(&server as &dyn LanguageServer),
        &root,
        &RelPath::new("src/lib.rs"),
        SymbolAt::Name("target_symbol"),
    );

    assert_eq!(
        result.confidence,
        Confidence::Exact,
        "a ready server's answer must be exact; note was {:?}",
        result.note
    );
    assert_eq!(
        result.value.len(),
        EXPECTED_REFERENCES,
        "expected the definition plus three call sites, got {:#?}",
        result.value
    );
}
