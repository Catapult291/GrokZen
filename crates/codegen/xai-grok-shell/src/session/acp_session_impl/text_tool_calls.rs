//! Recovery for tool calls the model writes into message text instead of the tool-call channel.
//!
//! A damaged parameter stream makes some models re-emit `<tool_call><function=…>` as prose. The
//! harness dispatches structured calls only, so a reply whose whole body is that markup used to end
//! the turn with nothing executed and nothing to read. The turn loop asks the model to re-issue the
//! calls once or twice, then tells the user instead of ending silently.

/// Markers that open a raw tool-call region in assistant text.
const OPENERS: [&str; 2] = ["<tool_call", "<function="];
/// Closing tag for each opener, in the same order.
const CLOSERS: [&str; 2] = ["</tool_call>", "</function>"];

/// Whether `text` is one or more raw tool-call blocks and nothing else.
///
/// Leading and trailing whitespace is ignored, and prose between or after the blocks makes this
/// return false: a reply that also answers the user has no dead end to recover from.
pub(super) fn is_tool_call_only(text: &str) -> bool {
    let mut rest = text.trim();
    let mut blocks = 0;
    while !rest.is_empty() {
        let Some(index) = OPENERS.iter().position(|opener| rest.starts_with(opener)) else {
            return false;
        };
        let Some(end) = rest.find(CLOSERS[index]) else {
            // A stream that broke mid-tag leaves nothing but the partial call; the dead end and the
            // correction are the same.
            return true;
        };
        rest = rest[end + CLOSERS[index].len()..].trim_start();
        blocks += 1;
    }
    blocks > 0
}

#[cfg(test)]
mod tests {
    use super::is_tool_call_only;

    #[test]
    fn a_single_block_with_no_prose_is_detected() {
        assert!(is_tool_call_only(
            "<tool_call><function=search_code><parameter=query>temperature</parameter></function></tool_call>"
        ));
    }

    #[test]
    fn several_blocks_with_whitespace_between_them_are_detected() {
        let text = "\n<tool_call><function=search_code><parameter=aggregate>file\n</parameter>\
                    <parameter=query>temperature</parameter></function></tool_call>\n\
                    <tool_call><function=search_code><parameter=path>.grok/stage-log.md</parameter>\
                    <parameter=query>并发</parameter></function></tool_call>\n";
        assert!(is_tool_call_only(text));
    }

    #[test]
    fn a_header_that_broke_into_a_bare_function_tag_is_detected() {
        assert!(is_tool_call_only(
            "<function=read_file><parameter=target_file>a.rs</parameter></function>"
        ));
    }

    #[test]
    fn prose_before_between_or_after_the_blocks_keeps_the_reply() {
        for text in [
            "Let me look that up.\n<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>",
            "<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>\nI will read it next.",
            "<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>\nand\n<tool_call><function=read_file><parameter=target_file>b.rs</parameter></function></tool_call>",
        ] {
            assert!(!is_tool_call_only(text), "{text}");
        }
    }

    #[test]
    fn a_fenced_example_is_ordinary_prose() {
        let text = "```text\n<tool_call><function=read_file><parameter=target_file>a.rs</parameter></function></tool_call>\n```";
        assert!(!is_tool_call_only(text));
    }

    #[test]
    fn a_truncated_block_still_counts_as_a_call() {
        assert!(is_tool_call_only(
            "<tool_call><function=read_file><parameter=target_file>a.rs"
        ));
    }
}
