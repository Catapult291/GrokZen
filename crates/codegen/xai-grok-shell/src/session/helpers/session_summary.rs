//! Session title generation via LLM tool call.

use xai_grok_sampler::SamplerConfig;
use xai_grok_sampling_types::ApiBackend;

use crate::sampling::{
    Client as OaiCompatClient, ConversationItem, ConversationRequest, ConversationToolChoice,
    SamplingClient, ToolSpec,
};
use crate::session::helpers::chat::floor_char_boundary;

/// Upper bound on the user text that feeds title generation.
/// Titles only need the opening, and this keeps the request well under the model prompt limit.
const TITLE_SOURCE_MAX_BYTES: usize = 8_000;

const SESSION_TITLE_SYSTEM_PROMPT: &str = r#"You are tasked with generating the session title. The user is asking almost always software engineering related questions on their codebase.
We describe the session title below
# Session Title
A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler.

You will be given the user query below encapsulated in <user_query></user_query>.

Write the title in the same natural language as the user query. If the user query contains Chinese, the session_title MUST be concise Chinese and MUST NOT be translated into English. For mixed-language queries, follow the predominant natural language while preserving code identifiers, commands, and product names.

Just generate the session_title and nothing else"#;

/// Hard byte cap guarding runaway title output; the instruction already targets 5-10 words.
/// Applied on a char boundary, so a multibyte title is capped a little shorter, which is fine for a safety bound.
const TITLE_MAX_BYTES: usize = 80;

/// Hard cap on fallback title length in Unicode scalar values.
/// Whitespace-word capping alone leaves CJK prompts almost intact (few spaces), so a Chinese first prompt used to become the entire title.
/// 50 keeps a normal 10-word English title intact (`"one two … ten"` is 49 chars) while still cutting a long CJK first prompt.
const TITLE_FALLBACK_MAX_CHARS: usize = 50;

/// An explicit sampler route a backend pins its titles to, instead of the configured default.
#[derive(Clone)]
pub struct DirectSessionTitleRoute {
    base_url: String,
    model: String,
    api_key: String,
}

impl DirectSessionTitleRoute {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        DirectSessionTitleRoute {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
        }
    }
}

/// Builds the title client for a daemon pinned to a direct Grok model endpoint. The route is
/// authoritative: its credential never falls through to the configured public endpoints.
pub fn build_direct_session_title_client(
    direct: DirectSessionTitleRoute,
    client_version: Option<String>,
) -> crate::sampling::Result<(SamplingClient, String)> {
    let sampling_config = direct_session_title_sampling_config(direct, client_version);
    let model = sampling_config.model.clone();
    let client = SamplingClient::new(sampling_config)?;
    Ok((client, model))
}

fn direct_session_title_sampling_config(
    direct: DirectSessionTitleRoute,
    client_version: Option<String>,
) -> SamplerConfig {
    SamplerConfig {
        api_key: Some(direct.api_key),
        base_url: direct.base_url,
        model: direct.model,
        api_backend: ApiBackend::Responses,
        context_window: 200_000,
        client_version,
        ..SamplerConfig::default()
    }
}

#[derive(serde::Deserialize)]
struct SessionTitle {
    session_title: String,
}

/// Remove `<system-reminder>…</system-reminder>` blocks from `text`.
/// They are system-injected context (e.g. the `/goal` setup reminder), not the user's words, so they must not drive the session title.
fn strip_system_reminder_blocks(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(before) = rest.get(..start) else {
            break;
        };
        out.push_str(before);
        let Some(after_open) = rest.get(start + OPEN.len()..) else {
            return out.trim().to_string();
        };
        // An unterminated reminder drops the remainder; it is system text
        let Some(end) = after_open.find(CLOSE) else {
            return out.trim().to_string();
        };
        let Some(next) = after_open.get(end + CLOSE.len()..) else {
            return out.trim().to_string();
        };
        rest = next;
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Text the session title is derived from: strip system reminders and skill XML markup, then cap to the first few KB.
/// Stripping runs before the cap so a leading reminder larger than the cap is still removed.
/// Callers that retain a prompt for later titling keep this, not the raw text.
pub fn title_source_text(user_message: &str) -> String {
    let without_reminders = strip_system_reminder_blocks(user_message);
    let base = if without_reminders.is_empty() {
        user_message
    } else {
        &without_reminders
    };
    let mut display =
        xai_grok_tools::implementations::skills::skill::extract_skill_display_text(base)
            .unwrap_or_else(|| base.to_string());
    display.truncate(floor_char_boundary(&display, TITLE_SOURCE_MAX_BYTES));
    display
}

/// The deterministic first-ten-words fallback shared by every initial-title path.
/// Also hard-capped by character count so space-delimited languages keep the word rule while CJK (almost no spaces) cannot spill the whole prompt into the title.
pub fn title_fallback_from_user_text(user_message: &str) -> String {
    let text = title_source_text(user_message);
    let mut s = text
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() {
        return "New session".to_string();
    }
    if s.chars().count() > TITLE_FALLBACK_MAX_CHARS {
        s = s.chars().take(TITLE_FALLBACK_MAX_CHARS).collect();
        s = s.trim_end().to_string();
    }
    s
}

pub(super) fn contains_han(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{20000}'..='\u{2FA1F}'
        )
    })
}

/// Keep generated titles non-empty and enforce the Chinese-language contract.
/// Other languages rely on the model instruction because this crate does not
/// carry a general-purpose language detector.
fn validated_generated_title(source: &str, generated: &str) -> Option<String> {
    let generated = generated.trim();
    if generated.is_empty() || (contains_han(source) && !contains_han(generated)) {
        return None;
    }
    Some(generated.to_string())
}

/// Generate the initial session title from the first user message, for the fast first-prompt path ([`crate::session::summary::SummaryGenerator`]).
/// The title is later re-generated from the whole conversation when the session settles idle and once more at session end,
/// until a manual `/rename` pins it.
pub async fn generate_session_summary(
    user_message: String,
    client: OaiCompatClient,
    model: &str,
) -> String {
    let clean_message = title_source_text(&user_message);
    let request = ConversationRequest::from_items(vec![
        ConversationItem::system(SESSION_TITLE_SYSTEM_PROMPT),
        ConversationItem::user(format!(
            r#"<user_query>
{}
</user_query>"#,
            clean_message
        )),
    ])
    .with_model(model)
    .with_tools(vec![ToolSpec {
        name: "session_title".to_owned(),
        description: Some(
            "Generate a concise session_title in the same natural language as the user query"
                .to_owned(),
        ),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["session_title"],
            "properties": {
                "session_title": {
                    "type": "string",
                    "description": "Final 5-10 word descriptive title in the same language as the user query. A Chinese query requires a concise Chinese title. Super info dense, no filler."
                }
            },
            "additionalProperties": false
        }),
    }])
    .with_max_output_tokens(100)
    .with_temperature(1.0)
    .with_tool_choice(ConversationToolChoice::Function("session_title".to_owned()));

    match client.conversation_collect(request).await {
        Ok(response) => {
            if let Some(a) = response.assistant()
                && let Some(tool_call) = a.tool_calls.first()
                && let Ok(result) = serde_json::from_str::<SessionTitle>(&tool_call.arguments)
            {
                if let Some(title) =
                    validated_generated_title(&clean_message, &result.session_title)
                {
                    return title;
                }
                tracing::warn!(
                    model = %model,
                    "session title generation returned an empty or wrong-language title; falling back to user text"
                );
            }
            tracing::debug!(
                model = %model,
                "session title generation: response did not contain a session_title tool call"
            );
        }
        Err(e) => {
            tracing::warn!(
                model = %model,
                error = %e,
                "session title generation failed, falling back to truncated user text"
            );
        }
    }
    title_fallback_from_user_text(&clean_message)
}

/// Instruction turn appended to a conversation snapshot to refresh the auto title.
/// Like the recap / turn-summary side-calls, all directions live in one reminder-wrapped turn.
/// The model sees the whole conversation, so the title reflects the real topic rather than a possibly-useless first prompt.
pub(crate) fn title_refresh_instruction(tag: &str) -> String {
    format!(
        "<{tag}>Generate a session title for the conversation above. It should be a short and \
         distinctive 5-10 word descriptive title capturing what this session is actually about \
         (the main task or topic), based on the WHOLE conversation — not just the first message. \
         Write the title in the same natural language as the conversation. If the conversation \
         contains Chinese, the title MUST be concise Chinese and MUST NOT be translated into \
         English. For mixed-language conversations, follow the predominant natural language \
         while preserving code identifiers, commands, and product names. Super info dense, no \
         filler. User-role messages wrapped in reminder tags like this one \
         are injected context, not the user.\n\n\
         Output ONLY the title: plain text, no quotes, no labels, no markdown. Do NOT call any \
         tools — respond with plain text only.</{tag}>"
    )
}

/// Clean a refreshed title into a one-line string.
/// Applies the recap normalization (whitespace collapse, stray label/quote stripping) plus the [`TITLE_MAX_BYTES`] cap.
pub(crate) fn clean_title_text(raw: &str) -> String {
    let mut out = crate::session::helpers::session_recap::clean_recap_text(raw);
    if out.len() > TITLE_MAX_BYTES {
        let cut = floor_char_boundary(&out, TITLE_MAX_BYTES);
        out.truncate(cut);
        out = out.trim_end().to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        DirectSessionTitleRoute, SESSION_TITLE_SYSTEM_PROMPT, TITLE_SOURCE_MAX_BYTES,
        clean_title_text, direct_session_title_sampling_config, strip_system_reminder_blocks,
        title_fallback_from_user_text, title_refresh_instruction, title_source_text,
        validated_generated_title,
    };

    #[test]
    fn direct_title_route_builds_its_own_sampler_config() {
        let config = direct_session_title_sampling_config(
            DirectSessionTitleRoute::new("http://127.0.0.1:4242/v1", "local-model", "direct-key"),
            Some("test-version".to_owned()),
        );

        assert_eq!("http://127.0.0.1:4242/v1", config.base_url);
        assert_eq!("local-model", config.model);
        assert_eq!(Some("direct-key"), config.api_key.as_deref());
    }

    #[test]
    fn title_refresh_instruction_wraps_tag_and_asks_for_whole_conversation() {
        let text = title_refresh_instruction("system-reminder");
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
        assert!(text.contains("WHOLE conversation"));
        assert!(text.contains("5-10 word"));
        assert!(text.contains("contains Chinese"));
        assert!(text.contains("MUST be concise Chinese"));
    }

    #[test]
    fn clean_title_normalizes_and_caps() {
        // Collapses whitespace and strips surrounding quotes.
        assert_eq!(
            clean_title_text("\"Fix the auth  bug\""),
            "Fix the auth bug"
        );
        let capped = clean_title_text(&"word ".repeat(50));
        assert!(capped.len() <= super::TITLE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_caps_oversized_input() {
        let big = "word ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_cap_is_utf8_safe() {
        // 3-byte chars straddle the byte cap; must truncate on a boundary, not panic.
        let big = "あ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_strips_leading_reminder_larger_than_cap() {
        // A leading reminder bigger than the cap must still be stripped, so the title derives from the objective rather than reminder text
        let reminder = "x".repeat(TITLE_SOURCE_MAX_BYTES * 2);
        let input =
            format!("<system-reminder>\n{reminder}\n</system-reminder>\n\nbuild a mario game");
        let out = title_source_text(&input);
        assert_eq!(out, "build a mario game");
    }

    #[test]
    fn strip_removes_goal_setup_reminder_leaving_objective() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nlots of rules\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game";
        assert_eq!(
            strip_system_reminder_blocks(input),
            "build a mario platformer game"
        );
    }

    #[test]
    fn strip_handles_unterminated_reminder() {
        assert_eq!(
            strip_system_reminder_blocks("<system-reminder>\nrules with no close tag"),
            ""
        );
    }

    #[test]
    fn strip_no_reminder_is_identity() {
        assert_eq!(
            strip_system_reminder_blocks("fix the auth bug"),
            "fix the auth bug"
        );
    }

    /// Regression: a `/goal <objective>` first turn must title off the objective, not the injected `<system-reminder>` setup block.
    #[test]
    fn fallback_titles_off_goal_objective_not_reminder() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game in html";
        assert_eq!(
            title_fallback_from_user_text(input),
            "build a mario platformer game in html"
        );
    }

    #[test]
    fn fallback_trims_to_words() {
        assert_eq!(
            title_fallback_from_user_text(
                "one two three four five six seven eight nine ten eleven"
            ),
            "one two three four five six seven eight nine ten"
        );
    }

    #[test]
    fn fallback_caps_cjk_without_spaces() {
        use super::{TITLE_FALLBACK_MAX_CHARS, contains_han};
        // Regression: a long Chinese first prompt used to become the entire title because
        // whitespace-word capping alone almost never truncates CJK.
        let long = format!(
            "https://in.mesl.cloud/#/dashboard ，这是一个机场网站，我得先到官网打开10分钟订阅链接开关[Image #1] ，才能在代理软件导入、更新链接，怎么让这个开关保持打开状态？{}",
            "继续补充很长的上下文内容".repeat(8)
        );
        let out = title_fallback_from_user_text(&long);
        assert!(
            out.chars().count() <= TITLE_FALLBACK_MAX_CHARS,
            "fallback chars {} > cap {}: {out}",
            out.chars().count(),
            TITLE_FALLBACK_MAX_CHARS
        );
        assert!(contains_han(&out));
        assert!(out.starts_with("https://in.mesl.cloud/#/dashboard"));
        // Full prompt must not survive as the title.
        assert_ne!(out, title_source_text(&long));
    }

    #[test]
    fn fallback_empty_input_is_new_session() {
        assert_eq!(title_fallback_from_user_text(""), "New session");
    }

    #[test]
    fn fallback_new_session_when_whitespace_only() {
        assert_eq!(title_fallback_from_user_text("   \n\t"), "New session");
    }

    #[test]
    fn fallback_strips_skill_xml_with_args() {
        let input = "<command-name>implement</command-name>\n\
                      <command-message>/implement</command-message>\n\
                      <command-args>fix the rendering bug</command-args>";
        assert_eq!(
            title_fallback_from_user_text(input),
            "/implement fix the rendering bug",
        );
    }

    #[test]
    fn fallback_strips_skill_xml_no_args() {
        let input = "<command-name>deploy</command-name>\n\
                      <command-message>/deploy</command-message>";
        assert_eq!(title_fallback_from_user_text(input), "/deploy");
    }

    #[test]
    fn fallback_plain_text_unaffected() {
        assert_eq!(
            title_fallback_from_user_text("fix the auth bug in login.rs"),
            "fix the auth bug in login.rs",
        );
    }

    #[test]
    fn prompt_requires_titles_to_follow_the_user_language() {
        assert!(SESSION_TITLE_SYSTEM_PROMPT.contains("same natural language as the user query"));
        assert!(SESSION_TITLE_SYSTEM_PROMPT.contains("contains Chinese"));
        assert!(SESSION_TITLE_SYSTEM_PROMPT.contains("MUST be concise Chinese"));
    }

    #[test]
    fn chinese_source_rejects_english_generated_title() {
        assert_eq!(
            validated_generated_title("测试各种工具链是否正常", "Tool Chain Functionality Test"),
            None
        );
        assert_eq!(
            validated_generated_title("测试各种工具链是否正常", "测试工具链功能"),
            Some("测试工具链功能".to_string())
        );
    }

    #[test]
    fn generated_title_is_trimmed_and_non_chinese_languages_remain_supported() {
        assert_eq!(
            validated_generated_title("fix the auth bug", "  Fix authentication bug  "),
            Some("Fix authentication bug".to_string())
        );
        assert_eq!(validated_generated_title("fix the auth bug", "   "), None);
    }
}
