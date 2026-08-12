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

[项目定位](#项目定位) · [当前状态](#当前状态) · [Windows-安装](#windows-安装) · [从源码构建](#从源码构建) · [共享数据与兼容约定](#共享数据与兼容约定) · [文档](#文档) · [开发](#开发) · [Releases](https://github.com/ljy6-6-6/grok-build-Chinese/releases) · [上游与发布策略](#上游与发布策略) · [许可证](#许可证)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

</div>

---

## 项目定位

- 官方 Grok Build 的产品介绍与服务说明见 [x.ai/cli](https://x.ai/cli)。
- 本仓库不是 SpaceXAI 官方发行版，也不代表官方翻译或服务承诺。
- `SOURCE_REV` 记录本仓库源码所对应的官方 monorepo 提交；发布时还会记录 Fork 的 Git 提交和工作树状态。
- 模型可用性、账号权限、订阅、远程会话、搜索、语音及其他在线能力依赖官方服务端，社区 Fork 无法保证。

## 当前状态

本仓库正在进行第一阶段汉化、Windows 绿色构建和社区 Release 更新链验证。当前 Windows 产物仍未经过 Authenticode 签名，不应把预发布包视为正式签名安装包。

已建立的产品与数据边界：

- 可执行文件：`grok-zh.exe`
- 与官方版共用的默认数据目录：`~/.grok`
- 两个程序共同使用的目录覆盖：`GROK_HOME`
- 默认界面语言：`zh-CN`，可用 `--locale en-US` 切换英文
- 内置更新器只读取本仓库的 Immutable GitHub Releases；官方 npm、GitHub、x.ai 和 GCS 更新源始终禁用

> [!WARNING]
> `crates/codegen/xai-grok-pager/scripts/` 下的安装脚本及同模块内的 npm 包装仍来自官方上游，可能安装或覆盖官方 `grok`。社区版发布流程完成前，请勿使用这些脚本安装本 Fork。测试时只使用独立绿色测试包中的 `grok-zh.exe`。

## Windows 安装

正式 Tag 工作流会在 [Releases](https://github.com/ljy6-6-6/grok-build-Chinese/releases)
中发布完整 Windows ZIP；`CI` 工作流仍会上传短期 Actions Artifact。
解压完整包后，在包内运行社区安装器：

```powershell
Set-ExecutionPolicy -Scope Process Bypass
& .\Install-GrokZh.ps1
```

默认安装会把 `grok-zh`、`agent-zh` 加入当前用户 `Path`，与官方命令共存；可选
接管 `grok`、`agent`，也可将官方命令备份后移走。完整参数、回滚方式和共享数据
边界见 [Windows 自动安装说明](packaging/windows/INSTALL-WINDOWS.md)。

### 自动更新

- 默认使用 `stable` 通道，只接受 `vX.Y.Z`、非 Draft、非 prerelease 且已经进入
  GitHub immutable 状态的本仓库 Release。
- `alpha` 通道可通过 `grok-zh update --alpha` 显式选择；它也只接受 immutable
  Release，并按 SemVer 选择稳定版与预发布版中的较新版本。
- Release 资产必须精确包含完整
  `grok-zh-<version>-windows-x86_64-gnu.zip` 及其 `.sha256` sidecar，不再发布单独的
  版本化裸 EXE。更新器核对固定 URL、大小、GitHub SHA-256、ZIP 安全布局和包内
  `SHA256SUMS.txt`，再运行候选 `grok-zh.exe --version`。
- 社区版默认关闭后台自动更新：启动时只检查 Release 元数据并显示欢迎页提示，不下载文件。
  按 `Ctrl+U` 是对本次下载和安装的明确授权；随后重新运行 `grok-zh`。只有在设置中显式
  开启“自动更新”后，启动流程才可在后台预下载和安装。
- 自动激活只替换已验证的 `grok-zh.exe`，不会在仍有进程运行时冒险逐个覆盖
  `agent-zh.cmd`、`rg.exe`、安装器或文档；需要同步旁载文件的版本应重新运行 ZIP 内安装器。
  下载、解压、校验、冒烟或替换失败时保留当前版本。
- 旧版社区程序没有这套更新器，第一次升级到带更新器的版本仍需手工下载完整 ZIP 安装；
  之后才会跟随 Releases。

不满足上述契约的旧预览 Release、可变 Release、含额外裸 EXE 的 Release 和官方 xAI 资产
都不会被自动更新器接受。`v1.0.0-zh.preview.3` 使用旧的裸 EXE 更新契约，因此迁移到首个
ZIP-only 版本需要手工下载完整 ZIP 安装一次。Immutable Release 与 SHA-256 提供发布对象和
传输完整性校验。正式 Tag 工作流还会通过 GitHub Artifact Attestations 为 ZIP 和 `.sha256`
生成与仓库、提交及构建工作流绑定的来源证明，不需要维护长期签名私钥。

下载正式资产后，可用 GitHub CLI 同时验证不可变 Release、Release 资产以及 Actions 构建来源。
把 `OWNER` 替换为仓库当前所属的 GitHub 用户名：

```powershell
$repo = 'OWNER/grok-build-Chinese'
$tag = 'v1.0.0'
$zip = '.\grok-zh-1.0.0-windows-x86_64-gnu.zip'
$assets = @($zip, "$zip.sha256")

gh release verify $tag --repo $repo
foreach ($asset in $assets) {
  gh release verify-asset $tag $asset --repo $repo
  gh attestation verify $asset --repo $repo `
    --signer-workflow "$repo/.github/workflows/zh-release-windows.yml" `
    --source-ref "refs/tags/$tag"
}
```

这些证明用于核对发布对象和云端构建来源，但不等同于 Windows Authenticode 签名，
也不会让未签名 EXE 自动获得 SmartScreen 发布者信誉。

## 从源码构建

### 通用要求

- Rust：版本由 `rust-toolchain.toml` 固定。
- [DotSlash](https://dotslash-cli.com)：用于下载并运行 `bin/` 下的密封工具，尤其是 `bin/protoc`。构建前请确保 `dotslash` 已加入 `PATH`：

  ```sh
  cargo install dotslash
  # 或使用预编译软件包：https://dotslash-cli.com/docs/installation/
  dotslash --help
  ```

- `protoc`：构建脚本优先通过 DotSlash 解析仓库内的 `bin/protoc`，也会回退到 `PATH` 或 `PROTOC` 指定的程序。
- 官方仓库主要支持 macOS 与 Linux；本 Fork 另行建设 Windows 构建和验证流程。

常用检查：

```sh
cargo run -p xai-grok-pager-bin
cargo build --locked -p xai-grok-pager-bin --release
cargo check --locked -p xai-grok-pager-bin --bin grok-zh --features release-dist
cargo test --locked -p xai-grok-locale
cargo fmt --all --check
```

普通 release 构建的产物为 `target/release/grok-zh`（Windows 为 `grok-zh.exe`）。首次启动会打开浏览器完成身份验证；详见[身份验证指南](crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/02-authentication.md)。

### Windows 绿色测试构建

下面的命令把 Cargo 缓存、构建输出和测试数据放在仓库忽略的
`.codex-local` 目录中。它仅用于本地开发测试，不属于正式安装器。

```powershell
$localRoot = Join-Path $PWD '.codex-local'
$env:CARGO_HOME = Join-Path $localRoot 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $localRoot 'target'
$env:GROK_HOME = Join-Path $localRoot 'test-home'
$env:GROK_VERSION = "1.0.0-zh.preview.1"
cargo build --frozen --target x86_64-pc-windows-gnu `
  -p xai-grok-pager-bin --profile release-dist --features release-dist
```

预期产物：

```text
.codex-local/target/x86_64-pc-windows-gnu/release-dist/grok-zh.exe
```

绿色测试包还会在 `grok-zh.exe` 同目录携带 `rg.exe`。社区版搜索入口优先使用该旁载工具，缺失时再回退到系统 `PATH`；这只隔离程序安装文件，不改变两个程序共用 `~/.grok` 数据的约定。

正式 Windows 发布仍需补齐 MSVC 构建、代码签名、安装包和 DLL 闭包验证；社区自动更新链
已经接入本仓库 Releases，并只消费经过双层校验的 Windows GNU 完整 ZIP。

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
- 1.0.0 简体中文发行说明：[`crates/codegen/xai-grok-shell/changelogs/1.0.0.zh-CN.md`](crates/codegen/xai-grok-shell/changelogs/1.0.0.zh-CN.md)
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
| `crates/codegen/...` | CLI 依赖闭包中的其他配置、MCP、Markdown、沙箱等 crate |
| `crates/common/`、`crates/build/`、`prod/mc/` | 依赖闭包中少量共享与构建辅助 crate |
| `third_party/` | 仓库内 vendored 的 Mermaid 相关源码；归属见其中的 `NOTICE` |

> [!IMPORTANT]
> 根 `Cargo.toml`（工作区成员、依赖版本、lint 和 profile）由上游生成，应视为只读。新增社区功能应优先放在独立 crate 或局部适配层中，避免对上游文件进行大范围结构改写。

## 开发

工作区很大，日常检查应优先指定具体 crate：

```sh
cargo check -p <crate>
cargo test -p xai-grok-config
cargo clippy -p <crate>
cargo fmt --all
```

上游仓库不接受外部拉取请求；本社区 Fork 尚未公布独立贡献流程。开始修改前请先阅读[中文贡献说明](CONTRIBUTING.zh-CN.md)，并通过本仓库 Issue 与维护者沟通。

## 上游与发布策略

- `main`：尽量保持官方上游镜像，只用于同步和审查。
- `zh-dev`：汉化开发、上游合并、构建和测试。
- 计划中的 `zh-stable`：只有在中文验证通过后才建立；稳定 Release 由指向已审核提交的纯
  SemVer Tag（`vX.Y.Z`）触发。
- 上游 `main` 更新只能触发审查和测试，不能直接进入用户更新源。
- GitHub 发布页正文统一使用中文；每条提交名称链接到对应的 GitHub 提交页面。若提交标题
  不是中文，必须先在 `.github/release-notes/commit-titles.zh-CN.json` 中按完整 SHA 提供
  可审查的中文标题，否则发布会在构建前失败。
- 合并上游的提交必须在同一映射文件中登记已审核的父提交与合并基线；生成器核对 Git
  提交图后，生成独立的“上游更新”区块，列出上游比较范围和实际同步的上游提交链接。
- 正式更新日志、协议兼容检查、本 Fork 的 Windows 测试结果、Immutable Releases 开关和
  精确资产摘要共同构成发布门槛；官方 stable 指针不参与社区更新。

## 贡献

本项目当前处于社区维护准备阶段。提交翻译时请保留命令、配置键、协议字段、代码块、占位符和 URL，并优先修改集中式 locale 目录；不要在业务代码中逐处硬编码中文。

上游仓库的外部贡献政策见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 许可证

本仓库第一方代码采用 **Apache License, Version 2.0**，详见 [`LICENSE`](LICENSE)。本 Fork 的修改继续遵守相同许可证，并保留上游版权和归属说明。

第三方及 vendored 代码保持各自原许可证，详见：

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
- [`third_party/NOTICE`](third_party/NOTICE)
