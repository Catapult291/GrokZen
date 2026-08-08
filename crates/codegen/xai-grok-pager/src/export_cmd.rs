use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::acp::meta::NotificationMeta;
use crate::acp::tracker::AcpUpdateTracker;
use crate::locale::LocaleContext;
use crate::scrollback::export::render_blocks_to_markdown;
use crate::scrollback::state::ScrollbackState;

#[derive(Debug, clap::Args, Clone)]
pub struct ExportArgs {
    /// 要导出的会话 ID
    pub session_id: String,
    /// 输出文件路径（默认：stdout）
    pub output: Option<PathBuf>,
    /// 复制到剪贴板，而不是写入 stdout
    #[arg(long, short)]
    pub clipboard: bool,
}

pub fn run(args: ExportArgs) -> Result<()> {
    run_with_locale(args, &LocaleContext::default())
}

pub fn run_with_locale(args: ExportArgs, locale: &LocaleContext) -> Result<()> {
    tracing::info!(session_id = %args.session_id, "export_cmd: starting session export");

    let updates = xai_grok_shell::session::storage::load_updates_for_replay(&args.session_id)?
        .with_context(|| {
            locale
                .named_text(
                    "transcript.export.session_not_found",
                    "Session '{session_id}' not found.",
                )
                .replace("{session_id}", &args.session_id)
        })?;

    let mut tracker = AcpUpdateTracker::new();
    let mut scrollback = ScrollbackState::new();
    let replay_meta = NotificationMeta {
        is_replay: true,
        ..Default::default()
    };

    for update in updates {
        tracker.handle_update(update, &replay_meta, &mut scrollback);
    }

    let blocks: Vec<_> = (0..scrollback.len())
        .filter_map(|i| scrollback.entry(i).map(|e| &e.block))
        .collect();
    let md = render_blocks_to_markdown(blocks);

    if md.is_empty() {
        anyhow::bail!(
            "{}",
            locale
                .named_text(
                    "transcript.export.no_content_for_session",
                    "Session '{session_id}' has no conversation content to export"
                )
                .replace("{session_id}", &args.session_id)
        );
    }

    if let Some(path) = args.output {
        let expanded = PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref());
        if let Some(parent) = expanded.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                locale
                    .named_text(
                        "transcript.export.create_directory_failed",
                        "Failed to create {path}",
                    )
                    .replace("{path}", &parent.display().to_string())
            })?;
        }
        std::fs::write(&expanded, &md).with_context(|| {
            locale
                .named_text("transcript.export.write_failed", "Failed to write {path}")
                .replace("{path}", &expanded.display().to_string())
        })?;
        tracing::info!(
            session_id = %args.session_id,
            path = %expanded.display(),
            bytes = md.len(),
            "export_cmd: wrote transcript to file"
        );
        eprintln!(
            "{}",
            locale
                .named_text(
                    "transcript.export.to_file",
                    "Conversation exported to {path}"
                )
                .replace("{path}", &expanded.display().to_string())
        );
    } else if args.clipboard {
        let _ = crate::clipboard::copy_text(&md);
        let lines = md.lines().count();
        tracing::info!(
            session_id = %args.session_id,
            bytes = md.len(),
            lines,
            "export_cmd: copied transcript to clipboard"
        );
        let stats = locale
            .named_text("transcript.stats.many", "({chars} chars, {lines} lines)")
            .replace("{chars}", &md.len().to_string())
            .replace("{lines}", &lines.to_string());
        eprintln!(
            "{}",
            locale
                .named_text(
                    "transcript.export.clipboard",
                    "Conversation copied to clipboard {stats}"
                )
                .replace("{stats}", &stats)
        );
    } else {
        std::io::stdout().write_all(md.as_bytes())?;
        std::io::stdout().write_all(b"\n")?;
    }

    Ok(())
}
