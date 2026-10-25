//! Text-search fallback for precise queries that timed out.
//!
//! A hung language server must not leave the caller with nothing: an empty
//! "unknown" answer sends an agent off to grep by hand, and a partial edit
//! made from it is how a call site gets missed. Instead we return every
//! whole-word occurrence of the name in files of the same language, labelled
//! [`Confidence::Syntactic`] with a note that says exactly what happened.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::scan::{scan, ScanOptions};
use crate::types::{Confidence, Language, RelPath};

use super::{Location, Position, Precise, Range};

/// Hard caps so the fallback itself can never become the new hang.
const MAX_FILES: usize = 20_000;
const MAX_HITS: usize = 2_000;
const BUDGET: Duration = Duration::from_secs(10);

/// The identifier under `position` in `path`, if any.
pub(crate) fn identifier_at(
    repo_root: &Path,
    path: &RelPath,
    position: Position,
) -> Option<String> {
    let source = crate::text::read_source(&repo_root.join(path.as_str())).ok()?;
    let line = source.split('\n').nth(position.line as usize)?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    // Map the UTF-16 column to a char index.
    let mut utf16 = 0u32;
    let chars: Vec<char> = line.chars().collect();
    let mut idx = chars.len();
    for (i, ch) in chars.iter().enumerate() {
        if utf16 >= position.character {
            idx = i;
            break;
        }
        utf16 += ch.len_utf16() as u32;
    }
    if idx >= chars.len() || !is_ident(chars[idx]) {
        return None;
    }
    let mut start = idx;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx;
    while end < chars.len() && is_ident(chars[end]) {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

/// Whole-word occurrences of `name` in files sharing `origin`'s language.
pub(crate) fn text_references(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    reason: &str,
    files: Option<&[RelPath]>,
) -> Precise<Vec<Location>> {
    let Some(language) = Language::from_path(origin) else {
        // Unknown origin language: matching "every unknown file" would sweep
        // docs, configs and binaries.
        return Precise {
            value: Vec::new(),
            confidence: Confidence::Unknown,
            note: Some(format!(
                "{reason}; the file's language is not recognised, so no text search was run for `{name}`"
            )),
        };
    };
    let (hits, truncated) = collect(
        repo_root,
        origin,
        name,
        |f| Language::from_path(f) == Some(language),
        false,
        files,
    );
    let mut note = format!(
        "{reason}; fell back to a whole-word text search for `{name}`. \
         These are textual matches (may include comments, strings, and \
         unrelated symbols with the same name), not language-server results"
    );
    if truncated {
        note.push_str("; the text search hit its size/time budget and may be incomplete");
    }
    Precise {
        value: hits,
        confidence: Confidence::Syntactic,
        note: Some(note),
    }
}

/// Code (non-comment, non-string) occurrences of `name` across the Swift or
/// Objective-C family `origin` belongs to, located with tree-sitter.
///
/// Used when sourcekit-lsp / clangd answer without an index (no SwiftPM
/// build, no compile flags, code inside an inactive `#if`). Objective-C
/// includes `.m`, `.mm` and `.h`, since an ObjC symbol is declared in a
/// header and used in implementation files.
pub(crate) fn syntactic_references(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    reason: &str,
    files: Option<&[RelPath]>,
) -> Precise<Vec<Location>> {
    let swift = Language::from_path(origin) == Some(Language::Swift);
    let (hits, truncated) = collect(
        repo_root,
        origin,
        name,
        |f| {
            if swift {
                Language::from_path(f) == Some(Language::Swift)
            } else {
                matches!(
                    Language::from_path(f),
                    Some(Language::ObjC | Language::ObjCpp)
                ) || f.extension().is_some_and(|e| e.eq_ignore_ascii_case("h"))
            }
        },
        true,
        files,
    );
    let mut note = format!(
        "{reason}. 已退回 tree-sitter 语法检索：以下是 `{name}` 在代码中（已排除注释和字符串）的同名出现，\
         不是语言服务器的语义结果，可能包含同名但无关的符号（confidence: Syntactic）"
    );
    if truncated {
        note.push_str("；检索达到文件数/时间上限，结果可能不完整");
    }
    Precise {
        value: hits,
        confidence: Confidence::Syntactic,
        note: Some(note),
    }
}

/// `known_files`: the admitted file list of a fresh index, when the caller
/// has one; reusing it skips a full directory walk per query. `None` walks
/// the tree with the default scan options.
fn collect(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    keep: impl Fn(&RelPath) -> bool,
    code_only: bool,
    known_files: Option<&[RelPath]>,
) -> (Vec<Location>, bool) {
    let started = Instant::now();
    let mut files: Vec<RelPath> = match known_files {
        Some(known) => known.iter().filter(|f| keep(f)).cloned().collect(),
        None => scan(repo_root, &ScanOptions::default()).files,
    };
    files.retain(|f| keep(f));
    files.sort();
    // Origin first: if the budget runs out, the local hits survive.
    if let Some(pos) = files.iter().position(|f| f == origin) {
        let f = files.remove(pos);
        files.insert(0, f);
    }
    // Only files that can contain the name need the sequential decode and
    // line scan; the byte-level rejection runs on all cores. `collect`
    // keeps the original order, so the origin-first guarantee holds.
    let files: Vec<RelPath> = {
        use rayon::prelude::*;
        let window = files.len().min(MAX_FILES + 1);
        let (head, tail) = files.split_at(window);
        let mut kept: Vec<RelPath> = head
            .par_iter()
            .filter(|f| {
                std::fs::read(repo_root.join(f.as_str()))
                    .map(|bytes| !cannot_contain_name(&bytes, name))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        kept.extend(tail.iter().cloned());
        kept
    };
    let mut hits = Vec::new();
    let mut truncated = false;
    for (n, file) in files.iter().enumerate() {
        if n >= MAX_FILES || hits.len() >= MAX_HITS || started.elapsed() > BUDGET {
            truncated = true;
            break;
        }
        let Ok(source) = crate::text::read_source(&repo_root.join(file.as_str())) else {
            continue;
        };
        // One substring search over the whole file rules out almost every
        // file before any per-line work (a whole-word hit is also a substring).
        if !source.contains(name) {
            continue;
        }
        let mut file_hits = Vec::new();
        for (line_idx, line) in source.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            for column in word_columns(line, name) {
                let start = Position {
                    line: line_idx as u32,
                    character: column,
                };
                let end = Position {
                    line: line_idx as u32,
                    character: column + name.encode_utf16().count() as u32,
                };
                file_hits.push(Location {
                    path: file.clone(),
                    range: Range { start, end },
                });
            }
        }
        if code_only && !file_hits.is_empty() {
            let starts: Vec<Position> = file_hits.iter().map(|l| l.range.start).collect();
            if let Some(mask) = super::anchor::code_mask(file, &source, &starts) {
                let mut keep = mask.into_iter();
                file_hits.retain(|_| keep.next().unwrap_or(true));
            }
        }
        hits.extend(file_hits);
        if hits.len() > MAX_HITS {
            // A single huge file can overshoot the cap inside one iteration.
            hits.truncate(MAX_HITS);
            truncated = true;
            break;
        }
    }
    (hits, truncated)
}

/// Identifier characters; `$` covers JS/PHP names (`$name`, `$el`).
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Cheap byte-level rejection before decoding. Only sound for an ASCII name
/// and a file in an ASCII-compatible encoding (UTF-8, GBK, Shift-JIS …): then
/// any decoded occurrence of the name is also a byte occurrence. UTF-16 (BOM
/// or NUL bytes up front) is never rejected here.
fn cannot_contain_name(bytes: &[u8], name: &str) -> bool {
    if !name.is_ascii() || name.is_empty() {
        return false;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return false;
    }
    if memchr::memchr(0, &bytes[..bytes.len().min(512)]).is_some() {
        return false;
    }
    memchr::memmem::find(bytes, name.as_bytes()).is_none()
}

fn word_columns(line: &str, name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    for (byte_idx, _) in line.match_indices(name) {
        let before_ok = line[..byte_idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = line[byte_idx + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            out.push(line[..byte_idx].encode_utf16().count() as u32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// Timing probe: `cargo test -p astrolabe-core --lib bench_text_references -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_text_references_many_files() {
        let dir = tempfile::tempdir().unwrap();
        for d in 0..50 {
            let sub = dir.path().join(format!("d{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..100 {
                let mut src = String::new();
                for l in 0..300 {
                    src.push_str(&format!(
                        "fn item_{d}_{f}_{l}(x: i32) -> i32 {{ x + {l} }}\n"
                    ));
                }
                if f % 50 == 0 {
                    src.push_str("fn caller() { needle_fn(); }\n");
                }
                std::fs::write(sub.join(format!("m{f}.rs")), src).unwrap();
            }
        }
        let t = Instant::now();
        let mut n = 0;
        for _ in 0..5 {
            n = text_references(
                dir.path(),
                &RelPath::new("d0/m0.rs"),
                "needle_fn",
                "x",
                None,
            )
            .value
            .len();
        }
        let ts = Instant::now();
        let nfiles = scan(dir.path(), &ScanOptions::default()).files.len();
        let tr = Instant::now();
        let files = scan(dir.path(), &ScanOptions::default()).files;
        let total: usize = files
            .iter()
            .map(|f| std::fs::read(dir.path().join(f.as_str())).unwrap().len())
            .sum();
        eprintln!(
            "BENCH5 scan+raw read of {total} bytes (floor): {:?}",
            tr.elapsed()
        );
        eprintln!("BENCH5 scan alone {nfiles} files: {:?}", ts.elapsed());
        eprintln!("BENCH5 5000 files, {n} hits: {:?}/call", t.elapsed() / 5);
        let known = scan(dir.path(), &ScanOptions::default()).files;
        let tk = Instant::now();
        for _ in 0..5 {
            n = text_references(
                dir.path(),
                &RelPath::new("d0/m0.rs"),
                "needle_fn",
                "x",
                Some(&known),
            )
            .value
            .len();
        }
        eprintln!(
            "BENCH5 known file list, {n} hits: {:?}/call",
            tk.elapsed() / 5
        );
    }

    /// A caller-supplied file list (the index's admitted paths) gives the
    /// same answer as walking the tree, in any order, and is what is
    /// searched: a file missing from the list is not read.
    #[test]
    fn known_file_list_matches_directory_walk() {
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &str, s: &str| {
            let full = dir.path().join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, s).unwrap();
        };
        w("a.rs", "fn foo() {}\nfn bar() { foo(); }\n");
        w(
            "sub/b.rs",
            "// foo in a comment\nfn baz() { foo(); foo_x(); }\n",
        );
        w("sub/c.py", "foo()\n");
        w("A.h", "@interface Widget : NSObject\n@end\n");
        w(
            "A.m",
            "@implementation Widget\n- (void)f { [Widget new]; }\n@end\n",
        );
        let mut walked = scan(dir.path(), &ScanOptions::default()).files;
        walked.reverse();
        let origin = RelPath::new("a.rs");
        let a = text_references(dir.path(), &origin, "foo", "x", None);
        let b = text_references(dir.path(), &origin, "foo", "x", Some(&walked));
        assert_eq!(a.value, b.value);
        assert_eq!(a.note, b.note);
        assert_eq!(a.value.len(), 4, "{:?}", a.value);
        let h = RelPath::new("A.h");
        let a = syntactic_references(dir.path(), &h, "Widget", "x", None);
        let b = syntactic_references(dir.path(), &h, "Widget", "x", Some(&walked));
        assert_eq!(a.value, b.value);
        let only_a = [RelPath::new("a.rs")];
        let c = text_references(dir.path(), &origin, "foo", "x", Some(&only_a));
        assert!(
            c.value.iter().all(|l| l.path.as_str() == "a.rs"),
            "{:?}",
            c.value
        );
        assert_eq!(c.value.len(), 2);
    }

    #[test]
    fn text_references_unknown_language_searches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.weird1"), "foo\n").unwrap();
        std::fs::write(dir.path().join("b.weird2"), "foo\n").unwrap();
        let out = text_references(dir.path(), &RelPath::new("a.weird1"), "foo", "x", None);
        assert!(out.value.is_empty());
        assert_eq!(out.confidence, Confidence::Unknown);
        assert!(out.note.unwrap().contains("not recognised"));
    }

    use super::*;

    #[test]
    fn byte_prefilter_never_hides_utf16_or_gbk_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut le = vec![0xFF, 0xFE];
        for u in "fn x() { needle_fn(); }\n".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.path().join("a.rs"), le).unwrap();
        let (gbk, _, _) =
            encoding_rs::GBK.encode("// 中文注释，这是一个测试\nfn y() { needle_fn(); }\n");
        std::fs::write(dir.path().join("b.rs"), &gbk).unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn z() {}\n").unwrap();
        let r = text_references(dir.path(), &RelPath::new("a.rs"), "needle_fn", "x", None);
        let mut paths: Vec<_> = r
            .value
            .iter()
            .map(|l| l.path.as_str().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, ["a.rs", "b.rs"]);
        assert!(cannot_contain_name(b"fn z() {}", "needle_fn"));
        assert!(!cannot_contain_name(b"a needle_fn b", "needle_fn"));
    }

    #[test]
    fn finds_whole_words_across_same_language_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nfn foobar() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn x() { foo(); }\n").unwrap();
        std::fs::write(dir.path().join("c.py"), "foo()\n").unwrap();
        let r = text_references(dir.path(), &RelPath::new("a.rs"), "foo", "timed out", None);
        assert_eq!(r.confidence, Confidence::Syntactic);
        assert!(r.note.unwrap().contains("timed out"));
        let paths: Vec<_> = r
            .value
            .iter()
            .map(|l| l.path.as_str().to_string())
            .collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn syntactic_references_skip_comments_and_span_objc_family() {
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &str, b: &str| std::fs::write(dir.path().join(p), b).unwrap();
        w(
            "A.h",
            "// Widget is documented here\n@interface Widget : NSObject\n@end\n",
        );
        w("A.m", "#import \"A.h\"\n@implementation Widget\n- (void)f { NSLog(@\"Widget\"); [Widget new]; }\n@end\n");
        w("B.mm", "void g() { Widget *w = nil; }\n");
        w("C.swift", "let Widget = 1\n");
        let r = syntactic_references(
            dir.path(),
            &RelPath::new("A.h"),
            "Widget",
            "clangd has no index",
            None,
        );
        assert_eq!(r.confidence, Confidence::Syntactic);
        assert!(r.note.as_deref().unwrap().contains("tree-sitter"));
        let got: Vec<_> = r
            .value
            .iter()
            .map(|l| (l.path.as_str().to_string(), l.range.start.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("A.h".to_string(), 1),
                ("A.m".to_string(), 1),
                ("A.m".to_string(), 2),
                ("B.mm".to_string(), 0),
            ],
            "comment / string hits dropped, swift excluded"
        );

        w(
            "D.swift",
            "/// Widget doc\nstruct Gadget { let w: Widget }\n",
        );
        let r = syntactic_references(
            dir.path(),
            &RelPath::new("C.swift"),
            "Widget",
            "no index",
            None,
        );
        let got: Vec<_> = r
            .value
            .iter()
            .map(|l| l.path.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["C.swift", "D.swift"]);
    }

    #[test]
    fn dollar_names_match_as_whole_words() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "$name = 1;\necho $name; echo name;\necho $names;\n",
        )
        .unwrap();
        let r = text_references(dir.path(), &RelPath::new("a.php"), "$name", "x", None);
        assert_eq!(r.value.len(), 2);
        // `name` alone must not match inside `$name`.
        let r = text_references(dir.path(), &RelPath::new("a.php"), "name", "x", None);
        assert_eq!(r.value.len(), 1);
        let pos = Position {
            line: 1,
            character: 7,
        };
        assert_eq!(
            identifier_at(dir.path(), &RelPath::new("a.php"), pos).as_deref(),
            Some("$name")
        );
    }

    #[test]
    fn single_huge_file_cannot_exceed_hit_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.rs"), "foo\n".repeat(MAX_HITS * 3)).unwrap();
        let r = text_references(dir.path(), &RelPath::new("big.rs"), "foo", "x", None);
        assert_eq!(r.value.len(), MAX_HITS);
        assert!(r.note.unwrap().contains("budget"));
    }

    #[test]
    fn identifier_at_position() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "let total_count = 1;\n").unwrap();
        let pos = Position {
            line: 0,
            character: 6,
        };
        assert_eq!(
            identifier_at(dir.path(), &RelPath::new("a.rs"), pos).as_deref(),
            Some("total_count")
        );
    }
}
