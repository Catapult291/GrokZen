use crate::clipboard::{ClipboardDelivery, ClipboardFeedback, CopyDelivery};
use crate::locale::LocaleContext;

/// Localize a route-aware clipboard delivery without translating paths,
/// commands, or other opaque values supplied by the clipboard backend.
pub(crate) fn localized_copy_toast(locale: &LocaleContext, delivery: &CopyDelivery) -> String {
    match delivery {
        CopyDelivery::Clipboard {
            result,
            file: Some(path),
        } if result.delivery == ClipboardDelivery::Unverified => localized_path_toast(
            locale,
            "clipboard.toast.unverified_with_backup",
            "Copy sent — saved to {path}",
            path,
        ),
        CopyDelivery::Clipboard { result, .. } => {
            let (id, english) = feedback_message(result.feedback);
            locale.named_text(id, english).into_owned()
        }
        CopyDelivery::File { path } => localized_path_toast(
            locale,
            "clipboard.toast.file_fallback",
            "Clipboard unreachable — wrote {path}",
            path,
        ),
        CopyDelivery::Failed { clipboard, .. } => {
            let (id, english) = feedback_message(clipboard.feedback);
            locale.named_text(id, english).into_owned()
        }
    }
}

fn localized_path_toast(
    locale: &LocaleContext,
    id: &str,
    english: &str,
    path: &std::path::Path,
) -> String {
    locale
        .named_text(id, english)
        .replace("{path}", &crate::clipboard::display_copy_path(path))
}

fn feedback_message(feedback: ClipboardFeedback) -> (&'static str, &'static str) {
    match feedback {
        ClipboardFeedback::Copied => ("clipboard.toast.copied", "Copied!"),
        ClipboardFeedback::CopiedTmux => (
            "clipboard.toast.copied_tmux",
            "Copied to tmux buffer, paste with prefix + ]",
        ),
        ClipboardFeedback::CopiedOscContainer => (
            "clipboard.toast.copied_osc_container",
            "Copied via OSC 52 from the container.",
        ),
        ClipboardFeedback::CopiedOscRemote => {
            ("clipboard.toast.copied_osc_remote", "Copied via OSC 52.")
        }
        ClipboardFeedback::UnverifiedOscRemote | ClipboardFeedback::UnverifiedOscContainer => (
            "clipboard.toast.unverified",
            "Copy sent. If paste fails, use grok wrap or /minimal.",
        ),
        ClipboardFeedback::VsCodeSshNonAscii => (
            "clipboard.toast.vscode_ssh_non_ascii",
            "Copied. VS Code over SSH may garble non-ASCII; use /minimal if needed.",
        ),
        ClipboardFeedback::FailedRemote | ClipboardFeedback::Failed => (
            "clipboard.toast.failed",
            "Copy failed. Try /doctor or /minimal.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locale::{LocaleSource, ResolvedLocale, UiLocale};

    fn zh_locale() -> LocaleContext {
        LocaleContext::new(ResolvedLocale {
            locale: UiLocale::ZhCn,
            source: LocaleSource::Requirement,
        })
    }

    fn result(
        feedback: ClipboardFeedback,
        delivery: ClipboardDelivery,
    ) -> crate::clipboard::CopyResult {
        crate::clipboard::CopyResult {
            message: "English fallback",
            message_lead: "English lead",
            ticks: 30,
            delivery,
            feedback,
        }
    }

    #[test]
    fn localizes_confirmed_copy_and_keeps_english_fallback() {
        let delivery = CopyDelivery::Clipboard {
            result: result(ClipboardFeedback::Copied, ClipboardDelivery::Confirmed),
            file: None,
        };

        assert_eq!(localized_copy_toast(&zh_locale(), &delivery), "已复制！");
        assert_eq!(
            localized_copy_toast(&LocaleContext::default(), &delivery),
            "Copied!"
        );
    }

    #[test]
    fn localizes_route_messages_and_preserves_backup_path() {
        let backup = std::path::PathBuf::from("C:/tmp/last-copy.txt");
        let unverified = CopyDelivery::Clipboard {
            result: result(
                ClipboardFeedback::UnverifiedOscRemote,
                ClipboardDelivery::Unverified,
            ),
            file: Some(backup.clone()),
        };
        assert_eq!(
            localized_copy_toast(&zh_locale(), &unverified),
            "已发送复制请求——已保存到 C:/tmp/last-copy.txt"
        );

        let file_only = CopyDelivery::File { path: backup };
        assert_eq!(
            localized_copy_toast(&zh_locale(), &file_only),
            "无法访问剪贴板——已写入 C:/tmp/last-copy.txt"
        );
    }
}
