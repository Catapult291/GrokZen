<div align="center">

# GrokZen（grok-zh）

**Grok Build 简体中文社区版，源码层禁用遥测回传。**

<p>
  <img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0">
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg" alt="Platform">
  <img src="https://img.shields.io/badge/locale-zh--CN%20%7C%20en--US-orange.svg" alt="Locale">
</p>

非 SpaceXAI 官方发行版。

</div>

---

## 项目简介

本项目是基于 xAI 官方 [Grok Build](https://github.com/xai-org/grok-build)
终端编程智能体构建的社区版本，面向中文用户提供**简体中文界面与文档**，
并在源码层面**禁用遥测数据回传**，兼顾使用习惯与隐私。

## 解决什么问题

GrokZen 在保留完整功能与官方协议兼容的前提下，针对隐私与本地化做了以下处理：

- **去遥测**：Mixpanel / 产品事件上报与 GCS 研究/会话追踪上传路径在源码层禁用
- **隐私防护**：上传路径在编译期由 `privacy` 特性排除，无法被环境变量、配置或
  服务端远程设置重新启用
- **厂商更新禁用**：不访问 x.ai 官方更新通道，避免被替换回官方版
- **中文界面**：CLI、TUI、设置、提示与文档全简体中文，可 `--locale en-US` 切英文

> 隐私验证三层证据链（线路层测试版本 **v1.0.13**）见 [DEVELOPER.md](DEVELOPER.md#隐私验证证据v1013)。

## 致谢

感谢 **Linux Do 社区**（[linux.do](https://linux.do)）在本项目开发交流中提供的
讨论与支持。

## 功能

| | |
|---|---|
| 简体中文 | CLI / TUI / 设置 / 提示 / 文档全中文 |
| 去遥测 | Mixpanel、GCS 上报路径源码层禁用（编译期排除） |
| 兼容官方 | 与官方版共用 `~/.grok` 数据目录，会话/登录/配置互通 |
| 独立程序名 | `grok-zh` / `agent-zh`，不与官方 `grok` / `agent` 冲突 |
| 自动更新 | 只从本仓库 Releases 更新，永不访问 x.ai 官方通道 |
| 跨平台 | Windows x64、macOS ARM64、Linux x86_64 |
| 中文会话标题 | 中文请求自动生成中文标题与计划步骤 |
| 动态公告翻译 | 公告中文映射可独立于程序更新（1.0.16 起） |

## 安装

从 [Releases](https://github.com/Catapult291/GrokZen/releases) 下载对应平台的完整包，
校验 SHA-256 后运行安装器。

### Windows

解压后双击 `一键安装.cmd`，完成后新终端运行：

```powershell
grok-zh
```

也可以不下载 ZIP，直接在 PowerShell 中粘贴下面一行，按中文菜单在线安装或更新最新
正式版，或创建便携版（支持 Windows PowerShell 5.1 与 PowerShell 7，无需管理员权限）：

```powershell
$p=Join-Path $env:TEMP ('grok-zh-install-'+[guid]::NewGuid().ToString('N')+'.ps1'); $tls=[Net.ServicePointManager]::SecurityProtocol; try { [Net.ServicePointManager]::SecurityProtocol=$tls -bor [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/Catapult291/GrokZen/zh-dev/packaging/windows/Install-GrokZhOnline.ps1' -OutFile $p; & "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File $p; if ($LASTEXITCODE -ne 0) { throw "安装未完成，退出码：$LASTEXITCODE" } } finally { [Net.ServicePointManager]::SecurityProtocol=$tls; Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue }
```

在线入口只选择本仓库可验证的最新正式 Release，不自动降级；便携目录顶层只有
`启动.cmd`、`使用说明.md` 与 `app/`，不修改 PATH。详细参数见
[Windows 安装说明](packaging/windows/INSTALL-WINDOWS.md)。

### macOS（Apple Silicon）

```sh
./Install-GrokZh.sh
# 完成后
grok-zh
```

### Linux x86_64

```sh
./Install-GrokZh.sh
# 完成后
grok-zh
```

各平台详细步骤、校验命令与安全边界见
[Windows](packaging/windows/INSTALL-WINDOWS.md) ·
[macOS](packaging/macos/INSTALL-MACOS.md) ·
[Linux](packaging/linux/INSTALL-LINUX.md)。

## 用法

```sh
grok-zh                    # 启动 TUI
agent-zh stdio             # 以 stdio 模式作为 agent 运行
agent-zh headless          # 无头模式
grok-zh --locale en-US     # 英文界面
grok-zh update             # 手动检查更新
```

首次启动会打开浏览器完成身份验证。完整中文用户指南见
[`crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md`](crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md)。

## 更新与公告译文

更新器只读取本仓库的不可变 GitHub Releases；官方 npm、GitHub、x.ai 与 GCS 更新通道
始终禁用。从 1.0.16 起每个平台只发一个安装包，独立 `.sha256` 侧车文件保留约两个月
兼容期后停止发布，校验改由包内 `SHA256SUMS.txt` 与 GitHub digest 完成。

中文公告译文从 1.0.16 起可独立于程序更新：程序在加载官方公告时并行检查译文版本，
不设轮询定时器；GitHub 不可用时沿用缓存或内置译文，未收录内容显示官方英文原文。
维护流程见 [`community/announcements/README.md`](community/announcements/README.md)。

## 从源码构建

依赖：Rust（版本见 `rust-toolchain.toml`）、[DotSlash](https://dotslash-cli.com)
（用于拉取 `bin/protoc`）。

```sh
cargo run -p xai-grok-pager-bin                  # 运行
cargo build --locked -p xai-grok-pager-bin --release
cargo test --locked -p xai-grok-locale
```

产物：`target/release/grok-zh`（Windows 为 `grok-zh.exe`）。
完整开发说明见 [DEVELOPER.md](DEVELOPER.md)。

## 项目结构

| 路径 | 内容 |
|---|---|
| `crates/codegen/xai-grok-pager` | TUI 界面与渲染 |
| `crates/codegen/xai-grok-shell` | 智能体运行时 |
| `crates/codegen/xai-grok-locale` | 集中式中文语言包 |
| `crates/codegen/xai-grok-tools` | 终端/文件/搜索等工具 |
| `packaging/` | 各平台安装器与文档 |
| `docs/` | 截图与隐私验证证据 |

完整目录见 [DEVELOPER.md](DEVELOPER.md#仓库结构)。

## 上游与许可

本项目是社区衍生构建，遵循 Apache-2.0：

- **官方上游**：[xai-org/grok-build](https://github.com/xai-org/grok-build)（SpaceXAI）
- **中文汉化**：[JoyElliot/grok-build-Chinese](https://github.com/JoyElliot/grok-build-Chinese)
- **去遥测补丁**：[thedavidweng/gork-build](https://github.com/thedavidweng/gork-build)（Gork Build）

保留上述上游的版权与归属声明，详见 [`NOTICE`](NOTICE) 与 [`LICENSE`](LICENSE)。
本项目与官方版共享 `~/.grok` 数据目录，仅为兼容，与 SpaceXAI 无关联。

## 许可证

Apache License 2.0，见 [`LICENSE`](LICENSE)。第三方组件许可见
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)。
