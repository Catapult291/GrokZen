#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Model to use for generating the compaction summary.
    /// None means use the session's current model.
    pub compact_model: Option<String>,

    /// Whether to run a memory flush turn before each compaction.
    /// When enabled, the session actor asks the model to summarize important information from the conversation before it's compacted.
    /// Requires the memory system to be enabled.
    pub memory_flush_enabled: bool,

    /// Per-compaction wall-clock budget (seconds); a generation exceeding it is cut and retried.
    /// This is the backstop for runaway reasoning that token limits miss.
    pub wall_clock_budget_secs: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            compact_model: None,
            memory_flush_enabled: false,
            wall_clock_budget_secs: 300,
        }
    }
}
