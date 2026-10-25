//! The code graph: import edges, centrality, and task-relevant ranking.
//!
//! Determinism is a feature. Same repo bytes must give the same graph and the
//! same ordering, every run, with no LLM and no embeddings in the loop. Use
//! fixed iteration counts and stable tie-breaks (path, then line) rather than
//! convergence thresholds or hash-map iteration order.
//!
//! Two ranking layers, kept separate so the expensive one stays cacheable:
//!   * **Static centrality** — PageRank over import edges. Cache it; it only
//!     changes when the graph does.
//!   * **Task personalization** — a restart vector derived from the task text,
//!     applied on top. Worth borrowing from aider's repo map, which weights
//!     identifiers named in the conversation ~10x, long specific identifiers
//!     ~10x, and files already in context ~50x.
//!
//! Import edges carry `Confidence::Scoped` (build-config-backed). Call edges
//! carry `Confidence::Syntactic` and must be labelled as such: measured
//! against a language server, name-matched call graphs recalled 66% on
//! TypeScript and 18% on Python. File-level import edges recalled 100%, which
//! is why file-level questions route here and symbol-level ones do not.

use crate::types::{CodeEdge, CodeFile, CodeSymbol, EdgeKind, FileId, SymbolKind};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub const PAGERANK_DAMPING: f64 = 0.85;
pub const PAGERANK_ITERATIONS: usize = 40;

#[derive(Debug, Default)]
pub struct CodeGraph {
    pub files: Vec<CodeFile>,
    pub symbols: Vec<CodeSymbol>,
    pub edges: Vec<CodeEdge>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Centrality {
    pub by_file: HashMap<FileId, f64>,
}

/// PageRank over file import edges (`importer -> imported`).
///
/// Files and edges are canonicalised before any floating-point addition. This
/// is deliberately sequential: changing the summation order changes the last
/// bits of a score and can consequently change a ranking.
pub fn compute_centrality(g: &CodeGraph) -> Centrality {
    let files = sorted_files(g);
    let n = files.len();
    if n == 0 {
        return Centrality::default();
    }

    let positions = file_positions(&files);
    let outgoing = import_adjacency(g, &positions, None);
    let mut scores = vec![1.0 / n as f64; n];
    let restart = scores.clone();
    run_pagerank(&outgoing, &restart, &mut scores);

    Centrality {
        by_file: files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.id, scores[i]))
            .collect(),
    }
}

/// Add task-specific restart and edge weights without modifying cached static
/// centrality. A file path mentioned in `task` is treated as already being in
/// context, because the public API intentionally has no separate context list.
pub fn rank_for_task(g: &CodeGraph, task: &str, base: &Centrality) -> Centrality {
    rank_and_match(g, task, base).0
}

/// `rank_for_task` and `match_task` in one pass over the symbols.
pub fn rank_and_match(g: &CodeGraph, task: &str, base: &Centrality) -> (Centrality, TaskMatch) {
    let task_words = split_words(task);
    let task_compact = compact_lower(task);
    let hits = symbol_hits(g, task, &task_words, &task_compact);
    let ranked = rank_with_hits(g, task, base, &task_words, &task_compact, &hits);
    let m = task_match(g, task, &task_words, &task_compact, &hits);
    (ranked, m)
}

fn task_match(
    g: &CodeGraph,
    task: &str,
    task_words: &BTreeSet<String>,
    task_compact: &str,
    hits: &[SymHit],
) -> TaskMatch {
    let mut m = match_task_files(g, task, task_words, task_compact);
    let mut exact_ids: HashSet<u32> = HashSet::new();
    for h in hits {
        m.symbols.insert(h.idx);
        if h.strength >= STRENGTH_EXACT {
            m.exact_files.insert(g.symbols[h.idx].file);
            exact_ids.insert(g.symbols[h.idx].id.0);
        }
    }
    if !m.exact_files.is_empty() {
        // The test file next to a file the task names (`quiet.go` ->
        // `quiet_test.go`, `a.ts` -> `a.test.ts`) is where its behaviour is
        // pinned down: same tier as callers.
        let keys: HashSet<(&str, String)> = g
            .files
            .iter()
            .filter(|f| m.exact_files.contains(&f.id))
            .map(|f| (f.path.dir(), stem_lower(f)))
            .collect();
        for f in &g.files {
            if m.exact_files.contains(&f.id) {
                continue;
            }
            let stem = stem_lower(f);
            let base = test_base(&stem);
            if base.is_some_and(|b| keys.contains(&(f.path.dir(), b.to_string()))) {
                m.caller_files.insert(f.id);
            }
        }
    }
    if !m.exact_files.is_empty() {
        // C-family: the header declaring the identifier and the file
        // implementing it are one unit; whichever of the two the exact match
        // landed in, the other half joins the caller tier.
        let pairs: Vec<FileId> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Pair)
            .filter_map(|e| {
                let (a, b) = (FileId(e.from), FileId(e.to));
                match (m.exact_files.contains(&a), m.exact_files.contains(&b)) {
                    (true, false) => Some(b),
                    (false, true) => Some(a),
                    _ => None,
                }
            })
            .collect();
        m.caller_files.extend(pairs);
    }
    // Interfaces the task is about: spelled out exactly, or named in a file
    // the task gives by path (`Provider` in `oauth/provider.go`). Their
    // implementing types (Go implicit satisfaction, `EdgeKind::Inherit`)
    // rank with the callers: "which types implement X" lives there.
    let mut focal: HashSet<u32> = hits
        .iter()
        .filter(|h| h.strength >= STRENGTH_EXACT)
        .map(|h| &g.symbols[h.idx])
        .filter(|s| s.kind == SymbolKind::Interface)
        .map(|s| s.id.0)
        .collect();
    if !m.exact_files.is_empty() {
        let tokens = task_tokens(task);
        focal.extend(
            g.symbols
                .iter()
                .filter(|s| {
                    s.kind == SymbolKind::Interface
                        && m.exact_files.contains(&s.file)
                        && tokens.contains_key(&compact_lower(&s.name))
                })
                .map(|s| s.id.0),
        );
    }
    if !focal.is_empty() {
        let implementors: HashSet<u32> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Inherit && focal.contains(&e.to))
            .map(|e| e.from)
            .collect();
        for (idx, symbol) in g.symbols.iter().enumerate() {
            if implementors.contains(&symbol.id.0) && !m.exact_files.contains(&symbol.file) {
                m.caller_files.insert(symbol.file);
                m.caller_symbols.insert(idx);
            }
        }
    }
    if !exact_ids.is_empty() {
        // Name-matched call edges into the identifier the task spells out:
        // their files are where "call sites of X" live.
        let callers: HashSet<u32> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Call && exact_ids.contains(&e.to))
            .map(|e| e.from)
            .collect();
        if !callers.is_empty() {
            for (idx, symbol) in g.symbols.iter().enumerate() {
                if callers.contains(&symbol.id.0) && !m.exact_files.contains(&symbol.file) {
                    m.caller_files.insert(symbol.file);
                    m.caller_symbols.insert(idx);
                }
            }
        }
    }
    package_siblings(g, &mut m);
    m
}

/// A Go package (directory) with at most this many files is read as a unit:
/// its other files join an exact file at the package tier. Larger packages
/// (`controller/` with 165 files) say nothing about any one file.
const PACKAGE_SIBLING_MAX: usize = 12;

/// Fill `m.package_files`: the other Go files of each small package that
/// contains an exact file, minus files already ranked higher.
fn package_siblings(g: &CodeGraph, m: &mut TaskMatch) {
    let is_go = |f: &CodeFile| f.language == Some(crate::types::Language::Go);
    let dirs: HashSet<&str> = g
        .files
        .iter()
        .filter(|f| is_go(f) && m.exact_files.contains(&f.id))
        .map(|f| f.path.dir())
        .collect();
    if dirs.is_empty() {
        return;
    }
    let mut members: HashMap<&str, Vec<FileId>> = HashMap::new();
    for f in g.files.iter().filter(|f| is_go(f)) {
        let dir = f.path.dir();
        if dirs.contains(dir) {
            members.entry(dir).or_default().push(f.id);
        }
    }
    for ids in members
        .values()
        .filter(|ids| ids.len() <= PACKAGE_SIBLING_MAX)
    {
        for id in ids {
            if !m.exact_files.contains(id) && !m.caller_files.contains(id) {
                m.package_files.insert(*id);
            }
        }
    }
}

/// A sub-word that names at least this share of all symbols (and at least
/// `GENERIC_MIN_DF` of them) cannot select a symbol on its own: in a repo
/// where `channel` occurs in thousands of names it says nothing.
const GENERIC_MIN_DF: usize = 40;
const GENERIC_DF_DIVISOR: usize = 64;
/// A one-word symbol (`Form`, `call`) matching one task word is only a strong
/// hit when that word is this rare among symbol names.
const RARE_DF: usize = 8;
/// Function words of a task description. Backs up the df test on small repos,
/// where no word reaches `GENERIC_MIN_DF`.
const STOP_WORDS: &[&str] = &[
    "the", "how", "does", "what", "where", "when", "which", "that", "this", "with", "from", "into",
    "and", "for", "are", "was", "not", "all", "any", "can", "should", "would", "could", "there",
    "their", "about", "have", "has", "been", "will", "you", "why", "who", "get", "set", "use",
    "add", "make", "new", "fix", "need", "want", "work", "works", "code", "file", "files",
];

/// Hit strengths. A file's boost is `10^strength` of its strongest symbol (not
/// a product over symbols: a file with seven weak hits must not outrank the
/// file that defines the identifier the task spells out).
const STRENGTH_WEAK: f64 = 1.0;
const STRENGTH_SPLIT: f64 = 2.0;
const STRENGTH_EXPLICIT: f64 = 3.0;
const STRENGTH_EXACT: f64 = 4.0;

/// One symbol that matches the task.
struct SymHit {
    idx: usize,
    strength: f64,
}

fn word_set_has(words: &BTreeSet<String>, w: &str) -> bool {
    words.contains(w)
}

/// Identifier-looking tokens of the raw task (`processChannelError`,
/// `snake_case`) vs plain words, compacted and lowercased. The flag says the
/// token is identifier-shaped (camel/snake), so an exact name match counts even
/// when its sub-words are common.
fn task_tokens(task: &str) -> HashMap<String, bool> {
    let mut out: HashMap<String, bool> = HashMap::new();
    for tok in task.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let compact = compact_lower(tok);
        if compact.len() < 4 {
            continue;
        }
        let chars: Vec<char> = tok.chars().collect();
        let shaped = tok.contains('_')
            || chars
                .windows(2)
                .any(|w| w[0].is_lowercase() && w[1].is_uppercase());
        let e = out.entry(compact).or_insert(false);
        *e |= shaped;
    }
    out
}

fn symbol_hits(
    g: &CodeGraph,
    task: &str,
    task_words: &BTreeSet<String>,
    task_compact: &str,
) -> Vec<SymHit> {
    struct Cand {
        idx: usize,
        compact_len: usize,
        explicit: bool,
        exact: Option<bool>,
        /// Sub-words of the name (>= 3 bytes) that the task also contains.
        matched: Vec<String>,
        meaningful: usize,
    }
    let tokens = task_tokens(task);
    let mut buf = String::new();
    let mut cands: Vec<Cand> = Vec::new();
    let mut df: HashMap<String, usize> = HashMap::new();
    for (idx, symbol) in g.symbols.iter().enumerate() {
        buf.clear();
        buf.extend(
            symbol
                .name
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase),
        );
        let explicit = buf.len() >= 3 && task_compact.contains(buf.as_str());
        let exact = tokens.get(buf.as_str()).copied();
        // Every name word is a contiguous run of `buf`, so a task word that
        // is not a substring of it cannot match; skip the allocation-heavy
        // `split_words` for the vast majority of symbols (ASCII only, so
        // lowercase folding is identical).
        if buf.is_ascii()
            && !explicit
            && exact.is_none()
            && !task_words
                .iter()
                .any(|w| w.len() >= 3 && buf.contains(w.as_str()))
        {
            continue;
        }
        let words = split_words(&symbol.name);
        let meaningful: Vec<&String> = words.iter().filter(|w| w.len() >= 3).collect();
        let matched: Vec<String> = meaningful
            .iter()
            .filter(|w| word_set_has(task_words, w))
            .map(|w| (*w).clone())
            .collect();
        for w in &matched {
            *df.entry(w.clone()).or_default() += 1;
        }
        if !explicit && exact.is_none() && matched.is_empty() {
            continue;
        }
        cands.push(Cand {
            idx,
            compact_len: buf.len(),
            explicit,
            exact,
            matched,
            meaningful: meaningful.len(),
        });
    }
    let threshold = GENERIC_MIN_DF.max(g.symbols.len() / GENERIC_DF_DIVISOR);
    let generic = |w: &str| STOP_WORDS.contains(&w) || df.get(w).copied().unwrap_or(0) >= threshold;
    let mut hits = Vec::new();
    // When every task word is frequent in this repo, the df test would leave
    // nothing; fall back to symbols that carry two (or all) of the task's
    // non-stop words, at the weakest strength.
    let mut fallback = Vec::new();
    for c in cands {
        let content = c
            .matched
            .iter()
            .filter(|w| !STOP_WORDS.contains(&w.as_str()))
            .count();
        if content >= 1 && content >= c.meaningful.min(2) {
            fallback.push(SymHit {
                idx: c.idx,
                strength: STRENGTH_WEAK,
            });
        }
        let specific = c.matched.iter().filter(|w| !generic(w)).count();
        // A plain word (`call`, `answer`) equal to a symbol name is common
        // English, not an identifier the task spells out: only camelCase /
        // snake_case tokens get the exact tier.
        let strength = if c.exact == Some(true) {
            STRENGTH_EXACT
        } else if c.explicit
            && c.compact_len >= 4
            && (specific > 0 || (c.matched.is_empty() && c.meaningful >= 2))
        {
            STRENGTH_EXPLICIT
        } else if specific >= 2
            || (specific == 1
                && c.meaningful == 1
                && c.matched
                    .iter()
                    .any(|w| df.get(w).copied().unwrap_or(0) <= RARE_DF))
        {
            STRENGTH_SPLIT
        } else if specific >= 1 {
            STRENGTH_WEAK
        } else {
            continue;
        };
        hits.push(SymHit {
            idx: c.idx,
            strength,
        });
    }
    if hits.is_empty() {
        return fallback;
    }
    hits
}

fn rank_with_hits(
    g: &CodeGraph,
    task: &str,
    base: &Centrality,
    task_words: &BTreeSet<String>,
    task_compact: &str,
    hits: &[SymHit],
) -> Centrality {
    let files = sorted_files(g);
    let n = files.len();
    if n == 0 {
        return Centrality::default();
    }
    let mut boosts = vec![1.0; n];
    let positions = file_positions(&files);
    // Per file: its strongest hit sets the boost; the number of hits adds a
    // small logarithmic bonus. Never a product over symbols.
    let mut best: Vec<(f64, usize)> = vec![(0.0, 0); n];
    for h in hits {
        let Some(&i) = positions.get(&g.symbols[h.idx].file) else {
            continue;
        };
        best[i].0 = best[i].0.max(h.strength);
        best[i].1 += 1;
    }
    for (i, &(strength, count)) in best.iter().enumerate() {
        if count > 0 {
            boosts[i] = 10f64.powf(strength) * (1.0 + (count as f64).ln());
        }
    }

    let task_lower = task.to_lowercase();
    let context_files: BTreeSet<FileId> = files
        .iter()
        .filter(|file| file_mentioned(&task_lower, file))
        .map(|file| file.id)
        .collect();
    // A file whose name words all appear in the task (`loop_guard.rs` for
    // "loop guard") is task-relevant even when the task never types the
    // extension. Weaker than an explicit path (10x, not 50x).
    for file in &files {
        if context_files.contains(&file.id) || !stem_matches(file, task_words, task_compact) {
            continue;
        }
        if let Some(&i) = positions.get(&file.id) {
            boosts[i] *= 10.0;
        }
    }
    for file in &context_files {
        if let Some(&i) = positions.get(file) {
            // Giving the source more restart mass makes its 50x outgoing
            // emphasis observable even when all of its import edges have the
            // same weight (where ordinary PageRank normalization would
            // otherwise cancel a uniform multiplier).
            boosts[i] *= 50.0;
        }
    }

    let mut restart: Vec<f64> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            base.by_file
                .get(&file.id)
                .copied()
                .unwrap_or(1.0 / n as f64)
                * boosts[i]
        })
        .collect();
    normalize(&mut restart);

    let outgoing = import_adjacency(g, &positions, Some(&context_files));
    let mut scores = restart.clone();
    run_pagerank(&outgoing, &restart, &mut scores);

    Centrality {
        by_file: files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.id, scores[i]))
            .collect(),
    }
}

/// Files that import `target`, ranked. This is the query that replaces a
/// language server for file-level impact analysis.
pub fn dependents(g: &CodeGraph, target: FileId) -> Vec<FileId> {
    reachable_imports(g, target, true)
}

pub fn dependencies(g: &CodeGraph, target: FileId) -> Vec<FileId> {
    reachable_imports(g, target, false)
}

/// The other half of a C-family header/source pair ([`EdgeKind::Pair`],
/// either direction), sorted. Empty for every other language.
pub fn paired_files(g: &CodeGraph, file: FileId) -> Vec<FileId> {
    let mut out: Vec<FileId> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Pair)
        .filter_map(|e| {
            if e.from == file.0 {
                Some(FileId(e.to))
            } else if e.to == file.0 {
                Some(FileId(e.from))
            } else {
                None
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn sorted_files(g: &CodeGraph) -> Vec<&CodeFile> {
    let mut files: Vec<&CodeFile> = g.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    files
}

fn file_positions(files: &[&CodeFile]) -> BTreeMap<FileId, usize> {
    files
        .iter()
        .enumerate()
        .map(|(position, file)| (file.id, position))
        .collect()
}

fn import_adjacency(
    g: &CodeGraph,
    positions: &BTreeMap<FileId, usize>,
    context_files: Option<&BTreeSet<FileId>>,
) -> Vec<Vec<(usize, f64)>> {
    let mut aggregated: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    // Namespace fan-out and name-matched calls are not real file references.
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (from, to) = (FileId(edge.from), FileId(edge.to));
        let (Some(&from_i), Some(&to_i)) = (positions.get(&from), positions.get(&to)) else {
            continue;
        };
        let context_multiplier = if context_files.is_some_and(|ids| ids.contains(&from)) {
            50_u64
        } else {
            1
        };
        let weight = u64::from(edge.weight.max(1)).saturating_mul(context_multiplier);
        let slot = aggregated.entry((from_i, to_i)).or_default();
        *slot = slot.saturating_add(weight);
    }

    let mut outgoing = vec![Vec::new(); positions.len()];
    for ((from, to), weight) in aggregated {
        outgoing[from].push((to, weight as f64));
    }
    outgoing
}

fn run_pagerank(outgoing: &[Vec<(usize, f64)>], restart: &[f64], scores: &mut Vec<f64>) {
    let n = scores.len();
    // Edge weights do not change between iterations; same summation order as
    // before, so the scores are bit-identical.
    let total_weights: Vec<f64> = outgoing
        .iter()
        .map(|edges| edges.iter().map(|(_, weight)| *weight).sum())
        .collect();
    for _ in 0..PAGERANK_ITERATIONS {
        let dangling: f64 = (0..n)
            .filter(|&i| outgoing[i].is_empty())
            .map(|i| scores[i])
            .sum();
        let mut next: Vec<f64> = restart
            .iter()
            .map(|&value| (1.0 - PAGERANK_DAMPING) * value)
            .collect();

        for from in 0..n {
            if outgoing[from].is_empty() {
                continue;
            }
            let total_weight = total_weights[from];
            for &(to, weight) in &outgoing[from] {
                next[to] += PAGERANK_DAMPING * scores[from] * weight / total_weight;
            }
        }
        for i in 0..n {
            next[i] += PAGERANK_DAMPING * dangling * restart[i];
        }
        *scores = next;
    }
}

fn normalize(values: &mut [f64]) {
    let sum: f64 = values.iter().sum();
    if sum > 0.0 && sum.is_finite() {
        for value in values {
            *value /= sum;
        }
    }
}

fn compact_lower(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// `needle` occurs in `haystack` delimited by non-path characters on both
/// sides (so `a.rs` is not "mentioned" by `data.rs`). Both lowercase.
pub fn mentions_path(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_path_char(c));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_path_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn file_mentioned(task_lower: &str, file: &CodeFile) -> bool {
    let path = file.path.as_str().to_lowercase();
    let name = file.path.file_name().to_lowercase();
    mentions_path(task_lower, &path) || mentions_path(task_lower, &name)
}

fn stem_matches(file: &CodeFile, task_words: &BTreeSet<String>, task_compact: &str) -> bool {
    let name = file.path.file_name();
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let compact = compact_lower(stem);
    if compact.len() < 4 {
        return false;
    }
    let words = split_words(stem);
    let meaningful: Vec<&String> = words.iter().filter(|w| w.len() >= 3).collect();
    let words_hit =
        !meaningful.is_empty() && meaningful.iter().all(|w| task_words.contains(w.as_str()));
    words_hit || (compact.len() >= 6 && task_compact.contains(&compact))
}

fn stem_lower(f: &CodeFile) -> String {
    let name = f.path.file_name();
    name.split('.').next().unwrap_or(name).to_lowercase()
}

/// `quiet_test` / `quiet.test` (first segment `quiet`) / `test_quiet` /
/// `QuietTest` -> `quiet`.
fn test_base(stem: &str) -> Option<&str> {
    stem.strip_suffix("_test")
        .or_else(|| stem.strip_suffix("test"))
        .or_else(|| stem.strip_prefix("test_"))
        .filter(|b| !b.is_empty())
}

/// Files and symbols that the task text actually names (strict predicates:
/// path/name boundary match, file-stem words, whole-name symbol hits). Empty
/// result means the task matched nothing and any ranking is pure centrality.
#[derive(Clone, Debug, Default)]
pub struct TaskMatch {
    pub files: BTreeSet<FileId>,
    /// Indexes into `CodeGraph::symbols`.
    pub symbols: BTreeSet<usize>,
    /// Files that define a symbol named exactly as an identifier in the task,
    /// or that the task names by path: they lead every ranking.
    pub exact_files: BTreeSet<FileId>,
    /// Files with a (name-matched) call into one of those symbols.
    pub caller_files: BTreeSet<FileId>,
    /// The calling symbols themselves (indexes into `CodeGraph::symbols`).
    pub caller_symbols: BTreeSet<usize>,
    /// The rest of a small Go package that holds an exact file (package doc,
    /// shared types, registry): ranked right after the callers.
    pub package_files: BTreeSet<FileId>,
}

impl TaskMatch {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.symbols.is_empty()
    }
}

fn match_task_files(
    g: &CodeGraph,
    task: &str,
    task_words: &BTreeSet<String>,
    task_compact: &str,
) -> TaskMatch {
    let task_lower = task.to_lowercase();
    let mut out = TaskMatch::default();
    for file in &g.files {
        let mentioned = file_mentioned(&task_lower, file);
        if mentioned || stem_matches(file, task_words, task_compact) {
            out.files.insert(file.id);
        }
        // Only a full path is unambiguous (`provider.go` may name ten files).
        if mentioned && mentions_path(&task_lower, &file.path.as_str().to_lowercase()) {
            out.exact_files.insert(file.id);
        }
    }
    out
}

pub fn match_task(g: &CodeGraph, task: &str) -> TaskMatch {
    let task_words = split_words(task);
    let task_compact = compact_lower(task);
    let hits = symbol_hits(g, task, &task_words, &task_compact);
    task_match(g, task, &task_words, &task_compact, &hits)
}

/// Lowercase sub-words of identifiers in `text`: split on non-alphanumerics,
/// camelCase and acronym boundaries (`parse_http_URL` → `http`, `parse`, `url`).
pub fn split_words(text: &str) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.insert(current.to_lowercase());
                current.clear();
            }
            continue;
        }
        let previous = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
        let next = chars.get(i + 1).copied();
        let camel_boundary =
            ch.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_numeric());
        let acronym_boundary = ch.is_uppercase()
            && previous.is_some_and(char::is_uppercase)
            && next.is_some_and(char::is_lowercase);
        if (camel_boundary || acronym_boundary) && !current.is_empty() {
            words.insert(current.to_lowercase());
            current.clear();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.insert(current.to_lowercase());
    }
    words
}

/// Fingerprint of everything `compute_centrality` reads.
fn centrality_fingerprint(g: &CodeGraph) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    g.files.len().hash(&mut h);
    for f in &g.files {
        f.id.hash(&mut h);
        f.path.as_str().hash(&mut h);
    }
    for e in g.edges.iter().filter(|e| e.kind == EdgeKind::Import) {
        (e.from, e.to, e.weight).hash(&mut h);
    }
    h.finish()
}

/// PageRank is the expensive part of `dependents`/`dependencies`; the answer
/// only changes when files or import edges do, so keep the last few results
/// keyed by a fingerprint of those inputs instead of recomputing per call.
pub fn cached_centrality(g: &CodeGraph) -> std::sync::Arc<Centrality> {
    use std::sync::{Arc, Mutex};
    static CACHE: Mutex<Vec<(u64, Arc<Centrality>)>> = Mutex::new(Vec::new());
    let key = centrality_fingerprint(g);
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, c)) = cache.iter().find(|(k, _)| *k == key) {
            return Arc::clone(c);
        }
    }
    let computed = Arc::new(compute_centrality(g));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 4 {
        cache.remove(0);
    }
    cache.push((key, Arc::clone(&computed)));
    computed
}

fn reachable_imports(g: &CodeGraph, target: FileId, reverse: bool) -> Vec<FileId> {
    let known: BTreeSet<FileId> = g.files.iter().map(|file| file.id).collect();
    if !known.contains(&target) {
        return Vec::new();
    }

    let mut adjacency: BTreeMap<FileId, BTreeSet<FileId>> = BTreeMap::new();
    // Namespace fan-out and name-matched calls are not real file references.
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (from, to) = (FileId(edge.from), FileId(edge.to));
        if !known.contains(&from) || !known.contains(&to) {
            continue;
        }
        let (source, destination) = if reverse { (to, from) } else { (from, to) };
        adjacency.entry(source).or_default().insert(destination);
    }

    let mut visited = BTreeSet::new();
    let mut frontier = vec![target];
    while let Some(current) = frontier.pop() {
        if let Some(next) = adjacency.get(&current) {
            // Reverse push order so the smallest ID is visited first. The
            // final score sort is authoritative, but traversal is stable too.
            for &file in next.iter().rev() {
                if file != target && visited.insert(file) {
                    frontier.push(file);
                }
            }
        }
    }

    let centrality = cached_centrality(g);
    let files_by_id: BTreeMap<FileId, &CodeFile> =
        g.files.iter().map(|file| (file.id, file)).collect();
    let first_line: BTreeMap<FileId, u32> =
        g.symbols.iter().fold(BTreeMap::new(), |mut lines, symbol| {
            lines
                .entry(symbol.file)
                .and_modify(|line| *line = (*line).min(symbol.start_line))
                .or_insert(symbol.start_line);
            lines
        });
    let mut result: Vec<FileId> = visited.into_iter().collect();
    result.sort_by(|a, b| {
        let score_a = centrality.by_file.get(a).copied().unwrap_or(0.0);
        let score_b = centrality.by_file.get(b).copied().unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                files_by_id
                    .get(a)
                    .map(|f| &f.path)
                    .cmp(&files_by_id.get(b).map(|f| &f.path))
            })
            .then_with(|| {
                first_line
                    .get(a)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&first_line.get(b).copied().unwrap_or(0))
            })
            .then_with(|| a.cmp(b))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CodeEdge, CodeFile, CodeSymbol, Confidence, Language, RelPath, SymbolId, SymbolKind,
    };

    fn file(id: u32, path: &str) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::Rust),
            loc: 10,
            sha: String::new(),
        }
    }

    fn import(from: u32, to: u32) -> CodeEdge {
        CodeEdge {
            from,
            to,
            kind: EdgeKind::Import,
            weight: 1,
            confidence: Confidence::Scoped,
        }
    }

    fn graph(edges: Vec<CodeEdge>) -> CodeGraph {
        CodeGraph {
            files: vec![file(2, "c.rs"), file(0, "a.rs"), file(1, "b.rs")],
            symbols: Vec::new(),
            edges,
        }
    }

    fn ranked(centrality: &Centrality) -> Vec<FileId> {
        let mut values: Vec<(FileId, f64)> = centrality
            .by_file
            .iter()
            .map(|(&id, &score)| (id, score))
            .collect();
        values.sort_by(|(a_id, a), (b_id, b)| {
            b.partial_cmp(a)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a_id.cmp(b_id))
        });
        values.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn pagerank_orders_a_three_node_chain() {
        let centrality = compute_centrality(&graph(vec![import(0, 1), import(1, 2)]));
        assert_eq!(ranked(&centrality), vec![FileId(2), FileId(1), FileId(0)]);
    }

    #[test]
    fn pagerank_ignores_namespace_fanout_and_name_matched_calls() {
        let imports_only = graph(vec![import(0, 1)]);
        let mixed = graph(vec![
            import(0, 1),
            CodeEdge {
                from: 0,
                to: 2,
                kind: EdgeKind::Namespace,
                weight: 4,
                confidence: Confidence::Scoped,
            },
            CodeEdge {
                from: 1,
                to: 0,
                kind: EdgeKind::Call,
                weight: 9,
                confidence: Confidence::Syntactic,
            },
        ]);
        assert_eq!(
            compute_centrality(&imports_only),
            compute_centrality(&mixed)
        );
        assert_eq!(
            dependencies(&mixed, FileId(0)),
            dependencies(&imports_only, FileId(0))
        );
        assert!(
            !dependencies(&mixed, FileId(0)).contains(&FileId(2)),
            "namespace edges are not file dependencies"
        );
        assert!(
            dependents(&mixed, FileId(0)).is_empty(),
            "a name-matched call into a file is not a dependent"
        );
    }

    #[test]
    fn pagerank_is_exactly_deterministic() {
        let g = graph(vec![import(1, 2), import(0, 1)]);
        let first = compute_centrality(&g);
        let second = compute_centrality(&g);
        assert_eq!(first, second);
        assert_eq!(ranked(&first), ranked(&second));
    }

    #[test]
    fn dangling_nodes_are_finite_and_do_not_leak_mass() {
        let centrality = compute_centrality(&graph(vec![import(0, 1)]));
        let sum: f64 = centrality.by_file.values().sum();
        assert!(centrality.by_file.values().all(|score| score.is_finite()));
        assert!((sum - 1.0).abs() < 1e-12, "PageRank mass was {sum}");
    }

    #[test]
    fn cycles_terminate_and_queries_exclude_the_target() {
        let g = graph(vec![import(0, 1), import(1, 2), import(2, 0)]);
        assert_eq!(dependencies(&g, FileId(0)).len(), 2);
        assert!(!dependencies(&g, FileId(0)).contains(&FileId(0)));
    }

    #[test]
    fn dependency_directions_are_correct_and_transitive() {
        let g = graph(vec![import(0, 1), import(1, 2)]);
        let deps = dependencies(&g, FileId(0));
        assert!(deps.contains(&FileId(1)));
        assert!(deps.contains(&FileId(2)));
        let users = dependents(&g, FileId(2));
        assert!(users.contains(&FileId(0)));
        assert!(users.contains(&FileId(1)));
        assert!(dependents(&g, FileId(0)).is_empty());
        assert!(dependencies(&g, FileId(2)).is_empty());
    }

    #[test]
    fn tokenizer_splits_camel_case_snake_case_and_acronyms() {
        assert_eq!(
            split_words("getUserName parse_http_URL"),
            ["get", "http", "name", "parse", "url", "user"]
                .map(str::to_string)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn personalization_promotes_matching_file_without_mutating_base() {
        let mut g = graph(vec![]);
        g.symbols = vec![
            CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "helper".into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 1,
                end_line: 1,
                exported: false,
            },
            CodeSymbol {
                id: SymbolId(1),
                file: FileId(2),
                name: "processStripeWebhook".into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 7,
                end_line: 8,
                exported: true,
            },
        ];
        let base = compute_centrality(&g);
        let snapshot = base.clone();
        let first = rank_for_task(&g, "fix the stripe webhook processing", &base);
        let second = rank_for_task(&g, "fix the stripe webhook processing", &base);

        assert_eq!(ranked(&first)[0], FileId(2));
        assert_eq!(first, second);
        assert_eq!(base, snapshot);
    }

    #[test]
    fn mentions_path_requires_boundaries() {
        assert!(mentions_path("edit a.rs now", "a.rs"));
        assert!(mentions_path("see src/a.rs.", "src/a.rs"));
        assert!(mentions_path("see x/src/a.rs", "src/a.rs"));
        assert!(!mentions_path("edit data.rs now", "a.rs"));
        assert!(!mentions_path("edit a.rs2", "a.rs"));
        assert!(!mentions_path("edit my-a.rs", "a.rs"));
        assert!(mentions_path("data.rs then a.rs", "a.rs"));
    }

    #[test]
    fn substring_file_name_is_not_in_context() {
        // `data.rs` contains `a.rs`; a task naming only data.rs must not pull
        // a.rs into the 50x context set.
        let g = CodeGraph {
            files: vec![file(0, "a.rs"), file(1, "data.rs"), file(2, "z.rs")],
            symbols: Vec::new(),
            edges: vec![import(0, 2), import(1, 2)],
        };
        let m = match_task(&g, "please edit data.rs");
        assert_eq!(m.files, BTreeSet::from([FileId(1)]));
        let m = match_task(&g, "please edit a.rs");
        assert_eq!(m.files, BTreeSet::from([FileId(0)]));
    }

    #[test]
    fn file_stem_words_match_task_without_extension() {
        let g = CodeGraph {
            files: vec![file(0, "src/loop_guard.rs"), file(1, "src/server.rs")],
            symbols: Vec::new(),
            edges: Vec::new(),
        };
        let m = match_task(&g, "where does the loop guard block repeats");
        assert!(m.files.contains(&FileId(0)));
        assert!(!m.files.contains(&FileId(1)));
        let base = compute_centrality(&g);
        let r = rank_for_task(&g, "where does the loop guard block repeats", &base);
        assert_eq!(ranked(&r)[0], FileId(0));
    }

    #[test]
    fn gibberish_task_matches_nothing() {
        let g = graph(vec![import(0, 1)]);
        assert!(match_task(&g, "zzzz qqqq nonexistent thing").is_empty());
    }

    #[test]
    fn cached_dependents_equal_fresh_and_track_graph_changes() {
        let g1 = graph(vec![import(0, 1), import(2, 1)]);
        let first = dependents(&g1, FileId(1));
        assert_eq!(first, dependents(&g1, FileId(1)));
        let g2 = graph(vec![import(0, 1)]);
        assert_eq!(dependents(&g2, FileId(1)), vec![FileId(0)]);
        assert_eq!(first.len(), 2);
    }

    #[test]
    fn rank_and_match_equals_separate_rank_and_match_passes() {
        let mut g = graph(vec![import(0, 1), import(2, 1), import(0, 2)]);
        let names = [
            "processStripeWebhook",
            "StripeWebhook",
            "helper",
            "run",
            "JsonSerializer",
            "serialize",
            "ErrorHandler",
            "über_cluster",
            "HTTPServer",
            "tie",
            "tie",
            "x",
        ];
        g.symbols = names
            .iter()
            .enumerate()
            .map(|(i, name)| CodeSymbol {
                id: SymbolId(i as u32),
                file: FileId([0, 1, 2][i % 3]),
                name: (*name).into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                // Same file+line for the two `tie` symbols: ordering falls
                // through to name and id.
                start_line: if *name == "tie" { 5 } else { i as u32 + 1 },
                end_line: 20,
                exported: true,
            })
            .collect();
        g.symbols[10].file = g.symbols[9].file;
        let base = compute_centrality(&g);
        for task in [
            "fix the stripe webhook processing",
            "look at a.rs and the JsonSerializer",
            "zzzz qqqq nonexistent thing",
            "!!! ??? ###",
            "",
            "tie tie tie",
            "how does the json serializer handle errors in über cluster",
            "HTTPServer run x",
        ] {
            let (ranked_both, matched_both) = rank_and_match(&g, task, &base);
            assert_eq!(ranked_both, rank_for_task(&g, task, &base), "rank: {task}");
            let separate = match_task(&g, task);
            assert_eq!(matched_both.files, separate.files, "files: {task}");
            assert_eq!(matched_both.symbols, separate.symbols, "symbols: {task}");
        }
        let (_, none) = rank_and_match(&g, "zzzz qqqq nonexistent thing", &base);
        assert!(none.is_empty());
        let (_, some) = rank_and_match(&g, "fix the stripe webhook processing", &base);
        assert!(!some.symbols.is_empty());
    }

    fn sym(i: u32, file: u32, name: &str) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(i),
            file: FileId(file),
            name: name.into(),
            kind: SymbolKind::Function,
            signature: String::new(),
            start_line: i + 1,
            end_line: i + 1,
            exported: true,
        }
    }

    fn named_graph(paths: &[&str], symbols: &[(u32, &str)]) -> CodeGraph {
        CodeGraph {
            files: paths
                .iter()
                .enumerate()
                .map(|(i, p)| file(i as u32, p))
                .collect(),
            symbols: symbols
                .iter()
                .enumerate()
                .map(|(i, (f, n))| sym(i as u32, *f, n))
                .collect(),
            edges: Vec::new(),
        }
    }

    #[test]
    fn many_weak_hits_do_not_outrank_the_exact_identifier_file() {
        // channel.go has seven symbols that only share common sub-words with
        // the task; relay.go defines the identifier the task spells out.
        let mut syms: Vec<(u32, &str)> = vec![
            (0, "ChannelError"),
            (0, "channelErrorCode"),
            (0, "processChannel"),
            (0, "ErrorChannel"),
            (0, "channelProcess"),
            (0, "processError"),
            (0, "errorProcessChannel"),
            (1, "processChannelError"),
            (1, "relayHelper"),
        ];
        syms.push((2, "unrelated"));
        let task = "find call sites of processChannelError";
        let g = named_graph(&["x.go", "relay.go", "z.go"], &syms);
        let base = compute_centrality(&g);
        let (ranked, m) = rank_and_match(&g, task, &base);
        assert_eq!(super::tests::ranked(&ranked)[0], FileId(1), "{ranked:?}");
        assert!(m.exact_files.contains(&FileId(1)));
        assert!(!m.exact_files.contains(&FileId(0)));
        // Not a product over symbols: doubling the weak hits in x.go moves
        // its score by a logarithmic bonus, not by another 10^7.
        let mut more = syms.clone();
        more.extend(syms[..7].iter().copied());
        let g2 = named_graph(&["x.go", "relay.go", "z.go"], &more);
        let (ranked2, _) = rank_and_match(&g2, task, &compute_centrality(&g2));
        let r1 = ranked.by_file[&FileId(0)] / ranked.by_file[&FileId(1)];
        let r2 = ranked2.by_file[&FileId(0)] / ranked2.by_file[&FileId(1)];
        assert!(r2 / r1 < 1.5, "{r1} -> {r2}");
        assert_eq!(super::tests::ranked(&ranked2)[0], FileId(1));
    }

    #[test]
    fn plain_word_equal_to_a_symbol_name_is_not_the_exact_tier() {
        let g = named_graph(&["a.go", "b.go"], &[(0, "call"), (1, "loadConfig")]);
        let m = match_task(&g, "where is the call to loadConfig");
        assert!(m.exact_files.contains(&FileId(1)));
        assert!(!m.exact_files.contains(&FileId(0)), "{:?}", m.exact_files);
    }

    #[test]
    fn frequent_sub_word_alone_does_not_select_a_symbol() {
        // `channel` names 60 symbols (>= GENERIC_MIN_DF): it says nothing.
        let names: Vec<String> = (0..60).map(|i| format!("channel{i}Thing")).collect();
        let mut syms: Vec<(u32, &str)> = names.iter().map(|n| (0, n.as_str())).collect();
        syms.push((1, "grokSpendingLimit"));
        let g = named_graph(&["noise.go", "limit.go"], &syms);
        let m = match_task(&g, "how does the channel spending limit work");
        let files: BTreeSet<FileId> = m.symbols.iter().map(|&i| g.symbols[i].file).collect();
        assert!(files.contains(&FileId(1)), "{files:?}");
        assert!(
            !files.contains(&FileId(0)),
            "generic channel matched: {files:?}"
        );
        // Stop words never match on their own, even in a tiny repo.
        let g = named_graph(&["a.go"], &[(0, "theHow")]);
        assert!(match_task(&g, "how does the thing work").symbols.is_empty());
    }

    #[test]
    fn callers_and_sibling_tests_of_an_exact_identifier_form_the_next_tier() {
        let mut g = named_graph(
            &[
                "svc/quiet.go",
                "svc/quiet_test.go",
                "api/use.go",
                "api/other.go",
            ],
            &[
                (0, "ViaQuietDropped"),
                (1, "TestThing"),
                (2, "handler"),
                (3, "other"),
            ],
        );
        g.edges.push(CodeEdge {
            from: 2,
            to: 0,
            kind: EdgeKind::Call,
            weight: 1,
            confidence: Confidence::Syntactic,
        });
        let m = match_task(&g, "rename ViaQuietDropped");
        assert_eq!(m.exact_files, BTreeSet::from([FileId(0)]));
        assert_eq!(m.caller_files, BTreeSet::from([FileId(1), FileId(2)]));
        assert!(m.caller_symbols.contains(&2));
        // A file named by full path is exact; a bare file name is not
        // (it may name many files).
        let m = match_task(&g, "look at svc/quiet.go");
        assert!(m.exact_files.contains(&FileId(0)));
        let m = match_task(&g, "look at other.go");
        assert!(m.files.contains(&FileId(3)) && m.exact_files.is_empty());
    }

    #[test]
    fn header_source_pair_of_an_exact_identifier_joins_the_caller_tier() {
        let mut g = named_graph(
            &["src/object.h", "src/object.c", "src/other.c"],
            &[(0, "redisObjectRefcount"), (2, "unrelated")],
        );
        g.edges.push(CodeEdge {
            from: 0,
            to: 1,
            kind: EdgeKind::Pair,
            weight: 1,
            confidence: Confidence::Syntactic,
        });
        let m = match_task(&g, "why does redisObjectRefcount leak");
        assert_eq!(m.exact_files, BTreeSet::from([FileId(0)]));
        assert_eq!(m.caller_files, BTreeSet::from([FileId(1)]));
        assert_eq!(paired_files(&g, FileId(1)), vec![FileId(0)]);
        assert_eq!(paired_files(&g, FileId(0)), vec![FileId(1)]);
        assert!(paired_files(&g, FileId(2)).is_empty());
        // Pair edges are not includes.
        assert!(dependencies(&g, FileId(1)).is_empty());
        assert!(dependents(&g, FileId(1)).is_empty());
    }

    fn go_graph(paths: &[&str], symbols: &[(u32, &str)]) -> CodeGraph {
        let mut g = named_graph(paths, symbols);
        for f in &mut g.files {
            f.language = Some(Language::Go);
        }
        g
    }

    #[test]
    fn implementors_of_a_named_interface_rank_with_the_callers() {
        let mut g = go_graph(
            &[
                "oauth/provider.go",
                "oauth/github.go",
                "other/impl.go",
                "x/noise.go",
            ],
            &[
                (0, "Provider"),
                (1, "GitHubProvider"),
                (2, "OtherImpl"),
                (3, "noise"),
            ],
        );
        g.symbols[0].kind = SymbolKind::Interface;
        g.symbols[1].kind = SymbolKind::Struct;
        g.symbols[2].kind = SymbolKind::Struct;
        for from in [1, 2] {
            g.edges.push(CodeEdge {
                from,
                to: 0,
                kind: EdgeKind::Inherit,
                weight: 1,
                confidence: Confidence::Syntactic,
            });
        }
        // Named by path: `Provider` in `oauth/provider.go` is the interface.
        let m = match_task(
            &g,
            "which types implement the `Provider` interface in oauth/provider.go?",
        );
        assert_eq!(m.exact_files, BTreeSet::from([FileId(0)]));
        assert!(m.caller_files.contains(&FileId(1)) && m.caller_files.contains(&FileId(2)));
        assert!(m.caller_symbols.contains(&1) && m.caller_symbols.contains(&2));
        assert!(!m.caller_files.contains(&FileId(3)));
        // Without the path (plain word `Provider`, not exact): no implementor tier.
        let m = match_task(&g, "which types implement Provider");
        assert!(m.caller_files.is_empty(), "{:?}", m.caller_files);
    }

    #[test]
    fn small_go_package_of_an_exact_file_is_the_package_tier() {
        let mut paths = vec![
            "push/quiet.go",
            "push/doc.go",
            "push/receipt.go",
            "far/x.go",
        ];
        let big: Vec<String> = (0..=PACKAGE_SIBLING_MAX)
            .map(|i| format!("big/f{i}.go"))
            .collect();
        paths.extend(big.iter().map(String::as_str));
        let mut syms = vec![(0, "ViaQuietDropped"), (2, "ObserveDrop"), (3, "far")];
        syms.push((4, "BigThing"));
        let g = go_graph(&paths, &syms);
        let m = match_task(&g, "rename ViaQuietDropped");
        assert_eq!(m.package_files, BTreeSet::from([FileId(1), FileId(2)]));
        // A package over the cap adds nothing.
        let m = match_task(&g, "rename BigThing");
        assert_eq!(m.exact_files, BTreeSet::from([FileId(4)]));
        assert!(m.package_files.is_empty(), "{:?}", m.package_files);
        // Not Go: no package tier.
        let g = named_graph(&paths, &syms);
        assert!(match_task(&g, "rename ViaQuietDropped")
            .package_files
            .is_empty());
    }
}
