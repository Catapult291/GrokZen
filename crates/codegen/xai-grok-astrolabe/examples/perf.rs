//! Manual performance harness for Phase 4 (large-repository timings).
//!
//! ```text
//! cargo run -p xai-grok-astrolabe --release --example perf -- <root> [--persist repo]
//! ```
//!
//! Prints one `key=value` line per measurement. The default configuration
//! keeps the index in memory (`persist = none`), so the first run measures a
//! cold build; `--persist repo` lets a second run read `<root>/.astrolabe`.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use xai_grok_astrolabe::{
    AstrolabeConfig, DenyPolicy, IndexPersist, IndexRoot, PathMap, SearchCodeRequest, engine_for,
};

const QUERIES: &[&str] = &[
    "grep_timeout_secs",
    "ToolServerConfig",
    "apply_changeset",
    "search_code",
    "budget_tokens",
    "FinalizedToolset",
    "DenyReadGlobs",
    "find_references",
];

fn main() {
    let mut root = PathBuf::from(".");
    let mut persist = IndexPersist::None;
    let mut max_files = 20_000;
    let mut watch = false;
    let mut idle_seconds = 0u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--persist" => {
                persist = match args.next().as_deref() {
                    Some("repo") => IndexPersist::Repo,
                    Some(other) => panic!("unknown persist mode {other}; expected repo or none"),
                    None => panic!("--persist needs a value"),
                };
            }
            "--max-files" => {
                max_files = args
                    .next()
                    .expect("--max-files needs a value")
                    .parse()
                    .expect("--max-files takes an integer");
            }
            "--watch" => watch = true,
            "--idle-seconds" => {
                idle_seconds = args
                    .next()
                    .expect("--idle-seconds needs a value")
                    .parse()
                    .expect("--idle-seconds takes an integer");
            }
            other => root = PathBuf::from(other),
        }
    }
    let root = dunce::canonicalize(&root).unwrap_or(root);
    if !root.is_dir() {
        panic!("{} is not a directory", root.display());
    }

    let cfg = AstrolabeConfig {
        index_root: IndexRoot::Cwd,
        watch,
        persist,
        max_files,
        ready_timeout: Duration::from_secs(600),
        budget_tokens: 2_500,
        ..AstrolabeConfig::default()
    };
    let map = PathMap::new(root.clone(), root.clone(), None);
    let deny = DenyPolicy::default();

    let started = Instant::now();
    let engine = engine_for(&root, &cfg).expect("engine opens");
    let first = engine
        .search_code(
            &map,
            &deny,
            SearchCodeRequest {
                query: "grep_timeout_secs".to_string(),
                ..SearchCodeRequest::default()
            },
        )
        .expect("first query");
    println!("root={}", root.display());
    println!("persist={persist:?}");
    println!(
        "cold_open_and_first_query_ms={}",
        started.elapsed().as_millis()
    );
    println!("first_query_bytes={}", first.text.len());
    println!(
        "languages={}",
        engine
            .get_languages(&map, &deny, Default::default())
            .map(|out| out.text.lines().next().unwrap_or_default().to_string())
            .unwrap_or_default()
    );
    // Give the background build thread a moment to settle before sampling.
    std::thread::sleep(Duration::from_millis(200));
    if let Some(mb) = working_set_mb() {
        println!("rss_after_index_mb={mb}");
    }

    let mut hot = Vec::new();
    for index in 0..20 {
        let query = QUERIES[index % QUERIES.len()];
        let started = Instant::now();
        let out = engine
            .search_code(
                &map,
                &deny,
                SearchCodeRequest {
                    query: query.to_string(),
                    ..SearchCodeRequest::default()
                },
            )
            .expect("hot query");
        hot.push((query.to_string(), started.elapsed(), out.text.len()));
    }
    let mut times: Vec<u128> = hot.iter().map(|(_, d, _)| d.as_millis()).collect();
    times.sort_unstable();
    println!("hot_queries={}", times.len());
    println!("hot_p50_ms={}", percentile(&times, 0.50));
    println!("hot_p95_ms={}", percentile(&times, 0.95));
    println!("hot_max_ms={}", times.last().copied().unwrap_or(0));
    println!(
        "hot_mean_bytes={}",
        hot.iter().map(|(_, _, n)| n).sum::<usize>() / hot.len().max(1)
    );

    let started = Instant::now();
    let wide = engine
        .search_code(
            &map,
            &deny,
            SearchCodeRequest {
                query: "timeout".to_string(),
                aggregate: Some("dir".to_string()),
                ..SearchCodeRequest::default()
            },
        )
        .expect("wide query");
    println!("wide_dir_query_ms={}", started.elapsed().as_millis());
    println!("wide_dir_query_bytes={}", wide.text.len());

    let started = Instant::now();
    let symbols = engine
        .find_symbol(
            &map,
            &deny,
            xai_grok_astrolabe::FindSymbolRequest {
                query: "search_code".to_string(),
                ..Default::default()
            },
        )
        .expect("find_symbol");
    println!("find_symbol_ms={}", started.elapsed().as_millis());
    println!("find_symbol_bytes={}", symbols.text.len());

    if let Some(mb) = working_set_mb() {
        println!("rss_after_wide_mb={mb}");
    }
    if idle_seconds > 0 {
        let start = cpu_seconds();
        std::thread::sleep(Duration::from_secs(idle_seconds));
        if let (Some(before), Some(after)) = (start, cpu_seconds()) {
            let used = (after - before).max(0.0);
            println!("idle_cpu_pct={:.2}", used / idle_seconds as f64 * 100.0);
        }
    }
    if persist == IndexPersist::Repo {
        let store = root.join(".astrolabe/index.redb");
        if let Ok(meta) = std::fs::metadata(&store) {
            println!("store_bytes={}", meta.len());
        }
    }
    engine.shutdown();
}

fn percentile(sorted: &[u128], at: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * at).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(windows)]
fn cpu_seconds() -> Option<f64> {
    let pid = std::process::id();
    run_pwsh(&format!("(Get-Process -Id {pid}).CPU"))
        .and_then(|text| text.trim().parse::<f64>().ok())
}

#[cfg(not(windows))]
fn cpu_seconds() -> Option<f64> {
    None
}

#[cfg(windows)]
fn working_set_mb() -> Option<u64> {
    let pid = std::process::id();
    run_pwsh(&format!("(Get-Process -Id {pid}).WorkingSet64"))
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
}

#[cfg(windows)]
fn run_pwsh(script: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(windows))]
fn working_set_mb() -> Option<u64> {
    None
}
