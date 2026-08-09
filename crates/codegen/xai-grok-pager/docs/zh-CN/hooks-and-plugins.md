# 钩子与插件指南

Grok Build 支持**钩子**（事件驱动的 shell 命令）和**插件**（技能、智能体、钩子及 MCP 服务器的捆绑包）。两者均通过统一的模态界面管理。

## 打开模态界面

| 方法 | 打开选项卡 |
|--------|-------------|
| `Ctrl+L` | 插件（任意窗格；**非 VS Code 系列**——在 VS Code / Cursor / Windsurf / Zed 中使用 `/plugins`） |
| `/plugins` | 插件（任意终端） |
| `/hooks` | 钩子 |

## 选项卡

模态界面有三个选项卡：**钩子**、**插件**和 **Marketplace**。使用 `Tab` / `→`（向前）或 `Shift+Tab` / `←`（向后）在它们之间切换。

---

## 钩子选项卡

钩子是在 `session_start`、`post_tool_use`、`notification` 等事件发生时自动运行的 shell 命令（或 HTTP 调用）。如何编写自己的钩子，请参阅[创建自定义钩子](custom-hooks.md)。

钩子按来源分组：

- **全局钩子**——来自 `~/.grok/hooks/`
- **项目钩子**——来自仓库中的 `.grok/hooks/`
- **插件钩子**——随已安装插件捆绑
- **自定义钩子**——通过路径手动添加

每个钩子都会显示：

- 触发它的**事件**（例如 `session_start`、`post_tool_use`）
- 要运行的**命令**或 **URL**
- **超时**时长
- **状态**——启用或 `[disabled]`

### 钩子选项卡快捷键

| 键 | 操作 |
|-----|--------|
| `l` | 重新加载所有钩子 |
| `a` | 从路径添加钩子 |
| `r` | 移除选中的钩子 |
| `e` | 启用/禁用选中的钩子 |
| `Space` | 展开/折叠分组 |

---

## 插件选项卡

插件是包含任意组合的技能、智能体、钩子和 MCP 服务器配置的目录。

展开后，每个插件会显示：

- **名称**和**版本**
- **范围**——`user`、`project`、`cli` 或 Marketplace 源名称
- **技能**——名称或数量
- **智能体**——名称或数量
- **钩子**——数量
- **MCP 服务器**——数量（不受信任时显示 `"blocked"`）
- **描述**
- **冲突**——如有冲突显示 ⚠ 警告

插件钩子会自动接收 `GROK_PLUGIN_ROOT` 和 `GROK_PLUGIN_DATA` 环境变量（参阅[插件指南](../user-guide/zh-CN/09-plugins.md#environment-variables-in-plugin-hooks)）。

### 插件选项卡快捷键

| 键 | 操作 |
|-----|--------|
| `r` | 重新加载所有插件 |
| `i` | 从路径安装插件 |
| `e` | 启用/禁用选中的插件 |
| `Space` | 展开/折叠插件详情 |
| `/` | 按名称搜索插件 |

---

## Marketplace 选项卡

从已配置的 Marketplace 源浏览并安装插件。

源加载自：

1. **config.toml**——`[[marketplace.sources]]` 条目
2. **settings.json**——`~/.grok/settings.json` 或 `~/.claude/settings.json` 中的 `extraKnownMarketplaces`

每个源会显示其插件的：

- **名称**和**版本**
- **描述**
- **安装状态**——`[installed]`、`[installed • update: v1 → v2]` 或未安装

### Marketplace 选项卡快捷键

| 键 | 操作 |
|-----|--------|
| `i` | 安装选中的插件 |
| `d` | 卸载选中的插件 |
| `r` | 刷新 Marketplace 源（重新克隆/拉取 git 仓库） |
| `u` | 更新所有已安装的 Marketplace 插件 |
| `Space` | 展开/折叠源或插件 |
| `/` | 按名称搜索插件 |

### 添加 Marketplace 源

在 Marketplace 选项卡按 `a`（或运行 `grok-zh plugin marketplace add <source>`），并提供 git URL、GitHub 简写（`owner/repo`）或本地目录路径（`/absolute`、`~/dir` 或 `./relative`）。本地路径会作为 `path` 源保存，适合从现有检出目录开发 Marketplace。

源会写入 `~/.grok/config.toml`：

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

或者写入 `~/.grok/settings.json` / `~/.claude/settings.json`：

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" },
      "autoUpdate": true
    }
  }
}
```

---

## 通用键盘快捷键

这些快捷键在所有选项卡中都有效：

| 键 | 操作 |
|-----|--------|
| `Tab` / `→` | 下一个选项卡 |
| `Shift+Tab` / `←` | 上一个选项卡 |
| `j` / `↓` | 向下移动选择 |
| `k` / `↑` | 向上移动选择 |
| `Space` | 切换展开/折叠 |
| `/` | 开始搜索（插件和 Marketplace） |
| `Backspace` | 删除搜索字符，或重新进入搜索 |
| `Esc` | 清除搜索，或关闭模态界面 |
| `q` | 关闭模态界面 |

## 确认与错误

某些操作（例如卸载插件）可能会要求确认：

- 按 `y` 确认
- 按 `Esc` 或其他任意键取消

错误会以消息覆盖层显示——按任意键关闭。

操作进行期间，模态界面会显示“Processing...”，并在操作完成前阻止输入。

## 另请参阅

- [创建自定义钩子](custom-hooks.md)——分步介绍如何编写自己的钩子和脚本
- [钩子用户指南](../user-guide/zh-CN/10-hooks.md)——事件、匹配器和信任模型
- [钩子示例](../../../xai-grok-hooks/examples/README.zh-CN.md)——可直接使用的示例钩子
- [插件用户指南](../user-guide/zh-CN/09-plugins.md)——安装、信任和 Marketplace
