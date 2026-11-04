use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockOutput};

use super::agent_tool_call_fold::{fold_raw_tool_calls, mentions_start_marker};
use super::markdown_content::MarkdownContent;
use super::mermaid_content::{self, MermaidContent};
use crate::appearance::AppearanceConfig;

/// How much of the already-emitted text is kept to spot a start marker split across chunks.
const PROBE_CHARS: usize = 32;

/// Block displaying an agent message with streaming markdown support. When text arrives in chunks, call
/// `push_chunk()` to append without re-rendering everything.
#[derive(Debug, Clone)]
pub struct AgentMessageBlock {
    content: MarkdownContent,
    /// Cached image references extracted from the markdown source.
    image_refs: Vec<crate::prompt_images::ScrollbackImageRef>,
    /// Cached video references extracted from the markdown source.
    video_refs: Vec<crate::prompt_images::ScrollbackVideoRef>,
    /// Detected ` ```mermaid ` diagrams and render skeleton, populated at construction/finish (never per streaming chunk) like the media refs.
    mermaid: MermaidContent,
    /// Verbatim text once raw tool-call XML folding rewrote the displayed source; `None` while
    /// `content` still holds the text unchanged.
    raw_source: Option<String>,
    /// Tail of the emitted text, used to catch a start marker whose bytes straddle two chunks.
    probe: String,
}

impl AgentMessageBlock {
    /// Create a new agent message block with complete text.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let folded = fold_raw_tool_calls(&text, true);
        let image_refs = crate::prompt_images::extract_image_refs(&folded);
        let video_refs = crate::prompt_images::extract_video_refs(&folded);
        let raw_source = (folded != text).then(|| text.clone());
        let content = if raw_source.is_some() {
            let mut content = MarkdownContent::streaming();
            content.push_chunk(&folded);
            content
        } else {
            MarkdownContent::new(folded)
        };
        let mermaid = content.mermaid_content();
        Self {
            content,
            image_refs,
            video_refs,
            mermaid,
            raw_source,
            probe: String::new(),
        }
    }

    /// Create an empty block for streaming.
    pub fn streaming() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            image_refs: Vec::new(),
            video_refs: Vec::new(),
            mermaid: MermaidContent::default(),
            raw_source: None,
            probe: String::new(),
        }
    }

    /// Push a streaming chunk of markdown text.
    pub fn push_chunk(&mut self, chunk: &str) {
        if let Some(raw) = self.raw_source.as_mut() {
            raw.push_str(chunk);
            self.refresh_folded(false, false);
        } else if mentions_start_marker(&self.probe, chunk) {
            let mut raw = self.content.text();
            raw.push_str(chunk);
            self.raw_source = Some(raw);
            self.refresh_folded(false, false);
        } else {
            self.content.push_chunk(chunk);
            self.note_probe(chunk);
        }
    }

    /// Push a chunk without rendering immediately.
    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        if let Some(raw) = self.raw_source.as_mut() {
            raw.push_str(chunk);
            self.refresh_folded(true, false);
        } else if mentions_start_marker(&self.probe, chunk) {
            let mut raw = self.content.text();
            raw.push_str(chunk);
            self.raw_source = Some(raw);
            self.refresh_folded(true, false);
        } else {
            self.content.push_chunk_deferred(chunk);
            self.note_probe(chunk);
        }
    }

    /// Get the source markdown text with any raw tool-call XML intact.
    pub fn text(&self) -> String {
        match &self.raw_source {
            Some(raw) => raw.clone(),
            None => self.content.text(),
        }
    }

    /// Whether the source markdown is empty (zero-alloc, unlike `text()`).
    pub fn is_empty(&self) -> bool {
        match &self.raw_source {
            Some(raw) => raw.is_empty(),
            None => self.content.is_empty(),
        }
    }

    /// Finish streaming and do a full re-render for safety.
    pub fn finish(&mut self) {
        if self.raw_source.is_some() {
            self.refresh_folded(false, true);
        }
        self.content.finish();
        let text = self.content.text();
        self.image_refs = crate::prompt_images::extract_image_refs(&text);
        self.video_refs = crate::prompt_images::extract_video_refs(&text);
        // Detection runs once the render is final, after the renderer freezes, never per streaming chunk
        self.mermaid = self.content.mermaid_content();
        self.probe.clear();
    }

    /// Re-derive the displayed text after `raw_source` grew, appending the delta when the folded
    /// prefix is unchanged and replacing the whole source when an earlier summary changed.
    fn refresh_folded(&mut self, deferred: bool, final_chunk: bool) {
        let Some(raw) = self.raw_source.as_deref() else {
            return;
        };
        let folded = fold_raw_tool_calls(raw, final_chunk);
        let current = self.content.text();
        if folded == current {
            return;
        }
        if let Some(delta) = folded.strip_prefix(current.as_str()) {
            if deferred {
                self.content.push_chunk_deferred(delta);
            } else {
                self.content.push_chunk(delta);
            }
        } else {
            self.content.replace_source(&folded);
        }
    }

    fn note_probe(&mut self, chunk: &str) {
        self.probe.push_str(chunk);
        let excess = self.probe.chars().count().saturating_sub(PROBE_CHARS);
        if excess > 0 {
            let cut = self
                .probe
                .char_indices()
                .nth(excess)
                .map(|(index, _)| index)
                .unwrap_or(self.probe.len());
            self.probe.drain(..cut);
        }
    }

    /// The detected Mermaid diagrams for this message (empty until finished or constructed from complete text).
    pub fn mermaid(&self) -> &MermaidContent {
        &self.mermaid
    }

    /// Set the raw mode, re-rendering if it changed.
    /// Raw mode reveals the verbatim text, so the folded XML can be inspected on demand.
    pub fn set_raw_mode(&mut self, raw: bool) {
        if let Some(source) = self.raw_source.clone() {
            let target = if raw {
                source
            } else {
                fold_raw_tool_calls(&source, true)
            };
            if target != self.content.text() {
                self.content.replace_source(&target);
            }
        }
        self.content.set_raw_mode(raw);
    }

    /// Access the underlying markdown content (for viewer item building).
    pub fn content(&self) -> &MarkdownContent {
        &self.content
    }

    /// Mutable access to the underlying markdown content.
    pub fn content_mut(&mut self) -> &mut MarkdownContent {
        &mut self.content
    }

    /// Get copyable text for this block.
    /// When `raw` is true, returns the raw markdown source.
    /// When `raw` is false, returns the rendered text (styles stripped).
    pub fn copy_text(&self, raw: bool) -> String {
        if raw {
            self.text()
        } else {
            self.content.rendered_plain_text()
        }
    }
}

impl AgentMessageBlock {
    /// Resolve the diagram display mode from the user setting without building `output()`; cheap enough to gate the per-frame affordance path.
    fn mermaid_display_mode(&self) -> mermaid_content::MermaidDisplay {
        // Minimal mode commits static text with no draw loop to paint the clickable affordance row, so suppress it there
        // The diagram art still renders; its source stays natively selectable
        // The inline-overlay force-off flag is set iff minimal
        mermaid_content::mermaid_display_static(
            crate::appearance::cache::load_render_mermaid(),
            crate::terminal::image::scrollback_inline_overlay_forced_off(),
        )
    }

    /// It is deterministic for a given `ctx`, so the two calls produce matching rows and offsets without a shared cache
    /// that could drift. Only callers that have already confirmed there are diagrams and we are not in raw mode should
    /// reach here. The common diagram-free path never pays this build.
    fn rendered_output(
        &self,
        ctx: &BlockContext,
    ) -> (BlockOutput, Vec<mermaid_content::DiagramAffordance>) {
        let mut out = self.content.output(ctx.width as usize);
        // Diagram pre-wrap ranges in document order
        // The fence count and order are width-invariant, so range index `idx` pairs with the diagram's source (`self.mermaid.source(idx)`)
        let ranges = self.content.mermaid_block_ranges();

        match self.mermaid_display_mode() {
            mermaid_content::MermaidDisplay::SourceOnly => (out, Vec::new()),
            mermaid_content::MermaidDisplay::Affordances => {
                let affordances =
                    mermaid_content::apply_affordance_rows(&mut out, &ranges, |idx| {
                        self.mermaid.source(idx).unwrap_or_default().to_string()
                    });
                (out, affordances)
            }
        }
    }
}

impl BlockContent for AgentMessageBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        // Common path: no diagrams (or raw mode) renders plain markdown, with no affordance machinery and no extra output rebuild
        if ctx.raw || self.mermaid.is_empty() {
            return self.content.output(ctx.width as usize);
        }
        self.rendered_output(ctx).0
    }

    fn diagram_affordances(&self, ctx: &BlockContext) -> Vec<mermaid_content::DiagramAffordance> {
        // Affordance rows exist only under the affordance display with diagrams
        // For every other (much more common) case, return without building output()
        if ctx.raw
            || self.mermaid.is_empty()
            || self.mermaid_display_mode() != mermaid_content::MermaidDisplay::Affordances
        {
            return Vec::new();
        }
        self.rendered_output(ctx).1
    }

    fn estimate_extra_rows(&self) -> u16 {
        // Each diagram inserts one treatment row (affordance row or fallback caption) into output() that the source-text estimate can't see
        // Count one per diagram (a safe over-estimate if a range is empty) so the off-screen estimate never under-reserves
        // Raw mode and the `off` setting add no such row
        if self.mermaid.is_empty()
            || self.content.is_raw()
            || self.mermaid_display_mode() == mermaid_content::MermaidDisplay::SourceOnly
        {
            return 0;
        }
        self.mermaid.len() as u16
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        true
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn image_references(&self) -> &[crate::prompt_images::ScrollbackImageRef] {
        &self.image_refs
    }

    fn video_references(&self) -> &[crate::prompt_images::ScrollbackVideoRef] {
        &self.video_refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::{AppearanceConfig, RenderMermaid};
    use crate::scrollback::types::Selectable;

    fn ctx(width: u16, raw: bool) -> BlockContext {
        BlockContext {
            mode: crate::scrollback::DisplayMode::Expanded,
            is_running: false,
            width,
            raw,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
            locale: Default::default(),
        }
    }

    fn shown_text(out: &BlockOutput) -> String {
        out.lines
            .iter()
            .flat_map(|line| line.content.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const RETRIED_BLOBS: &str = "改用正确的参数继续查。\n\
<tool_call><function=search_code><parameter=glob>*.rs\n</parameter><parameter=query>alpha</parameter></function></tool_call>\n\
<tool_call><function=search_code><parameter=glob>*.rs\n</parameter><parameter=query>beta</parameter></function></tool_call>\n\
<tool_call><function=search_code><parameter=glob>*.rs\n</parameter><parameter=query>gamma</parameter></function></tool_call>\n";

    #[test]
    fn raw_tool_call_xml_collapses_to_one_summary_line() {
        let block = AgentMessageBlock::new(RETRIED_BLOBS);
        let shown = shown_text(&block.output(&ctx(80, false)));
        assert!(!shown.contains("<tool_call"), "{shown}");
        assert_eq!(shown.matches("tool_call XML").count(), 1, "{shown}");
        assert!(shown.contains("tool_call XML ×3"), "{shown}");
        assert!(shown.contains("search_code(glob=*.rs) ×3"), "{shown}");
        // The verbatim source survives for raw copy and export.
        assert_eq!(block.copy_text(true), RETRIED_BLOBS);
        assert_eq!(block.text(), RETRIED_BLOBS);
    }

    #[test]
    fn streaming_marker_split_across_chunks_never_renders_raw_xml() {
        let mut block = AgentMessageBlock::streaming();
        block.push_chunk("prose\n<tool_ca");
        let partial = shown_text(&block.output(&ctx(80, false)));
        assert!(!partial.contains("<tool_ca"), "{partial}");

        block.push_chunk(
            "ll><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>",
        );
        block.finish();
        let shown = shown_text(&block.output(&ctx(80, false)));
        assert!(shown.contains("read_file(target_file=a.rs)"), "{shown}");
        assert!(!shown.contains("<tool_call"), "{shown}");
        assert!(block.text().contains("<tool_call>"), "raw source kept");
    }

    #[test]
    fn unterminated_stream_is_folded_on_finish() {
        let raw = "后台启动。\n<tool_call><function=monitor><parameter=command>tail -f log";
        let mut block = AgentMessageBlock::streaming();
        block.push_chunk(raw);
        let streaming = shown_text(&block.output(&ctx(80, false)));
        assert!(!streaming.contains("<tool_call"), "{streaming}");

        block.finish();
        let shown = shown_text(&block.output(&ctx(80, false)));
        assert!(shown.contains("tool_call XML ×1"), "{shown}");
        assert!(shown.contains("monitor(command=tail -f log)"), "{shown}");
        assert_eq!(block.text(), raw);
    }

    #[test]
    fn eight_identical_blobs_render_one_summary_line() {
        let blob = "<tool_call><function=run_terminal_command><parameter=block_until_ms>0</parameter><parameter=command>python script.py</parameter></function></tool_call>\n";
        let raw = format!("后台启动。\n{}", blob.repeat(8));
        let block = AgentMessageBlock::new(&raw);
        let shown = shown_text(&block.output(&ctx(100, false)));
        assert!(!shown.contains("<tool_call"), "{shown}");
        assert_eq!(shown.matches("tool_call XML").count(), 1, "{shown}");
        assert!(shown.contains("tool_call XML ×8"), "{shown}");
        assert!(
            shown.contains("run_terminal_command(block_until_ms=0) ×8"),
            "{shown}"
        );
    }

    #[test]
    fn raw_mode_reveals_the_original_xml() {
        let mut block = AgentMessageBlock::new(RETRIED_BLOBS);
        block.set_raw_mode(true);
        let raw = shown_text(&block.output(&ctx(200, true)));
        assert!(raw.contains("<tool_call>"), "{raw}");

        block.set_raw_mode(false);
        let pretty = shown_text(&block.output(&ctx(200, false)));
        assert!(!pretty.contains("<tool_call>"), "{pretty}");
        assert!(pretty.contains("tool_call XML ×3"), "{pretty}");
    }

    #[test]
    fn prose_without_xml_is_untouched() {
        let md = "plain **markdown** stays put\n";
        let block = AgentMessageBlock::new(md);
        assert_eq!(block.text(), md);
        assert!(shown_text(&block.output(&ctx(80, false))).contains("markdown"));
    }

    #[test]
    fn agent_markdown_body_uses_single_logical_range() {
        let block = AgentMessageBlock::new("hello world this should wrap across lines");
        let out = block.output(&ctx(10, false));
        assert!(out.lines.len() > 1);
        assert!(out.lines.iter().all(|line| line.selection_range == Some(0)));
        assert!(
            out.lines
                .iter()
                .all(|line| !matches!(line.selectable, Selectable::None))
        );
    }

    #[test]
    fn agent_copy_text_preserves_raw_semantics() {
        let block = AgentMessageBlock::new("**bold** text");
        assert_eq!(block.copy_text(true), "**bold** text");
        assert_eq!(
            block.copy_text(false),
            block.content().rendered_plain_text()
        );
    }

    #[test]
    fn mermaid_detected_at_construction() {
        let block = AgentMessageBlock::new("```mermaid\nflowchart TD\n  A --> B\n```\n");
        assert_eq!(block.mermaid().len(), 1);
        assert_eq!(block.mermaid().source(0), Some("flowchart TD\n  A --> B\n"));
    }

    #[test]
    fn mermaid_not_detected_during_streaming_until_finish() {
        let mut block = AgentMessageBlock::streaming();
        block.push_chunk("```mermaid\nflowchart TD\n");
        // Fence still open mid-stream, so no detection
        assert!(block.mermaid().is_empty());
        block.push_chunk("A --> B\n```\n");
        assert!(
            block.mermaid().is_empty(),
            "detection runs at finish(), not per chunk"
        );
        block.finish();
        assert_eq!(block.mermaid().len(), 1);
    }

    const MERMAID_MD: &str = "```mermaid\nflowchart TD\n  A --> B\n```\n";

    #[test]
    fn mermaid_treatment_row_shown_in_auto_not_off_not_raw() {
        let non_selectable = |o: &BlockOutput| {
            o.lines
                .iter()
                .filter(|l| matches!(l.selectable, Selectable::None))
                .count()
        };
        // off: plain code block, no extra row
        crate::appearance::cache::set_render_mermaid(RenderMermaid::Off);
        let off = AgentMessageBlock::new(MERMAID_MD).output(&ctx(40, false));
        assert_eq!(non_selectable(&off), 0, "off mode must not add a row");

        // auto: exactly one extra non-selectable row beneath the diagram (the affordance row)
        // The row is blank in `output()`; the draw loop paints its `◇ mermaid [Open Image] [Copy Image Path] [Copy Source]` buttons
        crate::appearance::cache::set_render_mermaid(RenderMermaid::Auto);
        let auto = AgentMessageBlock::new(MERMAID_MD).output(&ctx(40, false));
        assert_eq!(
            auto.lines.len(),
            off.lines.len() + 1,
            "auto mode adds one treatment row"
        );
        assert_eq!(non_selectable(&auto), 1);

        // raw: verbatim source, no extra row even in auto
        crate::appearance::cache::set_render_mermaid(RenderMermaid::Auto);
        let raw = AgentMessageBlock::new(MERMAID_MD).output(&ctx(40, true));
        assert_eq!(non_selectable(&raw), 0, "raw mode shows the fence verbatim");
    }

    #[test]
    fn mermaid_treatment_row_preserves_hyperlink_line_mapping() {
        // The inserted treatment row (caption or affordance) is a joiner-continuation line, so it must NOT add a logical (pre-wrap) line
        // Otherwise the hyperlink overlay walk desyncs for the paragraph after the diagram
        let md = "before\n\n```mermaid\nA-->B\n```\n\n[link](https://example.com) trailing\n";
        crate::appearance::cache::set_render_mermaid(RenderMermaid::Off);
        let off = AgentMessageBlock::new(md).output(&ctx(60, false));
        crate::appearance::cache::set_render_mermaid(RenderMermaid::Auto);
        let block = AgentMessageBlock::new(md);
        let auto = block.output(&ctx(60, false));

        let logical = |o: &BlockOutput| o.lines.iter().filter(|l| l.joiner.is_none()).count();
        assert_eq!(
            logical(&auto),
            logical(&off),
            "treatment row must not introduce a new logical line",
        );

        // The renderer's hyperlinks are pre-wrap and unchanged by the inserted row (it lives in the BlockOutput, not the renderer)
        // Walk the output's joiners to recover each row's pre-wrap index
        // The link's pre-wrap line must still map to its row; the inserted row did not shift it
        let link_line = block
            .content()
            .with_hyperlinks(|hs| hs.iter().map(|h| h.line_index).min())
            .expect("the trailing link must be detected");
        let mut prewrap = 0usize;
        let mut mapped_text = String::new();
        for (row, line) in auto.lines.iter().enumerate() {
            if row > 0 && line.joiner.is_none() {
                prewrap += 1;
            }
            if prewrap == link_line {
                mapped_text = line
                    .content
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                break;
            }
        }
        assert!(
            mapped_text.contains("example.com"),
            "link pre-wrap line {link_line} must still map to the link row, got {mapped_text:?}",
        );
    }

    // -- diagram affordance rows ---------------------------------------------

    mod affordances {
        use super::*;

        fn shown_text(out: &BlockOutput) -> String {
            out.lines
                .iter()
                .flat_map(|l| l.content.spans.iter())
                .map(|s| s.content.as_ref())
                .collect()
        }

        /// A detected diagram keeps its source code block on screen and exposes a single affordance row.
        /// The row carries the diagram source, the data every lazy `[Open]`/`[Copy path]`/`[Copy source]` button acts on.
        /// Rendering is lazy, so no path/state is tracked on the row.
        #[test]
        fn diagram_exposes_affordance_carrying_source_and_keeps_source_block() {
            crate::appearance::cache::set_render_mermaid(RenderMermaid::On);
            let block = AgentMessageBlock::new("intro\n\n```mermaid\nA-->B\n```\n\nbye\n");

            let affs = block.diagram_affordances(&ctx(60, false));
            assert_eq!(affs.len(), 1, "one diagram → one affordance row");
            let Some(aff) = affs.first() else {
                panic!("expected one affordance: {affs:?}");
            };
            assert_eq!(aff.source, "A-->B\n");

            // The diagram is shown as its source code block (never an image)
            // The affordance row sits at its reported (non-selectable) offset
            let out = block.output(&ctx(60, false));
            assert!(
                shown_text(&out).contains("A-->B"),
                "the source code block stays on screen",
            );
            assert!(matches!(
                out.lines
                    .get(aff.row_offset as usize)
                    .map(|l| &l.selectable),
                Some(&Selectable::None)
            ));
        }

        #[test]
        fn raw_mode_suppresses_affordances_and_shows_source() {
            crate::appearance::cache::set_render_mermaid(RenderMermaid::On);
            let block = AgentMessageBlock::new("```mermaid\nA-->B\n```\n");

            assert!(
                block.diagram_affordances(&ctx(60, true)).is_empty(),
                "raw mode suppresses affordances",
            );
            assert!(shown_text(&block.output(&ctx(60, true))).contains("A-->B"));

            // Toggling back to pretty restores the affordance row.
            assert_eq!(block.diagram_affordances(&ctx(60, false)).len(), 1);
        }

        #[test]
        fn off_setting_shows_source_with_no_affordances() {
            crate::appearance::cache::set_render_mermaid(RenderMermaid::Off);
            let block = AgentMessageBlock::new("```mermaid\nA-->B\n```\n");
            assert!(block.diagram_affordances(&ctx(60, false)).is_empty());
            assert!(shown_text(&block.output(&ctx(60, false))).contains("A-->B"));
        }

        #[test]
        fn copy_over_diagram_yields_fence_body() {
            use crate::scrollback::block::RenderBlock;
            crate::appearance::cache::set_render_mermaid(RenderMermaid::On);
            // Drive the real whole-block copy path (`copy_visible_text_in_state`, then `plain_text_from_output`)
            // Re-implementing the selectable filter would drift from production
            // The source code block is selectable; the blank affordance row is excluded
            let block = RenderBlock::AgentMessage(AgentMessageBlock::new(
                "```mermaid\nA-->B\nC-->D\n```\n",
            ));
            let copied = block
                .copy_visible_text_in_state(&ctx(60, false))
                .expect("the source code block yields selectable copy text");
            assert!(copied.contains("A-->B"), "copy yields source: {copied:?}");
            assert!(copied.contains("C-->D"), "copy yields source: {copied:?}");
        }

        /// With two diagrams, each affordance row anchors at its OWN (non-selectable) row in the final output, in document order.
        /// Each row carries that diagram's own source.
        #[test]
        fn two_diagrams_each_anchor_at_their_own_row() {
            crate::appearance::cache::set_render_mermaid(RenderMermaid::On);
            let md = "intro line\n\n```mermaid\nAAA-->BBB\n```\n\nmid line\n\n```mermaid\nCCC-->DDD\n```\n\nbye line\n";
            let block = AgentMessageBlock::new(md);
            assert_eq!(block.mermaid().len(), 2, "two diagrams");

            let out = block.output(&ctx(60, false));
            let affs = block.diagram_affordances(&ctx(60, false));
            assert_eq!(affs.len(), 2);
            let [a0, a1] = affs.as_slice() else {
                panic!("expected two affordances: {affs:?}");
            };
            assert!(
                a0.row_offset < a1.row_offset,
                "diagram order preserved: {} < {}",
                a0.row_offset,
                a1.row_offset,
            );
            assert_eq!(a0.source, "AAA-->BBB\n");
            assert_eq!(a1.source, "CCC-->DDD\n");
            for aff in &affs {
                assert!(matches!(
                    out.lines
                        .get(aff.row_offset as usize)
                        .map(|l| &l.selectable),
                    Some(&Selectable::None)
                ));
            }
            // Both diagrams' sources remain visible as code blocks.
            assert!(shown_text(&out).contains("AAA-->BBB"));
            assert!(shown_text(&out).contains("CCC-->DDD"));
        }
    }
}
