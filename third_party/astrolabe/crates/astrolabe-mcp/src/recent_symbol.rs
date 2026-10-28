//! Per-MCP-session memory of the last unambiguous symbol → path[:line].
//!
//! Agents often call `find_symbol` / `goto_definition`, then `find_references`
//! without `path`. Ambiguous names (e.g. many `Init`s) then get a weak unique-
//! definition guess or an opaque "needs path" error. Remembering the last
//! single-hit resolution in this process lets `find_references` inherit a path
//! when the schema still leaves it optional.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Cap so a long session cannot grow without bound.
const MAX_ENTRIES: usize = 64;

/// One remembered anchor for an exact symbol name under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentSymbolAnchor {
    pub path: String,
    /// 0-based line when known (definition / use site).
    pub line: Option<u32>,
}

/// Process-local map keyed by `root\0symbol`.
#[derive(Debug, Default)]
pub(crate) struct RecentSymbols {
    inner: Mutex<HashMap<String, RecentSymbolAnchor>>,
}

fn key(root: &Path, symbol: &str) -> String {
    format!("{}\0{}", root.display(), symbol)
}

impl RecentSymbols {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Remember only when the caller has a single unambiguous hit.
    pub(crate) fn remember(&self, root: &Path, symbol: &str, path: &str, line: Option<u32>) {
        let symbol = symbol.trim();
        let path = path.trim();
        if symbol.is_empty() || path.is_empty() {
            return;
        }
        let k = key(root, symbol);
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_ENTRIES && !map.contains_key(&k) {
            // Drop an arbitrary entry; order is not part of the contract.
            if let Some(evict) = map.keys().next().cloned() {
                map.remove(&evict);
            }
        }
        map.insert(
            k,
            RecentSymbolAnchor {
                path: path.to_string(),
                line,
            },
        );
    }

    pub(crate) fn recall(&self, root: &Path, symbol: &str) -> Option<RecentSymbolAnchor> {
        let symbol = symbol.trim();
        if symbol.is_empty() {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key(root, symbol))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn remember_and_recall_round_trip() {
        let store = RecentSymbols::new();
        let root = PathBuf::from("/tmp/repo");
        assert!(store.recall(&root, "Init").is_none());
        store.remember(&root, "Init", "pkg/a.go", Some(10));
        let hit = store.recall(&root, "Init").expect("remembered");
        assert_eq!(hit.path, "pkg/a.go");
        assert_eq!(hit.line, Some(10));
        // Other roots / names stay separate.
        assert!(store.recall(&PathBuf::from("/tmp/other"), "Init").is_none());
        assert!(store.recall(&root, "Other").is_none());
    }

    #[test]
    fn empty_symbol_or_path_is_ignored() {
        let store = RecentSymbols::new();
        let root = PathBuf::from("/tmp/repo");
        store.remember(&root, "  ", "a.go", None);
        store.remember(&root, "Init", "", None);
        assert!(store.recall(&root, "Init").is_none());
        assert!(store.recall(&root, "  ").is_none());
    }
}
