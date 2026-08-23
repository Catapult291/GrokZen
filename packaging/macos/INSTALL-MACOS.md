# macOS ARM64 预览版

此软件包是 Grok Build 简体中文社区版的 Apple Silicon 预览构建，仅支持
`aarch64-apple-darwin`（M1 及后续 Apple Silicon）。它不是 SpaceXAI 官方发行版。

## 安全边界

- 预览包由 GitHub Actions 的 `macos-15` Apple Silicon runner 构建，并在 CI 中执行
  `grok-zh --version` 冒烟测试。
- 软件包提供外层 `.sha256` 和包内 `SHA256SUMS.txt`，但尚未使用 Apple Developer ID
  签名，也没有经过 Apple 公证。
- 未签名预览包可能被 macOS Gatekeeper 阻止。请勿关闭系统安全功能；正式使用前请等待
  本仓库完成签名、公证与真实设备验收。
- 默认继续与官方 `grok` 共用 `~/.grok` 数据目录，不会复制聊天记录、登录状态或配置。

## 校验与试运行

把 `.tar.gz` 和同名 `.sha256` 放在同一目录，然后运行：

```sh
shasum -a 256 -c grok-zh-*-macos-aarch64.tar.gz.sha256
mkdir grok-zh-macos-preview
tar -xzf grok-zh-*-macos-aarch64.tar.gz -C grok-zh-macos-preview
cd grok-zh-macos-preview
shasum -a 256 -c SHA256SUMS.txt
./grok-zh --version
```

当前包只用于预览测试，不会安装全局命令，也不会覆盖官方 `grok`。正式 Release 与
macOS 社区自动更新将由本仓库的统一发布流程提供，不能用 Actions Artifact 代替正式
Release 验收。
