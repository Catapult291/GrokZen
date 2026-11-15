//! Sub-word index over symbol names, for `find_symbol`'s zero-hit fallback.
//!
//! A query like `update cloud client` (words out of order, or spaced) finds no
//! name by exact/prefix/substring matching, and the agent falls back to grep.
//! This index splits every symbol name, its innermost container's name and
//! its file's stem/directory into sub-words (the camel/snake/acronym
//! tokenizer `resolve_context` uses) and ranks symbols by a BM25-like score.
//! It is built once per index snapshot, lazily, on the first fallback query.

use std::collections::HashMap;

use astrolabe_core::graph::{split_words, CodeGraph};
use astrolabe_core::{CodeSymbol, FileId, SymbolKind};

/// Words too generic to rank on (they appear in natural-language queries).
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "by", "for", "from", "in", "into", "is", "of", "on", "or", "the", "to",
    "with",
];

const FIELD_NAME: u8 = 1;
const FIELD_CONTAINER: u8 = 2;

/// Light plural folding so `clients` meets `client` (`entries` → `entry`).
fn stem(word: &str) -> String {
    if word.len() > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") && word.is_ascii() {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

/// Sub-words of `text`, stemmed, deduplicated, stop words removed.
pub(crate) fn words_of(text: &str) -> Vec<String> {
    let mut out: Vec<String> = split_words(text)
        .into_iter()
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Kinds that can hold other symbols (mirrors `name_path`).
fn container_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Module
    )
}

/// `outer` strictly contains `inner`, as `name_path` defines containment:
/// a proper line range, or an equal range with a container around a leaf.
pub(crate) fn strictly_contains(outer: &CodeSymbol, inner: &CodeSymbol) -> bool {
    if outer.file != inner.file
        || outer.start_line > inner.start_line
        || outer.end_line < inner.end_line
    {
        return false;
    }
    if (outer.start_line, outer.end_line) != (inner.start_line, inner.end_line) {
        return true;
    }
    container_kind(outer.kind) && !container_kind(inner.kind)
}

/// Parent (innermost strict container) of each symbol in `members` (positions
/// into `symbols`, all from one file), plus the members in outline order
/// (start line, outer before inner). One sort and a stack: O(n log n).
pub(crate) fn nest(
    symbols: &[CodeSymbol],
    members: &[usize],
) -> (Vec<usize>, HashMap<usize, usize>) {
    let mut order: Vec<usize> = members.to_vec();
    order.sort_by(|&a, &b| {
        let (sa, sb) = (&symbols[a], &symbols[b]);
        sa.start_line
            .cmp(&sb.start_line)
            .then(sb.end_line.cmp(&sa.end_line))
            .then(container_kind(sb.kind).cmp(&container_kind(sa.kind)))
            .then(a.cmp(&b))
    });
    let mut parent = HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for &idx in &order {
        while let Some(&top) = stack.last() {
            if strictly_contains(&symbols[top], &symbols[idx]) {
                break;
            }
            stack.pop();
        }
        if let Some(&top) = stack.last() {
            parent.insert(idx, top);
        }
        stack.push(idx);
    }
    (order, parent)
}

/// Inverted index from sub-word to the symbols whose name (or innermost
/// container's name) contains it, plus each file's path words.
///
/// Posting lists are one flat CSR array (`offsets` + `data`): term `w`'s list
/// is `&data[offsets[w]..offsets[w + 1]]`, so a vocabulary of hundreds of
/// thousands of terms costs two allocations instead of one `Vec` per term.
#[derive(Debug, Default)]
pub(crate) struct WordIndex {
    vocab: HashMap<String, u32>,
    /// Row offsets into `data`, length `vocab.len() + 1`.
    offsets: Vec<u32>,
    /// All posting lists concatenated: `(symbol position, field mask)`.
    data: Vec<(u32, u8)>,
    /// Number of sub-words in each symbol's name.
    name_len: Vec<u8>,
    /// Innermost container of each symbol (`u32::MAX` = none).
    container: Vec<u32>,
    avg_name_len: f32,
    /// File → term ids of its stem and parent directory.
    path_terms: HashMap<FileId, Vec<u32>>,
}

/// Term id of `word`, interning it (and its still-empty posting list) on
/// first sight. `counts[t]` later accumulates term `t`'s posting frequency.
fn intern(vocab: &mut HashMap<String, u32>, counts: &mut Vec<u32>, word: &str) -> u32 {
    if let Some(&id) = vocab.get(word) {
        return id;
    }
    let id = counts.len() as u32;
    vocab.insert(word.to_string(), id);
    counts.push(0);
    id
}

/// Fill `out` with the distinct `(term, field mask)` postings of symbol
/// `pos`: the terms of its own name, then those of its innermost container's
/// name merged in with the container bit set.
fn collect_fields(
    pos: usize,
    name_terms: &[Vec<u32>],
    sym_name: &[u32],
    container: &[u32],
    out: &mut Vec<(u32, u8)>,
) {
    out.clear();
    let own = &name_terms[sym_name[pos] as usize];
    out.extend(own.iter().map(|&term| (term, FIELD_NAME)));
    let parent = container[pos];
    if parent != u32::MAX {
        for &term in &name_terms[sym_name[parent as usize] as usize] {
            match out.iter_mut().find(|(field, _)| *field == term) {
                Some(entry) => entry.1 |= FIELD_CONTAINER,
                None => out.push((term, FIELD_CONTAINER)),
            }
        }
    }
}

/// One ranked fallback candidate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WordHit {
    pub pos: usize,
    pub score: f32,
}

impl WordIndex {
    pub(crate) fn build(graph: &CodeGraph) -> Self {
        use rayon::prelude::*;
        let symbols = &graph.symbols;
        let mut buckets: HashMap<FileId, Vec<usize>> = HashMap::new();
        for (i, s) in symbols.iter().enumerate() {
            buckets.entry(s.file).or_default().push(i);
        }
        let parents: Vec<(usize, usize)> = buckets
            .par_iter()
            .flat_map_iter(|(_, members)| nest(symbols, members).1.into_iter())
            .collect();
        let mut container = vec![u32::MAX; symbols.len()];
        for (child, parent) in parents {
            container[child] = parent as u32;
        }
        // Distinct names are split once (in parallel) and their words
        // interned once; each symbol then only copies term ids.
        let mut name_ids: HashMap<&str, u32> = HashMap::new();
        let mut distinct: Vec<&str> = Vec::new();
        let sym_name: Vec<u32> = symbols
            .iter()
            .map(|s| {
                *name_ids.entry(s.name.as_str()).or_insert_with(|| {
                    distinct.push(s.name.as_str());
                    (distinct.len() - 1) as u32
                })
            })
            .collect();
        let split: Vec<Vec<String>> = distinct.par_iter().map(|n| words_of(n)).collect();

        // Term ids are assigned on first sight; `counts[t]` accumulates how
        // many postings term `t` will hold. That lets the posting lists below
        // become one flat CSR array with a single allocation.
        let mut vocab: HashMap<String, u32> = HashMap::new();
        let mut counts: Vec<u32> = Vec::new();
        let name_terms: Vec<Vec<u32>> = split
            .iter()
            .map(|words| {
                words
                    .iter()
                    .map(|w| intern(&mut vocab, &mut counts, w))
                    .collect()
            })
            .collect();
        let mut path_terms: Vec<(FileId, Vec<u32>)> = graph
            .files
            .iter()
            .map(|file| {
                let mut parts = file.path.as_str().rsplit('/');
                let stem = parts
                    .next()
                    .map(|name| name.split('.').next().unwrap_or(name))
                    .unwrap_or_default();
                let dir = parts.next().unwrap_or_default();
                let mut ids: Vec<u32> = words_of(&format!("{stem} {dir}"))
                    .iter()
                    .map(|w| intern(&mut vocab, &mut counts, w))
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                ids.shrink_to_fit();
                (file.id, ids)
            })
            .collect();
        debug_assert_eq!(counts.len(), vocab.len());

        // Pass 1: name lengths and per-term posting counts, reusing one
        // field buffer (so no per-symbol allocation either).
        let mut fields: Vec<(u32, u8)> = Vec::new();
        let mut name_len: Vec<u8> = Vec::with_capacity(symbols.len());
        let mut total_len = 0usize;
        for pos in 0..symbols.len() {
            let own_len = name_terms[sym_name[pos] as usize].len();
            name_len.push(own_len.min(255) as u8);
            total_len += own_len;
            collect_fields(pos, &name_terms, &sym_name, &container, &mut fields);
            for &(term, _) in &fields {
                counts[term as usize] += 1;
            }
        }

        // Prefix sums give each term's row; pass 2 fills the flat data array
        // by advancing one cursor per term.
        let mut offsets: Vec<u32> = Vec::with_capacity(counts.len() + 1);
        offsets.push(0);
        for &count in &counts {
            let next = offsets.last().copied().unwrap_or(0) + count;
            offsets.push(next);
        }
        let total_postings = offsets.last().copied().unwrap_or(0) as usize;
        let mut cursor = offsets[..counts.len()].to_vec();
        let mut data: Vec<(u32, u8)> = vec![(0, 0); total_postings];
        for pos in 0..symbols.len() {
            collect_fields(pos, &name_terms, &sym_name, &container, &mut fields);
            for &(term, mask) in &fields {
                let slot = &mut cursor[term as usize];
                data[*slot as usize] = (pos as u32, mask);
                *slot += 1;
            }
        }
        offsets.shrink_to_fit();
        data.shrink_to_fit();
        vocab.shrink_to_fit();
        path_terms.shrink_to_fit();

        let mut path_terms: HashMap<FileId, Vec<u32>> = path_terms.into_iter().collect();
        path_terms.shrink_to_fit();
        WordIndex {
            vocab,
            offsets,
            data,
            name_len,
            container,
            avg_name_len: if symbols.is_empty() {
                1.0
            } else {
                (total_len as f32 / symbols.len() as f32).max(1.0)
            },
            path_terms,
        }
    }

    /// The invocation list of term `id`: `(symbol position, field mask)`
    /// pairs, a slice of the flat CSR data (no allocation).
    #[inline]
    fn postings(&self, id: u32) -> &[(u32, u8)] {
        let start = self.offsets[id as usize] as usize;
        let end = self.offsets[id as usize + 1] as usize;
        &self.data[start..end]
    }

    /// Symbols ranked by sub-word relevance to `query_words` (already passed
    /// through [`words_of`]). A candidate must match at least one word in its
    /// own or its container's name, and at least `min(2, |query|)` words in
    /// total (path words count). `accept` filters by kind / path; the result
    /// is the best `limit`, highest score first (ties by position).
    pub(crate) fn search(
        &self,
        symbols: &[CodeSymbol],
        query_words: &[String],
        is_test: &dyn Fn(FileId) -> bool,
        accept: &dyn Fn(&CodeSymbol) -> bool,
        limit: usize,
    ) -> Vec<WordHit> {
        if query_words.is_empty() || symbols.is_empty() {
            return Vec::new();
        }
        let n = symbols.len() as f32;
        let idf = |df: usize| (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln();
        let term_ids: Vec<Option<u32>> = query_words
            .iter()
            .map(|w| self.vocab.get(w).copied())
            .collect();
        // position → (sum of name/container contributions, matched bitmask).
        let mut acc: HashMap<u32, (f32, u64)> = HashMap::new();
        for (qi, id) in term_ids.iter().enumerate().take(64) {
            let Some(id) = id else { continue };
            let list = self.postings(*id);
            let w_idf = idf(list.len());
            for &(pos, mask) in list {
                // BM25 length normalisation (tf = 1, k1 = 1.2, b = 0.75) of
                // the name the word was found in.
                let norm = |len: u8| {
                    2.2 / (1.0 + 1.2 * (0.25 + 0.75 * f32::from(len) / self.avg_name_len))
                };
                let field = if mask & FIELD_NAME != 0 {
                    norm(self.name_len[pos as usize])
                } else {
                    let parent = self.container[pos as usize] as usize;
                    0.5 * norm(self.name_len.get(parent).copied().unwrap_or(1))
                };
                let entry = acc.entry(pos).or_insert((0.0, 0));
                entry.0 += w_idf * field;
                entry.1 |= 1 << qi;
            }
        }
        let q = query_words.len().min(64);
        let need = q.min(2);
        let mut hits: Vec<WordHit> = Vec::new();
        for (pos, (mut score, mut matched)) in acc {
            let symbol = &symbols[pos as usize];
            if let Some(path_ids) = self.path_terms.get(&symbol.file) {
                for (qi, id) in term_ids.iter().enumerate().take(64) {
                    if matched & (1 << qi) != 0 {
                        continue;
                    }
                    if let Some(id) = id {
                        if path_ids.binary_search(id).is_ok() {
                            score += idf(self.postings(*id).len()) * 0.35;
                            matched |= 1 << qi;
                        }
                    }
                }
            }
            let count = matched.count_ones() as usize;
            if count < need || !accept(symbol) {
                continue;
            }
            let coverage = count as f32 / q as f32;
            let kind_prior = match symbol.kind {
                SymbolKind::Function | SymbolKind::Method => 1.0,
                SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Interface
                | SymbolKind::Trait
                | SymbolKind::Enum
                | SymbolKind::Type => 0.95,
                SymbolKind::Const | SymbolKind::Module => 0.85,
                SymbolKind::Field | SymbolKind::Variable => 0.7,
            };
            let test = if is_test(symbol.file) { 0.8 } else { 1.0 };
            score *= (0.5 + 0.5 * coverage) * coverage * kind_prior * test;
            hits.push(WordHit {
                pos: pos as usize,
                score,
            });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.pos.cmp(&b.pos)));
        hits.truncate(limit);
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::{CodeFile, Language, RelPath, SymbolId};

    fn sym(id: u32, file: u32, name: &str, kind: SymbolKind, lines: (u32, u32)) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(id),
            file: FileId(file),
            name: name.into(),
            kind,
            signature: String::new(),
            start_line: lines.0,
            end_line: lines.1,
            exported: true,
        }
    }

    fn file(id: u32, path: &str) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::Go),
            loc: 100,
            sha: String::new(),
        }
    }

    fn graph() -> CodeGraph {
        CodeGraph {
            files: vec![file(0, "cloud/client.go"), file(1, "local/store.go")],
            symbols: vec![
                sym(0, 0, "CloudClient", SymbolKind::Struct, (1, 40)),
                sym(1, 0, "Update", SymbolKind::Method, (10, 20)),
                sym(2, 0, "Delete", SymbolKind::Method, (21, 30)),
                sym(3, 1, "updateStore", SymbolKind::Function, (1, 9)),
                sym(4, 1, "clientCount", SymbolKind::Variable, (10, 10)),
                sym(5, 1, "update", SymbolKind::Function, (11, 12)),
            ],
            edges: vec![],
        }
    }

    fn top(index: &WordIndex, g: &CodeGraph, q: &str) -> Vec<String> {
        index
            .search(&g.symbols, &words_of(q), &|_| false, &|_| true, 10)
            .into_iter()
            .map(|h| g.symbols[h.pos].name.clone())
            .collect()
    }

    #[test]
    fn words_split_stem_and_drop_stop_words() {
        assert_eq!(
            words_of("update the cloudClients"),
            ["client", "cloud", "update"]
        );
        assert_eq!(words_of("list_entries"), ["entry", "list"]);
        assert_eq!(words_of("class"), ["class"]);
    }

    #[test]
    fn container_and_path_words_rank_the_method_first() {
        let g = graph();
        let index = WordIndex::build(&g);
        let names = top(&index, &g, "update cloud client");
        assert_eq!(
            names.first().map(String::as_str),
            Some("Update"),
            "{names:?}"
        );
        // `CloudClient` itself (2 of 3 words) also qualifies; the bare
        // single-word `update` elsewhere does not.
        assert!(names.contains(&"CloudClient".to_string()), "{names:?}");
        assert!(!names.contains(&"update".to_string()), "{names:?}");
    }

    #[test]
    fn reversed_word_order_and_camel_query_match() {
        let g = graph();
        let index = WordIndex::build(&g);
        assert_eq!(
            top(&index, &g, "store update").first().map(String::as_str),
            Some("updateStore")
        );
        assert_eq!(
            top(&index, &g, "clientCloudUpdate")
                .first()
                .map(String::as_str),
            Some("Update")
        );
    }

    #[test]
    fn accept_filter_and_unknown_words_yield_nothing() {
        let g = graph();
        let index = WordIndex::build(&g);
        assert!(top(&index, &g, "frobnicate widget").is_empty());
        let only_fns = index.search(
            &g.symbols,
            &words_of("update cloud client"),
            &|_| false,
            &|s| s.kind == SymbolKind::Struct,
            10,
        );
        assert_eq!(only_fns.len(), 1);
        assert_eq!(g.symbols[only_fns[0].pos].name, "CloudClient");
    }

    #[test]
    fn postings_are_csr_flat_and_consistent() {
        let g = graph();
        let index = WordIndex::build(&g);
        assert_eq!(index.offsets.len(), index.vocab.len() + 1);
        assert_eq!(
            *index.offsets.last().unwrap() as usize,
            index.data.len(),
            "offsets must partition data exactly"
        );
        assert!(index.offsets.windows(2).all(|w| w[0] <= w[1]));
        let cloud = index.vocab["cloud"];
        assert!(index
            .postings(cloud)
            .iter()
            .any(|&(pos, _)| g.symbols[pos as usize].name == "CloudClient"));
        // Every term's list is exactly its CSR row of the shared flat array.
        for &term in index.vocab.values() {
            let start = index.offsets[term as usize] as usize;
            let end = index.offsets[term as usize + 1] as usize;
            assert_eq!(index.postings(term).len(), end - start);
        }
    }

    #[test]
    fn nest_orders_outer_first_and_finds_innermost_parent() {
        let g = graph();
        let (order, parent) = nest(&g.symbols, &[2, 1, 0]);
        assert_eq!(order, [0, 1, 2]);
        assert_eq!(parent.get(&1), Some(&0));
        assert_eq!(parent.get(&2), Some(&0));
        assert_eq!(parent.get(&0), None);
    }
}
