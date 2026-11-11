# Windows 删除脚本

`safe-delete.ps1` 是模型在 Windows 上执行删除的唯一通道：只接受一个字面路径，
校验 Windows 侧的路径写法与允许根，校验通过后执行真正的删除。校验失败直接拒绝，
不触碰文件系统。脚本保留删除语义，不做归档移动、不进回收站。

权限层的 hardline 检查是灾难性目标的第一道拦截；本脚本是第二道，两道互相独立。
整体设计、边界与残余风险见 [`docs/SAFE-DELETE.md`](../../docs/SAFE-DELETE.md)。

## 用法

```powershell
pwsh -NoProfile -NonInteractive -File scripts/windows/safe-delete.ps1 `
  -Path "C:\Users\me\Projects\old-build" `
  -Reason "clean build output"
```

一次调用只删除一个路径；多个目标逐次调用。成功时 stdout 输出规范化后的被删路径。

| 参数 | 必填 | 说明 |
|---|---|---|
| `-Path` | 是 | 要删除的字面路径。不接受通配符，不接受相对路径。 |
| `-Reason` | 否 | 调用方给出的删除原因，写入日志的 `reason` 字段。 |
| `-AllowedRoot` | 否 | 允许删除的根目录，可传多个；多根时用逗号（PowerShell 数组）或分号分隔。 |
| `-LogPath` | 否 | 日志文件路径。不传时依次看 `GROK_SAFE_DELETE_LOG` 与 `%LOCALAPPDATA%\Grok\safe-delete.log`。 |

## 允许根

删除目标必须位于某个允许根之内，且不能等于允许根本身。优先级：

1. `-AllowedRoot`；
2. 环境变量 `GROK_SAFE_DELETE_ROOTS`（分号分隔）；
3. 当前工作目录（`$PWD`）。

默认只放开工作区，不放开整个用户目录。`C:\Users\<user>\Projects` 之类的路径
只应在需要时通过 `-AllowedRoot` 或环境变量显式配置。

## 校验规则

以下任何一条不满足即以退出码 2 拒绝，且不执行删除：

- PowerShell 7+（`pwsh`）。
- `-Path` 非空、非纯空白，且不含 `*`、`?`、`[`。
- 不接受相对路径、盘符相对路径（`C:foo`）、根相对路径（`\foo`）、
  UNC（`\\server\share`）、设备命名空间（`\\?\`、`\\.\`）。
- 路径经 `[System.IO.Path]::GetFullPath` 词法规范化后，不得残留 `..`。
- 拒绝盘根（`C:\`）、系统目录（`C:\Windows`、`C:\Program Files`、
  `C:\Program Files (x86)`、`C:\ProgramData` 及其子目录）、
  用户主目录根（`C:\Users\<name>`）、`C:\Users` 本身。
- 拒绝以空格或点结尾的路径组件（Windows 会静默改写这类名字）。
- 目标必须严格位于允许根之内，不等于是允许根本身。
- 目标及其到允许根之间的每一级现有组件都不得是重解析点（junction / symlink）。
  允许根本身可以是 junction / symlink，工作区经常通过它访问。

目标不存在时退出 0，日志记 `not_found`，不做任何删除。

## 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 删除成功，或目标本来就不存在 |
| 2 | 校验失败，未执行删除 |
| 3 | 校验通过但删除失败（权限、占用等） |

## 日志

一行一个 JSON 对象，字段固定：

```json
{"timestamp":"2026-11-02T21:56:14.2985590+08:00","action":"deleted","user":"me","original":"C:\\tmp\\build","resolved":"C:\\tmp\\build","allowed_root":"C:\\tmp","reason":"clean build output","error":""}
```

`action` 取 `deleted`、`not_found`、`rejected`、`failed` 之一；
`error` 在拒绝和失败时给出原因。

## 测试

```powershell
pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1
```

测试不依赖 Pester，在临时目录里创建、删除对象，结束时清理；全部通过时退出码为 0。
