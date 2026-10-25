//! Single-file incremental apply latency (what a query waits for after an
//! edit), measured in-process without the MCP layer.
//!
//! ```text
//! cargo run --release --example incr_bench -- <repo> <rel-path> [rounds]
//! ```
//!
//! Appends a function to `<rel-path>`, applies a one-file `modified`
//! changeset, restores the file and applies again; prints min / median / max
//! of both applies. The file is restored on exit.

use std::path::PathBuf;
use std::time::Instant;

use astrolabe_core::index::{IncrementalIndex, IndexOptions};
use astrolabe_core::watch::ChangeSet;
use astrolabe_core::RelPath;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(repo), Some(rel)) = (args.next(), args.next()) else {
        eprintln!("usage: incr_bench <repo> <rel-path> [rounds]");
        std::process::exit(2);
    };
    let rounds: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(10);
    if std::env::var_os("INCR_BENCH_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    }
    let root = PathBuf::from(repo);
    let file = root.join(&rel);
    let original = std::fs::read(&file).expect("read target file");
    let opts = IndexOptions {
        call_edges: true,
        ..IndexOptions::default()
    };
    let t = Instant::now();
    let mut index = IncrementalIndex::build(&root, &opts);
    eprintln!(
        "build: {:.0} ms, files={} symbols={} edges={}",
        t.elapsed().as_secs_f64() * 1e3,
        index.graph.files.len(),
        index.graph.symbols.len(),
        index.graph.edges.len()
    );
    let changes = ChangeSet {
        added: vec![],
        modified: vec![RelPath::new(&rel)],
        removed: vec![],
    };
    let mut add = Vec::new();
    let mut restore = Vec::new();
    for k in 0..rounds {
        let mut edited = original.clone();
        edited.extend_from_slice(format!("\nfunc IncrBenchProbe{k}() {{}}\n").as_bytes());
        std::fs::write(&file, &edited).unwrap();
        let t = Instant::now();
        let next = index.apply_changeset(&root, &changes, &opts);
        add.push(t.elapsed().as_secs_f64() * 1e3);
        drop(std::mem::replace(&mut index, next));
        std::fs::write(&file, &original).unwrap();
        let t = Instant::now();
        let next = index.apply_changeset(&root, &changes, &opts);
        restore.push(t.elapsed().as_secs_f64() * 1e3);
        drop(std::mem::replace(&mut index, next));
    }
    std::fs::write(&file, &original).unwrap();
    for (label, mut v) in [("add", add), ("restore", restore)] {
        v.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{label}: min {:.1} ms  median {:.1} ms  max {:.1} ms  (n={})",
            v[0],
            v[v.len() / 2],
            v[v.len() - 1],
            v.len()
        );
    }
}
