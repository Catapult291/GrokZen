# Grok Build 用户指南

学习如何安装、配置和扩展 SpaceXAI 推出的终端 AI 编程助手 Grok Build。

---

## 第 1 层：必读用户文档

从这里开始。这些指南涵盖第一天使用所需的内容。

| # | 文档 | 说明 |
|---|----------|-------------|
| 1 | [入门指南](01-getting-started.md) | 安装、首次启动、身份验证、基本交互和核心概念 |
| 2 | [身份验证](02-authentication.md) | 浏览器登录、API 密钥、OIDC/SSO、外部身份提供方和设备代码流程 |
| 3 | [键盘快捷键](03-keyboard-shortcuts.md) | TUI 中每个按键绑定和鼠标操作的参考 |
| 4 | [斜杠命令](04-slash-commands.md) | 全部 `/` 命令，包括目标、深度研究和工作流运行管理 |
| 5 | [配置](05-configuration.md) | `config.toml`、`pager.toml`、环境变量和文件位置 |

---

## 第 2 层：核心功能文档

自定义并扩展 Grok Build。

| # | 文档 | 说明 |
|---|----------|-------------|
| 6 | [主题与外观](06-theming.md) | 主题、`/theme` 命令、`pager.toml` 和颜色支持检测 |
| 7 | [MCP 服务器](07-mcp-servers.md) | 通过模型上下文协议（Model Context Protocol）集成外部工具 |
| 8 | [技能](08-skills.md) | SKILL.md 格式的可复用提示包 |
| 9 | [插件](09-plugins.md) | 打包并共享技能、命令、智能体、钩子和 MCP 服务器；从市场安装、编写插件并进行治理（组织控制） |
| 10 | [钩子](10-hooks.md) | 工具使用前后事件的生命周期脚本和 HTTP 回调 |
| 11 | [自定义模型](11-custom-models.md) | 自带密钥、Ollama 和 OpenAI 兼容端点 |
| 12 | [项目规则（AGENTS.md）](12-project-rules.md) | 按目录生效的 AGENTS.md 指令及其优先级 |
| 13 | [记忆](13-memory.md) | 通过 `/flush`、`/dream` 和混合搜索持久化跨会话知识 |

---

## 第 3 层：高级用法文档

将 Grok Build 与其他系统自动化、脚本化并集成。

| # | 文档 | 说明 |
|---|----------|-------------|
| 14 | [无头模式与脚本](14-headless-mode.md) | `grok-zh -p`、输出格式、CI/CD 集成和管道 |
| 15 | [智能体模式与 IDE 集成](15-agent-mode.md) | ACP stdio 传输、WebSocket 中继和 SDK 集成 |
| 16 | [子智能体与角色](16-subagents.md) | 并行子会话、智能体类型、角色和工具访问模式 |
| 17 | [会话管理](17-sessions.md) | 保存、加载、恢复、回退、压缩和会话持久化格式 |
| 18 | [沙箱模式](18-sandbox.md) | 操作系统级文件系统和网络隔离配置 |
| 19 | [计划模式](19-plan-mode.md) | 结构化规划、计划文件编辑以及编码前的审批 |
| 20 | [后台任务与监控](20-background-tasks.md) | `background: true`、`/loop`、`monitor` 和用于降级的 `Ctrl+B` |
| 21 | [终端支持与故障排查](21-terminal-support.md) | tmux、SSH、真彩色、剪贴板和 OSC 52 |
| 22 | [权限与安全](22-permissions-and-safety.md) | 模式（always-approve、auto、ask）、规则、匹配、钩子和示例 |
| 23 | [智能体面板](23-dashboard.md) | 本地会话和分支的集中概览 |
| 24 | [使用量监控（外部 OpenTelemetry）](24-monitoring-usage.md) | 客户端 OTEL 导出 |
| 25 | [状态栏](25-status-line.md) | 底部状态行：内置分段、命令脚本和标准输入 JSON 约定 |
| 26 | [配置参考](26-config-reference.md) | `config.toml`、`managed_config.toml` 和 `requirements.toml` 的字段列表 |
| 27 | [grok clone](27-grok-clone.md) | 通过 Grove 快速克隆仓库、浅克隆和完整历史 |
