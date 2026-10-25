//! 层级化符号路径匹配——对齐 Serena `NamePathMatcher` 的一个子集。
//!
//! [`CodeSymbol`](crate::CodeSymbol) 是平的：没有 parent 字段。但
//! `start_line`/`end_line`（1-based 闭区间）天然构成**行范围嵌套**——
//! 同文件内 Class 的范围包含 Method 的范围。本模块就用这一启发式推断
//! 层级，从而支持 `"Class/method"` 形式的路径查询。
//!
//! # 启发式的局限
//!
//! 行范围嵌套不如 LSP documentSymbol 层级精确：
//! - 语法符号（如 Rust 的 `impl` 块）会形成中间层，因此祖先链验证
//!   允许**跳过中间层**（`"Class/method"` 不要求直接父子）；
//! - 范围相等（同一行的多个符号）没有列号可比：容器种类（Class/Struct/…）
//!   包含非容器种类，其余视为兄弟；范围交叉（而非嵌套）的符号互不包含；
//! - 跨文件不构成层级：祖先链只在同文件内计算。
//!
//! 与 Serena 的差异（有意为之）：
//! - 匹配大小写不敏感（Serena 区分大小写）；
//! - 祖先段允许跳过中间层（当前 Serena 逐段对齐，不允许跳层）；
//! - 尾斜杠 `"Foo/"` 只命中容器本身，子树一律由 `depth` 参数展开
//!   （当前 Serena 只是把尾斜杠剥掉）。
//!
//! 复杂度：按文件分桶，祖先链与直接子符号只在同文件内扫描
//!（O(Σ n_file²)）；查询串只小写一次，符号名比较走 ASCII 零分配快路。

use std::collections::HashMap;

use crate::types::{CodeSymbol, SymbolId, SymbolKind};

/// kind 过滤：None = 不过滤。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindFilter(pub Option<SymbolKind>);

impl KindFilter {
    /// `sym` 能否通过过滤。
    fn accepts(self, sym: &CodeSymbol) -> bool {
        match self.0 {
            None => true,
            Some(kind) => sym.kind == kind,
        }
    }
}

/// 单个查询命中：符号 + 相对查询的深度（0 = 命中自身；>0 = 命中符号的
/// 第 N 层后代，供 Serena depth 语义：返回匹配者的孩子）。
#[derive(Debug)]
pub struct NamePathMatch {
    pub symbol: SymbolId,
    /// Serena depth 语义：0 只返回匹配符号；1 返回其直接子符号。
    pub depth: u8,
}

/// 层级化匹配。query 语法（对齐 Serena NamePathMatcher 子集）：
/// - "name"            全库任意符号名（大小写不敏感）精确或子串匹配
/// - "Foo/method"      路径后缀匹配：同文件内 Foo 行范围包含 method 行范围
/// - "/Foo/method"     绝对路径：Foo 必须是文件顶层（无包含者）
/// - "Foo/" 结尾斜杠   只匹配容器本身（要求命中者确实是容器；其子按
///   depth 展开，不自动带出）
///
/// 层级 = 同文件内行范围包含关系（祖先严格包含后代；范围相等视为兄弟）。
/// substring=true 时最后一段按子串匹配（Foo/get 命中 getValue），祖先段
/// 一律精确匹配；全部大小写不敏感。
/// kind 过滤作用于**最终返回的每个符号**（含 depth 展开的子符号）。
/// depth 展开：对每个匹配者返回其第 1..=depth 层后代（0 = 不展开，
/// 只返回匹配者自身；同一符号命中多个匹配者时取最小深度）。
/// 结果按 (file, start_line, name) 全序，确定性。
pub fn match_name_path(
    symbols: &[CodeSymbol],
    query: &str,
    substring: bool,
    kind: KindFilter,
    depth: u8,
) -> Vec<NamePathMatch> {
    match_name_path_with(
        symbols,
        &NamePathIndex::new(symbols),
        query,
        substring,
        kind,
        depth,
    )
}

/// [`match_name_path`] with a prebuilt [`NamePathIndex`] (built once per
/// immutable symbol table instead of once per query).
pub fn match_name_path_with(
    symbols: &[CodeSymbol],
    index: &NamePathIndex,
    query: &str,
    substring: bool,
    kind: KindFilter,
    depth: u8,
) -> Vec<NamePathMatch> {
    let Some(mut query) = parse_query(query) else {
        return Vec::new();
    };
    // 名字本身含 `/`（`operator /`、TS 的 "a/b" 方法名）：先整串匹配，
    // 命中就当单段查询；否则尝试"祖先路径 + 含 `/` 的尾段"（最长尾段优先）；
    // 都没有符号名匹配时才回落到按 `/` 分段。
    if query.whole.contains('/') {
        let any_named = |text: &str| {
            let lowered = lower_once(text);
            match &index.names {
                Some(table) => table.any_matches(&lowered, substring),
                None => symbols
                    .iter()
                    .any(|s| name_matches(&s.name, &lowered, substring)),
            }
        };
        if any_named(&query.whole) {
            query = Query {
                segments: vec![query.whole.clone()],
                absolute: false,
                trailing_slash: false,
                whole: query.whole,
            };
        } else {
            let whole = query.whole.clone();
            for (pos, _) in whole.match_indices('/') {
                let tail = whole[pos + 1..].trim();
                if tail.is_empty() || !tail.contains('/') || !any_named(tail) {
                    continue;
                }
                let mut segments: Vec<String> = whole[..pos]
                    .split('/')
                    .map(str::trim)
                    .filter(|seg| !seg.is_empty())
                    .map(str::to_owned)
                    .collect();
                segments.push(tail.to_owned());
                query = Query {
                    segments,
                    absolute: query.absolute,
                    trailing_slash: false,
                    whole: whole.clone(),
                };
                break;
            }
        }
    }
    let ctx = Ctx::new(symbols, index);
    let by_id = ctx.by_id;
    // 查询段只小写一次；符号名比较走 ASCII 快路，不再逐符号分配。
    let lowered: Vec<String> = query.segments.iter().map(|s| lower_once(s)).collect();
    let last = lowered.last().expect("解析结果至少一段");

    // 1) 候选 = 最后一段的名字匹配者；2) 向上验证祖先链与绝对路径约束。
    let mut matched: Vec<SymbolId> = Vec::new();
    // Candidates come in `symbols` order either way: from the name table
    // (exact last segment: one binary search instead of a full scan) or
    // from the full scan when no table was built.
    let candidates: Vec<usize> = match &index.names {
        Some(table) => table.matching(last, substring),
        None => symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| name_matches(&s.name, last, substring))
            .map(|(i, _)| i)
            .collect(),
    };
    for sym in candidates.into_iter().map(|i| &symbols[i]) {
        if lowered.len() == 1 {
            // 绝对路径的单段查询：命中者必须是文件顶层。
            if query.absolute && !ctx.ancestors_within_file(sym).is_empty() {
                continue;
            }
        } else if !chain_matches(&ctx, sym, &lowered[..lowered.len() - 1], query.absolute) {
            continue;
        }
        // 尾斜杠：只命中容器本身（叶子不算容器）。
        if query.trailing_slash && !ctx.is_container(sym) {
            continue;
        }
        matched.push(sym.id);
    }

    // depth 展开：0 = 匹配者自身；>0 = 每个匹配者的第 1..=depth 层后代
    //（匹配者本身不进入结果）。同一符号命中多个匹配者时取最小深度。
    let mut depth_by_id: HashMap<SymbolId, u8> = HashMap::new();
    for id in &matched {
        if depth == 0 {
            depth_by_id.insert(*id, 0);
            continue;
        }
        let mut layer = vec![*id];
        for d in 1..=depth {
            let mut next = Vec::new();
            for cur in &layer {
                next.extend(ctx.direct_children(&symbols[by_id[cur]]));
            }
            for child in &next {
                match depth_by_id.entry(*child) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if d < *e.get() {
                            e.insert(d);
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(d);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            layer = next;
        }
    }

    // kind 过滤作用于最终返回的每个符号；输出按全序排序保证确定性。
    let mut out: Vec<NamePathMatch> = depth_by_id
        .into_iter()
        .filter(|(id, _)| kind.accepts(&symbols[by_id[id]]))
        .map(|(symbol, depth)| NamePathMatch { symbol, depth })
        .collect();
    out.sort_by(|a, b| {
        let sa = &symbols[by_id[&a.symbol]];
        let sb = &symbols[by_id[&b.symbol]];
        (sa.file, sa.start_line, &sa.name, sa.end_line, sa.id).cmp(&(
            sb.file,
            sb.start_line,
            &sb.name,
            sb.end_line,
            sb.id,
        ))
    });
    out
}

/// 每个目标（`symbols` 下标）在同文件内的最内层包含者（`symbols` 下标）。
/// 供 find_symbol 在每行标注"所属容器"；按文件分桶，不做全库扫描。
pub fn innermost_containers(symbols: &[CodeSymbol], targets: &[usize]) -> Vec<Option<usize>> {
    innermost_containers_with(symbols, &NamePathIndex::new(symbols), targets)
}

/// [`innermost_containers`] with a prebuilt [`NamePathIndex`].
pub fn innermost_containers_with(
    symbols: &[CodeSymbol],
    index: &NamePathIndex,
    targets: &[usize],
) -> Vec<Option<usize>> {
    let ctx = Ctx::new(symbols, index);
    targets
        .iter()
        .map(|&t| {
            ctx.ancestors_within_file(&symbols[t])
                .first()
                .map(|id| ctx.by_id[id])
        })
        .collect()
}

// ------------------------------------------------------------------ 解析

/// 解析后的查询。段保留原始大小写（匹配时统一小写比较）。
struct Query {
    segments: Vec<String>,
    /// 前导 `/`：绝对路径，链首必须是文件顶层。
    absolute: bool,
    /// 尾部 `/`：只匹配容器本身。
    trailing_slash: bool,
    /// 去空白后的整串（用于名字里带 `/` 的符号先整串匹配）。
    whole: String,
}

/// 去空白、按 `/` 分段、空段丢弃；记录绝对/尾斜杠标志（单段也合法）。
/// 没有任何有效段（空 query、只有斜杠）时返回 `None`。
fn parse_query(query: &str) -> Option<Query> {
    let trimmed = query.trim();
    let absolute = trimmed.starts_with('/');
    let trailing_slash = trimmed.ends_with('/');
    let segments: Vec<String> = trimmed
        .split('/')
        .map(str::trim)
        .filter(|seg| !seg.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.is_empty() {
        None
    } else {
        Some(Query {
            segments,
            absolute,
            trailing_slash,
            whole: trimmed.to_owned(),
        })
    }
}

// ------------------------------------------------------------名字与层级判定

/// 查询段小写化（只做一次）。
fn lower_once(s: &str) -> String {
    s.to_lowercase()
}

/// 大小写不敏感的名字匹配；`seg_lower` 必须已小写；`substring` 为真时按子串
///（用于最后一段）。ASCII 对 ASCII 走零分配快路。
fn name_matches(sym_name: &str, seg_lower: &str, substring: bool) -> bool {
    if sym_name.is_ascii() && seg_lower.is_ascii() {
        let (n, s) = (sym_name.as_bytes(), seg_lower.as_bytes());
        return if substring {
            s.is_empty() || n.windows(s.len()).any(|w| w.eq_ignore_ascii_case(s))
        } else {
            n.eq_ignore_ascii_case(s)
        };
    }
    let n = sym_name.to_lowercase();
    if substring {
        n.contains(seg_lower)
    } else {
        n == seg_lower
    }
}

/// 能容纳其它符号的种类。
fn is_container_kind(kind: SymbolKind) -> bool {
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

/// `outer` 是否**严格包含** `inner`（须同文件）。行范围真包含即成立；
/// 范围完全相等（一行类、压缩/生成代码）时没有列号可比，改用种类兜底：
/// 容器种类（Class/Struct/…）包含非容器种类，其余视为兄弟。
fn strictly_contains(outer: &CodeSymbol, inner: &CodeSymbol) -> bool {
    if outer.file != inner.file
        || outer.start_line > inner.start_line
        || outer.end_line < inner.end_line
    {
        return false;
    }
    if (outer.start_line, outer.end_line) != (inner.start_line, inner.end_line) {
        return true;
    }
    is_container_kind(outer.kind) && !is_container_kind(inner.kind)
}

/// 查询期上下文：按文件分桶，层级判定只在同文件符号里扫描
///（O(Σ n_file²) 而非 O(N²)），id→下标映射一次建好。
struct Ctx<'a> {
    symbols: &'a [CodeSymbol],
    by_id: &'a HashMap<SymbolId, usize>,
    buckets: &'a HashMap<crate::types::FileId, Vec<usize>>,
}

/// Lookup tables over one symbol table (id → position, per-file buckets,
/// and optionally a [`NameTable`]). Immutable-snapshot callers build it once
/// and reuse it across queries.
#[derive(Debug, Default)]
pub struct NamePathIndex {
    by_id: HashMap<SymbolId, usize>,
    buckets: HashMap<crate::types::FileId, Vec<usize>>,
    names: Option<NameTable>,
}

impl NamePathIndex {
    pub fn new(symbols: &[CodeSymbol]) -> Self {
        let mut buckets: HashMap<crate::types::FileId, Vec<usize>> = HashMap::new();
        for (i, s) in symbols.iter().enumerate() {
            buckets.entry(s.file).or_default().push(i);
        }
        NamePathIndex {
            by_id: index_by_id(symbols),
            buckets,
            names: None,
        }
    }

    /// [`Self::new`] plus a [`NameTable`], so the last path segment is looked
    /// up instead of scanned. Both halves are built in parallel.
    pub fn with_names(symbols: &[CodeSymbol]) -> Self {
        let (mut index, names) = rayon::join(|| Self::new(symbols), || NameTable::new(symbols));
        index.names = Some(names);
        index
    }

    /// The name table, when built with [`Self::with_names`].
    pub fn names(&self) -> Option<&NameTable> {
        self.names.as_ref()
    }

    /// Positions (ascending, in `symbols` order) of the symbols in `file`.
    pub fn file_symbols(&self, file: crate::types::FileId) -> &[usize] {
        self.buckets.get(&file).map_or(&[], Vec::as_slice)
    }
}

/// Lowercased symbol names plus the positions sorted by (lowercase name,
/// position): an inverted index from a name to its symbols. Exact lookups
/// are a binary search; each group lists positions in `symbols` order.
#[derive(Debug, Default)]
pub struct NameTable {
    lower: Vec<String>,
    sorted: Vec<u32>,
}

impl NameTable {
    pub fn new(symbols: &[CodeSymbol]) -> Self {
        use rayon::prelude::*;
        let lower: Vec<String> = symbols.par_iter().map(|s| s.name.to_lowercase()).collect();
        let mut sorted: Vec<u32> = (0..symbols.len() as u32).collect();
        sorted.par_sort_unstable_by(|&a, &b| {
            lower[a as usize].cmp(&lower[b as usize]).then(a.cmp(&b))
        });
        NameTable { lower, sorted }
    }

    /// `name.to_lowercase()` of every symbol, by position.
    pub fn lower(&self) -> &[String] {
        &self.lower
    }

    /// Positions (ascending) of the symbols whose lowercase name equals
    /// `lower_name` (which must already be lowercase).
    pub fn exact(&self, lower_name: &str) -> &[u32] {
        let key = |i: &u32| self.lower[*i as usize].as_str();
        let lo = self.sorted.partition_point(|i| key(i) < lower_name);
        let len = self.sorted[lo..].partition_point(|i| key(i) == lower_name);
        &self.sorted[lo..lo + len]
    }

    /// One group per distinct lowercase name (in name order); each group
    /// lists its positions ascending, so `group[0]` is the first occurrence.
    pub fn groups(&self) -> impl Iterator<Item = &[u32]> {
        self.sorted
            .chunk_by(|a, b| self.lower[*a as usize] == self.lower[*b as usize])
    }

    /// Positions (ascending) whose name matches `seg_lower` the way
    /// `name_matches` does: equal ignoring case, or containing it when
    /// `substring`.
    fn matching(&self, seg_lower: &str, substring: bool) -> Vec<usize> {
        if substring {
            self.lower
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(seg_lower))
                .map(|(i, _)| i)
                .collect()
        } else {
            self.exact(seg_lower).iter().map(|&i| i as usize).collect()
        }
    }

    fn any_matches(&self, seg_lower: &str, substring: bool) -> bool {
        if substring {
            self.lower.iter().any(|l| l.contains(seg_lower))
        } else {
            !self.exact(seg_lower).is_empty()
        }
    }
}

impl<'a> Ctx<'a> {
    fn new(symbols: &'a [CodeSymbol], index: &'a NamePathIndex) -> Self {
        Ctx {
            symbols,
            by_id: &index.by_id,
            buckets: &index.buckets,
        }
    }

    fn bucket(&self, file: crate::types::FileId) -> &[usize] {
        self.buckets.get(&file).map_or(&[], Vec::as_slice)
    }

    /// `target` 在同文件内的包含者，按范围**从内到外**排序
    ///（start_line 降序、end_line 升序；范围交叉的包含者也由此排定确定顺序）。
    fn ancestors_within_file(&self, target: &CodeSymbol) -> Vec<SymbolId> {
        let mut ancestors: Vec<&CodeSymbol> = self
            .bucket(target.file)
            .iter()
            .map(|&i| &self.symbols[i])
            .filter(|s| strictly_contains(s, target))
            .collect();
        ancestors.sort_by(|a, b| {
            b.start_line
                .cmp(&a.start_line)
                .then(a.end_line.cmp(&b.end_line))
                // 范围相等时容器（外层）排在后面
                .then(is_container_kind(a.kind).cmp(&is_container_kind(b.kind)))
        });
        ancestors.into_iter().map(|s| s.id).collect()
    }

    /// `parent` 的直接子符号：严格被 `parent` 包含、且二者之间没有中间符号。
    fn direct_children(&self, parent: &CodeSymbol) -> Vec<SymbolId> {
        let inside: Vec<&CodeSymbol> = self
            .bucket(parent.file)
            .iter()
            .map(|&i| &self.symbols[i])
            .filter(|s| strictly_contains(parent, s))
            .collect();
        inside
            .iter()
            .filter(|s| !inside.iter().any(|mid| strictly_contains(mid, s)))
            .map(|s| s.id)
            .collect()
    }

    /// `target` 是否容器：同文件内至少严格包含一个其它符号。
    fn is_container(&self, target: &CodeSymbol) -> bool {
        self.bucket(target.file)
            .iter()
            .any(|&i| strictly_contains(target, &self.symbols[i]))
    }
}

/// `SymbolId` → 下标。`match_name_path` 假定 id 在 `symbols` 内唯一。
fn index_by_id(symbols: &[CodeSymbol]) -> HashMap<SymbolId, usize> {
    symbols.iter().enumerate().map(|(i, s)| (s.id, i)).collect()
}

/// 多段查询的祖先链验证。`ancestor_segs` 是除最后一段外的段（已小写，
/// 书写顺序，外→内）；从最内段开始在链（内→外）中**贪心**寻找最近的匹配
/// 者，允许跳过中间层（后缀匹配语义：`Class/method` 允许 Class→impl→method）。
/// `absolute` 为真时还要求链首（最外层匹配段）之上没有更外层的包含者。
/// 祖先段一律精确匹配（子串只作用于最后一段）。
fn chain_matches(ctx: &Ctx, target: &CodeSymbol, ancestor_segs: &[String], absolute: bool) -> bool {
    let symbols = ctx.symbols;
    let by_id = &ctx.by_id;
    let chain = ctx.ancestors_within_file(target);
    let mut idx = 0;
    let mut outermost: Option<usize> = None;
    let last_seg_i = ancestor_segs.len().saturating_sub(1);
    for (seg_i, seg) in ancestor_segs.iter().rev().enumerate() {
        let mut found = false;
        while idx < chain.len() {
            let ancestor = &symbols[by_id[&chain[idx]]];
            idx += 1;
            if name_matches(&ancestor.name, seg, false) {
                // 绝对路径的最外段必须绑到链顶（文件顶层）。嵌套同名时
                // 贪心会先命中内层同名祖先，继续向外找顶层同名者，
                // 否则 `/Dup/leaf` 会被错误拒绝。
                if absolute && seg_i == last_seg_i && idx != chain.len() {
                    continue;
                }
                outermost = Some(idx - 1);
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    match outermost {
        Some(i) => !absolute || i + 1 == chain.len(),
        // ancestor_segs 非空（多段查询），走不到这里；保守返回 false。
        None => false,
    }
}

// -------------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileId;

    fn sym(id: u32, file: u32, name: &str, kind: SymbolKind, lines: (u32, u32)) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(id),
            file: FileId(file),
            name: name.to_string(),
            kind,
            signature: String::new(),
            start_line: lines.0,
            end_line: lines.1,
            exported: true,
        }
    }

    fn all() -> KindFilter {
        KindFilter(None)
    }

    fn only(kind: SymbolKind) -> KindFilter {
        KindFilter(Some(kind))
    }

    /// (symbol id, depth) 对，便于整表断言。
    fn ids(matches: &[NamePathMatch]) -> Vec<(u32, u8)> {
        matches.iter().map(|m| (m.symbol.0, m.depth)).collect()
    }

    /// 文件 1：Parser(10-200) 含 parse/getValue/Node/count；
    /// Node(130-160) 含 serialize；top_fn 与顶层 parse 在 Parser 之外。
    fn fixture() -> Vec<CodeSymbol> {
        vec![
            sym(0, 1, "Parser", SymbolKind::Class, (10, 200)),
            sym(1, 1, "parse", SymbolKind::Method, (20, 80)),
            sym(2, 1, "getValue", SymbolKind::Method, (90, 120)),
            sym(3, 1, "Node", SymbolKind::Class, (130, 160)),
            sym(4, 1, "serialize", SymbolKind::Method, (140, 150)),
            sym(5, 1, "top_fn", SymbolKind::Function, (1, 5)),
            sym(6, 1, "parse", SymbolKind::Function, (300, 340)),
            sym(7, 1, "count", SymbolKind::Field, (15, 15)),
        ]
    }

    // 1. 单段精确 + 子串两态
    #[test]
    fn single_segment_exact_and_substring() {
        let s = fixture();
        // 精确、大小写不敏感
        assert_eq!(
            ids(&match_name_path(&s, "parser", false, all(), 0)),
            vec![(0, 0)]
        );
        // 精确不开子串："get" 不命中 getValue
        assert!(match_name_path(&s, "get", false, all(), 0).is_empty());
        // 子串：get → getValue
        assert_eq!(
            ids(&match_name_path(&s, "get", true, all(), 0)),
            vec![(2, 0)]
        );
        // 子串命中多处（"parse" 也命中 "Parser"），按 (file, start_line, name) 排序
        assert_eq!(
            ids(&match_name_path(&s, "parse", true, all(), 0)),
            vec![(0, 0), (1, 0), (6, 0)]
        );
    }

    // 2. "Class/method" 直接父子命中；跨中间层（Class 含 impl 含 method）仍命中
    #[test]
    fn path_matches_direct_parent_and_across_middle_layer() {
        let s = fixture();
        // 直接父子：Parser/parse 命中 parse@1；顶层同名 parse 不借 Parser 命中
        assert_eq!(
            ids(&match_name_path(&s, "Parser/parse", false, all(), 0)),
            vec![(1, 0)]
        );
        // 文件 2：Repo 含 "impl Repo"（Module 中间层）含 load；save 是 Repo 直系
        let s2 = vec![
            sym(10, 2, "Repo", SymbolKind::Class, (10, 100)),
            sym(11, 2, "impl Repo", SymbolKind::Module, (20, 90)),
            sym(12, 2, "load", SymbolKind::Method, (30, 60)),
            sym(13, 2, "save", SymbolKind::Method, (95, 99)),
        ];
        // 跳过 impl 中间层
        assert_eq!(
            ids(&match_name_path(&s2, "Repo/load", false, all(), 0)),
            vec![(12, 0)]
        );
        // 直系
        assert_eq!(
            ids(&match_name_path(&s2, "Repo/save", false, all(), 0)),
            vec![(13, 0)]
        );
        // 显式写出中间层的三段也命中
        assert_eq!(
            ids(&match_name_path(
                &s2,
                "Repo/impl Repo/load",
                false,
                all(),
                0
            )),
            vec![(12, 0)]
        );
        // 祖先段不吃子串（子串只作用于最后一段）：见 substring_applies_only_to_last_segment
    }

    // 3. 绝对路径 "/Class/method"：嵌套在函数里的内层 Class 不命中，顶层命中
    #[test]
    fn absolute_path_requires_top_level_chain_head() {
        let s = vec![
            sym(20, 3, "outer", SymbolKind::Function, (1, 100)),
            sym(21, 3, "Config", SymbolKind::Class, (10, 50)),
            sym(22, 3, "reload", SymbolKind::Method, (20, 30)),
            sym(23, 3, "Config", SymbolKind::Class, (200, 260)),
            sym(24, 3, "reload", SymbolKind::Method, (210, 240)),
        ];
        // 相对：两个 reload 都命中
        assert_eq!(
            ids(&match_name_path(&s, "Config/reload", false, all(), 0)),
            vec![(22, 0), (24, 0)]
        );
        // 绝对：嵌在 outer 里的 Config 之上的链没走完，被拒
        assert_eq!(
            ids(&match_name_path(&s, "/Config/reload", false, all(), 0)),
            vec![(24, 0)]
        );
        // 绝对单段：/Config 只命中顶层 Config@23
        assert_eq!(
            ids(&match_name_path(&s, "/Config", false, all(), 0)),
            vec![(23, 0)]
        );
    }

    // 4. kind 过滤：Method only 时 Class 被滤掉；作用于 depth 展开的子符号
    #[test]
    fn kind_filter_applies_to_matches_and_expanded_children() {
        let s = fixture();
        // depth=0：命中 Parser，但 kind=Method 把 Class 滤光
        assert!(match_name_path(&s, "Parser", false, only(SymbolKind::Method), 0).is_empty());
        // depth=1：Parser 的直接子符号里只留 Method（parse、getValue；Node/count 被滤）
        assert_eq!(
            ids(&match_name_path(
                &s,
                "Parser",
                false,
                only(SymbolKind::Method),
                1
            )),
            vec![(1, 1), (2, 1)]
        );
        // depth=2：孙辈 serialize 也是 Method，一并返回
        assert_eq!(
            ids(&match_name_path(
                &s,
                "Parser",
                false,
                only(SymbolKind::Method),
                2
            )),
            vec![(1, 1), (2, 1), (4, 2)]
        );
        // kind 作用于展开出的子符号本身：Class only 时只剩 Node
        assert_eq!(
            ids(&match_name_path(
                &s,
                "Parser",
                false,
                only(SymbolKind::Class),
                2
            )),
            vec![(3, 1)]
        );
    }

    // 5. depth=1 展开：命中 Class 返回其方法（不含孙辈）；depth=0 只自身
    #[test]
    fn depth_expansion_layers() {
        let s = fixture();
        // depth=0：只返回匹配者自身
        assert_eq!(
            ids(&match_name_path(&s, "Parser", false, all(), 0)),
            vec![(0, 0)]
        );
        // depth=1：直接子符号（count、parse、getValue、Node），不含孙辈 serialize
        assert_eq!(
            ids(&match_name_path(&s, "Parser", false, all(), 1)),
            vec![(7, 1), (1, 1), (2, 1), (3, 1)]
        );
        // depth=2：孙辈 serialize 以 depth=2 进入
        assert_eq!(
            ids(&match_name_path(&s, "Parser", false, all(), 2)),
            vec![(7, 1), (1, 1), (2, 1), (3, 1), (4, 2)]
        );
    }

    // 6. 不同文件同名 Class 互不串层
    #[test]
    fn same_name_classes_in_different_files_do_not_mix() {
        let s = vec![
            // 文件 1：Parser 含 parse
            sym(0, 1, "Parser", SymbolKind::Class, (10, 200)),
            sym(1, 1, "parse", SymbolKind::Method, (20, 80)),
            // 文件 4：另一个 Parser 含 parse@33；parse@32 嵌在 Lexer 里
            sym(30, 4, "Parser", SymbolKind::Class, (1, 50)),
            sym(31, 4, "Lexer", SymbolKind::Class, (60, 120)),
            sym(32, 4, "parse", SymbolKind::Method, (70, 90)),
            sym(33, 4, "parse", SymbolKind::Method, (5, 20)),
        ];
        // 各自文件内命中；Lexer 里的 parse 不借任何文件的 Parser 命中
        assert_eq!(
            ids(&match_name_path(&s, "Parser/parse", false, all(), 0)),
            vec![(1, 0), (33, 0)]
        );
        // 展开也不跨文件：两个 Parser 的直接子各只有一个 parse
        assert_eq!(
            ids(&match_name_path(&s, "Parser", false, all(), 1)),
            vec![(1, 1), (33, 1)]
        );
    }

    // 7. 确定性全序断言
    #[test]
    fn results_are_deterministic_and_totally_ordered() {
        let s = vec![
            sym(40, 1, "Zeta", SymbolKind::Class, (10, 50)),
            sym(41, 1, "alpha", SymbolKind::Function, (10, 20)), // 同起始行 → 按名字
            sym(42, 2, "gamma", SymbolKind::Struct, (5, 8)),     // 文件序优先
        ];
        let a = match_name_path(&s, "a", true, all(), 0); // 三个名字都含 "a"
        let b = match_name_path(&s, "a", true, all(), 0);
        assert_eq!(ids(&a), ids(&b));
        // (file, start_line, name)：文件 1 全部先于文件 2；文件 1 同行按名字 Zeta < alpha
        assert_eq!(ids(&a), vec![(40, 0), (41, 0), (42, 0)]);
    }

    // 8. 空 query / 只有斜杠 → 空结果
    #[test]
    fn empty_or_slash_only_query_matches_nothing() {
        let s = fixture();
        for q in ["", " ", "/", "//", " / ", "/ /"] {
            assert!(
                match_name_path(&s, q, false, all(), 0).is_empty(),
                "query={q:?}"
            );
            assert!(
                match_name_path(&s, q, true, all(), 2).is_empty(),
                "query={q:?}"
            );
        }
    }

    // 额外：尾斜杠只匹配容器本身，子树由 depth 控制
    #[test]
    fn trailing_slash_matches_container_itself_only() {
        let s = vec![
            sym(50, 1, "Foo", SymbolKind::Class, (1, 100)),
            sym(51, 1, "bar", SymbolKind::Method, (10, 20)),
            sym(52, 1, "Foo", SymbolKind::Function, (200, 210)), // 叶子，同名
        ];
        // 无尾斜杠：两个 Foo 都命中
        assert_eq!(
            ids(&match_name_path(&s, "Foo", false, all(), 0)),
            vec![(50, 0), (52, 0)]
        );
        // 尾斜杠：只有容器 Foo 命中；叶子不算容器
        assert_eq!(
            ids(&match_name_path(&s, "Foo/", false, all(), 0)),
            vec![(50, 0)]
        );
        // 子符号不自动带出，由 depth 展开
        assert_eq!(
            ids(&match_name_path(&s, "Foo/", false, all(), 1)),
            vec![(51, 1)]
        );
    }

    // 额外：子串只作用于最后一段，祖先段一律精确
    #[test]
    fn substring_applies_only_to_last_segment() {
        let s = vec![
            sym(60, 1, "MyRepo", SymbolKind::Class, (1, 100)),
            sym(61, 1, "load_all", SymbolKind::Method, (10, 20)),
        ];
        // 最后一段子串：load → load_all ✓
        assert_eq!(
            ids(&match_name_path(&s, "MyRepo/load", true, all(), 0)),
            vec![(61, 0)]
        );
        // 祖先段不吃子串：Repos/load_all 不命中（尽管 MyRepo 含 "repo"）
        assert!(match_name_path(&s, "Repos/load_all", true, all(), 0).is_empty());
    }

    // 额外：嵌套同名匹配者同时命中时，后代取最小深度
    #[test]
    fn nested_same_name_matchers_keep_min_depth() {
        let s = vec![
            sym(70, 1, "Dup", SymbolKind::Class, (1, 100)),
            sym(71, 1, "Dup", SymbolKind::Class, (10, 50)), // 嵌套同名
            sym(72, 1, "leaf", SymbolKind::Method, (20, 30)),
        ];
        // depth=1：内层 Dup 是外层的直接子（d=1），leaf 是内层的直接子（d=1）
        assert_eq!(
            ids(&match_name_path(&s, "Dup", false, all(), 1)),
            vec![(71, 1), (72, 1)]
        );
        // depth=2：leaf 同时是外层的 d=2 与内层的 d=1 → 取 1
        assert_eq!(
            ids(&match_name_path(&s, "Dup", false, all(), 2)),
            vec![(71, 1), (72, 1)]
        );
    }

    // 绝对路径 + 嵌套同名：贪心先绑内层 Dup 时仍应接受顶层 Dup 下的 leaf
    #[test]
    fn absolute_path_with_nested_same_name_binds_top_level() {
        let s = vec![
            sym(70, 1, "Dup", SymbolKind::Class, (1, 100)),
            sym(71, 1, "Dup", SymbolKind::Class, (10, 50)), // 嵌套同名
            sym(72, 1, "leaf", SymbolKind::Method, (20, 30)),
        ];
        // 相对：内层/外层 Dup 都能作为路径后缀的祖先
        assert_eq!(
            ids(&match_name_path(&s, "Dup/leaf", false, all(), 0)),
            vec![(72, 0)]
        );
        // 绝对：最外段必须是文件顶层 Dup@70；不能因贪心绑到内层而拒掉
        assert_eq!(
            ids(&match_name_path(&s, "/Dup/leaf", false, all(), 0)),
            vec![(72, 0)]
        );
    }

    // 同一行的类与方法：行范围相等，靠种类兜底（审计 parse-front #6）
    #[test]
    fn same_line_container_contains_member() {
        let s = vec![
            sym(80, 1, "A", SymbolKind::Class, (1, 1)),
            sym(81, 1, "m", SymbolKind::Method, (1, 1)),
            sym(82, 1, "B", SymbolKind::Class, (1, 1)), // 容器之间仍是兄弟
        ];
        assert_eq!(
            ids(&match_name_path(&s, "A/m", false, all(), 0)),
            vec![(81, 0)]
        );
        assert!(match_name_path(&s, "A/B", false, all(), 0).is_empty());
        assert_eq!(
            ids(&match_name_path(&s, "A/", false, all(), 1)),
            vec![(81, 1)]
        );
    }

    // 名字含 `/`：先整串精确匹配，再回落分段（审计 parse-front #7）
    #[test]
    fn slash_in_symbol_name_matches_whole_string_first() {
        let s = vec![
            sym(90, 1, "Vec2", SymbolKind::Struct, (1, 20)),
            sym(91, 1, "operator /", SymbolKind::Method, (5, 8)),
        ];
        assert_eq!(
            ids(&match_name_path(&s, "operator /", false, all(), 0)),
            vec![(91, 0)]
        );
        assert_eq!(
            ids(&match_name_path(&s, "Vec2/operator /", false, all(), 0)),
            vec![(91, 0)]
        );
        // 没有符号名匹配整串时仍按分段
        assert_eq!(
            ids(&match_name_path(&s, "Vec2/", false, all(), 0)),
            vec![(90, 0)]
        );
    }

    // 大小写不敏感 + 非 ASCII 名字仍可匹配
    #[test]
    fn case_insensitive_ascii_and_unicode() {
        let s = vec![
            sym(95, 1, "HandleÉvent", SymbolKind::Function, (1, 2)),
            sym(96, 1, "FooBar", SymbolKind::Function, (3, 4)),
        ];
        assert_eq!(
            ids(&match_name_path(&s, "handleévent", false, all(), 0)),
            vec![(95, 0)]
        );
        assert_eq!(
            ids(&match_name_path(&s, "OOB", true, all(), 0)),
            vec![(96, 0)]
        );
    }

    /// The name table (inverted index) must answer every query exactly like
    /// the full scan: exact/substring last segment, ancestors, absolute and
    /// trailing-slash forms, names containing `/`, mixed case, non-ASCII.
    #[test]
    fn name_table_lookup_matches_full_scan_for_every_query_form() {
        let mut symbols = fixture();
        let extra = [
            (8, 2, "PARSER", SymbolKind::Class, (1, 50)),
            (9, 2, "Parse", SymbolKind::Method, (2, 10)),
            (10, 2, "operator /", SymbolKind::Method, (11, 12)),
            (11, 2, "a/b", SymbolKind::Method, (13, 14)),
            (12, 3, "Ärger", SymbolKind::Class, (1, 30)),
            (13, 3, "ärgern", SymbolKind::Method, (2, 9)),
            (14, 3, "İd", SymbolKind::Function, (10, 11)),
            (15, 3, "straße", SymbolKind::Function, (12, 13)),
            (16, 1, "parse", SymbolKind::Method, (21, 22)),
        ];
        for (id, file, name, kind, lines) in extra {
            symbols.push(sym(id, file, name, kind, lines));
        }
        let scan = NamePathIndex::new(&symbols);
        let table = NamePathIndex::with_names(&symbols);
        assert!(scan.names().is_none() && table.names().is_some());
        let queries = [
            "parse",
            "PARSE",
            "Parser/parse",
            "/Parser/parse",
            "/parse",
            "Parser/",
            "Node/serialize",
            "Parser/Node/serialize",
            "pars",
            "Parser/get",
            "operator /",
            "PARSER/a/b",
            "a/b",
            "ärger",
            "ÄRGER/ärgern",
            "Ärger/ÄRG",
            "i̇d",
            "İd",
            "STRASSE",
            "straße",
            "x/y/z",
            "",
            "/",
            "count",
            "Parser/count",
        ];
        for q in queries {
            for substring in [false, true] {
                for depth in [0, 1, 2] {
                    for kind in [all(), only(SymbolKind::Method)] {
                        let a = match_name_path_with(&symbols, &scan, q, substring, kind, depth);
                        let b = match_name_path_with(&symbols, &table, q, substring, kind, depth);
                        assert_eq!(
                            ids(&a),
                            ids(&b),
                            "{q:?} substring={substring} depth={depth}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn name_table_exact_and_groups_follow_symbol_order() {
        let symbols = vec![
            sym(0, 1, "Foo", SymbolKind::Class, (1, 9)),
            sym(1, 1, "bar", SymbolKind::Function, (2, 3)),
            sym(2, 2, "foo", SymbolKind::Function, (1, 2)),
            sym(3, 2, "FOO", SymbolKind::Function, (3, 4)),
            sym(4, 2, "Bar", SymbolKind::Function, (5, 6)),
        ];
        let table = NameTable::new(&symbols);
        assert_eq!(table.exact("foo"), &[0, 2, 3]);
        assert_eq!(table.exact("bar"), &[1, 4]);
        assert!(table.exact("Foo").is_empty(), "keys are lowercase");
        assert!(table.exact("fo").is_empty());
        let groups: Vec<Vec<u32>> = table.groups().map(<[u32]>::to_vec).collect();
        assert_eq!(groups, vec![vec![1, 4], vec![0, 2, 3]]);
        assert_eq!(table.lower()[3], "foo");
        assert!(NameTable::new(&[]).groups().next().is_none());
    }
}
