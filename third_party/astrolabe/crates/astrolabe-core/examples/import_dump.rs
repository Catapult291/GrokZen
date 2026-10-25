//! Dump every extracted import and the file it resolves to, as TSV
//! (`from \t lang \t spec \t resolved-or-empty`). Used by the import
//! resolution recall bench to compare against an independent ground truth.
//!
//! ```text
//! cargo run --release --example import_dump -- <repo> [lang ...]
//! ```

use std::path::PathBuf;

use astrolabe_core::parse::ParserPool;
use astrolabe_core::resolvers::ResolverSet;
use astrolabe_core::scan::{scan, to_index, ScanOptions};
use astrolabe_core::types::ImportResolution;
use astrolabe_core::Language;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: import_dump <repo> [lang ...]"));
    let wanted: Vec<String> = args.map(|a| a.to_ascii_lowercase()).collect();
    let res = scan(&root, &ScanOptions::default());
    if std::env::var_os("DUMP_SKIPPED").is_some() {
        for (p, why) in &res.skipped {
            eprintln!("skipped\t{}\t{why:?}", p.as_str());
        }
    }
    let files = to_index(&root, &res);
    let set = ResolverSet::detect(&files);
    let pool = ParserPool::new();
    for path in &res.files {
        let Ok(src) = std::fs::read_to_string(root.join(path.as_str())) else {
            eprintln!("unreadable\t{}", path.as_str());
            continue;
        };
        let Some(lang) = Language::from_path_and_source(path, &src) else {
            if std::env::var_os("DUMP_SKIPPED").is_some() {
                eprintln!("no-language\t{}", path.as_str());
            }
            continue;
        };
        if !wanted.is_empty()
            && !wanted
                .iter()
                .any(|w| w == &lang.name().to_ascii_lowercase())
        {
            continue;
        }
        let parsed = match pool.parse(lang, path, &src) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("parse-error\t{}\t{e:?}", path.as_str());
                continue;
            }
        };
        for spec in parsed.imports {
            let target = match set.resolve_import_as(lang, path, &spec, &files) {
                ImportResolution::File(p) => p.as_str().to_owned(),
                _ => String::new(),
            };
            println!(
                "{}\t{}\t{}\t{}",
                path.as_str(),
                lang.name(),
                spec.replace(['\t', '\n'], " "),
                target
            );
        }
    }
}
