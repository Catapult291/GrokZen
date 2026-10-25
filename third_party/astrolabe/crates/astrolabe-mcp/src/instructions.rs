//! Serena-style tiered instructions: connection prompt (L1) + full manual (L2).
//!
//! Astrolabe adopts a three-tier instruction model inspired by Serena:
//! - L1: `connection_instructions` returned in MCP server initialization (connection prompt),
//!   providing a critical bootstrap reminder plus the factual index-root header.
//! - L2: `manual` / `cc_system_prompt_override` for hosts that inject a full instruction
//!   manual (strict doctrine for Claude Code, mild advisory for other clients).
//! - L3: Detailed tool descriptions and schemas implemented by `server.rs`.

use std::path::Path;

/// L1: the MCP `instructions` field. CRITICAL bootstrap + the factual
/// index-root header (中文事实段原样保留).
pub fn connection_instructions(context_name: &str, root: &Path) -> String {
    let _ = context_name;
    format!(
        "CRITICAL: Prefer Astrolabe over Grep/Read for code. Tools: resolve_context, search_code, find_symbol, find_references, goto_definition, get_diagnostics, get_languages. All are read-only: batch independent calls in parallel in one turn.\n\n\
         当前索引根 index_root: {}。结果里的 index_root 即该路径；\
         若与你正在编辑的仓库不符，不要用这些路径读文件。\
         结果末行写“已全列”时即完整，直接据此作答，不必再用 grep 复核或换词重查。\
         统计/计数（每个目录有多少处调用）用 search_code aggregate=top|dir|file，可加 skip_comments、exclude，不必写脚本。任务不明确时可先用 resolve_context。\
         scoped 来自 import 图，syntactic 来自名字匹配。\
         改完代码后若已跑通构建或测试（如 go test、cargo test），就不必再调 get_diagnostics；它只在没法跑构建/测试时用。\
         缺语言服务器时精确工具会注明并给出替代做法，不要为此自行安装（get_languages 可看状态）。\
         其他语言源码文件不入索引：search_code/find_symbol 空结果不覆盖它们，请直接使用 Grep/Read。{}",
        root.display(),
        go_layout_hint(root).map(|h| format!("\n{h}")).unwrap_or_default()
    )
}

/// Go modules that do not sit at the index root. E1 (batch 8): a repo whose
/// only `go.mod` is `src/go.mod` (root holds just `go.work`) made the model
/// run `go test ./...` at the root, which matches no package with or without
/// the `go.work` ("directory prefix . does not contain ..."), then wander for
/// ~10 turns. One line naming the module roots up front avoids that.
/// `None` when the root itself is a module or holds no Go module at all.
fn go_layout_hint(root: &Path) -> Option<String> {
    if root.join("go.mod").is_file() {
        return None;
    }
    let from_work = root.join("go.work").is_file();
    let mut mods: Vec<String> = if from_work {
        std::fs::read_to_string(root.join("go.work"))
            .map(|w| go_work_uses(&w))
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d != "." && root.join(d).join("go.mod").is_file())
            .collect()
    } else {
        scan_go_modules(root)
    };
    if mods.is_empty() {
        return None;
    }
    mods.sort();
    mods.dedup();
    let first = mods[0].clone();
    let mut list = mods
        .iter()
        .take(4)
        .map(|d| format!("{d}/"))
        .collect::<Vec<_>>()
        .join("、");
    if mods.len() > 4 {
        list.push_str(&format!(" 等 {} 个", mods.len()));
    }
    let via = if from_work {
        "（由根目录 go.work use）"
    } else {
        ""
    };
    Some(format!(
        "Go：仓库根没有 go.mod，模块根在 {list}{via}；在根目录跑 `go test ./...` 匹配不到任何包，会直接报错。\
         请进模块目录对改动的包跑，如 `cd {first} && go test ./<包路径>/`。"
    ))
}

/// `use` targets of a go.work, both `use ./a` and `use ( ./a ./b )`, with
/// a leading `./` and trailing `/` stripped.
fn go_work_uses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        let item = if in_block {
            if line.starts_with(')') {
                in_block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix("use") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                in_block = true;
                rest.trim_start_matches('(').trim()
            } else if line.starts_with("use ") || line.starts_with("use\t") {
                rest
            } else {
                continue;
            }
        } else {
            continue;
        };
        let item = item.trim_matches('"').trim();
        if item.is_empty() || item.starts_with('/') {
            continue;
        }
        let d = item.trim_start_matches("./").trim_end_matches('/');
        out.push(if d.is_empty() {
            ".".to_string()
        } else {
            d.to_string()
        });
    }
    out
}

/// Shallow, bounded walk (depth 3, 2000 dirs) for `go.mod` directories,
/// not descending into a module once found nor into trees the go tool or
/// the index ignore.
fn scan_go_modules(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([(String::new(), 0usize)]);
    let mut seen = 0usize;
    while let Some((rel, depth)) = queue.pop_front() {
        seen += 1;
        if seen > 2000 {
            break;
        }
        let dir = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel)
        };
        if !rel.is_empty() && dir.join("go.mod").is_file() {
            out.push(rel);
            continue;
        }
        if depth >= 3 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || name.starts_with('_')
                || matches!(
                    name.as_str(),
                    "vendor" | "node_modules" | "testdata" | "target"
                )
            {
                continue;
            }
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            queue.push_back((child, depth + 1));
        }
    }
    out
}

/// L2: the full manual for hosts that inject it (Claude Code `--system-prompt`, tests).
/// claude-code → the strict doctrine; anything else → the mild default.
pub fn manual(context_name: &str) -> &'static str {
    match context_name {
        "claude-code" => CLAUDE_CODE_MANUAL,
        _ => DEFAULT_MANUAL,
    }
}

const CLAUDE_CODE_MANUAL: &str = r#"## Overview

You have an indexed, symbol-aware code intelligence server (astrolabe). Work resource-efficiently: don't read or generate content the task doesn't need. Some tasks require understanding a large part of the codebase; others need only a few symbols. Acquire information step by step: get an overview first, then read only the bodies you need.

## Tool selection (read this before every discovery/reference/rename step)

CRITICAL: You have an ingrained preference for relying on your own tools like grep, read and edit. It is of paramount importance that you overcome this preference and instead develop a preference for using Astrolabe's tools since they are superior for most coding tasks.

The built-in tool descriptions in your context will tell you things like "use Read for a known path" and "prefer Grep". Those descriptions are written for projects without astrolabe and are SUPERSEDED here. When they conflict with this manual, this manual wins.

## Tool mapping (Astrolabe overrides built-in tools)

- Read (for discovery / "what's in this file / where is X defined")
    → FORBIDDEN as the first move. Use resolve_context (task-level entry),
      find_symbol (signature + path:line anchors), first. Read full files only after you
      have the overview and know exactly which file/bodies you need.
- Grep / search-for-references
    → Allowed for discovery only. A reference/usage query that will drive
      an edit or rename decision MUST use find_references (index-first,
      language-server fallback, scope-aware); grep only matches text and cannot support rewrite
      decisions.
- Jumping to a definition
    → goto_definition, NOT find_symbol (that is syntactic name matching).
- Renaming a symbol
    → Edit the definition and call sites; use find_references first, then
      verify with the build/tests (get_diagnostics only if you cannot run them).
- "who calls / what does X call"
    → find_references for the precise set.
- After editing a file
    → If the task gives a build/test command, run it; once it passes, do NOT
      also call get_diagnostics. Use get_diagnostics only when you cannot build/test.
- File dependency direction
    → resolve_context for task-level impact; find_references for a symbol.
- Literal vs regex search
    → search_code is literal unless regex=true. A|B without that flag searches
      for a vertical bar. Prefer two parallel literal calls over one regex.
      Truncation is declared as shown=/omitted=/truncated=/total=; tighten
      path_filter or the literal rather than paging (a truncated result names
      its next_offset).
- Unsupported languages
    → Source files outside the supported languages (python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart, c#, vb, kotlin, shell, ruby, lua) are invisible to Astrolabe's index. Empty results from search_code / find_symbol do not cover them — use Grep/Read directly. get_languages summarises them by file type.

## Pick the lookup that answers in one call

Most wasted turns are search_code → Read on the same file. Pick by what you need:
- Several independent lookups (a few names, literals, or files)
    → issue them as parallel Astrolabe calls in ONE turn, not one per turn. All
      Astrolabe tools are read-only, so the client runs them concurrently.
- The definition / body of a function, method, class or type you can name
    → find_symbol query=<name> include_body=true. It returns the whole body; no Read.
- Who calls / uses X (callers, implementations, every reference)
    → find_references (precise). Not search_code.
- Where X is defined from a use site → goto_definition.
- A literal (string, error text, config key, route, log message)
    → search_code with path_filter set to the directory you care about. Each match
      comes with surrounding lines (context_lines, adaptive by default); answer from
      that context instead of Reading the file. Read only for a range the context
      does not cover, and then with offset/limit around the reported line.
- search_code results are exhaustive over the files Astrolabe has indexed: total= is the
  full count of literal matches there and truncation is always declared, so one query
  is enough per literal. Files in unsupported languages and files created very recently
  may be missing or tagged [text-only]. find_symbol matches
  declared names only (syntax-level), not every textual mention.
- Before editing or renaming a literal, or asserting on it: search_code group=true
  (automatic when hits span more than 3 files) groups hits per file, code first then
  tests, tagged assert/load, so you see every loader and assertion in one call.
- Prefer these tools over Grep/Read for code: the goal is fewer tokens and less wall time.
- search_code auto-groups wide hits (see above); few hits list every row and every
  result ends with a "汇总" line saying whether the listing is complete — if it says
  complete (已全列), answer from it: do not re-check with grep or re-query with other words. Counting / 统计 / 计数 (how many
  calls per directory or file, 每个目录有多少处调用): search_code aggregate=top|dir|file gives
  "N 处 / M 个文件" per row plus a total — add skip_comments=true to ignore comment/string
  hits and exclude=["_test.go","vendor"] to skip paths; count_only=true for totals only.
  Do not write a script for this. Batch rename: find_references symbol=<old> dry_run_rename=<new>
  returns a per-file edit checklist (read-only), then edit file by file.
- Broad queries (common words, short names, no path_filter) produce big truncated
  pages: narrow with path_filter, a more specific literal, or find_symbol first
  rather than paging through hundreds of hits.

## Supported languages & unindexed files

Astrolabe indexes the following languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, vb, kotlin, shell, ruby, lua.
Source files in other languages are not indexed: empty results from `search_code` and `find_symbol` do not cover them. Use Grep/Read directly when searching or examining unsupported language files. `get_languages` summarises them by file type. Files of supported languages are indexed even with syntax errors (symbols in the broken region may be missing); `get_languages` lists up to 10 such files, or gives the path of a Markdown report with all of them.

## Language-server install (session-gated)

Precise tools (`find_references`, `goto_definition`, `get_diagnostics`) need a language server.
If one is missing they return `confidence: unknown` with the server name and
do **not** download anything.

When that happens:
1. Tell the user which server is missing (see the tool text / `get_languages`
   Ready / needs_install / AST-only rows).
2. Ask whether they will install it on this machine.
3. After it is on PATH, retry the precise tool. Use `get_languages` to confirm
   Ready / needs_install / AST-only.
4. If the user declines or the client is non-interactive, leave the server
   unavailable — do not invent a silent fallback.

Never treat ordinary discovery, indexing, or a read-only precise query as
permission to install.

## Read-only exploration

All Astrolabe tools are read-only: they never write, create, or
delete anything. Read-only tasks (surveys, reviews, codebase exploration, "just look
around") can and should still use `search_code` / `find_symbol` / `find_references` /
`resolve_context` — they return precise path:line anchors and exact symbol bodies at
a fraction of the tokens of whole-file reads. A deny reminder from the anti-drift hook
is NOT a ban on reading or exploring; it points at the cheaper read-only route.

## Disallowed rationalizations

Do NOT use any of the following to justify skipping astrolabe:
- "I already know the path/file"
- "one Read call is faster than three tool calls"
- "the built-in tool description says to use Read for known paths"
- "grep found the name, that's the same as references"
If you catch yourself reaching for one of these, that is the signal to switch to astrolabe.

You have hereby read the 'Astrolabe Instructions Manual' and do not need to read it again."#;

const DEFAULT_MANUAL: &str = r#"## Overview

You have an indexed, symbol-aware code intelligence server (astrolabe). Work resource-efficiently: don't read or generate content the task doesn't need. Some tasks require understanding a large part of the codebase; others need only a few symbols. Acquire information step by step: get an overview first, then read only the bodies you need.

## Tool selection (read this before every discovery/reference/rename step)

Astrolabe provides indexed, symbol-aware tools that are generally more efficient and precise than plain file reads and text searches. When working on code discovery, reference analysis, or renames, prefer Astrolabe's tools over generic built-ins.

## Tool mapping (Astrolabe recommended toolset)

- Read (for discovery / "what's in this file / where is X defined")
    → Avoid as the first move. Prefer resolve_context (task-level entry),
      find_symbol (signature + path:line anchors), first. Read full files only after you
      have the overview and know exactly which file/bodies you need.
- Grep / search-for-references
    → Allowed for discovery. A reference/usage query that will drive
      an edit or rename decision should use find_references (index-first,
      language-server fallback, scope-aware); grep only matches text and cannot support rewrite
      decisions.
- Jumping to a definition
    → goto_definition, NOT find_symbol (that is syntactic name matching).
- Renaming a symbol
    → Use find_references first, then edit the definition and call sites,
      then verify with the build/tests (get_diagnostics only if you cannot run them).
- "who calls / what does X call"
    → find_references for the precise set.
- After editing a file
    → If the task gives a build/test command, run it; once it passes, do NOT
      also call get_diagnostics. Use get_diagnostics only when you cannot build/test.
- File dependency direction
    → resolve_context for task-level impact; find_references for a symbol.
- Literal vs regex search
    → search_code is literal unless regex=true. A|B without that flag searches
      for a vertical bar. Prefer two parallel literal calls over one regex.
      Truncation is declared as shown=/omitted=/truncated=/total=; tighten
      path_filter or the literal rather than paging (a truncated result names
      its next_offset).
- Unsupported languages
    → Source files outside the supported languages (python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart, c#, vb, kotlin, shell, ruby, lua) are invisible to Astrolabe's index. Empty results from search_code / find_symbol do not cover them — use Grep/Read directly. get_languages summarises them by file type.

## Pick the lookup that answers in one call

Most wasted turns are search_code → Read on the same file. Pick by what you need:
- Several independent lookups (a few names, literals, or files)
    → issue them as parallel Astrolabe calls in ONE turn, not one per turn. All
      Astrolabe tools are read-only, so the client runs them concurrently.
- The definition / body of a function, method, class or type you can name
    → find_symbol query=<name> include_body=true. It returns the whole body; no Read.
- Who calls / uses X (callers, implementations, every reference)
    → find_references (precise). Not search_code.
- Where X is defined from a use site → goto_definition.
- A literal (string, error text, config key, route, log message)
    → search_code with path_filter set to the directory you care about. Each match
      comes with surrounding lines (context_lines, adaptive by default); answer from
      that context instead of Reading the file. Read only for a range the context
      does not cover, and then with offset/limit around the reported line.
- search_code results are exhaustive over the files Astrolabe has indexed: total= is the
  full count of literal matches there and truncation is always declared, so one query
  is enough per literal. Files in unsupported languages and files created very recently
  may be missing or tagged [text-only]. find_symbol matches
  declared names only (syntax-level), not every textual mention.
- Before editing or renaming a literal, or asserting on it: search_code group=true
  (automatic when hits span more than 3 files) groups hits per file, code first then
  tests, tagged assert/load, so you see every loader and assertion in one call.
- Prefer these tools over Grep/Read for code: the goal is fewer tokens and less wall time.
- search_code auto-groups wide hits (see above); few hits list every row and every
  result ends with a "汇总" line saying whether the listing is complete — if it says
  complete (已全列), answer from it: do not re-check with grep or re-query with other words. Counting / 统计 / 计数 (how many
  calls per directory or file, 每个目录有多少处调用): search_code aggregate=top|dir|file gives
  "N 处 / M 个文件" per row plus a total — add skip_comments=true to ignore comment/string
  hits and exclude=["_test.go","vendor"] to skip paths; count_only=true for totals only.
  Do not write a script for this. Batch rename: find_references symbol=<old> dry_run_rename=<new>
  returns a per-file edit checklist (read-only), then edit file by file.
- Broad queries (common words, short names, no path_filter) produce big truncated
  pages: narrow with path_filter, a more specific literal, or find_symbol first
  rather than paging through hundreds of hits.

## Supported languages & unindexed files

Astrolabe indexes the following languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, vb, kotlin, shell, ruby, lua.
Source files in other languages are not indexed: empty results from `search_code` and `find_symbol` do not cover them. Use Grep/Read directly when searching or examining unsupported language files. `get_languages` summarises them by file type. Files of supported languages are indexed even with syntax errors (symbols in the broken region may be missing); `get_languages` lists up to 10 such files, or gives the path of a Markdown report with all of them.

## Language-server install (session-gated)

Precise tools (`find_references`, `goto_definition`, `get_diagnostics`) need a language server.
If one is missing they return `confidence: unknown` with the server name and
do **not** download anything.

When that happens:
1. Tell the user which server is missing (see the tool text / `get_languages`
   Ready / needs_install / AST-only rows).
2. Ask whether they will install it on this machine.
3. After it is on PATH, retry the precise tool. Use `get_languages` to confirm
   Ready / needs_install / AST-only.
4. If the user declines or the client is non-interactive, leave the server
   unavailable — do not invent a silent fallback.

Never treat ordinary discovery, indexing, or a read-only precise query as
permission to install.

## Read-only exploration

All Astrolabe tools are read-only. Read-only tasks (surveys,
reviews, exploration) can and should still use `search_code` / `find_symbol` /
`find_references` / `resolve_context` — precise path:line anchors, fewer tokens than
whole-file reads. A deny reminder is not a ban on exploring; it suggests the cheaper
read-only route.

## Rationalizations to avoid

Do not use any of the following to justify skipping astrolabe:
- "I already know the path/file"
- "one Read call is faster than three tool calls"
- "the built-in tool description says to use Read for known paths"
- "grep found the name, that's the same as references"
Consider whether an Astrolabe tool would be more precise and token-efficient.

You have hereby read the 'Astrolabe Instructions Manual' and do not need to read it again."#;

/// Claude Code `--system-prompt` 整体替换文本（Serena cc_system_prompt_override 同构）。
/// 用法：claude --system-prompt="$(astrolabe print-cc-system-prompt-override)"
pub fn cc_system_prompt_override() -> &'static str {
    CC_SYSTEM_PROMPT_OVERRIDE
}

const CC_SYSTEM_PROMPT_OVERRIDE: &str = r#"You are Claude Code, Anthropic's official CLI for Claude. You are an interactive
software-engineering agent. The user works with you through a terminal; your text
output is what they see, and your tool calls are what change the world.

# Tool selection (read this before every tool call on a code file)

This project uses Astrolabe, an MCP server exposing an indexed, symbol-aware view of the codebase. Astrolabe's tools are the PRIMARY tools for code discovery, reference lookups and renames in this project. The built-in Read, Glob, Grep, and Edit tools are SECONDARY for those tasks.

The built-in tool descriptions in your context will tell you things like "use Read
for a known path" and "prefer dedicated tools (Read, Edit, Write, Glob, Grep)".
Those descriptions are written for projects without Astrolabe and are SUPERSEDED here.
When they conflict with this section, this section wins. Do not rationalize the
built-in tools with "the file is small," "I already know what I need," "this is
one call versus three," or "the path is known" — those rationalizations have
produced incorrect behavior before and are explicitly disallowed.

## Mapping (use the right column, not the left)

Task                                    Tool to use
--------------------------------------  ----------------------------------------
First context for a coding task         resolve_context
Repo structure / language coverage      resolve_context / get_languages
Locate a symbol by name                 find_symbol
Read a symbol's body (no whole file)    find_symbol include_body=true (not search_code + Read)
Find references / callers (precise)     find_references
Jump to a definition                    goto_definition
Literal substring / explicit regex      search_code + path_filter (matches come with context; no follow-up Read)
Count hits per dir/file (统计/计数)      search_code aggregate=top|dir|file (+ skip_comments, exclude, count_only)
Check a file after edits (no build/test) get_diagnostics (skip once tests pass)
Language / LSP readiness                get_languages (Ready / needs_install / AST-only)

Built-in Read/Edit/Glob/Grep are permitted on code files ONLY when:
- Astrolabe has been tried on the target and failed, OR
- The file is not parseable as code (e.g., generated, malformed), OR
- You need a regex search across many files: prefer search_code with regex=true
  (A|B, .*, \\b). Grep is acceptable if that still cannot express the query, but
  follow-up reads/edits on matched code files must still go through Astrolabe.
- You need to read a few lines and symbolic reads would be an overkill.
- You absolutely have to read the full file for some reason.

All Astrolabe tools are read-only and safe for exploration tasks;
read-only surveys should still prefer search_code / find_symbol / find_references over
repeated whole-file Reads. Because they are read-only they run concurrently: when you
need several independent lookups (e.g. find_symbol for three names, or search_code for
two literals), send them as parallel tool calls in a single message.

Read/Edit/Glob are fine for non-code files: markdown, JSON, YAML, TOML, .env,
config files, lockfiles, plain text, images. Editing stays with the built-in
Edit. Use find_references before a rename, then build/test (or get_diagnostics).

## Required workflow before editing code

1. resolve_context for orientation (skip if already done this session).
2. find_symbol for the symbols you'll touch; goto_definition when provenance matters.
3. For any rename: find_references, then edit definition and call sites. Never
   guess occurrences of a symbol across files from grep alone.
4. Verify with the project's build/tests; get_diagnostics on edited files only
   when you cannot run them (not again after they pass).

## Self-check

Before every Read, Glob, or Grep call: 'Does this target a code file, and does the
mapping above name an Astrolabe tool for this task?' If yes, switch. Do this check
every time — not just once per session."#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn connection_instructions_contains_required_sections() {
        let dummy_root = Path::new("/workspace/test_repo");
        let instructions = connection_instructions("claude-code", dummy_root);
        assert!(instructions.contains("CRITICAL"));
        assert!(instructions.contains("/workspace/test_repo"));
        assert!(instructions.contains("resolve_context"));
        assert!(instructions.contains("search_code"));
        assert!(instructions.contains("get_languages"));
        assert!(instructions.contains("parallel"));
        // Complete results need no grep re-check: said in the default-loaded tier.
        assert!(instructions.contains("已全列"), "{instructions}");
        assert!(
            instructions.contains("不必再用 grep 复核"),
            "{instructions}"
        );
        // Language servers are a fallback: never tell the model to install one.
        assert!(
            !instructions.contains("安装对应语言服务器"),
            "{instructions}"
        );
        assert!(
            !instructions.contains("先调用 resolve_context"),
            "{instructions}"
        );
    }

    fn tmp_repo(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("astrolabe-gw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (p, body) in files {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        d
    }

    #[test]
    fn go_layout_hint_names_module_root_from_go_work() {
        // The AI-con layout: root go.work `use ./src`, nested third_party module.
        let d = tmp_repo(
            "work",
            &[
                ("go.work", "go 1.26.0\n\nuse ./src\n"),
                ("src/go.mod", "module example.com/m\n"),
                ("src/third_party/q/go.mod", "module q\n"),
            ],
        );
        let l1 = connection_instructions("claude-code", &d);
        assert!(l1.contains("模块根在 src/（由根目录 go.work use）"), "{l1}");
        assert!(l1.contains("cd src && go test"), "{l1}");
        assert!(!l1.contains("third_party"), "{l1}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn go_layout_hint_scans_without_go_work() {
        let d = tmp_repo(
            "scan",
            &[
                ("svc/a/go.mod", "module a\n"),
                ("svc/a/inner/go.mod", "module inner\n"),
                ("tools/go.mod", "module tools\n"),
                ("node_modules/x/go.mod", "module x\n"),
                (".hidden/go.mod", "module h\n"),
            ],
        );
        let h = go_layout_hint(&d).unwrap();
        assert!(h.contains("svc/a/、tools/"), "{h}");
        assert!(
            !h.contains("inner") && !h.contains("node_modules") && !h.contains("hidden"),
            "{h}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn go_layout_hint_silent_for_root_module_and_non_go() {
        let d = tmp_repo(
            "root",
            &[("go.mod", "module m\n"), ("sub/go.mod", "module s\n")],
        );
        assert!(go_layout_hint(&d).is_none());
        let _ = std::fs::remove_dir_all(&d);
        let d = tmp_repo("none", &[("src/lib.rs", "")]);
        assert!(go_layout_hint(&d).is_none());
        assert!(!connection_instructions("claude-code", &d).contains("Go："));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn go_work_uses_parses_block_and_single_forms() {
        let w = "go 1.22\nuse ./a\nuse (\n\t./b/\n\t\"c\" // note\n\t.\n)\nuser x\n";
        assert_eq!(go_work_uses(w), vec!["a", "b", "c", "."]);
    }

    #[test]
    fn every_tier_skips_diagnostics_after_passing_tests() {
        // E1 (batch 7): get_diagnostics after `go test` already passed cost a
        // turn and a gopls start in 3 of the 4 runs7 runs that called it.
        let l1 = connection_instructions("claude-code", Path::new("/r"));
        assert!(l1.contains("不必再调 get_diagnostics"), "{l1}");
        for name in ["claude-code", "default"] {
            let m = manual(name);
            assert!(
                m.contains("do NOT\n      also call get_diagnostics"),
                "{name}"
            );
            assert!(
                !m.contains("get_diagnostics to confirm the code still holds"),
                "{name}"
            );
        }
        let p = cc_system_prompt_override();
        assert!(p.contains("skip once tests pass"));
        assert!(p.contains("not again after they pass"));
    }

    #[test]
    fn every_tier_encourages_parallel_read_only_calls() {
        for name in ["claude-code", "default"] {
            let m = manual(name);
            assert!(m.contains("parallel Astrolabe calls in ONE turn"), "{name}");
        }
        assert!(cc_system_prompt_override().contains("parallel tool calls in a single message"));
    }

    #[test]
    fn manuals_state_scope_and_limits() {
        for name in ["claude-code", "default"] {
            let m = manual(name);
            assert!(m.contains("fewer tokens"), "{name}");
            assert!(m.contains("auto-groups"), "{name}");
            assert!(m.contains("aggregate=top|dir|file"), "{name}");
            assert!(m.contains("dry_run_rename"), "{name}");
        }
    }

    #[test]
    fn manuals_do_not_advertise_removed_tools() {
        for name in ["claude-code", "default"] {
            let m = manual(name);
            for gone in [
                "trace_calls",
                "get_repo_skeleton",
                "get_dependents",
                "get_neighborhood",
                "get_group_graph",
                "get_hotspots",
                "get_symbol_info",
                "plan_rename",
                "apply_rename",
                "ensure_language_server",
                "explain_symbol",
                "read_memory",
                "write_memory",
                "delete_memory",
                "initial_instructions",
                "list_memories",
            ] {
                assert!(!m.contains(gone), "{name} still mentions {gone}");
            }
        }
        let p = cc_system_prompt_override();
        for gone in [
            "trace_calls",
            "get_dependents",
            "get_neighborhood",
            "get_group_graph",
            "get_symbol_info",
            "plan_rename",
            "apply_rename",
            "read_memory",
            "write_memory",
            "get_repo_skeleton",
            "get_hotspots",
            "ensure_language_server",
            "explain_symbol",
            "delete_memory",
            "list_memories",
            "initial_instructions",
        ] {
            assert!(!p.contains(gone), "cc override still mentions {gone}");
        }
    }

    #[test]
    fn manual_claude_code_strict_doctrine() {
        let m = manual("claude-code");
        assert!(m.contains("FORBIDDEN"));
        assert!(m.contains("find_references"));
        assert!(m.contains("goto_definition"));
        assert!(m.contains("get_diagnostics"));
        assert!(m.contains("SUPERSEDED"));
        assert!(m.contains("Disallowed rationalizations"));
        assert!(m.contains("paramount importance"));
        assert!(m.contains("CRITICAL"));
        assert!(m.contains("You have hereby read"));
        assert!(m.contains("## Language-server install (session-gated)"));
        assert!(m.contains("## Read-only exploration"));
        assert!(m.contains("NOT a ban on reading or exploring"));
        assert!(m.contains("search_code is literal unless regex=true"));
        assert!(!m.contains("offset=next_offset"), "paging is not taught");
        assert!(!m.contains("narrow Grep"), "no grep re-check advice");
        assert!(m.contains("get_languages") || m.contains("language server"));
    }

    #[test]
    fn manual_default_mild_advisory() {
        let m = manual("default");
        assert!(!m.contains("FORBIDDEN"));
        assert!(m.contains("find_references"));
        assert!(m.contains("get_languages") || m.contains("language server"));
        assert!(m.contains("You have hereby read"));
    }

    #[test]
    fn manual_fallback_unknown_context() {
        assert_eq!(manual("cursor"), manual("default"));
        assert_eq!(manual("unknown-client"), manual("default"));
    }

    #[test]
    fn both_manuals_contain_completion_marker() {
        assert!(manual("claude-code").contains("You have hereby read"));
        assert!(manual("default").contains("You have hereby read"));
    }

    #[test]
    fn cc_system_prompt_override_content() {
        let p = cc_system_prompt_override();
        assert!(p.contains("You are Claude Code, Anthropic's official CLI for Claude."));
        assert!(p.contains("SUPERSEDED"));
        assert!(p.contains("find_references"));
        assert!(p.contains("resolve_context"));
        assert!(p.contains("Self-check"));
        assert!(p.contains("All Astrolabe tools are read-only"));
        assert_ne!(p, manual("claude-code"));
    }
}
