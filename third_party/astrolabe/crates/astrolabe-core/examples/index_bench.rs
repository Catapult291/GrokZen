//! Index-cost regression bench: warmup + repeated runs, median, peak RSS.
//!
//! Release only (`cargo run --release --example index_bench`). Peak RSS is
//! taken from the kernel high-water mark of an isolated worker process so
//! successive runs do not contaminate `ru_maxrss`.
//!
//! ```text
//! cargo run --release --example index_bench -- [OPTIONS] <repo>
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use astrolabe_core::index::{index_repo, IndexOptions};
use astrolabe_core::EdgeKind;
use serde_json::{json, Value};

fn main() {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        println!("{}", help());
        process::exit(0);
    }
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            process::exit(2);
        }
    };

    if args.once {
        match run_once(&args.repo, args.call_edges) {
            Ok(sample) => emit_sample(&args, &sample),
            Err(e) => {
                eprintln!("index_bench: {e}");
                process::exit(1);
            }
        }
        return;
    }

    match run_aggregate(&args) {
        Ok(report) => {
            // Save before printing: a piped-away stdout must not lose the baseline.
            if let Some(path) = &args.save {
                if let Err(e) = fs::write(path, serde_json::to_string_pretty(&report.json).unwrap())
                {
                    eprintln!("index_bench: failed to write {}: {e}", path.display());
                    process::exit(1);
                }
            }
            emit_aggregate(&args, &report);
            if args.ci && report.verdict == Verdict::Regress {
                process::exit(1);
            }
            if args.ci && report.verdict == Verdict::StaleBaseline {
                process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("index_bench: {e}");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Human,
    Json,
    Kv,
}

struct Args {
    repo: PathBuf,
    name: String,
    warmup: usize,
    runs: usize,
    call_edges: bool,
    format: Format,
    baseline: Option<PathBuf>,
    save: Option<PathBuf>,
    ci: bool,
    once: bool,
    in_process: bool,
    rss_ratio: f64,
    time_ratio: f64,
    rss_floor_bytes: u64,
    time_floor_ms: f64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        if raw.iter().any(|a| a == "-h" || a == "--help") {
            return Err(help());
        }

        let mut repo = None;
        let mut name = None;
        let mut warmup = 2usize;
        let mut runs = 5usize;
        let mut call_edges = false;
        let mut format = Format::Human;
        let mut baseline = None;
        let mut save = None;
        let mut ci = false;
        let mut once = false;
        let mut in_process = false;
        let mut rss_ratio = 1.20;
        let mut time_ratio = 2.50;
        let mut rss_floor_bytes = 4 * 1024 * 1024;
        let mut time_floor_ms = 150.0;

        let mut i = 0;
        while i < raw.len() {
            let a = raw[i].as_str();
            match a {
                "--warmup" => {
                    warmup = parse_next(&raw, &mut i, "--warmup")?;
                }
                "--runs" => {
                    runs = parse_next(&raw, &mut i, "--runs")?;
                }
                "--calls" => call_edges = true,
                "--format" => {
                    let v: String = parse_next(&raw, &mut i, "--format")?;
                    format = match v.as_str() {
                        "human" => Format::Human,
                        "json" => Format::Json,
                        "kv" => Format::Kv,
                        _ => return Err(format!("unknown --format {v} (human|json|kv)")),
                    };
                }
                "--baseline" => {
                    let v: String = parse_next(&raw, &mut i, "--baseline")?;
                    baseline = Some(PathBuf::from(v));
                }
                "--save" => {
                    let v: String = parse_next(&raw, &mut i, "--save")?;
                    save = Some(PathBuf::from(v));
                }
                "--name" => {
                    let v: String = parse_next(&raw, &mut i, "--name")?;
                    name = Some(v);
                }
                "--ci" => ci = true,
                "--once" => once = true,
                "--in-process" => in_process = true,
                "--rss-ratio" => {
                    rss_ratio = parse_next(&raw, &mut i, "--rss-ratio")?;
                }
                "--time-ratio" => {
                    time_ratio = parse_next(&raw, &mut i, "--time-ratio")?;
                }
                "--rss-floor-bytes" => {
                    rss_floor_bytes = parse_next(&raw, &mut i, "--rss-floor-bytes")?;
                }
                "--time-floor-ms" => {
                    time_floor_ms = parse_next(&raw, &mut i, "--time-floor-ms")?;
                }
                "--" => {
                    i += 1;
                    if i < raw.len() {
                        repo = Some(PathBuf::from(&raw[i]));
                    }
                    break;
                }
                s if s.starts_with('-') => {
                    return Err(format!("unknown flag {s}\n{}", help()));
                }
                _ => {
                    if repo.is_some() {
                        return Err(format!("unexpected argument {a}"));
                    }
                    repo = Some(PathBuf::from(a));
                }
            }
            i += 1;
        }

        let repo = repo.ok_or_else(help)?;
        if warmup == 0 && !once {
            // A zero warmup is allowed, but document it.
        }
        if runs == 0 && !once {
            return Err("--runs must be >= 1".into());
        }
        let name = name.unwrap_or_else(|| infer_name(&repo));
        Ok(Self {
            repo,
            name,
            warmup,
            runs,
            call_edges,
            format,
            baseline,
            save,
            ci,
            once,
            in_process,
            rss_ratio,
            time_ratio,
            rss_floor_bytes,
            time_floor_ms,
        })
    }
}

fn parse_next<T: std::str::FromStr>(raw: &[String], i: &mut usize, flag: &str) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    *i += 1;
    let v = raw.get(*i).ok_or_else(|| format!("{flag} needs a value"))?;
    v.parse().map_err(|e| format!("invalid {flag} '{v}': {e}"))
}

fn help() -> String {
    "usage: index_bench [OPTIONS] <repo>\n\
     \n\
     Options:\n\
       --warmup N            discarded runs before measuring (default 2)\n\
       --runs N              measured runs; median is the headline (default 5)\n\
       --calls               enable syntactic call edges\n\
       --format human|json|kv\n\
       --name NAME           corpus label (default: last path component)\n\
       --baseline FILE       previous aggregate JSON to compare against\n\
       --save FILE           write aggregate JSON\n\
       --ci                  exit 1 on regression / stale baseline\n\
       --rss-ratio F         fail if peak RSS > max(base*F, base+floor) (default 1.20)\n\
       --time-ratio F        same for elapsed time (default 2.50)\n\
       --rss-floor-bytes N   absolute slack for RSS (default 4 MiB)\n\
       --time-floor-ms N     absolute slack for time (default 150)\n\
       --in-process          do not spawn workers (RSS HWM is then process-lifetime)\n\
       --once                single sample (used by the spawn harness)\n\
     \n\
     Peak RSS uses /proc/self/status VmHWM on Linux and mach_task_basic_info\n\
     resident_size_max (fallback: getrusage ru_maxrss, bytes) on macOS.\n\
     Workers are separate processes so the kernel HWM is per-run, not cumulative."
        .into()
}

fn infer_name(repo: &Path) -> String {
    repo.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string()
}

// ---------------------------------------------------------------------------
// Sample + aggregate
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Sample {
    elapsed_us: u64,
    peak_rss_bytes: u64,
    held_rss_bytes: u64,
    sampled_peak_rss_bytes: u64,
    files_scanned: usize,
    files_parsed: usize,
    symbols: usize,
    import_edges: usize,
    call_edges: usize,
    parse_failures: usize,
    unresolved_imports: usize,
    rss_method: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Regress,
    StaleBaseline,
    NoBaseline,
}

struct Aggregate {
    json: Value,
    verdict: Verdict,
}

fn run_once(root: &Path, call_edges: bool) -> Result<Sample, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }

    let rss_method = rss::method();
    let flag = Arc::new(AtomicBool::new(true));
    let sampled = Arc::new(AtomicU64::new(rss::current_bytes().unwrap_or(0)));
    let sampler = {
        let flag = flag.clone();
        let sampled = sampled.clone();
        thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                if let Some(v) = rss::current_bytes() {
                    sampled.fetch_max(v, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let t = Instant::now();
    let (graph, report) = index_repo(
        root,
        &IndexOptions {
            call_edges,
            ..IndexOptions::default()
        },
    );
    let elapsed = t.elapsed();
    std::hint::black_box(&graph);

    let held = rss::current_bytes().unwrap_or(0);
    sampled.fetch_max(held, Ordering::Relaxed);
    let kernel_peak = rss::peak_bytes().unwrap_or(0);
    flag.store(false, Ordering::Relaxed);
    let _ = sampler.join();
    let sampled_peak = sampled.load(Ordering::Relaxed);
    let peak = kernel_peak.max(sampled_peak).max(held);

    let imports = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import)
        .count();
    let calls = graph.edges.len().saturating_sub(imports);

    if std::env::var_os("ASTROLABE_HOLD").is_some() {
        thread::sleep(Duration::from_secs(6));
    }

    std::hint::black_box(&graph);
    Ok(Sample {
        elapsed_us: elapsed.as_micros() as u64,
        peak_rss_bytes: peak,
        held_rss_bytes: held,
        sampled_peak_rss_bytes: sampled_peak,
        files_scanned: report.files_scanned,
        files_parsed: report.files_parsed,
        symbols: graph.symbols.len(),
        import_edges: imports,
        call_edges: calls,
        parse_failures: report.parse_failures.len(),
        unresolved_imports: report.unresolved_imports.len(),
        rss_method,
    })
}

fn run_aggregate(args: &Args) -> Result<Aggregate, String> {
    if !args.repo.is_dir() {
        return Err(format!("not a directory: {}", args.repo.display()));
    }

    let mut samples: Vec<Sample> = Vec::with_capacity(args.runs);
    let total = args.warmup + args.runs;
    for i in 0..total {
        let phase = if i < args.warmup { "warmup" } else { "run" };
        let n = if i < args.warmup {
            i + 1
        } else {
            i - args.warmup + 1
        };
        let of = if i < args.warmup {
            args.warmup
        } else {
            args.runs
        };
        eprint!("index_bench: {phase} {n}/{of} {} ... ", args.repo.display());
        let sample = if args.in_process {
            run_once(&args.repo, args.call_edges)?
        } else {
            spawn_worker(args)?
        };
        eprintln!(
            "{:.0} ms  peak {}  held {}",
            sample.elapsed_us as f64 / 1000.0,
            fmt_bytes(sample.peak_rss_bytes),
            fmt_bytes(sample.held_rss_bytes)
        );
        if i >= args.warmup {
            samples.push(sample);
        }
    }
    if samples.is_empty() {
        return Err("no measured samples".into());
    }

    build_report(args, &samples)
}

fn spawn_worker(args: &Args) -> Result<Sample, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("--once").arg("--format").arg("json");
    if args.call_edges {
        cmd.arg("--calls");
    }
    cmd.arg(&args.repo);
    let out = cmd.output().map_err(|e| format!("spawn worker: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "worker exited {}: {err}",
            out.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // A worker may have printed a human line if format parsing failed; take
    // the last JSON object.
    let json_line = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| format!("worker produced no JSON: {text}"))?;
    sample_from_json(&serde_json::from_str::<Value>(json_line).map_err(|e| e.to_string())?)
}

fn sample_from_json(v: &Value) -> Result<Sample, String> {
    let get_u64 = |k: &str| -> Result<u64, String> {
        v.get(k)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| format!("missing {k}"))
    };
    let get_usize = |k: &str| -> Result<usize, String> { Ok(get_u64(k)? as usize) };
    let elapsed_us = v
        .get("elapsed_us")
        .and_then(|x| x.as_u64())
        .or_else(|| {
            v.get("elapsed_ms")
                .and_then(|x| x.as_f64())
                .map(|ms| (ms * 1000.0) as u64)
        })
        .ok_or_else(|| "missing elapsed_us".to_string())?;
    Ok(Sample {
        elapsed_us,
        peak_rss_bytes: get_u64("peak_rss_bytes")?,
        held_rss_bytes: v
            .get("held_rss_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        sampled_peak_rss_bytes: v
            .get("sampled_peak_rss_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        files_scanned: get_usize("files_scanned")?,
        files_parsed: get_usize("files_parsed")?,
        symbols: get_usize("symbols")?,
        import_edges: get_usize("import_edges")?,
        call_edges: get_usize("call_edges")?,
        parse_failures: get_usize("parse_failures")?,
        unresolved_imports: get_usize("unresolved_imports")?,
        rss_method: v
            .get("rss_method")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string(),
    })
}

fn build_report(args: &Args, samples: &[Sample]) -> Result<Aggregate, String> {
    let mut elapsed: Vec<u64> = samples.iter().map(|s| s.elapsed_us).collect();
    let mut peak: Vec<u64> = samples.iter().map(|s| s.peak_rss_bytes).collect();
    let mut held: Vec<u64> = samples.iter().map(|s| s.held_rss_bytes).collect();
    let med_elapsed = median_u64(&mut elapsed);
    let med_peak = median_u64(&mut peak);
    let med_held = median_u64(&mut held);
    let min_elapsed = *elapsed.iter().min().unwrap();
    let max_elapsed = *elapsed.iter().max().unwrap();
    let min_peak = *peak.iter().min().unwrap();
    let max_peak = *peak.iter().max().unwrap();

    let first = &samples[0];
    for s in samples {
        if s.files_scanned != first.files_scanned || s.symbols != first.symbols {
            eprintln!(
                "index_bench: warning: counts are not stable across runs (files {} vs {}, symbols {} vs {})",
                s.files_scanned, first.files_scanned, s.symbols, first.symbols
            );
        }
    }

    let jitter_time_pct = pct_spread(min_elapsed, max_elapsed, med_elapsed);
    let jitter_rss_pct = pct_spread(min_peak, max_peak, med_peak);

    let host = host_info();
    let mut median = json!({
        "elapsed_us": med_elapsed,
        "elapsed_ms": med_elapsed as f64 / 1000.0,
        "peak_rss_bytes": med_peak,
        "held_rss_bytes": med_held,
        "files_scanned": first.files_scanned,
        "files_parsed": first.files_parsed,
        "symbols": first.symbols,
        "import_edges": first.import_edges,
        "call_edges": first.call_edges,
        "parse_failures": first.parse_failures,
        "unresolved_imports": first.unresolved_imports,
    });

    let mut regressions = Vec::new();
    let mut verdict = Verdict::NoBaseline;
    if let Some(path) = &args.baseline {
        if path.is_file() {
            let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let base: Value = serde_json::from_str(&text)
                .map_err(|e| format!("baseline {}: {e}", path.display()))?;
            let bmed = base.get("median").cloned().unwrap_or(base.clone());
            let b_files = bmed.get("files_scanned").and_then(|x| x.as_u64());
            if let Some(bf) = b_files {
                if bf as usize != first.files_scanned {
                    regressions.push(format!(
                        "files_scanned changed: baseline {bf} vs now {}",
                        first.files_scanned
                    ));
                    verdict = Verdict::StaleBaseline;
                }
            }
            compare_higher(
                "peak_rss_bytes",
                med_peak,
                bmed.get("peak_rss_bytes").and_then(|x| x.as_u64()),
                args.rss_ratio,
                args.rss_floor_bytes,
                &mut regressions,
            );
            let base_elapsed = bmed.get("elapsed_us").and_then(|x| x.as_u64()).or_else(|| {
                bmed.get("elapsed_ms")
                    .and_then(|x| x.as_f64())
                    .map(|ms| (ms * 1000.0) as u64)
            });
            compare_higher(
                "elapsed_us",
                med_elapsed,
                base_elapsed,
                args.time_ratio,
                (args.time_floor_ms * 1000.0) as u64,
                &mut regressions,
            );
            if let Some(bp) = bmed.get("parse_failures").and_then(|x| x.as_u64()) {
                if (first.parse_failures as u64) > bp {
                    regressions.push(format!(
                        "parse_failures rose: {bp} -> {}",
                        first.parse_failures
                    ));
                }
            }
            if let Some(bs) = bmed.get("symbols").and_then(|x| x.as_u64()) {
                if (first.symbols as u64) < (bs * 99) / 100 {
                    regressions.push(format!("symbols dropped >1%: {bs} -> {}", first.symbols));
                }
            }
            if verdict != Verdict::StaleBaseline {
                verdict = if regressions.is_empty() {
                    Verdict::Pass
                } else {
                    Verdict::Regress
                };
            }
            median.as_object_mut().unwrap().insert(
                "baseline_peak_rss_bytes".into(),
                json!(bmed.get("peak_rss_bytes").and_then(|x| x.as_u64())),
            );
            median
                .as_object_mut()
                .unwrap()
                .insert("baseline_elapsed_us".into(), json!(base_elapsed));
        } else if args.ci {
            return Err(format!(
                "--ci requires an existing --baseline (missing {})",
                path.display()
            ));
        }
    }

    let json = json!({
        "schema": "astrolabe-bench/v1",
        "mode": "aggregate",
        "name": args.name,
        "repo": args.repo.display().to_string(),
        "git_head": astrolabe_git(),
        "git_dirty": astrolabe_dirty(),
        "measured_unix": now_unix(),
        "host": host,
        "warmup": args.warmup,
        "runs": args.runs,
        "spawn": !args.in_process,
        "call_edges": args.call_edges,
        "rss_method": first.rss_method,
        "thresholds": {
            "rss_ratio": args.rss_ratio,
            "time_ratio": args.time_ratio,
            "rss_floor_bytes": args.rss_floor_bytes,
            "time_floor_ms": args.time_floor_ms,
        },
        "median": median,
        "min": {
            "elapsed_us": min_elapsed,
            "elapsed_ms": min_elapsed as f64 / 1000.0,
            "peak_rss_bytes": min_peak,
        },
        "max": {
            "elapsed_us": max_elapsed,
            "elapsed_ms": max_elapsed as f64 / 1000.0,
            "peak_rss_bytes": max_peak,
        },
        "jitter": {
            "elapsed_ms_pct": jitter_time_pct,
            "peak_rss_pct": jitter_rss_pct,
        },
        "samples": samples.iter().map(sample_json).collect::<Vec<_>>(),
        "verdict": verdict_str(verdict),
        "regressions": regressions,
    });

    Ok(Aggregate { json, verdict })
}

fn compare_higher(
    label: &str,
    current: u64,
    baseline: Option<u64>,
    ratio: f64,
    floor: u64,
    regressions: &mut Vec<String>,
) {
    let Some(base) = baseline else { return };
    if base == 0 {
        return;
    }
    let by_ratio = (base as f64 * ratio).ceil() as u64;
    let by_floor = base.saturating_add(floor);
    let limit = by_ratio.max(by_floor);
    if current > limit {
        regressions.push(format!(
            "{label} {current} > limit {limit} (baseline {base}, ratio {ratio}, floor {floor})"
        ));
    }
}

fn sample_json(s: &Sample) -> Value {
    json!({
        "elapsed_us": s.elapsed_us,
        "elapsed_ms": s.elapsed_us as f64 / 1000.0,
        "peak_rss_bytes": s.peak_rss_bytes,
        "held_rss_bytes": s.held_rss_bytes,
        "sampled_peak_rss_bytes": s.sampled_peak_rss_bytes,
        "files_scanned": s.files_scanned,
        "files_parsed": s.files_parsed,
        "symbols": s.symbols,
        "import_edges": s.import_edges,
        "call_edges": s.call_edges,
        "parse_failures": s.parse_failures,
        "unresolved_imports": s.unresolved_imports,
        "rss_method": s.rss_method,
    })
}

fn emit_sample(args: &Args, s: &Sample) {
    match args.format {
        Format::Json => println!("{}", sample_json(s)),
        Format::Kv => print_kv_sample(&args.name, s),
        Format::Human => {
            print_human_sample(&args.repo, s);
            print_kv_sample(&args.name, s);
        }
    }
}

fn emit_aggregate(args: &Args, report: &Aggregate) {
    match args.format {
        Format::Json => {
            writeln_stdout(&serde_json::to_string_pretty(&report.json).unwrap());
        }
        Format::Kv => print_kv_aggregate(args, report),
        Format::Human => {
            print_human_aggregate(args, report);
            print_kv_aggregate(args, report);
        }
    }
}

fn writeln_stdout(s: &str) {
    use std::io::{self, Write};
    let mut out = io::stdout();
    if let Err(e) = writeln!(out, "{s}") {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("index_bench: stdout: {e}");
        }
    }
}

fn print_human_sample(repo: &Path, s: &Sample) {
    println!(
        "{:<44} {:>6} 扫描 {:>6} 解析 {:>7} 符号 {:>6} 导入边 {:>7} 调用边 {:>4} 失败 {:>4} 未解析  {:>7.0}ms  peak {}  held {}",
        repo.display(),
        s.files_scanned,
        s.files_parsed,
        s.symbols,
        s.import_edges,
        s.call_edges,
        s.parse_failures,
        s.unresolved_imports,
        s.elapsed_us as f64 / 1000.0,
        fmt_bytes(s.peak_rss_bytes),
        fmt_bytes(s.held_rss_bytes),
    );
}

fn print_human_aggregate(args: &Args, report: &Aggregate) {
    let j = &report.json;
    let med = &j["median"];
    let min = &j["min"];
    let max = &j["max"];
    println!(
        "{:<16} {:>5} 扫描 {:>7} 符号 {:>4} 失败  {:>7.0} ms ({}–{})  peak {} ({}–{})  held {}  {}",
        args.name,
        med["files_scanned"].as_u64().unwrap_or(0),
        med["symbols"].as_u64().unwrap_or(0),
        med["parse_failures"].as_u64().unwrap_or(0),
        med["elapsed_ms"].as_f64().unwrap_or(0.0),
        fmt_ms(min["elapsed_ms"].as_f64().unwrap_or(0.0)),
        fmt_ms(max["elapsed_ms"].as_f64().unwrap_or(0.0)),
        fmt_bytes(med["peak_rss_bytes"].as_u64().unwrap_or(0)),
        fmt_bytes(min["peak_rss_bytes"].as_u64().unwrap_or(0)),
        fmt_bytes(max["peak_rss_bytes"].as_u64().unwrap_or(0)),
        fmt_bytes(med["held_rss_bytes"].as_u64().unwrap_or(0)),
        verdict_str(report.verdict),
    );
    if let Some(regs) = j["regressions"].as_array() {
        for r in regs {
            if let Some(s) = r.as_str() {
                eprintln!("  REGRESS: {s}");
            }
        }
    }
}

fn print_kv_sample(name: &str, s: &Sample) {
    println!(
        "ASTROLABE_BENCH|{name}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|sample",
        s.files_scanned,
        s.files_parsed,
        s.symbols,
        s.import_edges,
        s.parse_failures,
        s.unresolved_imports,
        s.elapsed_us,
        s.peak_rss_bytes,
        s.held_rss_bytes,
        s.sampled_peak_rss_bytes,
    );
}

fn print_kv_aggregate(args: &Args, report: &Aggregate) {
    let med = &report.json["median"];
    let min = &report.json["min"];
    let max = &report.json["max"];
    println!(
        "ASTROLABE_BENCH|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        args.name,
        med["files_scanned"].as_u64().unwrap_or(0),
        med["symbols"].as_u64().unwrap_or(0),
        med["parse_failures"].as_u64().unwrap_or(0),
        med["unresolved_imports"].as_u64().unwrap_or(0),
        med["elapsed_us"].as_u64().unwrap_or(0),
        min["elapsed_us"].as_u64().unwrap_or(0),
        max["elapsed_us"].as_u64().unwrap_or(0),
        med["peak_rss_bytes"].as_u64().unwrap_or(0),
        min["peak_rss_bytes"].as_u64().unwrap_or(0),
        max["peak_rss_bytes"].as_u64().unwrap_or(0),
        verdict_str(report.verdict),
    );
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass => "PASS",
        Verdict::Regress => "REGRESS",
        Verdict::StaleBaseline => "STALE_BASELINE",
        Verdict::NoBaseline => "NO_BASELINE",
    }
}

fn fmt_bytes(n: u64) -> String {
    let mb = n as f64 / (1024.0 * 1024.0);
    format!("{mb:.1}MB")
}

fn fmt_ms(ms: f64) -> String {
    format!("{ms:.0}ms")
}

fn median_u64(xs: &mut [u64]) -> u64 {
    xs.sort_unstable();
    let n = xs.len();
    if n == 0 {
        return 0;
    }
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        xs[n / 2 - 1] / 2 + xs[n / 2] / 2
    }
}

fn pct_spread(min: u64, max: u64, med: u64) -> f64 {
    if med == 0 {
        return 0.0;
    }
    (max.saturating_sub(min)) as f64 / med as f64 * 100.0
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn astrolabe_git() -> Value {
    let Some(ws) = workspace_root() else {
        return Value::Null;
    };
    git_trim(&["-C", &ws, "rev-parse", "HEAD"])
        .map(Value::String)
        .unwrap_or(Value::Null)
}

fn astrolabe_dirty() -> bool {
    let Some(ws) = workspace_root() else {
        return false;
    };
    git_trim(&["-C", &ws, "status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

fn workspace_root() -> Option<String> {
    if let Some(top) = git_trim(&["rev-parse", "--show-toplevel"]) {
        return Some(top);
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_string_lossy().into_owned())
}

fn git_trim(args: &[&str]) -> Option<String> {
    for bin in ["/usr/bin/git", "/opt/homebrew/bin/git", "git"] {
        let Ok(out) = Command::new(bin).args(args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn host_info() -> Value {
    let cpu = mac_cpu().or_else(linux_cpu);
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "cpu": cpu,
    })
}

fn mac_cpu() -> Option<String> {
    for bin in ["/usr/sbin/sysctl", "/sbin/sysctl", "sysctl"] {
        let Ok(out) = Command::new(bin)
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
        else {
            continue;
        };
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn linux_cpu() -> Option<String> {
    fs::read_to_string("/proc/cpuinfo").ok().and_then(|t| {
        t.lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
    })
}

// ---------------------------------------------------------------------------
// Peak RSS: no extra crates. Linux = /proc; macOS = mach + getrusage.
// ---------------------------------------------------------------------------

mod rss {
    use std::mem::MaybeUninit;

    pub fn method() -> String {
        #[cfg(target_os = "linux")]
        {
            "proc_self_status.VmHWM".into()
        }
        #[cfg(target_os = "macos")]
        {
            "mach_task_basic_info.resident_size_max+getrusage".into()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            "unsupported".into()
        }
    }

    pub fn current_bytes() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            proc_kb("VmRSS")?.checked_mul(1024)
        }
        #[cfg(target_os = "macos")]
        {
            darwin_task().map(|t| t.resident_size)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    pub fn peak_bytes() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            proc_kb("VmHWM")?.checked_mul(1024)
        }
        #[cfg(target_os = "macos")]
        {
            let mach = darwin_task().map(|t| t.resident_size_max).unwrap_or(0);
            let ru = rusage_maxrss_bytes().unwrap_or(0);
            let n = mach.max(ru);
            if n == 0 {
                None
            } else {
                Some(n)
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn proc_kb(key: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in text.lines() {
            let Some((k, rest)) = line.split_once(':') else {
                continue;
            };
            if k != key {
                continue;
            }
            let num = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(num);
        }
        None
    }

    /// `ru_maxrss` is **bytes** on Darwin and **kilobytes** on Linux/BSD.
    #[allow(dead_code)]
    fn rusage_maxrss_bytes() -> Option<u64> {
        #[repr(C)]
        struct Rusage {
            _times: [u64; 4],
            ru_maxrss: i64,
            _rest: [i64; 14],
        }
        extern "C" {
            fn getrusage(who: i32, usage: *mut Rusage) -> i32;
        }
        const RUSAGE_SELF: i32 = 0;
        unsafe {
            let mut ru = MaybeUninit::<Rusage>::zeroed();
            if getrusage(RUSAGE_SELF, ru.as_mut_ptr()) != 0 {
                return None;
            }
            let n = ru.assume_init().ru_maxrss;
            if n <= 0 {
                return None;
            }
            let n = n as u64;
            if cfg!(target_os = "macos") {
                Some(n)
            } else {
                n.checked_mul(1024)
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        _user_time: [i32; 2],
        _system_time: [i32; 2],
        _policy: i32,
        _suspend_count: i32,
    }

    #[cfg(target_os = "macos")]
    fn darwin_task() -> Option<MachTaskBasicInfo> {
        extern "C" {
            static mut mach_task_self_: u32;
            fn task_info(target: u32, flavor: i32, out: *mut u8, count: *mut u32) -> i32;
        }
        const MACH_TASK_BASIC_INFO: i32 = 20;
        unsafe {
            let mut info = MaybeUninit::<MachTaskBasicInfo>::zeroed();
            let mut count = (std::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
            let rc = task_info(
                mach_task_self_,
                MACH_TASK_BASIC_INFO,
                info.as_mut_ptr() as *mut u8,
                &mut count,
            );
            if rc != 0 {
                return None;
            }
            Some(info.assume_init())
        }
    }
}
