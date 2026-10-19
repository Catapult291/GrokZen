//! Adjacent-request prefix diagnostics (G5 / H1): the wire `input` region of a request
//! and the `input[:k]` LCP between two adjacent snapshots.
//!
//! Two consumers:
//! - the shell's optional `GROK_DEBUG_PREFIX_LCP` snapshot in the main tool loop
//!   (a prefix break there is a guaranteed cache miss on the next request), and
//! - the prefix-stability tests in `xai-chat-state`.

use crate::ConversationRequest;

/// The wire `input` region of a request (Responses shape), serialized through the public
/// `From` mapping with the reasoning-text type patch applied — the bytes that reach the wire.
///
/// This is the region the server-side KV cache keys on; `tools` / `model` sit outside it
/// and are compared separately by the system+tools region assertions.
pub fn responses_input_json(req: &ConversationRequest) -> serde_json::Value {
    let create: crate::rs::CreateResponse = req.into();
    let mut body =
        serde_json::to_value(&create).expect("CreateResponse serialization cannot fail");
    crate::patch_reasoning_text_types(&mut body);
    body.get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// `input[:k]` LCP of two [`responses_input_json`] snapshots: how many leading items are
/// exactly equal. `base` is the earlier snapshot.
///
/// An append-only continuation returns `base`'s full length; a rewritten system head,
/// a mutated history item, or a reordered input shows up as a smaller value.
pub fn input_prefix_lcp(base: &serde_json::Value, extended: &serde_json::Value) -> usize {
    let (Some(base_items), Some(ext_items)) = (base.as_array(), extended.as_array()) else {
        return 0;
    };
    base_items
        .iter()
        .zip(ext_items.iter())
        .take_while(|(b, e)| b == e)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversationItem, ToolSpec};

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: Some(format!("{name} tool")),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn lcp_is_full_length_when_items_only_append() {
        let base = ConversationRequest::from_items(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ]);
        let mut extended = base.clone();
        extended.push(ConversationItem::assistant("hi"));
        extended.push(ConversationItem::user("more"));

        let base_input = responses_input_json(&base);
        let ext_input = responses_input_json(&extended);
        let base_len = base_input.as_array().map(Vec::len).unwrap_or(0);
        assert_eq!(
            input_prefix_lcp(&base_input, &ext_input),
            base_len,
            "append-only history must keep the full input prefix"
        );
        assert!(
            ext_input.as_array().is_some_and(|i| i.len() > base_len),
            "the extended request must actually be longer"
        );
    }

    #[test]
    fn lcp_stops_at_a_rewritten_system_head() {
        let base = ConversationRequest::from_items(vec![
            ConversationItem::system("original system"),
            ConversationItem::user("hello"),
        ]);
        let extended = ConversationRequest::from_items(vec![
            ConversationItem::system("REWRITTEN system"),
            ConversationItem::user("hello"),
            ConversationItem::assistant("hi"),
        ]);

        let base_input = responses_input_json(&base);
        let ext_input = responses_input_json(&extended);
        assert_eq!(
            input_prefix_lcp(&base_input, &ext_input),
            0,
            "a rewritten system item must show as an immediate prefix break"
        );
    }

    #[test]
    fn lcp_stops_at_a_mutated_history_item() {
        let base = ConversationRequest::from_items(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("first"),
            ConversationItem::assistant("answer"),
        ]);
        let extended = ConversationRequest::from_items(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("first"),
            ConversationItem::assistant("rewritten answer"),
        ]);

        let base_input = responses_input_json(&base);
        let ext_input = responses_input_json(&extended);
        assert_eq!(
            input_prefix_lcp(&base_input, &ext_input),
            2,
            "divergence must be reported at the mutated item, not masked by later appends"
        );
    }

    #[test]
    fn input_snapshot_is_independent_of_tools() {
        let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
        ];
        let with_tools = ConversationRequest::from_items(items.clone()).with_tools(vec![
            tool("read_file"),
            tool("edit_file"),
        ]);
        let without_tools = ConversationRequest::from_items(items);

        assert_eq!(
            responses_input_json(&with_tools),
            responses_input_json(&without_tools),
            "the input region must not change when the tool set does"
        );
    }
}
