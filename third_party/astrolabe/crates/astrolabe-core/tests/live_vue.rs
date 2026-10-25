//! End-to-end checks of the Vue bridge against a real Volar install.
//!
//! Ignored by default. Run with
//! `ASTROLABE_TEST_VUE_NM=/path/to/node_modules cargo test --test live_vue -- --ignored --nocapture`
//! where that `node_modules` holds `@vue/language-server` 3.x,
//! `@vue/typescript-plugin`, `typescript` and `typescript-language-server`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrolabe_core::lsp::pool::SpawnServer;
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::{Position, ServerSpec};
use astrolabe_core::{Language, RelPath};

fn node_modules() -> Option<PathBuf> {
    let nm = PathBuf::from(std::env::var_os("ASTROLABE_TEST_VUE_NM")?);
    nm.join("@vue/language-server/package.json")
        .is_file()
        .then_some(nm)
}

fn fixture(nm: &Path, tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("astro-live-vue-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(nm, root.join("node_modules")).unwrap();
    std::fs::write(root.join("package.json"), r#"{"name":"fx","private":true}"#).unwrap();
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"module":"esnext","moduleResolution":"bundler","target":"es2020","jsx":"preserve"},"include":["src/**/*.ts","src/**/*.vue"]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/util.ts"),
        "export function greet(name: string): string {\n  return `hi ${name}`;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/Comp.vue"),
        "<script setup lang=\"ts\">\nimport { greet } from './util'\nconst who = 'vue'\n</script>\n\n<template>\n  <p>{{ greet(who) }}</p>\n  <p>{{ missingThing }}</p>\n  <div></span>\n</template>\n",
    )
    .unwrap();
    root
}

fn spec(command: PathBuf) -> ServerSpec {
    ServerSpec {
        language: Language::Vue,
        command,
        args: vec!["--stdio".into()],
        install_hint: String::new(),
    }
}

fn run(tag: &str, command: impl FnOnce(&Path, &Path) -> PathBuf) {
    let Some(nm) = node_modules() else {
        eprintln!("ASTROLABE_TEST_VUE_NM not set; skipping");
        return;
    };
    let root = fixture(&nm, tag);
    let cmd = command(&nm, &root);
    let spawner = StdioSpawn::new(&root);
    let t0 = Instant::now();
    let server = spawner.spawn(&spec(cmd), &root).expect("bridge starts");
    let started = t0.elapsed();
    let ready = server.wait_until_ready(Duration::from_secs(60));
    let ready_at = t0.elapsed();
    let util = RelPath::new("src/util.ts");
    let comp = RelPath::new("src/Comp.vue");
    // `greet` in util.ts, line 0 col 16.
    let refs = server
        .references(
            &comp,
            Position {
                line: 1,
                character: 10,
            },
        )
        .unwrap_or_default();
    let refs_util = server
        .references(
            &util,
            Position {
                line: 0,
                character: 17,
            },
        )
        .unwrap_or_default();
    let diags = server.diagnostics(&comp).unwrap_or_default();
    eprintln!(
        "[{tag}] spawn {:?}, ready={ready} at {:?}, refs(from vue)={} {:?}, refs(from ts)={} {:?}, diags={} {:?}",
        started,
        ready_at,
        refs.len(),
        refs.iter().map(|l| (l.path.as_str().to_string(), l.range.start.line)).collect::<Vec<_>>(),
        refs_util.len(),
        refs_util.iter().map(|l| (l.path.as_str().to_string(), l.range.start.line)).collect::<Vec<_>>(),
        diags.len(),
        diags.iter().map(|d| (d.range.start.line, d.message.clone())).collect::<Vec<_>>(),
    );
    let t1 = Instant::now();
    let _ = server.shutdown();
    eprintln!("[{tag}] shutdown {:?}", t1.elapsed());
    assert!(
        refs.iter()
            .any(|l| l.path == comp && l.range.start.line == 6),
        "template use of greet missing from references: {refs:?}"
    );
    // `<div></span>`: a template compiler error only the Vue server reports.
    assert!(
        diags.iter().any(|d| d.range.start.line == 8),
        "template diagnostic missing: {diags:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs a Volar 3.x node_modules (ASTROLABE_TEST_VUE_NM)"]
fn bridge_known_version() {
    run("known", |nm, _| {
        nm.join("@vue/language-server/bin/vue-language-server.js")
    });
}

#[test]
#[ignore = "needs a Volar 3.x node_modules (ASTROLABE_TEST_VUE_NM)"]
fn bridge_unknown_version_wrapper() {
    // A wrapper script hides the package: the major version is only learnt
    // from `initialize`.
    run("wrapper", |nm, _root| {
        // Outside the fixture: its `node_modules` would reveal the package.
        let bin = std::env::temp_dir().join(format!("astro-live-vue-wrap-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        let w = bin.join("vue-ls-wrapper");
        std::fs::write(
            &w,
            format!(
                "#!/bin/sh\nexec node {} \"$@\"\n",
                nm.join("@vue/language-server/bin/vue-language-server.js")
                    .display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&w, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        w
    });
}
