<div align="center">

<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://media.x.ai/v1/website/spacexai-symbol-white-transparent-0c31957f.png">
    <source media="(prefers-color-scheme: light)" srcset="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png">
    <img alt="SpaceXAI logo" src="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png" width="96">
  </picture>
  <br>
  Grok Build 简体中文社区版（<code>grok-zh</code>）
</h1>

这是基于官方 [xai-org/grok-build](https://github.com/xai-org/grok-build) Fork 的非官方简体中文社区版。

本项目在尽量保持原有功能、命令行参数、配置格式和协议兼容性的前提下，为 Grok Build 的 CLI、TUI、设置、提示信息和用户文档提供简体中文支持。它以独立程序名 `grok-zh` 与官方版并行使用，但有意共用 `~/.grok` 数据目录：会话、登录状态、配置、第三方 API、插件与本地状态在两个入口之间保持一致。

[项目定位](#项目定位) · [当前状态](#当前状态) · [Windows-安装](#windows-安装) · [从源码构建](#从源码构建) · [共享数据与兼容约定](#共享数据与兼容约定) · [文档](#文档) · [Releases](https://github.com/ljy6-6-6/grok-build-Chinese/releases) · [上游与发布策略](#上游与发布策略) · [许可证](#许可证)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

</div>

---

## 项目定位

- 官方 Grok Build 的产品介绍与服务说明见 [x.ai/cli](https://x.ai/cli)。
- 本仓库不是 SpaceXAI 官方发行版，也不代表官方翻译或服务承诺。
- `SOURCE_REV` 记录本仓库源码所对应的官方 monorepo 提交；发布时还会记录 Fork 的 Git 提交和工作树状态。
- 模型可用性、账号权限、订阅、远程会话、搜索、语音及其他在线能力依赖官方服务端，社区 Fork 无法保证。

## 当前状态

本仓库正在进行第一阶段汉化和 Windows 绿色构建验证。当前测试产物属于未签名预览版，不应视为稳定发布版。

已建立的产品与数据边界：

- 可执行文件：`grok-zh.exe`
- 与官方版共用的默认数据目录：`~/.grok`
- 两个程序共同使用的目录覆盖：`GROK_HOME`
- 默认界面语言：`zh-CN`，可用 `--locale en-US` 切换英文
- 社区版独立更新源完成前，内置更新器保持关闭并安全失败

> [!WARNING]
> `crates/codegen/xai-grok-pager/scripts/` 下的安装脚本及同模块内的 npm 包装仍来自官方上游，可能安装或覆盖官方 `grok`。社区版发布流程完成前，请勿使用这些脚本安装本 Fork。测试时只使用独立绿色测试包中的 `grok-zh.exe`。

## Windows 安装

`zh-dev Windows 预览版` 云构建现在直接上传包目录。下载 GitHub Artifact 后只需
解压一次，即可在包内运行社区安装器：

```powershell
Set-ExecutionPolicy -Scope Process Bypass
& .\Install-GrokZh.ps1
```

默认安装会把 `grok-zh`、`agent-zh` 加入当前用户 `Path`，与官方命令共存；可选
接管 `grok`、`agent`，也可将官方命令备份后移走。完整参数、回滚方式和共享数据
边界见 [Windows 自动安装说明](packaging/windows/INSTALL-WINDOWS.md)。

## 从源码构建

### 通用要求

- Rust：版本由 `rust-toolchain.toml` 固定。
- `protoc`：构建脚本会依次查找仓库内工具、`PATH` 和 `PROTOC`。
- 官方仓库主要支持 macOS 与 Linux；本 Fork 另行建设 Windows 构建和验证流程。

常用检查：

```sh
cargo check --locked -p xai-grok-pager-bin --bin grok-zh --features release-dist
cargo test --locked -p xai-grok-locale
cargo fmt --all --check
```

### Windows 绿色测试构建

下面的命令把 Cargo 缓存、构建输出和测试数据放在仓库忽略的
`.codex-local` 目录中。它仅用于本地开发测试，不属于正式安装器。

```powershell
$localRoot = Join-Path $PWD '.codex-local'
$env:CARGO_HOME = Join-Path $localRoot 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $localRoot 'target'
$env:GROK_HOME = Join-Path $localRoot 'test-home'
$env:GROK_VERSION = "0.2.121-zh.preview.1"
cargo build --frozen --target x86_64-pc-windows-gnu `
  -p xai-grok-pager-bin --profile release-dist --features release-dist
```

预期产物：

```text
.codex-local/target/x86_64-pc-windows-gnu/release-dist/grok-zh.exe
```

绿色测试包还会在 `grok-zh.exe` 同目录携带 `rg.exe`。社区版搜索入口优先使用该旁载工具，缺失时再回退到系统 `PATH`；这只隔离程序安装文件，不改变两个程序共用 `~/.grok` 数据的约定。

正式 Windows 发布仍需补齐 MSVC 构建、代码签名、安装包、DLL 闭包验证和独立自动更新流程。

## 共享数据与兼容约定

`grok` 与 `grok-zh` 直接读写同一个 `~/.grok`（或 `GROK_HOME`）目录，不使用复制或双向同步层。因此在任一入口创建、恢复、重命名或删除会话，登录或退出账号，修改模型、第三方 API、MCP、插件和用户配置，另一入口都会看到相同结果。若两个程序同时运行，它们也遵循上游已有的文件锁与并发规则。

以下名称必须保持稳定，不做翻译：

- CLI 子命令、参数与取值，例如 `agent`、`--resume`、`--output-format json`
- 配置键、环境变量和序列化字段，例如 `[ui] screen_mode`、`GROK_HOME`、JSON key
- MCP、ACP、OAuth、OIDC、OSC 52 等协议名
- 工具名、模型 ID、会话 ID、路径、URL、日志字段和服务端原始错误

协议身份、遥测字段或兼容性所需的内部 `grok-pager` 名称可能继续保留；中文版的程序名使用 `grok-zh`，用户数据路径与官方版共同使用 `.grok`。

## 文档

- Windows 自动安装：[`packaging/windows/INSTALL-WINDOWS.md`](packaging/windows/INSTALL-WINDOWS.md)
- 中文用户指南：[`crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md`](crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md)
- 中文入门教程：[`crates/codegen/xai-grok-pager/docs/tutorial/zh-CN/`](crates/codegen/xai-grok-pager/docs/tutorial/zh-CN/)
- 英文上游用户指南：[`crates/codegen/xai-grok-pager/docs/user-guide/README.md`](crates/codegen/xai-grok-pager/docs/user-guide/README.md)
- 贡献说明：[`CONTRIBUTING.zh-CN.md`](CONTRIBUTING.zh-CN.md)
- 安全策略：[`SECURITY.zh-CN.md`](SECURITY.zh-CN.md)
- 版本发布：[`Releases`](https://github.com/ljy6-6-6/grok-build-Chinese/releases)
- 官方在线文档：[docs.x.ai/build/overview](https://docs.x.ai/build/overview)

中文文档将使用稳定文档 ID 和 `zh-CN` 平行目录，不直接改变英文标题所承担的查找身份，以降低合并上游更新时的冲突。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/codegen/xai-grok-locale` | 集中式语言目录、locale 解析与回退 |
| `crates/codegen/xai-grok-product` | 社区版程序名、共享数据目录与更新安全策略 |
| `crates/codegen/xai-grok-pager-bin` | 组合入口，生成 `grok-zh` |
| `crates/codegen/xai-grok-pager` | TUI、回滚区、提示输入、模态框和渲染 |
| `crates/codegen/xai-grok-shell` | 智能体运行时及 leader/stdio/headless 入口 |
| `crates/codegen/xai-grok-tools` | 终端、文件编辑、搜索等工具实现 |
| `crates/codegen/xai-grok-workspace` | 文件系统、版本控制、执行和检查点 |

根 `Cargo.toml` 的大部分内容由上游生成。新增社区功能应优先放在独立 crate 或局部适配层中，避免对上游文件进行大范围结构改写。

## 上游与发布策略

- `main`：尽量保持官方上游镜像，只用于同步和审查。
- `zh-dev`：汉化开发、上游合并、构建和测试。
- 计划中的 `zh-stable`：只有在中文验证通过后才建立并用于稳定发布；当前尚未创建。
- 上游 `main` 更新只能触发审查和测试，不能直接进入用户更新源。
- 官方 stable 指针、正式更新日志、协议兼容检查和本 Fork 的 Windows 测试结果共同构成发布门槛。

## 贡献

本项目当前处于社区维护准备阶段。提交翻译时请保留命令、配置键、协议字段、代码块、占位符和 URL，并优先修改集中式 locale 目录；不要在业务代码中逐处硬编码中文。

上游仓库的外部贡献政策见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 许可证

本仓库第一方代码采用 **Apache License, Version 2.0**，详见 [`LICENSE`](LICENSE)。本 Fork 的修改继续遵守相同许可证，并保留上游版权和归属说明。

第三方及 vendored 代码保持各自原许可证，详见：

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
- [`third_party/NOTICE`](third_party/NOTICE)
