# 开发者指南（GrokZen / grok-zh）

本文档面向本仓库的维护者与贡献者，记录开发、构建、发布与上游同步的内部约定。
普通用户请阅读根目录 [`README.md`](README.md) 与各平台安装文档。

## 分支模型

- `main`：尽量保持官方上游镜像，只用于同步和审查。
- `zh-dev`：汉化开发、上游合并、构建和测试。
- 计划中的 `zh-stable`：只有在中文验证通过后才建立。

`v1.0.8` 是最后一个旧通道桥接 Tag，后续稳定版使用 `release-vA.B.C`；程序自身仍保持
与上游一致的严格三段 SemVer。

## 上游与发布策略

- 上游 `main` 更新只能触发审查和测试，不能直接进入用户更新源。
- 仓库 Ruleset 必须同时限制 `v*` 与 `release-v*` Tag 的创建、更新和删除权限，只允许
  维护者给已审核分支提交打 Tag；工作流内的 SHA 复核不能替代 GitHub 服务端的 Tag 保护。
- GitHub 发布页正文统一使用中文；每条提交名称链接到对应的 GitHub 提交页面。若提交标题
  不是中文，必须先在 `.github/release-notes/commit-titles.zh-CN.json` 中按完整 SHA 提供
  可审查的中文标题，否则发布会在构建前失败。
- 合并上游的提交必须在同一映射文件中登记已审核的父提交与合并基线；生成器核对 Git
  提交图后，生成独立的"上游更新"区块，列出上游比较范围和实际同步的上游提交链接。
- 正式更新日志、协议兼容检查、本 Fork 的 Windows 测试结果、Immutable Releases 开关和
  精确资产摘要共同构成发布门槛；官方 stable 指针不参与社区更新。

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
> 根 `Cargo.toml`（工作区成员、依赖版本、lint 和 profile）由上游生成，应视为只读。
> 新增社区功能应优先放在独立 crate 或局部适配层中，避免对上游文件进行大范围结构改写。

## 开发

工作区很大，日常检查应优先指定具体 crate：

```sh
cargo check -p <crate>
cargo test -p xai-grok-config
cargo clippy -p <crate>
cargo fmt --all
```

提交翻译时请保留命令、配置键、协议字段、代码块、占位符和 URL，并优先修改集中式
locale 目录；不要在业务代码中逐处硬编码中文。

上游仓库不接受外部拉取请求；开始修改前请先阅读
[`CONTRIBUTING.zh-CN.md`](CONTRIBUTING.zh-CN.md)。

## 文档索引

面向用户的文档（根 README 只保留安装与用法主干，完整清单在此）：

- Windows 自动安装：[`packaging/windows/INSTALL-WINDOWS.md`](packaging/windows/INSTALL-WINDOWS.md)
- macOS ARM64 安装与自动更新：[`packaging/macos/INSTALL-MACOS.md`](packaging/macos/INSTALL-MACOS.md)
- Linux x86_64 GNU 安装与自动更新：[`packaging/linux/INSTALL-LINUX.md`](packaging/linux/INSTALL-LINUX.md)
- 中文用户指南：[`crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md`](crates/codegen/xai-grok-pager/docs/user-guide/zh-CN/README.md)
- 中文入门教程：[`crates/codegen/xai-grok-pager/docs/tutorial/zh-CN/`](crates/codegen/xai-grok-pager/docs/tutorial/zh-CN/)
- 英文上游用户指南：[`crates/codegen/xai-grok-pager/docs/user-guide/README.md`](crates/codegen/xai-grok-pager/docs/user-guide/README.md)
- 贡献说明：[`CONTRIBUTING.zh-CN.md`](CONTRIBUTING.zh-CN.md)
- 安全策略：[`SECURITY.zh-CN.md`](SECURITY.zh-CN.md)
- 各版本简体中文更新说明：`crates/codegen/xai-grok-shell/changelogs/*.zh-CN.md`
- 官方在线文档：[docs.x.ai/build/overview](https://docs.x.ai/build/overview)

中文文档使用稳定文档 ID 和 `zh-CN` 平行目录，不直接改变英文标题所承担的查找身份，
以降低合并上游更新时的冲突。

## 隐私验证证据（v1.0.13）

2026-09 发布的隐私验证三层证据链针对 v1.0.13 给出可复现的验证记录。验证锚定的构建
commit 为 `35b87edcdcc38ca56be0b63f80e08d91984cd611`（发布包二进制内置的构建号）；
2026-09-07 本仓库历史完成匿名化整理后，该提交在新历史中的等值提交为 `db4744b6`
（`zh-dev-pre-rewrite` 顶端）与 `95d2d8b6`（`zh-dev` 主干），源码树完全一致。

- **线路层**：金丝雀仓库 + mitmproxy 全量抓包，免登录与 OAuth 登录态均未观察到代码
  仓库上传、GCS 流量或遥测外传，金丝雀标记 0 命中。
- **源码层**：上游 `data_collection_disabled` 测试族与隐私硬开关回归测试 19/19 通过——
  上传路径代码继承上游但被编译期 `privacy` 特性封死，无法被环境变量、本地配置或
  服务端远程设置重新打开。
- **回归保护**：隐私测试已进入本仓库 CI。

验证方法与工具来自开源项目
[grok-build-privacy-retest](https://github.com/arafatkatze/grok-build-privacy-retest)，
任何人可按相同步骤复现；图表源文件见
[`docs/grok-zh-privacy-evidence-1.0.13.html`](docs/grok-zh-privacy-evidence-1.0.13.html)。
该记录仅覆盖上述版本与受控单轮场景，不构成对服务端侧数据处理行为的证明。

> [!IMPORTANT]
> 线路层（抓包）证据的测试版本是 **v1.0.13**，未在后续版本重跑。`release-v1.0.16`
> 沿用同一编译期 `privacy` 门禁与 CI 回归测试，但本仓库目前没有覆盖 1.0.16 的线路层
> 现场记录；在补充之前，任何涉及运行期网络行为的对外表述都应限定为 v1.0.13。
