//! Display-time folding for raw tool-call XML left in assistant prose by a damaged parameter stream.
//!
//! The markup is model output, never an execution request: tool calls are dispatched from the structured
//! ACP payload, so folding changes only the rendered text. A malformed stream repeats
//! `<tool_call><function=name><parameter=...>` once per retry; showing every copy buries the reply.

/// Markers that start a raw tool-call region. A region normally opens with `<tool_call`; a stream
/// that broke mid-header can leave a bare `<function=` as well.
const START_MARKERS: [&str; 2] = ["<tool_call", "<function="];
const TOOL_CALL_END: &str = "</tool_call>";
const FUNCTION_END: &str = "</function>";
/// Longest first-parameter value kept in the folded summary.
const VALUE_PREVIEW_CHARS: usize = 40;

/// Whether `text` mentions a raw tool-call start marker outside a fenced code block.
#[cfg(test)]
pub(crate) fn contains_raw_tool_call(text: &str) -> bool {
    find_start(text, 0, &fenced_bytes(text)).is_some()
}

/// Whether `chunk` starts a raw tool-call region, including a marker whose bytes straddle the
/// previous chunk boundary. Marker checks only: fence context is decided by [`fold_raw_tool_calls`].
pub(crate) fn mentions_start_marker(probe: &str, chunk: &str) -> bool {
    START_MARKERS.iter().any(|marker| {
        chunk.contains(marker)
            || straddles(probe, chunk, marker)
            || !partial_tail(chunk, marker).is_empty()
    })
}

/// Length of a trailing proper prefix of `marker`, e.g. 8 for `"<tool_ca"`.
fn partial_marker_tail_len(text: &str) -> usize {
    START_MARKERS
        .iter()
        .map(|marker| partial_tail(text, marker).len())
        .max()
        .unwrap_or(0)
}

fn partial_tail<'a>(text: &'a str, marker: &str) -> &'a str {
    let bytes = marker.as_bytes();
    (1..bytes.len())
        .rev()
        .find(|&len| text.ends_with(&marker[..len]))
        .map(|len| &text[text.len() - len..])
        .unwrap_or_default()
}

fn straddles(probe: &str, chunk: &str, marker: &str) -> bool {
    let marker = marker.as_bytes();
    (1..marker.len()).any(|split| {
        probe.as_bytes().ends_with(&marker[..split])
            && chunk.as_bytes().starts_with(&marker[split..])
    })
}

/// Replace every contiguous run of raw tool-call XML with one summary line.
///
/// While `final_chunk` is false an unterminated trailing region is left out of the display, so a
/// streaming block never shows half a tag; the caller folds again once more text arrives or the
/// stream ends. Markers inside fenced code blocks are ordinary content and stay verbatim.
pub(crate) fn fold_raw_tool_calls(raw: &str, final_chunk: bool) -> String {
    let fenced = fenced_bytes(raw);
    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0;
    let mut summary = Summary::default();

    loop {
        let Some(start) = find_start(raw, cursor, &fenced) else {
            if !summary.is_empty() {
                push_summary(&mut out, &mut summary);
            }
            // Hold back a marker prefix split across chunks so half a tag never renders.
            let hold = if final_chunk {
                0
            } else {
                partial_marker_tail_len(&raw[cursor..])
            };
            push_text(&mut out, &raw[cursor..raw.len() - hold]);
            break;
        };

        let gap = &raw[cursor..start];
        if gap.trim().is_empty() {
            // Whitespace between copies of the same blob is dropped; otherwise keep the spacing
            // that separated the blob from the prose before it.
            if summary.is_empty() {
                push_text(&mut out, gap);
            }
        } else {
            if !summary.is_empty() {
                push_summary(&mut out, &mut summary);
            }
            push_text(&mut out, gap);
        }

        let Some(end) = find_end(raw, start, final_chunk) else {
            // Unterminated: hide the whole group until its closing tag or the end of the stream.
            break;
        };
        summary.add(summarize(&raw[start..end]));
        cursor = end;
    }

    out
}

/// One contiguous run of raw tool-call XML, collapsed to a single line.
#[derive(Default)]
struct Summary {
    blocks: usize,
    /// Signature and occurrence count in first-seen order.
    entries: Vec<(String, usize)>,
}

impl Summary {
    fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    fn add(&mut self, signature: Option<String>) {
        self.blocks += 1;
        let Some(signature) = signature else {
            return;
        };
        if let Some((_, count)) = self.entries.iter_mut().find(|(seen, _)| *seen == signature) {
            *count += 1;
        } else {
            self.entries.push((signature, 1));
        }
    }

    fn render(&self) -> String {
        let mut line = format!("tool_call XML ×{}", self.blocks);
        if !self.entries.is_empty() {
            let details = self
                .entries
                .iter()
                .map(|(signature, count)| {
                    if *count > 1 {
                        format!("{signature} ×{count}")
                    } else {
                        signature.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            line.push_str(" · ");
            line.push_str(&details);
        }
        line.push_str(" — not executed");
        inline_code(&line)
    }
}

fn push_summary(out: &mut String, summary: &mut Summary) {
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(&summary.render());
    out.push('\n');
    summary.blocks = 0;
    summary.entries.clear();
}

/// Append prose, keeping a blank line after a summary so the two stay separate paragraphs.
fn push_text(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if out.ends_with('\n') && !out.ends_with("\n\n") && !text.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
}

fn find_start(text: &str, from: usize, fenced: &[bool]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for marker in START_MARKERS {
        let mut search = from;
        while let Some(offset) = text[search..].find(marker) {
            let index = search + offset;
            if !fenced.get(index).copied().unwrap_or(false) {
                best = Some(best.map_or(index, |current: usize| current.min(index)));
                break;
            }
            search = index + marker.len();
        }
    }
    best
}

/// End of the region that starts at `start`, or `None` while it is still unterminated mid-stream.
fn find_end(raw: &str, start: usize, final_chunk: bool) -> Option<usize> {
    let rest = &raw[start..];
    if let Some(offset) = rest.find(TOOL_CALL_END) {
        return Some(start + offset + TOOL_CALL_END.len());
    }
    if let Some(offset) = rest.find(FUNCTION_END) {
        let mut end = start + offset + FUNCTION_END.len();
        if let Some(tail) = raw.get(end..) {
            if let Some(tool_call) = tail.find(TOOL_CALL_END)
                && tail[..tool_call].trim().is_empty()
            {
                end += tool_call + TOOL_CALL_END.len();
            }
        }
        return Some(end);
    }
    final_chunk.then(|| truncated_end(raw, start))
}

/// Where a region that never closes ends: at the next blank line followed by prose, or at the end.
fn truncated_end(raw: &str, start: usize) -> usize {
    let rest = &raw[start..];
    let mut search = 0;
    while let Some(offset) = rest[search..].find("\n\n") {
        let after = search + offset + 2;
        let tail = &rest[after..];
        if tail.starts_with('<') || tail.trim().is_empty() {
            search = after;
            continue;
        }
        return start + after;
    }
    raw.len()
}

/// Tool name plus its first parameter, each whitespace-normalized and bounded.
fn summarize(region: &str) -> Option<String> {
    let name: String = region
        .split_once("function=")?
        .1
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        .collect();
    if name.is_empty() {
        return None;
    }

    let Some((_, after_parameter)) = region.split_once("parameter=") else {
        return Some(name);
    };
    let key: String = after_parameter
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    if key.is_empty() {
        return Some(name);
    }

    let mut signature = format!("{name}({key}");
    if let Some(value) = parameter_value(after_parameter) {
        let value = collapse_whitespace(&value);
        if !value.is_empty() {
            signature.push('=');
            signature.push_str(&truncate(&value, VALUE_PREVIEW_CHARS));
        }
    }
    signature.push(')');
    Some(signature)
}

fn parameter_value(after_name: &str) -> Option<String> {
    let start = after_name.find('>')? + 1;
    let body = &after_name[start..];
    let end = ["</parameter", "<parameter", FUNCTION_END, TOOL_CALL_END]
        .iter()
        .filter_map(|terminator| body.find(terminator))
        .min()
        .unwrap_or(body.len());
    Some(body[..end].to_string())
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Inline code span, so the summary's `*`, `_`, `<` and `|` cannot turn into markdown.
fn inline_code(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    if longest == 0 {
        format!("{fence}{text}{fence}")
    } else {
        format!("{fence} {text} {fence}")
    }
}

/// Mark the bytes that belong to a fenced code block (opening and closing fence lines included).
fn fenced_bytes(text: &str) -> Vec<bool> {
    let mut mask = vec![false; text.len()];
    let mut fence: Option<(u8, usize)> = None;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();
        let bytes = trimmed.as_bytes();
        if indent <= 3 {
            if let Some((ch, len)) = fence {
                let run = bytes.iter().take_while(|b| **b == ch).count();
                if run >= len && trimmed[run..].trim().is_empty() {
                    mask[line_start..line_end].fill(true);
                    fence = None;
                    line_start = line_end;
                    continue;
                }
            } else if let Some(&ch) = bytes.first()
                && (ch == b'`' || ch == b'~')
            {
                let run = bytes.iter().take_while(|b| **b == ch).count();
                if run >= 3 {
                    mask[line_start..line_end].fill(true);
                    fence = Some((ch, run));
                    line_start = line_end;
                    continue;
                }
            }
        }
        if fence.is_some() {
            mask[line_start..line_end].fill(true);
        }
        line_start = line_end;
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETRIED: &str = "\
改用正确的参数继续查。
<tool_call><function=search_code><parameter=glob>*.rs
</parameter><parameter=query>alpha</parameter></function></tool_call>
<tool_call><function=search_code><parameter=glob>*.rs
</parameter><parameter=query>beta</parameter></function></tool_call>
<tool_call><function=search_code><parameter=glob>*.rs
</parameter><parameter=query>gamma</parameter></function></tool_call>
";

    #[test]
    fn repeated_blobs_collapse_to_one_line() {
        let folded = fold_raw_tool_calls(RETRIED, true);
        assert!(!folded.contains("<tool_call"), "{folded}");
        assert!(!folded.contains("alpha"), "{folded}");
        assert_eq!(folded.matches("tool_call XML").count(), 1, "{folded}");
        assert!(folded.contains("tool_call XML ×3"), "{folded}");
        assert!(folded.contains("search_code(glob=*.rs) ×3"), "{folded}");
        assert!(folded.contains("not executed"), "{folded}");
        assert!(folded.starts_with("改用正确的参数继续查。"), "{folded}");
    }

    #[test]
    fn variants_of_one_tool_are_listed_separately() {
        let raw = "<tool_call><function=run_terminal_command><parameter=block_until_ms>0</parameter><parameter=command>a</parameter></function></tool_call>\n\
<tool_call><function=monitor><parameter=persistent>true</parameter></function></tool_call>\n";
        let folded = fold_raw_tool_calls(raw, true);
        assert!(folded.contains("tool_call XML ×2"), "{folded}");
        assert!(
            folded.contains("run_terminal_command(block_until_ms=0)"),
            "{folded}"
        );
        assert!(folded.contains("monitor(persistent=true)"), "{folded}");
    }

    #[test]
    fn unterminated_region_is_hidden_until_the_stream_ends() {
        let raw = "done so far\n<tool_call><function=read_file><parameter=target_file>a.rs";
        let streaming = fold_raw_tool_calls(raw, false);
        assert!(!streaming.contains("<tool_call"), "{streaming}");
        assert!(!streaming.contains("tool_call XML"), "{streaming}");
        assert_eq!(streaming.trim(), "done so far");

        let finished = fold_raw_tool_calls(raw, true);
        assert!(
            finished.contains("read_file(target_file=a.rs)"),
            "{finished}"
        );
        assert!(finished.contains("not executed"), "{finished}");
    }

    #[test]
    fn a_function_closed_without_the_outer_tag_still_folds_mid_stream() {
        let raw = "done\n<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function>";
        let streaming = fold_raw_tool_calls(raw, false);
        assert!(
            streaming.contains("read_file(target_file=a.rs)"),
            "{streaming}"
        );
        assert!(!streaming.contains("<tool_call"), "{streaming}");
    }

    #[test]
    fn marker_split_across_chunks_is_still_folded() {
        let (first, second) = (
            "prose\n<tool_ca",
            "ll><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>",
        );
        assert!(
            mentions_start_marker("", first),
            "partial tail keeps it out of the display"
        );
        assert!(mentions_start_marker("pro <tool", "_call><function=x>"));
        let combined = format!("{first}{second}");
        let before = fold_raw_tool_calls(first, false);
        assert_eq!(before.trim(), "prose");
        let after = fold_raw_tool_calls(&combined, false);
        assert!(after.contains("read_file(target_file=a.rs)"), "{after}");
    }

    #[test]
    fn fenced_examples_stay_verbatim() {
        let raw = "```text\n<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>\n```\n";
        assert_eq!(fold_raw_tool_calls(raw, true), raw);
        assert!(!contains_raw_tool_call(raw));
    }

    #[test]
    fn prose_after_an_unterminated_region_is_kept() {
        let raw = "start <function=read_file><parameter=target_file>a.rs</parameter>\n\nand then the answer.";
        let folded = fold_raw_tool_calls(raw, true);
        assert!(folded.contains("and then the answer."), "{folded}");
    }

    #[test]
    fn summary_escapes_markdown_metacharacters() {
        let raw = "<tool_call><function=search_code><parameter=glob>*.[rs]</parameter></function></tool_call>";
        let folded = fold_raw_tool_calls(raw, true);
        assert!(folded.starts_with('`'), "{folded}");
        assert!(folded.contains("glob=*.[rs]"), "{folded}");
    }

    #[test]
    fn long_values_are_bounded() {
        let value = "x".repeat(200);
        let raw = format!(
            "<tool_call><function=run_terminal_command><parameter=command>{value}</parameter></function></tool_call>"
        );
        let folded = fold_raw_tool_calls(&raw, true);
        assert!(folded.contains('…'), "{folded}");
        assert!(!folded.contains(&value), "{folded}");
    }
}
