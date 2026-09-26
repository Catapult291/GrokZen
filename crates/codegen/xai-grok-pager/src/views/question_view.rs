//! Question view state and helpers.
//!
//! When the agent calls `AskUserQuestion`, the pager takes over the prompt
//! area and shows a structured question UI. This module contains:
//!
//! - [`QuestionViewState`]: all state for the question overlay
//! - [`QuestionSelection`]: per-question selection tracking
//! - [`QuestionFocus`]: navigation vs input mode
//!
//! No rendering or input handling here; this is pure data and helpers.

use std::collections::HashSet;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use xai_acp_lib::AcpResult;
use xai_grok_markdown::StreamingMarkdownRenderer;
pub use xai_grok_tools::implementations::grok_build::ask_user_question::{
    AskUserQuestionExtResponse, AskUserQuestionMode, Question, QuestionAnswerImage, QuestionOption,
};

use unicode_width::UnicodeWidthStr;

use crate::input::key::RowWalk;
use crate::render::line_utils::{byte_offset_at_width, truncate_line, truncate_str};
use crate::render::safe_buf::SafeBuf;
use crate::render::wrapping::word_wrap_lines_with_joiners;
use crate::syntax::get_syntect;
use crate::theme::Theme;
use crate::theme::md_style;
use crate::views::prompt_widget::{PromptBg, PromptStyle, StashedPrompt};

/// Maximum description lines shown in the question chrome before truncation.
const DEFAULT_MAX_CHROME_DESC_LINES: u16 = 5;

/// Maximum preview lines shown in the question chrome before truncation.
const DEFAULT_MAX_CHROME_PREVIEW_LINES: u16 = 6;

/// Minimum number of option rows that must be visible before dynamic cap reduction kicks in.
const MIN_VISIBLE_OPTION_ROWS: u16 = 3;

fn hovered_bg(theme: &Theme) -> ratatui::style::Color {
    theme.bg_hover
}

// ── Enums ──────────────────────────────────────────────────────────────

/// Per-question selection state.
#[derive(Debug, Clone)]
pub enum QuestionSelection {
    /// Single-choice: at most one option selected.
    Single(Option<usize>),
    /// Multi-choice: zero or more options toggled on.
    Multi(HashSet<usize>),
}

/// A cursor move within one question's answer rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMotion {
    Next,
    Prev,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    First,
    Last,
}

/// Focus mode within the question view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionFocus {
    /// Cursor is on an option row or the free-form row (not typing).
    /// j/k navigate, Enter selects or enters input mode.
    Navigation,
    /// User is typing in the TextArea (input mode).
    /// @-dropdown may be open. Esc exits back to Navigation.
    InputMode,
}

/// Pager-internal origin for a locally-opened question (one that was NOT driven by an ACP `x.ai/ask_user_question` request).
///
/// Drives what `submit_question_answers` returns when the user submits.
/// Mutually exclusive with `QuestionViewState.response_tx`: a local question never has an ACP sender.
///
/// Each variant carries the data the local handler needs to translate the submitted selection into an [`crate::app::actions::Action`].
///
/// Not `Clone`: `FeedbackTrace` owns its attachments' staged temp files.
#[derive(Debug)]
pub enum LocalQuestionKind {
    /// Hard-modal card opened when a `UserPromptSubmit` hook blocks a prompt.
    /// Carries the local queue row the blocked prompt was requeued into.
    /// Esc, Ctrl+C, and dismissal are refused: the queue stays parked until the user picks Edit, Resend, or Discard.
    /// The pick is translated into [`crate::app::actions::Action::PromptBlockAnswered`].
    PromptBlocked {
        row_id: u64,
    },
    /// Modal opened by `/fork` to resolve the worktree question.
    /// On submit, the selected option index plus the carried directive and fork point are translated into an [`crate::app::actions::Action::ForkAnswered`].
    Fork {
        /// Optional directive supplied via `/fork <directive>`.
        /// Stashed here so the modal can carry it across the synchronous return path back to `dispatch_fork_resolved` without a global mailbox.
        directive: Option<String>,
        /// Fork point already resolved by the picker or `--at`, carried the same way.
        cut: crate::slash::commands::fork::ForkCut,
    },
    /// Modal opened by `/new` to resolve the worktree question.
    /// On submit, the selected option index is translated into an [`crate::app::actions::Action::NewSessionAnswered`].
    NewSession,
    /// Modal shown when the user hits the credit/rate limit (403).
    /// Options map to upsell URLs (upgrade tier when not max-tier, buy
    /// credits / PAYG) plus "Try Again". `choices` maps each option
    /// index to a telemetry choice variant.
    CreditLimitUpsell {
        choices: Vec<xai_grok_telemetry::events::CreditLimitChoice>,
    },
    /// SuperGrok upsell modal: the free-usage paywall (429 with `subscription:free-usage-exhausted`) or a tier-restricted slash command invocation.
    /// Upgrade options carry their URL in the option `id`.
    FreeUsageUpsell {
        /// Telemetry source for `SuperGrokUpsellClicked`; distinguishes the paywall from the restricted-command upsell.
        source: xai_grok_telemetry::events::SuperGrokUpsell,
    },
    /// Modal shown when the shell rejects a model switch due to agent type incompatibility.
    /// Carries the target model and effort so the answer handler can create a new session with it.
    AgentTypeMismatch {
        model_id: agent_client_protocol::ModelId,
        effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    },
    DoctorFix {
        target: crate::app::actions::DoctorFixTarget,
        plan: Box<crate::diagnostics::FixPlan>,
    },
    DeleteCurrentSession,
    /// Freeform report modal opened by `/feedback`.
    Feedback,
    /// Second stage of the `/feedback` card: trace consent.
    /// Carries the committed report (text and drained image attachments) so Esc can skip the question without dropping it.
    FeedbackTrace {
        report: String,
        images: crate::views::prompt_widget::FeedbackImages,
    },
}

/// Bare `/feedback` pane label (first paragraph of the question chrome).
pub const FEEDBACK_QUESTION_LABEL: &str = "How can we improve Grok Build?";

/// Trace-consent question shown after the report is submitted.
/// The wording comes from legal review: it discloses retention/training scope, not just debugging.
pub const FEEDBACK_TRACE_QUESTION_LABEL: &str = "Opt-in to provide your trace for debugging \
     purposes. This will also provide SpaceXAI the ability to retain and train on coding data, \
     e.g., prompts, traces, & metrics.";

/// Option ids for the trace-consent question; the submit handler maps ids (never positions) back to a [`crate::app::actions::FeedbackTraceChoice`].
pub const FEEDBACK_TRACE_OPTION_OPT_IN: &str = "always_upload";
pub const FEEDBACK_TRACE_OPTION_OPT_OUT: &str = "no_upload";
pub const FEEDBACK_TRACE_OPTION_NEVER_ASK: &str = "never_ask";

// ── State ──────────────────────────────────────────────────────────────

/// Complete state for the question view overlay.
///
/// Created when an `x.ai/ask_user_question` ext-method request arrives;
/// destroyed on submit, skip, or cancel.
///
/// Not `Clone` because it owns a `oneshot::Sender` for the ACP response.
#[derive(Debug)]
pub struct QuestionViewState {
    /// The tool call ID of the `AskUserQuestion` invocation.
    pub tool_call_id: String,
    /// The questions to present.
    pub questions: Vec<Question>,
    /// Which question is currently shown (0-based index).
    pub active_tab: usize,
    /// Per-question selection state (same length as `questions`).
    pub selections: Vec<QuestionSelection>,
    /// Current focus mode.
    pub focus: QuestionFocus,
    /// Whether fullscreen mode is active (removes height cap).
    pub fullscreen: bool,
    /// Whether the card is collapsed to a single summary row so the transcript behind it can be read.
    ///
    /// Minimizing also hands the keyboard to the scrollback while the card keeps waiting, so the
    /// conversation can be scrolled and read without answering first.
    pub minimized: bool,
    /// Original prompt state, stashed on entry and restored on exit.
    pub stashed_prompt: StashedPrompt,

    /// Cursor position per question (index into options plus the freeform row).
    pub per_question_cursor: Vec<usize>,
    /// Scroll offset (visual lines) per question.
    pub per_question_scroll: Vec<u16>,
    /// Per-question freeform text (additional context).
    /// Each question has its own text so switching tabs doesn't mix content.
    pub per_question_freeform: Vec<String>,
    /// Whether the per-question freeform answer is "selected" (included in submission).
    /// Toggled by Space, auto-set when exiting InputMode with text.
    /// Independent of the text content; text is preserved on untoggle.
    pub per_question_freeform_selected: Vec<bool>,
    /// Images pasted into each question's freeform answer, per question.
    ///
    /// Only the *inactive* questions are parked here: the active question's
    /// images live in the composer (the same place its text does) and are saved
    /// on the way out by the freeform swap. Both halves are merged at submit.
    pub per_question_images: Vec<Vec<crate::prompt_images::PastedImage>>,

    // ── Cached chrome caps (recomputed on resize / question switch) ──
    /// Cached cap on description lines in chrome (capped in non-fullscreen).
    pub cached_desc_cap: u16,
    /// Cached cap on preview lines in chrome (capped in non-fullscreen).
    pub cached_preview_cap: u16,

    // ── ACP response channel ──
    /// Stashed ACP response sender.
    /// When the user submits/cancels, the pager serializes the response and sends it here.
    /// `take()` ensures we never send twice.
    pub response_tx:
        Option<tokio::sync::oneshot::Sender<AcpResult<agent_client_protocol::ExtResponse>>>,
    /// Mode context from the ext-method request.
    /// Controls whether the bottom panel (Chat about this / Skip interview) is shown.
    pub mode: AskUserQuestionMode,
    /// Bottom panel selection index (plan mode only).
    /// `None` means the options list has focus; `Some(0)` is Chat about this and `Some(1)` is Skip interview.
    pub bottom_panel_index: Option<usize>,
    /// `Some` when this question was opened locally (e.g. by `/fork`) instead of by an ACP `x.ai/ask_user_question` request.
    /// `None` for ACP questions.
    ///
    /// Mutually exclusive with `response_tx`: a local question never has an ACP sender.
    pub local_kind: Option<LocalQuestionKind>,
    /// When this question view was created. Used to pause the turn timer while the user is answering questions.
    /// The time spent in the question view is subtracted from the turn elapsed display.
    pub opened_at: Instant,
    /// Wall-clock twin of `opened_at` (UTC ms).
    /// `Instant` does not advance across a suspend, so a pause netted against the wall-anchored turn span must itself be measured on the wall clock.
    /// Otherwise a suspend during an open question would read as worked time.
    pub opened_at_wall_ms: i64,
    /// When `true`, the freeform "Other" input row is hidden.
    /// Used by locally-driven questions (e.g. the credit-limit upsell) that only offer fixed options with no free-text fallback.
    pub no_freeform: bool,

    /// Whether Enter on the report advances to the trace-consent question.
    pub feedback_offer_trace: bool,
    /// Opted-out account: the "Opt in" option also switches coding-data
    /// sharing back on (and says so in its description).
    pub feedback_offer_reenables_sharing: bool,
}

// ── Constructor & basic helpers ────────────────────────────────────────

impl QuestionViewState {
    /// Initializes per-question vectors (selections, cursors, scroll) based on each question's type (single vs multi-select).
    pub fn new(
        tool_call_id: String,
        questions: Vec<Question>,
        stashed_prompt: StashedPrompt,
    ) -> Self {
        Self::with_response_tx(
            tool_call_id,
            questions,
            stashed_prompt,
            None,
            AskUserQuestionMode::Default,
        )
    }

    /// Create a new question view state with an ACP response sender.
    ///
    /// Called by the `ExtMethod` handler when a blocking `x.ai/ask_user_question` request arrives from the shell coordinator.
    pub fn with_response_tx(
        tool_call_id: String,
        questions: Vec<Question>,
        stashed_prompt: StashedPrompt,
        response_tx: Option<
            tokio::sync::oneshot::Sender<AcpResult<agent_client_protocol::ExtResponse>>,
        >,
        mode: AskUserQuestionMode,
    ) -> Self {
        let n = questions.len();
        let selections: Vec<QuestionSelection> = questions
            .iter()
            .map(|q| {
                if q.multi_select.unwrap_or(false) {
                    QuestionSelection::Multi(HashSet::new())
                } else {
                    QuestionSelection::Single(None)
                }
            })
            .collect();

        Self {
            tool_call_id,
            questions,
            active_tab: 0,
            selections,
            focus: QuestionFocus::Navigation,
            fullscreen: false,
            minimized: false,
            stashed_prompt,
            per_question_cursor: vec![0; n],
            per_question_scroll: vec![0; n],
            per_question_freeform: vec![String::new(); n],
            per_question_freeform_selected: vec![false; n],
            per_question_images: vec![Vec::new(); n],
            cached_desc_cap: DEFAULT_MAX_CHROME_DESC_LINES,
            cached_preview_cap: DEFAULT_MAX_CHROME_PREVIEW_LINES,
            response_tx,
            mode,
            bottom_panel_index: None,
            local_kind: None,
            opened_at: Instant::now(),
            opened_at_wall_ms: chrono::Utc::now().timestamp_millis(),
            no_freeform: false,
            feedback_offer_trace: false,
            feedback_offer_reenables_sharing: false,
        }
    }

    /// Builder-style helper to attach a [`LocalQuestionKind`].
    ///
    /// Used by `open_fork_question` to mark a freshly-built `QuestionViewState` as locally-driven.
    /// Submit/cancel then routes through the synchronous `Action` path instead of the ACP `response_tx`.
    pub fn with_local_kind(mut self, kind: LocalQuestionKind) -> Self {
        self.local_kind = Some(kind);
        self
    }

    /// Builder-style helper to hide the freeform "Other" input row.
    pub fn with_no_freeform(mut self) -> Self {
        self.no_freeform = true;
        self
    }

    /// Number of items for a given question: options plus 1 free-form row (unless `no_freeform` is set).
    pub fn total_items(&self, question_idx: usize) -> usize {
        let freeform = if self.no_freeform { 0 } else { 1 };
        self.questions
            .get(question_idx)
            .map(|q| q.options.len() + freeform)
            .unwrap_or(1)
    }

    /// Whether the cursor is on the free-form row for the active question.
    pub fn is_on_freeform_row(&self) -> bool {
        if self.no_freeform {
            return false;
        }
        let idx = self.active_tab;
        self.cursor()
            == self
                .questions
                .get(idx)
                .map(|q| q.options.len())
                .unwrap_or(0)
    }

    /// Current cursor position for the active question.
    pub fn cursor(&self) -> usize {
        self.per_question_cursor
            .get(self.active_tab)
            .copied()
            .unwrap_or(0)
    }

    /// Move the cursor within the active question, clamped at both ends.
    pub fn move_cursor(&mut self, motion: CursorMotion) {
        let last = self.total_items(self.active_tab).saturating_sub(1);
        let cursor = self.cursor();
        let target = match motion {
            CursorMotion::Next => cursor + 1,
            CursorMotion::Prev => cursor.saturating_sub(1),
            CursorMotion::HalfPageDown => cursor + (last / 2).max(1),
            CursorMotion::HalfPageUp => cursor.saturating_sub((last.max(1) / 2).max(1)),
            CursorMotion::PageDown => cursor + last.max(1),
            CursorMotion::PageUp => cursor.saturating_sub(last.max(1)),
            CursorMotion::First => 0,
            CursorMotion::Last => last,
        };
        self.set_cursor(target.min(last));
    }

    /// Walk one answer row of the active question, wrapping at both ends.
    pub fn walk_cursor(&mut self, walk: RowWalk) {
        let target = walk.step(self.cursor(), self.total_items(self.active_tab));
        self.set_cursor(target);
    }

    pub fn clear_selection(&mut self, q_idx: usize) {
        match self.selections.get_mut(q_idx) {
            Some(QuestionSelection::Multi(selected)) => selected.clear(),
            Some(QuestionSelection::Single(selected)) => *selected = None,
            None => {}
        }
        if let Some(freeform_selected) = self.per_question_freeform_selected.get_mut(q_idx) {
            *freeform_selected = false;
        }
    }

    /// Set cursor position for the active question, clamped to valid range.
    pub fn set_cursor(&mut self, pos: usize) {
        let max = self.total_items(self.active_tab).saturating_sub(1);
        let clamped = pos.min(max);
        if let Some(c) = self.per_question_cursor.get_mut(self.active_tab) {
            *c = clamped;
        }
    }

    /// Adjust scroll so the cursor row is visible within `visible_h` lines.
    ///
    /// Call this after every cursor change.
    /// `content_w` is needed to compute per-option visual heights for stacked layout.
    pub fn ensure_cursor_visible(&mut self, visible_h: u16, content_w: usize) {
        let q_idx = self.active_tab;
        let Some(question) = self.questions.get(q_idx) else {
            return;
        };

        let cursor = self.cursor();
        let heights = option_heights(question, content_w, cursor);
        let scroll = self.per_question_scroll.get(q_idx).copied().unwrap_or(0);

        // Compute the visual Y range of the cursor item.
        let cursor_top: u16 = heights[..cursor].iter().sum();
        let cursor_bottom = cursor_top + heights.get(cursor).copied().unwrap_or(1);

        let mut new_scroll = scroll;

        // If cursor is above the visible window, scroll up.
        if cursor_top < new_scroll {
            new_scroll = cursor_top;
        }

        // If cursor is below the visible window, scroll down.
        if cursor_bottom > new_scroll + visible_h {
            new_scroll = cursor_bottom.saturating_sub(visible_h);
        }

        if let Some(s) = self.per_question_scroll.get_mut(q_idx) {
            *s = new_scroll;
        }
    }

    /// Clamp the active question's scroll offset to the current viewport.
    pub fn clamp_scroll(&mut self, visible_h: u16, content_w: usize) {
        let q_idx = self.active_tab;
        let Some(question) = self.questions.get(q_idx) else {
            return;
        };

        let max_scroll = total_options_height(question, content_w, self.cursor())
            .saturating_sub(self.phantom_freeform_h())
            .saturating_sub(visible_h);
        if let Some(s) = self.per_question_scroll.get_mut(q_idx) {
            *s = (*s).min(max_scroll);
        }
    }

    /// Height of the freeform rows that [`option_heights`] and [`total_options_height`] always include.
    /// The rows are never rendered when `no_freeform` is set; subtract this from those totals wherever they feed layout or scroll limits.
    pub fn phantom_freeform_h(&self) -> u16 {
        if self.no_freeform {
            FREEFORM_ROW_ROWS + OPTION_ROW_GAP_ROWS
        } else {
            0
        }
    }
}

/// Visual heights for each item in a question: all options, then freeform.
///
/// Every item draws one label row plus one row per wrapped description line, and every item after
/// the first is preceded by [`OPTION_ROW_GAP_ROWS`] blank rows.
pub fn option_heights(question: &Question, content_w: usize, cursor: usize) -> Vec<u16> {
    let prefix_w = option_prefix_w(question);
    let text_w = option_text_w(content_w, prefix_w);
    let max_lw = compute_max_label_w(&question.options, text_w);

    question
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let gap = if i == 0 { 0 } else { OPTION_ROW_GAP_ROWS };
            option_block_height(o, text_w, prefix_w, max_lw) + gap
        })
        .chain(std::iter::once(FREEFORM_ROW_ROWS + OPTION_ROW_GAP_ROWS))
        .collect()
}

/// Visual-line offsets (within the option list) that open a box's top rule.
///
/// The freeform item is included; a caller that draws it sticky accounts for its own offset.
pub fn divider_visual_lines(question: &Question, content_w: usize, cursor: usize) -> Vec<u16> {
    let mut out = Vec::new();
    let mut top = 0u16;
    for (idx, height) in option_heights(question, content_w, cursor)
        .into_iter()
        .enumerate()
    {
        if idx > 0 {
            out.push(top - OPTION_ROW_GAP_ROWS);
        }
        top += height;
    }
    out
}

/// Total visual height of all option rows plus the freeform row.
pub fn total_options_height(question: &Question, content_w: usize, cursor: usize) -> u16 {
    option_heights(question, content_w, cursor)
        .into_iter()
        .sum()
}

/// Map a visual line offset within the scrolled options list to an item index.
pub fn item_index_at_visual_line(
    question: &Question,
    content_w: usize,
    visual_line: u16,
    cursor: usize,
) -> usize {
    let heights = option_heights(question, content_w, cursor);
    let mut top = 0u16;
    for (idx, height) in heights.iter().copied().enumerate() {
        if visual_line < top + height {
            return idx;
        }
        top += height;
    }
    heights.len().saturating_sub(1)
}

pub fn item_top_offset(
    question: &Question,
    content_w: usize,
    item_index: usize,
    cursor: usize,
) -> u16 {
    let heights = option_heights(question, content_w, cursor);
    let clamped = item_index.min(heights.len());
    heights[..clamped].iter().sum()
}

pub fn scroll_offset_for_item_delta(
    question: &Question,
    content_w: usize,
    current_scroll: u16,
    delta: i32,
    viewport_height: u16,
    cursor: usize,
    phantom_freeform_h: u16,
) -> u16 {
    // Line-based scrolling: add delta directly to the scroll offset, clamped to [0, max_scroll]
    // Each visual line (including wrapped description lines) is independent, so we scroll by individual lines instead of jumping whole items
    //
    // For `no_freeform` questions, `phantom_freeform_h` removes the never-rendered freeform line from the scrollable total
    let max_scroll = total_options_height(question, content_w, cursor)
        .saturating_sub(phantom_freeform_h)
        .saturating_sub(viewport_height);
    ((current_scroll as i32) + delta).clamp(0, max_scroll as i32) as u16
}

/// Visible option rows height within a rendered question area.
///
/// The card's closing rule sits at the bottom of the area, so it comes off the viewport as well.
pub fn visible_options_height(
    question: &Question,
    area_height: u16,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    area_height
        .saturating_sub(CARD_BOTTOM_ROWS)
        .saturating_sub(chrome_height(
            question,
            content_w,
            preview,
            fullscreen,
            desc_cap,
            preview_cap,
        ))
}

/// Maximum scroll offset for the option rows in a rendered question area.
#[allow(clippy::too_many_arguments)]
pub fn max_scroll_offset(
    question: &Question,
    area_height: u16,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
    cursor: usize,
) -> u16 {
    total_options_height(question, content_w, cursor).saturating_sub(visible_options_height(
        question,
        area_height,
        content_w,
        preview,
        fullscreen,
        desc_cap,
        preview_cap,
    ))
}

/// Resolve the item index for a screen row within the rendered question area.
#[allow(clippy::too_many_arguments)]
pub fn item_index_at_screen_row(
    question: &Question,
    area: Rect,
    content_w: usize,
    scroll: u16,
    row: u16,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
    cursor: usize,
) -> Option<usize> {
    let options_start_y = area.y
        + chrome_height(
            question,
            content_w,
            preview,
            fullscreen,
            desc_cap,
            preview_cap,
        );
    let options_end_y = card_footer_rule_row(area);
    if row < options_start_y || row >= options_end_y {
        return None;
    }

    let visual_line = (row - options_start_y) + scroll;
    Some(item_index_at_visual_line(
        question,
        content_w,
        visual_line,
        cursor,
    ))
}

// ── Layout helpers ─────────────────────────────────────────────────────

/// Compute the aligned label column width.
///
/// The column fits the longest label, capped at 60% of the available width.
/// Labels stay visible while the collapsed description (with its `…` affordance) keeps the remaining space.
/// Labels longer than the cap are truncated with `…` on unfocused rows and get stacked/wrapped layout when focused.
pub fn compute_max_label_w(options: &[QuestionOption], content_w: usize) -> usize {
    let cap = content_w * 3 / 5;
    options
        .iter()
        .map(|o| normalize_label(&o.label).width())
        .max()
        .unwrap_or(0)
        .min(cap)
}

/// Visual rows one option occupies: its label row plus every wrapped description line.
///
/// The label always gets its own row and descriptions always stack underneath it, so a row is
/// never truncated horizontally and the same height holds whether or not the card has focus.
pub fn option_block_height(
    option: &QuestionOption,
    content_w: usize,
    prefix_w: usize,
    _max_label_w: usize,
) -> u16 {
    let text_w = content_w.max(1);
    let label_lines = wrap_label_chunks(&normalize_label(&option.label), text_w).len() as u16;
    let desc_lines = rendered_option_description_lines(option, text_w).len() as u16;
    (label_lines + desc_lines).max(1)
}

/// Visual height of a single option row.
///
/// Kept for callers that want the row alone; it excludes the blank row that separates it from the
/// option above.
pub fn option_visual_height(
    option: &QuestionOption,
    content_w: usize,
    prefix_w: usize,
    max_label_w: usize,
) -> u16 {
    option_block_height(option, content_w, prefix_w, max_label_w)
}

/// Inner chrome-height computation with explicit description/preview caps.
///
/// Same logic as [`chrome_height`] but accepts caps as parameters instead of branching on `fullscreen`.
/// Used by the dynamic-cap fallback in [`question_view_height`].
fn chrome_height_with_dynamic_caps(
    question: &Question,
    content_w: usize,
    preview: Option<&str>,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    let wrap_w = content_w.max(1);
    let (label, desc) = split_question_label_desc(&question.question);
    let raw_line = Line::from(vec![Span::raw(label.to_string())]);
    let label_lines = crate::render::wrapping::word_wrap_line(&raw_line, wrap_w)
        .len()
        .max(1) as u16;
    let desc_lines = if desc.is_empty() {
        0u16
    } else {
        rendered_option_description_lines(
            &QuestionOption {
                label: String::new(),
                description: desc.to_string(),
                preview: None,
                id: None,
            },
            wrap_w,
        )
        .len() as u16
    };
    let preview_lines: u16 = match preview {
        Some(p) if !p.is_empty() => p
            .lines()
            .map(|l| {
                let raw = Line::from(vec![Span::raw(l.to_string())]);
                crate::render::wrapping::word_wrap_line(&raw, wrap_w)
                    .len()
                    .max(1) as u16
            })
            .sum(),
        _ => 0,
    };

    let desc_lines = desc_lines.min(desc_cap);
    let preview_lines = preview_lines.min(preview_cap);

    let preview_gap = if preview_lines > 0 { 1 } else { 0 };
    // The label sits directly on the description or the preview when there is one. With neither,
    // the only separator left is the single blank row the option list already opens with, so a
    // second one would leave a short question floating in two blank rows.
    let label_gap = if desc_lines > 0 || preview_lines > 0 { 1 } else { 0 };

    // vpad(1) + card header (top rule + title row + rule) + label + label_gap(1) + description (if any)
    //   + [preview_gap(1) + preview_lines if preview exists] + gap(1)
    // The card's closing rule is not chrome: it sits after the option rows and is counted by `question_view_height`.
    1 + CARD_HEADER_ROWS + label_lines + label_gap + desc_lines + preview_gap + preview_lines + 1
}
/// How many options still sit below the fold.
///
/// The option list is the only part of the card that can be cut, so this is what the card has to
/// admit: a list that silently stops short of the last option reads as "these are all the
/// choices", and the user cannot tell a hidden answer from a missing one. The freeform row is
/// pinned above the footer and never scrolls, so it is not counted.
pub fn hidden_items_below(
    question: &Question,
    content_w: usize,
    cursor: usize,
    scroll: u16,
    visible_h: u16,
) -> usize {
    if visible_h == 0 {
        return 0;
    }
    let heights = option_heights(question, content_w, cursor);
    let fold = scroll as usize + visible_h as usize;
    let mut top = 0usize;
    let mut hidden = 0usize;
    for height in heights.iter().take(question.options.len()).copied() {
        if top + height as usize > fold {
            hidden += 1;
        }
        top += height as usize;
    }
    hidden
}

/// Chrome height for a question: vpad + label lines + gap + [description lines] + gap.
///
/// The label (first paragraph of the question text) word-wraps across multiple lines.
/// If the question contains a paragraph break (`\n\n`), the remaining text is rendered as a description below the label.
/// Must match `render_question_chrome`.
///
/// When `fullscreen` is false, description and preview lines are capped to `desc_cap` / `preview_cap` (with room for a truncation indicator).
/// When `fullscreen` is true the caps are ignored and all lines are counted.
pub fn chrome_height(
    question: &Question,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    if fullscreen {
        chrome_height_with_dynamic_caps(question, content_w, preview, u16::MAX, u16::MAX)
    } else {
        chrome_height_with_dynamic_caps(question, content_w, preview, desc_cap, preview_cap)
    }
}

/// Split question text into a label (first paragraph) and description (rest).
///
/// A paragraph break is `\n\n`.
/// If no break exists, the full text is the label and the description is empty.
fn split_question_label_desc(text: &str) -> (&str, &str) {
    if let Some(pos) = text.find("\n\n") {
        (text[..pos].trim(), text[pos + 2..].trim())
    } else {
        (text.trim(), "")
    }
}

// ── Selection helpers ──────────────────────────────────────────────────

impl QuestionViewState {
    /// Toggle an option for a question.
    ///
    /// - Multi: toggle in/out of the HashSet.
    /// - Single: set to `Some(option_idx)`, or `None` if already selected (deselect).
    pub fn toggle_option(&mut self, question_idx: usize, option_idx: usize) {
        let Some(sel) = self.selections.get_mut(question_idx) else {
            return;
        };
        match sel {
            QuestionSelection::Multi(set) => {
                if !set.remove(&option_idx) {
                    set.insert(option_idx);
                }
            }
            QuestionSelection::Single(current) => {
                if *current == Some(option_idx) {
                    *current = None;
                } else {
                    *current = Some(option_idx);
                }
            }
        }
    }

    /// Select an option (no toggle; always selects).
    ///
    /// - Single: set to `Some(option_idx)`.
    /// - Multi: add to set.
    pub fn select_option(&mut self, question_idx: usize, option_idx: usize) {
        let Some(sel) = self.selections.get_mut(question_idx) else {
            return;
        };
        match sel {
            QuestionSelection::Multi(set) => {
                set.insert(option_idx);
            }
            QuestionSelection::Single(current) => {
                *current = Some(option_idx);
            }
        }
    }

    /// Activate freeform input for the active question.
    ///
    /// Marks the freeform row as selected, clears the option selection (single-select exclusivity), and sets focus to `InputMode`.
    /// Returns the current freeform text so the caller can load it into the prompt.
    ///
    /// No-op returning an empty string when `no_freeform` is set.
    /// Such questions (e.g. the SuperGrok upsell) have no freeform row, so `InputMode` must be unreachable.
    /// Callers gate on `no_freeform` / [`Self::is_on_freeform_row`] too; this is defense in depth.
    pub fn activate_freeform_input(&mut self) -> String {
        if self.no_freeform {
            return String::new();
        }
        let idx = self.active_tab;
        if let Some(sel) = self.per_question_freeform_selected.get_mut(idx) {
            *sel = true;
        }
        if let Some(QuestionSelection::Single(sel)) = self.selections.get_mut(idx) {
            *sel = None;
        }
        self.focus = QuestionFocus::InputMode;
        self.per_question_freeform
            .get(idx)
            .cloned()
            .unwrap_or_default()
    }

    /// Images parked for a question whose freeform answer is not the one being edited.
    pub fn parked_images(&self, question_idx: usize) -> &[crate::prompt_images::PastedImage] {
        self.per_question_images
            .get(question_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether an answer to this card can carry pasted images.
    ///
    /// ACP questions ship them in the `accepted` response and the `/feedback`
    /// card ships them with `SendFeedback`; every other local question answers
    /// through an `Action` with no image channel, so pasting there stays
    /// text-only instead of attaching a chip the submit would silently drop.
    pub fn accepts_answer_images(&self) -> bool {
        self.local_kind.is_none() || self.is_feedback()
    }

    /// Park `images` as the answer attachments of `question_idx`, replacing whatever was there.
    pub fn set_parked_images(
        &mut self,
        question_idx: usize,
        images: Vec<crate::prompt_images::PastedImage>,
    ) {
        if let Some(slot) = self.per_question_images.get_mut(question_idx) {
            crate::prompt_images::drain_and_cleanup(slot);
            *slot = images;
        }
    }

    /// Preview text for the currently focused option, if any.
    ///
    /// Returns `Some(preview)` when the cursor is on an option (not freeform)
    /// and that option has a `preview` field set.
    pub fn focused_preview(&self) -> Option<&str> {
        let q = self.questions.get(self.active_tab)?;
        let cursor = self.cursor();
        let option = q.options.get(cursor)?;
        option.preview.as_deref()
    }

    /// Either stage of the `/feedback` card (report or trace consent).
    pub fn is_feedback(&self) -> bool {
        matches!(
            self.local_kind,
            Some(LocalQuestionKind::Feedback | LocalQuestionKind::FeedbackTrace { .. })
        )
    }

    /// The freeform report stage of the `/feedback` card.
    pub fn is_feedback_report(&self) -> bool {
        matches!(self.local_kind, Some(LocalQuestionKind::Feedback))
    }

    /// The hard-modal blocked-prompt card: dismissal is refused, so the footer must not advertise it.
    pub fn is_prompt_blocked(&self) -> bool {
        matches!(
            self.local_kind,
            Some(LocalQuestionKind::PromptBlocked { .. })
        )
    }

    /// The trace-consent stage of the `/feedback` card.
    pub fn is_feedback_trace(&self) -> bool {
        matches!(
            self.local_kind,
            Some(LocalQuestionKind::FeedbackTrace { .. })
        )
    }

    pub fn feedback_report(&self) -> String {
        self.per_question_freeform
            .first()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// Swap the report card for the trace-consent question, keeping the stashed prompt.
    /// Built through the constructor so the per-question vector-length invariant lives in exactly one place.
    pub fn begin_feedback_trace_stage(
        &mut self,
        report: String,
        images: Vec<crate::prompt_images::PastedImage>,
    ) {
        self.begin_feedback_trace_stage_with_locale(report, images, None);
    }

    pub fn begin_feedback_trace_stage_with_locale(
        &mut self,
        report: String,
        images: Vec<crate::prompt_images::PastedImage>,
        locale: Option<&crate::locale::LocaleContext>,
    ) {
        let text = |id: &str, english: &str| {
            locale
                .map(|locale| locale.named_text(id, english).into_owned())
                .unwrap_or_else(|| english.to_owned())
        };
        // "Opt in" is a persistent grant, so its description names what it
        // turns on beyond this one upload.
        let opt_in_description = if self.feedback_offer_reenables_sharing {
            text(
                "feedback.trace.opt_in.description.persisted",
                "Turns on trace upload for future sessions on this machine and switches coding \
                 data sharing back on for this account.",
            )
        } else {
            text(
                "feedback.trace.opt_in.description.reenable",
                "Turns on trace upload for future sessions on this machine (change any time with \
                 [telemetry] trace_upload in config.toml).",
            )
        };
        let question = Question {
            question: text("feedback.trace.question", FEEDBACK_TRACE_QUESTION_LABEL),
            options: vec![
                QuestionOption {
                    label: text("feedback.trace.opt_in", "Opt in"),
                    description: opt_in_description,
                    preview: None,
                    id: Some(FEEDBACK_TRACE_OPTION_OPT_IN.into()),
                },
                QuestionOption {
                    label: text("feedback.trace.opt_out_once", "Opt out this time"),
                    description: String::new(),
                    preview: None,
                    id: Some(FEEDBACK_TRACE_OPTION_OPT_OUT.into()),
                },
                QuestionOption {
                    label: text("feedback.trace.never_ask", "Opt out and don't ask again"),
                    description: String::new(),
                    preview: None,
                    id: Some(FEEDBACK_TRACE_OPTION_NEVER_ASK.into()),
                },
            ],
            multi_select: Some(false),
            id: None,
        };
        let mut next = QuestionViewState::new(
            std::mem::take(&mut self.tool_call_id),
            vec![question],
            std::mem::take(&mut self.stashed_prompt),
        );
        next.selections = vec![QuestionSelection::Single(Some(0))];
        next.no_freeform = true;
        next.fullscreen = self.fullscreen;
        // Card-open time spans both stages (pause accounting).
        next.opened_at = self.opened_at;
        next.opened_at_wall_ms = self.opened_at_wall_ms;
        next.feedback_offer_trace = self.feedback_offer_trace;
        next.feedback_offer_reenables_sharing = self.feedback_offer_reenables_sharing;
        next.local_kind = Some(LocalQuestionKind::FeedbackTrace {
            report,
            images: images.into(),
        });
        *self = next;
    }

    /// Labels of the selected options for a given question.
    pub fn selected_labels(&self, question_idx: usize) -> Vec<String> {
        let Some(sel) = self.selections.get(question_idx) else {
            return Vec::new();
        };
        let Some(q) = self.questions.get(question_idx) else {
            return Vec::new();
        };
        match sel {
            QuestionSelection::Multi(set) => {
                let mut indices: Vec<_> = set.iter().copied().collect();
                indices.sort_unstable();
                indices
                    .into_iter()
                    .filter_map(|i| q.options.get(i).map(|o| o.label.clone()))
                    .collect()
            }
            QuestionSelection::Single(Some(idx)) => q
                .options
                .get(*idx)
                .map(|o| vec![o.label.clone()])
                .unwrap_or_default(),
            QuestionSelection::Single(None) => Vec::new(),
        }
    }

    /// True when the active tab has any option selected, or its free-form answer marked selected.
    pub fn active_tab_has_selection(&self) -> bool {
        let q_idx = self.active_tab;
        let option_selected = !self.selected_labels(q_idx).is_empty();
        let freeform_selected = self
            .per_question_freeform_selected
            .get(q_idx)
            .copied()
            .unwrap_or(false);
        option_selected || freeform_selected
    }
}

// ── ACP response builders ──────────────────────────────────────────────

impl QuestionViewState {
    /// Build the `Accepted` ext-method response from the current state.
    ///
    /// Rules:
    /// - Only answered questions appear in `answers` (unanswered omitted).
    /// - Multi-select: labels joined with `, `.
    /// - Freeform-only (no option, only typed text): the label is `"Other"` and the typed text goes in `annotations[q].notes`.
    /// - Preview included for single-select only, verbatim from the option.
    /// - Notes included when freeform text is non-empty and selected.
    /// - Images the user pasted into an answer ride `annotations[q].images`, and an
    ///   image-only answer counts as answered on its own.
    pub fn build_accepted_response(
        &self,
    ) -> xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionExtResponse
    {
        self.build_accepted_response_with_images(Vec::new())
    }

    /// [`Self::build_accepted_response`] with the composer's live images for the
    /// active question, whose chips the caller has not parked in
    /// [`Self::per_question_images`] yet.
    ///
    /// Encoding goes through the same helper the composer uses for its own
    /// attachments, so the two paths cannot drift.
    pub fn build_accepted_response_with_images(
        &self,
        active_question_images: Vec<crate::prompt_images::PastedImage>,
    ) -> xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionExtResponse
    {
        use indexmap::IndexMap;
        use std::collections::HashMap;
        use xai_grok_tools::implementations::grok_build::ask_user_question::{
            AskUserQuestionExtResponse, QuestionAnnotation,
        };

        let mut answers = IndexMap::new();
        let mut annotations: HashMap<String, QuestionAnnotation> = HashMap::new();

        for (i, q) in self.questions.iter().enumerate() {
            let labels = self.selected_labels(i);
            let freeform_selected = self
                .per_question_freeform_selected
                .get(i)
                .copied()
                .unwrap_or(false);
            let freeform_text = self
                .per_question_freeform
                .get(i)
                .cloned()
                .unwrap_or_default();
            let images = self.answer_images_for(i, &active_question_images);
            let has_text = freeform_selected && !freeform_text.trim().is_empty();
            let has_images = freeform_selected && !images.is_empty();
            let has_freeform = has_text || has_images;

            if labels.is_empty() && !has_freeform {
                // Unanswered, so omit from answers
                continue;
            }

            // Build the per-question label vec: one element for single-select, multiple for multi-select, or `["Other"]` for freeform-only
            // The wire format carries these as separate elements so downstream cursor-shape resolvers do not have to re-split a comma-joined string
            let label_vec: Vec<String> = if labels.is_empty() && has_freeform {
                vec!["Other".to_string()]
            } else {
                labels
            };

            answers.insert(q.question.clone(), label_vec);

            // Build annotation if there's preview, notes, or attached images.
            let is_single = !q.multi_select.unwrap_or(false);
            let preview = if is_single {
                // Preview from selected option (single-select only).
                match &self.selections[i] {
                    QuestionSelection::Single(Some(idx)) => {
                        q.options.get(*idx).and_then(|o| o.preview.clone())
                    }
                    _ => None,
                }
            } else {
                None
            };

            let notes = if has_text { Some(freeform_text) } else { None };
            let images = if has_images { Some(images) } else { None };

            if preview.is_some() || notes.is_some() || images.is_some() {
                annotations.insert(
                    q.question.clone(),
                    QuestionAnnotation {
                        preview,
                        notes,
                        images,
                    },
                );
            }
        }

        let annotations = if annotations.is_empty() {
            None
        } else {
            Some(annotations)
        };

        AskUserQuestionExtResponse::Accepted {
            answers,
            annotations,
        }
    }

    /// Base64 payloads for the answer to question `question_idx`: the composer's
    /// live set for the active question, the parked set for every other one.
    ///
    /// A caller that already parked the active question's images (the submit
    /// path swaps the freeform draft first) passes an empty live set, so the
    /// parked set is the fallback rather than a second source of truth.
    fn answer_images_for(
        &self,
        question_idx: usize,
        active_question_images: &[crate::prompt_images::PastedImage],
    ) -> Vec<QuestionAnswerImage> {
        let images: Vec<crate::prompt_images::PastedImage> =
            if question_idx == self.active_tab && !active_question_images.is_empty() {
                active_question_images.to_vec()
            } else {
                self.per_question_images
                    .get(question_idx)
                    .cloned()
                    .unwrap_or_default()
            };
        if images.is_empty() {
            return Vec::new();
        }
        crate::prompt_images::build_content_blocks_with_prefixes(String::new(), images, None)
            .into_iter()
            .filter_map(|block| match block {
                agent_client_protocol::ContentBlock::Image(image) => Some(QuestionAnswerImage {
                    mime_type: image.mime_type,
                    data: image.data,
                }),
                _ => None,
            })
            .collect()
    }

    /// Send the ACP ext-method response and return `true` if the response was actually sent (i.e. `response_tx` was present).
    ///
    /// After sending, `response_tx` is consumed (set to `None`) to prevent double-send.
    pub fn send_ext_response(
        &mut self,
        response: xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionExtResponse,
    ) -> bool {
        let Some(tx) = self.response_tx.take() else {
            return false;
        };
        let raw = serde_json::value::to_raw_value(&response)
            .expect("AskUserQuestionExtResponse serialization should not fail");
        tx.send(Ok(agent_client_protocol::ExtResponse::new(raw.into())))
            .ok();
        true
    }
}

// ── Tab cycling ────────────────────────────────────────────────────────

impl QuestionViewState {
    /// Advance to the next question (clamped, no wrap).
    pub fn next_question(&mut self) {
        if self.active_tab + 1 < self.questions.len() {
            self.active_tab += 1;
        }
    }

    /// Go to the previous question (clamped, no wrap).
    pub fn prev_question(&mut self) {
        self.active_tab = self.active_tab.saturating_sub(1);
    }
}

// ── Rendering ──────────────────────────────────────────────────────────

/// Desired height for the question view overlay.
///
/// Height cap: 33% of `screen_h`, clamped to min 8, max 80%.
/// Fullscreen mode removes the cap.
///
/// The description and preview caps shrink dynamically so at least [`MIN_VISIBLE_OPTION_ROWS`] option rows stay visible under the chrome.
/// The guarantee holds when the terminal is large enough for the fixed chrome overhead.
/// On extremely small terminals this is best-effort: it may not hold when even zero desc/preview lines cannot free enough space.
/// The effective caps are written to `state.cached_desc_cap` / `state.cached_preview_cap` so the renderer uses matching values.
pub fn question_view_height(state: &mut QuestionViewState, screen_h: u16, content_w: usize) -> u16 {
    let q_idx = state.active_tab;
    let Some(question) = state.questions.get(q_idx) else {
        return 0;
    };

    // A minimized card is one summary row; the transcript behind it keeps the rest of the screen.
    if state.minimized {
        state.cached_desc_cap = DEFAULT_MAX_CHROME_DESC_LINES;
        state.cached_preview_cap = DEFAULT_MAX_CHROME_PREVIEW_LINES;
        return if screen_h == 0 { 0 } else { 1 };
    }

    // `total_options_height` unconditionally counts the freeform rows
    // When `no_freeform` is set they are never rendered, so subtract them from the totals below
    // Otherwise the panel keeps clickable dead rows under the last option
    let phantom_freeform = state.phantom_freeform_h();
    let freeform_h: u16 = FREEFORM_ROW_ROWS.saturating_sub(phantom_freeform);
    let min_options_space = MIN_VISIBLE_OPTION_ROWS + freeform_h;

    if state.fullscreen {
        // desc_cap/preview_cap are ignored when fullscreen=true (chrome_height routes to u16::MAX internally), but pass MAX for clarity
        let chrome_h = chrome_height(
            question,
            content_w,
            state.focused_preview(),
            true,
            u16::MAX,
            u16::MAX,
        );
        let total = chrome_h
            + total_options_height(question, content_w, state.cursor())
                .saturating_sub(phantom_freeform)
            + CARD_BOTTOM_ROWS;
        state.cached_desc_cap = u16::MAX;
        state.cached_preview_cap = u16::MAX;
        return total.min(screen_h);
    }

    // Fixed overhead: vpad + card header + label + blank + gap + the card's closing rule.
    let (label, desc) = split_question_label_desc(&question.question);
    let raw_line = Line::from(vec![Span::raw(label.to_string())]);
    let label_lines = crate::render::wrapping::word_wrap_line(&raw_line, content_w.max(1))
        .len()
        .max(1) as u16;
    let fixed_overhead = 1 + CARD_HEADER_ROWS + label_lines + 1 + 1 + CARD_BOTTOM_ROWS;

    // The card floats over the transcript, so it may take most of the panel: the user's first need
    // is to read every option, and an option hidden below the fold cannot be chosen. Two rows are
    // held back so the card still reads as floating, and the floor guarantees the option list is
    // never starved by the card's own chrome.
    let floor = (fixed_overhead as u32 + min_options_space as u32).min(screen_h as u32) as u16;
    let room = screen_h.saturating_sub(2);
    let cap = room.max(floor).min(screen_h);

    let mut effective_desc_cap = DEFAULT_MAX_CHROME_DESC_LINES;
    let mut effective_preview_cap = DEFAULT_MAX_CHROME_PREVIEW_LINES;

    let mut chrome_h = chrome_height_with_dynamic_caps(
        question,
        content_w,
        state.focused_preview(),
        effective_desc_cap,
        effective_preview_cap,
    );

    if chrome_h + min_options_space > cap {
        // Compute actual description line count so unused desc budget can be reallocated to preview instead of being wasted
        let actual_desc_lines = if desc.is_empty() {
            0u16
        } else {
            rendered_option_description_lines(
                &QuestionOption {
                    label: String::new(),
                    description: desc.to_string(),
                    preview: None,
                    id: None,
                },
                content_w.max(1),
            )
            .len() as u16
        };

        let content_budget = cap
            .saturating_sub(fixed_overhead)
            .saturating_sub(min_options_space);
        effective_desc_cap = content_budget
            .min(DEFAULT_MAX_CHROME_DESC_LINES)
            .min(actual_desc_lines);
        let remaining = content_budget.saturating_sub(effective_desc_cap);
        // Reserve 1 row for the preview gap (blank separator) when preview text exists and there is any remaining budget for preview lines
        let preview_gap_allowance = if state.focused_preview().is_some() && remaining > 0 {
            1u16
        } else {
            0
        };
        effective_preview_cap = remaining
            .saturating_sub(preview_gap_allowance)
            .min(DEFAULT_MAX_CHROME_PREVIEW_LINES);

        // Recompute chrome with reduced caps.
        chrome_h = chrome_height_with_dynamic_caps(
            question,
            content_w,
            state.focused_preview(),
            effective_desc_cap,
            effective_preview_cap,
        );
    }

    state.cached_desc_cap = effective_desc_cap;
    state.cached_preview_cap = effective_preview_cap;

    let total = chrome_h
        + total_options_height(question, content_w, state.cursor())
            .saturating_sub(phantom_freeform)
        + CARD_BOTTOM_ROWS;
    total.min(cap)
}

/// Shortcut label for an option index: 1-9 then a-z.
///
/// Returns `'1'`..`'9'` for indices 0..8, `'a'`..`'z'` for 9..34.
/// Returns `None` for indices 35 and above.
pub fn option_shortcut_label(idx: usize) -> Option<char> {
    match idx {
        0..=8 => Some((b'1' + idx as u8) as char),
        9..=34 => Some((b'a' + (idx - 9) as u8) as char),
        _ => None,
    }
}

/// Map a pressed key character to an option index.
///
/// Maps `'1'`..`'9'` to 0..8 and `'a'`..`'f'` to 9..14.
/// Only a-f are mapped as shortcuts to avoid conflicts with navigation keys
/// (g=top, h=prev-question, j=down, k=up, l=next-question, n=next, s=skip).
pub fn option_index_for_key(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some((c as usize) - ('1' as usize)),
        'a'..='f' => Some(9 + (c as usize) - ('a' as usize)),
        _ => None,
    }
}

/// Columns the panel spends outside the card's inner text column:
/// rail(1) + gap(1) + card border(1) + inner pad(1) on the left,
/// inner pad(1) + card border(1) + gap(1) + scrollbar(1) on the right.
pub const QUESTION_VIEW_HPAD: u16 = 8;

/// Left inset of the card's inner text column inside the panel area (`rail + gap + border + pad`).
pub const QUESTION_VIEW_CONTENT_X: u16 = 4;

/// Rows the card spends above the question label: the top rule, the title row, and the rule under it.
pub const CARD_HEADER_ROWS: u16 = 3;

/// Rows the card spends below the option rows: the footer rule, the button row, the key-hint row,
/// and the card's bottom rule.
pub const CARD_BOTTOM_ROWS: u16 = 4;

/// The button row inside [`CARD_BOTTOM_ROWS`], as an offset above the card's bottom rule.
pub const CARD_BUTTON_ROW_ABOVE_BOTTOM: u16 = 2;

/// The key-hint row inside [`CARD_BOTTOM_ROWS`], just above the card's bottom rule.
pub const CARD_HINT_ROW_ABOVE_BOTTOM: u16 = 1;

/// Blank rows separating two option blocks: option → option, and last option → freeform.
pub const OPTION_ROW_GAP_ROWS: u16 = 1;

/// Text width available to an option once its `❯ N (●) ` prefix is placed, given the card's inner width.
pub fn option_text_w(content_w: usize, prefix_w: usize) -> usize {
    content_w.saturating_sub(prefix_w).max(1)
}

/// Rows the freeform row costs when it is drawn: one text row.
pub const FREEFORM_ROW_ROWS: u16 = 1;

/// Rows the sticky freeform row occupies including the gap that separates it from the options above.
pub const STICKY_FREEFORM_ROWS: u16 = FREEFORM_ROW_ROWS + OPTION_ROW_GAP_ROWS;

/// Prefix width for option rows.
///
/// Every row leads with the cursor arrow so the keyboard position stays readable even when the
/// card is not focused, then the shortcut column (1 character, 1-9 / a-z), the selection marker,
/// and a gap: `❯ 1 (●) ` = 2 + 1 + 1 + 3 + 1 = 8.
pub fn option_prefix_w(_question: &Question) -> usize {
    8
}

/// Report area of the bare `/feedback` card: a multi-line box standing in for the option rows, shared by the full TUI and minimal renderers.
/// `draw` needs a blank [`crate::views::prompt_widget::PromptInfo`] to put the bottom rule in place.
pub mod feedback_input {
    use super::{PromptBg, PromptStyle, QUESTION_VIEW_HPAD, Theme};

    /// Rows at rest: top rule, five text rows, bottom rule. The box grows with the report up to the caller's cap.
    pub const HEIGHT: u16 = 7;

    /// Rows of that height spent on the outline rather than text.
    pub const CHROME_H: u16 = 2;

    /// Smallest box that can still carry its outline: the two rules plus one row of text. Below this the renderers drop to [`flat_style`].
    pub const MIN_HEIGHT: u16 = CHROME_H + 1;

    /// Shown while the box is empty, including while it has focus.
    pub const PLACEHOLDER: &str = "Please provide as much detail as possible.";

    /// The card's content column, so the box lines up under the label.
    pub fn width(area_width: u16) -> u16 {
        area_width.saturating_sub(QUESTION_VIEW_HPAD)
    }

    pub fn style(theme: &Theme) -> PromptStyle {
        style_with_locale(theme, None)
    }

    pub fn style_with_locale(
        theme: &Theme,
        locale: Option<&crate::locale::LocaleContext>,
    ) -> PromptStyle {
        PromptStyle {
            // Sits on the card, so it takes the card's surface rather than the composer's, and pads symmetrically inside its own rules.
            bg: PromptBg::Panel(theme.bg_light),
            chrome_pad_right: 2,
            placeholder_when_focused: true,
            placeholder_override: Some(
                locale
                    .map(|locale| {
                        locale.named_static_text("feedback.placeholder.detailed", PLACEHOLDER)
                    })
                    .unwrap_or(PLACEHOLDER),
            ),
            ..PromptStyle::default()
        }
    }

    /// Unoutlined variant for a panel too short to spare the two rows the rules cost.
    pub fn flat_style(theme: &Theme) -> PromptStyle {
        flat_style_with_locale(theme, None)
    }

    pub fn flat_style_with_locale(
        theme: &Theme,
        locale: Option<&crate::locale::LocaleContext>,
    ) -> PromptStyle {
        PromptStyle {
            vpad_top: 0,
            chrome: false,
            show_borders: false,
            ..style_with_locale(theme, locale)
        }
    }
}

/// Width available for inline prompt text given the full area width.
///
/// Subtracts left padding (accent col + 2 = 3), the option prefix
/// (`"z [x] "` = 6 chars), and the prompt indicator (`"❯ "` = 2 chars).
/// Matches the `text_w` computed during rendering so `desired_height`
/// wraps at the same width as the draw area.
pub fn inline_text_width(area_width: u16) -> u16 {
    const LEFT_PAD: u16 = 3; // accent column + 2 padding
    const OPTION_PREFIX_W: u16 = 6; // shortcut + marker ("z [x] ")
    const PROMPT_INDICATOR_W: u16 = 2; // "❯ "
    area_width.saturating_sub(LEFT_PAD + OPTION_PREFIX_W + PROMPT_INDICATOR_W)
}

/// Normalize a label for single-line display: replace newlines with spaces.
pub(crate) fn normalize_label(label: &str) -> String {
    label.replace('\n', " ").replace("  ", " ")
}

fn rendered_option_description_lines(option: &QuestionOption, width: usize) -> Vec<Line<'static>> {
    if option.description.trim().is_empty() {
        return Vec::new();
    }

    let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
    renderer.push(&option.description);
    // finish() (not render()) so the LaTeX-delimiter normalizer flushes any
    // trailing held-back bytes for this complete, one-shot description.
    renderer.finish(Some(get_syntect()));
    let view = renderer.view();
    let lines_owned: Vec<Line<'static>> = view
        .lines
        .iter()
        .map(crate::render::line_utils::line_to_static)
        .collect();
    let (wrapped, _) = word_wrap_lines_with_joiners(lines_owned, width.max(1));

    let mut compact = Vec::new();
    let mut prev_blank = true;
    for line in wrapped {
        let is_blank = line.spans.iter().all(|s| s.content.trim().is_empty());
        if is_blank {
            if prev_blank {
                continue;
            }
            prev_blank = true;
            continue;
        }
        prev_blank = false;
        compact.push(line);
    }
    compact
}

fn styled_description_lines(
    option: &QuestionOption,
    width: usize,
    row_bg: ratatui::style::Color,
    desc_fg: ratatui::style::Color,
) -> Vec<Line<'static>> {
    rendered_option_description_lines(option, width)
        .into_iter()
        .map(|mut line| {
            line.style = line.style.patch(Style::default().fg(desc_fg).bg(row_bg));
            for span in &mut line.spans {
                span.style = span.style.patch(Style::default().fg(desc_fg).bg(row_bg));
            }
            line
        })
        .collect()
}

/// Build a flat list of styled lines for all option rows and the freeform row.
///
/// Each visual line (including wrapped description continuation lines) is a separate `Line<'static>`.
/// The caller can render a scrolled window by slicing `[scroll .. scroll + visible_h]`, which makes scrolling smooth and line-granular.
#[allow(clippy::too_many_arguments)]
pub fn build_flat_option_lines(
    question: &Question,
    content_w: usize,
    cursor: usize,
    hovered: Option<usize>,
    selections: &QuestionSelection,
    theme: &Theme,
    show_freeform: bool,
    freeform_text: &str,
    freeform_selected: bool,
    panel_focused: bool,
) -> Vec<Line<'static>> {
    build_flat_option_lines_with_placeholder(
        question,
        content_w,
        cursor,
        hovered,
        selections,
        theme,
        show_freeform,
        freeform_text,
        freeform_selected,
        panel_focused,
        "Type your answer here",
    )
}

#[allow(clippy::too_many_arguments)]
fn build_flat_option_lines_with_placeholder(
    question: &Question,
    content_w: usize,
    cursor: usize,
    hovered: Option<usize>,
    selections: &QuestionSelection,
    theme: &Theme,
    show_freeform: bool,
    freeform_text: &str,
    freeform_selected: bool,
    panel_focused: bool,
    freeform_placeholder: &str,
) -> Vec<Line<'static>> {
    let prefix_w = option_prefix_w(question);
    let is_multi = question.multi_select.unwrap_or(false);
    // Rows run the card's full inner width, so text wraps one step narrower than the card itself.
    let text_w = option_text_w(content_w, prefix_w);
    let max_lw = compute_max_label_w(&question.options, text_w);

    let mut all_lines = Vec::new();

    let hover_bg = hovered_bg(theme);

    for (i, option) in question.options.iter().enumerate() {
        let is_cursor_item = i == cursor;
        let is_hovered_item = hovered == Some(i);
        let is_selected = match selections {
            QuestionSelection::Multi(set) => set.contains(&i),
            QuestionSelection::Single(sel) => *sel == Some(i),
        };
        let embed =
            crate::views::modal_window::embedded_row_style(theme, is_cursor_item && panel_focused);
        // The keyboard cursor keeps a filled row background while the card owns focus. When focus
        // leaves the card the arrow in `build_single_option_lines` still marks the row, so the
        // position never disappears; the fill is dropped so the card reads as inactive.
        let row_bg = match embed {
            Some(e) => e.bg,
            None if is_cursor_item && panel_focused => theme.bg_visual,
            None if is_hovered_item => hover_bg,
            None => theme.bg_light,
        };

        // Every block after the first is separated from the one above by a blank row.
        if i > 0 {
            for _ in 0..OPTION_ROW_GAP_ROWS {
                all_lines.push(card_gap_line(content_w, theme));
            }
        }

        all_lines.extend(build_single_option_lines(
            i,
            option,
            is_multi,
            is_selected,
            max_lw,
            prefix_w,
            text_w,
            row_bg,
            embed,
            theme,
            is_cursor_item,
        ));
    }

    // The freeform row is hidden in InputMode (the prompt widget below replaces it)
    if show_freeform {
        let freeform_idx = question.options.len();
        if freeform_idx > 0 {
            for _ in 0..OPTION_ROW_GAP_ROWS {
                all_lines.push(card_gap_line(content_w, theme));
            }
        }
        all_lines.push(build_freeform_line_with_placeholder(
            freeform_idx == cursor,
            hovered == Some(freeform_idx),
            freeform_text,
            freeform_selected,
            is_multi,
            theme,
            panel_focused,
            freeform_placeholder,
        ));
    }

    all_lines
}

/// One blank row between two option blocks; carries the card's background and nothing else.
fn card_gap_line(content_w: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        " ".repeat(content_w),
        Style::default().bg(theme.bg_light),
    ))
}

/// Build an indented description continuation line.
fn build_indented_desc_line(
    indent: usize,
    desc_line: &Line<'static>,
    row_bg: ratatui::style::Color,
) -> Line<'static> {
    Line::from(
        std::iter::once(Span::styled(
            " ".repeat(indent),
            Style::default().bg(row_bg),
        ))
        .chain(desc_line.spans.iter().cloned())
        .collect::<Vec<_>>(),
    )
    .style(Style::default().bg(row_bg))
}

/// Word-wrap an overflowing label into chunks of at most `width` columns.
fn wrap_label_chunks(label: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut remaining = label;
    while !remaining.is_empty() {
        if remaining.width() <= width {
            out.push(remaining.to_string());
            break;
        }
        let byte_end = byte_offset_at_width(remaining, width);
        let break_at = remaining[..byte_end]
            .rfind(' ')
            .map(|i| i + 1)
            .unwrap_or(byte_end);
        let break_at = if break_at == 0 {
            remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len())
        } else {
            break_at
        };
        out.push(remaining[..break_at].to_string());
        remaining = remaining[break_at..].trim_start();
    }
    out
}

/// Build the visual lines for a single option: the label row, then every description line
/// indented under it.
///
/// The prompt arrow marks the keyboard cursor and is drawn whether or not the card has focus, so
/// the row the keys will act on is never ambiguous. The fill behind the row is the separate cue
/// for "this card owns the keyboard", which is why an unfocused card keeps the arrow but drops the
/// fill.
#[allow(clippy::too_many_arguments)]
fn build_single_option_lines(
    idx: usize,
    option: &QuestionOption,
    is_multi: bool,
    is_selected: bool,
    max_label_w: usize,
    prefix_w: usize,
    text_w: usize,
    row_bg: ratatui::style::Color,
    embed: Option<crate::views::modal_window::EmbeddedRowStyle>,
    theme: &Theme,
    is_cursor: bool,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let fg = |normal| embed.map_or(normal, |e| e.fg(normal));
    let shortcut_ch = option_shortcut_label(idx).unwrap_or(' ');
    let num_str = format!("{shortcut_ch}");
    let num_style = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    let label_style = Style::default()
        .fg(fg(theme.text_primary))
        .bg(row_bg)
        .add_modifier(if is_cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let cursor_style = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    // `prompt_arrow` is always two columns wide, so the two-space stand-in keeps every row's
    // label on the same column whether or not the row holds the cursor.
    let arrow = if is_cursor {
        crate::glyphs::prompt_arrow().to_string()
    } else {
        "  ".to_string()
    };

    // Multi-select: `[x]`/`[ ]` checkboxes. Single-select: `(●)`/`(○)` radios.
    // The marker reports the committed answer, the arrow reports the cursor; the two are
    // independent, so a card that has already been answered still shows where the keys will land.
    let (marker, marker_style) = if is_multi {
        if is_selected {
            (
                "[x]".to_string(),
                Style::default()
                    .fg(fg(theme.text_primary))
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("[ ]".to_string(), Style::default().fg(fg(theme.gray)).bg(row_bg))
        }
    } else if is_selected {
        // (●) falls back to (•) on legacy ConHost
        (
            format!("({})", crate::glyphs::filled_dot()),
            Style::default()
                .fg(fg(theme.text_primary))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        // (○)
        (
            "(\u{25cb})".to_string(),
            Style::default().fg(fg(theme.gray)).bg(row_bg),
        )
    };

    let prefix_spans: Vec<Span<'static>> = vec![
        Span::styled(arrow, cursor_style),
        Span::styled(format!("{num_str} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
    ];

    // Label on its own row, then every description line indented to the same column.
    let wide_w = text_w.max(1);
    let chunks = wrap_label_chunks(&normalize_label(&option.label), wide_w);
    for (li, chunk) in chunks.into_iter().enumerate() {
        if li == 0 {
            let mut spans = prefix_spans.clone();
            spans.push(Span::styled(chunk, label_style));
            out.push(Line::from(spans).style(Style::default().bg(row_bg)));
        } else {
            let spans = vec![
                Span::styled(" ".repeat(prefix_w), Style::default().bg(row_bg)),
                Span::styled(chunk, label_style),
            ];
            out.push(Line::from(spans).style(Style::default().bg(row_bg)));
        }
    }
    for line in styled_description_lines(option, wide_w, row_bg, fg(theme.gray)) {
        out.push(build_indented_desc_line(prefix_w, &line, row_bg));
    }
    let _ = max_label_w;
    out
}

/// Build the freeform row line.
///
/// The row carries the same `arrow + marker` prefix as an option row so the list reads as one
/// column, with the arrow in the space an option's shortcut number would occupy. `freeform_text`
/// is the per-question freeform text — when non-empty the row shows as ticked with a preview of
/// the answer.
#[cfg(test)]
fn build_freeform_line(
    is_cursor: bool,
    is_hovered: bool,
    freeform_text: &str,
    is_selected: bool,
    is_multi: bool,
    theme: &Theme,
    panel_focused: bool,
) -> Line<'static> {
    build_freeform_line_with_placeholder(
        is_cursor,
        is_hovered,
        freeform_text,
        is_selected,
        is_multi,
        theme,
        panel_focused,
        "Type your answer here",
    )
}

#[allow(clippy::too_many_arguments)]
fn build_freeform_line_with_placeholder(
    is_cursor: bool,
    is_hovered: bool,
    freeform_text: &str,
    is_selected: bool,
    is_multi: bool,
    theme: &Theme,
    panel_focused: bool,
    freeform_placeholder: &str,
) -> Line<'static> {
    // Whitespace-only freeform is treated as empty — never shown as selected.
    let is_selected = is_selected && !freeform_text.trim().is_empty();

    let embed = crate::views::modal_window::embedded_row_style(theme, is_cursor && panel_focused);
    let fg = |normal| embed.map_or(normal, |e| e.fg(normal));
    let row_bg = match embed {
        Some(e) => e.bg,
        None if is_cursor && panel_focused => theme.bg_visual,
        None if is_hovered => hovered_bg(theme),
        None => theme.bg_light,
    };

    // Multi-select: [x]/[ ] checkboxes.  Single-select: (●)/(○) radio buttons.
    // Both are 3 display cells, same as option rows
    let marker: String = if is_multi {
        (if is_selected { "[x]" } else { "[ ]" }).to_string()
    } else if is_selected {
        format!("({})", crate::glyphs::filled_dot())
    } else {
        "(\u{25cb})".to_string()
    };
    let marker_style = if is_selected {
        Style::default()
            .fg(fg(theme.text_primary))
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else if is_cursor {
        Style::default().fg(fg(theme.accent_user)).bg(row_bg)
    } else {
        Style::default().fg(fg(theme.gray)).bg(row_bg)
    };

    let cursor_style = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    // `prompt_arrow` is always two columns wide; the freeform row has no shortcut number, so the
    // number column holds spaces and its label still lines up with the option labels.
    let arrow = if is_cursor {
        crate::glyphs::prompt_arrow().to_string()
    } else {
        "  ".to_string()
    };

    let has_text = !freeform_text.trim().is_empty();
    let prompt_indicator = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    let (label, label_style) = if has_text {
        // Show a truncated preview of the typed answer.
        let first_line = freeform_text.lines().next().unwrap_or("");
        let preview = truncate_str(first_line, 50);
        if is_selected {
            (
                preview,
                Style::default().fg(fg(theme.text_primary)).bg(row_bg),
            )
        } else {
            (
                preview,
                Style::default().fg(fg(theme.gray)).bg(row_bg),
            )
        }
    } else {
        // Empty: show placeholder
        (
            freeform_placeholder.to_string(),
            Style::default().fg(fg(theme.gray)).bg(row_bg),
        )
    };

    let mut spans = vec![
        Span::styled(arrow, cursor_style),
        Span::styled("  ", Style::default().bg(row_bg)),
        Span::styled(format!("{marker} "), marker_style),
    ];
    // Show the prompt arrow only when there's text (not on placeholder).
    if has_text {
        spans.push(Span::styled(
            crate::glyphs::prompt_arrow(),
            prompt_indicator,
        ));
    }
    spans.push(Span::styled(label, label_style));

    Line::from(spans).style(Style::default().bg(row_bg))
}

/// Render the complete question view into the given area.
///
/// `area` is the region above the textarea allocated for the question chrome and option rows.
/// The accent `┃` line and background are rendered here.
/// Return value from [`render_question_view`] with layout info for mouse handling.
pub struct QuestionViewRenderResult {
    /// Y coordinate where the scrollable options area starts (after chrome header).
    pub options_start_y: u16,
    /// Y coordinate where the scrollable options area ends (before freeform/inline prompt).
    pub options_end_y: u16,
}

pub fn render_question_view(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
) -> QuestionViewRenderResult {
    render_question_view_with_placeholder(
        buf,
        area,
        state,
        hovered_item,
        theme,
        focused,
        "Type your answer here",
        None,
    )
}

/// Locale-aware variant of [`render_question_view`].
#[allow(clippy::too_many_arguments)]
pub fn render_question_view_with_placeholder(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
    freeform_placeholder: &str,
    locale: Option<&crate::locale::LocaleContext>,
) -> QuestionViewRenderResult {
    if area.height == 0 || area.width == 0 {
        return QuestionViewRenderResult {
            options_start_y: area.y,
            options_end_y: area.y,
        };
    }

    let q_idx = state.active_tab;
    let Some(question) = state.questions.get(q_idx) else {
        return QuestionViewRenderResult {
            options_start_y: area.y,
            options_end_y: area.y,
        };
    };

    let content_w = area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;

    // Fill background, same as the focused prompt (bg_light)
    let bg = Style::default().bg(theme.bg_light);
    buf.set_style(area, bg);

    // Accent line ┃ on the left column, blue to match the shortcut key color
    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::glyphs::accent_bar()); // ┃ falls back to │ on legacy ConHost
            cell.set_style(accent_style);
        }
    }

    // ── Minimized: one summary row; the transcript above keeps the rest of the screen ──
    if state.minimized {
        render_minimized_question_row(buf, area, state, theme, locale);
        if !focused {
            crate::render::color::blend_area(buf, area, Some((theme.bg_light, 0.66)), None);
        }
        return QuestionViewRenderResult {
            options_start_y: area.y,
            options_end_y: area.y,
        };
    }

    // Content column: rail + gap + card border + pad on the left, and the mirrored run on the right.
    let content_x = area.x + QUESTION_VIEW_CONTENT_X;
    let content_width = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    // The card's bottom rule owns the panel's last row; nothing else may be written there.
    let card_bottom_y = (area.y + area.height).saturating_sub(1);
    // The footer's top rule separates the option area from the button and hint rows.
    let footer_rule_y = card_bottom_y.saturating_sub(CARD_BOTTOM_ROWS - 1);

    // ── Card header: the top rule, the title row, and the rule under it ──
    render_question_card_header(buf, area, state, question, theme, locale);

    let mut y = area.y + 1 + CARD_HEADER_ROWS;

    // ── Question chrome (label, description, focused preview) ──
    // Clip to the footer rule: the accounted height and the rendered height can disagree (wrap-width drift, stale caps)
    // The chrome must degrade to truncation instead of writing past the panel; set_line past the buffer bottom aborts the TUI
    y = render_question_chrome(
        buf,
        content_x,
        y,
        content_width,
        footer_rule_y,
        state,
        question,
        theme,
        state.fullscreen,
        state.cached_desc_cap,
        state.cached_preview_cap,
        locale,
    );

    // ── Gap ──
    y += 1;

    let options_start_y = y;

    // ── Option cells (scrollable) + sticky freeform cell ──
    let scroll = state.per_question_scroll.get(q_idx).copied().unwrap_or(0) as usize;
    let cursor = state.cursor();
    let is_input_mode = state.focus == QuestionFocus::InputMode;

    let freeform_text = state
        .per_question_freeform
        .get(q_idx)
        .map(|s| s.as_str())
        .unwrap_or("");
    let freeform_selected = state
        .per_question_freeform_selected
        .get(q_idx)
        .copied()
        .unwrap_or(false);

    // The freeform cell is always rendered sticky at the bottom (not in the scrollable list), unless in InputMode where the inline prompt replaces it
    // When `no_freeform` is set the cell is hidden entirely.
    let sticky_freeform = !is_input_mode && !state.no_freeform;
    // The freeform box sits directly above the card's footer rule, so its height comes off the scroll area.
    let freeform_h: u16 = if sticky_freeform {
        FREEFORM_ROW_ROWS + OPTION_ROW_GAP_ROWS
    } else {
        0
    };
    // The freeform box sits flush above the footer rule; the gap that separates it from the
    // options above is already part of `freeform_h`.
    let freeform_top_y = footer_rule_y.saturating_sub(FREEFORM_ROW_ROWS);

    // Build option lines WITHOUT the freeform row (it's sticky or inline).
    let all_lines = build_flat_option_lines_with_placeholder(
        question,
        content_w,
        cursor,
        hovered_item,
        &state.selections[q_idx],
        theme,
        false, // never in scroll list
        freeform_text,
        freeform_selected,
        focused,
        freeform_placeholder,
    );

    let scroll_bottom = footer_rule_y.saturating_sub(freeform_h);
    let visible_h = scroll_bottom.saturating_sub(y) as usize;
    for line in all_lines.iter().skip(scroll).take(visible_h) {
        if y >= scroll_bottom {
            break;
        }
        let row_rect = Rect {
            x: content_x,
            y,
            width: content_width,
            height: 1,
        };
        buf.set_style(row_rect, line.style);
        set_line_clipped(buf, content_x, y, line, content_width);
        y += 1;
    }

    // ── Sticky freeform row pinned above the footer ──
    if sticky_freeform && freeform_top_y >= y {
        let freeform_idx = question.options.len();
        let is_multi = question.multi_select.unwrap_or(false);
        let freeform_line = build_freeform_line_with_placeholder(
            freeform_idx == cursor,
            hovered_item == Some(freeform_idx),
            freeform_text,
            freeform_selected,
            is_multi,
            theme,
            focused,
            freeform_placeholder,
        );
        let row_rect = Rect {
            x: content_x,
            y: freeform_top_y,
            width: content_width,
            height: 1,
        };
        buf.set_style(row_rect, freeform_line.style);
        set_line_clipped(buf, content_x, freeform_top_y, &freeform_line, content_width);
    }

    // ── Card footer: the rule, the buttons, and the key hints ──
    let hidden_below =
        hidden_items_below(question, content_w, cursor, scroll as u16, visible_h as u16);
    render_question_card_footer(buf, area, state, question, theme, locale, footer_rule_y, hidden_below);

    // ── Card frame: rules, side borders ──
    paint_question_card_frame(buf, area, theme, focused, card_bottom_y, footer_rule_y);

    // Unfocus dim: when the user has navigated to the scrollback (or any other pane), blend foregrounds toward `bg_light` so the panel recedes
    // Mirrors the unfocused prompt widget pattern (`prompt_widget.rs:1948`)
    if !focused {
        crate::render::color::blend_area(buf, area, Some((theme.bg_light, 0.66)), None);
    }

    let options_end_y = scroll_bottom;
    QuestionViewRenderResult {
        options_start_y,
        options_end_y,
    }
}

/// Draw the card's footer: the advance button on the left, `取消` on the right, and the key-hint
/// row under them.
///
/// The question counter is not repeated here — the header already carries it, and a second copy
/// of the same number in one card is noise. Only the buttons the card can act on are drawn, so a
/// single-question card keeps `取消` alone instead of offering a `下一题` that would do nothing.
fn render_question_card_footer(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    question: &Question,
    theme: &Theme,
    locale: Option<&crate::locale::LocaleContext>,
    footer_rule_y: u16,
    hidden_below: usize,
) {
    let inner_x = area.x + QUESTION_VIEW_CONTENT_X;
    let inner_w = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    if inner_w == 0 {
        return;
    }
    let bg = Style::default().bg(theme.bg_light);
    let muted = Style::default().fg(theme.gray).bg(theme.bg_light);
    // Footer rule, then the button row, then the card's bottom rule.
    let button_row_y = footer_rule_y.saturating_add(1);

    let mut x = inner_x;
    let _ = question;

    let next_label = card_text(locale, "question.next", "Next");
    let is_last = state.active_tab + 1 >= state.questions.len();
    let next_text = if is_last {
        // The last question submits the whole card, so the button names that instead of advancing.
        card_text(locale, "shortcut.submit", "Submit").to_string()
    } else {
        format!("{next_label} \u{2192}")
    };
    let next_style = if state.active_tab_has_selection() {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_light)
            .add_modifier(Modifier::BOLD)
    } else {
        muted
    };
    let next_rect = Rect {
        x,
        y: button_row_y,
        width: next_text.width() as u16 + 2,
        height: 1,
    };
    if next_rect.right() < inner_x + inner_w {
        set_line_clipped(
            buf,
            next_rect.x,
            button_row_y,
            &Line::from(Span::styled(format!(" {} ", next_text), next_style)),
            next_rect.width,
        );
    }

    // Dismiss button, right-aligned. It throws the whole card away, so it wears the error accent
    // rather than the card's own colour, which is what the advance button uses.
    let cancel_text = card_text(locale, "question.dismiss", "Cancel").to_string();
    let cancel_w = cancel_text.width() as u16 + 2;
    let cancel_x = (inner_x + inner_w).saturating_sub(cancel_w);
    if cancel_x > inner_x {
        set_line_clipped(
            buf,
            cancel_x,
            button_row_y,
            &Line::from(Span::styled(
                format!(" {cancel_text} "),
                Style::default()
                    .fg(theme.accent_error)
                    .bg(theme.bg_light)
                    .add_modifier(Modifier::BOLD),
            )),
            cancel_w,
        );
    }

    // ── Key-hint row: `j/k 选择 · 1-9 直达 · Enter 选中 · Tab 切题 · Space 多选 · Esc 关闭` ──
    let hint_y = footer_rule_y.saturating_add(2);
    let key_style = Style::default().fg(theme.accent_user).bg(theme.bg_light);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push_hint =
        |spans: &mut Vec<Span<'static>>, key: &str, label_id: &str, english_tag: &'static str| {
        if !spans.is_empty() {
            spans.push(Span::styled(" \u{b7} ", muted));
        }
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(
            format!(" {}", card_text(locale, label_id, english_tag)),
            muted,
        ));
    };
    push_hint(&mut spans, "j/k", "question.footer.select", "select");
    if question.options.len() > 1 {
        push_hint(&mut spans, "1-9", "question.footer.jump", "jump");
    }
    push_hint(&mut spans, "Enter", "question.footer.pick", "pick");
    if state.questions.len() > 1 {
        push_hint(&mut spans, "Tab", "question.footer.switch", "switch");
    }
    if question.multi_select.unwrap_or(false) {
        push_hint(&mut spans, "Space", "question.footer.multi", "multi");
    }
    push_hint(&mut spans, "Esc", "question.footer.close", "close");
    // The list is the only part of the card that can be cut. Say how many options are still below
    // the fold, so a scrolled card never reads as a complete one.
    if hidden_below > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled(" \u{b7} ", muted));
        }
        spans.push(Span::styled(
            card_text(locale, "question.footer.more", "{count} more")
                .replace("{count}", &hidden_below.to_string()),
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ));
    }
    set_line_clipped(buf, inner_x, hint_y, &Line::from(spans), inner_w);
}

/// Bounds-checked [`Buffer::set_line`]: clamps the width to the buffer so a resize race cannot abort the TUI.
fn set_line_clipped(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, width: u16) {
    let avail = buf.area.right().saturating_sub(x);
    buf.set_line_safe(x, y, line, width.min(avail));
}

/// Localized question-card phrase, with the English fallback the no-locale renderers get.
fn card_text(
    locale: Option<&crate::locale::LocaleContext>,
    id: &str,
    english: &'static str,
) -> &'static str {
    locale.map_or(english, |locale| locale.named_static_text(id, english))
}

/// `第 3/5 题` / `Question 3/5`, from a catalog template so word order stays translatable.
fn question_counter_text(
    locale: Option<&crate::locale::LocaleContext>,
    index: usize,
    total: usize,
) -> String {
    card_text(locale, "question.counter", "Question {current}/{total}")
        .replace("{current}", &(index + 1).to_string())
        .replace("{total}", &total.to_string())
}

/// The card header's minimize control: a fixed three-column target at the inner right edge.
///
/// Fixed width, so the drawn icon and the mouse target cannot drift apart when the label changes language.
/// `None` when the panel is too narrow to carry it; the icon is not drawn either.
pub fn minimize_control_rect(area: Rect) -> Option<Rect> {
    let inner_w = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    if inner_w < 5 || area.height <= CARD_HEADER_ROWS {
        return None;
    }
    Some(Rect {
        x: area.x + QUESTION_VIEW_CONTENT_X + inner_w - 3,
        y: area.y + 2,
        width: 3,
        height: 1,
    })
}

/// The card's own bottom rule, which owns the panel's last row.
pub fn card_bottom_row(area: Rect) -> u16 {
    (area.y + area.height).saturating_sub(1)
}

/// The rule that separates the option area from the card's footer.
pub fn card_footer_rule_row(area: Rect) -> u16 {
    card_bottom_row(area).saturating_sub(CARD_BOTTOM_ROWS - 1)
}

/// Rows the prompt pane reserves below the card: the collapsed-card hint row and its gaps.
pub const QUESTION_FOOTER_H: u16 = 3;

/// The question card's area inside the prompt pane.
///
/// The renderer subtracts the inline composer and the card footer from the pane, and mouse
/// hit-testing has to land on exactly those rows. Both sides ask here rather than each re-deriving
/// a height: a handler that is one row off puts its hit rect on rows the user cannot see, and a
/// click on the drawn row then does nothing.
pub fn question_card_area(prompt: Rect, inline_prompt_h: u16, footer_h: u16) -> Rect {
    Rect {
        x: prompt.x,
        y: prompt.y,
        width: prompt.width,
        height: prompt
            .height
            .saturating_sub(inline_prompt_h)
            .saturating_sub(footer_h),
    }
}

/// The whole sticky freeform box, or `None` when the panel is too short to place it.
///
/// Shared by the renderer and mouse hit-testing so a click and the drawn box cannot drift apart.
pub fn freeform_box_rect(area: Rect) -> Option<Rect> {
    let footer_rule_y = card_footer_rule_row(area);
    let top = footer_rule_y.saturating_sub(FREEFORM_ROW_ROWS);
    // The box needs its three rows above the footer rule.
    if top <= area.y + 1 + CARD_HEADER_ROWS || footer_rule_y <= top {
        return None;
    }
    Some(Rect {
        x: area.x + QUESTION_VIEW_CONTENT_X,
        y: top,
        width: area.width.saturating_sub(QUESTION_VIEW_HPAD),
        height: FREEFORM_ROW_ROWS,
    })
}

/// Draw the card's top rule, title row, and the rule under it.
///
/// The title row carries the card's identity (`◆ 提问` plus a `可多选` badge) on the left,
/// the question counter and the minimize control on the right.
fn render_question_card_header(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    question: &Question,
    theme: &Theme,
    locale: Option<&crate::locale::LocaleContext>,
) {
    let inner_x = area.x + QUESTION_VIEW_CONTENT_X;
    let inner_w = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    if inner_w == 0 {
        return;
    }
    let y = area.y + 2;
    let bg = Style::default().bg(theme.bg_light);
    let accent = theme.accent_assistant;

    let mut spans = vec![
        Span::styled(
            crate::glyphs::diamond_filled(),
            Style::default().fg(accent).bg(theme.bg_light),
        ),
        Span::styled(" ", bg),
        Span::styled(
            card_text(locale, "question.title", "Question"),
            Style::default()
                .fg(accent)
                .bg(theme.bg_light)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if question.multi_select.unwrap_or(false) {
        spans.push(Span::styled("  ", bg));
        spans.push(Span::styled(
            format!(
                "[{}]",
                card_text(locale, "question.multi_select", "multi-select")
            ),
            Style::default().fg(accent).bg(theme.bg_light),
        ));
    }

    let counter = (state.questions.len() > 1)
        .then(|| question_counter_text(locale, state.active_tab, state.questions.len()));

    let Some(control) = minimize_control_rect(area) else {
        set_line_clipped(buf, inner_x, y, &Line::from(spans), inner_w);
        return;
    };

    if let Some(cell) = buf.cell_mut((control.x + 1, y)) {
        cell.set_symbol(crate::glyphs::minimize_icon());
        cell.set_style(
            Style::default()
                .fg(theme.gray)
                .bg(theme.bg_light)
                .add_modifier(Modifier::BOLD),
        );
    }

    let Some(counter) = counter else {
        set_line_clipped(buf, inner_x, y, &Line::from(spans), inner_w);
        return;
    };
    let counter_w = counter.width() as u16;
    let counter_x = control.x.saturating_sub(counter_w).saturating_sub(2);
    if counter_x > inner_x {
        set_line_clipped(
            buf,
            counter_x,
            y,
            &Line::from(Span::styled(
                counter,
                Style::default().fg(theme.gray).bg(theme.bg_light),
            )),
            counter_w,
        );
        set_line_clipped(buf, inner_x, y, &Line::from(spans), counter_x - inner_x);
    } else {
        set_line_clipped(buf, inner_x, y, &Line::from(spans), inner_w);
    }
}

/// Draw the collapsed card: one row naming the question and its state.
///
/// The way back is the footer's (`m` / `Tab`), so the row itself carries no key hints.
fn render_minimized_question_row(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    theme: &Theme,
    locale: Option<&crate::locale::LocaleContext>,
) {
    let inner_x = area.x + QUESTION_VIEW_CONTENT_X;
    let inner_w = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    if inner_w == 0 {
        return;
    }
    let bg = Style::default().bg(theme.bg_light);
    let muted = Style::default().fg(theme.gray).bg(theme.bg_light);

    let mut spans = vec![
        Span::styled(
            crate::glyphs::diamond_filled(),
            Style::default()
                .fg(theme.accent_assistant)
                .bg(theme.bg_light),
        ),
        Span::styled(" ", bg),
        Span::styled(
            card_text(locale, "question.title", "Question"),
            Style::default()
                .fg(theme.accent_assistant)
                .bg(theme.bg_light)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", bg),
    ];
    if state.questions.len() > 1 {
        spans.push(Span::styled(
            question_counter_text(locale, state.active_tab, state.questions.len()),
            muted,
        ));
        spans.push(Span::styled("  ", bg));
    }
    spans.push(Span::styled(
        card_text(locale, "question.minimized", "minimized"),
        muted,
    ));
    set_line_clipped(buf, inner_x, area.y, &Line::from(spans), inner_w);
}

/// Draw the card's own frame: the top rule, the rule under the title, the footer rule, and the
/// bottom rule, plus a side border on every row between the header and the footer.
///
/// The option rows are drawn by [`build_flat_option_lines_with_placeholder`]; this only frames the
/// card they sit in.
fn paint_question_card_frame(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    focused: bool,
    card_bottom_y: u16,
    footer_rule_y: u16,
) {
    let left_x = area.x + QUESTION_VIEW_CONTENT_X - 2;
    let right_x = area.x + area.width.saturating_sub(3);
    let top_y = area.y + 1;
    let header_rule_y = top_y + CARD_HEADER_ROWS - 1;
    // Below three columns the frame would be all border and no content.
    if right_x < left_x + 3 || footer_rule_y <= header_rule_y + 1 {
        return;
    }

    // The card floats over the transcript, so its outline carries the card's accent while it owns
    // the keyboard and drops to the dim prompt border when it does not. A single number cannot
    // both say "this is a surface" and "these keys are live", so the colour splits the two.
    let border = Style::default()
        .fg(if focused {
            theme.accent_assistant
        } else {
            theme.prompt_border
        })
        .bg(theme.bg_light);
    let rule = crate::glyphs::light_horizontal();
    let mut put = |x: u16, y: u16, symbol: &str| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_symbol(symbol);
            cell.set_style(border);
        }
    };

    for x in (left_x + 1)..right_x {
        put(x, top_y, rule);
        put(x, header_rule_y, rule);
        put(x, footer_rule_y, rule);
        put(x, card_bottom_y, rule);
    }
    put(left_x, top_y, crate::glyphs::box_top_left());
    put(right_x, top_y, crate::glyphs::box_top_right());
    put(left_x, header_rule_y, crate::glyphs::box_left_tee());
    put(right_x, header_rule_y, crate::glyphs::box_right_tee());
    put(left_x, footer_rule_y, crate::glyphs::box_left_tee());
    put(right_x, footer_rule_y, crate::glyphs::box_right_tee());
    put(left_x, card_bottom_y, crate::glyphs::box_bottom_left());
    put(right_x, card_bottom_y, crate::glyphs::box_bottom_right());

    for y in (header_rule_y + 1)..card_bottom_y {
        if y == footer_rule_y {
            continue;
        }
        put(left_x, y, "\u{2502}");
        put(right_x, y, "\u{2502}");
    }
}

/// Render the question view scrollbar. Call this AFTER `render_prompt_chrome`.
///
/// `scrollbar_x` is the column to render the scrollbar in; it should be outside the selection box border (same column as the scrollback scrollbar).
/// Returns the scrollbar track rect if one was rendered (for mouse hit-testing).
pub fn render_question_scrollbar(
    buf: &mut Buffer,
    scrollbar_x: u16,
    state: &QuestionViewState,
    theme: &Theme,
    scroll_region: (u16, u16),
) -> Option<Rect> {
    let q_idx = state.active_tab;
    let question = state.questions.get(q_idx)?;

    let (scroll_top, scroll_bottom) = scroll_region;
    let visible_options_h = scroll_bottom.saturating_sub(scroll_top);
    // Content width for height computation; use a reasonable estimate
    let total_option_h = {
        let cw = buf.area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
        total_options_height(question, cw, state.cursor())
            .saturating_sub(state.phantom_freeform_h())
    };

    let scroll = state.per_question_scroll.get(q_idx).copied().unwrap_or(0);

    if total_option_h > visible_options_h && visible_options_h > 0 {
        let scrollbar_area = Rect {
            x: scrollbar_x,
            y: scroll_top,
            width: 1,
            height: visible_options_h,
        };
        // Use visible colors against bg_light: dim track, bright thumb.
        let track_style = Style::default().fg(theme.gray_dim).bg(theme.bg_light);
        let thumb_style = Style::default().fg(theme.gray).bg(theme.bg_light);
        crate::render::scrollbar::render_scrollbar_styled(
            buf,
            Some(scrollbar_area),
            total_option_h,
            visible_options_h,
            scroll,
            track_style,
            thumb_style,
        );
        return Some(scrollbar_area);
    }
    None
}

/// Render a truncation indicator line: `... Ctrl-F to expand`.
fn render_truncation_indicator(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    theme: &Theme,
    locale: Option<&crate::locale::LocaleContext>,
) {
    let style = Style::default().fg(theme.gray).bg(theme.bg_light);
    let indicator = Line::from(vec![
        Span::styled("... ", style),
        Span::styled(
            "Ctrl-F",
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ),
        Span::styled(
            locale
                .map(|locale| locale.named_static_text("question.truncation.expand", " to expand"))
                .unwrap_or(" to expand"),
            style,
        ),
    ]);
    buf.set_line(x, y, &indicator, width);
}

/// Render question chrome: label line and description.
///
/// Returns the Y position after the rendered chrome.
///
/// All writes are clipped to `max_y` (exclusive).
/// The accounted chrome height (`chrome_height`) and the rendered height can drift (e.g. wrap-width differences).
/// An unclipped `set_line` below the buffer bottom panics inside ratatui; clipping degrades to truncation instead.
#[allow(clippy::too_many_arguments)]
fn render_question_chrome(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    max_y: u16,
    state: &QuestionViewState,
    question: &Question,
    theme: &Theme,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
    locale: Option<&crate::locale::LocaleContext>,
) -> u16 {
    let mut cur_y = y;
    let w = width as usize;
    // Never write below the panel or the buffer
    // The area itself should already be inside the buffer, but a mis-sized area must degrade to truncation, not an abort
    let max_y = max_y.min(buf.area.bottom());

    // Split into label (first paragraph) and description (rest).
    let (label_text, desc_text) = split_question_label_desc(&question.question);

    // ── Label (bold, primary text, word-wrapped) ──
    let label_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);

    let raw_line = Line::from(vec![Span::styled(label_text.to_string(), label_style)]);
    let wrapped = crate::render::wrapping::word_wrap_line(&raw_line, w);
    for line in &wrapped {
        if cur_y >= max_y {
            return cur_y;
        }
        buf.set_line(x, cur_y, line, width);
        cur_y += 1;
    }

    // Blank line separating the label from what follows it. With neither a description nor a
    // preview below there is nothing to separate, and the option list opens with its own blank
    // row, so a second one would only leave a short question floating.
    // `chrome_height_with_dynamic_caps` counts the same row under the same condition.
    let has_chrome_below = !desc_text.is_empty()
        || state
            .focused_preview()
            .is_some_and(|p| !p.is_empty());
    if has_chrome_below {
        cur_y += 1;
    }

    // ── Description (dimmed, markdown-rendered) ──
    if !desc_text.is_empty() {
        let desc_lines = styled_description_lines(
            &QuestionOption {
                label: String::new(),
                description: desc_text.to_string(),
                preview: None,
                id: None,
            },
            w,
            theme.bg_light,
            theme.gray,
        );
        let raw_desc_count = desc_lines.len() as u16;
        let is_truncated = !fullscreen && raw_desc_count > desc_cap;
        for (desc_rendered, line) in desc_lines.into_iter().enumerate() {
            let desc_rendered = desc_rendered as u16;
            if !fullscreen && desc_rendered >= desc_cap {
                break;
            }
            if cur_y >= max_y {
                return cur_y;
            }
            // Always render the real content line first
            // When truncated and there is room for content plus an indicator (cap >= 2), the indicator lands after the second-to-last real line
            // When cap == 1 we show the single content line without an indicator; there is no room for both
            buf.set_line(x, cur_y, &line, width);
            cur_y += 1;
            if is_truncated && desc_cap >= 2 && desc_rendered == desc_cap.saturating_sub(2) {
                if cur_y >= max_y {
                    return cur_y;
                }
                render_truncation_indicator(buf, x, cur_y, width, theme, locale);
                cur_y += 1;
                break;
            }
        }
    }

    // ── Preview for focused option (dimmed, word-wrapped) ──
    if let Some(preview_text) = state.focused_preview()
        && !preview_text.is_empty()
    {
        let preview_style = Style::default().fg(theme.gray).bg(theme.bg_light);

        // Count total preview lines first to determine truncation.
        // Uses Span::raw to match chrome_height (style doesn't affect wrapping).
        let mut total_preview_count = 0u16;
        for text_line in preview_text.lines() {
            let raw = Line::from(vec![Span::raw(text_line.to_string())]);
            total_preview_count += crate::render::wrapping::word_wrap_line(&raw, w)
                .len()
                .max(1) as u16;
        }

        // Cap to match chrome_height accounting.
        let capped_count = if fullscreen {
            total_preview_count
        } else {
            total_preview_count.min(preview_cap)
        };

        // Only emit the preview gap if there will be visible preview lines.
        if capped_count > 0 {
            cur_y += 1;
        }

        let is_truncated = !fullscreen && total_preview_count > preview_cap;
        let mut preview_rendered = 0u16;
        'preview_done: for text_line in preview_text.lines() {
            let raw = Line::from(vec![Span::styled(text_line.to_string(), preview_style)]);
            let wrapped = crate::render::wrapping::word_wrap_line(&raw, w);
            for line in &wrapped {
                if !fullscreen && preview_rendered >= preview_cap {
                    break 'preview_done;
                }
                if cur_y >= max_y {
                    return cur_y;
                }
                // Always render the real content line first
                // Append the truncation indicator only when cap >= 2 so at least one real preview line is visible above it
                buf.set_line(x, cur_y, line, width);
                cur_y += 1;
                preview_rendered += 1;
                if is_truncated
                    && preview_cap >= 2
                    && preview_rendered == preview_cap.saturating_sub(1)
                {
                    if cur_y >= max_y {
                        return cur_y;
                    }
                    render_truncation_indicator(buf, x, cur_y, width, theme, locale);
                    cur_y += 1;
                    break 'preview_done;
                }
            }
        }
    }

    cur_y
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic long multi-line `ask_user_question` payload for layout regression tests (wide wrap and multi-line option previews).
    /// Content is fictional and not from a real session.
    fn gb3747_question() -> Question {
        Question {
            question: "When renaming the shared helper module used by both the \
                       CLI and the desktop client, which compatibility approach \
                       should we take for the public config keys?"
                .to_string(),
            options: vec![
                QuestionOption {
                    label: "Keep old keys as aliases for one release".to_string(),
                    description: "Ship dual-read of the previous key names, log a \
                                  deprecation notice once per session, and remove \
                                  the aliases in the following minor version."
                        .to_string(),
                    preview: Some(
                        "config.legacy_keys: dual-read for one release\n\
                         deprecation notice: once per session\n\
                         remove aliases: next minor\n\
                         tests: load both old and new keys"
                            .to_string(),
                    ),
                    id: None,
                },
                QuestionOption {
                    label: "Break now with a clear migration note".to_string(),
                    description: "Drop the old keys immediately, document the rename \
                                  in the changelog, and print a one-line hint when an \
                                  unknown legacy key is present."
                        .to_string(),
                    preview: Some(
                        "config.legacy_keys: removed\n\
                         changelog: document rename\n\
                         unknown legacy key: one-line hint\n\
                         tests: reject old keys"
                            .to_string(),
                    ),
                    id: None,
                },
            ],
            multi_select: None,
            id: None,
        }
    }

    /// Regression: `draw()` used to size the question panel by wrapping at the full inner width.
    /// The renderer wraps at `width - QUESTION_VIEW_HPAD`, so the under-allocated panel let the unclipped chrome walk past the buffer bottom.
    /// That aborted in ratatui (`index outside of buffer: ... but index is (5, H)`).
    ///
    /// Recreates that exact under-allocation (height computed at `w`, render at `w - HPAD`) across terminal sizes: rendering must clip, never panic.
    /// Fails on pre-fix code at e.g. 31x14 with `(5, 14)`.
    #[test]
    fn gb3747_regression_mis_sized_area_never_panics() {
        let theme = Theme::default();
        for w in 20u16..=140 {
            for h in 10u16..=60 {
                let mut state = QuestionViewState::new(
                    "tc".into(),
                    vec![gb3747_question()],
                    StashedPrompt::default(),
                );
                let inner_width = w.saturating_sub(4); // hpad_left 2 + hpad_right 2
                // Pre-fix draw() bug: full inner width (no HPAD subtraction).
                let qv_h = question_view_height(&mut state, h, inner_width as usize);
                let question_footer_h: u16 = 3;
                let reserved = 1 + 5 + 1 + 3; // draw()'s overcommit clamp
                let prompt_height = (qv_h + question_footer_h)
                    .max(3)
                    .min(h.saturating_sub(reserved));
                // The prompt slot sits directly above the shortcuts row.
                let question_area = Rect {
                    x: 2,
                    y: h.saturating_sub(1).saturating_sub(prompt_height),
                    width: inner_width,
                    height: prompt_height.saturating_sub(question_footer_h),
                };
                let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
                let _ = render_question_view(&mut buf, question_area, &state, None, &theme, true);
            }
        }
    }

    /// Companion to the mis-sized-area regression, with the *fixed* accounting (heights computed at the same `content_w` the renderer wraps at).
    /// The chrome must fit the allocation exactly: rendering into a generous buffer, the rendered chrome height equals `chrome_height`.
    #[test]
    fn gb3747_chrome_accounting_matches_render() {
        let theme = Theme::default();
        for content_w in [20usize, 35, 60, 90, 120] {
            let mut state = QuestionViewState::new(
                "tc".into(),
                vec![gb3747_question()],
                StashedPrompt::default(),
            );
            // Fixed convention: accounting at the render wrap width.
            let _ = question_view_height(&mut state, 200, content_w);
            let question = &state.questions[0];
            let expected_chrome = chrome_height(
                question,
                content_w,
                state.focused_preview(),
                false,
                state.cached_desc_cap,
                state.cached_preview_cap,
            );

            let area_w = content_w as u16 + QUESTION_VIEW_HPAD;
            let area = Rect::new(0, 0, area_w, 200);
            let mut buf = Buffer::empty(area);
            let result = render_question_view(&mut buf, area, &state, None, &theme, true);
            // chrome_height counts vpad(1) + label + gap + desc + preview + bottom gap(1); options_start_y sits after exactly that
            assert_eq!(
                result.options_start_y - area.y,
                expected_chrome,
                "chrome accounting vs render drift at content_w={content_w}"
            );
        }
    }

    #[test]
    fn begin_feedback_trace_stage_swaps_report_for_consent_options() {
        let mut state = QuestionViewState::new(
            "fb".into(),
            vec![Question {
                question: FEEDBACK_QUESTION_LABEL.into(),
                options: vec![],
                multi_select: Some(false),
                id: None,
            }],
            StashedPrompt::default(),
        )
        .with_local_kind(LocalQuestionKind::Feedback);
        state.per_question_freeform[0] = "clipboard is broken over ssh".into();

        state.begin_feedback_trace_stage(state.feedback_report(), vec![]);

        assert!(
            state.is_feedback(),
            "trace stage is still the feedback card"
        );
        assert!(state.is_feedback_trace());
        assert!(!state.is_feedback_report());
        assert_eq!(state.questions.len(), 1);
        assert_eq!(state.questions[0].question, FEEDBACK_TRACE_QUESTION_LABEL);
        assert_eq!(state.questions[0].options.len(), 3);
        assert_eq!(
            state.questions[0].options[2].label,
            "Opt out and don't ask again"
        );
        assert_eq!(
            state.questions[0]
                .options
                .iter()
                .map(|o| o.id.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some(FEEDBACK_TRACE_OPTION_OPT_IN),
                Some(FEEDBACK_TRACE_OPTION_OPT_OUT),
                Some(FEEDBACK_TRACE_OPTION_NEVER_ASK),
            ],
            "consent maps from ids, so every option must carry one"
        );
        assert!(
            matches!(state.selections[0], QuestionSelection::Single(Some(0))),
            "turning trace upload on is the default"
        );
        assert!(state.no_freeform, "consent card has no free-text row");
        assert_eq!(state.focus, QuestionFocus::Navigation);
        let Some(LocalQuestionKind::FeedbackTrace { report, .. }) = &state.local_kind else {
            panic!("local kind must carry the report");
        };
        assert_eq!(report, "clipboard is broken over ssh");
    }

    /// Helper: build a question with N options.
    fn make_question(text: &str, labels: &[&str], multi: bool) -> Question {
        Question {
            question: text.to_string(),
            options: labels
                .iter()
                .map(|l| QuestionOption {
                    label: l.to_string(),
                    description: format!("Desc for {l}"),
                    preview: None,
                    id: None,
                })
                .collect(),
            multi_select: Some(multi),
            id: None,
        }
    }

    /// A real 16×16 PNG, so the encoder reads back actual bytes and a real MIME type.
    fn answer_image() -> crate::prompt_images::PastedImage {
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(16, 16, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode test png");
        crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
            data: bytes,
            mime_type: "image/png".to_string(),
        })
    }

    /// An answer that is only a pasted image must survive as an answer: the
    /// `answers` entry is `["Other"]` and the payload rides the annotation,
    /// because the model-visible notes are empty.
    #[test]
    fn image_only_answer_is_an_answer_on_the_wire() {
        let mut state = QuestionViewState::new(
            "tc".into(),
            vec![make_question("Pick one?", &["A", "B"], false)],
            StashedPrompt::default(),
        );
        state.per_question_freeform_selected[0] = true;
        state.set_parked_images(0, vec![answer_image()]);

        let response = state.build_accepted_response();
        let AskUserQuestionExtResponse::Accepted {
            answers,
            annotations,
        } = response
        else {
            panic!("expected an accepted response")
        };
        assert_eq!(answers["Pick one?"], vec!["Other".to_string()]);
        let annotation = annotations
            .expect("an image-only answer still carries an annotation")
            .remove("Pick one?")
            .expect("annotation keyed by the question text");
        assert_eq!(annotation.notes, None, "no text was typed");
        assert_eq!(annotation.answer_images().len(), 1);
        assert_eq!(annotation.answer_images()[0].mime_type, "image/png");
        assert!(
            !annotation.answer_images()[0].data.is_empty(),
            "the base64 payload must be carried"
        );
    }

    /// A text answer with an image keeps both, and the notes stay free of the
    /// composer's `[Image #N]` chip text.
    #[test]
    fn text_and_image_answer_carry_both() {
        let mut state = QuestionViewState::new(
            "tc".into(),
            vec![make_question("Pick one?", &["A", "B"], false)],
            StashedPrompt::default(),
        );
        state.per_question_freeform_selected[0] = true;
        state.per_question_freeform[0] = "look at this".to_string();
        state.set_parked_images(0, vec![answer_image()]);

        let response = state.build_accepted_response();
        let AskUserQuestionExtResponse::Accepted { annotations, .. } = response else {
            panic!("expected an accepted response")
        };
        let annotation = annotations.unwrap().remove("Pick one?").unwrap();
        assert_eq!(annotation.notes.as_deref(), Some("look at this"));
        assert_eq!(annotation.answer_images().len(), 1);
    }

    /// The composer's live set is the active question's source of truth: a parked
    /// copy from an earlier visit to the same tab must not be appended to it.
    #[test]
    fn composer_images_win_over_the_parked_copy_for_the_active_question() {
        let mut state = QuestionViewState::new(
            "tc".into(),
            vec![make_question("Pick one?", &["A", "B"], false)],
            StashedPrompt::default(),
        );
        state.per_question_freeform_selected[0] = true;
        state.set_parked_images(0, vec![answer_image()]);

        let response =
            state.build_accepted_response_with_images(vec![answer_image(), answer_image()]);
        let AskUserQuestionExtResponse::Accepted { annotations, .. } = response else {
            panic!("expected an accepted response")
        };
        assert_eq!(
            annotations
                .unwrap()
                .remove("Pick one?")
                .unwrap()
                .answer_images()
                .len(),
            2,
            "only the live composer set belongs to the active question"
        );
    }

    /// The submit path parks the active draft before building the response, so an
    /// empty live set must fall back to the parked images rather than lose them.
    #[test]
    fn parked_images_are_reused_when_the_composer_holds_none() {
        let mut state = QuestionViewState::new(
            "tc".into(),
            vec![make_question("Pick one?", &["A", "B"], false)],
            StashedPrompt::default(),
        );
        state.per_question_freeform_selected[0] = true;
        state.set_parked_images(0, vec![answer_image()]);

        let response = state.build_accepted_response();
        let AskUserQuestionExtResponse::Accepted { annotations, .. } = response else {
            panic!("expected an accepted response")
        };
        assert_eq!(
            annotations
                .unwrap()
                .remove("Pick one?")
                .unwrap()
                .answer_images()
                .len(),
            1
        );
    }

    /// Regression: on the terminal-native palette (`bg_visual = Reset`) the
    /// embedded cursor row used to be indistinguishable except for a bold
    /// label.
    #[test]
    #[serial_test::serial]
    fn embedded_cursor_row_takes_selection_accent() {
        use ratatui::style::Color;

        struct EmbedReset;
        impl Drop for EmbedReset {
            fn drop(&mut self) {
                crate::views::modal_window::set_embedded(false);
            }
        }
        let _reset = EmbedReset;
        crate::views::modal_window::set_embedded(true);

        let theme = Theme::terminal_default();
        let q = make_question("Pick one?", &["Alpha", "Beta"], false);
        let lines = build_flat_option_lines(
            &q,
            80,
            0,
            None,
            &QuestionSelection::Single(None),
            &theme,
            true,
            "",
            false,
            true,
        );

        // Cursor row (option 0): every colored span carries the accent, and the row stays transparent
        // The label leads its own row, so find that row rather than assuming a position.
        let cursor_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Alpha")))
            .expect("cursor row carries the label");
        // The box's own border carries the theme's border color, so only the text spans are checked.
        let text_spans: Vec<_> = cursor_line
            .spans
            .iter()
            .filter(|s| s.content.contains("Alpha") || s.content.starts_with('1'))
            .collect();
        assert!(
            text_spans
                .iter()
                .all(|s| s.style.fg == Some(theme.fuzzy_accent)),
            "cursor row must recolor all text with the selection accent, got {:?}",
            cursor_line
                .spans
                .iter()
                .map(|s| (s.content.clone(), s.style.fg))
                .collect::<Vec<_>>()
        );
        assert!(
            cursor_line
                .spans
                .iter()
                .all(|s| s.style.bg == Some(Color::Reset) || s.style.bg.is_none()),
            "embedded rows must not paint a background band"
        );

        // Non-cursor row keeps normal colors (the label is text_primary)
        let other_line = lines
            .iter()
            .find(|line| line_text(line).contains("Beta"))
            .expect("the non-cursor option must be rendered");
        assert!(
            other_line
                .spans
                .iter()
                .any(|s| s.content.contains("Beta") && s.style.fg == Some(theme.text_primary)),
            "non-cursor row keeps the normal label color"
        );

        // Freeform row on cursor: same accent treatment.
        let freeform_cursor =
            build_freeform_line(true, false, "", false, false, &theme, true);
        assert!(
            freeform_cursor
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty() && s.content != "\u{2502}")
                .all(|s| s.style.fg == Some(theme.fuzzy_accent)),
            "freeform cursor row must take the accent"
        );
    }

    #[test]
    #[serial_test::serial]
    fn full_tui_cursor_row_keeps_bg_visual_band() {
        crate::views::modal_window::set_embedded(false);
        let theme = Theme::default();
        let q = make_question("Pick one?", &["Alpha", "Beta"], false);
        let lines = build_flat_option_lines(
            &q,
            80,
            0,
            None,
            &QuestionSelection::Single(None),
            &theme,
            true,
            "",
            false,
            true,
        );
        // The cursor's box body carries the bg_visual band; its rules stay on the card surface.
        let cursor_body = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Alpha")))
            .expect("cursor row carries the label");
        assert!(
            cursor_body
                .spans
                .iter()
                .all(|s| s.style.bg == Some(theme.bg_visual)),
            "full TUI cursor row paints the bg_visual band"
        );
        assert!(
            cursor_body
                .spans
                .iter()
                .any(|s| s.content.contains("Alpha") && s.style.fg == Some(theme.text_primary)),
            "full TUI label keeps text_primary (no accent recolor)"
        );
    }

    // ── new() ──────────────────────────────────────────────────────────

    #[test]
    fn new_initializes_vectors_correctly() {
        let q1 = make_question("Pick one?", &["A", "B", "C"], false);
        let q2 = make_question("Pick many?", &["X", "Y"], true);
        let state = QuestionViewState::new(
            "tc-1".into(),
            vec![q1, q2],
            StashedPrompt {
                text: "stashed".into(),
                cursor: 0,
                images: Vec::new(),
                chip_elements: Vec::new(),
                image_counter: 0,
                image_undo_stash: Vec::new(),
            },
        );

        assert_eq!(state.questions.len(), 2);
        assert_eq!(state.selections.len(), 2);
        assert_eq!(state.per_question_cursor.len(), 2);
        assert_eq!(state.per_question_scroll.len(), 2);
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.stashed_prompt.text, "stashed");

        // Single-choice initialized to None
        assert!(matches!(
            state.selections[0],
            QuestionSelection::Single(None)
        ));
        // Multi-choice initialized to empty set
        assert!(matches!(
            state.selections[1],
            QuestionSelection::Multi(ref s) if s.is_empty()
        ));

        // Cursors all start at 0
        assert!(state.per_question_cursor.iter().all(|&c| c == 0));
        assert!(state.per_question_scroll.iter().all(|&s| s == 0));
    }

    // ── toggle_option ──────────────────────────────────────────────────

    #[test]
    fn toggle_option_multi_toggles_in_out() {
        let q = make_question("Pick?", &["A", "B", "C"], true);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        // Toggle on
        state.toggle_option(0, 1);
        assert_eq!(state.selected_labels(0), vec!["B"]);

        // Toggle another on
        state.toggle_option(0, 0);
        let mut labels = state.selected_labels(0);
        labels.sort();
        assert_eq!(labels, vec!["A", "B"]);

        // Toggle first one off
        state.toggle_option(0, 1);
        assert_eq!(state.selected_labels(0), vec!["A"]);
    }

    // ── select_option ──────────────────────────────────────────────────

    #[test]
    fn select_option_single_replaces_previous() {
        let q = make_question("Pick?", &["A", "B", "C"], false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        state.select_option(0, 0);
        assert_eq!(state.selected_labels(0), vec!["A"]);

        state.select_option(0, 2);
        assert_eq!(state.selected_labels(0), vec!["C"]);
    }

    // ── selected_labels ────────────────────────────────────────────────

    #[test]
    fn selected_labels_mixed_selections() {
        let q1 = make_question("Single?", &["X", "Y"], false);
        let q2 = make_question("Multi?", &["P", "Q", "R"], true);
        let mut state = QuestionViewState::new("tc".into(), vec![q1, q2], StashedPrompt::default());

        state.select_option(0, 1); // Y
        state.toggle_option(1, 0); // P
        state.toggle_option(1, 2); // R

        assert_eq!(state.selected_labels(0), vec!["Y"]);
        let mut multi = state.selected_labels(1);
        multi.sort();
        assert_eq!(multi, vec!["P", "R"]);
    }

    // ── next_question / prev_question ──────────────────────────────────

    #[test]
    fn question_cycling_clamps_at_boundaries() {
        let qs = vec![
            make_question("Q1?", &["A"], false),
            make_question("Q2?", &["B"], false),
            make_question("Q3?", &["C"], false),
        ];
        let mut state = QuestionViewState::new("tc".into(), qs, StashedPrompt::default());

        assert_eq!(state.active_tab, 0);
        state.next_question();
        assert_eq!(state.active_tab, 1);
        state.next_question();
        assert_eq!(state.active_tab, 2);
        state.next_question();
        assert_eq!(state.active_tab, 2); // clamped at end

        state.prev_question();
        assert_eq!(state.active_tab, 1);
        state.prev_question();
        assert_eq!(state.active_tab, 0);
        state.prev_question();
        assert_eq!(state.active_tab, 0); // clamped at start
    }

    // ── compute_max_label_w ────────────────────────────────────────────

    #[test]
    fn compute_max_label_w_caps_long_labels_at_60_percent() {
        let options = vec![
            QuestionOption {
                label: "A very long label that is way too wide for half the width here".into(),
                description: String::new(),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "Medium label".into(),
                description: String::new(),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "Short".into(),
                description: String::new(),
                preview: None,
                id: None,
            },
        ];
        // content_w=80 gives cap 48. The longest label is 63, so it is capped at 48.
        assert_eq!(compute_max_label_w(&options, 80), 48);
    }

    #[test]
    fn compute_max_label_w_uses_longest_when_all_fit() {
        let options = vec![
            QuestionOption {
                label: "Hello".into(),
                description: String::new(),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "World!".into(),
                description: String::new(),
                preview: None,
                id: None,
            },
        ];
        // content_w=80 gives cap 48. Both fit; the longest is "World!" at 6.
        assert_eq!(compute_max_label_w(&options, 80), 6);
    }

    #[test]
    fn compute_max_label_w_never_zero_when_all_labels_long() {
        let options = vec![
            QuestionOption {
                label: "Lorem ipsum dolor sit amet consectetur adipiscing!".into(),
                description: "Lorem ipsum dolor sit ame".into(),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "Lorem ipsum dolor sit amet elit sed tempor".into(),
                description: "Lorem ipsum dolor sit amet elit s".into(),
                preview: None,
                id: None,
            },
        ];
        // content_w=100 gives cap 60. The longest label is 50, so it fits and the column is 50.
        assert_eq!(compute_max_label_w(&options, 100), 50);
    }

    #[test]
    fn row_with_all_long_labels_still_shows_label_and_description() {
        let opt = QuestionOption {
            label: "Lorem ipsum dolor sit amet consectetur adipiscing!".into(),
            description: "Lorem ipsum dolor sit amet, consectetur adipiscing.".into(),
            preview: None,
            id: None,
        };
        let content_w = 100usize;
        let max_label_w = compute_max_label_w(std::slice::from_ref(&opt), content_w);
        assert!(max_label_w > 0);

        let theme = Theme::default();
        let lines = build_single_option_lines(
            0,
            &opt,
            false,
            false,
            max_label_w,
            6,
            content_w,
            theme.bg_light,
            None,
            &theme,
            false,
        );
        let text: String = lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect();
        assert!(
            text.contains("Lorem ipsum dolor sit amet consectetur adipiscing!"),
            "the label must be shown, got: {text:?}"
        );
        assert!(
            text.contains("Lorem ipsum dolor sit amet, consectetur"),
            "the description must be shown, got: {text:?}"
        );
    }

    #[test]
    fn focused_stacked_description_wraps_at_prefix_indent_not_label_column() {
        let opt = QuestionOption {
            label: "This is an intentionally very long option label designed to test how the \
                    question view handles label visibility when the text greatly exceeds the cap"
                .into(),
            description: "This description exists purely to stress test the option description \
                          rendering in the question view. It should be long enough to force \
                          wrapping across multiple lines on most terminal widths."
                .into(),
            preview: None,
            id: None,
        };
        let content_w = 100usize;
        let prefix_w = 6usize;
        let max_label_w = compute_max_label_w(std::slice::from_ref(&opt), content_w);
        assert!(normalize_label(&opt.label).width() > max_label_w);

        let theme = Theme::default();
        let lines = build_single_option_lines(
            0,
            &opt,
            false,
            false,
            max_label_w,
            prefix_w,
            content_w,
            theme.bg_light,
            None,
            &theme,
            true,
        );

        let label_line_count =
            wrap_label_chunks(&normalize_label(&opt.label), content_w - prefix_w).len();
        let desc_lines = &lines[label_line_count..];
        assert!(!desc_lines.is_empty());
        let mut texts = Vec::new();
        for line in desc_lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let leading = text.len() - text.trim_start().len();
            assert_eq!(
                leading, prefix_w,
                "stacked description must be indented at prefix_w, not the label column: {text:?}"
            );
            texts.push(text);
        }
        assert!(
            texts
                .iter()
                .any(|t| t.trim_end().len() > prefix_w + max_label_w),
            "stacked description should use the full row width, got: {texts:?}"
        );

        let heights = option_visual_height(&opt, content_w, prefix_w, max_label_w);
        assert_eq!(heights as usize, lines.len());
    }

    // ── is_on_freeform_row ─────────────────────────────────────────────

    #[test]
    fn is_on_freeform_row_returns_true_at_end() {
        let q = make_question("Pick?", &["A", "B"], false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        // Cursor at 0 is on option A, not freeform
        assert!(!state.is_on_freeform_row());

        // Cursor at 2 with options.len() == 2, so this is the freeform row
        state.set_cursor(2);
        assert!(state.is_on_freeform_row());
    }

    // ── cursor / set_cursor ────────────────────────────────────────────

    #[test]
    fn set_cursor_clamps_to_valid_range() {
        let q = make_question("Pick?", &["A", "B"], false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        // total_items = 3 (A, B, freeform), so max cursor = 2
        state.set_cursor(100);
        assert_eq!(state.cursor(), 2);

        state.set_cursor(0);
        assert_eq!(state.cursor(), 0);
    }

    // ── total_items ────────────────────────────────────────────────────

    #[test]
    fn total_items_counts_options_plus_freeform() {
        let q = make_question("Pick?", &["A", "B", "C"], false);
        let state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());
        assert_eq!(state.total_items(0), 4); // 3 options + 1 freeform
    }

    // ── no_freeform ────────────────────────────────────────────────────

    /// `no_freeform` questions (e.g. the SuperGrok upsell) have no "Other" row, so activating freeform input must be impossible.
    /// Focus stays in Navigation and nothing gets marked selected.
    /// Regression test for the upsell modal letting the user type after clicking under the last option.
    #[test]
    fn activate_freeform_input_is_noop_when_no_freeform() {
        let q = make_question("Pick?", &["A", "B"], false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default())
            .with_no_freeform();
        state.selections[0] = QuestionSelection::Single(Some(1));

        let text = state.activate_freeform_input();

        assert_eq!(text, "");
        assert_eq!(state.focus, QuestionFocus::Navigation);
        assert!(!state.per_question_freeform_selected[0]);
        assert!(
            matches!(state.selections[0], QuestionSelection::Single(Some(1))),
            "option selection must survive"
        );
    }

    /// The panel height for a `no_freeform` question must not reserve the (never rendered) freeform row.
    /// That dead row was clickable and activated freeform input on the upsell modal.
    #[test]
    fn question_view_height_excludes_freeform_row_when_no_freeform() {
        let q = make_question("Pick?", &["A", "B", "C"], false);
        let mut with_freeform =
            QuestionViewState::new("tc".into(), vec![q.clone()], StashedPrompt::default());
        let mut without_freeform =
            QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default())
                .with_no_freeform();

        let h_with = question_view_height(&mut with_freeform, 50, 80);
        let h_without = question_view_height(&mut without_freeform, 50, 80);
        // The 33% cap can truncate this panel, so only assert that no dead row is reserved.
        assert!(
            h_without <= h_with,
            "no_freeform panel must not reserve a freeform row: {h_without} vs {h_with}"
        );

        // Fullscreen path is uncapped, so the drop is exact.
        with_freeform.fullscreen = true;
        without_freeform.fullscreen = true;
        let h_with = question_view_height(&mut with_freeform, 50, 80);
        let h_without = question_view_height(&mut without_freeform, 50, 80);
        assert_eq!(h_with, h_without + STICKY_FREEFORM_ROWS);
    }

    // ── option_visual_height ───────────────────────────────────────────

    #[test]
    fn option_visual_height_stacks_the_description() {
        let opt = QuestionOption {
            label: "Short".into(),
            description: "A description".into(),
            preview: None,
            id: None,
        };
        // The label and its description are always on separate rows, focus or no focus, so the
        // height never depends on whether the card owns the keyboard.
        assert_eq!(option_visual_height(&opt, 30, 6, 5), 2);
    }

    #[test]
    fn option_visual_height_wraps_a_long_description() {
        let opt = QuestionOption {
            label: "Short".into(),
            description: "A description that is longer than the available width".into(),
            preview: None,
            id: None,
        };
        // content_w=30 is the text column the description wraps at
        let h = option_visual_height(&opt, 30, 6, 5);
        assert!(h >= 3, "expected >= 3, got {h}");
    }

    // ── chrome_height ──────────────────────────────────────────────────

    #[test]
    fn split_question_label_desc_no_break() {
        let (label, desc) = split_question_label_desc("Which database engine?");
        assert_eq!(label, "Which database engine?");
        assert_eq!(desc, "");
    }

    #[test]
    fn split_question_label_desc_with_break() {
        let (label, desc) =
            split_question_label_desc("Which database?\n\nPick the engine for the backend.");
        assert_eq!(label, "Which database?");
        assert_eq!(desc, "Pick the engine for the backend.");
    }

    #[test]
    fn chrome_height_short_question() {
        // Short question, no description: the label sits straight on the option list, so only
        // the single blank row that opens the list is counted:
        // vpad(1) + card header(3) + label(1) + gap(1) = 6.
        let q = make_question("Which database engine?", &["A"], false);
        assert_eq!(
            chrome_height(
                &q,
                80,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            6
        );
    }

    #[test]
    fn chrome_height_option_less_question_drops_the_label_gap() {
        // Nothing under the label to separate it from, so the gap goes: vpad(1) + card header(3) + label(1) + gap(1) = 6. This is the bare `/feedback` card.
        let q = make_question("How can we improve Grok Build?", &[], false);
        assert_eq!(
            chrome_height(
                &q,
                80,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            6
        );
    }

    #[test]
    fn chrome_height_with_description() {
        // Question with paragraph break: label + gap + description + gap.
        let q = make_question(
            "Which database?\n\nChoose the primary data store for the backend service.",
            &["A"],
            false,
        );
        let desc_part = "Choose the primary data store for the backend service.";
        // vpad(1) + card header(3) + label(1) + gap(1) + desc lines + gap(1)
        let desc_lines = desc_part.len().div_ceil(80).max(1) as u16; // 1 line at width 80
        assert_eq!(
            chrome_height(
                &q,
                80,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            1 + CARD_HEADER_ROWS + 1 + 1 + desc_lines + 1
        );
    }

    #[test]
    fn chrome_height_wraps_long_question() {
        // 60-char question at width 40 wraps to 2 lines: vpad(1) + card header(3) + label(2) + gap(1) = 7.
        let q = make_question(
            "Which database engine should we use for the backend service?",
            &["A"],
            false,
        );
        assert_eq!(
            chrome_height(
                &q,
                40,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            7
        );
        // Same question at width 80 fits on 1 line: 6.
        assert_eq!(
            chrome_height(
                &q,
                80,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            6
        );
    }

    #[test]
    fn chrome_height_wraps_extra_long_question() {
        // 150-char question wraps across multiple lines depending on width.
        //
        // At width 75 (typical terminal with chrome):
        //   ┃  Given the requirements for high availability, horizontal
        //   ┃  scaling, and strict ACID compliance, which database engine
        //   ┃  and replication topology should we adopt for the user
        //   ┃  accounts microservice?
        //   ┃
        //   ┃  1 [ ] PostgreSQL   ...
        //
        // At width 40 (narrow):
        //   ┃  Given the requirements for high
        //   ┃  availability, horizontal scaling,
        //   ┃  and strict ACID compliance, which
        //   ┃  database engine and replication
        //   ┃  topology should we adopt for the
        //   ┃  user accounts microservice?
        //   ┃
        //   ┃  1 [ ] PostgreSQL   ...
        let q = make_question(
            "Given the requirements for high availability, horizontal scaling, \
             and strict ACID compliance, which database engine and replication \
             topology should we adopt for the user accounts microservice?",
            &["PostgreSQL", "CockroachDB", "TiDB"],
            false,
        );
        // Word-wrap at width 75: 3 lines, so vpad(1) + card header(3) + label(3) + gap(1) = 8
        assert_eq!(
            chrome_height(
                &q,
                75,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            8
        );
        // Word-wrap at width 40: 6 lines (word boundaries prevent mid-word splits), so vpad(1) + card header(3) + label(6) + gap(1) = 11
        assert_eq!(
            chrome_height(
                &q,
                40,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            11
        );
        // Word-wrap at width 200: 1 line, so vpad(1) + card header(3) + label(1) + gap(1) = 6
        assert_eq!(
            chrome_height(
                &q,
                200,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            6
        );
    }

    #[test]
    fn chrome_height_with_preview() {
        // Short question + preview: vpad(1) + card header(3) + label(1) + gap(1) + preview_gap(1) + preview(1) + gap(1) = 9.
        let q = make_question("Which database?", &["A"], false);
        let preview = "commit abc123: fix the bug";
        assert_eq!(
            chrome_height(
                &q,
                80,
                Some(preview),
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            9
        );
    }

    #[test]
    fn chrome_height_with_long_preview() {
        // Preview that wraps across 2 lines at width 40.
        let q = make_question("Confirm?", &["A"], false);
        let preview = "fix(auth): resolve token refresh race condition in middleware";
        // Word-wrap at width 40: 2 lines, so vpad(1) + card header(3) + label(1) + gap(1) + preview_gap(1) + preview(2) + gap(1) = 10
        assert_eq!(
            chrome_height(
                &q,
                40,
                Some(preview),
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            10
        );
    }

    #[test]
    fn chrome_height_with_multiline_preview() {
        // Multi-line preview: each \n-separated line is wrapped independently.
        let q = make_question("Confirm?", &["A"], false);
        let preview =
            "fix(auth): token refresh\n\nResolves the race condition\nin the middleware layer";
        // .lines() yields 4 segments (including one empty line).
        // word_wrap_line returns 1 line for each, so 4 preview lines total.
        // vpad(1) + card header(3) + label(1) + gap(1) + preview_gap(1) + preview(4) + gap(1) = 12
        assert_eq!(
            chrome_height(
                &q,
                80,
                Some(preview),
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            12
        );
    }

    #[test]
    fn chrome_height_with_empty_preview() {
        // Empty preview should be the same as None.
        let q = make_question("Pick?", &["A"], false);
        assert_eq!(
            chrome_height(
                &q,
                80,
                Some(""),
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
            chrome_height(
                &q,
                80,
                None,
                false,
                DEFAULT_MAX_CHROME_DESC_LINES,
                DEFAULT_MAX_CHROME_PREVIEW_LINES
            ),
        );
    }

    // ── focused_preview ──────────────────────────────────────────────

    #[test]
    fn focused_preview_returns_preview_when_on_option() {
        let q = Question {
            question: "Pick?".into(),
            options: vec![
                QuestionOption {
                    label: "A".into(),
                    description: "desc A".into(),
                    preview: Some("preview content".into()),
                    id: None,
                },
                QuestionOption {
                    label: "B".into(),
                    description: "desc B".into(),
                    preview: None,
                    id: None,
                },
            ],
            multi_select: Some(false),
            id: None,
        };
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        // Cursor at 0: option A has preview
        assert_eq!(state.focused_preview(), Some("preview content"));

        // Cursor at 1: option B has no preview
        state.set_cursor(1);
        assert_eq!(state.focused_preview(), None);

        // Cursor at 2: freeform row, no preview
        state.set_cursor(2);
        assert_eq!(state.focused_preview(), None);
    }

    // ── toggle on Single ───────────────────────────────────────────────

    #[test]
    fn toggle_option_single_deselects_when_same() {
        let q = make_question("Pick?", &["A", "B"], false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());

        state.toggle_option(0, 1);
        assert_eq!(state.selected_labels(0), vec!["B"]);

        // Toggling the same option deselects it
        state.toggle_option(0, 1);
        assert!(state.selected_labels(0).is_empty());
    }

    #[test]
    fn item_index_at_visual_line_accounts_for_wrapped_rows() {
        let q = Question {
            question: "Pick one".into(),
            options: vec![
                QuestionOption {
                    label: "Alpha".into(),
                    description: "one two three four five six seven eight nine ten eleven".into(),
                    preview: None,
                    id: None,
                },
                QuestionOption {
                    label: "Beta".into(),
                    description: "short desc".into(),
                    preview: None,
                    id: None,
                },
            ],
            multi_select: Some(true),
            id: None,
        };

        let content_w = 20;
        let cursor = 0; // focus first option so it gets full height
        let heights = option_heights(&q, content_w, cursor);
        assert!(heights[0] > 1);

        for line in 0..heights[0] {
            assert_eq!(item_index_at_visual_line(&q, content_w, line, cursor), 0);
        }
        assert_eq!(
            item_index_at_visual_line(&q, content_w, heights[0], cursor),
            1
        );
    }

    #[test]
    fn clamp_scroll_limits_offset_to_viewport() {
        let q = make_question("Pick?", &["A", "B", "C", "D"], true);
        let mut state =
            QuestionViewState::new("tc".into(), vec![q.clone()], StashedPrompt::default());
        state.per_question_scroll[0] = 100;

        let visible_h = 2;
        let content_w = 80;
        let expected_max =
            total_options_height(&q, content_w, state.cursor()).saturating_sub(visible_h);
        state.clamp_scroll(visible_h, content_w);

        assert_eq!(state.per_question_scroll[0], expected_max);
    }

    // ── truncation cap tests ───────────────────────────────────────────

    #[test]
    fn chrome_height_caps_long_description() {
        // 10-line description using CommonMark hard breaks (`  \n`) so each logical line renders as its own visual line
        // A bare `\n` between text lines is a soft break and collapses to a space
        let q = make_question(
            "Q?\n\nline1  \nline2  \nline3  \nline4  \nline5  \nline6  \nline7  \nline8  \nline9  \nline10",
            &["A"],
            false,
        );
        let uncapped = chrome_height(&q, 80, None, true, 5, 6);
        let capped = chrome_height(&q, 80, None, false, 5, 6);
        assert!(
            uncapped > capped,
            "fullscreen ({uncapped}) should exceed capped ({capped})",
        );
        // capped: vpad(1) + card header(3) + label(1) + gap(1) + desc(5) + gap(1) = 12
        assert_eq!(capped, 12);
    }

    #[test]
    fn chrome_height_caps_long_preview() {
        // 8-line preview at preview_cap=3 should be capped.
        let q = make_question("Pick?", &["A"], false);
        let preview = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8";
        let uncapped = chrome_height(&q, 80, Some(preview), true, 5, 3);
        let capped = chrome_height(&q, 80, Some(preview), false, 5, 3);
        assert!(
            uncapped > capped,
            "fullscreen ({uncapped}) should exceed capped ({capped})",
        );
        // capped: vpad(1) + card header(3) + label(1) + gap(1) + preview_gap(1) + preview(3) + gap(1) = 11
        assert_eq!(capped, 11);
    }

    #[test]
    fn chrome_height_preview_cap_zero_no_gap() {
        // When preview_cap=0 and !fullscreen, preview contributes 0 lines and no gap
        // This matches render_question_chrome, which guards the gap on capped_count > 0
        let q = make_question("Pick?", &["A"], false);
        let preview = "some preview text";
        let with_preview = chrome_height(&q, 80, Some(preview), false, 5, 0);
        let without = chrome_height(&q, 80, None, false, 5, 0);
        assert_eq!(with_preview, without);
    }

    #[test]
    fn chrome_height_desc_cap_one() {
        // Edge case: desc_cap=1 should still show exactly 1 description line.
        let q = make_question("Q?\n\nline1\nline2\nline3", &["A"], false);
        let h = chrome_height(&q, 80, None, false, 1, 6);
        // vpad(1) + card header(3) + label(1) + gap(1) + desc(1) + gap(1) = 8
        assert_eq!(h, 8);
    }

    // ── question_view_height / minimum visible option rows ─────────────

    /// Helper: build a QuestionViewState for height tests.
    fn make_state_for_height(
        question_text: &str,
        labels: &[&str],
        fullscreen: bool,
    ) -> QuestionViewState {
        let q = make_question(question_text, labels, false);
        let mut state = QuestionViewState::new("tc".into(), vec![q], StashedPrompt::default());
        state.fullscreen = fullscreen;
        state
    }

    #[test]
    fn question_view_height_large_terminal_uses_static_caps() {
        // On a big terminal (80 rows), static caps work and at least MIN_VISIBLE_OPTION_ROWS options are visible
        let mut state = make_state_for_height(
            "Which database?",
            &["PostgreSQL", "MySQL", "SQLite", "CockroachDB", "TiDB"],
            false,
        );
        let content_w = 75;
        let h = question_view_height(&mut state, 80, content_w);

        let chrome_h = chrome_height(
            &state.questions[0],
            content_w,
            state.focused_preview(),
            false,
            state.cached_desc_cap,
            state.cached_preview_cap,
        );
        let visible_h = h.saturating_sub(chrome_h);
        assert!(
            visible_h >= MIN_VISIBLE_OPTION_ROWS,
            "visible_h={visible_h} < MIN_VISIBLE_OPTION_ROWS={MIN_VISIBLE_OPTION_ROWS}",
        );
        // Static caps should be unchanged.
        assert_eq!(state.cached_desc_cap, DEFAULT_MAX_CHROME_DESC_LINES);
        assert_eq!(state.cached_preview_cap, DEFAULT_MAX_CHROME_PREVIEW_LINES);
    }

    #[test]
    fn question_view_height_small_terminal_reduces_caps() {
        // On a small terminal (24 rows) the card's fixed chrome raises the panel's floor rather than
        // starving the option list, so the promised rows stay visible and the caps survive.
        let mut state = make_state_for_height(
            "Which database?\n\nline1\nline2\nline3\nline4\nline5\nline6\nline7\nline8",
            &["PostgreSQL", "MySQL", "SQLite", "CockroachDB", "TiDB"],
            false,
        );
        let content_w = 75;
        let h = question_view_height(&mut state, 24, content_w);

        let chrome_h = chrome_height(
            &state.questions[0],
            content_w,
            state.focused_preview(),
            false,
            state.cached_desc_cap,
            state.cached_preview_cap,
        );
        let visible_h = h.saturating_sub(chrome_h);
        assert!(
            visible_h >= MIN_VISIBLE_OPTION_ROWS,
            "visible_h={visible_h} < MIN_VISIBLE_OPTION_ROWS={MIN_VISIBLE_OPTION_ROWS} on 24-row terminal",
        );
        assert_eq!(
            state.cached_desc_cap, DEFAULT_MAX_CHROME_DESC_LINES,
            "a terminal that can hold the card's floor must not trim the caps"
        );

        // Shorter than the card's floor: the floor wins over the rows held back for the
        // transcript, and the description cap is what pays for the promised option rows.
        let mut tight = make_state_for_height(
            "Which database?

line1
line2
line3
line4
line5
line6
line7
line8",
            &["PostgreSQL", "MySQL", "SQLite", "CockroachDB", "TiDB"],
            false,
        );
        let tight_h = question_view_height(&mut tight, 12, content_w);
        let tight_chrome = chrome_height(
            &tight.questions[0],
            content_w,
            tight.focused_preview(),
            false,
            tight.cached_desc_cap,
            tight.cached_preview_cap,
        );
        assert!(
            tight_h.saturating_sub(tight_chrome) >= MIN_VISIBLE_OPTION_ROWS,
            "even a 12-row terminal owes the option list its rows: h={tight_h} chrome={tight_chrome}",
        );
        assert!(
            tight_h <= 12,
            "the card must still fit the terminal it was given: h={tight_h}",
        );
    }

    #[test]
    fn question_view_height_fits_every_option_on_a_normal_terminal() {
        // The regression this guards: the card used to be capped at a third of the screen, so a
        // five-option question showed one option and the rest had to be scrolled to find.
        let mut state = make_state_for_height(
            "Which database engine?",
            &["PostgreSQL", "MySQL", "SQLite", "CockroachDB", "TiDB"],
            false,
        );
        let content_w = 75;
        let h = question_view_height(&mut state, 40, content_w);
        let needed = chrome_height(
            &state.questions[0],
            content_w,
            state.focused_preview(),
            false,
            state.cached_desc_cap,
            state.cached_preview_cap,
        ) + total_options_height(&state.questions[0], content_w, state.cursor())
            + CARD_BOTTOM_ROWS;
        assert!(
            h >= needed,
            "a 40-row terminal must show all five options: h={h} needed={needed}"
        );
    }


    #[test]
    fn question_view_height_fullscreen_uses_max_caps() {
        let mut state = make_state_for_height(
            "Which database?\n\nline1\nline2\nline3",
            &["A", "B", "C"],
            true,
        );
        let _ = question_view_height(&mut state, 80, 75);
        assert_eq!(state.cached_desc_cap, u16::MAX);
        assert_eq!(state.cached_preview_cap, u16::MAX);
    }

    // ── option height ─────────────────────────────────────────────────

    #[test]
    fn option_height_shows_every_description_line() {
        let opt = QuestionOption {
            label: "Opt".into(),
            description: "line1  \nline2  \nline3  \nline4  \nline5  \nline6".into(),
            preview: None,
            id: None,
        };
        let content_w = 40;
        let prefix_w = 6;
        let max_label_w = 5;
        // Focus no longer changes the height: a description is always shown in full, because a
        // card the user is not driving still has to be readable.
        let h = option_visual_height(&opt, content_w, prefix_w, max_label_w);
        assert!(
            h >= 6,
            "every description line should be shown, got {h}",
        );
    }

    #[test]
    fn build_flat_option_lines_count_matches_option_heights_sum() {
        // Verify that the number of lines produced by build_flat_option_lines equals the sum of option_heights (consistency check)
        let q = Question {
            question: "Pick one".into(),
            options: vec![
                QuestionOption {
                    label: "Alpha".into(),
                    description: "short".into(),
                    preview: None,
                    id: None,
                },
                QuestionOption {
                    label: "Beta".into(),
                    description: "a longer description that should wrap at narrow width and produce multiple lines of text".into(),
                    preview: None,
                    id: None,
                },
                QuestionOption {
                    label: "Gamma".into(),
                    description: "desc".into(),
                    preview: None,
                    id: None,
                },
            ],
            multi_select: Some(false),
            id: None,
        };
        let content_w = 30;
        let cursor = 1; // focus Beta
        let theme = Theme::default();
        let sel = QuestionSelection::Single(None);

        let lines = build_flat_option_lines(
            &q, content_w, cursor, None, &sel, &theme, true, // show freeform
            "", false, true, // panel focused
        );
        let heights = option_heights(&q, content_w, cursor);
        let expected_total: u16 = heights.iter().sum();
        assert_eq!(
            lines.len(),
            expected_total as usize,
            "flat lines ({}) should equal sum of option_heights ({expected_total})",
            lines.len(),
        );
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn single_option_lines(desc: &str, content_w: usize, focused: bool) -> Vec<Line<'static>> {
        let q = Question {
            question: "Pick".into(),
            options: vec![QuestionOption {
                label: "Yes".into(),
                description: desc.into(),
                preview: None,
                id: None,
            }],
            multi_select: Some(false),
            id: None,
        };
        let theme = Theme::default();
        let sel = QuestionSelection::Single(None);
        let cursor = if focused { 0 } else { 1 };
        build_flat_option_lines(
            &q, content_w, cursor, None, &sel, &theme, false, "", false, true,
        )
    }

    /// Join the rendered rows into one string with runs of spaces collapsed, so an assertion can
    /// talk about the text the user reads rather than the column the wrap happened to break at.
    fn squash(lines: &[Line<'static>]) -> String {
        let joined: String = lines.iter().map(line_text).collect();
        joined.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn unfocused_long_description_wraps_instead_of_collapsing() {
        let lines = single_option_lines(
            "a very long single line description that will not fit on the row and must be \
             wrapped rather than truncated",
            40,
            false,
        );
        // A card the user is not driving still has to be readable, so the description wraps onto
        // further rows instead of being cut to one line with an ellipsis.
        assert!(
            lines.len() >= 3,
            "the description must wrap, got {} rows: {:?}",
            lines.len(),
            lines.iter().map(line_text).collect::<Vec<_>>(),
        );
        let text: String = lines.iter().map(line_text).collect();
        assert!(
            !text.contains('\u{2026}'),
            "a wrapped description must not be ellipsized: {text:?}",
        );
        assert_eq!(
            squash(&lines),
            "1 (○) Yes a very long single line description that will not fit on the row and must \
             be wrapped rather than truncated",
            "the whole description must survive the wrap",
        );
    }

    #[test]
    fn unfocused_multiline_description_keeps_every_line() {
        let lines = single_option_lines("short  \nthen a second line of content", 60, false);
        // The hard line break in the description is honoured: label row plus both description rows.
        assert_eq!(lines.len(), 3, "label row plus two description rows");
        assert_eq!(squash(&lines), "1 (○) Yes short then a second line of content");
    }

    #[test]
    fn unfocused_short_description_has_no_ellipsis() {
        let lines = single_option_lines("tiny", 60, false);
        assert_eq!(lines.len(), 2, "label row plus one description row");
        let text = line_text(&lines[1]);
        assert!(text.contains("tiny"));
        assert!(
            !text.contains('\u{2026}'),
            "short fully-shown description should not get an ellipsis: {text:?}",
        );
    }

    #[test]
    fn focused_long_description_expands_to_multiple_lines() {
        let lines = single_option_lines("line1  \nline2  \nline3  \nline4", 40, true);
        assert!(
            lines.len() >= 4,
            "focused option should show all lines, got {}",
            lines.len(),
        );
    }

    #[test]
    fn no_navigate_and_expand_hint_in_rendered_lines() {
        let lines = single_option_lines("l1  \nl2  \nl3  \nl4  \nl5", 40, false);
        for line in &lines {
            let text = line_text(line);
            assert!(
                !text.contains("navigate & expand"),
                "removed hint should not appear: {text:?}",
            );
        }
    }

    #[test]
    fn flat_lines_match_heights_with_overflowing_focused_label() {
        let q = Question {
            question: "Pick".into(),
            options: vec![
                QuestionOption {
                    label: "A really long label that overflows the aligned column width".into(),
                    description: "line1  \nline2  \nline3".into(),
                    preview: None,
                    id: None,
                },
                QuestionOption {
                    label: "Short".into(),
                    description: "desc".into(),
                    preview: None,
                    id: None,
                },
            ],
            multi_select: Some(false),
            id: None,
        };
        let content_w = 30;
        let cursor = 0;
        let theme = Theme::default();
        let sel = QuestionSelection::Single(None);
        let lines = build_flat_option_lines(
            &q, content_w, cursor, None, &sel, &theme, false, "", false, true,
        );
        let heights = option_heights(&q, content_w, cursor);
        let expected: u16 = heights[..q.options.len()].iter().sum();
        assert_eq!(lines.len(), expected as usize);
    }

    /// The plain text of buffer row `y`, so a frame assertion can talk about what is on screen.
    fn buffer_row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// A card with two questions, so the header counter and the `下一题` button have something to show.
    fn two_question_state() -> QuestionViewState {
        QuestionViewState::new(
            "tc-two".into(),
            vec![
                make_question("First question?", &["A", "B"], false),
                make_question("Second question?", &["C", "D"], false),
            ],
            StashedPrompt::default(),
        )
    }

    #[test]
    fn card_frame_wraps_the_panel() {
        let theme = Theme::default();
        let state = two_question_state();
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        let _ = render_question_view(&mut buf, area, &state, None, &theme, true);

        // The panel's first row is padding; the card opens under it.
        let top = buffer_row_text(&buf, 1);
        assert!(
            top.contains(crate::glyphs::box_top_left()),
            "card top rule missing: {top:?}"
        );
        assert!(
            top.contains(crate::glyphs::box_top_right()),
            "card top rule missing: {top:?}"
        );

        let header = buffer_row_text(&buf, 2);
        assert!(
            header.contains(crate::glyphs::diamond_filled()),
            "header icon missing: {header:?}"
        );
        assert!(header.contains("Question"), "header title missing: {header:?}");
        assert!(header.contains("1/2"), "question counter missing: {header:?}");
        assert!(
            header.contains(crate::glyphs::minimize_icon()),
            "minimize control missing: {header:?}"
        );

        // The card's closing rule is the panel's last row.
        let bottom = buffer_row_text(&buf, 19);
        assert!(
            bottom.contains(crate::glyphs::box_bottom_left()),
            "card bottom rule missing: {bottom:?}"
        );
        assert!(
            bottom.contains(crate::glyphs::box_bottom_right()),
            "card bottom rule missing: {bottom:?}"
        );

        // A cell boundary opens with a tee on both borders.
        let tee_row = (3u16..19)
            .find(|y| buffer_row_text(&buf, *y).contains(crate::glyphs::box_left_tee()));
        assert!(tee_row.is_some(), "no divider row between the cells");
    }

    /// A minimized card is a single row: the counter, the state, and the way back.
    #[test]
    fn minimized_card_collapses_to_a_single_row() {
        let theme = Theme::default();
        let mut state = two_question_state();
        state.minimized = true;

        assert_eq!(question_view_height(&mut state, 40, 52), 1);

        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let result = render_question_view(&mut buf, area, &state, None, &theme, true);
        assert_eq!(
            result.options_start_y, area.y,
            "a collapsed card has no option region"
        );

        let row = buffer_row_text(&buf, 0);
        assert!(row.contains("minimized"), "collapsed row must say so: {row:?}");
        assert!(row.contains("1/2"), "collapsed row keeps the counter: {row:?}");
        assert!(
            !row.contains(crate::glyphs::box_top_left()),
            "a collapsed card draws no frame: {row:?}"
        );
    }

    /// Text dump of a rendered card, for eyeballing the layout:
    /// `cargo test -p xai-grok-pager --lib print_card_layout -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_card_layout() {
        let theme = Theme::default();
        let questions = vec![
            make_question(
                "Which database engine should we adopt?\n\nPick the primary store for the user accounts service.",
                &["PostgreSQL", "MySQL", "SQLite", "CockroachDB"],
                true,
            ),
            make_question("And the cache?", &["Redis", "Memcached"], false),
        ];
        for (label, minimized) in [("expanded", false), ("minimized", true)] {
            let mut state =
                QuestionViewState::new("tc".into(), questions.clone(), StashedPrompt::default());
            state.minimized = minimized;
            let width = 76u16;
            let content_w = width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
            let height = question_view_height(&mut state, 30, content_w).max(1);
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            let _ = render_question_view(&mut buf, area, &state, None, &theme, true);
            println!("\n=== {label} (height={height}, min_visible={}) ===", 3);
            for y in 0..height {
                println!("{:>2}|{}|", y, buffer_row_text(&buf, y));
            }
        }
    }
}
