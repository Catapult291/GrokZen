//! Onboarding tutorial content (embedded markdown).
//!
//! Short, curated topics shown by the `/tutorial` overlay (strictly opt-in; nothing auto-shows).
//! Deliberately separate from [`crate::docs`] (the full how-to guides): these pages are bite-size intros that point at the guides for depth.

/// A compile-time tutorial topic. All fields are `&'static str`.
#[derive(Debug)]
pub struct TutorialTopic {
    /// Stable identity shared by all locale tables.
    pub id: &'static str,
    /// Row title in the topic list.
    pub title: &'static str,
    /// Short right-column blurb in the topic list.
    pub blurb: &'static str,
    /// Embedded markdown page content.
    pub content: &'static str,
    /// Stable identity of the primary how-to guide opened by `d`.
    pub go_deeper: Option<crate::docs::DocId>,
}

macro_rules! topic_en {
    ($id:literal, $file:literal, $title:literal, $blurb:literal, $go_deeper:expr) => {
        TutorialTopic {
            id: $id,
            title: $title,
            blurb: $blurb,
            content: include_str!(concat!("../docs/tutorial/", $file)),
            go_deeper: $go_deeper,
        }
    };
}

macro_rules! topic_zh {
    ($id:literal, $file:literal, $title:literal, $blurb:literal, $go_deeper:expr) => {
        TutorialTopic {
            id: $id,
            title: $title,
            blurb: $blurb,
            content: include_str!(concat!("../docs/tutorial/zh-CN/", $file)),
            go_deeper: $go_deeper,
        }
    };
}

/// The tutorial topics, in display order. Ordered as a linear flow (the
/// topic screen's `→` advances through them): what carries over from other
/// tools, send a prompt, feed it context, learn the screen, then the
/// bigger features.
pub static TUTORIAL_TOPICS: &[TutorialTopic] = &[
    topic_en!(
        "tutorial.coming-from-another-tool",
        "01-coming-from-another-tool.md",
        "Coming from Claude, Cursor, or Codex?",
        "your settings, rules & skills carry over",
        Some(crate::docs::PROJECT_RULES)
    ),
    topic_en!(
        "tutorial.first-prompt",
        "02-first-prompt.md",
        "Your First Prompt",
        "send, queue, cancel",
        Some(crate::docs::GETTING_STARTED)
    ),
    topic_en!(
        "tutorial.attach-and-paste",
        "03-attach-and-paste.md",
        "Attach Files, Images & Paste",
        "@files, line ranges, screenshots",
        Some(crate::docs::GETTING_STARTED)
    ),
    topic_en!(
        "tutorial.navigation",
        "04-navigation.md",
        "Finding Your Way Around",
        "focus, scrollback, panes",
        Some(crate::docs::KEYBOARD_SHORTCUTS)
    ),
    topic_en!(
        "tutorial.slash-commands",
        "05-slash-commands.md",
        "Slash Commands",
        "/help  /model  /resume  and Ctrl+P",
        Some(crate::docs::SLASH_COMMANDS)
    ),
    topic_en!(
        "tutorial.worktrees",
        "06-worktrees.md",
        "Parallel Work: Worktrees",
        "isolated sessions on one repo",
        Some(crate::docs::SESSIONS)
    ),
    topic_en!(
        "tutorial.plan-and-permissions",
        "07-plan-and-permissions.md",
        "Plan Mode & Permissions",
        "review the approach before it acts",
        Some(crate::docs::PLAN_MODE)
    ),
    topic_en!(
        "tutorial.make-it-yours",
        "08-make-it-yours.md",
        "Make It Yours",
        "just ask: AGENTS.md, memory, themes",
        Some(crate::docs::PROJECT_RULES)
    ),
    topic_en!(
        "tutorial.where-next",
        "09-where-next.md",
        "Where to Go Next",
        "guides, feedback, and good habits",
        None
    ),
];

pub static ZH_CN_TUTORIAL_TOPICS: &[TutorialTopic] = &[
    topic_zh!(
        "tutorial.coming-from-another-tool",
        "01-coming-from-another-tool.md",
        "从 Claude、Cursor 或 Codex 迁移？",
        "设置、规则和技能都可沿用",
        Some(crate::docs::PROJECT_RULES)
    ),
    topic_zh!(
        "tutorial.first-prompt",
        "02-first-prompt.md",
        "你的第一个提示",
        "发送、排队与取消",
        Some(crate::docs::GETTING_STARTED)
    ),
    topic_zh!(
        "tutorial.attach-and-paste",
        "03-attach-and-paste.md",
        "附加文件、图像并粘贴",
        "@文件、行范围和截图",
        Some(crate::docs::GETTING_STARTED)
    ),
    topic_zh!(
        "tutorial.navigation",
        "04-navigation.md",
        "熟悉界面",
        "焦点、回滚区和面板",
        Some(crate::docs::KEYBOARD_SHORTCUTS)
    ),
    topic_zh!(
        "tutorial.slash-commands",
        "05-slash-commands.md",
        "斜杠命令",
        "/help、/model、/resume 和 Ctrl+P",
        Some(crate::docs::SLASH_COMMANDS)
    ),
    topic_zh!(
        "tutorial.worktrees",
        "06-worktrees.md",
        "并行工作：工作树",
        "在同一仓库中隔离会话",
        Some(crate::docs::SESSIONS)
    ),
    topic_zh!(
        "tutorial.plan-and-permissions",
        "07-plan-and-permissions.md",
        "计划模式与权限",
        "执行前先审阅方案",
        Some(crate::docs::PLAN_MODE)
    ),
    topic_zh!(
        "tutorial.make-it-yours",
        "08-make-it-yours.md",
        "自定义 Grok",
        "直接提出需求：AGENTS.md、记忆与主题",
        Some(crate::docs::PROJECT_RULES)
    ),
    topic_zh!(
        "tutorial.where-next",
        "09-where-next.md",
        "下一步",
        "指南、反馈与良好习惯",
        None
    ),
];

pub fn topics_for(locale: crate::locale::UiLocale) -> &'static [TutorialTopic] {
    match locale {
        crate::locale::UiLocale::EnUs => TUTORIAL_TOPICS,
        crate::locale::UiLocale::ZhCn => ZH_CN_TUTORIAL_TOPICS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_valid() {
        for t in TUTORIAL_TOPICS {
            assert!(!t.title.is_empty(), "topic has empty title");
            assert!(!t.blurb.is_empty(), "topic {} has empty blurb", t.title);
            assert!(!t.content.is_empty(), "topic {} is empty", t.title);
            assert!(
                t.content.starts_with('#'),
                "topic {} should start with a markdown header",
                t.title
            );
        }
    }

    #[test]
    fn go_deeper_titles_resolve_to_real_guides() {
        // `d` on a topic page opens this guide; a typo'd title would turn the shortcut into a silent no-op
        for t in TUTORIAL_TOPICS {
            if let Some(id) = t.go_deeper {
                assert!(
                    crate::docs::find_doc_by_id(id).is_some(),
                    "topic {}: go_deeper {:?} matches no how-to guide",
                    t.title,
                    id.as_str(),
                );
            }
        }
    }

    #[test]
    fn topics_have_unique_titles() {
        let mut seen = std::collections::HashSet::new();
        for t in TUTORIAL_TOPICS {
            assert!(seen.insert(t.title), "duplicate topic title: {}", t.title);
        }
    }

    #[test]
    fn topics_stay_bite_size() {
        // The tutorial promises quick reads; keep each page short
        // Bump this limit only after re-checking a page still reads in under a minute
        for t in TUTORIAL_TOPICS {
            let lines = t.content.lines().count();
            assert!(
                lines <= 50,
                "topic {} is {} lines; keep tutorial pages bite-size (≤50)",
                t.title,
                lines
            );
        }
    }

    #[test]
    fn locale_tables_share_ids_and_links() {
        assert_eq!(TUTORIAL_TOPICS.len(), ZH_CN_TUTORIAL_TOPICS.len());
        for (english, chinese) in TUTORIAL_TOPICS.iter().zip(ZH_CN_TUTORIAL_TOPICS) {
            assert_eq!(english.id, chinese.id);
            assert_eq!(english.go_deeper, chinese.go_deeper);
            assert!(chinese.content.starts_with('#'));
            assert!(chinese.content.lines().count() <= 50);
        }
    }
}
