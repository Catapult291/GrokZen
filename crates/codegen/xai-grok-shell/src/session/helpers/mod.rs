pub mod chat;
pub mod compaction_context;
pub mod full_replace_compaction;
pub mod memory_context;
pub mod memory_flush_window;
pub mod prepared_compaction_history;
pub mod prompt_suggest;
pub mod replay;
pub mod rewind_preview;
pub mod session_compact;
pub mod session_recap;
pub mod session_summary;
pub mod tool_input_parsing;
pub mod turn_summary;

pub use compaction_context::CompactionStateContext;

/// Dedicated side model for cache-sensitive aux calls (flush/title/dream/suggest/turn-summary)
/// so they do not compete with the main conversation's KV pool.
/// Catalog-guarded at each call site: when missing (or already the session model), fall back to the session path.
/// Matches the cache-miss G1/G2 target (`cline-pass/deepseek-v4.1-flash`).
pub(crate) const CACHE_MISS_SIDE_MODEL: &str = "cline-pass/deepseek-v4.1-flash";
