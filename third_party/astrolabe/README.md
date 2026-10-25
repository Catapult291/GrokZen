# Astrolabe AI

**Astrolabe AI is a local MCP server that gives Claude Code, Cursor and Codex fast code search, symbol lookup and reference navigation, with directory-level counts for questions like "how many call sites per module".**
It is a single Rust binary (`astrolabe`) that indexes your repo with tree-sitter and starts language servers on demand.

[简体中文](README.zh-CN.md) · [Install guide](docs/INSTALL_GUIDE.md) · [FAQ](docs/FAQ.md) · [Benchmark harness](bench/README.md) · [Changelog](CHANGELOG.md)

> **Not to be confused with** the npm packages `astrolabe` (a 2014 Protractor page-object library) and `astrolabe-ai` (an unrelated resume tool), or other MCP servers named "astrolabe". This project lives at `github.com/wojiushirencai/Astrolabe`; the binary is `astrolabe`.

## Fact card (as of 2026-10-23)

| | |
|---|---|
| Latest release | [v0.17.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.17.0), published 2026-10-23 |
| What it exposes | 7 MCP tools: `resolve_context`, `search_code`, `find_symbol`, `find_references`, `goto_definition`, `get_diagnostics`, `get_languages` |
| `tools/list` size | about 3.6 KB compact JSON (3,607 B measured on Linux for v0.16.0; not re-measured on macOS) |
| Binary | one executable, no runtime; release archives are 7.4–9.1 MB, the unpacked Linux x64 binary is about 50 MB |
| Platforms | macOS arm64/x64, Linux x64 (glibc/musl), Linux arm64, Windows x64 |
| Languages | 17 matrix rows incl. Python, Go, Rust, Java, TypeScript/JavaScript, C/C++, Swift, Objective-C, PHP, Vue, Dart, C#, VB.NET, Kotlin, Shell, Ruby, Lua ([details](docs/REFERENCE.zh-CN.md)) |
| Needs an API key / network? | No. Indexing and search do not call an LLM. (The model that reads the results still spends tokens.) |
| Clients documented | Claude Code, Cursor, Codex, Windsurf/Cline/Roo Code/Continue, OpenCode, DeepSeek Harness, Codebuddy, Grok Build, Kimi Code, ZCode |
| License | MIT OR Apache-2.0 |
| Tests | `cargo test --workspace -- --list` lists 1,117 test cases (2026-10-18) |

## Quick start (3 steps)

**1. Install** (macOS / Linux; verifies the release sha256):

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
```

Other ways (manual download, Windows, build from source): [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md). npm: `npx -y @zlnap/astrolabe` (published); wire clients: `npx -y @zlnap/astrolabe setup`.

**2. Connect your client.** Claude Code:

```bash
npx -y @zlnap/astrolabe setup   # detect agents and merge MCP configs
# or manually:
claude mcp add --scope user astrolabe -- npx -y @zlnap/astrolabe . --context=claude-code
```

Cursor (`.cursor/mcp.json`): `{"mcpServers":{"astrolabe":{"command":"astrolabe","args":[".","--context=cursor"]}}}`.
Codex, Windsurf, OpenCode, DSH, Codebuddy, Grok, Kimi, ZCode and others: [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md#2-client-configuration).

**3. Verify.** `astrolabe --version` prints the version; in your client, ask it to call `get_languages` or `search_code`. Restart the client session after adding the server.

## The seven tools

| Tool | Use it for |
|---|---|
| `search_code` | Literal/regex search with path filters. Groups hits and labels each as definition / use / comment; ends with a verifiable summary ("23/23 hits, no truncation"). Counting: `aggregate=top\|dir\|file` (hits and files per top-level dir / dir / file), `count_only`, `skip_comments`, `exclude` (e.g. `_test.go`, `vendor`). |
| `resolve_context` | Give it a task description; it returns relevant files and symbols ranked by relevance, as pages (`offset`, `path_filter`) whose header says how many matched and where the rest lives, and hints which tool to use next. |
| `find_symbol` | Find a function, class or type by name or substring. |
| `find_references` | Compiler-grade references via the language server; `dry_run_rename=<new name>` lists per-file edit locations without writing anything. |
| `goto_definition` | Jump to the real definition through the language server. |
| `get_diagnostics` | Syntax and type errors for a file. |
| `get_languages` | Languages, file counts, lines of code, language-server status, and index coverage (see below). |

`tools/list` advertises only the parameters models actually use (about 3.3 KB smaller, resent on every API turn). The others — `regex`, `exclude`, `count_only`, `context_lines`, `group`, `dedup`, `kind`, `depth`, `substring`, `dry_run_rename`, `character`, `severity`, `offset`, `page`, `budget_tokens`, and `root` outside multi-repo workspaces — are still accepted. Set `ASTROLABE_FULL_SCHEMA=1` to advertise all of them.

**Index coverage.** Every file of a supported language is indexed, including files with syntax or compile errors: the symbols that parse are kept, but symbols inside the broken region may be missing. `get_languages` reports:
- files *not* in the index, as counts by reason (over the size cap, read failure, parser timeout);
- files with syntax errors, counted by kind:
  - *other syntax errors* — `has syntax errors, index may be incomplete (symbols may be missing)`;
  - *macro/preprocessor* (C, C++, Objective-C only) — `macro/preprocessor syntax errors, symbols mostly intact`: the error sits next to a directive or macro-looking code (an `#if` cutting a declaration, `LUAI_FUNC int f()`, `TEST_CASE("x")`, `NAMESPACE_BEGIN`, `foreach(x) {`). No grammar can expand macros without a build configuration, but error recovery keeps the symbol names; ranges near the error may be off. This is a heuristic: in doubt a file counts as an other syntax error.

  Listed inline (path, kind label, first error line with a snippet of that line) when there are 10 or fewer; otherwise only the counts plus the absolute path of a Markdown report (`<repo>/.astrolabe/coverage-report.md`, self-gitignored; temp dir if that is not writable) with path, language, kind and first error for each file (other syntax errors first), capped at 1,000 entries per section;
- unsupported-language files, summarised by extension (top 8, the rest folded into one count), never per file.

Other tools append one short line (`注意：N 个文件…；详见 get_languages。`) only when such gaps exist.

## Evidence: what we measured, and what we did not

All numbers below come from our own paired A/B runs: **A** = Claude Code without Astrolabe, **C** = the same task with Astrolabe connected. Cost is Claude Code's `total_cost_usd`, time is `duration_ms`. One model was used: `gemini-3.8-flash-high` through Claude Code and an Anthropic-compatible proxy (so USD figures use that proxy's pricing). WebSearch/WebFetch were disabled in every arm. Summaries of the reports are in [bench/results/](bench/results/) and the method in [bench/README.md](bench/README.md).

**Simultaneous pairs on v0.13.0: 15 tasks, 95 pairs.** 11 tasks with 5 pairs each and 4 tasks (N5, N8, N4, E3b) with 10 pairs each. Each pair started A and C at the same instant.

| | A (no Astrolabe) | C (Astrolabe v0.13.0) | C − A, paired, 95% bootstrap CI |
|---|---|---|---|
| Cost, mean per run | $2.01 | $1.51 | **−$0.49 [−0.73, −0.28]** |
| Time, mean per run | 440 s | 357 s | **−83 s [−129, −41]** |
| Turns, mean | 34.9 | 28.1 | −6.8 [−9.2, −4.3] |
| Correct, primary scoring | 93/95 | 95/95 | +2.1 pp [0.0, +5.3] |
| Correct, strict scoring | 93/95 | 86/95 | −7.4 pp [−14.7, −1.1] |

Equal weight per task gives a similar picture: cost −$0.45 [−0.90, −0.15], time −76 s [−153, −21]; C is cheaper than A on average in 11 of 15 tasks. Dropping the pairs that overlapped a machine freeze changes little (87 pairs: cost −$0.45, time −87 s).

*Primary vs strict scoring (N5):* the reference answer for N5 counts 2 call sites that sit inside comments (model=15/44, total=321). With `skip_comments=true` Astrolabe returns 15/43 and 319, which is also a defensible answer. The primary N5 verdict therefore accepts either convention; the strict verdict (reference convention only) is still computed and reported next to it. The whole strict gap comes from N5 (C 1/10 strict, 10/10 primary). We changed the N5 primary after seeing this result; both numbers are published so you can judge it.

**Where the gain is.** It is concentrated in aggregation/discovery tasks: N1 (cost C − A −$3.10 [−4.88, −1.32] over 5 pairs), N5 −$0.64 [−1.22, −0.02], N8 −$0.91 [−1.48, −0.31] over 10 pairs each. The other 10 of the 11 newly added tasks have cost intervals that include zero, and simple locating/small tasks (H2, H3, H6, H7, E2, N6, N7) are about even. N4 and E3b (private repos) were cheaper on average but not significantly, and in them the model never used the `aggregate` options. Per-turn latency did not differ (+0.6 s [−0.3, +1.4]); the time saving comes from fewer turns.

**What this does not show.**
- It does **not** show a general benefit. The 15 tasks are our own, there is one model, and each task has only 5 or 10 pairs, so per-task intervals are wide. Treat the averages as describing these tasks only.
- The older v0.12.0 matrix (75 cells, 15 tasks, 5 rounds, triplets started together) found Astrolabe **more expensive**, not cheaper: cost C − A = **+$0.244 [+0.035, +0.467]**; time +53 s [−8, +119] and correctness −4.0 points [−10.7, +2.7] both include zero. On its 40 control cells the extra cost was +$0.294 [+0.085, +0.540]. Newer results used v0.13.0, which changed `search_code` and the tool list, so the two runs are not the same product.
- Six of the 15 tasks use private or edited code (N4, E3b, H4, H5, E1, N7) and **cannot be reproduced** by others. The public-repo tasks can be re-run. The reference snapshot we used was a local copy; see [bench/README.md](bench/README.md) for how to check your checkout against it.
- Latency per turn drifts during the day, so only simultaneous pairs are comparable for time.

Reproduce the public part: `bash bench/fetch_corpora.sh && bash bench/run_pairs.sh --tasks N5,N8 --pairs 10` (real API spend: our 40-pair run cost about $214, the 55-pair run about $121), then `python3 bench/report.py`.

## When it helps, when it does not

- **Measured to help:** counting or listing occurrences per directory/module with exclusions (N5, N8) and one broad discovery task (N1): fewer turns and lower cost than the same model doing it with shell scripts.
- **Not shown to help:** simple locating or small-edit tasks (H2, H3, H6, H7, E2, N6, N7 are about even, intervals include zero); the 40 general-purpose control cells of the v0.12.0 matrix cost more with Astrolabe; batch edit workflows (N4, E3b) were cheaper on average but not significantly.
- **Overlap:** Claude Code can provide go-to-definition and find-references through its own LSP plugins ([docs](https://code.claude.com/docs/en/plugins/code-intelligence)); our `goto_definition` / `find_references` overlap with that.
- **Adoption is not guaranteed:** models sometimes ignore MCP tools. In v0.12.0 the model used Astrolabe in 0/5 runs of N5 and 1/5 of N8; with v0.13.0 it did so in 92 of 95 runs across the 15 tasks. That is an observation about this model and these tasks, not a promise.

## Related projects

[Serena](https://github.com/oraios/serena) (LSP-based coding toolkit), [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp), [codegraph](https://github.com/colbymchenry/codegraph), [claude-context](https://github.com/zilliztech/claude-context), the Sourcegraph MCP server, and Claude Code's built-in LSP plugins. We have not benchmarked Astrolabe against any of them; indexing-time numbers that older versions of this README published against Serena and OpenVisio are archived with their caveats in [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## More documentation

[Install & clients](docs/INSTALL_GUIDE.md) · [FAQ](docs/FAQ.md) · [Reference (Chinese): config, env vars, language matrix, quality gates](docs/REFERENCE.zh-CN.md) · [Architecture](docs/ARCHITECTURE.md) · [Performance method](docs/PERFORMANCE.md) · [Optional anti-drift hooks (effect not benchmarked)](docs/HOOKS.md) · [Releasing](.github/RELEASING.md) · [llms.txt](llms.txt)

Build from source: `cargo build --release --bin astrolabe` (Rust 1.90+). Tests: `cargo test --workspace`.

## License

MIT OR Apache-2.0, see [NOTICE](NOTICE). Astrolabe was inspired by [Serena](https://github.com/oraios/serena) and [OpenVisio](https://github.com/syntaxPriest/openvisio-oss); details in [docs/REFERENCE.zh-CN.md](docs/REFERENCE.zh-CN.md#五致谢与技术渊源).
