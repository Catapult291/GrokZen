//! Bounded in-memory caching. Memory is a product guarantee here.
//!
//! Built on moka, which gives TTL, TTI (time-to-idle — the sliding window we
//! actually want), per-entry expiry, a `weigher` for size-aware eviction, and
//! an eviction listener for spilling to `store`.
//!
//! Why this matters: the two systems we measured both failed here. One kept
//! unbounded symbol caches with no eviction of any kind; the other capped by
//! entry count only, so a cap of 10000 line-arrays said nothing about bytes.
//!
//! Rules:
//!   * Every cache declares a byte budget via `weigher`, never just a count.
//!   * Use `time_to_idle`, not only `time_to_live` — a repo untouched for N
//!     minutes should release, while an actively used one should not expire
//!     mid-session.
//!   * The sum of all budgets is the process's steady-state ceiling, and that
//!     number goes in the README as a promise.

use std::{mem::size_of, ops::Index, sync::Arc, time::Duration};

use moka::sync::Cache;

/// Default ceiling for all in-memory caches combined.
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Release a repo's hot state after this long with no access.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

pub struct CacheConfig {
    pub budget_bytes: u64,
    pub idle_ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            idle_ttl: DEFAULT_IDLE_TTL,
        }
    }
}

/// Compact, allocation-cheap line buffer for a cached source file.
///
/// The whole file lives in one [`String`] and each line is addressed by a
/// single `u32` start offset, so caching a million-line file costs two heap
/// allocations instead of one `String` allocation per line. Line text is
/// sliced on demand. Text input follows [`str::lines`] semantics: a trailing
/// `\n` is removed, and a `\r` is removed only when it preceded that `\n`
/// (a bare `\r` is content); a final line ending produces no extra empty line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachedLines {
    text: String,
    /// Start byte offset of each line, plus a sentinel at `text.len()`.
    /// Length is `line_count + 1`; an empty file has zero lines and so an
    /// empty vector, matching `str::lines`.
    line_starts: Vec<u32>,
    /// Explicit line arrays retain their boundaries and all line content.
    owned_lines: bool,
}

impl CachedLines {
    /// Build from the full file text (usually `astrolabe_core::text::read_source`).
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let mut line_starts = Vec::new();
        if !text.is_empty() {
            line_starts.push(0);
            let len = text.len();
            for (index, byte) in text.bytes().enumerate() {
                if byte == b'\n' && index + 1 < len {
                    line_starts.push(offset_u32(index + 1));
                }
            }
            line_starts.push(offset_u32(len));
        }
        Self {
            text,
            line_starts,
            owned_lines: false,
        }
    }

    /// Build from owned lines, joining with `\n` for [`Self::as_text`].
    ///
    /// Unlike text input, explicit line arrays preserve trailing empty lines
    /// and bare carriage returns. An empty vector yields zero lines.
    pub fn from_lines(lines: Vec<String>) -> Self {
        if lines.is_empty() {
            return Self::default();
        }
        let mut text =
            String::with_capacity(lines.iter().map(String::len).sum::<usize>() + lines.len() - 1);
        let mut line_starts = Vec::with_capacity(lines.len() + 1);
        for (index, line) in lines.into_iter().enumerate() {
            if index != 0 {
                text.push('\n');
            }
            line_starts.push(offset_u32(text.len()));
            text.push_str(&line);
        }
        line_starts.push(offset_u32(text.len()));
        Self {
            text,
            line_starts,
            owned_lines: true,
        }
    }

    pub fn len(&self) -> usize {
        self.line_starts.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The line at `index` without its line ending, or `None` past the end.
    pub fn get(&self, index: usize) -> Option<&str> {
        let start = *self.line_starts.get(index)? as usize;
        let mut end = *self.line_starts.get(index + 1)? as usize;
        if self.owned_lines {
            // Only the separator we inserted belongs to neither line. Do not
            // interpret a line's own trailing CR (or other content) as an EOL.
            if index + 1 < self.len() {
                end -= 1;
            }
            return Some(&self.text[start..end]);
        }
        let raw = &self.text[start..end];
        Some(match raw.strip_suffix('\n') {
            Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
            None => raw,
        })
    }

    /// Lines in order, each borrowed from the shared text buffer.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + DoubleEndedIterator + '_ {
        (0..self.len()).map(move |index| self.get(index).unwrap_or_default())
    }

    /// Borrowed slice-compatible line view, without copying source text.
    ///
    /// This allocates only the reference table. Prefer [`Self::iter`] when a
    /// consumer does not require a slice.
    pub fn borrowed_lines(&self) -> Vec<&str> {
        self.iter().collect()
    }

    /// The full file text, line endings included.
    /// Explicit line arrays are joined with `\n`; their boundaries may not
    /// be recoverable by parsing this text with [`str::lines`].
    pub fn as_text(&self) -> &str {
        &self.text
    }

    /// Materialize owned lines. Only for APIs that still take `&[String]`;
    /// prefer [`Self::get`] / [`Self::iter`] on hot paths.
    pub fn to_vec(&self) -> Vec<String> {
        self.iter().map(str::to_owned).collect()
    }

    /// Bytes held on the heap by this buffer, approximated by capacity.
    pub fn heap_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.text.capacity())
            .saturating_add(self.line_starts.capacity().saturating_mul(size_of::<u32>()))
    }
}

fn offset_u32(offset: usize) -> u32 {
    u32::try_from(offset).expect("source file exceeds the 4 GiB CachedLines offset limit")
}

impl Index<usize> for CachedLines {
    type Output = str;

    fn index(&self, index: usize) -> &str {
        self.get(index)
            .unwrap_or_else(|| panic!("line index {index} out of bounds for {} lines", self.len()))
    }
}

impl From<String> for CachedLines {
    fn from(text: String) -> Self {
        Self::from_text(text)
    }
}

impl From<&str> for CachedLines {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}

impl From<Vec<String>> for CachedLines {
    fn from(lines: Vec<String>) -> Self {
        Self::from_lines(lines)
    }
}

/// File-content cache: the read-side hot layer for slicing and search.
pub struct FileCache {
    cache: Cache<String, CachedFile>,
}

#[derive(Clone)]
struct CachedFile {
    lines: Arc<CachedLines>,
    mtime_ms: u64,
    /// On-disk byte length when the caller supplied one.
    len: Option<u64>,
}

impl FileCache {
    pub fn new(cfg: CacheConfig) -> Self {
        let cache = Cache::builder()
            .max_capacity(cfg.budget_bytes)
            .weigher(|path: &String, file: &CachedFile| -> u32 {
                // True heap footprint: the key text, the per-entry metadata,
                // and the compact line buffer (text + offset table, at
                // capacity). Saturating to u32::MAX is required by moka's
                // per-entry weight type.
                let bytes = path
                    .capacity()
                    .saturating_add(size_of::<CachedFile>())
                    .saturating_add(file.lines.heap_bytes());
                u32::try_from(bytes).unwrap_or(u32::MAX)
            })
            .time_to_idle(cfg.idle_ttl)
            .build();

        Self { cache }
    }

    /// Returns the cached lines when the file's mtime still matches.
    pub fn get(&self, path: &str, mtime_ms: u64) -> Option<Arc<CachedLines>> {
        let file = self.cache.get(path)?;
        if file.mtime_ms == mtime_ms {
            Some(file.lines)
        } else {
            self.cache.invalidate(path);
            None
        }
    }

    pub fn insert(&self, path: &str, lines: impl Into<CachedLines>, mtime_ms: u64) {
        self.insert_arc(path, Arc::new(lines.into()), mtime_ms);
    }

    /// Insert already-shared lines without deep-copying them.
    pub fn insert_arc(&self, path: &str, lines: Arc<CachedLines>, mtime_ms: u64) {
        self.cache.insert(
            path.to_owned(),
            CachedFile {
                lines,
                mtime_ms,
                len: None,
            },
        );
    }

    /// Like [`Self::get`], but the file's byte length must match too. An
    /// mtime restored by `cp -p`/`git stash`, or two writes inside one
    /// timestamp tick, usually change the length; mtime alone cannot tell.
    /// An entry stored without a length never matches here.
    pub fn get_checked(&self, path: &str, mtime_ms: u64, len: u64) -> Option<Arc<CachedLines>> {
        let file = self.cache.get(path)?;
        if file.mtime_ms == mtime_ms && file.len == Some(len) {
            Some(file.lines)
        } else {
            self.cache.invalidate(path);
            None
        }
    }

    /// Store shared lines together with the file's byte length, for
    /// [`Self::get_checked`].
    pub fn insert_checked(&self, path: &str, lines: Arc<CachedLines>, mtime_ms: u64, len: u64) {
        self.cache.insert(
            path.to_owned(),
            CachedFile {
                lines,
                mtime_ms,
                len: Some(len),
            },
        );
    }

    /// Drop all cached file entries immediately (used during idle eviction /
    /// hibernation).
    pub fn clear(&self) {
        self.cache.invalidate_all();
        self.cache.run_pending_tasks();
    }

    /// Current weighted size in bytes — exposed so tests can assert the
    /// ceiling actually holds under load.
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    fn config(budget_bytes: u64, idle_ttl: Duration) -> CacheConfig {
        CacheConfig {
            budget_bytes,
            idle_ttl,
        }
    }

    fn cached(text: &str) -> CachedLines {
        CachedLines::from_text(text)
    }

    #[test]
    fn cached_lines_match_str_lines() {
        // `\r` only belongs to a line ending when it precedes `\n`; a bare
        // `\r` is content. A final line ending adds no empty line.
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\r\nb",
            "a\r",
            "a\r\n",
            "\r",
            "\n\r",
            "\r\n",
            "a\n\r",
            "a\rb",
            "\n",
            "\n\n",
            "a\n\nb",
            "a\n\r\nb",
            "日本語\nÉté\r\nß\n",
        ] {
            let expected: Vec<&str> = text.lines().collect();
            let lines = cached(text);
            assert_eq!(lines.len(), expected.len(), "len for {text:?}");
            assert_eq!(
                lines.is_empty(),
                expected.is_empty(),
                "is_empty for {text:?}"
            );
            assert_eq!(lines.to_vec(), expected, "lines for {text:?}");
            for (index, want) in expected.iter().enumerate() {
                assert_eq!(lines.get(index), Some(*want), "get({index}) for {text:?}");
                assert_eq!(&lines[index], *want, "index {index} for {text:?}");
            }
            assert_eq!(lines.get(expected.len()), None);
            assert_eq!(
                lines.iter().rev().collect::<Vec<_>>(),
                expected.iter().rev().copied().collect::<Vec<_>>()
            );
            assert_eq!(lines.as_text(), text);
        }
    }

    #[test]
    fn cached_lines_from_lines_roundtrips_owned_lines() {
        let lines = CachedLines::from_lines(vec!["first".into(), "second".into()]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.get(0), Some("first"));
        assert_eq!(lines.get(1), Some("second"));
        assert_eq!(lines.to_vec(), CachedLines::from("first\nsecond").to_vec());
        assert!(CachedLines::from(Vec::new()).is_empty());
        // A `Vec<String>` is accepted wherever `Into<CachedLines>` is expected.
        let lines: CachedLines = vec!["x".to_string()].into();
        assert_eq!(lines.get(0), Some("x"));
    }

    #[test]
    fn explicit_line_arrays_preserve_empty_lines_and_carriage_returns() {
        let cache = FileCache::new(CacheConfig::default());
        for expected in [
            vec![],
            vec![""],
            vec!["", ""],
            vec!["a", ""],
            vec!["a", "", ""],
            vec!["\r", ""],
            vec!["a\r", "b\r", ""],
            vec!["a\rb", "日本語\r", "Été", ""],
        ] {
            let owned: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
            let lines = CachedLines::from_lines(owned.clone());
            assert_eq!(lines.len(), expected.len());
            assert_eq!(lines.to_vec(), owned);
            assert_eq!(lines.borrowed_lines(), expected);
            assert_eq!(
                lines.iter().rev().collect::<Vec<_>>(),
                expected.iter().rev().copied().collect::<Vec<_>>()
            );
            assert_eq!(lines.as_text(), expected.join("\n"));
            for (index, line) in expected.iter().enumerate() {
                assert_eq!(lines.get(index), Some(*line));
                assert_eq!(&lines[index], *line);
            }
            assert_eq!(lines.get(expected.len()), None);
            cache.insert("roundtrip", owned.clone(), 1);
            assert_eq!(cache.get("roundtrip", 1).unwrap().to_vec(), owned);
        }
    }

    #[test]
    fn borrowed_lines_point_into_the_text_buffer() {
        let lines = CachedLines::from_text("first\r\nsecond\n");
        let view = lines.borrowed_lines();
        assert_eq!(view, ["first", "second"]);
        assert_eq!(view[0].as_ptr(), lines.as_text().as_ptr());
        assert_eq!(view[1].as_ptr(), lines.as_text()[7..].as_ptr());
    }

    #[test]
    fn cached_lines_heap_bytes_counts_capacity() {
        let lines = CachedLines::from_text("one\ntwo\nthree");
        assert!(lines.heap_bytes() >= size_of::<CachedLines>() + lines.as_text().len());
        let empty = CachedLines::default();
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.heap_bytes(), size_of::<CachedLines>());
    }

    #[test]
    fn checked_get_rejects_same_mtime_different_length() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert_checked(
            "a.rs",
            Arc::new(CachedLines::from_lines(vec!["old".into()])),
            1000,
            4,
        );
        assert!(cache.get_checked("a.rs", 1000, 4).is_some());
        // Same mtime, different length: the file changed under a restored timestamp.
        assert!(cache.get_checked("a.rs", 1000, 5).is_none());
        assert!(
            cache.get_checked("a.rs", 1000, 4).is_none(),
            "a mismatch must evict the stale entry"
        );
        // Entries stored without a length never satisfy a checked read.
        cache.insert("b.rs", vec!["x".into()], 7);
        assert!(cache.get_checked("b.rs", 7, 1).is_none());
    }

    #[test]
    fn insert_then_get_returns_shared_lines() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert("src/lib.rs", vec!["first".into(), "second".into()], 42);

        let lines = cache.get("src/lib.rs", 42).expect("entry should exist");
        assert_eq!(
            lines.to_vec(),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn changed_mtime_is_a_miss_and_invalidates_entry() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert("src/lib.rs", vec!["old".into()], 10);

        assert!(cache.get("src/lib.rs", 11).is_none());
        assert!(cache.get("src/lib.rs", 10).is_none());
    }

    #[test]
    fn clear_drops_all_entries() {
        let cache = FileCache::new(CacheConfig::default());
        cache.insert("a.rs", vec!["a".into()], 1);
        cache.insert("b.rs", vec!["b".into()], 1);
        assert!(cache.get("a.rs", 1).is_some());
        assert!(cache.get("b.rs", 1).is_some());

        cache.clear();

        assert!(cache.get("a.rs", 1).is_none());
        assert!(cache.get("b.rs", 1).is_none());
        assert_eq!(cache.weighted_size(), 0);
    }

    #[test]
    fn byte_budget_is_enforced() {
        const BUDGET: u64 = 1024;
        let cache = FileCache::new(config(BUDGET, Duration::from_secs(60)));

        for index in 0..8 {
            cache.insert(
                &format!("large-{index}"),
                vec!["x".repeat(BUDGET as usize)],
                index,
            );
        }
        cache.cache.run_pending_tasks();

        assert!(cache.weighted_size() <= BUDGET);
    }

    #[test]
    fn idle_entries_expire() {
        let cache = FileCache::new(config(1024, Duration::from_millis(100)));
        cache.insert("idle", vec!["line".into()], 1);
        assert!(cache.get("idle", 1).is_some());

        thread::sleep(Duration::from_millis(175));

        assert!(cache.get("idle", 1).is_none());
    }

    #[test]
    fn concurrent_reads_and_writes_do_not_panic() {
        let cache = Arc::new(FileCache::new(config(64 * 1024, Duration::from_secs(60))));
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for index in 0..250 {
                        let path = format!("worker-{worker}/file-{}", index % 20);
                        cache.insert(&path, vec![format!("line-{index}")], index);
                        let _ = cache.get(&path, index);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("cache worker panicked");
        }
        cache.cache.run_pending_tasks();
        assert!(cache.weighted_size() <= 64 * 1024);
    }
}
