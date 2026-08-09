# Windows 自动安装说明

本说明适用于 `zh-dev` GitHub Actions 生成的未签名 Windows x64 GNU 预览包。
它不是 xAI 官方安装器，也不是稳定版签名安装包。

## 下载与解压

1. 在仓库的 **Actions → zh-dev Windows preview** 中下载本次运行的
   `grok-zh-windows-<run-number>` Artifact。
2. 解压一次。新流水线直接上传包目录，不再让 Artifact ZIP 内再套一层 ZIP。
3. 确认目录中至少包含：

   ```text
   grok-zh.exe
   agent-zh.cmd
   rg.exe
   Install-GrokZh.ps1
   INSTALL-WINDOWS.md
   SHA256SUMS.txt
   BUILD-INFO.txt
   ```

`Install-GrokZh.ps1` 会在写入任何安装目录前，自动核对 `SHA256SUMS.txt` 中的
文件哈希。Artifact 本身的 SHA-256 也会显示在 Actions 构建摘要中。

## 默认安装：与官方版共存

在解压后的目录中打开 PowerShell，运行：

```powershell
Set-ExecutionPolicy -Scope Process Bypass
& .\Install-GrokZh.ps1
```

默认安装位置是：

```text
%LOCALAPPDATA%\Programs\grok-zh\bin
```

安装器会把该目录放到当前用户的 `Path` 最前方，并同步当前 PowerShell 进程。
这不会写入 Machine 级 `Path`，不需要管理员权限。其他已经打开的终端需要重新
打开一次。

默认提供两个不会占用官方名称的命令：

```powershell
grok-zh --version
agent-zh --help
```

- `grok-zh` 启动中文版 TUI/CLI。
- `agent-zh` 是包装命令，等价于 `grok-zh agent ...`；例如
  `agent-zh stdio`、`agent-zh headless`。
- `rg.exe` 与 `grok-zh.exe` 保持在同一目录，供内置搜索使用。

这里修改的是用户 `Path`，不是创建名为 `grok-zh` 或 `agent-zh` 的环境变量。

## 可选：接管 `grok` 和 `agent` 命令名

如果希望在终端中直接输入 `grok` 和 `agent` 时使用中文版，运行：

```powershell
& .\Install-GrokZh.ps1 -OverrideOfficialCommands
```

该选项不会覆盖官方 `grok.exe` 或 `agent.exe`。它会在中文版安装目录中创建
`grok.cmd` 与 `agent.cmd`，并利用该目录位于用户 `Path` 最前方来接管命令解析。
删除这两个 shim 或重新执行不带该开关的安装，即可恢复默认共存模式。

这里只调整用户级 `Path`。如果同名程序来自更靠前的 Machine 级 `Path`，Windows
仍可能优先解析该程序；安装后必须用下面的命令核对实际结果。此时可卸载对应的
Machine 级安装，或继续显式使用 `grok-zh`、`agent-zh`，安装器不会擅自修改
Machine 级环境变量。

可用以下命令确认当前解析到哪个文件：

```powershell
Get-Command grok-zh, agent-zh, grok, agent -All
```

## 可选：备份并移走官方命令

如果除接管命令名外，还希望移除官方安装器放在共享目录中的两个入口，运行：

```powershell
& .\Install-GrokZh.ps1 -UninstallOfficial
```

`-UninstallOfficial` 会自动启用 `-OverrideOfficialCommands`，并且只检查：

```text
%GROK_HOME%\bin\grok.exe
%GROK_HOME%\bin\agent.exe
```

未设置 `GROK_HOME` 时，`%GROK_HOME%` 按 `%USERPROFILE%\.grok` 处理。找到的文件
不会直接删除，而会连同 SHA-256 记录一起移动到：

```text
%LOCALAPPDATA%\Programs\grok-zh\bin\official-backup\<timestamp>\
```

这项操作不会删除或修改共享的 `auth.json`、`config.toml`、会话、第三方 API、
MCP、插件、缓存或其他 `~/.grok` 数据。若文件正被占用，安装器会保留原文件并
提示关闭相关进程后重试，不会强行结束进程。

如果官方版由 npm 或其他包管理器安装，它们在其他目录中的 shim/包记录不会被
这个开关猜测性删除。可先检查：

```powershell
npm list -g --depth=0
Get-Command grok, agent -All
```

确认确实通过 npm 安装后，才单独执行：

```powershell
npm uninstall -g @xai-official/grok
```

## 恢复官方命令

备份目录中的 `official-backup.json` 记录了原路径和哈希。关闭相关进程后，可将
对应的 `grok.exe`、`agent.exe` 移回记录的 `original_path`。恢复前请先用
`Get-Command grok, agent -All` 检查是否已有同名文件，避免覆盖后来安装的版本。

## 自定义选项

```powershell
# 自定义程序安装目录
& .\Install-GrokZh.ps1 -InstallDir 'D:\Apps\grok-zh\bin'

# 指定要检查官方命令的共享数据根
& .\Install-GrokZh.ps1 -GrokHome 'D:\GrokData' -UninstallOfficial

# 绿色复制，不修改用户 Path
& .\Install-GrokZh.ps1 -InstallDir 'D:\Apps\grok-zh\bin' -NoPathUpdate

# 仅预览将执行的安装，不写文件
& .\Install-GrokZh.ps1 -WhatIf
```

如果目标目录已存在但不是本安装器创建的，安装器会拒绝覆盖。只有在你已经检查
该目录并确认可以整体替换时，才使用 `-Force`。升级已有社区安装时，旧目录会被
重命名为同级的 `bin.previous.<timestamp>-<id>`，便于回滚。

`-GrokHome`（以及环境变量 `GROK_HOME`）必须是已经展开的绝对路径；不要把
`%USERPROFILE%` 这样的未展开占位符作为其字面值。
安装器只用该参数定位可选的官方 `grok.exe`、`agent.exe`，不会替你持久化
`GROK_HOME`；若程序运行时也要使用自定义数据根，请另行设置同值的环境变量。

## 数据共享与安全边界

程序安装目录和用户数据目录是两件事：

- 中文版程序默认安装到 `%LOCALAPPDATA%\Programs\grok-zh\bin`；
- 官方版与中文版有意共用 `~/.grok`（或 `GROK_HOME`）；
- 登录状态、会话删除、配置、第三方 API、MCP、插件和本地状态会在两个入口之间
  立即同步，不存在复制或双向同步层；
- 不要为了卸载任一程序而删除整个 `~/.grok`。

本预览包没有 Authenticode 签名，CI 只构建、剥离、校验和打包，不会启动生成的
可执行文件。首次运行前请查看 `BUILD-INFO.txt`、Actions 构建提交和哈希记录。

> 仓库中的 `crates/codegen/xai-grok-pager/scripts/install.ps1`、`install.sh` 与
> `@xai-official/grok` 属于官方上游安装链，不能用于安装或更新本社区版。
