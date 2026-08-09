# 来自 Claude、Cursor 或 Codex？

别担心——你的设置、规则和技能都会随你迁移。Grok Build
读取其他智能体使用的相同项目约定，并导入其余内容。

## 自动获取

- **规则与指令** — `AGENTS.md`（Codex/OpenCode 约定）、
  `CLAUDE.md`（包括嵌套文件），以及
  `.claude/rules/` 和 `.cursor/rules/` 下的 `*.md` 规则文件。
- **技能与自定义命令** — `~/.claude/skills/`、`~/.claude/commands/`、
  `~/.cursor/skills/`，以及对应的项目级目录。扁平的命令 `.md`
  文件在这里也会成为斜杠命令。
- **MCP 服务器** — 来自 `~/.claude.json`、`.cursor/mcp.json` 以及项目的
  `.mcp.json`。
- **钩子** — 来自 `.claude/settings.json`，包括 `Bash` 等匹配器别名，
  因此大多数钩子无需改动即可运行。

## 一步导入

**`/import-claude`** 会扫描你的 `~/.claude` 设置——权限、环境变量、MCP
服务器和钩子——并显示复选框预览；确认后会将你选中的项目写入 `.grok`
项目配置。可随时重新运行。

## 从上次中断处继续

**`/resume-claude`**、**`/resume-codex`** 和 **`/resume-cursor`**
技能会将这些工具中的最近会话接续到这里。

## 查看发现的内容

在仓库中运行 **`grok-zh inspect`**，即可查看 Grok 获取到的每个规则文件、
技能和 MCP 服务器，并标注其来源。可在 `[compat.claude]` /
`[compat.cursor]` 配置节中分别启用或停用每个兼容来源。

另外，有些功能你可能在其他工具中错过了：`/btw` 可在不中断当前任务的情况下
提出旁支问题，`/rewind` 则将对话回退到较早轮次（文件更改保持不变）。

*深入了解：`/docs Project Rules (AGENTS.md)`、`/docs Skills` 或 `/docs MCP Servers`*
