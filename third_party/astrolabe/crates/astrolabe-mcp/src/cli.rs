//! Startup flags for the MCP binary.
//!
//! Kept tiny on purpose: path + `--context`. No clap.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::context::{resolve_context_name, ClientContext};

pub const USAGE: &str = "\
Astrolabe MCP server (stdio)

用法: astrolabe [选项] [ROOT]

ROOT 优先级：位置参数 → ASTROLABE_ROOT → .（从 cwd 探测）
  . / 省略   向上找最近的 .git 或 .serena/project.yml
  其它路径   只 canonicalize，不向上走

选项:
  --context NAME   客户端上下文（default, claude-code, cursor, codex, readonly）
                   或指向自定义 .yml 的路径
                   也可用 ASTROLABE_CONTEXT；CLI 优先
  -V, --version    显示版本号
  -h, --help       显示本说明

子命令（Serena 式防漂移 hooks 与提示词输出）:
  hooks remind --client=NAME   PreToolUse hook：stdin 读 hook payload，计数连续
                               grep/read 滥用，超阈值输出 deny 决策 JSON。
                               client: claude-code/codebuddy/vscode/codex/grok/
                                       kimicode/zcode/cursor/opencode
                               （grokbuild/grok-build 亦映射为 grok；
                                kimi-code/kimi_code/kimi→kimicode，z-code/zai→zcode，
                                cursor-agent/cursor_agent/cursor-cli→cursor）
                               可选：--deny-text-search[=true|false]
                                     硬禁用 Grep/Glob/shell 文本搜索（首次即 deny）
                               可选：--context=NAME  加载上下文 YAML 的 deny_text_search
                               优先级：CLI > ASTROLABE_DENY_TEXT_SEARCH >
                                       context YAML > 默认 off
  hooks cleanup                SessionEnd hook：按 stdin session_id 清理状态目录
  hooks gc                     清理超过 24h（ASTROLABE_HOOK_GC_HOURS 可调）未活动的
                               会话状态目录；remind 也会每小时机会式执行一次
                               （适用于 ZCode 等无 SessionEnd 事件的宿主）
  print-cc-system-prompt-override
                               输出 Claude Code --system-prompt 整体替换文本
  setup                        探测本机 AI 客户端并写入 MCP 配置（实现在 npm 包；
                               cargo 安装请运行: npx -y @zlnap/astrolabe setup）
";

#[derive(Debug)]
pub enum Launch {
    Help,
    Version,
    HooksRemind {
        client: String,
        /// CLI `--deny-text-search` / `--deny-text-search=true|false`；None = 未指定
        deny_text_search: Option<bool>,
        /// Optional `--context=NAME` for YAML `deny_text_search`
        context: Option<String>,
    },
    HooksCleanup,
    /// `hooks gc`：清理过期会话状态目录
    HooksGc,
    PrintCcSystemPromptOverride,
    /// npm-shipped interactive MCP client setup (cargo binary prints a hint).
    Setup {
        args: Vec<OsString>,
    },
    Run {
        requested_root: PathBuf,
        context: ClientContext,
    },
}

/// Parse argv (including argv[0]). Reads `ASTROLABE_CONTEXT` / `ASTROLABE_ROOT`
/// when the corresponding CLI piece is omitted.
pub fn parse_launch<I, S>(args: I) -> anyhow::Result<Launch>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    parse_launch_from(
        args,
        std::env::var("ASTROLABE_CONTEXT").ok(),
        std::env::var_os("ASTROLABE_ROOT"),
    )
}

pub fn parse_launch_from<I, S>(
    args: I,
    env_context: Option<String>,
    env_root: Option<OsString>,
) -> anyhow::Result<Launch>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut iter = args.into_iter().map(Into::into);
    let _argv0 = iter.next();
    let rest: Vec<OsString> = iter.collect();

    // 子命令分支：第一个位置参数是保留字时不再按 ROOT 解析。
    if let Some(first) = rest.first() {
        match first.to_string_lossy().as_ref() {
            "hooks" => return parse_hooks_subcommand(&rest[1..]),
            "print-cc-system-prompt-override" => {
                if rest.len() > 1 {
                    anyhow::bail!(
                        "unexpected extra argument: {}\n{USAGE}",
                        rest[1].to_string_lossy()
                    );
                }
                return Ok(Launch::PrintCcSystemPromptOverride);
            }
            "setup" => {
                return Ok(Launch::Setup {
                    args: rest[1..].to_vec(),
                });
            }
            _ => {}
        }
    }

    let mut context_cli: Option<String> = None;
    let mut positional: Option<PathBuf> = None;
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy();
        if arg == "--help" || arg == "-h" {
            return Ok(Launch::Help);
        }
        if arg == "--version" || arg == "-V" {
            return Ok(Launch::Version);
        }
        if arg == "--context" {
            i += 1;
            let value = rest
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("--context requires a name"))?;
            let value = value.to_string_lossy();
            if value.trim().is_empty() || value.starts_with('-') {
                anyhow::bail!("--context requires a name");
            }
            context_cli = Some(value.into_owned());
            i += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--context=") {
            if value.trim().is_empty() {
                anyhow::bail!("--context requires a name");
            }
            context_cli = Some(value.to_string());
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            anyhow::bail!("unknown flag: {arg}\n{USAGE}");
        }
        if positional.is_some() {
            anyhow::bail!("unexpected extra argument: {arg}");
        }
        positional = Some(PathBuf::from(&rest[i]));
        i += 1;
    }

    let name = resolve_context_name(context_cli.as_deref(), env_context.as_deref());
    let context = ClientContext::load(name)?;

    // Empty ASTROLABE_ROOT (set-but-blank) is treated like unset → `.`.
    let requested_root = positional.unwrap_or_else(|| {
        env_root
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    });

    Ok(Launch::Run {
        requested_root,
        context,
    })
}

/// `astrolabe hooks <remind|cleanup|gc> [--client=NAME]`：hook 协议入口不索引仓库，
/// 由 main 直接分发到 hooks 模块。
fn parse_hooks_subcommand(rest: &[OsString]) -> anyhow::Result<Launch> {
    // `hooks -h` / `hooks remind --help` should show usage, not "unknown …".
    for arg in rest {
        let s = arg.to_string_lossy();
        if s == "--help" || s == "-h" {
            return Ok(Launch::Help);
        }
    }
    let Some(sub) = rest.first() else {
        anyhow::bail!("hooks requires a subcommand: remind | cleanup | gc\n{USAGE}");
    };
    match sub.to_string_lossy().as_ref() {
        "remind" => {
            let mut client: Option<String> = None;
            let mut deny_text_search: Option<bool> = None;
            let mut context: Option<String> = None;
            for arg in &rest[1..] {
                let arg = arg.to_string_lossy();
                if let Some(value) = arg.strip_prefix("--client=") {
                    if value.trim().is_empty() {
                        anyhow::bail!("--client requires a name\n{USAGE}");
                    }
                    client = Some(value.to_string());
                } else if arg == "--client" {
                    anyhow::bail!("--client requires =NAME in hook commands\n{USAGE}");
                } else if arg == "--deny-text-search" {
                    deny_text_search = Some(true);
                } else if let Some(value) = arg.strip_prefix("--deny-text-search=") {
                    let v = value.trim().to_ascii_lowercase();
                    match v.as_str() {
                        "1" | "true" | "yes" | "on" => deny_text_search = Some(true),
                        "0" | "false" | "no" | "off" => deny_text_search = Some(false),
                        _ => anyhow::bail!(
                            "--deny-text-search expects true/false (got {value})\n{USAGE}"
                        ),
                    }
                } else if let Some(value) = arg.strip_prefix("--context=") {
                    if value.trim().is_empty() {
                        anyhow::bail!("--context requires a name\n{USAGE}");
                    }
                    context = Some(value.to_string());
                } else if arg == "--context" {
                    anyhow::bail!("--context requires =NAME in hook commands\n{USAGE}");
                } else {
                    anyhow::bail!("unknown hooks remind argument: {arg}\n{USAGE}");
                }
            }
            // 未指定 client 时按 claude-code 形态输出（最常见宿主，格式是其超集）。
            Ok(Launch::HooksRemind {
                client: client.unwrap_or_else(|| "claude-code".to_string()),
                deny_text_search,
                context,
            })
        }
        "cleanup" => {
            if rest.len() > 1 {
                anyhow::bail!(
                    "unexpected extra argument: {}\n{USAGE}",
                    rest[1].to_string_lossy()
                );
            }
            Ok(Launch::HooksCleanup)
        }
        "gc" => {
            if rest.len() > 1 {
                anyhow::bail!(
                    "unexpected extra argument: {}\n{USAGE}",
                    rest[1].to_string_lossy()
                );
            }
            Ok(Launch::HooksGc)
        }
        other => anyhow::bail!("unknown hooks subcommand: {other}\n{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Launch {
        parse_launch_from(args.iter().copied(), None, None).expect("parse")
    }

    #[test]
    fn help_flag() {
        assert!(matches!(parse(&["astrolabe", "--help"]), Launch::Help));
        assert!(matches!(parse(&["astrolabe", "-h"]), Launch::Help));
    }

    #[test]
    fn version_flag() {
        assert!(matches!(
            parse(&["astrolabe", "--version"]),
            Launch::Version
        ));
        assert!(matches!(parse(&["astrolabe", "-V"]), Launch::Version));
    }

    #[test]
    fn hooks_subcommands() {
        match parse(&["astrolabe", "hooks", "remind", "--client=codex"]) {
            Launch::HooksRemind {
                client,
                deny_text_search,
                context,
            } => {
                assert_eq!(client, "codex");
                assert_eq!(deny_text_search, None);
                assert_eq!(context, None);
            }
            other => panic!("expected HooksRemind, got {other:?}"),
        }
        // 未指定 client 回落 claude-code（输出格式是其超集）
        match parse(&["astrolabe", "hooks", "remind"]) {
            Launch::HooksRemind { client, .. } => assert_eq!(client, "claude-code"),
            other => panic!("expected HooksRemind, got {other:?}"),
        }
        match parse(&[
            "astrolabe",
            "hooks",
            "remind",
            "--client=cursor",
            "--deny-text-search",
            "--context=claude-code",
        ]) {
            Launch::HooksRemind {
                client,
                deny_text_search,
                context,
            } => {
                assert_eq!(client, "cursor");
                assert_eq!(deny_text_search, Some(true));
                assert_eq!(context.as_deref(), Some("claude-code"));
            }
            other => panic!("expected HooksRemind, got {other:?}"),
        }
        match parse(&["astrolabe", "hooks", "remind", "--deny-text-search=false"]) {
            Launch::HooksRemind {
                deny_text_search, ..
            } => assert_eq!(deny_text_search, Some(false)),
            other => panic!("expected HooksRemind, got {other:?}"),
        }
        assert!(matches!(
            parse(&["astrolabe", "hooks", "cleanup"]),
            Launch::HooksCleanup
        ));
        assert!(matches!(
            parse(&["astrolabe", "hooks", "gc"]),
            Launch::HooksGc
        ));
        assert!(parse_launch_from(
            ["astrolabe", "hooks", "gc", "x"].iter().copied(),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn hooks_subcommand_errors() {
        assert!(parse_launch_from(["astrolabe", "hooks"].iter().copied(), None, None).is_err());
        assert!(parse_launch_from(
            ["astrolabe", "hooks", "frobnicate"].iter().copied(),
            None,
            None
        )
        .is_err());
        assert!(parse_launch_from(
            ["astrolabe", "hooks", "remind", "--client"].iter().copied(),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn print_override_subcommand() {
        assert!(matches!(
            parse(&["astrolabe", "print-cc-system-prompt-override"]),
            Launch::PrintCcSystemPromptOverride
        ));
        assert!(parse_launch_from(
            ["astrolabe", "print-cc-system-prompt-override", "extra"]
                .iter()
                .copied(),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn setup_subcommand_forwards_args() {
        match parse(&["astrolabe", "setup", "--list", "--yes"]) {
            Launch::Setup { args } => {
                assert_eq!(
                    args,
                    vec![
                        std::ffi::OsString::from("--list"),
                        std::ffi::OsString::from("--yes")
                    ]
                );
            }
            _ => panic!("expected setup"),
        }
        assert!(matches!(
            parse(&["astrolabe", "setup"]),
            Launch::Setup { args } if args.is_empty()
        ));
    }

    #[test]
    fn context_equals_and_space_forms() {
        match parse(&["astrolabe", "--context=cursor", "."]) {
            Launch::Run {
                context,
                requested_root,
            } => {
                assert_eq!(context.name, "cursor");
                assert_eq!(context.structured_tool_output, Some(true));
                assert_eq!(requested_root, PathBuf::from("."));
            }
            _ => panic!("expected run"),
        }
        match parse(&["astrolabe", "--context", "claude-code", "/tmp/repo"]) {
            Launch::Run {
                context,
                requested_root,
            } => {
                assert_eq!(context.name, "claude-code");
                assert_eq!(context.structured_tool_output, Some(false));
                assert_eq!(requested_root, PathBuf::from("/tmp/repo"));
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn default_context_when_omitted() {
        match parse(&["astrolabe"]) {
            Launch::Run {
                context,
                requested_root,
            } => {
                assert_eq!(context.name, "default");
                assert_eq!(requested_root, PathBuf::from("."));
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn env_context_used_without_cli_flag() {
        let launch = parse_launch_from(["astrolabe"], Some("codex".into()), None).unwrap();
        match launch {
            Launch::Run { context, .. } => assert_eq!(context.name, "codex"),
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn cli_context_wins_over_env() {
        let launch = parse_launch_from(
            ["astrolabe", "--context=cursor"],
            Some("codex".into()),
            None,
        )
        .unwrap();
        match launch {
            Launch::Run { context, .. } => assert_eq!(context.name, "cursor"),
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn unknown_flag_errors() {
        let err = parse_launch_from(["astrolabe", "--nope"], None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown flag"), "{err}");
    }

    #[test]
    fn missing_context_value_errors() {
        let err = parse_launch_from(["astrolabe", "--context"], None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--context requires a name"), "{err}");
    }

    #[test]
    fn empty_env_root_falls_back_to_dot() {
        let launch = parse_launch_from(["astrolabe"], None, Some(OsString::from(""))).unwrap();
        match launch {
            Launch::Run { requested_root, .. } => {
                assert_eq!(requested_root, PathBuf::from("."));
            }
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn empty_client_flag_errors() {
        let err = parse_launch_from(
            ["astrolabe", "hooks", "remind", "--client="]
                .iter()
                .copied(),
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--client requires a name"), "{err}");
        let err = parse_launch_from(
            ["astrolabe", "hooks", "remind", "--client=  "]
                .iter()
                .copied(),
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--client requires a name"), "{err}");
    }

    #[test]
    fn hooks_help_flags() {
        assert!(matches!(parse(&["astrolabe", "hooks", "-h"]), Launch::Help));
        assert!(matches!(
            parse(&["astrolabe", "hooks", "remind", "--help"]),
            Launch::Help
        ));
        assert!(matches!(
            parse(&["astrolabe", "hooks", "--help", "cleanup"]),
            Launch::Help
        ));
    }
}
