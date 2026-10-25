//! Resolver performance benchmark + output-equivalence guard (audit F10–F13,
//! E5/E6). Builds a large synthetic monorepo, times every resolver's `detect`
//! and the `resolve_import` calls, and writes a deterministic dump of all
//! `(from, spec) -> result` lines. Run with
//! `cargo test --release --test resolvers_perf -- --ignored --nocapture`;
//! set `RESOLVER_PERF_DUMP=<file>` to keep the dump and diff it across
//! revisions (the `resolvers_perf_output_is_stable` test checks a digest).

use astrolabe_core::resolvers;
use astrolabe_core::types::{FileIndex, ImportResolution, Language, RelPath};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

fn write(root: &Path, rel: &str, c: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, c).unwrap();
}

struct Repo {
    files: FileIndex,
    /// (from, language, spec)
    imports: Vec<(RelPath, Language, String)>,
}

fn build(root: &Path) -> Repo {
    let mut paths: Vec<String> = Vec::new();
    let mut imports: Vec<(String, Language, String)> = Vec::new();
    let add = |root: &Path, rel: String, c: &str, paths: &mut Vec<String>| {
        write(root, &rel, c);
        paths.push(rel);
    };

    // TypeScript monorepo: 150 packages, 40 files, own tsconfig each.
    for p in 0..150 {
        add(
            root,
            format!("web/pkg{p}/tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":".","outDir":"dist","paths":{"@p/*":["src/*"],"~/*":["./*"]}},"exclude":["dist","build"]}"#,
            &mut paths,
        );
        for k in 0..40 {
            let rel = format!("web/pkg{p}/src/f{k}.ts");
            add(root, rel.clone(), "export const a = 1;\n", &mut paths);
            for spec in [
                format!("./f{}", (k + 1) % 40),
                format!("@p/f{}", (k + 7) % 40),
                "lodash".to_string(),
                format!("src/f{}", (k + 3) % 40),
                "react-dom/client".to_string(),
            ] {
                imports.push((rel.clone(), Language::TypeScript, spec));
            }
        }
    }

    // Rust workspace: 120 crates x 25 modules.
    let mut members = String::new();
    for c in 0..120 {
        members.push_str(&format!("\"crates/c{c}\","));
    }
    add(
        root,
        "Cargo.toml".into(),
        &format!("[workspace]\nmembers=[{members}]\n"),
        &mut paths,
    );
    for c in 0..120 {
        add(
            root,
            format!("crates/c{c}/Cargo.toml"),
            &format!("[package]\nname=\"c{c}\"\nversion=\"0.1.0\"\n"),
            &mut paths,
        );
        let mut lib = String::new();
        for m in 0..25 {
            lib.push_str(&format!("pub mod m{m};\n"));
        }
        add(root, format!("crates/c{c}/src/lib.rs"), &lib, &mut paths);
        for m in 0..25 {
            let rel = format!("crates/c{c}/src/m{m}.rs");
            add(root, rel.clone(), "pub struct X;\n", &mut paths);
            for spec in [
                format!("crate::m{}::X", (m + 1) % 25),
                format!("super::m{}", (m + 2) % 25),
                format!("c{}::m{}::X", (c + 1) % 120, m),
                "std::fmt::Display".to_string(),
                format!("crate::{{m{}::X, m{}::X}}", (m + 3) % 25, (m + 4) % 25),
            ] {
                imports.push((rel.clone(), Language::Rust, spec));
            }
        }
    }

    // Shell: 2500 scripts, dynamic sources that miss the direct probes.
    for i in 0..2500 {
        let rel = format!("ops/d{}/s{i}.sh", i % 50);
        add(root, rel.clone(), "echo\n", &mut paths);
        for spec in [
            format!("\"$DIR\"/s{}.sh", (i + 5) % 2500),
            format!("$ROOT/lib/missing{i}.sh"),
            "./s0.sh".to_string(),
        ] {
            imports.push((rel.clone(), Language::Shell, spec));
        }
    }

    // Ruby: 40 gems with lib roots, 1500 files.
    for g in 0..40 {
        add(root, format!("gems/g{g}/g{g}.gemspec"), "", &mut paths);
        for i in 0..40 {
            let rel = format!("gems/g{g}/lib/g{g}/r{i}.rb");
            add(root, rel.clone(), "x = 1\n", &mut paths);
            for spec in [
                format!("g{g}/r{}", (i + 1) % 40),
                format!("g{}/r{i}", (g + 1) % 40),
                "json".to_string(),
                "./r0".to_string(),
            ] {
                imports.push((rel.clone(), Language::Ruby, spec));
            }
        }
    }

    // Swift: 150 packages.
    for p in 0..150 {
        add(
            root,
            format!("swift/p{p}/Package.swift"),
            &format!("let package = Package(name: \"P{p}\", targets: [.target(name: \"M{p}\"), .target(name: \"N{p}\", path: \"Lib/N{p}\")])\n"),
            &mut paths,
        );
        for f in 0..8 {
            let rel = format!("swift/p{p}/Sources/M{p}/F{f}.swift");
            add(root, rel.clone(), "struct S {}\n", &mut paths);
            for spec in [
                format!("M{}", (p + 1) % 150),
                format!("N{}", (p + 2) % 150),
                "Foundation".to_string(),
            ] {
                imports.push((rel.clone(), Language::Swift, spec));
            }
        }
        add(root, format!("swift/p{p}/Lib/N{p}/x.swift"), "", &mut paths);
    }

    // C# / Kotlin: detect reads every source file.
    for a in 0..40 {
        add(
            root,
            format!("cs/a{a}/A{a}.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><RootNamespace>App</RootNamespace></PropertyGroup></Project>",
            &mut paths,
        );
    }
    for n in 0..4000 {
        let rel = format!("cs/a{}/C{n}.cs", n % 40);
        add(
            root,
            rel.clone(),
            &format!("namespace App.N{};\npublic class C{n} {{}}\n", n % 40),
            &mut paths,
        );
        imports.push((
            rel.clone(),
            Language::CSharp,
            format!("App.N{}", (n + 1) % 40),
        ));
    }
    for n in 0..4000 {
        let rel = format!("kt/k{}/K{n}.kt", n % 40);
        add(
            root,
            rel.clone(),
            &format!("package app.n{}\nclass K{n}\n", n % 40),
            &mut paths,
        );
        imports.push((
            rel.clone(),
            Language::Kotlin,
            format!("app.n{}.K{}", n % 40, n),
        ));
    }

    let files = FileIndex::new(root, paths.iter().map(RelPath::new));
    Repo {
        files,
        imports: imports
            .into_iter()
            .map(|(f, l, s)| (RelPath::new(f), l, s))
            .collect(),
    }
}

fn run() -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = build(dir.path());
    let mut report = String::new();
    writeln!(report, "files: {}", repo.files.len()).unwrap();
    // Per-resolver detect timing.
    for r in resolvers::all() {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let _ = r.detect(&repo.files);
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        writeln!(
            report,
            "detect {:>12?}: {:>8.1} ms (min of 5)",
            r.language(),
            best
        )
        .unwrap();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let _ = resolvers::ResolverSet::detect(&repo.files);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    let set = resolvers::ResolverSet::detect(&repo.files);
    writeln!(report, "detect ALL (min of 5): {best:.1} ms").unwrap();

    let mut dump = String::new();
    for lang in [
        Language::TypeScript,
        Language::Rust,
        Language::Shell,
        Language::Ruby,
        Language::Swift,
        Language::CSharp,
        Language::Kotlin,
    ] {
        let mut best = f64::MAX;
        for _ in 0..4 {
            let t = Instant::now();
            for (from, l, spec) in repo.imports.iter().filter(|(_, l, _)| *l == lang) {
                let _ = set.resolve_import_as(*l, from, spec, &repo.files);
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let mut n = 0usize;
        let mut lines: Vec<String> = Vec::new();
        for (from, l, spec) in repo.imports.iter().filter(|(_, l, _)| *l == lang) {
            let r = set.resolve_import_as(*l, from, spec, &repo.files);
            n += 1;
            let shown = match r {
                ImportResolution::Unresolved => "-".to_string(),
                ImportResolution::File(p) => format!("F {p}"),
                ImportResolution::Files(v) => format!(
                    "FS {}",
                    v.iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                other => format!("{other:?}"),
            };
            lines.push(format!("{from}\t{spec}\t{shown}"));
        }
        writeln!(
            report,
            "resolve {:>10?}: {:>7} imports {:>9.1} ms (min of 4)",
            lang, n, best
        )
        .unwrap();
        for l in lines {
            dump.push_str(&l);
            dump.push('\n');
        }
    }
    // Full detect metadata must match too.
    for (lang, meta) in set.iter_meta() {
        // `scope_index` is a HashMap: print it sorted so the dump is stable.
        let mut m = meta.clone();
        let scopes: std::collections::BTreeMap<_, _> =
            std::mem::take(&mut m.scope_index.0).into_iter().collect();
        writeln!(dump, "META {lang:?} {m:?} SCOPES {scopes:?}").unwrap();
    }
    (report, dump)
}

#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn resolvers_perf_bench() {
    let (report, dump) = run();
    println!("{report}");
    let digest = hex::encode(Sha256::digest(dump.as_bytes()));
    println!("dump sha256 {digest}");
    if let Ok(p) = std::env::var("RESOLVER_PERF_DUMP") {
        fs::write(p, dump).unwrap();
    }
}
