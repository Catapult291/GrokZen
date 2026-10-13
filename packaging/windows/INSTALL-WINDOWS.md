# Windows 安装说明

本文适用于从本仓库 [Releases](https://github.com/Catapult291/GrokZen/releases)
下载的 Windows x64 GNU 完整包。它不是 xAI 官方安装器。

## 下载与校验

1. 从 [Releases](https://github.com/Catapult291/GrokZen/releases) 下载
   `grok-zh-<version>-windows-x86_64-gnu.zip` 及同名 `.sha256` 文件。
2. 解压 ZIP，进入解压出的 `grok-zh-<version>-windows-x86_64-gnu` 目录。
   包内 `SHA256SUMS.txt` 是文件清单，`Install-GrokZh.ps1` 会在写入前自动核对哈希。

## 在线安装

不想下载 ZIP 时，在 Windows PowerShell 5.1 或 PowerShell 7 中粘贴下面一行：

```powershell
$p=Join-Path $env:TEMP ('grok-zh-install-'+[guid]::NewGuid().ToString('N')+'.ps1'); $tls=[Net.ServicePointManager]::SecurityProtocol; try { [Net.ServicePointManager]::SecurityProtocol=$tls -bor [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/Catapult291/GrokZen/zh-dev/packaging/windows/Install-GrokZhOnline.ps1' -OutFile $p; & "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File $p; if ($LASTEXITCODE -ne 0) { throw "安装未完成，退出码：$LASTEXITCODE" } } finally { [Net.ServicePointManager]::SecurityProtocol=$tls; Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue }
```

菜单提供共存安装、设置 `grok` / `agent` 命令、自定义目录和便携版；默认保留官方版，
全程中文提示，无需管理员权限。在线入口只选择本仓库可验证的最新正式 Release，不会
自动降级；下载与解压的临时文件在结束后清理。

1. 安装或更新：默认与官方版共存，安装到 `%LOCALAPPDATA%\Programs\grok-zh\bin`。
2. 安装并设置 `grok` / `agent` 启动命令：进入发布包自带的中文命令设置菜单。
3. 自定义安装目录。
4. 创建或更新便携版：指定一个新目录，或选择此前由本入口创建的便携目录。
0. 退出。

脚本读取本仓库最新正式、不可变 Release，核对当前平台 ZIP 的固定下载地址、大小、GitHub SHA-256、包内协议、路径和全部文件哈希后，调用包内安装器。新版不下载独立 `.sha256`，也不依赖其他平台附件。安装完成后核对实际程序版本，不自动启动完整界面。版本相同时可退出或修复安装；本地版本更高时停止，避免自动降级。

下载显示进度和速度，网络故障最多尝试 3 次；支持系统代理及 `HTTPS_PROXY` / `HTTP_PROXY`。失败信息与退出码会保留，下载和解压的临时目录在结束或取消时清理。需要安装历史版本或预发布时，使用下方手动下载安装流程。

### 便携版目录

```text
Grok 中文版/
├─ 启动.cmd
├─ 使用说明.md
└─ app/
   ├─ grok-zh.exe
   ├─ agent-zh.cmd
   ├─ rg.exe
   └─ 许可证、构建信息与安装记录
```

双击 `启动.cmd`，或在终端中运行它并传入原有参数；它保留当前工作目录、标准输入输出和退出码。机器接口可使用 `.\app\agent-zh.cmd stdio`。本模式不写入 PATH，账号、会话和配置仍与官方版共用 `~/.grok` / `GROK_HOME`。

更新完整便携版时重新运行在线入口并选择原目录。入口先在同级临时目录准备新版本，校验并切换成功后自动清理旧版本；激活失败时恢复旧目录。文件占用或包含额外数据时保留备份，下次更新重试。已有目录必须具有本入口的归属记录，且顶层只有上述 3 项；个人文件请存放在别处。内置自动更新仍只更新 `app/grok-zh.exe`，同步旁载工具与许可证时使用在线入口。在线更新完成后会显示该版本的 Release 更新日志。

### 在线安装高级参数

需要自动化时，先从本仓库下载 [Install-GrokZhOnline.ps1](https://raw.githubusercontent.com/Catapult291/GrokZen/zh-dev/packaging/windows/Install-GrokZhOnline.ps1) 并保存，再运行：

```powershell
# 按固定目录重建便携版；同版本重跑需显式指定 -Repair
& .\Install-GrokZhOnline.ps1 -Mode Portable -PortableDir 'D:\Apps\GrokZen' -Repair -NonInteractive

# 只下载并校验，不安装
& .\Install-GrokZhOnline.ps1 -VerifyOnly -NonInteractive
```

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

从 1.0.16 起每个平台只发布一个安装包：更新器验证当前平台完整 ZIP 的固定地址、
大小、GitHub SHA-256、包内协议与清单以及候选程序版本，不再依赖独立 `.sha256`
或其他平台附件。独立 `.sha256` 在约两个月兼容期内继续保留，之后停止公开发布；
包内 `SHA256SUMS.txt` 继续用于文件校验。

## 数据与安全边界

- 程序安装目录（`%LOCALAPPDATA%\Programs\grok-zh\bin`）与用户数据目录
  （`~/.grok` 或 `GROK_HOME`）是两回事。
- 中文版与官方版**共用** `~/.grok`：会话、登录、配置、插件在两边即时同步。
- 不要为了卸载而删除整个 `~/.grok`。
- 当前包未做 Authenticode 签名，首次运行可能触发 SmartScreen；请只从本仓库
  Releases 下载，并核对 `BUILD-INFO.txt` 与 Release 构建信息。

### 高级：卸载官方程序并接管命令

如果除接管命令名外，还希望卸载已验证的官方程序，运行：

```powershell
& .\Install-GrokZh.ps1 -UninstallOfficial
```

`-UninstallOfficial` 会自动启用 `-OverrideOfficialCommands`。默认检查：

```text
%GROK_HOME%\bin
%GROK_BIN_DIR%（已设置时）
Path 中 grok.exe、agent.exe 所在的目录
npm root --global 下的 @xai-official/grok
```

未设置 `GROK_HOME` 时，按 `%USERPROFILE%\.grok` 处理。只有官方名称且通过有效
X.AI LLC 签名验证的程序才会删除；来源无法验证的同名文件保持原样。npm 包通过
精确包名和实际全局安装前缀识别，并由 npm 卸载。显式传入 `-GrokHome` 时，处理范围
限定为该目录的 `bin`，不会检查全局命令或 npm 包。

这项操作不会删除或修改共享的 `auth.json`、`config.toml`、会话、第三方 API、
MCP、插件、缓存或其他 `~/.grok` 数据。若文件正被占用，安装器会保留原文件并
提示关闭相关进程后重试，不会强行结束进程。

菜单和命令行使用相同的身份验证与卸载逻辑，均不创建备份。其他包管理器安装的
包记录不会被猜测性删除；可用以下命令检查当前安装：

```powershell
npm list -g --depth=0
Get-Command grok, agent -All
```

## 重新安装官方版

需要恢复官方命令时，先重新运行不带接管选项的安装命令，移除中文版的 `grok.cmd`、
`agent.cmd`，然后安装官方 npm 包：

```powershell
& .\Install-GrokZh.ps1
npm install -g @xai-official/grok
```

重新打开终端后，用 `Get-Command grok, agent -All` 检查解析结果。旧版安装器已经
创建的 `official-backup` 目录会保留，升级不会擅自删除这些已有文件。

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
重命名为同级的 `bin.previous.<timestamp>-<id>`，在更新过程中用于回滚；成功后清理。
文件仍被占用时，下次更新会重试清理。清理也识别内置更新器遗留的、带进程号和序号的
下载 ZIP 与候选 EXE；只识别受管旧目录中的精确文件名，不会将任意 ZIP、EXE 或嵌套目录内的额外文件视为更新残留。
含个人文件的目录会保留，历史 `official-backup`
只有在新目录中完整保留后，才会随旧安装目录一并清理其重复副本。

`-GrokHome`（以及环境变量 `GROK_HOME`）必须解析为绝对路径。安装器会展开已经
定义的 `%USERPROFILE%` 这类 Windows 环境变量；无法展开的 `%VAR%` 或字面的
`$env:VAR` 会被拒绝，请先在 PowerShell 中解析后再传入。
安装器只用该参数定位可选的官方 `grok.exe`、`agent.exe`，不会替你持久化
`GROK_HOME`；若程序运行时也要使用自定义数据根，请另行设置同值的环境变量。

## 通过 GitHub Releases 自动更新

安装带社区更新器的版本后，程序启动时会查询固定仓库
`Catapult291/GrokZen`：

- 默认 `stable` 只接受 immutable、非 Draft、非 prerelease 的严格三段版本。`v1.0.8`
  是旧更新器可见的最后一个桥接 Tag；后续版本使用 `release-vA.B.C`，二进制报告的版本仍是
  `A.B.C`，不会增加第四段社区修订号；
- `grok-zh update --alpha` 可选择预发布通道，`grok-zh update --stable` 可切回稳定通道；
- 更新器接受精确命名的完整 `grok-zh-<version>-windows-x86_64-gnu.zip`，验证固定 URL、
  大小、GitHub SHA-256、安全 ZIP 布局和包内 `SHA256SUMS.txt`；新版读取 `BUILD-INFO.txt` 中的声明协议，不要求独立 sidecar；
- 社区版的“自动更新”设置默认关闭。此时启动只查询版本并显示提示，不下载；欢迎页按
  `Ctrl+U` 后才退出旧 TUI、下载 ZIP 并执行更新。交互式下载会显示大小、百分比、速度和
  预计剩余时间；输出重定向或后台更新时进度条自动隐藏。显式开启该设置后才允许后台预下载；
- ZIP 中的候选 EXE 必须通过 `--version`，之后才使用 Windows 的重命名旁置和失败回滚逻辑
  替换当前 `grok-zh.exe`；不会强制结束其他会话；完成后重新运行 `grok-zh`；
- `v1.0.0-zh.preview.3` 的旧更新器只认识裸 EXE，需要手工下载完整 ZIP 迁移；已撤回的
  `v1.0.0.1` 使用了不兼容代理版本门禁的四段版本号，不应继续安装或分发。
- 已发布的 `v1.0.3`、`v1.0.5` 会先更新到仅含 Windows 两个资产、扁平 ZIP 和 7 项兼容清单的
  `v1.0.8`。从首个 `release-v*` 版本起，Windows、macOS、Linux 共用六资产 Release；旧客户端
  无法识别 `release-v*`，因此不会跳过桥接版本。

停止公开独立 `.sha256` 后，旧现代客户端会先升级到永久保留的最后一个六资产过渡版，
再由其新更新器升级到三资产 Release。包内声明协议与维护步骤见
[单包更新协议](https://github.com/Catapult291/GrokZen/blob/zh-dev/docs/COMMUNITY-UPDATE-PROTOCOL.md)。

自动激活仍沿用原版的单 EXE 替换语义，但传输与验证只使用完整 ZIP；不会在运行中的安装
目录内逐个改写 `agent-zh.cmd`、`rg.exe`、安装文档、Path 或官方 `grok`/`agent`。若某个版本
要求同步升级这些旁载文件，应重新运行已下载 ZIP 内的安装器。

更新器不会读取 `GROK_INSTALLER` 来切换来源，也不会回退到 xAI npm、官方 GitHub、x.ai
或 GCS。网络、元数据、digest、解压、内层清单、候选运行或文件替换任一步失败都会保持
当前 EXE。

## 数据共享与安全边界

程序安装目录和用户数据目录是两件事：

- 中文版程序默认安装到 `%LOCALAPPDATA%\Programs\grok-zh\bin`；
- 官方版与中文版有意共用 `~/.grok`（或 `GROK_HOME`）；
- 登录状态、会话删除、配置、第三方 API、MCP、插件和本地状态会在两个入口之间
  立即同步，不存在复制或双向同步层；
- 不要为了卸载任一程序而删除整个 `~/.grok`。

当前 Windows 包没有 Authenticode 签名，CI 只构建、剥离、校验和打包，不启动完整 TUI。
Immutable Release 与 SHA-256 不能替代代码签名；首次运行前请查看 `BUILD-INFO.txt`、
Release Tag、构建提交和哈希记录。

> 仓库中的 `crates/codegen/xai-grok-pager/scripts/install.ps1`、`install.sh` 与
> `@xai-official/grok` 属于官方上游安装链，不能用于安装或更新本社区版。
