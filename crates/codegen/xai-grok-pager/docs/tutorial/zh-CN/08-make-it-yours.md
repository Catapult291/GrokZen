# 自定义 Grok

## 最简单的方法：直接提出请求

Grok 了解自身能力，也能自行配置。试试：

- *“为我们的预发布数据库添加 Postgres MCP 服务器”*
- *“切换到浅色主题”*
- *“为此仓库编写一个 AGENTS.md”*

如果你更愿意亲自操作，下面的每项也都有对应命令。

## 教 Grok 了解你的项目：AGENTS.md

在仓库根目录放置一个 `AGENTS.md` 文件，写入构建命令、约定和注意事项。
Grok 会在每次会话中自动读取它——这是投入产出比最高的自定义方式：

```markdown
# 我的项目
- 使用 `pnpm test` 运行测试
- 切勿编辑 `generated/` 下的文件
```

## 教 Grok 记住你的事实：记忆

在提示开头输入 `#`（或使用 `/remember`），为后续会话保存一条备注：
`# 预发布部署使用 eu-west`。

## 外观、快捷键和扩展

- **`/theme`** — 配色主题（或使用 `auto` 跟随操作系统）；其他所有设置请用
  **`/settings`**（或 `F2`）；如果你喜欢 Vim 风格，则使用 **`/vim-mode`**。
- **技能**（`/skills`）— 可复用的提示包；用户可调用的技能
  会自动成为斜杠命令。
- **MCP 服务器**（`/mcps`）以及 **插件和钩子**（`/plugins`、`/hooks`）。

从 `AGENTS.md` 和主题开始；需要时再添加其余内容。

*深入了解：`/docs Project Rules (AGENTS.md)`、`/docs Skills` 或 `/docs MCP Servers`*
