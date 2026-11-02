# 删除防护与安全删除通道

[返回项目首页](../README.md)

Windows 上没有 Grok Build 的 OS 级沙箱，`always-approve`（YOLO）会直接放行普通
Bash 命令，包括 `rm`、`del`、`rd`、`Remove-Item` 等删除命令，默认不弹审批。
本仓库用两层互相独立的防护缩小这一暴露面：权限层的 hardline 检查拦灾难性目标，
`scripts/windows/safe-delete.ps1` 作为模型执行删除的唯一通道。

完整设计与后续阶段见[删除防护计划](DELETE-GUARD-PLAN.md)。

## 两层防护

| 层 | 位置 | 作用 | 覆盖范围 |
| --- | --- | --- | --- |
| hardline | 权限管理器 | 命中即 `PolicyDeny`，不弹审批、不进入 YOLO、不消耗分类器 | `AccessKind::Bash` 与 `AccessKind::DetachBackground` 的命令 |
| safe-delete.ps1 | `scripts/windows/` | 校验一个绝对字面路径后执行真正的删除 | 模型按指令主动调用的删除 |

两层互不依赖：脚本被绕过时 hardline 仍拦盘根与系统目录；hardline 漏判普通清理时，
脚本负责路径写法与允许根的校验。

### 第一层：hardline

`xai-grok-permission-rules/src/hardline.rs` 提供纯函数
`detect_hardline(command) -> Option<HardlineHit>`。检测是词法分析，不读写文件系统，
因此在 Linux 上也能识别 Windows 写法。权限管理器在命中的 `deny` 规则之后、
YOLO 与 grant 之前调用它；即使用户此前授予了完全相同的命令，hardline 仍然拒绝。
拒绝消息只带原因，不回显完整命令。

检测会剥掉 `sudo`、`doas`、`env`、`exec`、`nohup`、`setsid`、`time`、`nice`、
`timeout`、`chroot` 等包装，递归检查 `bash -c`、`sh -c`、`cmd /c`、`pwsh -Command`
的内层命令，最多两层。

| `code` | 拦截目标 |
| --- | --- |
| `posix_root_delete` | `rm -rf /`、`//`、`/./`、`/../..`、`/*` |
| `posix_system_dir_delete` | `/home`、`/root`、`/etc`、`/usr`、`/var`、`/boot`、`/bin`、`/sbin`、`/lib` 及其 `/*` |
| `home_dir_delete` | `~`、`$HOME`、`${HOME}` |
| `windows_drive_root` | `C:\`、`C:/`、`/c/`、`\\?\C:\`、`\\.\C:` |
| `windows_system_dir_delete` | `C:\Windows`、`C:\Program Files`、`C:\Program Files (x86)`、`C:\ProgramData` 及其全部子目录 |
| `windows_user_home_delete` | `C:\Users`、`C:\Users\<name>`（不含其子目录） |
| `mkfs` | `mkfs`、`mkfs.ext4` 等 |
| `raw_device_write` | `dd of=/dev/sd*`、`nvme*`、`hd*`、`mmcblk*`、`vd*`、`xvd*`，以及 `> /dev/sd*` 等裸设备重定向 |
| `fork_bomb` | 经典 `:(){ :|:& };:` |
| `kill_all` | `kill -1`、`kill -9 -1` |
| `system_power` | `shutdown`、`reboot`、`halt`、`poweroff`、`init 0/6`、`telinit 0/6`、`systemctl poweroff/reboot/halt/kexec` |
| `disk_management` | `format C:`、`Format-Volume`、`Clear-Disk`、`diskpart` + `clean` / `clean all` |
| `shadow_copy_delete` | `vssadmin delete shadows`、`wbadmin delete` |
| `boot_config_write` | `bcdedit /set` |
| `cipher_wipe` | `cipher /w` |

系统目录按整棵子树匹配，`C:\Windows\System32`、`C:\Program Files\<app>`、
`C:\ProgramData\<vendor>` 都会命中；`C:\Windows\Temp` 这类清理目标也在拒绝范围内，
需要在代理外处理。

明确不命中的情况：`rm -rf ./build`、`Remove-Item .\build -Recurse -Force`、
`rm -rf /home/user/project`、`C:\Users\me\Documents`、`C:\WindowsOld`、
`C:\ProgramData2`、`git clean -fdx`、`git reset --hard`，以及引号里的文本
（如 `echo "rm -rf /"`）。

hardline 不解析任意变量：`$HOME`、`$env:USERPROFILE`、`%USERPROFILE%`、
`$env:SystemDrive`、`%SystemDrive%` 按已知根处理，`$TARGET` / `%DIR%` 不进入检测。

### 第二层：safe-delete.ps1

```powershell
pwsh -NoProfile -NonInteractive -File scripts/windows/safe-delete.ps1 `
  -Path "C:\Users\me\Projects\old-build" `
  -Reason "clean build output"
```

脚本一次只删除一个绝对字面路径，拒绝通配符、相对路径、盘符相对路径、根相对路径、
UNC、设备命名空间、结尾带空格或点的路径组件，以及规范化后仍含 `..` 的路径。
盘根、系统目录和用户主目录根由脚本独立再拒绝一次。

目标必须严格位于允许根之内，且不能等于允许根本身；允许根按
`-AllowedRoot`、`GROK_SAFE_DELETE_ROOTS`（分号分隔）、当前工作目录的顺序取值。
默认只放开当前工作区，不放开整个用户目录。目标及其到允许根之间的现有组件
不得是 junction / symlink；允许根本身可以是链接。

脚本执行真正的删除，不建归档目录、不进回收站。目标不存在时退出 0 并记
`not_found`；校验失败退出 2 且不触碰文件系统；删除失败退出 3。

```powershell
# 测试
pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1
```

参数、校验规则、退出码与日志字段见[脚本说明](../scripts/windows/README.md)。
日志默认写入 `%LOCALAPPDATA%\Grok\safe-delete.log`，一行一个 JSON 对象。

## 明确不使用的机制

- **不靠 `[permission] ask` 规则拦截删除。** `always-approve` 下普通删除默认直接
  放行；本方案不为普通删除制造审批。
- **不把删除改成归档移动。** 脚本保留删除语义，没有回收站或归档恢复路径。
- **`[permission] deny` 不是替代品。** deny 在 always-approve 之前生效，适合放
  项目自己的固定禁令；它按命令文本的模式匹配，覆盖不了包装、拼接与变量写法，
  也不能替代 hardline 和脚本。
- **不假设 `PreToolUse` 钩子可靠。** 钩子可以补充项目级拦截，但它故障开放：
  脚本崩溃、超时或输出格式错误都会放行，因此不替代核心 hardline。

## 配置示例

写入 `~/.grok/config.toml` 或项目 `.grok/config.toml`：

```toml
[ui]
permission_mode = "always-approve"

# deny 在 always-approve 之前生效；hardline 已覆盖盘根和系统目录，
# 这里只放项目自己的固定禁令。
[permission]
deny = [
  "Bash(rm -rf ./data/*)",
]
```

规则优先级为 deny > ask > allow；deny 命中后返回 `PolicyDeny`，不会进入
always-approve 的直通路径。紧凑规则中的 `Bash(...)` 按命令文本做 glob 匹配。

## 给模型的指令片段

把以下片段加入项目的 `AGENTS.md`（或用户级规则文件）：

```text
删除任何文件或目录前，必须调用 scripts/windows/safe-delete.ps1。
不要直接使用 rm、del、rd、Remove-Item 或任何等价命令。
脚本校验失败时，向用户报告并停止；不要改写命令绕过脚本。
脚本执行的是真删除，没有归档或回收站恢复。
```

这是指令约束，不是硬拦截。模型仍可能忽略它直接执行删除命令；hardline 只兜住
灾难性目标，其余依赖这层指令和用户监督。

## 残余风险

- **绕过脚本的直接删除。** `rm`、`del`、`Remove-Item` 在 always-approve 下仍可
  执行，`AGENTS.md` 只能降低概率。
- **变量目标。** 权限层只认识已知根变量；脚本拒绝相对路径和非字面写法，但绕过
  脚本的变量删除仍可能发生。
- **永久删除。** 路径写错但在允许根内时没有归档可恢复；重要数据由用户自己的
  备份覆盖。
- **解释器与多层嵌套。** `python -c`、`node -e`、自定义二进制，以及超过两层
  `cmd /c` / `pwsh -Command` 的嵌套不在 hardline 的词法覆盖内。
- **编码命令。** `powershell -EncodedCommand` / `pwsh -enc` 的内容不可审计，
  hardline 不解析，属于已知残余风险。
- **非 shell 工具。** MCP server、文件编辑工具和 `Read` / `Edit` 类访问没有可
  检查的命令字符串，不进入 hardline。
- **高保证场景。** 需要更强隔离时应使用 VM、Windows Sandbox 或独立用户，
  权限规则和脚本都不提供内核级隔离。

## 验证

```bash
cargo test -p xai-grok-permission-rules hardline
cargo test -p xai-grok-workspace --lib permission::manager
pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1
```

手工检查时只使用临时目录：在 `always-approve` 会话里执行 `rm -rf ./tmp-probe`
应直接执行且不弹审批；执行 `rm -rf /c/`、`Remove-Item C:\ -Recurse -Force`、
`cmd /c rd /s /q C:\` 应得到 `PolicyDeny`。
