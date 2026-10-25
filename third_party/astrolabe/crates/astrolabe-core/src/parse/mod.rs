//! Tree-sitter parsing and symbol extraction.
//!
//! Three hard requirements, each from a measured failure in the prior art:
//!
//! 1. **Release resources.** Drop every `Tree` and reuse `Parser` instances via
//!    a pool keyed by language. The WASM implementation allocated a parser per
//!    file and never freed the trees, which cost 5 GB of peak RSS on a
//!    1200-file repo — index a two-file repo and it still took 1.4 GB.
//! 2. **Never swallow a failure.** Every error returns an `Err` that the caller
//!    records in `IndexReport::parse_failures`. A `catch {}` here lost 205 of
//!    1026 files silently.
//! 3. **Real timeouts.** If a per-file budget is needed, use tree-sitter's own
//!    cancellation (`Parser::set_timeout_micros` / cancellation flag). Racing a
//!    synchronous parse against a timer does nothing — the parse holds the
//!    thread and the timer only fires afterwards, which is how the prior art
//!    produced 913 bogus "timeout" logs for files that had parsed fine.
//!
//! Symbol extraction uses the `.scm` queries in `queries/`, loaded per
//! language. Prefer the upstream grammar's `tags.scm` conventions
//! (`@definition.*` / `@reference.*`) over inventing capture names.

use std::cell::Cell;
use std::ops::ControlFlow;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::types::{CodeSymbol, FileId, Language, RelPath, SymbolId, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator};

pub mod queries;
mod razor;
mod vue;

mod cfamily;
mod csharp_pp;
// lang-batch-8 per-language hooks (one file per worker).
mod dart;
mod decor;
mod kotlin;
mod lua;
mod objc;
pub use objc::header_language as objc_header_language;
mod diag;
mod ruby;
mod shell;
mod swift;
pub use diag::{
    diagnose_indexed_syntax, diagnose_syntax, SyntaxDiagnosis, SyntaxErrorKind, DIAG_FORMAT_VERSION,
};
mod jsx_amp;
mod ts_index_sig;
mod ts_variance;

pub(crate) use razor::extract_razor_csharp;

const PARSE_BUDGET: Duration = Duration::from_secs(5);
const MAX_SIGNATURE_CHARS: usize = 240;
const LANGUAGES: [Language; 20] = [
    Language::Python,
    Language::Go,
    Language::Java,
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Php,
    Language::C,
    Language::Cpp,
    Language::Dart,
    Language::CSharp,
    Language::VisualBasic,
    Language::ObjC,
    Language::ObjCpp,
    Language::Swift,
    Language::Kotlin,
    Language::Shell,
    Language::Ruby,
    Language::Lua,
];

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("no grammar for {0:?}")]
    NoGrammar(Language),
    #[error("tree-sitter failed to produce a tree")]
    NoTree,
    #[error("parse exceeded its budget")]
    Timeout,
    #[error("query error: {0}")]
    Query(String),
}

#[derive(Clone, Debug, Default)]
pub struct ParsedFile {
    pub symbols: Vec<CodeSymbol>,
    /// Raw import specifiers, in source order. Resolution happens later, in
    /// `resolvers` — this layer must not guess at paths.
    pub imports: Vec<String>,
    /// `(callee_name, line)`; name-based, so downstream confidence is
    /// `Syntactic`.
    pub calls: Vec<(String, u32)>,
    /// The syntax tree contained ERROR / MISSING nodes, so `symbols`,
    /// `imports` and `calls` may be incomplete. The result is still usable;
    /// the indexer reports it in `partial_parses` (not `parse_failures`);
    /// it is cached like a clean parse, flag included.
    pub partial: bool,
    /// C / C++: this result is the parse of the `cfamily.rs` rewrite of the
    /// source (same bytes per line), not of the raw text. Diagnostics of a
    /// still-partial file must then look at that rewrite
    /// ([`diagnose_indexed_syntax`]).
    pub c_family_rewrite: bool,
}

/// Parser pool. Holds reusable `Parser`s per language.
pub struct ParserPool {
    // `Parser` is not Sync and a parse needs `&mut Parser`. Each language has a
    // small idle stack: a parse pops one (or builds one when the stack is
    // empty) and pushes it back afterwards, so same-language parses run in
    // parallel instead of serializing on one mutex. The lock is held only for
    // the pop/push, never during a parse. The stack can never hold more
    // parsers than the peak number of concurrent parses of that language
    // (bounded by the thread count), and no Tree is ever retained.
    parsers: [Mutex<Vec<Parser>>; LANGUAGES.len()],
}

/// A parser checked out of the pool; returns itself on drop.
struct PooledParser<'a> {
    pool: &'a Mutex<Vec<Parser>>,
    parser: Option<Parser>,
}

impl std::ops::Deref for PooledParser<'_> {
    type Target = Parser;
    fn deref(&self) -> &Parser {
        self.parser.as_ref().expect("parser present until drop")
    }
}

impl std::ops::DerefMut for PooledParser<'_> {
    fn deref_mut(&mut self) -> &mut Parser {
        self.parser.as_mut().expect("parser present until drop")
    }
}

impl Drop for PooledParser<'_> {
    fn drop(&mut self) {
        if let Some(mut parser) = self.parser.take() {
            // A cancelled parse is resumable; always start the next file clean.
            parser.reset();
            if let Ok(mut idle) = self.pool.lock() {
                idle.push(parser);
            }
        }
    }
}

thread_local! {
    /// Deadline shared by the parse and the three query passes of the file
    /// currently being processed on this thread.
    static QUERY_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static QUERY_TIMED_OUT: Cell<bool> = const { Cell::new(false) };
}

/// Progress callback for `QueryCursor`: breaks once the per-file deadline set
/// by `parse_raw` has passed and records that it did (an interrupted cursor
/// otherwise just looks like "no more matches").
fn query_progress() -> impl FnMut(&tree_sitter::QueryCursorState) -> ControlFlow<()> {
    let deadline = QUERY_DEADLINE.with(Cell::get);
    move |_| match deadline {
        Some(deadline) if Instant::now() >= deadline => {
            QUERY_TIMED_OUT.with(|flag| flag.set(true));
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    }
}

impl ParserPool {
    pub fn new() -> Self {
        ParserPool {
            // Parsers are created lazily; a grammar ABI failure surfaces as an
            // explicit `NoGrammar` from `parse`.
            parsers: std::array::from_fn(|_| Mutex::new(Vec::new())),
        }
    }

    pub fn parse(
        &self,
        lang: Language,
        path: &RelPath,
        source: &str,
    ) -> Result<ParsedFile, ParseError> {
        // A preprocessor directive on the last line without a trailing
        // newline (`#pragma warning restore 618<EOF>`) makes the C-family
        // grammars flag the whole file as erroneous although every symbol is
        // extracted. Terminate it; offsets and lines are unchanged.
        if needs_eof_newline(lang, source) {
            let fixed = format!("{source}\n");
            return self.parse(lang, path, &fixed);
        }
        // Vue SFCs: embedded <script> extraction (no tree-sitter-vue on ABI 0.27).
        if lang == Language::Vue {
            return vue::parse_sfc(self, path, source);
        }
        // Razor / cshtml stay Language::CSharp (no extra parser-pool slot).
        // Extract before the C# grammar; a `.cs` stand-in path avoids
        // extracting twice. The caller still attributes symbols to `path`.
        if lang == Language::CSharp && razor::is_razor_source(path.as_str()) {
            let extracted = razor::extract_razor_csharp(source);
            return self.parse(
                Language::CSharp,
                &RelPath::new("__astrolabe_razor_extract__.cs"),
                &extracted,
            );
        }
        // ObjC / ObjC++: macro normalisation, and for `.mm` an ObjC pass over
        // C++-masked text unioned with a C++ pass over ObjC-masked text.
        if matches!(lang, Language::ObjC | Language::ObjCpp) {
            return objc::parse_with(lang, source, |l, s| self.parse_raw(l, path, s));
        }
        // Shell: zsh-only syntax and `.bats` `@test` blocks are rewritten
        // (line-preserving) for the bash grammar; nvm-style `\. file` source
        // lines are normalized (same length) on every parse pass.
        // Heredoc bodies with a line that starts with the delimiter
        // (`BLOCKER (…)` in `<<'BLOCKER'`) end early in tree-sitter-bash;
        // on a partial parse retry with those bodies neutralized (same
        // length), kept only when clean. See `neutralize_heredoc_bodies`.
        if lang == Language::Shell {
            let run = |text: &str| {
                shell::parse_with(
                    path.as_str(),
                    text,
                    |s| match shell::normalize_backslash_dot(s) {
                        Some(fixed) => self.parse_raw(lang, path, &fixed),
                        None => self.parse_raw(lang, path, s),
                    },
                )
            };
            let parsed = run(source)?;
            if parsed.partial {
                if let Some(fixed) = shell::neutralize_heredoc_bodies(source) {
                    if let Ok(retry) = run(&fixed) {
                        if !retry.partial {
                            return Ok(retry);
                        }
                    }
                }
            }
            return Ok(parsed);
        }
        // TypeScript 5.0 `export type * from './x'` is unknown to
        // tree-sitter-typescript 0.23 (ERROR node, whole file flagged partial).
        // Blank the `type` keyword (same byte length) so it parses as the
        // equivalent `export * from`, which keeps the re-export import edge.
        if matches!(lang, Language::TypeScript | Language::Tsx) {
            if let Some(fixed) = normalize_export_type_star(source) {
                return self.parse_raw(lang, path, &fixed);
            }
        }
        let parsed = self.parse_raw(lang, path, source)?;
        // C / C++: only when the first pass has errors, retry after
        // length-preserving rewrites (comments in multi-line `#define`,
        // `__has_include(<x>)`, `__cplusplus`-guarded `extern "C"` braces,
        // well-known library macros). See `cfamily.rs`.
        if parsed.partial && matches!(lang, Language::C | Language::Cpp) {
            if let Some(mut retry) =
                cfamily::retry(lang, source, &parsed, |s| self.parse_raw(lang, path, s))
            {
                retry.c_family_rewrite = true;
                return Ok(retry);
            }
        }
        // C# / VB: `#if` that splits a declaration, and .NET 10 `#:` lines,
        // are ERROR nodes. Only on a partial first pass, blank inactive
        // branches and `#:` lines (same length) and keep the retry if it is
        // clean. Blanked branches' `using`s stay (import edges are a union),
        // and their declarations come from a last-branch retry. Grammar
        // gaps fixed in the same retry: VB `Imports` after a blank line, C#
        // `[method]` collection elements.
        if parsed.partial && matches!(lang, Language::CSharp | Language::VisualBasic) {
            let vb = lang == Language::VisualBasic;
            // VB: `Imports` after a blank / comment line, too.
            let vb_header = |text: Option<String>| match &text {
                Some(t) => csharp_pp::regroup_vb_header(t).or(text),
                None => csharp_pp::regroup_vb_header(source),
            };
            // C#: `[method]` in a collection expression, too.
            let cs_targets = |text: Option<String>| match &text {
                Some(t) => csharp_pp::rename_collection_targets(t).or(text),
                None => csharp_pp::rename_collection_targets(source),
            };
            let fixed = if vb {
                vb_header(csharp_pp::blank_inactive_vb(source))
            } else {
                cs_targets(csharp_pp::blank_inactive(source))
            };
            if let Some(fixed) = fixed {
                if let Ok(mut retry) = self.parse_raw(lang, path, &fixed) {
                    if !retry.partial {
                        retry.imports = csharp_pp::union_imports(parsed.imports, retry.imports);
                        // Declarations only in a later branch: from the
                        // last-branch text, when that parses clean too.
                        let alt = csharp_pp::blank_alternate(source, vb)
                            .map(|a| match vb {
                                true => csharp_pp::regroup_vb_header(&a).unwrap_or(a),
                                false => csharp_pp::rename_collection_targets(&a).unwrap_or(a),
                            })
                            .filter(|alt| *alt != fixed);
                        if let Some(alt) = alt {
                            if let Ok(alt_parsed) = self.parse_raw(lang, path, &alt) {
                                if !alt_parsed.partial {
                                    csharp_pp::merge_alternate(
                                        &mut retry, source, &fixed, alt_parsed,
                                    );
                                }
                            }
                        }
                        return Ok(retry);
                    }
                }
            }
        }
        // TS import types in generics (`f<typeof import('x')>()`,
        // `Ref<import('vue').Ref<T>>`) make tree-sitter-typescript emit ERROR
        // nodes. Only when the first pass is partial, blank those
        // `import('lit')` spans with an identifier of equal length and keep
        // the retry only if it parses cleanly. Dynamic `import()` calls are
        // never touched (see `normalize_import_types`).
        // JSX attribute strings with a bare `&` (`href="/?a=1&b=2"`), see
        // `jsx_amp.rs`: kept only when clean; for TSX also the base of the
        // TS retries below so the fixes combine.
        let mut amp = None;
        if parsed.partial && matches!(lang, Language::JavaScript | Language::Tsx) {
            let attr = jsx_amp::escape_bare_ampersands(source);
            if let Some(fixed) = &attr {
                if let Ok(retry) = self.parse_raw(lang, path, fixed) {
                    if !retry.partial {
                        return Ok(retry);
                    }
                }
            }
            // Bare `&` in element text (`<span>A & B</span>`), on top.
            let base = attr.as_deref().unwrap_or(source);
            let text = jsx_amp::escape_text_ampersands(lang, base);
            if let Some(fixed) = &text {
                if let Ok(retry) = self.parse_raw(lang, path, fixed) {
                    if !retry.partial {
                        return Ok(retry);
                    }
                }
            }
            amp = text.or(attr);
        }
        if parsed.partial && matches!(lang, Language::TypeScript | Language::Tsx) {
            let source = amp.as_deref().unwrap_or(source);
            // TS 4.7 variance (`interface I<in T, out U>`): same-length blank
            // of the keywords, kept only when the retry is clean. Also the
            // base for the import-type retry so both fixes can combine.
            let variance = ts_variance::normalize_variance(source);
            if let Some(fixed) = &variance {
                if let Ok(retry) = self.parse_raw(lang, path, fixed) {
                    if !retry.partial {
                        return Ok(retry);
                    }
                }
            }
            let base = variance.as_deref().unwrap_or(source);
            // Line-leading index signature in a call's type argument
            // (`useState<{ id: number\n [key: string]: unknown }>`), see
            // `ts_index_sig.rs`; combines with the fixes before and after.
            let sig = ts_index_sig::separate_index_signatures(base);
            if let Some(fixed) = &sig {
                if let Ok(retry) = self.parse_raw(lang, path, fixed) {
                    if !retry.partial {
                        return Ok(retry);
                    }
                }
            }
            let base = sig.as_deref().unwrap_or(base);
            if let Some(fixed) = normalize_import_types(base) {
                if let Ok(retry) = self.parse_raw(lang, path, &fixed) {
                    if !retry.partial {
                        return Ok(retry);
                    }
                }
            }
        }
        Ok(parsed)
    }

    /// One tree-sitter parse + query extraction, no per-language pre-pass.
    fn parse_raw(
        &self,
        lang: Language,
        _path: &RelPath,
        source: &str,
    ) -> Result<ParsedFile, ParseError> {
        let index = language_index(lang).ok_or(ParseError::NoGrammar(lang))?;
        let idle = self.parsers[index]
            .lock()
            .map_err(|_| ParseError::Query("parser pool mutex poisoned".into()))?
            .pop();
        let parser = match idle {
            Some(parser) => parser,
            None => configured_parser(lang)?,
        };
        let mut parser = PooledParser {
            pool: &self.parsers[index],
            parser: Some(parser),
        };

        // One budget covers the parse and the query passes. Tree-sitter calls
        // these callbacks from inside the synchronous work, so expiration
        // really interrupts it. There is no racing timer thread.
        let deadline = Instant::now() + PARSE_BUDGET;
        let mut timed_out = false;
        let tree = {
            let mut progress = |_: &tree_sitter::ParseState| {
                if Instant::now() >= deadline {
                    timed_out = true;
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
            let bytes = source.as_bytes();
            parser.parse_with_options(&mut |offset, _| &bytes[offset..], None, Some(options))
        };
        // Return the parser (reset on drop) before the query phase.
        drop(parser);

        let tree = match tree {
            Some(tree) => tree,
            None => {
                return Err(if timed_out {
                    ParseError::Timeout
                } else {
                    ParseError::NoTree
                });
            }
        };

        let compiled = compiled_queries(lang)?;
        QUERY_DEADLINE.with(|d| d.set(Some(deadline)));
        QUERY_TIMED_OUT.with(|f| f.set(false));
        let symbols = extract_symbols(lang, &compiled.symbols, tree.root_node(), source);
        let imports = extract_imports(lang, &compiled.imports, tree.root_node(), source);
        let calls = extract_calls_for(lang, &compiled.calls, tree.root_node(), source);
        let query_timed_out = QUERY_TIMED_OUT.with(|f| f.replace(false));
        QUERY_DEADLINE.with(|d| d.set(None));
        if query_timed_out {
            return Err(ParseError::Timeout);
        }
        let partial = tree.root_node().has_error();

        // `tree`, all Nodes, queries, and cursors are local and drop here.
        // ParsedFile contains only owned strings and scalar line numbers.
        Ok(ParsedFile {
            symbols,
            imports,
            calls,
            partial,
            c_family_rewrite: false,
        })
    }
}

/// Replace `type` with spaces in statement-leading `export type * [as ns] from`
/// (TS 5.0 syntax). Byte offsets, lines and columns are unchanged. Returns
/// `None` when nothing matches.
fn normalize_export_type_star(source: &str) -> Option<String> {
    if !source.contains("export type") {
        return None;
    }
    let mut out: Option<String> = None;
    let mut pos = 0usize;
    for line in source.split_inclusive('\n') {
        let lead = line.len() - line.trim_start().len();
        let rest = &line[lead..];
        if let Some(after) = rest.strip_prefix("export") {
            let after_ws = after.trim_start();
            let ws = after.len() - after_ws.len();
            if ws > 0 {
                if let Some(tail) = after_ws.strip_prefix("type") {
                    let t = tail.trim_start();
                    if t.starts_with('*') {
                        let at = pos + lead + "export".len() + ws;
                        let buf = out.get_or_insert_with(|| source.to_owned());
                        buf.replace_range(at..at + 4, "    ");
                    }
                }
            }
        }
        pos += line.len();
    }
    out
}

/// Replace type-position `import('lit')` (preceded by `typeof`, `<`, `|` or
/// `&`) with `_` of the same byte length so it reads as a type name.
/// Offsets are unchanged. Calls after `await`, `=`, `(`, `,` etc. are left
/// alone. `None` when nothing matches.
fn normalize_import_types(source: &str) -> Option<String> {
    let b = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let mut from = 0usize;
    while let Some(off) = source[from..].find("import") {
        let at = from + off;
        from = at + 6;
        if at > 0
            && (b[at - 1].is_ascii_alphanumeric()
                || b[at - 1] == b'_'
                || b[at - 1] == b'$'
                || b[at - 1] == b'.')
        {
            continue;
        }
        let mut j = at + 6;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b'(') {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let Some(&q) = b.get(j) else { continue };
        if !matches!(q, b'\'' | b'"') {
            continue;
        }
        j += 1;
        while j < b.len() && b[j] != q && b[j] != b'\n' && b[j] != b'\\' {
            j += 1;
        }
        if b.get(j) != Some(&q) {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b')') {
            continue;
        }
        let end = j + 1;
        let before = source[..at].trim_end();
        let typed = before.ends_with('<')
            || before.ends_with('|')
            || before.ends_with('&')
            || before.strip_suffix("typeof").is_some_and(|r| {
                !r.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$')
            });
        if !typed {
            continue;
        }
        let buf = out.get_or_insert_with(|| b.to_vec());
        for x in &mut buf[at..end] {
            *x = b'_';
        }
        from = end;
    }
    out.and_then(|v| String::from_utf8(v).ok())
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

/// The three compiled queries of one language. `Query` is immutable after
/// construction and `Send + Sync`, so one copy per language is shared by all
/// parse calls instead of recompiling the `.scm` sources for every file.
struct CompiledQueries {
    symbols: Query,
    imports: Query,
    calls: Query,
}

fn compiled_queries(lang: Language) -> Result<&'static CompiledQueries, ParseError> {
    static CACHE: [OnceLock<Result<CompiledQueries, String>>; LANGUAGES.len()] =
        [const { OnceLock::new() }; LANGUAGES.len()];
    let index = language_index(lang).ok_or(ParseError::NoGrammar(lang))?;
    CACHE[index]
        .get_or_init(|| {
            let sources =
                queries::for_language(lang).ok_or_else(|| format!("no queries for {lang:?}"))?;
            let grammar = grammar(lang).ok_or_else(|| format!("no grammar for {lang:?}"))?;
            let compile = |source: &str| {
                Query::new(&grammar, source).map_err(|error| match query_error(error) {
                    ParseError::Query(message) => message,
                    other => other.to_string(),
                })
            };
            Ok(CompiledQueries {
                symbols: compile(sources.symbols)?,
                imports: compile(sources.imports)?,
                calls: compile(sources.calls)?,
            })
        })
        .as_ref()
        .map_err(|message| ParseError::Query(message.clone()))
}

fn language_index(lang: Language) -> Option<usize> {
    Some(match lang {
        Language::Python => 0,
        Language::Go => 1,
        Language::Java => 2,
        Language::Rust => 3,
        Language::TypeScript => 4,
        Language::Tsx => 5,
        Language::JavaScript => 6,
        Language::Php => 7,
        Language::C => 8,
        Language::Cpp => 9,
        Language::Dart => 10,
        Language::CSharp => 11,
        Language::VisualBasic => 12,
        Language::ObjC => 13,
        Language::ObjCpp => 14,
        Language::Swift => 15,
        Language::Kotlin => 16,
        Language::Shell => 17,
        Language::Ruby => 18,
        Language::Lua => 19,
        Language::Vue => return None,
    })
}

/// Extra parse-cache key material for extractors whose output depends on
/// the path, not just the content (Vue SFC component name = file stem).
pub(crate) fn path_cache_salt(lang: Language, path: &RelPath) -> Option<String> {
    match lang {
        Language::Vue => Some(vue::component_name(path)),
        _ => None,
    }
}

pub(crate) fn grammar(lang: Language) -> Option<tree_sitter::Language> {
    Some(match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Dart => tree_sitter_dart::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Language::VisualBasic => tree_sitter_vb_dotnet::LANGUAGE.into(),
        Language::ObjC => tree_sitter_objc::LANGUAGE.into(),
        Language::ObjCpp => tree_sitter_objc::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        Language::Shell => tree_sitter_bash::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Language::Lua => tree_sitter_lua::LANGUAGE.into(),
        Language::Vue => return None,
    })
}

fn configured_parser(lang: Language) -> Result<Parser, ParseError> {
    let language = grammar(lang).ok_or(ParseError::NoGrammar(lang))?;
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| ParseError::NoGrammar(lang))?;
    Ok(parser)
}

/// True when `source` ends in a C-family preprocessor line with no newline.
fn needs_eof_newline(lang: Language, source: &str) -> bool {
    matches!(
        lang,
        Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::CSharp
            | Language::VisualBasic
    ) && !source.is_empty()
        && !source.ends_with('\n')
        && source
            .rsplit('\n')
            .next()
            .is_some_and(|last| last.trim_start().starts_with('#'))
}

/// Locate the first syntax error of `source` for diagnostics: `(1-based line,
/// description)`. Best effort, see [`diagnose_syntax`] (which also gives the
/// error kind): `None` for languages without a plain grammar (Vue) or when
/// the raw parse is clean.
pub fn first_syntax_error(lang: Language, source: &str) -> Option<(u32, String)> {
    diagnose_syntax(lang, source).map(|d| (d.line, d.what))
}

fn query_error(error: tree_sitter::QueryError) -> ParseError {
    ParseError::Query(error.to_string())
}

fn extract_symbols(
    lang: Language,
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
) -> Vec<CodeSymbol> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut progress = query_progress();
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source.as_bytes(),
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    let mut symbols = Vec::new();
    // Built lazily (only annotated Java/C#/VB/Swift symbols need it) so
    // `signature_from_row` is O(1) per symbol instead of O(rows).
    let mut rows: Option<Vec<&str>> = None;

    while let Some(query_match) = matches.next() {
        let definition = query_match.captures().iter().find_map(|capture| {
            let capture_name = capture_names[capture.index as usize];
            kind_for_definition_capture(capture_name).map(|kind| (capture.node, kind))
        });
        let Some((definition_node, kind)) = definition else {
            continue;
        };
        let name_node = query_match
            .captures()
            .iter()
            .find(|capture| capture_names[capture.index as usize] == queries::NAME)
            .map(|capture| capture.node)
            .unwrap_or(definition_node);
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if matches!(lang, Language::TypeScript | Language::Tsx)
            && queries::ts_inline_object_member(definition_node)
        {
            // Inline object type (parameter annotation, cast, generic arg).
            continue;
        }
        let (name, kind) = if lang == Language::CSharp {
            csharp_symbol_name(definition_node, name, kind, source)
        } else if lang == Language::Dart && definition_node.kind() == "operator_signature" {
            (dart_operator_name(definition_node, source), kind)
        } else {
            match lang {
                Language::Swift => swift::symbol_name(definition_node, name, kind, source),
                Language::ObjC | Language::ObjCpp => {
                    objc::symbol_name(definition_node, name, kind, source)
                }
                Language::Kotlin => kotlin::symbol_name(definition_node, name, kind, source),
                Language::Shell => shell::symbol_name(definition_node, name, kind, source),
                Language::Ruby => ruby::symbol_name(definition_node, name, kind, source),
                Language::Lua => lua::symbol_name(definition_node, name_node, name, kind, source),
                Language::Dart => (name.to_owned(), dart::refine_kind(definition_node, kind)),
                _ => (name.to_owned(), kind),
            }
        };

        // Java/C#/VB 的注解/特性（`@Override`、`[HttpGet]`、`<Fact>`）属于
        // 定义节点，但锚点与签名应落在声明名字所在行，否则 find_symbol
        // 给出的是注解行而非真正的声明。
        let def_row = definition_node.start_position().row;
        let name_row = name_node.start_position().row;
        let anchor_on_name = matches!(
            lang,
            Language::Java | Language::CSharp | Language::VisualBasic | Language::Swift
        ) && name_row > def_row;
        let (sig, start_row) = if anchor_on_name {
            {
                let rows = rows.get_or_insert_with(|| source.lines().collect());
                (
                    signature_from_line(rows.get(name_row).copied().unwrap_or_default()),
                    name_row,
                )
            }
        } else {
            (
                signature(definition_node, source),
                decor::start_row(definition_node).map_or(def_row, |r| r.min(def_row)),
            )
        };
        let exported = is_exported(lang, &name, definition_node, source);
        symbols.push(CodeSymbol {
            // Stable IDs are assigned by the graph builder after all files
            // have been parsed; this layer has no FileId allocation context.
            id: SymbolId(0),
            file: FileId(0),
            name,
            kind,
            signature: sig,
            start_line: line_number(start_row),
            end_line: line_number(definition_node.end_position().row),
            exported,
        });
    }
    symbols
}

fn extract_imports(
    lang: Language,
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
) -> Vec<String> {
    // Other languages keep the raw specifier. C# must emit the forms
    // `resolvers/csharp.rs` documents (`static:`, `alias:`, or a bare
    // namespace name) or namespace usings and type usings collapse together.
    if lang == Language::Ruby {
        return extract_ruby_imports(query, root, source);
    }
    if lang == Language::Php {
        return extract_php_imports(query, root, source);
    }
    if lang != Language::CSharp && lang != Language::VisualBasic {
        // C/C++ keep the brackets of `<x.h>` so the resolver can tell a
        // system include from a quoted one; `"x.h"` arrives as bare `x.h`.
        let keep_angle = matches!(lang, Language::C | Language::Cpp);
        return extract_named_captures(query, root, source, |name| {
            name == "import" || name == "source" || name == "module"
        })
        .into_iter()
        .map(|(text, _)| {
            let t = text.trim();
            if keep_angle && t.len() >= 2 && t.starts_with('<') && t.ends_with('>') {
                t.to_owned()
            } else {
                unquote(t).to_owned()
            }
        })
        .collect();
    }

    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut progress = query_progress();
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source.as_bytes(),
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    let mut output = Vec::new();
    while let Some(query_match) = matches.next() {
        let mut import_text: Option<String> = None;
        let mut is_static = false;
        let mut is_alias = false;
        for capture in query_match.captures() {
            let name = capture_names[capture.index as usize];
            let Ok(text) = capture.node.utf8_text(source.as_bytes()) else {
                continue;
            };
            match name {
                "import" => import_text = Some(unquote(text.trim()).to_owned()),
                "import.static" => is_static = true,
                "import.alias" => is_alias = true,
                _ => {}
            }
        }
        let Some(text) = import_text else {
            continue;
        };
        let text = text.strip_prefix("global::").unwrap_or(&text);
        let spec = if is_static {
            format!("static:{text}")
        } else if is_alias {
            format!("alias:{text}")
        } else {
            text.to_owned()
        };
        output.push(spec);
    }
    output
}

/// Ruby: `require_relative "x"` is emitted as `relative:x` so the resolver
/// resolves it against the requiring file only — never a same-named file on
/// the `$LOAD_PATH` (`lib/x.rb`). `require` / `load` / `autoload` stay raw.
/// PHP: the query supplies include/require literals. `use` declarations are
/// walked here instead so grouped `use A\\{B, C as D}` yields the full
/// `A\\B`, `A\\C` (the query alone would only see the bare `B`, `C`), and a
/// leading `\\` is dropped.
fn extract_php_imports(query: &Query, root: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let mut output: Vec<String> =
        extract_named_captures(query, root, source, |name| name == "import")
            .into_iter()
            .filter_map(|(text, _)| {
                // Skipped here: `use` names, collected by the walk below.
                let t = unquote(text.trim());
                (!t.is_empty()).then(|| t.to_owned())
            })
            .collect();
    // Drop the query's bare `use` names; keep only include/require literals.
    let mut uses = Vec::new();
    collect_php_uses(root, source, &mut uses);
    let mut bare = Vec::new();
    collect_php_use_nodes(root, source, &mut bare);
    output.retain(|t| !bare.contains(t));
    output.extend(uses);
    collect_php_dir_includes(root, source, &mut output);
    output
}

/// Number of directory levels above the including file that `node` denotes:
/// `__DIR__` / `dirname(__FILE__)` = 0, `dirname(X)` = levels(X)+1,
/// `dirname(X, n)` = levels(X)+n. Anything else is unresolvable.
fn php_dir_levels(node: tree_sitter::Node<'_>, source: &str) -> Option<usize> {
    let text = |n: tree_sitter::Node<'_>| n.utf8_text(source.as_bytes()).ok();
    match node.kind() {
        "parenthesized_expression" => php_dir_levels(node.named_child(0)?, source),
        "name" | "constant" | "magic_constant" | "relative_scope" => {
            text(node)?.eq_ignore_ascii_case("__DIR__").then_some(0)
        }
        "function_call_expression" => {
            let f = node.child_by_field_name("function")?;
            if !text(f)?
                .trim_start_matches('\\')
                .eq_ignore_ascii_case("dirname")
            {
                return None;
            }
            let args = node.child_by_field_name("arguments")?;
            let mut c = args.walk();
            let list: Vec<_> = args.named_children(&mut c).collect();
            fn arg(a: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
                a.named_child(0).unwrap_or(a)
            }
            let first = arg(*list.first()?);
            let n = match list.get(1) {
                None => 1,
                Some(a) => text(arg(*a))?.trim().parse::<usize>().ok()?,
            };
            if list.len() > 2 || n == 0 {
                return None;
            }
            let base = if first.kind() == "function_call_expression"
                || first.kind() == "parenthesized_expression"
            {
                php_dir_levels(first, source)?
            } else if text(first)?.eq_ignore_ascii_case("__FILE__") {
                return Some(n - 1);
            } else {
                php_dir_levels(first, source)?
            };
            Some(base + n)
        }
        _ => None,
    }
}

/// `require __DIR__ . '/x.php'` and `dirname(__DIR__, 2) . '/x.php'`: emitted as
/// `/` + `../` * levels + path, so `resolve_path` joins it to the file's dir.
/// Left operands that cannot be evaluated (variables, constants) are dropped
/// rather than guessed.
fn collect_php_dir_includes(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression"
    ) {
        let mut arg = node.named_child(0);
        while let Some(a) = arg.filter(|a| a.kind() == "parenthesized_expression") {
            arg = a.named_child(0);
        }
        if let Some(bin) = arg.filter(|a| a.kind() == "binary_expression") {
            let is_dot = bin
                .child_by_field_name("operator")
                .and_then(|o| o.utf8_text(source.as_bytes()).ok())
                == Some(".");
            let (l, r) = (
                bin.child_by_field_name("left"),
                bin.child_by_field_name("right"),
            );
            if let (true, Some(l), Some(r)) = (is_dot, l, r) {
                if let (Some(levels), true) = (
                    php_dir_levels(l, source),
                    matches!(r.kind(), "string" | "encapsed_string"),
                ) {
                    if let Ok(t) = r.utf8_text(source.as_bytes()) {
                        let t = unquote(t.trim()).trim_start_matches('/');
                        if !t.is_empty() && !t.contains('$') {
                            out.push(format!("/{}{t}", "../".repeat(levels)));
                        }
                    }
                }
            }
        }
        return;
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        collect_php_dir_includes(ch, source, out);
    }
}

fn collect_php_use_nodes(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<String>) {
    if node.kind() == "namespace_use_clause" {
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if matches!(ch.kind(), "name" | "qualified_name") {
                if let Ok(t) = ch.utf8_text(source.as_bytes()) {
                    out.push(unquote(t.trim()).to_owned());
                }
            }
        }
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        collect_php_use_nodes(ch, source, out);
    }
}

fn php_clause_name<'a>(clause: tree_sitter::Node<'_>, source: &'a str) -> Option<&'a str> {
    let mut c = clause.walk();
    let found = clause
        .children(&mut c)
        .find(|n| matches!(n.kind(), "name" | "qualified_name"))?;
    found.utf8_text(source.as_bytes()).ok()
}

/// `function:` / `const:` when the node carries a `function` / `const` keyword
/// child (`use function A\b;`, or a clause inside `use A\{function f}`).
fn php_use_kind(node: tree_sitter::Node<'_>) -> &'static str {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "function" => return "function:",
            "const" => return "const:",
            _ => {}
        }
    }
    ""
}

fn collect_php_uses(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<String>) {
    if node.kind() == "namespace_use_declaration" {
        let mut prefix: Option<&str> = None;
        let decl_kind = php_use_kind(node);
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            match ch.kind() {
                "namespace_name" => prefix = ch.utf8_text(source.as_bytes()).ok(),
                "namespace_use_clause" => {
                    if let Some(n) = php_clause_name(ch, source) {
                        let k = [decl_kind, php_use_kind(ch)]
                            .into_iter()
                            .find(|k| !k.is_empty())
                            .unwrap_or("");
                        out.push(format!("{k}{}", n.trim_start_matches('\\')));
                    }
                }
                "namespace_use_group" => {
                    let pre = prefix.unwrap_or("").trim_matches('\\');
                    let mut g = ch.walk();
                    for cl in ch.children(&mut g) {
                        if matches!(
                            cl.kind(),
                            "namespace_use_clause" | "namespace_use_group_clause"
                        ) {
                            if let Some(n) = php_clause_name(cl, source) {
                                let n = n.trim_start_matches('\\');
                                let k = [decl_kind, php_use_kind(cl)]
                                    .into_iter()
                                    .find(|k| !k.is_empty())
                                    .unwrap_or("");
                                out.push(if pre.is_empty() {
                                    format!("{k}{n}")
                                } else {
                                    format!("{k}{pre}\\{n}")
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return;
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        collect_php_uses(ch, source, out);
    }
}

fn extract_ruby_imports(query: &Query, root: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut progress = query_progress();
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source.as_bytes(),
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    let mut output = Vec::new();
    while let Some(query_match) = matches.next() {
        let mut import_text: Option<&str> = None;
        let mut relative = false;
        for capture in query_match.captures() {
            let Ok(text) = capture.node.utf8_text(source.as_bytes()) else {
                continue;
            };
            match capture_names[capture.index as usize] {
                "import" => import_text = Some(text),
                "_m" => relative = text == "require_relative",
                _ => {}
            }
        }
        let Some(text) = import_text else {
            continue;
        };
        let text = unquote(text.trim());
        output.push(if relative {
            format!("{}{text}", crate::resolvers::ruby::RELATIVE_PREFIX)
        } else {
            text.to_owned()
        });
    }
    output
}

fn extract_calls_for(
    lang: Language,
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
) -> Vec<(String, u32)> {
    let wanted = |name: &str| name == "call" || name == "name" || name == "reference.call";
    if !matches!(lang, Language::ObjC | Language::ObjCpp) {
        return extract_named_captures(query, root, source, wanted);
    }
    // ObjC: name message sends by full selector (`setObject:forKey:`) so call
    // names line up with method symbols.
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut progress = query_progress();
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source.as_bytes(),
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    let mut output = Vec::new();
    while let Some(query_match) = matches.next() {
        let call_node = query_match
            .captures()
            .iter()
            .find(|c| capture_names[c.index as usize] == "reference.call")
            .map(|c| c.node);
        for capture in query_match.captures() {
            let capture_name = capture_names[capture.index as usize];
            let redundant_call =
                capture_name == "reference.call" && query_match.captures().len() > 1;
            if !wanted(capture_name) || redundant_call {
                continue;
            }
            let Ok(text) = capture.node.utf8_text(source.as_bytes()) else {
                continue;
            };
            let name = match call_node {
                Some(call) => objc::call_name(call, text, source),
                None => text.to_owned(),
            };
            output.push((name, line_number(capture.node.start_position().row)));
        }
    }
    output
}

fn extract_named_captures(
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
    wanted: impl Fn(&str) -> bool,
) -> Vec<(String, u32)> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut progress = query_progress();
    let mut matches = cursor.matches_with_options(
        query,
        root,
        source.as_bytes(),
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    let mut output = Vec::new();
    // Complete matches (not `captures()`, which may yield a match before its
    // later captures are known) so the whole-call capture can be recognised.
    while let Some(query_match) = matches.next() {
        // `@call` / `@reference.call` capture the whole call expression
        // (`h(a, b)`), the callee name is `@name` in the same match. Storing
        // both doubles the table and keeps argument text; skip the whole-call
        // capture whenever the match also names the callee.
        let names_callee = query_match
            .captures()
            .iter()
            .any(|c| capture_names[c.index as usize] == queries::NAME);
        for capture in query_match.captures() {
            let capture_name = capture_names[capture.index as usize];
            if !wanted(capture_name)
                || (names_callee && matches!(capture_name, "call" | "reference.call"))
            {
                continue;
            }
            if let Ok(text) = capture.node.utf8_text(source.as_bytes()) {
                output.push((
                    text.to_owned(),
                    line_number(capture.node.start_position().row),
                ));
            }
        }
    }
    output
}

/// Map a `@definition.<kind>` capture to [`SymbolKind`].
///
/// Prefer [`queries::symbol_kind_for_capture`] so this layer stays aligned with
/// the query pack. Field / Variable (and any other kind the pack has not yet
/// wired) are filled in locally so a concurrent query update cannot drop the
/// new categories on the floor.
/// Dart `operator_signature` has no name field (`operator []` is two
/// anonymous tokens), so render `operator <tokens>` from the children between
/// the `operator` keyword and the parameter list.
fn dart_operator_name(node: tree_sitter::Node<'_>, source: &str) -> String {
    let mut cursor = node.walk();
    let mut seen_keyword = false;
    let mut op = String::new();
    for child in node.children(&mut cursor) {
        if child.kind() == "formal_parameter_list" {
            break;
        }
        if seen_keyword {
            op.push_str(child.utf8_text(source.as_bytes()).unwrap_or("").trim());
        } else if child.kind() == "operator" {
            seen_keyword = true;
        }
    }
    format!("operator {op}")
}

/// C# members whose identifier alone is not the symbol name language
/// servers report (`operator +`, `implicit operator string`), and `const`
/// fields, which the query reports as plain fields.
fn csharp_symbol_name(
    node: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    match node.kind() {
        "operator_declaration" => (format!("operator {name}"), kind),
        "conversion_operator_declaration" => {
            let mut cursor = node.walk();
            let direction = node
                .children(&mut cursor)
                .find(|child| matches!(child.kind(), "implicit" | "explicit"))
                .map_or("", |child| child.kind());
            (
                format!("{direction} operator {name}").trim().to_owned(),
                kind,
            )
        }
        "field_declaration" => {
            let mut cursor = node.walk();
            let is_const = node.children(&mut cursor).any(|child| {
                child.kind() == "modifier"
                    && child
                        .utf8_text(source.as_bytes())
                        .is_ok_and(|t| t == "const")
            });
            let kind = if is_const { SymbolKind::Const } else { kind };
            (name.to_owned(), kind)
        }
        _ => (name.to_owned(), kind),
    }
}

fn kind_for_definition_capture(capture: &str) -> Option<SymbolKind> {
    if let Some(kind) = queries::symbol_kind_for_capture(capture) {
        return Some(kind);
    }
    match capture.strip_prefix(queries::DEFINITION_PREFIX)? {
        "field" | "property" | "enum_variant" => Some(SymbolKind::Field),
        "variable" | "var" => Some(SymbolKind::Variable),
        "macro" => Some(SymbolKind::Function),
        _ => None,
    }
}

fn is_exported(lang: Language, name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    match lang {
        Language::Go => name.chars().next().is_some_and(char::is_uppercase),
        Language::Python => python_exported(name),
        Language::Java => java_exported(node),
        Language::Rust => rust_exported(node, source),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            js_exported(name, node, source)
        }
        Language::Php => php_exported(node, source),
        Language::Dart => !name.starts_with('_'),
        // Sibling P0 languages: treat as exported until their agents land visibility rules.
        Language::Swift => swift::exported(name, node, source),
        Language::ObjC | Language::ObjCpp => objc::exported(name, node, source),
        Language::Kotlin => kotlin::exported(name, node, source),
        Language::Shell => shell::exported(name, node, source),
        Language::Ruby => ruby::exported(name, node, source),
        Language::Lua => lua::exported(name, node, source),
        Language::C | Language::Cpp | Language::Vue | Language::CSharp | Language::VisualBasic => {
            true
        }
    }
}

fn php_exported(node: tree_sitter::Node<'_>, source: &str) -> bool {
    // Walk up to a declaration that may carry a visibility modifier.
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            "method_declaration"
            | "property_declaration"
            | "const_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition" => {
                if let Ok(text) = n.utf8_text(source.as_bytes()) {
                    // First keyword tokens: private is never exported; protected
                    // stays internal to the inheritance hierarchy.
                    let head = text.split('{').next().unwrap_or(text);
                    if head
                        .split_whitespace()
                        .any(|t| t == "private" || t == "protected")
                    {
                        return false;
                    }
                }
                return true;
            }
            _ => cur = n.parent(),
        }
    }
    true
}

fn python_exported(name: &str) -> bool {
    let dunder = name.len() >= 4 && name.starts_with("__") && name.ends_with("__");
    dunder || !name.starts_with('_')
}

fn java_exported(node: tree_sitter::Node<'_>) -> bool {
    let item = java_defining_item(node);
    if let Some(exported) = java_modifiers_exported(item) {
        return exported;
    }
    if item.kind() == "enum_constant" || has_ancestor_kind(item, "enum_constant") {
        return true;
    }
    java_inside_interface(item)
}

fn java_defining_item(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
        "annotation_type_declaration",
        "method_declaration",
        "constructor_declaration",
        "compact_constructor_declaration",
        "field_declaration",
        "constant_declaration",
        "enum_constant",
        "variable_declarator",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            if node.kind() == "variable_declarator" {
                if let Some(parent) = node.parent() {
                    if matches!(
                        parent.kind(),
                        "field_declaration" | "constant_declaration" | "local_variable_declaration"
                    ) {
                        return parent;
                    }
                }
            }
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn java_modifiers_exported(node: tree_sitter::Node<'_>) -> Option<bool> {
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if child.kind() != "modifiers" {
            continue;
        }
        let mut saw_public = false;
        for j in 0..child.child_count() {
            match child.child(j).map(|c| c.kind()) {
                Some("private") | Some("protected") => return Some(false),
                Some("public") => saw_public = true,
                _ => {}
            }
        }
        return Some(saw_public);
    }
    None
}

fn java_inside_interface(node: tree_sitter::Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "interface_declaration" | "annotation_type_declaration" => return true,
            "class_declaration" | "enum_declaration" | "record_declaration" => return false,
            _ => current = parent,
        }
    }
    false
}

fn rust_exported(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let item = rust_defining_item(node);
    match item.kind() {
        "field_declaration" => rust_has_pub(item, source),
        "enum_variant" => {
            rust_enclosing_kind(item, "enum_item")
                .map(|enum_item| rust_has_pub(enum_item, source))
                .unwrap_or(false)
                || rust_has_pub(item, source)
        }
        "function_item" | "function_signature_item" => {
            rust_has_pub(item, source)
                || rust_enclosing_kind(item, "trait_item")
                    .is_some_and(|trait_item| rust_has_pub(trait_item, source))
        }
        _ => rust_has_pub(item, source),
    }
}

fn rust_defining_item(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "function_item",
        "function_signature_item",
        "struct_item",
        "enum_item",
        "union_item",
        "trait_item",
        "impl_item",
        "type_item",
        "const_item",
        "static_item",
        "mod_item",
        "macro_definition",
        "field_declaration",
        "enum_variant",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn rust_has_pub(node: tree_sitter::Node<'_>, source: &str) -> bool {
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if child.kind() == "visibility_modifier" {
            return child
                .utf8_text(source.as_bytes())
                .map(|text| text.starts_with("pub"))
                .unwrap_or(false);
        }
    }
    false
}

fn rust_enclosing_kind<'a>(
    mut node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn js_exported(name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    if name.starts_with('#') {
        return false;
    }
    let member = js_member_node(node);
    if js_private_member(member, source) {
        return false;
    }
    if has_ancestor_kind(node, "export_statement") {
        return true;
    }
    js_commonjs_export(node, source)
}

fn js_member_node(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "method_definition",
        "method_signature",
        "abstract_method_signature",
        "public_field_definition",
        "field_definition",
        "property_signature",
        "function_declaration",
        "function_signature",
        "generator_function_declaration",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "internal_module",
        "module",
        "lexical_declaration",
        "variable_declaration",
        "variable_declarator",
        "assignment_expression",
        "export_statement",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn js_private_member(node: tree_sitter::Node<'_>, source: &str) -> bool {
    if node.kind() == "private_property_identifier" {
        return true;
    }
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if child.kind() == "private_property_identifier" {
            return true;
        }
        if child.kind() == "accessibility_modifier" {
            let hidden = child
                .child(0)
                .is_some_and(|c| matches!(c.kind(), "private" | "protected"))
                || child
                    .utf8_text(source.as_bytes())
                    .map(|text| text == "private" || text == "protected")
                    .unwrap_or(false);
            if hidden {
                return true;
            }
        }
    }
    false
}

fn js_commonjs_export(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "assignment_expression" {
            if let Some(left) = n.child_by_field_name("left") {
                if let Ok(text) = left.utf8_text(source.as_bytes()) {
                    let text = text.trim();
                    if text.starts_with("module.exports") || text.starts_with("exports.") {
                        return true;
                    }
                }
            }
        }
        current = n.parent();
    }
    false
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn line_number(zero_based_row: usize) -> u32 {
    u32::try_from(zero_based_row)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

fn signature(node: tree_sitter::Node<'_>, source: &str) -> String {
    let text = node.utf8_text(source.as_bytes()).unwrap_or_default();
    let first_line = text.split(['\n', '\r']).next().unwrap_or_default();
    let before_body = first_line.split('{').next().unwrap_or(first_line).trim();
    let mut result: String = before_body.chars().take(MAX_SIGNATURE_CHARS).collect();
    if before_body.chars().count() > MAX_SIGNATURE_CHARS {
        result.push('…');
    }
    result
}

/// 取源码第 `row` 行（0-based）在 `{` 之前的部分作为签名。
fn signature_from_row(source: &str, row: usize) -> String {
    signature_from_line(source.lines().nth(row).unwrap_or_default())
}

fn signature_from_line(line: &str) -> String {
    let before_body = line.split('{').next().unwrap_or(line).trim();
    let mut result: String = before_body.chars().take(MAX_SIGNATURE_CHARS).collect();
    if before_body.chars().count() > MAX_SIGNATURE_CHARS {
        result.push('…');
    }
    result
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        // Quotes for ordinary string literals; angle brackets for C/C++
        // `#include <…>` (`system_lib_string` nodes include the brackets).
        if matches!(
            (bytes[0], bytes[value.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"') | (b'`', b'`') | (b'<', b'>')
        ) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn annotated_symbols_anchor_on_name_row_with_signature() {
        let pool = ParserPool::new();
        let src = "class A {\n  @Override\n  public void m() {}\n  @Deprecated\n  @X\n  void n(int a) {}\n}\n";
        let p = pool
            .parse(Language::Java, &RelPath::new("A.java"), src)
            .expect("java parses");
        let m = p.symbols.iter().find(|s| s.name == "m").expect("m");
        assert_eq!(
            (m.start_line, m.signature.as_str()),
            (3, "public void m() "[..15].trim())
        );
        let n = p.symbols.iter().find(|s| s.name == "n").expect("n");
        assert_eq!((n.start_line, n.signature.as_str()), (6, "void n(int a)"));
    }

    /// Perf probe (ignored): `cargo test -p astrolabe-core --lib parse::tests::perf_probe --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_probe() {
        use std::hash::{Hash, Hasher};
        let pool = ParserPool::new();
        let mut java = String::from("class A {\n");
        for i in 0..20000 {
            java.push_str(&format!("  @Override\n  public void m{i}() {{}}\n"));
        }
        java.push_str("}\n");
        let mut sh = String::from("if [[ ( ]]; then\n");
        for i in 0..3000 {
            sh.push_str(&format!("f{i}() {{\n  echo {i}\n}}\n"));
        }
        for (lang, path, src) in [
            (Language::Java, "A.java", &java),
            (Language::Shell, "a.sh", &sh),
        ] {
            let t = std::time::Instant::now();
            let p = pool.parse(lang, &RelPath::new(path), src).expect("parses");
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for s in &p.symbols {
                (&s.name, &s.signature, s.start_line, s.end_line).hash(&mut h);
            }
            eprintln!(
                "PERF {path}: {} symbols, {:?}, digest {:x}",
                p.symbols.len(),
                t.elapsed(),
                h.finish()
            );
        }
    }

    #[test]
    fn cpp_destructors_and_nested_namespaces_are_symbols() {
        let pool = ParserPool::new();
        let src = "namespace a::b {\nclass X {\n public:\n  ~X();\n};\nX::~X() {}\n}\n";
        let parsed = pool
            .parse(Language::Cpp, &RelPath::new("x.cpp"), src)
            .expect("cpp parses");
        let got: Vec<(&str, u32)> = parsed
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.start_line))
            .collect();
        assert!(got.contains(&("a::b", 1)), "{got:?}");
        assert!(got.contains(&("~X", 4)), "{got:?}");
        assert!(got.contains(&("~X", 6)), "{got:?}");
    }

    #[test]
    fn objc_message_sends_use_full_selector_and_headers_sniff_objc() {
        let pool = ParserPool::new();
        let src = "void f(NSMutableDictionary *d) {\n  [d setObject:@1 forKey:@\"k\"];\n  [d removeAllObjects];\n  g(1);\n}\n";
        let parsed = pool
            .parse(Language::ObjC, &RelPath::new("a.m"), src)
            .expect("objc parses");
        let names: Vec<&str> = parsed.calls.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"setObject:forKey:"), "{names:?}");
        assert!(names.contains(&"removeAllObjects"), "{names:?}");
        assert!(names.contains(&"g"), "{names:?}");

        let h = RelPath::new("A.h");
        let objc = "#import <Foundation/Foundation.h>\n@interface A : NSObject\n@end\n";
        assert_eq!(
            Language::from_path_and_source(&h, objc),
            Some(Language::ObjC)
        );
        let cpp = "#pragma once\nnamespace n { class C {}; }\n";
        assert_eq!(Language::from_path_and_source(&h, cpp), Some(Language::Cpp));
        let c = "#ifndef A_H\n#define A_H\nint a(void);\n#endif\n";
        assert_eq!(Language::from_path_and_source(&h, c), Some(Language::C));
    }

    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn parse_src(lang: Language, path: &str, src: &str) -> ParsedFile {
        ParserPool::new()
            .parse(lang, &RelPath::new(path), src)
            .unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    fn named<'a>(parsed: &'a ParsedFile, name: &str) -> &'a CodeSymbol {
        parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "missing symbol {name} in {:?}",
                    parsed
                        .symbols
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    const CSHARP_MEMBERS: &str = r#"
namespace App;

public delegate void Handler(object sender);
public enum Level { Low, High = 2 }

public partial class Money(decimal amount)
{
    private const int Scale = 2;
    private readonly decimal _amount = amount, _rounded;
    public event Handler Changed;
    public event Handler Closed { add { } remove { } }
    public decimal this[int index] => _amount;
    public static Money operator +(Money a, Money b) => a;
    public static implicit operator decimal(Money m) => m._amount;
    ~Money() { }
    void IDisposable.Dispose() { }
    public int Run()
    {
        int Twice(int x) => x * 2;
        return Twice(1);
    }
}
"#;

    #[test]
    fn csharp_members_fields_events_operators_and_local_functions() {
        let parsed = parse_src(Language::CSharp, "Money.cs", CSHARP_MEMBERS);
        for (name, kind) in [
            ("Handler", SymbolKind::Type),
            ("Low", SymbolKind::Const),
            ("High", SymbolKind::Const),
            ("Scale", SymbolKind::Const),
            ("_amount", SymbolKind::Field),
            ("_rounded", SymbolKind::Field),
            ("Changed", SymbolKind::Field),
            ("Closed", SymbolKind::Field),
            ("this", SymbolKind::Method),
            ("operator +", SymbolKind::Method),
            ("implicit operator decimal", SymbolKind::Method),
            ("Dispose", SymbolKind::Method),
            ("Twice", SymbolKind::Function),
        ] {
            assert_eq!(named(&parsed, name).kind, kind, "{name}");
        }
        assert_eq!(
            parsed.symbols.iter().filter(|s| s.name == "Money").count(),
            2,
            "class + destructor"
        );
    }

    #[test]
    fn csharp_edited_file_reparse_sees_new_members() {
        let before = parse_src(Language::CSharp, "Money.cs", CSHARP_MEMBERS);
        assert!(before.symbols.iter().all(|s| s.name != "Rate"));
        let edited = CSHARP_MEMBERS.replace(
            "private const int Scale = 2;",
            "private const int Scale = 2;\n    public static readonly decimal Rate = 1m;",
        );
        let after = parse_src(Language::CSharp, "Money.cs", &edited);
        assert_eq!(named(&after, "Rate").kind, SymbolKind::Field);
        assert_eq!(named(&after, "Scale").kind, SymbolKind::Const);
    }

    #[test]
    fn typescript_alias_members_enum_members_and_string_methods() {
        let src = "type TabBlock = { value: string; line: number };\n\
type LinkedList = null | { value: number; next: LinkedList };\n\
type Responses = { 200: string };\n\
class Std { \"~standard\": number = 1; }\n\
export enum Color { Red, Green = 2 }\n\
const bench = { \"no rest (valid)\"() {} };\n\
interface I { \"quoted-key\": string }\n\
function f(opts: { inlineParam: string }) { return (opts as { castOnly: 1 }); }\n";
        for (lang, path) in [(Language::TypeScript, "a.ts"), (Language::Tsx, "a.tsx")] {
            let parsed = parse_src(lang, path, src);
            let names: Vec<_> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
            for want in [
                "line",
                "next",
                "200",
                "~standard",
                "Red",
                "Green",
                "no rest (valid)",
                "quoted-key",
            ] {
                assert!(names.contains(&want), "{lang:?} missing {want}: {names:?}");
            }
            assert_eq!(named(&parsed, "Green").kind, SymbolKind::Field);
            assert_eq!(named(&parsed, "next").start_line, 2);
            let edited = format!("{src}type Added = {{ freshMember: string }};\n");
            let reparsed = parse_src(lang, path, &edited);
            assert!(
                reparsed.symbols.iter().any(|s| s.name == "freshMember"),
                "{lang:?}: edit then reparse"
            );
            for unwanted in ["inlineParam", "castOnly"] {
                assert!(!names.contains(&unwanted), "{lang:?} kept {unwanted}");
            }
        }
    }

    #[test]
    fn all_languages_have_compatible_parsers() {
        for language in LANGUAGES {
            assert!(
                configured_parser(language).is_ok(),
                "failed to configure the {} parser",
                language.name()
            );
        }
    }

    #[test]
    fn parser_pool_size_does_not_grow_with_file_count() {
        let pool = ParserPool::new();
        for _ in 0..100 {
            pool.parse(Language::Rust, &RelPath::new("a.rs"), "fn f() {}")
                .unwrap();
        }
        // Sequential parses reuse one parser: the idle stack stays at 1.
        let idle = pool.parsers[language_index(Language::Rust).unwrap()]
            .lock()
            .unwrap()
            .len();
        assert_eq!(idle, 1);
        assert_eq!(pool.parsers.len(), LANGUAGES.len());
    }

    #[test]
    fn same_language_parses_run_concurrently_and_pool_stays_bounded() {
        let pool = ParserPool::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        let parsed = pool
                            .parse(Language::Rust, &RelPath::new("a.rs"), "fn f() { g(1); }")
                            .unwrap();
                        assert_eq!(parsed.symbols.len(), 1);
                    }
                });
            }
        });
        let idle = pool.parsers[language_index(Language::Rust).unwrap()]
            .lock()
            .unwrap()
            .len();
        assert!((1..=4).contains(&idle), "idle parsers: {idle}");
    }

    #[test]
    fn ts_export_type_star_parses_cleanly_and_keeps_import_edges() {
        let src = "export type * from './user';\nexport type * as ns from '@v/typings';\nexport type Foo = 1;\nexport function f() {}\n";
        let pool = ParserPool::new();
        for lang in [Language::TypeScript, Language::Tsx] {
            let p = pool.parse(lang, &RelPath::new("index.ts"), src).unwrap();
            assert!(!p.partial, "{lang:?} flagged partial");
            assert!(
                p.imports.iter().any(|i| i.contains("./user")),
                "{:?}",
                p.imports
            );
            assert!(p.symbols.iter().any(|s| s.name == "f" && s.start_line == 4));
            assert!(p.symbols.iter().any(|s| s.name == "Foo"));
        }
        // String contents are untouched.
        assert!(normalize_export_type_star("const s = \"export type * x\";\n").is_none());
    }

    #[test]
    fn ts_generic_import_types_parse_cleanly_and_dynamic_import_edges_stay() {
        let src = "const m = await vi.importActual<typeof import('vue')>('vue');\nlet r: Readonly<import('vue').Ref<string>>;\nexpect(x).toEqualTypeOf<Readonly<import('vue').Ref<string>>>();\nconst lazy = () => import('./lazy');\nexport function f() {}\n";
        let pool = ParserPool::new();
        for lang in [Language::TypeScript, Language::Tsx] {
            let p = pool.parse(lang, &RelPath::new("a.ts"), src).unwrap();
            assert!(!p.partial, "{lang:?} partial");
            assert!(p.symbols.iter().any(|s| s.name == "f" && s.start_line == 5));
            assert!(
                p.imports.iter().any(|i| i.contains("./lazy")),
                "{:?}",
                p.imports
            );
        }
        let fixed = normalize_import_types(src).unwrap();
        assert_eq!(fixed.len(), src.len());
        assert!(fixed.contains("import('./lazy')"));
        assert!(!fixed.contains("import('vue')"));
        assert!(
            normalize_import_types("const a = await import('x');\nf(import('y'));\n").is_none()
        );
    }

    #[test]
    fn top_level_for_loop_counter_is_indexed_but_nested_is_not() {
        let src = "const n = 3;\nfor (let i = 0; i < n; i++) {}\nfunction f() {\n  for (let j = 0; j < 2; j++) {}\n}\n";
        let pool = ParserPool::new();
        for lang in [Language::TypeScript, Language::JavaScript] {
            let p = pool.parse(lang, &RelPath::new("a.ts"), src).unwrap();
            assert!(
                p.symbols.iter().any(|s| s.name == "i" && s.start_line == 2),
                "{lang:?}"
            );
            assert!(!p.symbols.iter().any(|s| s.name == "j"), "{lang:?}");
        }
    }

    #[test]
    fn trailing_directive_without_newline_is_not_partial() {
        let pool = ParserPool::new();
        let src = "#pragma warning disable 618\nnamespace N {\n    public class C { public void M() {} }\n}\n#pragma warning restore 618";
        let parsed = pool
            .parse(Language::CSharp, &RelPath::new("C.cs"), src)
            .unwrap();
        assert!(!parsed.partial, "EOF directive flagged the file partial");
        assert!(parsed.symbols.iter().any(|s| s.name == "M"));
        assert_eq!(first_syntax_error(Language::CSharp, src), None);
        let c = "int f(void) { return 0; }\n#endif";
        assert!(!needs_eof_newline(Language::Python, "x = 1\n# c"));
        assert!(needs_eof_newline(Language::C, c));
    }

    #[test]
    fn first_syntax_error_reports_line() {
        let (line, what) = first_syntax_error(Language::Python, "def a():\n    pass\n\ndef b(:\n")
            .expect("error located");
        assert_eq!(line, 4, "{what}");
        assert_eq!(
            first_syntax_error(Language::Python, "def a():\n    pass\n"),
            None
        );
    }

    #[test]
    fn malformed_source_sets_partial_and_clean_source_does_not() {
        let pool = ParserPool::new();
        let bad = pool
            .parse(Language::Rust, &RelPath::new("b.rs"), "fn broken( {")
            .unwrap();
        assert!(bad.partial);
        let ok = pool
            .parse(Language::Rust, &RelPath::new("ok.rs"), "fn fine() {}\n")
            .unwrap();
        assert!(!ok.partial);
        assert!(pool
            .parse(Language::Rust, &RelPath::new("empty.rs"), "")
            .is_ok());
    }

    #[test]
    fn calls_do_not_store_whole_call_text_next_to_callee_name() {
        let pool = ParserPool::new();
        for (lang, path, src, callee) in [
            (Language::JavaScript, "a.js", "function f(){ h(a); }\n", "h"),
            (
                Language::Python,
                "a.py",
                "def f():\n    os.path.join(1)\n",
                "join",
            ),
        ] {
            let parsed = pool.parse(lang, &RelPath::new(path), src).unwrap();
            assert!(
                parsed.calls.iter().any(|(n, _)| n == callee),
                "{lang:?}: {:?}",
                parsed.calls
            );
            assert!(
                parsed.calls.iter().all(|(n, _)| !n.contains('(')),
                "{lang:?} stored call text: {:?}",
                parsed.calls
            );
        }
    }

    #[test]
    fn query_phase_honours_the_deadline() {
        // An already-expired deadline must interrupt the query cursor and be
        // visible through the flag (not look like "no matches").
        let source = format!("fn f() {{ {} }}\n", "g(1); ".repeat(20_000));
        let source = source.as_str();
        let grammar = grammar(Language::Rust).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let compiled = compiled_queries(Language::Rust).unwrap();
        QUERY_DEADLINE.with(|d| d.set(Some(Instant::now() - Duration::from_secs(1))));
        QUERY_TIMED_OUT.with(|f| f.set(false));
        let _ = extract_calls_for(Language::Rust, &compiled.calls, tree.root_node(), source);
        let flagged = QUERY_TIMED_OUT.with(|f| f.replace(false));
        QUERY_DEADLINE.with(|d| d.set(None));
        assert!(flagged);
    }

    /// `cargo test -p astrolabe-core --release --lib pool_parallel_bench -- --ignored --nocapture`
    /// Parses every `.rs` file under this crate from 8 threads, once with a
    /// global lock around each parse (the old one-parser-per-language
    /// behaviour: same-language parses serialize) and once without.
    #[test]
    #[ignore = "benchmark"]
    fn pool_parallel_bench() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(std::fs::read_to_string(&p).unwrap());
                }
            }
        }
        let run = |serialize: bool| {
            let pool = ParserPool::new();
            let gate = Mutex::new(());
            let next = std::sync::atomic::AtomicUsize::new(0);
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    scope.spawn(|| loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= files.len() * 3 {
                            break;
                        }
                        let _g = serialize.then(|| gate.lock().unwrap());
                        pool.parse(
                            Language::Rust,
                            &RelPath::new("x.rs"),
                            &files[i % files.len()],
                        )
                        .unwrap();
                    });
                }
            });
            start.elapsed()
        };
        let serial = run(true);
        let parallel = run(false);
        println!(
            "files={} serialized={serial:?} parallel={parallel:?}",
            files.len()
        );
    }

    #[test]
    fn malformed_source_and_empty_source_are_recoverable() {
        let mut parser = configured_parser(Language::Rust).unwrap();
        let malformed = parser.parse("fn broken( {", None).unwrap();
        assert!(malformed.root_node().has_error());
        assert!(parser.parse("", None).is_some());
    }

    #[test]
    fn parse_recovers_from_syntax_errors_and_accepts_empty_source() {
        let pool = ParserPool::new();
        let malformed = pool
            .parse(Language::Rust, &RelPath::new("broken.rs"), "fn broken( {")
            .unwrap();
        assert!(malformed.symbols.len() <= 1);
        assert!(pool
            .parse(Language::Rust, &RelPath::new("empty.rs"), "")
            .is_ok());
    }

    #[test]
    fn symbol_lines_are_one_based() {
        let parsed = parse_src(Language::Rust, "line.rs", "\nfn second_line() {}\n");
        let symbol = named(&parsed, "second_line");
        assert_eq!(symbol.start_line, 2);
        assert_eq!(symbol.end_line, 2);
    }

    #[test]
    fn annotated_java_and_csharp_symbols_anchor_on_name_line() {
        let java = parse_src(
            Language::Java,
            "A.java",
            "class A {\n    @Override\n    @Deprecated\n    public String toString() { return \"\"; }\n}\n",
        );
        let sym = named(&java, "toString");
        assert_eq!(sym.start_line, 4);
        assert!(
            sym.signature.contains("public String toString()"),
            "{}",
            sym.signature
        );
        assert!(!sym.signature.contains('@'), "{}", sym.signature);

        let cs = parse_src(
            Language::CSharp,
            "A.cs",
            "class A {\n    [HttpGet]\n    public int Get() { return 1; }\n}\n",
        );
        let sym = named(&cs, "Get");
        assert_eq!(sym.start_line, 3);
        assert!(
            sym.signature.starts_with("public int Get()"),
            "{}",
            sym.signature
        );
    }

    #[test]
    fn new_and_existing_definition_captures_map_to_kinds() {
        assert_eq!(
            kind_for_definition_capture("definition.function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            kind_for_definition_capture("definition.macro"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            kind_for_definition_capture("definition.field"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.property"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.enum_variant"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.variable"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(
            kind_for_definition_capture("definition.var"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(kind_for_definition_capture("name"), None);
        assert_eq!(kind_for_definition_capture("_scope"), None);
        assert_eq!(kind_for_definition_capture("definition.bogus"), None);
    }

    #[test]
    fn rust_macros_are_kept_as_functions() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "macro_rules! my_macro { () => {}; }\n",
        );
        let symbol = named(&parsed, "my_macro");
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert!(!symbol.exported);
    }

    #[test]
    fn python_export_follows_leading_underscore_convention() {
        let parsed = parse_src(
            Language::Python,
            "m.py",
            "def public_fn():\n    pass\ndef _private():\n    pass\n__all__ = []\n",
        );
        assert!(named(&parsed, "public_fn").exported);
        assert!(!named(&parsed, "_private").exported);
        assert!(named(&parsed, "__all__").exported);
    }

    #[test]
    fn go_export_follows_capitalization() {
        let parsed = parse_src(
            Language::Go,
            "m.go",
            "package p\nfunc New() {}\nfunc helper() {}\n",
        );
        assert!(named(&parsed, "New").exported);
        assert!(!named(&parsed, "helper").exported);
    }

    #[test]
    fn java_export_honours_public_versus_private() {
        let parsed = parse_src(
            Language::Java,
            "Box.java",
            "public class Box {\n  public void open() {}\n  private void hide() {}\n  void pkg() {}\n}\n",
        );
        assert!(named(&parsed, "Box").exported);
        assert!(named(&parsed, "open").exported);
        assert!(!named(&parsed, "hide").exported);
        assert!(!named(&parsed, "pkg").exported);
    }

    #[test]
    fn rust_export_honours_pub_and_does_not_export_private_items() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub fn free() {}\nfn hidden() {}\npub struct Point { x: i32 }\nstruct Secret { pub y: i32 }\n",
        );
        assert!(named(&parsed, "free").exported);
        assert!(!named(&parsed, "hidden").exported);
        assert!(named(&parsed, "Point").exported);
        assert!(!named(&parsed, "Secret").exported);
    }

    #[test]
    fn typescript_export_and_private_members() {
        let parsed = parse_src(
            Language::TypeScript,
            "m.ts",
            "export function greet() {}\nfunction hidden() {}\nexport class App {\n  render() {}\n  #secret() {}\n  private hide() {}\n}\n",
        );
        assert!(named(&parsed, "greet").exported);
        assert!(!named(&parsed, "hidden").exported);
        assert!(named(&parsed, "App").exported);
        assert!(named(&parsed, "render").exported);
        assert!(!named(&parsed, "#secret").exported);
        assert!(!named(&parsed, "hide").exported);
    }

    #[test]
    fn javascript_commonjs_assignment_counts_as_exported() {
        let parsed = parse_src(
            Language::JavaScript,
            "m.js",
            "module.exports.legacy = function () {};\nfunction hidden() {}\n",
        );
        assert!(named(&parsed, "legacy").exported);
        assert!(!named(&parsed, "hidden").exported);
    }

    #[test]
    fn java_fields_carry_kind_and_visibility() {
        let parsed = parse_src(
            Language::Java,
            "Box.java",
            "public class Box {\n  public int open;\n  private int secret;\n}\n",
        );
        let open = parsed
            .symbols
            .iter()
            .find(|s| s.name == "open")
            .expect("missing Field `open` — queries 尚未发出 definition.field");
        assert_eq!(open.kind, SymbolKind::Field);
        assert!(open.exported);
        let secret = parsed
            .symbols
            .iter()
            .find(|s| s.name == "secret")
            .expect("missing Field `secret`");
        assert_eq!(secret.kind, SymbolKind::Field);
        assert!(!secret.exported);
    }

    #[test]
    fn rust_struct_fields_require_pub_to_export() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub struct Point { pub x: i32, y: i32 }\n",
        );
        let x = named(&parsed, "x");
        assert_eq!(x.kind, SymbolKind::Field);
        assert!(x.exported);
        let y = named(&parsed, "y");
        assert_eq!(y.kind, SymbolKind::Field);
        assert!(!y.exported);
    }

    #[test]
    fn rust_enum_variants_inherit_enum_visibility() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub enum Shape { Circle, Square }\nenum Hidden { A }\n",
        );
        let circle = named(&parsed, "Circle");
        assert_eq!(circle.kind, SymbolKind::Field);
        assert!(circle.exported);
        let hidden = named(&parsed, "A");
        assert_eq!(hidden.kind, SymbolKind::Field);
        assert!(!hidden.exported);
    }

    #[test]
    fn go_vars_are_variables_and_struct_fields_follow_capitalization() {
        let parsed = parse_src(
            Language::Go,
            "m.go",
            "package p\nvar Global = 1\nvar hidden = 2\ntype Point struct{ X, y int }\n",
        );
        assert_eq!(named(&parsed, "Global").kind, SymbolKind::Variable);
        assert!(named(&parsed, "Global").exported);
        assert_eq!(named(&parsed, "hidden").kind, SymbolKind::Variable);
        assert!(!named(&parsed, "hidden").exported);
        assert_eq!(named(&parsed, "X").kind, SymbolKind::Field);
        assert!(named(&parsed, "X").exported);
        assert_eq!(named(&parsed, "y").kind, SymbolKind::Field);
        assert!(!named(&parsed, "y").exported);
    }

    #[test]
    fn python_distinguishes_const_variable_and_class_field() {
        let parsed = parse_src(
            Language::Python,
            "m.py",
            "MAX = 10\nname = \"x\"\nclass Foo:\n    attr = 1\n",
        );
        assert_eq!(named(&parsed, "MAX").kind, SymbolKind::Const);
        assert_eq!(named(&parsed, "name").kind, SymbolKind::Variable);
        assert_eq!(named(&parsed, "attr").kind, SymbolKind::Field);
        assert!(named(&parsed, "attr").exported);
    }

    #[test]
    fn typescript_fields_and_variables_honour_export_and_private() {
        let parsed = parse_src(
            Language::TypeScript,
            "m.ts",
            "export let counter = 0;\nlet hidden = 1;\nexport interface Props { title: string }\nexport class App {\n  private count = 0;\n  visible = 1;\n}\n",
        );
        assert_eq!(named(&parsed, "counter").kind, SymbolKind::Variable);
        assert!(named(&parsed, "counter").exported);
        assert_eq!(named(&parsed, "hidden").kind, SymbolKind::Variable);
        assert!(!named(&parsed, "hidden").exported);
        assert_eq!(named(&parsed, "title").kind, SymbolKind::Field);
        assert!(named(&parsed, "title").exported);
        assert_eq!(named(&parsed, "count").kind, SymbolKind::Field);
        assert!(!named(&parsed, "count").exported);
        assert_eq!(named(&parsed, "visible").kind, SymbolKind::Field);
        assert!(named(&parsed, "visible").exported);
    }

    #[test]
    fn ground_truth_skips_function_locals_and_call_sites() {
        let go = go_defs(&mask_c_like(
            "package p\ntype Point struct {\n\tX int\n\ty int\n}\nfunc New() *Point {\n\treq := 1\n\tuser := req\n\treturn &Point{}\n}\n",
            CFlavor::Go,
        ));
        assert!(go.contains(&"Point".to_string()));
        assert!(go.contains(&"X".to_string()));
        assert!(go.contains(&"y".to_string()));
        assert!(go.contains(&"New".to_string()));
        assert!(!go.contains(&"req".to_string()));
        assert!(!go.contains(&"user".to_string()));

        let rust = rust_defs(&mask_c_like(
            "fn main() { let Ok(x) = f(); }\nfn helper() {}\npub struct Point { pub x: i32, y: i32 }\n",
            CFlavor::Rust,
        ));
        assert!(rust.contains(&"main".to_string()));
        assert!(rust.contains(&"helper".to_string()));
        assert!(rust.contains(&"Point".to_string()));
        assert!(rust.contains(&"x".to_string()));
        assert!(rust.contains(&"y".to_string()));
        assert!(!rust.contains(&"Ok".to_string()));

        let ts = ts_defs(&mask_c_like(
            "export function greet() { const id = 1; assignNodeIds(); }\nexport interface Props { title: string }\nfunction exploredFiles(\n  graph: CodeGraph,\n): { greppedIds: number } {\n  const content = 1;\n}\nfunction assembleGraph(\n  ctx: AssembleContext = {},\n): Promise<CodeGraph> {\n  const cache = 1;\n}\n",
            CFlavor::Js,
        ));
        assert!(ts.contains(&"greet".to_string()));
        assert!(ts.contains(&"Props".to_string()));
        assert!(ts.contains(&"title".to_string()));
        assert!(ts.contains(&"exploredFiles".to_string()));
        assert!(!ts.contains(&"id".to_string()));
        assert!(!ts.contains(&"assignNodeIds".to_string()));
        assert!(!ts.contains(&"content".to_string()));
        assert!(!ts.contains(&"greppedIds".to_string()));
        assert!(ts.contains(&"assembleGraph".to_string()));
        assert!(!ts.contains(&"cache".to_string()));
    }

    #[test]
    fn ground_truth_mask_drops_defs_inside_comments_and_strings() {
        let py = mask_python(
            "\"\"\"\ndef fake():\n    pass\n\"\"\"\ndef real():\n    # def also_fake():\n    pass\n",
        );
        assert_eq!(python_defs(&py), ["real"]);

        let java = mask_c_like(
            "class Real {}\n/* class Fake {} */\n// class AlsoFake {}\nString s = \"class Nope {}\";\n",
            CFlavor::Java,
        );
        assert_eq!(java_defs(&java), ["Real"]);
    }

    // ---------------------------------------------------------------- recall
    //
    // Quality gate: symbol recall ≥90% vs an independent definition scan.
    //
    // Denominator design (the 209/209 lesson): ground truth is produced from a
    // filesystem walk by extension plus a comment/string-stripped keyword scan.
    // ParserPool is consulted *afterwards*. A parse failure, empty extract, or
    // binary misclassification in the code under test cannot shrink the
    // denominator — unread files contribute a sentinel `<unreadable>` name that
    // the parser will never match.
    //
    // Known residual misses (not used to shrink GT):
    // - Java annotation type elements (`value()`, `serialize()`) are true
    //   misses until queries capture `annotation_type_element_declaration`.
    // - Java `String` / enum constant names / `K` are field-scanner noise.
    // - Go embedded `http.ResponseWriter` is recorded as `http` by the line
    //   scanner; the parser correctly names `ResponseWriter`.
    // - Rust `$name` macro metavariables and test `HAYSTACK` macros are noise.
    // - TypeScript leftover short names in generated grammar files are noise.

    const RECALL_THRESHOLD: f64 = 0.90;

    #[derive(Clone, Copy)]
    enum RecallLang {
        Python,
        Go,
        Java,
        Rust,
        TypeScript,
    }

    struct RecallCorpus {
        language: &'static str,
        name: &'static str,
        root: PathBuf,
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("astrolabe-core must be inside the workspace")
            .to_path_buf()
    }

    fn recall_corpus(kind: RecallLang) -> RecallCorpus {
        let workspace = workspace_root();
        // Env override first, then a sibling of the workspace. No absolute
        // fallback: a baked-in path only works on one machine.
        let sibling = |name: &str, env_key: &str| {
            if let Some(p) = std::env::var_os(env_key).map(PathBuf::from) {
                return p;
            }
            workspace
                .parent()
                .map(|p| p.join(name))
                .unwrap_or_else(|| workspace.join(name))
        };
        match kind {
            RecallLang::Python => RecallCorpus {
                language: "Python",
                name: "serena",
                root: sibling("serena", "ASTROLABE_CORPUS_PYTHON"),
            },
            RecallLang::Go => RecallCorpus {
                language: "Go",
                name: "gin",
                root: workspace.join("corpus/go"),
            },
            RecallLang::Java => RecallCorpus {
                language: "Java",
                name: "gson",
                root: workspace.join("corpus/java"),
            },
            RecallLang::Rust => RecallCorpus {
                language: "Rust",
                name: "ripgrep",
                root: workspace.join("corpus/rust"),
            },
            RecallLang::TypeScript => RecallCorpus {
                language: "TypeScript",
                name: "openvisio-oss",
                root: sibling("openvisio-oss", "ASTROLABE_CORPUS_TYPESCRIPT"),
            },
        }
    }

    fn ignored_dir(name: &str) -> bool {
        matches!(
            name,
            ".git"
                | ".venv"
                | "venv"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | "__pycache__"
                | ".mypy_cache"
                | ".next"
                | ".openvisio"
                | ".serena"
        )
    }

    fn walk_files(dir: &Path, root: &Path, out: &mut Vec<RelPath>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !ignored_dir(&entry.file_name().to_string_lossy()) {
                    walk_files(&path, root, out);
                }
            } else if kind.is_file() {
                if let Ok(relative) = path.strip_prefix(root) {
                    out.push(RelPath::new(relative.to_string_lossy()));
                }
            }
        }
    }

    fn language_of(kind: RecallLang, path: &RelPath) -> Option<Language> {
        let lang = Language::from_path(path)?;
        match kind {
            RecallLang::Python => matches!(lang, Language::Python).then_some(lang),
            RecallLang::Go => matches!(lang, Language::Go).then_some(lang),
            RecallLang::Java => matches!(lang, Language::Java).then_some(lang),
            RecallLang::Rust => matches!(lang, Language::Rust).then_some(lang),
            RecallLang::TypeScript => {
                matches!(lang, Language::TypeScript | Language::Tsx).then_some(lang)
            }
        }
    }

    fn ground_truth_names(lang: Language, source: &str) -> Vec<String> {
        match lang {
            Language::Python => python_defs(&mask_python(source)),
            Language::Go => go_defs(&mask_c_like(source, CFlavor::Go)),
            Language::Java => java_defs(&mask_c_like(source, CFlavor::Java)),
            Language::Rust => rust_defs(&mask_c_like(source, CFlavor::Rust)),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                ts_defs(&mask_c_like(source, CFlavor::Js))
            }
            // P0 languages without recall baselines yet.
            Language::Php
            | Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::Swift
            | Language::Vue
            | Language::Dart
            | Language::CSharp
            | Language::VisualBasic
            | Language::Kotlin
            | Language::Shell
            | Language::Ruby
            | Language::Lua => Vec::new(),
        }
    }

    fn run_symbol_recall(kind: RecallLang) {
        let corpus = recall_corpus(kind);
        assert!(
            corpus.root.is_dir(),
            "语料目录不存在: {}",
            corpus.root.display()
        );

        let mut files = Vec::new();
        walk_files(&corpus.root, &corpus.root, &mut files);
        files.sort();
        files.retain(|path| language_of(kind, path).is_some());
        assert!(
            !files.is_empty(),
            "{} 语料没有源文件: {}",
            corpus.language,
            corpus.root.display()
        );

        // Ground truth is built from the filesystem walk + regex-like scans of
        // file bytes. ParserPool is consulted afterwards. A parse failure
        // therefore cannot shrink the denominator — the file's GT names stay.
        let pool = ParserPool::new();
        let mut gt: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut got: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut kind_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut parse_fail = 0usize;
        let mut unread = 0usize;
        let mut fail_samples = Vec::new();

        for path in &files {
            let lang = language_of(kind, path).expect("filtered");
            let disk = corpus.root.join(path.as_str());
            let source = match fs::read_to_string(&disk) {
                Ok(source) => source,
                Err(_) => {
                    unread += 1;
                    *gt.entry((path.as_str().to_string(), "<unreadable>".into()))
                        .or_insert(0) += 1;
                    continue;
                }
            };
            for name in ground_truth_names(lang, &source) {
                *gt.entry((path.as_str().to_string(), name)).or_insert(0) += 1;
            }
            match pool.parse(lang, path, &source) {
                Ok(parsed) => {
                    for symbol in parsed.symbols {
                        *got.entry((path.as_str().to_string(), symbol.name))
                            .or_insert(0) += 1;
                        let label = match symbol.kind {
                            SymbolKind::Function => "function",
                            SymbolKind::Method => "method",
                            SymbolKind::Class => "class",
                            SymbolKind::Interface => "interface",
                            SymbolKind::Struct => "struct",
                            SymbolKind::Enum => "enum",
                            SymbolKind::Trait => "trait",
                            SymbolKind::Type => "type",
                            SymbolKind::Const => "const",
                            SymbolKind::Module => "module",
                            SymbolKind::Field => "field",
                            SymbolKind::Variable => "variable",
                        };
                        *kind_counts.entry(label).or_insert(0) += 1;
                    }
                }
                Err(error) => {
                    parse_fail += 1;
                    if fail_samples.len() < 8 {
                        fail_samples.push(format!("{path}: {error}"));
                    }
                }
            }
        }

        let mut gt_total = 0usize;
        let mut matched = 0usize;
        let mut misses = Vec::new();
        for (key, &count) in &gt {
            gt_total += count;
            let hit = got.get(key).copied().unwrap_or(0).min(count);
            matched += hit;
            for _ in hit..count {
                if misses.len() < 24 {
                    misses.push(format!("{} :: {}", key.0, key.1));
                }
            }
        }

        let rate = if gt_total == 0 {
            1.0
        } else {
            matched as f64 / gt_total as f64
        };
        println!(
            "ASTROLABE_SYMBOL_RECALL|{}|{}|{}|{}|{:.2}|{}|{}|{}|{}",
            corpus.language,
            corpus.name,
            gt_total,
            matched,
            rate * 100.0,
            files.len(),
            parse_fail,
            unread,
            if rate + f64::EPSILON >= RECALL_THRESHOLD {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!("  kinds: {kind_counts:?}");
        if !fail_samples.is_empty() {
            println!("  parse_failures: {fail_samples:?}");
        }
        if !misses.is_empty() {
            println!("  miss_samples: {misses:?}");
        }
        assert!(
            gt_total > 0,
            "{} ground truth 为空；请检查语料路径和提取规则",
            corpus.language
        );
        assert!(
            rate + f64::EPSILON >= RECALL_THRESHOLD,
            "{} 符号召回率 {:.2}% 低于 {:.0}%（GT={gt_total} matched={matched} files={} parse_fail={parse_fail} unread={unread}）。分母来自独立扫描，parse 失败不会把它缩小。漏报样例: {misses:?}",
            corpus.language,
            rate * 100.0,
            RECALL_THRESHOLD * 100.0,
            files.len(),
        );
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn python_symbol_recall() {
        run_symbol_recall(RecallLang::Python);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn go_symbol_recall() {
        run_symbol_recall(RecallLang::Go);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn java_symbol_recall() {
        run_symbol_recall(RecallLang::Java);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn rust_symbol_recall() {
        run_symbol_recall(RecallLang::Rust);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn typescript_symbol_recall() {
        run_symbol_recall(RecallLang::TypeScript);
    }

    #[test]
    fn dart_full_declaration_coverage() {
        let src = "typedef Cb = void Function(int);\ntypedef void Old(int x);\nconst top = 1, top2 = 2;\nint topLate;\nenum Color { red, green }\nmixin M on Object { int mf = 0; }\nextension Ext on String { int get len2 => length; }\nextension type ET(int v) { void m() {} }\nclass A {\n  static const k = 1;\n  final int x, y;\n  abstract int af;\n  A(this.x, this.y);\n  const A.c(this.x, this.y);\n  factory A.f() => A(1, 2);\n  A operator +(A o) => this;\n  A operator [](int i) => this;\n}\nclass D = B with M;\n";
        let parsed = ParserPool::new()
            .parse(Language::Dart, &RelPath::new("lib/a.dart"), src)
            .expect("parse");
        let got: Vec<(String, SymbolKind, u32)> = parsed
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind, s.start_line))
            .collect();
        let has = |name: &str, kind: SymbolKind, line: u32| {
            assert!(
                got.contains(&(name.to_owned(), kind, line)),
                "missing {name} {kind:?} @{line} in {got:?}"
            );
        };
        has("Cb", SymbolKind::Type, 1);
        has("Old", SymbolKind::Type, 2);
        has("top", SymbolKind::Variable, 3);
        has("top2", SymbolKind::Variable, 3);
        has("topLate", SymbolKind::Variable, 4);
        has("red", SymbolKind::Field, 5);
        has("green", SymbolKind::Field, 5);
        has("mf", SymbolKind::Field, 6);
        has("Ext", SymbolKind::Class, 7);
        has("ET", SymbolKind::Class, 8);
        has("k", SymbolKind::Field, 10);
        has("x", SymbolKind::Field, 11);
        has("y", SymbolKind::Field, 11);
        has("af", SymbolKind::Field, 12);
        has("A", SymbolKind::Method, 13);
        has("A", SymbolKind::Method, 14);
        has("c", SymbolKind::Method, 14);
        has("A", SymbolKind::Method, 15);
        has("f", SymbolKind::Method, 15);
        has("operator +", SymbolKind::Method, 16);
        has("operator []", SymbolKind::Method, 17);
        has("D", SymbolKind::Class, 19);
    }

    #[test]
    fn dart_reparse_after_edit_picks_up_new_constructor() {
        let pool = ParserPool::new();
        let path = RelPath::new("lib/p.dart");
        let before = pool
            .parse(Language::Dart, &path, "class P {\n  P();\n}\n")
            .expect("parse");
        let count = |f: &ParsedFile| f.symbols.iter().filter(|s| s.name == "P").count();
        assert_eq!(count(&before), 2);
        let after = pool
            .parse(
                Language::Dart,
                &path,
                "class P {\n  P();\n  P.value(int v);\n}\n",
            )
            .expect("parse");
        assert_eq!(count(&after), 3);
        assert!(after
            .symbols
            .iter()
            .any(|s| s.name == "value" && s.start_line == 3));
    }

    #[test]
    #[ignore = "Dart ABI spike on AI-con project"]
    fn dart_abi_spike_on_aicon() {
        let lang = Language::Dart;
        let ts_lang = grammar(lang).expect("Dart grammar must be present");
        let mut parser = Parser::new();
        parser
            .set_language(&ts_lang)
            .expect("tree-sitter set_language must succeed for Dart");

        let q_sources = queries::for_language(lang).expect("Dart queries must be present");
        let sym_query =
            Query::new(&ts_lang, q_sources.symbols).expect("symbols query must compile");
        let imp_query =
            Query::new(&ts_lang, q_sources.imports).expect("imports query must compile");

        let root = std::path::Path::new("/Users/zhoulei/Project/AI-con/src/app/lib");
        if !root.exists() {
            println!(
                "AI-con path does not exist, skipping spike: {}",
                root.display()
            );
            return;
        }

        let mut dart_files = Vec::new();
        fn collect_dart(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect_dart(&path, out);
                    } else if path.extension().is_some_and(|ext| ext == "dart") {
                        out.push(path);
                    }
                }
            }
        }
        collect_dart(root, &mut dart_files);

        let total_files = dart_files.len();
        let mut error_files = 0;
        let mut total_symbols = 0;
        let mut total_imports = 0;

        for path in &dart_files {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let tree = parser
                .parse(&content, None)
                .expect("parser should produce a tree");
            let root_node = tree.root_node();
            if root_node.has_error() {
                error_files += 1;
            }

            let syms = extract_symbols(lang, &sym_query, root_node, &content);
            total_symbols += syms.len();

            let imps = extract_imports(lang, &imp_query, root_node, &content);
            total_imports += imps.len();
        }

        let error_rate = if total_files > 0 {
            error_files as f64 / total_files as f64
        } else {
            0.0
        };

        println!("=== Dart ABI Spike on AI-con ===");
        println!("Total Dart files: {total_files}");
        println!(
            "Files with ERROR/MISSING: {error_files} ({:.2}%)",
            error_rate * 100.0
        );
        println!("Total symbols extracted: {total_symbols}");
        println!("Total imports extracted: {total_imports}");

        assert!(
            error_rate < 0.10,
            "ERROR file rate {:.2}% exceeds threshold of 10%",
            error_rate * 100.0
        );
    }

    // ------------------------------------------------------------- masking

    #[derive(Clone, Copy)]
    enum CFlavor {
        Go,
        Java,
        Rust,
        Js,
    }

    fn mask_python(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            if let Some((consumed, quote, triple, raw)) = python_string_start(&chars, i) {
                for _ in 0..consumed {
                    out.push(' ');
                }
                i += consumed;
                if triple {
                    while i + 2 < chars.len()
                        && !(chars[i] == quote && chars[i + 1] == quote && chars[i + 2] == quote)
                    {
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    for _ in 0..3 {
                        if i < chars.len() {
                            out.push(' ');
                            i += 1;
                        }
                    }
                } else {
                    while i < chars.len() && chars[i] != quote {
                        if !raw && chars[i] == '\\' && i + 1 < chars.len() {
                            out.push(' ');
                            out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                            i += 2;
                            continue;
                        }
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                }
                continue;
            }
            if chars[i] == '#' {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn python_string_start(chars: &[char], i: usize) -> Option<(usize, char, bool, bool)> {
        let mut j = i;
        let mut prefix = 0usize;
        let mut raw = false;
        while j < chars.len() && matches!(chars[j], 'r' | 'R' | 'u' | 'U' | 'f' | 'F' | 'b' | 'B') {
            if matches!(chars[j], 'r' | 'R') {
                raw = true;
            }
            prefix += 1;
            j += 1;
            if prefix > 2 {
                return None;
            }
        }
        let q = *chars.get(j)?;
        if q != '\'' && q != '"' {
            return None;
        }
        let triple = j + 2 < chars.len() && chars[j + 1] == q && chars[j + 2] == q;
        let quote_len = if triple { 3 } else { 1 };
        Some((j - i + quote_len, q, triple, raw))
    }

    fn mask_c_like(source: &str, flavor: CFlavor) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            if matches!(flavor, CFlavor::Rust) {
                if let Some(hashes) = rust_raw_string_hashes(&chars, i) {
                    let start_len = rust_raw_prefix_len(&chars, i, hashes);
                    for _ in 0..start_len {
                        out.push(' ');
                    }
                    i += start_len;
                    loop {
                        if i >= chars.len() {
                            break;
                        }
                        if chars[i] == '"' && rust_raw_closer(&chars, i + 1, hashes) {
                            out.push(' ');
                            i += 1;
                            for _ in 0..hashes {
                                out.push(' ');
                                i += 1;
                            }
                            break;
                        }
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    continue;
                }
            }
            if matches!(flavor, CFlavor::Java)
                && chars[i] == '"'
                && i + 2 < chars.len()
                && chars[i + 1] == '"'
                && chars[i + 2] == '"'
            {
                for _ in 0..3 {
                    out.push(' ');
                }
                i += 3;
                while i + 2 < chars.len()
                    && !(chars[i] == '"' && chars[i + 1] == '"' && chars[i + 2] == '"')
                {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                for _ in 0..3 {
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                }
                continue;
            }
            if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                out.push(' ');
                out.push(' ');
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                        break;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                continue;
            }
            if chars[i] == '"' {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(' ');
                        out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                        i += 2;
                        continue;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '\'' {
                if matches!(flavor, CFlavor::Rust) && rust_lifetime(&chars, i) {
                    out.push('\'');
                    i += 1;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        out.push(chars[i]);
                        i += 1;
                    }
                    continue;
                }
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(' ');
                        out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                        i += 2;
                        continue;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '`' && matches!(flavor, CFlavor::Go | CFlavor::Js) {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn rust_lifetime(chars: &[char], i: usize) -> bool {
        let Some(&next) = chars.get(i + 1) else {
            return false;
        };
        if !(next.is_alphabetic() || next == '_') {
            return false;
        }
        let mut j = i + 2;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        chars.get(j) != Some(&'\'') || j > i + 2
    }

    fn rust_raw_prefix_len(chars: &[char], i: usize, hashes: usize) -> usize {
        let mut n = 0;
        if chars.get(i) == Some(&'b') || chars.get(i) == Some(&'c') {
            n += 1;
        }
        n += 1; // r
        n += hashes;
        n + 1 // opening quote
    }

    fn rust_raw_string_hashes(chars: &[char], i: usize) -> Option<usize> {
        let mut j = i;
        if matches!(chars.get(j), Some('b') | Some('c')) {
            j += 1;
        }
        if chars.get(j) != Some(&'r') {
            return None;
        }
        j += 1;
        let mut hashes = 0usize;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
        if chars.get(j) == Some(&'"') {
            Some(hashes)
        } else {
            None
        }
    }

    fn rust_raw_closer(chars: &[char], mut i: usize, hashes: usize) -> bool {
        for _ in 0..hashes {
            if chars.get(i) != Some(&'#') {
                return false;
            }
            i += 1;
        }
        true
    }

    fn leading_ident(s: &str) -> Option<&str> {
        let s = s.trim_start();
        let mut chars = s.char_indices();
        let (_, first) = chars.next()?;
        if !(first.is_alphabetic() || first == '_' || first == '$' || first == '#') {
            return None;
        }
        let mut end = first.len_utf8();
        for (idx, ch) in chars {
            if ch.is_alphanumeric() || ch == '_' {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        Some(&s[..end])
    }

    fn push_ident(s: &str, names: &mut Vec<String>) {
        if let Some(name) = leading_ident(s) {
            names.push(name.to_string());
        }
    }

    fn skip_balanced_parens(s: &str) -> Option<&str> {
        let s = s.trim_start();
        if !s.starts_with('(') {
            return Some(s);
        }
        let mut depth = 0i32;
        for (idx, ch) in s.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(s[idx + 1..].trim_start());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn net_count(s: &str, open: char, close: char) -> i32 {
        s.chars().filter(|&c| c == open).count() as i32
            - s.chars().filter(|&c| c == close).count() as i32
    }

    fn python_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if", "elif", "else", "for", "while", "try", "except", "finally", "with", "import",
            "from", "return", "raise", "assert", "class", "def", "async", "await", "yield", "pass",
            "break", "continue", "global", "nonlocal", "lambda", "and", "or", "not", "in", "is",
            "match", "case",
        ];
        let mut names = Vec::new();
        for line in masked.lines() {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("class ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = trimmed.strip_prefix("async def ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = trimmed.strip_prefix("def ") {
                push_ident(rest, &mut names);
            } else if indent == 0 {
                if let Some(name) = leading_ident(trimmed) {
                    if SKIP.contains(&name) {
                        continue;
                    }
                    let after = trimmed[name.len()..].trim_start();
                    if after.starts_with('=') || after.starts_with(':') {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names
    }

    fn go_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "package",
            "import",
            "func",
            "return",
            "if",
            "for",
            "range",
            "switch",
            "case",
            "default",
            "go",
            "defer",
            "select",
            "struct",
            "interface",
            "map",
            "chan",
            "type",
            "const",
            "var",
            "else",
            "break",
            "continue",
            "fallthrough",
            "goto",
        ];
        let mut names = Vec::new();
        let mut block: Option<&'static str> = None;
        let mut paren = 0i32;
        let mut func_depth = 0i32;
        let mut pending_func = false;
        let mut type_depth = 0i32;

        for line in masked.lines() {
            let trimmed = line.trim();
            let braces = net_count(line, '{', '}');

            if func_depth > 0 || pending_func {
                if pending_func && line.contains('{') {
                    pending_func = false;
                    func_depth = (func_depth + braces).max(0);
                } else if func_depth > 0 {
                    func_depth = (func_depth + braces).max(0);
                }
                continue;
            }

            if type_depth > 0 {
                collect_go_field_names(trimmed, SKIP, &mut names);
                type_depth = (type_depth + braces).max(0);
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("func ") {
                let rest = rest.trim_start();
                let after = if rest.starts_with('(') {
                    skip_balanced_parens(rest).unwrap_or("")
                } else {
                    rest
                };
                push_ident(after, &mut names);
                if line.contains('{') {
                    func_depth = braces.max(0);
                } else {
                    pending_func = true;
                }
                continue;
            }

            if let Some(kind) = block {
                paren += net_count(line, '(', ')');
                if let Some(name) = leading_ident(trimmed) {
                    if !SKIP.contains(&name) {
                        names.push(name.to_string());
                    }
                }
                if kind == "type"
                    && (trimmed.contains("struct") || trimmed.contains("interface"))
                    && line.contains('{')
                {
                    type_depth = braces.max(0);
                }
                if paren <= 0 {
                    block = None;
                    paren = 0;
                }
                continue;
            }

            for (kw, kind) in [("type ", "type"), ("const ", "const"), ("var ", "var")] {
                if let Some(rest) = trimmed.strip_prefix(kw) {
                    let rest = rest.trim_start();
                    if rest.starts_with('(') {
                        block = Some(kind);
                        paren = net_count(rest, '(', ')');
                        if let Some(after) = rest.strip_prefix('(') {
                            if let Some(name) = leading_ident(after) {
                                if !SKIP.contains(&name) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                        if paren <= 0 {
                            block = None;
                        }
                    } else {
                        push_ident(rest, &mut names);
                        if kind == "type" && (rest.contains("struct") || rest.contains("interface"))
                        {
                            if braces > 0 {
                                type_depth = braces;
                            } else if line.contains('{') && line.contains('}') {
                                if let Some(inner) = line
                                    .split_once('{')
                                    .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                                {
                                    collect_go_field_names(inner.trim(), SKIP, &mut names);
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
        names
    }

    fn collect_go_field_names(trimmed: &str, skip: &[&str], names: &mut Vec<String>) {
        let mut rest = trimmed;
        while let Some(name) = leading_ident(rest) {
            if skip.contains(&name) {
                break;
            }
            names.push(name.to_string());
            rest = rest[name.len()..].trim_start();
            if rest.starts_with(',') {
                rest = rest[1..].trim_start();
                continue;
            }
            break;
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tok<'a> {
        Word(&'a str),
        Punct(char),
    }

    fn lex(masked: &str) -> Vec<Tok<'_>> {
        let mut out = Vec::new();
        let mut rest = masked;
        while !rest.is_empty() {
            let ch = rest.chars().next().unwrap();
            if ch.is_whitespace() {
                rest = &rest[ch.len_utf8()..];
                continue;
            }
            if ch.is_alphabetic() || ch == '_' || ch == '$' || ch == '#' {
                let mut len = ch.len_utf8();
                for next in rest[len..].chars() {
                    if next.is_alphanumeric() || next == '_' {
                        len += next.len_utf8();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Word(&rest[..len]));
                rest = &rest[len..];
                continue;
            }
            out.push(Tok::Punct(ch));
            rest = &rest[ch.len_utf8()..];
        }
        out
    }

    fn java_defs(masked: &str) -> Vec<String> {
        const CONTROL: &[&str] = &[
            "if",
            "for",
            "while",
            "switch",
            "catch",
            "synchronized",
            "try",
            "return",
            "throw",
            "new",
            "assert",
            "else",
            "this",
            "super",
            "case",
            "instanceof",
            "when",
            "yield",
            "class",
            "interface",
            "enum",
            "record",
            "package",
            "import",
            "extends",
            "implements",
            "throws",
            "break",
            "continue",
            "finally",
            "do",
        ];
        const ACCESS: &[&str] = &["public", "private", "protected"];
        let toks = lex(masked);
        let mut names = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            match toks[i] {
                Tok::Word("class" | "interface" | "enum" | "record") => {
                    let dotted = i > 0 && toks[i - 1] == Tok::Punct('.');
                    if !dotted {
                        if let Some(Tok::Word(name)) = toks.get(i + 1) {
                            if !CONTROL.contains(name) {
                                names.push((*name).to_string());
                            }
                        }
                    }
                }
                Tok::Word(word) => {
                    let next = toks.get(i + 1);
                    if next == Some(&Tok::Punct('(')) && !CONTROL.contains(&word) {
                        if java_looks_like_method(&toks, i) {
                            names.push(word.to_string());
                        }
                    } else if matches!(
                        next,
                        Some(&Tok::Punct(';')) | Some(&Tok::Punct('=')) | Some(&Tok::Punct(','))
                    ) && !matches!(word, "true" | "false" | "null" | "L" | "D" | "F")
                        && java_looks_like_field(&toks, i, ACCESS)
                    {
                        names.push(word.to_string());
                    }
                }
                _ => {}
            }
            i += 1;
        }
        names
    }

    fn java_looks_like_method(toks: &[Tok<'_>], i: usize) -> bool {
        match toks.get(i.saturating_sub(1)) {
            None => false,
            Some(Tok::Punct('.' | '=' | ',' | '(')) => false,
            Some(Tok::Word(
                "new" | "return" | "throw" | "if" | "while" | "switch" | "for" | "catch" | "this"
                | "super" | "case",
            )) => false,
            Some(Tok::Word(_)) | Some(Tok::Punct('>' | ']')) => true,
            Some(Tok::Punct(_)) => false,
        }
    }

    fn java_looks_like_field(toks: &[Tok<'_>], i: usize, access: &[&str]) -> bool {
        let mut saw_access = false;
        let mut saw_type = false;
        let mut j = i;
        while j > 0 {
            j -= 1;
            match toks[j] {
                Tok::Punct(';' | '{' | '}' | '(' | ')') => break,
                Tok::Word(w) if access.contains(&w) => saw_access = true,
                Tok::Word("static" | "final" | "transient" | "volatile" | "native") => {}
                Tok::Word(_) | Tok::Punct('>' | ']') => saw_type = true,
                _ => {}
            }
        }
        saw_access && saw_type
    }

    fn rust_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if", "else", "for", "while", "loop", "match", "return", "let", "mut", "ref", "self",
            "Self", "crate", "super", "as", "in", "where", "unsafe", "async", "await", "dyn",
            "impl", "use", "extern", "move", "break", "continue", "pub", "enum", "struct", "fn",
        ];
        let mut names = Vec::new();
        let mut field_body = 0i32;
        let mut pending_fields = false;

        for line in masked.lines() {
            let stripped = strip_rust_prefix(line.trim_start());
            let braces = net_count(line, '{', '}');
            let is_field_item = stripped.starts_with("struct ")
                || stripped.starts_with("enum ")
                || stripped.starts_with("union ");

            if let Some(rest) = stripped.strip_prefix("macro_rules!") {
                push_ident(rest.trim_start(), &mut names);
            } else {
                for kw in [
                    "fn ", "struct ", "enum ", "trait ", "union ", "type ", "mod ", "const ",
                    "static ",
                ] {
                    if let Some(rest) = stripped.strip_prefix(kw) {
                        let rest = rest
                            .trim_start()
                            .strip_prefix("mut ")
                            .unwrap_or(rest.trim_start());
                        push_ident(rest, &mut names);
                        break;
                    }
                }
            }

            if field_body > 0 {
                if !is_field_item {
                    if let Some(name) = leading_ident(stripped) {
                        if !SKIP.contains(&name) {
                            let after = stripped[name.len()..].trim_start();
                            if after.starts_with(':')
                                || after.starts_with('(')
                                || after.starts_with('{')
                                || after.starts_with(',')
                                || after.starts_with('=')
                                || after.is_empty()
                            {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
                field_body = (field_body + braces).max(0);
                continue;
            }

            if pending_fields {
                if line.contains('{') {
                    pending_fields = false;
                    field_body = braces.max(0);
                } else if line.contains(';') {
                    pending_fields = false;
                }
                continue;
            }

            if is_field_item {
                if line.contains('{') {
                    field_body = braces.max(0);
                    if braces == 0 && line.contains('}') {
                        if let Some(inner) = line
                            .split_once('{')
                            .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                        {
                            for piece in inner.split(',') {
                                if let Some(name) = leading_ident(strip_rust_prefix(piece)) {
                                    if !SKIP.contains(&name) {
                                        names.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                } else if !line.contains(';') {
                    pending_fields = true;
                }
            }
        }
        names
    }

    fn strip_rust_prefix(s: &str) -> &str {
        let s = s.trim_start();
        if let Some(rest) = s.strip_prefix("pub") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                if let Some(after) = skip_balanced_parens(rest) {
                    return strip_rust_prefix(after);
                }
            }
            return strip_rust_prefix(rest);
        }
        for prefix in ["async ", "unsafe ", "default "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return strip_rust_prefix(rest);
            }
        }
        if let Some(rest) = s.strip_prefix("const ") {
            let rest = rest.trim_start();
            if rest.starts_with("fn ") || rest.starts_with("unsafe ") || rest.starts_with("async ")
            {
                return strip_rust_prefix(rest);
            }
        }
        if let Some(rest) = s.strip_prefix("extern ") {
            return strip_rust_prefix(rest.trim_start().trim_start_matches(|c: char| {
                c == '"' || c.is_alphanumeric() || c == '_' || c.is_whitespace()
            }));
        }
        s
    }

    fn ts_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if",
            "else",
            "for",
            "while",
            "switch",
            "case",
            "return",
            "throw",
            "new",
            "typeof",
            "instanceof",
            "await",
            "yield",
            "break",
            "continue",
            "try",
            "catch",
            "finally",
            "do",
            "import",
            "from",
            "as",
            "of",
            "in",
            "void",
            "this",
            "super",
            "with",
            "function",
            "class",
            "interface",
            "enum",
            "type",
            "const",
            "let",
            "var",
        ];
        let mut names = Vec::new();
        let mut func_depth = 0i32;
        let mut type_depth = 0i32;
        let mut pending_func = false;

        for line in masked.lines() {
            let stripped = strip_ts_prefix(line.trim_start());
            let braces = net_count(line, '{', '}');
            let is_fn = stripped.starts_with("function");
            let is_type = stripped.starts_with("class ")
                || stripped.starts_with("interface ")
                || stripped.starts_with("enum ")
                || stripped.starts_with("namespace ")
                || stripped.starts_with("module ");

            if func_depth > 0 || pending_func {
                if pending_func {
                    let open_body = line.trim_end().ends_with('{');
                    if open_body {
                        pending_func = false;
                        func_depth = braces.max(0);
                    } else if line.trim_end().ends_with(';') {
                        pending_func = false;
                    }
                } else {
                    if is_fn {
                        if let Some(rest) = stripped.strip_prefix("function") {
                            let rest = rest.trim_start().trim_start_matches('*').trim_start();
                            push_ident(rest, &mut names);
                        }
                    } else if stripped.starts_with("class ") {
                        if let Some(rest) = stripped.strip_prefix("class ") {
                            push_ident(rest, &mut names);
                        }
                    }
                    func_depth = (func_depth + braces).max(0);
                }
                continue;
            }

            if type_depth > 0 {
                if type_depth == 1 {
                    if let Some(name) = leading_ident(stripped) {
                        if !SKIP.contains(&name) {
                            let after = stripped[name.len()..].trim_start();
                            if after.starts_with('(')
                                || after.starts_with('=')
                                || after.starts_with(':')
                                || after.starts_with('?')
                                || after.starts_with('<')
                            {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
                type_depth = (type_depth + braces).max(0);
                continue;
            }

            if let Some(rest) = stripped.strip_prefix("function") {
                let rest = rest.trim_start().trim_start_matches('*').trim_start();
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("class ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("interface ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("enum ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("namespace ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("module ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("type ") {
                if let Some(name) = leading_ident(rest) {
                    let after = rest[name.len()..].trim_start();
                    if after.starts_with('=') || after.starts_with('<') {
                        names.push(name.to_string());
                    }
                }
            } else {
                for kw in ["const ", "let ", "var "] {
                    if let Some(rest) = stripped.strip_prefix(kw) {
                        let rest = rest.trim_start();
                        if !rest.starts_with('{') && !rest.starts_with('[') {
                            push_ident(rest, &mut names);
                        }
                        break;
                    }
                }
            }

            if is_fn || looks_like_ts_arrow_fn(stripped) {
                if line.contains('{') {
                    func_depth = braces.max(0);
                } else if !line.contains(';') {
                    pending_func = true;
                }
            } else if is_type {
                type_depth = braces.max(0);
                if braces == 0 && line.contains('{') && line.contains('}') {
                    if let Some(inner) = line
                        .split_once('{')
                        .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                    {
                        for piece in inner.split([';', ',']) {
                            if let Some(name) = leading_ident(piece) {
                                if !SKIP.contains(&name) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        names
    }

    fn looks_like_ts_arrow_fn(stripped: &str) -> bool {
        (stripped.starts_with("const ")
            || stripped.starts_with("let ")
            || stripped.starts_with("var ")
            || stripped.starts_with("export "))
            && (stripped.contains("=>") || stripped.contains("function"))
    }

    fn strip_ts_prefix(s: &str) -> &str {
        let s = s.trim_start();
        for prefix in [
            "export ",
            "default ",
            "declare ",
            "async ",
            "abstract ",
            "public ",
            "private ",
            "protected ",
            "static ",
            "readonly ",
            "override ",
            "accessor ",
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return strip_ts_prefix(rest);
            }
        }
        s
    }

    #[test]
    fn php_dirname_levels_and_unresolvable_left_operands() {
        let pool = ParserPool::new();
        let src = "<?php\nrequire __DIR__ . '/a.php';\nrequire_once dirname(__DIR__) . '/b.php';\ninclude dirname(__DIR__, 2) . '/c.php';\nrequire dirname(dirname(__DIR__)) . \"/d.php\";\nrequire dirname(__FILE__) . '/e.php';\nrequire BASE_PATH . '/f.php';\nrequire $root . '/g.php';\nrequire 'h.php';\n";
        let mut got = pool
            .parse(Language::Php, &RelPath::new("a.php"), src)
            .expect("parse php")
            .imports;
        got.sort();
        assert_eq!(
            got,
            [
                "/../../c.php",
                "/../../d.php",
                "/../b.php",
                "/a.php",
                "/e.php",
                "h.php"
            ]
        );
    }

    #[test]
    fn c_angle_includes_keep_brackets() {
        let pool = ParserPool::new();
        let mut got = pool
            .parse(
                Language::C,
                &RelPath::new("a.c"),
                "#include <stdio.h>\n#include \"x.h\"\n",
            )
            .expect("parse c")
            .imports;
        got.sort();
        assert_eq!(got, ["<stdio.h>", "x.h"]);
    }

    #[test]
    fn php_use_forms_and_include_literals() {
        let pool = ParserPool::new();
        let src = "<?php\nnamespace X;\nuse \\App\\A;\nuse App\\B as Bee;\nuse App\\Models\\{User, Post as P};\nuse function App\\helper;\nrequire_once __DIR__ . '/inc/x.php';\ninclude 'y.php';\n";
        let mut got = pool
            .parse(Language::Php, &RelPath::new("a.php"), src)
            .expect("parse php")
            .imports;
        got.sort();
        assert_eq!(
            got,
            [
                "/inc/x.php",
                "App\\A",
                "App\\B",
                "App\\Models\\Post",
                "App\\Models\\User",
                "function:App\\helper",
                "y.php"
            ]
        );
    }

    #[test]
    fn ruby_require_relative_is_marked_and_shell_backslash_dot_is_a_source() {
        let pool = ParserPool::new();
        let rb = pool
            .parse(
                Language::Ruby,
                &RelPath::new("a.rb"),
                "require \"json\"\nrequire_relative \"helper\"\nload 'x.rb'\n",
            )
            .expect("parse ruby");
        assert_eq!(rb.imports, ["json", "relative:helper", "x.rb"]);
        let sh = pool
            .parse(
                Language::Shell,
                &RelPath::new("t"),
                "#!/bin/sh\n: nvm.sh\n\\. ../../nvm.sh\n. ./a.sh\nsource b.sh\n",
            )
            .expect("parse shell");
        assert_eq!(sh.imports, ["../../nvm.sh", "./a.sh", "b.sh"]);
    }
}
