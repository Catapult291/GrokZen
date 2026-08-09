# xai-grok-pager

Grok Build 的终端 UI（TUI）。提供交互式全屏界面，包括回滚区、提示输入、会话管理和所有模态对话框。

## 架构

```
src/
├── app/                 # 应用状态和事件处理
│   ├── app_view.rs      # 顶层状态（欢迎屏幕、智能体、配置）
│   ├── agent_view/      # 每会话智能体视图（mod.rs 中的结构体及各领域实现模块）
│   ├── dispatch/        # Action → Effect 分发器（路由器及各领域模块）
│   ├── effects.rs       # 异步副作用（ACP 调用、文件 I/O）
│   └── event_loop.rs    # 主事件循环（输入、tick、ACP 消息）
├── views/               # UI 组件
│   ├── prompt_widget.rs # 支持文件搜索、斜杠、历史记录的文本编辑器
│   ├── welcome/         # 欢迎屏幕
│   ├── extensions_modal.rs   # 扩展模态界面（钩子、插件、Marketplace、技能、MCP 服务器）
│   ├── file_search/     # @ 补全下拉框和行查看器
│   ├── slash_dropdown.rs# /command 补全下拉框
│   └── ...              # 回滚区、状态栏、窗格等
├── scrollback/          # 消息历史渲染
├── slash/               # 斜杠命令注册表和内置命令
├── appearance/          # 主题和 pager.toml 配置
├── acp/                 # Agent Communication Protocol 客户端状态
└── render/              # 底层渲染辅助工具（颜色、换行等）
```

## 核心概念

- **AppView**——拥有欢迎屏幕、智能体会话和全局配置
- **AgentView**——每个会话一个；拥有提示、回滚区、工具窗格和模态界面
- **PromptWidget**——支持文件搜索（`@`）、斜杠命令（`/`）、历史搜索和粘贴元素的文本编辑器
- **Action/Effect**——Elm 风格架构：输入 → Action → dispatch → Effect → 状态更新

## 键盘快捷键

| 键 | 上下文 | 操作 |
|-----|---------|--------|
| `Ctrl+P` 或 `?` | 智能体屏幕 | 打开命令面板 |
| `Ctrl+L` | 任意（非 VS Code 系列） | 打开插件/钩子模态界面；在 VS Code / Cursor / Windsurf / Zed 中使用 `/plugins` 或 `/hooks`（`Ctrl+L` 用于回合中插话） |
| `Tab` | 提示 | 切换到回滚区 |
| `Esc` | 回合运行中 | 取消——精简模式下或关闭 Vim 回滚区模式时（默认）。全屏 Vim 模式：无操作（使用 `Ctrl+C`） |
| `Esc` `Esc` | 空闲、提示非空 | 清空提示（800 毫秒内；第一次按下会显示提示） |
| `Esc` `Esc` | 空闲、提示为空且有消息 | 打开回退选择器（第一次按下无提示） |
| `Ctrl+M` | 提示 | 切换多行模式 |
| `Shift+Enter` | 提示 | 插入换行 |
| `/` | 提示 | 开始斜杠命令 |
| `@` | 提示 | 开始文件搜索 |
| `!` | 提示（为空） | 进入 bash 模式 |
| `Ctrl+C` | 提示（有文本） | 清空提示（即使回合正在运行） |
| `Ctrl+C` | 提示（为空）+ 回合运行中 | 取消正在运行的回合 |
| `Ctrl+B` | 智能体屏幕 + 前台命令运行中 | 将命令发送到后台 |
| `Ctrl+G` | 智能体屏幕（完整 TUI） | 切换任务窗格 |
| `Ctrl+G` | 普通编辑器（精简模式） | 在外部编辑草稿；如果该组合键已被占用，请使用命令面板入口 |

## 文档

- [终端支持与故障排查](docs/user-guide/zh-CN/21-terminal-support.md)——tmux/SSH 真彩色、剪贴板、鼠标、诊断、`/doctor`
- [钩子与插件指南](docs/zh-CN/hooks-and-plugins.md)——管理钩子、插件和 Marketplace 源
- [自定义钩子指南](docs/zh-CN/custom-hooks.md)——创建、配置和编写自己的钩子
- [钩子示例](../xai-grok-hooks/examples/README.zh-CN.md)——常见工作流的示例
- [钩子 crate（`xai-grok-hooks`）](../xai-grok-hooks/)——钩子运行时、事件类型和执行引擎
- [插件 Marketplace crate（`xai-grok-plugin-marketplace`）](../xai-grok-plugin-marketplace/)——Marketplace 源加载、扫描和安装
