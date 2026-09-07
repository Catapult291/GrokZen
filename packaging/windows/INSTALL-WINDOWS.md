# Windows 安装说明

本文适用于从本仓库 [Releases](https://github.com/Catapult291/GrokZen/releases)
下载的 Windows x64 GNU 完整包。它不是 xAI 官方安装器。

## 下载与校验

1. 从 [Releases](https://github.com/Catapult291/GrokZen/releases) 下载
   `grok-zh-<version>-windows-x86_64-gnu.zip` 及同名 `.sha256` 文件。
2. 解压 ZIP，进入解压出的 `grok-zh-<version>-windows-x86_64-gnu` 目录。
   包内 `SHA256SUMS.txt` 是文件清单，`Install-GrokZh.ps1` 会在写入前自动核对哈希。

## 一键安装（与官方版共存，推荐）

在包目录中双击：

```text
一键安装.cmd
```

它会启动 PowerShell（仅本次使用 `ExecutionPolicy Bypass`，不改永久策略），
校验 SHA-256 并复制程序到：

```text
%LOCALAPPDATA%\Programs\grok-zh\bin
```

然后把该目录加到当前用户的 `Path` 最前方（不写 Machine 级 `Path`，无需管理员权限）。
完成后关闭并重新打开终端，输入：

```powershell
grok-zh        # 启动中文 TUI
agent-zh       # 等价于 grok-zh agent ...
```

默认提供 `grok-zh`、`agent-zh` 两个命令，不占用官方 `grok` / `agent` 名称。

## 可选：直接使用 grok / agent 命令

如希望输入 `grok` / `agent` 时启动中文版，双击：

```text
[可选]替换原始启动方式.cmd
```

菜单提供"保留官方版（推荐，仅创建 shim）"或"备份并停用官方入口"两个方案，
都不会覆盖官方 `grok.exe` / `agent.exe`，也不会改动共享的 `~/.grok` 数据。

## 自动更新

程序内置更新器只检查本仓库 Releases：

```powershell
grok-zh update          # 检查稳定版
grok-zh update --alpha  # 检查预览版
```

默认不后台自动下载；启动时只提示，按 `Ctrl+U` 才下载并安装。更新器校验
URL、SHA-256、ZIP 布局与包内清单后才替换 `grok-zh.exe`，失败会保留当前版本。

## 数据与安全边界

- 程序安装目录（`%LOCALAPPDATA%\Programs\grok-zh\bin`）与用户数据目录
  （`~/.grok` 或 `GROK_HOME`）是两回事。
- 中文版与官方版**共用** `~/.grok`：会话、登录、配置、插件在两边即时同步。
- 不要为了卸载而删除整个 `~/.grok`。
- 当前包未做 Authenticode 签名，首次运行可能触发 SmartScreen；请只从本仓库
  Releases 下载，并核对 `BUILD-INFO.txt` 与 Release 构建信息。

> 仓库内 `crates/codegen/xai-grok-pager/scripts/install.ps1` 与 `@xai-official/grok`
> 属于**官方上游安装链**，不能用于安装或更新本社区版。
