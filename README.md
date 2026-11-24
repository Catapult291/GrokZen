<div align="center">

# GrokZen（grok-zh）

**Grok Build 简体中文社区版。**

<p>
  <img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache 2.0">
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg" alt="Platform">
  <img src="https://img.shields.io/badge/locale-zh--CN%20%7C%20en--US-orange.svg" alt="Locale">
</p>
</div>

## 简介

基于 xAI 官方 Grok Build 构建的社区版：

- **全中文**：CLI / TUI / 设置 / 文档全简体中文，可 `--locale en-US` 切英文
- **去遥测**：上报路径源码层禁用，编译期排除，无法远程重开
- **无 x.ai 登录**：不强制跳转 x.ai，模型与 API Key 自行配置

## 安装

不想下载 ZIP 时，在 Windows PowerShell 5.1 或 PowerShell 7 中粘贴下面一行：

```powershell
$p=Join-Path $env:TEMP ('grok-zh-install-'+[guid]::NewGuid().ToString('N')+'.ps1'); $tls=[Net.ServicePointManager]::SecurityProtocol; try { [Net.ServicePointManager]::SecurityProtocol=$tls -bor [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/Catapult291/GrokZen/zh-dev/packaging/windows/Install-GrokZhOnline.ps1' -OutFile $p; & "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File $p; if ($LASTEXITCODE -ne 0) { throw "安装未完成，退出码：$LASTEXITCODE" } } finally { [Net.ServicePointManager]::SecurityProtocol=$tls; Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue }
```

也可以从 [Releases](https://github.com/Catapult291/GrokZen/releases) 下载对应平台的完整包，解压后运行包内 `Install-GrokZh.ps1` 完成安装；安装说明见 [Windows](packaging/windows/INSTALL-WINDOWS.md) · [macOS](packaging/macos/INSTALL-MACOS.md) · [Linux](packaging/linux/INSTALL-LINUX.md)。

## 独立功能

- **Astrolabe 语义索引引擎**：进程内代码索引，`search_code` / `find_symbol` / `resolve_context` 等 7 个检索工具替代内置 grep
- **codemode 元工具**：QuickJS 沙盒编排 MCP 工具调用，中间结果不进主上下文；MCP 工具按 direct / deferred / codemode_only / hidden 四档暴露
- **安全删除机制**：Windows 删除统一走 safe-delete 校验通道，拦截盘根、系统目录、格式化等不可恢复操作
- **子代理强化**：内置 explore / plan / implementer 类型路由，支持并行 fan-out、worktree 隔离收尾与结构化输出
- **循环护栏**：文本复读熔断、跨轮循环拦截与同轮工具调用去重，杜绝空转烧 token
- **缓存与旁路请求**：长会话提示词缓存守卫；标题刷新、记忆整理等旁路请求走轻量模型，不占主回合
- **niubash 默认 Shell**：原生 Windows Bash 零改写透传路径与开关，超长命令自动暂存脚本、后台任务隐藏窗口补通知
- **跨会话任务面板**：detach 任务活过启动它的会话，迟到完成投给下一个打开的会话；面板并排列出各会话任务、可直接终止
- **会话操作增强**：`/fork` 支持从较早轮次分叉，`/undo` 独立回退对话

## 上游与许可

本项目是社区衍生构建，遵循 Apache-2.0：

- **官方上游**：[xai-org/grok-build](https://github.com/xai-org/grok-build)（SpaceXAI）
- **中文汉化**：[JoyElliot/grok-build-Chinese](https://github.com/JoyElliot/grok-build-Chinese)
- **去遥测补丁**：[thedavidweng/gork-build](https://github.com/thedavidweng/gork-build)（Gork Build）

保留上述上游的版权与归属声明，详见 [`NOTICE`](NOTICE) 与 [`LICENSE`](LICENSE)。
本项目与官方版共享 `~/.grok` 数据目录，仅为兼容，与 SpaceXAI 无关联。

## 致谢

感谢 **Linux Do 社区**（[linux.do](https://linux.do)）

感谢 **Astrolabe**（[wojiushirencai/Astrolabe](https://github.com/wojiushirencai/Astrolabe)）

## 许可证

Apache License 2.0，见 [`LICENSE`](LICENSE)。第三方组件许可见
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)。
