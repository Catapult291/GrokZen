//! Real auto-install of the pinned clangd, off the probe path.
//!
//! Ignored by default (downloads ~50 MB). Run with
//! `ASTROLABE_TEST_INSTALL_CACHE=/scratch/servers cargo test --test live_install -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use astrolabe_core::lsp::discovery::{catalog_install_spec, Discovery};
use astrolabe_core::lsp::installer::{AutoInstallMode, INSTALL_PENDING};
use astrolabe_core::Language;

#[test]
#[ignore = "downloads clangd (ASTROLABE_TEST_INSTALL_CACHE)"]
fn probe_answers_at_once_while_clangd_downloads_then_finds_it() {
    let Some(cache) = std::env::var_os("ASTROLABE_TEST_INSTALL_CACHE") else {
        eprintln!("ASTROLABE_TEST_INSTALL_CACHE not set; skipping");
        return;
    };
    if catalog_install_spec(Language::Cpp).is_none() {
        return;
    }
    let d = Discovery::new("")
        .with_cache_root(&cache)
        .with_auto_install(AutoInstallMode::On);
    let t0 = Instant::now();
    let first = d.probe(Language::Cpp);
    let first_took = t0.elapsed();
    eprintln!("first probe {first_took:?}: {}", first.install_hint);
    assert!(first_took < Duration::from_secs(2), "{first_took:?}");
    assert!(first.spec.is_none() && first.install_hint.contains(INSTALL_PENDING));
    let mut probes = 1;
    let mut slowest = first_took;
    let spec = loop {
        std::thread::sleep(Duration::from_millis(500));
        let t = Instant::now();
        let p = d.probe(Language::Cpp);
        slowest = slowest.max(t.elapsed());
        probes += 1;
        if let Some(spec) = p.spec {
            break spec;
        }
        assert!(
            p.install_hint.contains(INSTALL_PENDING),
            "install failed: {}",
            p.install_hint
        );
        assert!(
            t0.elapsed() < Duration::from_secs(600),
            "install took too long"
        );
    };
    eprintln!(
        "installed after {:?} ({probes} probes, slowest probe {slowest:?}): {}",
        t0.elapsed(),
        spec.command.display()
    );
    assert!(spec.command.is_file());
}
