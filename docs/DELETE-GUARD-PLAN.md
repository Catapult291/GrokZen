# 删除防护与安全删除通道计划

> 编写时间：2026-11-02（UTC+8）。
>
> 基线分支：`zh-dev`。实现分支：`feat/delete-guard`，在独立 worktree 中完成，验收后合并回 `zh-dev`。
>
> 关联背景（会话内截图）：
> - 图 3：Hermes Agent 的 hardline / deny 机制。原文：“优先吸收 Hermes 的拦截机制；独立删除脚本作为配套的‘唯一删除通道’。”
> - 图 4：独立删除脚本的建议。原文大意：用一个独立脚本来删除文件，由脚本校验 Windows 侧的语法问题，一旦有错误就拒绝执行；不要自己写删除命令，永远调用这个脚本。该建议还提到可以把脚本改成归档移动，本计划不采用这一点。
>
> 使用前提：只考虑 `always-approve`（YOLO）权限模式，不考虑 `ask` / `auto`。

## 0. 目标与边界

### 目标

1. 在 `always-approve` 下，仍然无条件拒绝“删除系统盘 / 系统目录 / 用户主目录根 / 格式化磁盘 / 删除卷影副本”等不可恢复操作。这是唯一不依赖审批的兜底。
2. 提供一个 Windows 删除脚本，作为模型执行删除的唯一通道：只接受字面路径，校验通过后执行真正的删除；校验失败直接拒绝，不执行任何删除。脚本不改成归档移动。
3. 用文档、`AGENTS.md` 指令和配置示例把这套约束固定下来，后续会话可直接复用。

### 非目标

- 不考虑 `ask` / `auto` 模式的审批行为，也不为这些模式增加规则或测试。
- 不依赖 `[permission] ask` 规则拦截普通删除。`always-approve` 下普通删除默认不弹审批；本计划不通过增加 ask 规则来制造审批。
- 不把删除改成“校验 + 归档移动”。不建归档根，不做回收站转存，不做删除后恢复流程。
- 不照搬 Hermes 的 100+ 条 `DANGEROUS_PATTERNS`。Rust `regex` 不支持 lookaround，且大量模式与 Hermes 自身命令耦合。
- 不实现 Windows 内核沙箱。Windows 上 Grok Build 目前没有 OS 级沙箱后端。
- 不改变 `always-approve` 模式本身；本计划以它为固定运行前提。
- 不处理 MCP server 内部的删除行为；MCP 工具仍走现有 `MCPTool` 规则。
- 不顺手格式化无关文件，不重构权限系统。

### 关键决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 防护主路径 | 核心 hardline 检查，放在 YOLO 之前 | 脚本会被模型绕过；只有权限层能在所有 shell 命令执行前拦截 |
| 删除通道 | 独立 `safe-delete.ps1`，校验后直接删除 | 保留删除语义；用单一脚本拦截 Windows 路径、通配符、重解析点等易错写法 |
| Hermes 机制 | 吸收“分层 + normalization + hardline + 用户 deny”设计，不照搬模式表 | 降低维护成本和误报 |
| Windows 覆盖 | 扩展 hardline 到盘根、系统目录、PowerShell/cmd 删除动词 | Hermes 原生 hardline 以 POSIX 路径为主；`always-approve` 下没有审批层可兜底 |
| 变量目标 | 已知根变量（`$HOME`、`%SystemDrive%` 等）按 hardline；其他变量删除不在权限层解析 | 不尝试在权限层解析变量；脚本拒绝非字面路径，残余风险写入文档 |
| 审批层 | 不使用 | 只运行 `always-approve`；普通删除默认无提示 |

## 1. 隔离 worktree 与基线

### 1.1 创建 worktree

```bash
cd C:/Users/bypassnro/Documents/project/grok/GrokZen
git fetch origin
# 若本地 zh-dev 落后于 origin/zh-dev，先确认没有未合并提交，再更新本地分支
git worktree add ../.worktrees/grokzen-delete-guard -b feat/delete-guard zh-dev
cd ../.worktrees/grokzen-delete-guard
```

> 当前主 worktree 在 `fix/pager-test-clipboard-isolation`，且有未提交文件
> `crates/codegen/xai-grok-shell/src/agent/subagent/tests/wake.rs`。不要在主 worktree 里切换分支或合并；
> 所有实现和验收都在新 worktree 完成。

> 本文档如果还没有提交到 `zh-dev`，新 worktree 里不会自动出现。创建后先复制过去：
>
> ```bash
> cp ../../GrokZen/docs/DELETE-GUARD-PLAN.md docs/DELETE-GUARD-PLAN.md
> ```
>
> 或者先把本文档提交到 `zh-dev`，再从 `zh-dev` 创建 worktree。

### 1.2 基线验证

```bash
cargo test -p xai-grok-permission-rules
cargo test -p xai-grok-workspace --lib permission::manager
```

记录基线失败项。若已有失败与本计划无关，写进提交说明，不要在本计划里修复。

### 1.3 完成记录

| 阶段 | 状态 | 提交 | 验收命令 | 备注 |
|---|---|---|---|---|
| 0. worktree / 基线 | 完成 | `docs: 导入删除防护计划并记录基线` | 见 1.2 | worktree `../.worktrees/grokzen-delete-guard`，分支 `feat/delete-guard`（基于 zh-dev `b1e19d09`）；两个基线测试全绿（permission-rules 346 通过，permission::manager 133 通过），无既有失败 |
| 1. hardline 检测模块 | 完成 | `ad043291` | `cargo test -p xai-grok-permission-rules hardline` | 12 个用例通过；匹配走 parser + 引号感知分词，未新增 `regex` 依赖 |
| 2. 权限管理器接入 | 完成 | `7589b621` | `cargo test -p xai-grok-workspace --lib permission::manager` | 137 通过；`reasons::HARDLINE` 与 telemetry `PermissionDecisionReason::Hardline` 同步 |
| 3. safe-delete 脚本 | 完成 | `755d099d` | `pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1` | 39 项断言全过、0 跳过（含 junction 逃逸与允许根为 junction）；拒绝盘根/系统目录/主目录根/通配符/UNC/设备路径；裸相对路径的拒绝在 `c7b26383` 补齐（41 项通过） |
| 4. 文档与配置示例 | 完成 | `c134bd0c` | 人工检查文档与脚本输出一致；`safe-delete.tests.ps1` 41 项通过 | 新增 `docs/SAFE-DELETE.md`，`DEVELOPMENT.md` 加入口链接；阶段 4 核对后按用户要求把 `windows_system_dir_delete` 从“目录本身 + 一级 `/*`”收紧为整棵子树（`fe258faf`），文档同步 |
| 5. 合并回 zh-dev | 完成 | `889e9639` | 见 6.1 / 6.2 | `feat/delete-guard` 的 13 个提交合并进 `zh-dev`，合并树与 feat 分支一致；验收记录见 6.4 |

## 2. 阶段 1：hardline 检测模块（P0）

### 2.1 目标

新增一个纯函数模块：输入一条 shell 命令，输出是否命中“不可恢复删除/系统破坏” hardline，以及原因。该模块不依赖权限管理器、不读写文件、不发网络请求。

### 2.2 改动点

- 新增 `crates/codegen/xai-grok-permission-rules/src/hardline.rs`
- `crates/codegen/xai-grok-permission-rules/src/lib.rs` 增加 `pub mod hardline;`
- `crates/codegen/xai-grok-permission-rules/Cargo.toml` 增加 `regex = { workspace = true }`

### 2.3 API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardlineHit {
    pub code: &'static str,
    pub reason: &'static str,
}

pub fn detect_hardline(command: &str) -> Option<HardlineHit>;
```

`code` 用于遥测和测试（例如 `windows_drive_root`、`posix_root_delete`），`reason` 用于给模型的拒绝消息。不要在消息里回显完整命令，避免把敏感路径写进事件。

### 2.4 检测范围

**POSIX / niubash / Git Bash**

- `rm -rf /`、`rm -rf //`、`rm -rf /./`、`rm -rf /../..`、`rm -rf / *`
- `rm -rf /home`、`/home/*`、`/root`、`/etc`、`/usr`、`/var`、`/boot`、`/bin`、`/sbin`、`/lib`
- `rm -rf ~`、`rm -rf $HOME`、`rm -rf ${HOME}`
- `mkfs`、`mkfs.ext4` 等
- `dd ... of=/dev/sd*|nvme*|hd*|mmcblk*|vd*|xvd*`
- `> /dev/sd*` 等裸设备重定向
- fork bomb（经典 `:(){ :|:& };:`）
- `kill -1`、`kill -9 -1`
- `shutdown`、`reboot`、`halt`、`poweroff`、`init 0/6`、`telinit 0/6`、`systemctl poweroff|reboot|halt|kexec`

**Windows**

- 盘根：`C:\`、`C:/`、`C:`、`/c/`、`//?/C:/`、`\\?\C:\`、`\\.\C:`
- 系统目录：`C:\Windows`、`C:\Program Files`、`C:\Program Files (x86)`、`C:\ProgramData` 及其子目录
- 用户主目录根：`C:\Users\<name>`（精确匹配；不匹配其子目录）
- `Remove-Item` / `ri` / `rm` / `del` / `erase` / `rd` / `rmdir` 指向上述路径，且带 `-Recurse` / `-Force` / `/s` / `/q` / `/f`
- `format C:`、`Format-Volume`、`Clear-Disk`、`diskpart` + `clean` / `clean all`
- `vssadmin delete shadows`、`wbadmin delete`、`bcdedit /set`、`cipher /w`
- `cmd /c rd /s /q C:\`、`cmd /c del /f /s /q C:\Windows`
- `powershell -EncodedCommand` / `pwsh -enc`：不属于 hardline，作为残余风险记录（编码命令不可审计）

**变量目标**

- `$HOME`、`${HOME}`、`$env:USERPROFILE`、`%USERPROFILE%`、`$env:SystemDrive`、`%SystemDrive%` 作为删除目标时，按 hardline。
- 其他未解析变量（`$TARGET`、`%DIR%`）不在权限层解析，也不进入 hardline；删除脚本只接受字面路径，会在运行时拒绝这类目标。模型绕过脚本直接执行变量删除时，属于残余风险（见第 7 节）。

### 2.5 实现要点

1. **不要直接抄 Hermes 的正则。** Rust `regex` 不支持 lookaround；Hermes 大量使用 `(?<!...)`、`(?=...)`。用以下组合替代：
   - `bash_command_splitting::try_parse_shell` + `try_parse_word_only_commands_sequence` 获取命令分段；
   - `unwrap_wrappers` 剥掉 `sudo`、`env`、`exec`、`nohup`、`setsid`、`time`、`nice`、`timeout`、`stdbuf`、`ionice`、`chrt`、`taskset`、`chroot`（复用现有工具，避免另写一套）；
   - 对每个分段检查命令头与参数；
   - 解析失败时，对原始字符串跑保守的 Windows/POSIX 关键词 + 路径正则，命中即 hardline（fail-closed）。
2. **路径归一化**：
   - 生成两个变体：原始字符串；反斜杠转 `/` 后的字符串。
   - 小写化后匹配（Windows 路径大小写不敏感）。
   - 折叠重复 `/`、`/./`、`/../`，识别 `/c/` 与 `C:/`。
   - 不依赖宿主 `std::path::Path` 判断 Windows 盘根；在 Linux 上也要能识别 Windows 命令。
3. **引号与拼接**：
   - 去掉 shell 引号、`''`/`""` 空拼接、反斜杠转义，识别 `r\m`、`r''m`。
   - 折叠 `$IFS` / `${IFS}` 为空格。
   - 对 `bash -c` / `sh -c` / `cmd /c` / `pwsh -Command` 的内层字符串递归检测，最大深度 2。
   - 复合语句（`if …; then …`、`do …; done`、子 shell、`case` 标签）按命令起点检查；
     `$( … )` 与反引号命令替换递归检查内容，单引号内字面文本不检查。阶段 5 手工验证
     发现 `if false; then rm -rf Q:/; fi` 此前会漏判，已补测试与实现。
4. **误报控制**：
   - `echo "rm -rf /"`、`git commit -m "Remove-Item C:\"` 不能命中。
   - `rm -rf ./build`、`Remove-Item .\build -Recurse -Force` 不能命中。
   - `rm -rf /home/user/project` 不命中 hardline（它不是 `/home` 或 `/home/*` 本身）。
   - `git clean -fdx`、`git reset --hard` 不命中 hardline。

### 2.6 测试

在 `hardline.rs` 内写 `#[cfg(test)] mod tests`，至少覆盖：

- POSIX 正例：`rm -rf /`、`rm -rf "/"`、`rm -rf //`、`rm -rf /home`、`rm -rf ~`、`rm -rf $HOME`、`mkfs.ext4 /dev/sda`、`dd if=/dev/zero of=/dev/sda`、fork bomb、`shutdown -h now`。
- Windows 正例：`rm -rf C:\`、`rm -rf C:/`、`rm -rf /c/`、`Remove-Item C:\ -Recurse -Force`、`rd /s /q C:\`、`del /f /s /q C:\Windows`、`cmd /c rd /s /q C:\`、`pwsh -Command "Remove-Item C:\ -Recurse -Force"`、`format C:`、`diskpart` + `clean`、`vssadmin delete shadows /all`。
- 负例：`echo "rm -rf /"`、`git commit -m "Remove-Item C:\"`、`rm -rf ./build`、`Remove-Item .\build -Recurse -Force`、`rm -rf /home/user/project`、`git clean -fdx`、`git reset --hard`。
- 拼接/转义：`r\m -rf /`、`rm -rf "/"`、`rm -rf $IFS/`、`rm -rf ''/''`。
- 变量：`rm -rf $HOME` 命中；`rm -rf $TARGET` 不命中 hardline。

### 2.7 验收

```bash
cargo test -p xai-grok-permission-rules hardline
cargo clippy -p xai-grok-permission-rules --all-targets -- -D warnings
```

**提交**：`feat(permission): 新增删除 hardline 检测`

## 3. 阶段 2：权限管理器接入（P0）

### 3.1 目标

在权限管理器里，把 hardline 检查放在 `policy deny` 之后、YOLO / grant / classifier 之前。命中后返回 `Decision::PolicyDeny`，不弹审批、不进入 YOLO、不消耗分类器预算。

### 3.2 改动点

- `crates/codegen/xai-grok-workspace/src/permission/manager/mod.rs`
- `crates/codegen/xai-grok-permission-rules/src/reasons.rs`
- 可能涉及 `xai-grok-workspace/src/permission/telemetry/permission.rs` 的 reason 校验（若 `reasons::ALL` 被测试覆盖）

### 3.3 做法

1. 在 `reasons.rs` 增加：

   ```rust
   pub const HARDLINE: &str = "hardline";
   ```

   并加入 `reasons::ALL`。

2. 在 `manager/mod.rs` 的 `Request` 分支里，找到 `policy_decision` 的拒绝处理之后、`if yolo_mode && ...` 之前，插入：

   ```rust
   let hardline_command = match &access {
       AccessKind::Bash(cmd) => Some(cmd.as_str()),
       AccessKind::DetachBackground { command } => Some(command.as_str()),
       _ => None,
   };
   if let Some(command) = hardline_command
       && let Some(hit) = xai_grok_permission_rules::hardline::detect_hardline(command)
   {
       let decision = Decision::PolicyDeny(format!(
           "hardline: {} — this command is not allowed through the agent; run it outside the agent if needed",
           hit.reason
       ));
       let event = emit_event(&decision, false, false, None, Some(reasons::HARDLINE));
       let _ = respond_to.send(PermissionResolution {
           decision,
           event: Some(event),
       });
       continue;
   }
   ```

   实际代码按现有 `emit_event` 借用/可变借用结构调整；上面的形状是语义参照。

3. 覆盖范围：
   - `AccessKind::Bash` 覆盖 `run_terminal_command`、`monitor` 等映射到 Bash 的工具；
   - `AccessKind::DetachBackground` 本身在 YOLO 下仍会强制提示，但 hardline 应该更早拒绝，不进入审批；
   - `Read` / `Edit` / `MCPTool` / `Tool` 没有可检查的 shell 命令字符串，不进入 hardline；它们的删除行为属于残余风险。

4. 本阶段不引入审批。`always-approve` 下普通删除默认直接放行；hardline 是这一前提下唯一的核心拦截层。`[permission] ask` 规则虽然对 Bash 命令仍会强制提示，但本计划不靠它拦截删除，也不要求用户配置。

### 3.4 测试

在 `manager/mod.rs` 末尾的测试模块新增：

- `yolo_bash_hardline_denies_without_prompt`
  - `mgr.set_yolo_mode(true)`；
  - `AccessKind::Bash("rm -rf /c/")`、`"Remove-Item C:\\ -Recurse -Force"`、`"cmd /c rd /s /q C:\\"` 分别断言 `Decision::PolicyDeny(_)`；
  - 断言 `prompts.borrow().len() == 0`。
- `yolo_bash_non_hardline_still_allows`
  - `rm -rf ./build`、`Remove-Item .\\build -Recurse -Force` 在 YOLO 下仍 `Allow`、不弹审批，证明没有误杀普通清理，也锁住“普通删除无需审批”的现状。
- `hardline_denies_detach_background`
  - `AccessKind::DetachBackground` + `rm -rf /c/`，断言 `PolicyDeny`（detach 本身也会强制提示，但 hardline 应该先拒绝）。
- `hardline_denies_despite_exact_grant`
  - 在 `PermissionState.allowed_bash_commands` 里放入完全相同的命令，断言 hardline 仍然拒绝（hardline 在 grant 之前）。

可复用现有 `test_manager` / `decide` / `tool_call` 辅助函数，参考 `ask_bash_disallow_rejects_despite_blanket_grant`。

### 3.5 验收

```bash
cargo test -p xai-grok-workspace --lib permission::manager
cargo clippy -p xai-grok-workspace --all-targets -- -D warnings
```

**提交**：`feat(permission): hardline 在 YOLO 之前拒绝删除系统盘`

## 4. 阶段 3：safe-delete 脚本（P1）

### 4.1 目标

提供一个 Windows PowerShell 7 脚本，作为模型执行删除的唯一通道：只接受字面路径，规范化后校验在允许根目录内，拒绝盘根、UNC、`..`、通配符、重解析点；校验通过后直接删除目标，并写 JSON 日志。脚本执行真正的删除，不建归档目录，不把删除改成移动。

### 4.2 改动点

- 新增 `scripts/windows/safe-delete.ps1`
- 新增 `scripts/windows/tests/safe-delete.tests.ps1`
- 新增 `scripts/windows/README.md`

### 4.3 脚本行为

参数：

```powershell
pwsh -NoProfile -NonInteractive -File scripts/windows/safe-delete.ps1 `
  -Path "C:\Users\me\Projects\old-build" `
  -Reason "clean build output"
```

规则：

1. 要求 PowerShell 7+。
2. `-Path` 必须是字面路径；拒绝包含 `*`、`?`、`[` 的字符串。
3. 拒绝空字符串、仅空白、相对路径、盘符相对路径（`C:foo`）、根相对路径（`\foo`）、UNC（`\\server\share`）、设备命名空间（`\\?\`、`\\.\`）。
4. 用 `[System.IO.Path]::GetFullPath($Path)` 做词法规范化；拒绝解析后仍包含 `..` 的路径。
5. 拒绝盘根（`C:\`）、系统目录（`C:\Windows`、`C:\Program Files`、`C:\Program Files (x86)`、`C:\ProgramData`）、用户主目录根（`C:\Users\<name>`）。即使权限层已经 hardline，脚本仍要独立拒绝。
6. 拒绝路径组件以空格或点结尾的写法，避免 Windows 静默改写后指向另一个位置。
7. 允许根来自 `-AllowedRoot` 或环境变量 `GROK_SAFE_DELETE_ROOTS`（分号分隔）；默认只允许当前工作区（`$PWD`），不要默认放开整个用户目录。`C:\Users\<user>\Projects` 只作为配置示例。
8. 校验目标位于某个允许根之内，且目标不等于允许根本身（比较时大小写不敏感，并按路径分隔符判断边界）。
9. 拒绝目标及其到允许根之间的中间组件上的重解析点（junction/symlink）。允许根本身可以是 junction/symlink，避免工作区本身是链接时误拒。
10. 目标不存在：退出 0，写日志 `not_found`（幂等）。
11. 校验失败：退出 2，写日志 `rejected`，stderr 输出简短原因；不执行任何删除。
12. 删除：用 `Remove-Item -LiteralPath $resolved -Recurse -Force`。删除失败：退出 3，写日志 `failed`；不做“复制后删除”或归档移动之类的回退。
13. 成功：退出 0，写日志 `deleted`，stdout 输出被删除的路径。

日志字段：`timestamp`、`action`、`user`、`original`、`resolved`、`allowed_root`、`reason`、`error`。一行一个 JSON 对象。

### 4.4 测试脚本

不引入 Pester 依赖，用普通 PowerShell 断言：

- 在临时允许根下创建目录和文件。
- 调用脚本删除目录，断言原路径消失、日志包含 `deleted`。
- 调用脚本删除单文件，断言原路径消失、日志包含 `deleted`。
- 断言脚本不会创建任何归档目录（脚本没有归档行为）。
- 传 `C:\`、`C:\Windows`、`..\..\Windows`、含 `*` 的路径、UNC、`\\?\C:\...`，断言退出码 2，目标未被删除。
- 传允许根之外的路径，断言退出码 2。
- 传允许根本身，断言退出码 2。
- 传不存在路径，断言退出码 0 且日志 `not_found`。
- 可选：创建 junction 指向允许根之外，断言退出码 2（需要权限；失败则跳过并记录）。

### 4.5 验收

```bash
pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1
```

**提交**：`feat(scripts): 新增 Windows safe-delete 删除脚本`

## 5. 阶段 4：文档与配置示例（P1）

### 5.1 目标

把“为什么需要 hardline、为什么普通删除没有审批、怎么用 safe-delete、有哪些残余风险”写清楚，后续会话不用重新推导。

### 5.2 改动点

- 新增 `docs/SAFE-DELETE.md`
- 可选：在 `docs/DEVELOPMENT.md` 的 Windows 小节加一行链接
- 可选：在仓库根 `AGENTS.md` 增加删除指令片段

### 5.3 文档内容

1. 背景：Windows 上没有 Grok Build OS 沙箱；`always-approve` 会直接放行普通 Bash，包括 `rm`、`del`、`Remove-Item` 等删除命令，默认不弹审批。
2. 两层防护：
   - hardline：权限层无条件拒绝盘根/系统目录/格式化/卷影删除；
   - `safe-delete.ps1`：模型执行删除的唯一通道，先校验路径与 Windows 写法，再执行真正的删除。
3. 明确不使用的机制：
   - 不依赖 `[permission] ask` 规则拦截删除；本计划不要求用户为普通删除增加审批。
   - `[permission] deny` 仍然先于 `always-approve` 生效，可以作为项目自己的固定禁令，但它只能覆盖固定模式，不能替代 hardline 和脚本。
4. 配置示例（`~/.grok/config.toml` 或项目 `.grok/config.toml`）：

   ```toml
   [ui]
   permission_mode = "always-approve"

   # deny 在 always-approve 之前生效；ask 规则只对 Bash 命令强制提示。
   # hardline 已覆盖盘根和系统目录，这里只放项目自己的固定禁令。
   [permission]
   deny = [
     "Bash(rm -rf ./data/*)",
   ]
   ```

5. `AGENTS.md` 指令片段：

   ```text
   删除任何文件或目录前，必须调用 scripts/windows/safe-delete.ps1。
   不要直接使用 rm、del、rd、Remove-Item 或任何等价命令。
   脚本校验失败时，向用户报告并停止；不要改写命令绕过脚本。
   脚本执行的是真删除，没有归档或回收站恢复。
   ```

6. 可选：`PreToolUse` 钩子示例。钩子读 `.toolInput.command`，做小写化/反斜杠归一化，命中 hardline 或删除动词时返回 `deny`。必须写明：钩子是 fail-open，脚本崩溃/超时会放行，所以不能替代核心 hardline。

7. 残余风险：
   - 模型仍可能绕过脚本，直接执行 `rm`、`del`、`Remove-Item`；`AGENTS.md` 是指令约束，不是硬拦截；
   - 变量目标不在权限层解析；脚本拒绝非字面路径，但绕过脚本的变量删除仍可能发生；
   - 脚本执行永久删除。路径写错但在允许根内时，没有归档可恢复；
   - `python -c`、`node -e`、自定义二进制、`cmd /c` 多层嵌套等仍可能绕过模式匹配；
   - 归档/回收站方案的恢复能力不在本计划内；真正的高保证需要 VM、Windows Sandbox 或独立用户。

### 5.4 验收

人工检查文档中的命令可复制执行；配置示例与 `xai-grok-workspace` 的规则语法一致。

**提交**：`docs: 删除防护与 safe-delete 使用说明`

## 6. 阶段 5：验证与合并回 zh-dev

### 6.1 完整验证

在 worktree 中：

```bash
cargo fmt --check
cargo test -p xai-grok-permission-rules
cargo test -p xai-grok-workspace --lib permission
cargo clippy -p xai-grok-permission-rules -p xai-grok-workspace --all-targets -- -D warnings
pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1
```

如果全量 clippy 因既有告警失败，记录基线，只对本计划涉及的两个 crate 做 `-D warnings` 检查。

### 6.2 手工验证

1. 用 `grok-zh --permission-mode always-approve` 启动一个临时会话，要求执行 `rm -rf ./tmp-probe`（工作区内、非 hardline），确认直接执行、不弹审批。
2. 在同一会话要求执行 `rm -rf /c/`、`Remove-Item C:\ -Recurse -Force`、`cmd /c rd /s /q C:\`，确认都被 `PolicyDeny` 拒绝，且没有弹审批。
3. 用 `safe-delete.ps1` 删除一个临时目录，确认目录消失、日志有 `deleted`、没有生成归档目录。
4. 用 `safe-delete.ps1` 指向 `C:\`、`C:\Windows`、允许根外路径，确认退出码 2，目标未被删除。

> 手工验证只使用临时目录，不要对真实项目目录执行删除测试。

### 6.3 合并

先确保 `feat/delete-guard` 与最新 `zh-dev` 对齐：

```bash
git fetch origin
git rebase origin/zh-dev   # 若本地 zh-dev 已更新，用 zh-dev
```

重新跑 6.1。然后用一个干净的 `zh-dev` worktree 合并，避免碰主 worktree 的未提交文件：

```bash
cd C:/Users/bypassnro/Documents/project/grok/GrokZen
git worktree add ../.worktrees/grokzen-zh-dev zh-dev
cd ../.worktrees/grokzen-zh-dev
git merge --no-ff feat/delete-guard
```

验收后：

```bash
git push origin zh-dev
```

清理：

```bash
cd C:/Users/bypassnro/Documents/project/grok/GrokZen
git worktree remove ../.worktrees/grokzen-zh-dev
git worktree remove ../.worktrees/grokzen-delete-guard
git branch -d feat/delete-guard
```

> 如果 `git push` 需要代理，按现有 GitHub 规则先确认 `HTTPS_PROXY`，再执行。

### 6.4 阶段 5 验收记录

合并提交 `889e9639`（`feat/delete-guard` → `zh-dev`，`--no-ff`），合并树与
`feat/delete-guard` 的树完全一致。

| 检查 | 结果 |
|---|---|
| `cargo test -p xai-grok-permission-rules` | 359 通过 / 0 失败 |
| `cargo test -p xai-grok-workspace --lib permission::manager` | 137 通过 / 0 失败 |
| `cargo test -p xai-grok-workspace --lib permission` | 22 个既有失败（`hub_gate` / `sandbox_wire` / `grants`），在基线 `b1e19d09` 上同样失败 |
| `cargo clippy -p xai-grok-permission-rules -p xai-grok-workspace --lib --tests --no-deps` | 本计划涉及文件 0 告警；`shell_access.rs` 的 5 处 `unnecessary_cast` 为既有基线；`--all-targets -- -D warnings` 会在 `xai-grok-config` / `xai-fast-worktree` 等既有告警处失败，基线同样失败 |
| `cargo fmt -p xai-grok-permission-rules -p xai-grok-workspace --check` | 通过；仓库根 `cargo fmt --check` 在本机报 `os error 206`（文件名或扩展名太长），基线工作区同样复现 |
| `pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1` | 41 通过 / 0 失败 / 0 跳过 |

手工验证（新构建的 `grok-zh`，`--permission-mode bypassPermissions`）：

- `if false; then rm -rf Q:/; fi`：阶段 5 首次探测时未被识别，命令进入 shell 并
  以退出码 0 空转。修复提交 `6a317fc8` 后同一条命令在 spawn 前返回
  `PolicyDeny`，原文为 `hardline: deleting a drive root — this command is not
  allowed through the agent; run it outside the agent if needed`。
- `rm -rf ./tmp-probe`：直接执行、不弹审批，目录被删除，确认普通清理未被误杀。
- `safe-delete.ps1`：删除目录与文件均写 `deleted`；`C:\`、`C:\Windows`、
  `..\..\Windows`、含 `*` 路径、UNC、`\\?\C:\...`、允许根外路径、允许根本身
  均退出 2 且目标未动；不存在的路径退出 0 并写 `not_found`；未创建归档目录。

> 推送状态：`origin/zh-dev` 停在 `4f68b950`，本地 `zh-dev` 在其之上还有 53 个
> 与删除防护无关的既有提交，`git push origin zh-dev` 会一并发布。

## 7. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| hardline 误杀普通清理 | 工作流受阻 | 负例测试覆盖 `./build`、`/home/user/project`、`git clean`；只匹配盘根/系统目录/主目录根 |
| Rust regex 无 lookaround | 模式写不出来 | 用 parser + 分段 + 简单正则；不照搬 Hermes 模式 |
| Windows 路径变体漏判 | 仍可能删盘 | 覆盖 `C:\`、`C:/`、`/c/`、`//?/C:/`、`\\?\C:\`；解析失败 fail-closed |
| 变量目标绕过 | 仍可能删盘 | 已知根变量 hardline；脚本拒绝非字面路径；残余风险写入文档 |
| 模型绕过脚本直接删除 | 普通删除仍可执行 | `AGENTS.md` 指令 + 可选钩子；hardline 兜住灾难性目标；剩余风险明确记录 |
| 脚本永久删除且无归档 | 路径写错无法恢复 | 这是保留删除语义的代价；允许根限制缩小误删范围，重要数据由用户自己的备份覆盖 |
| 合并冲突 | 阻塞 | 独立 worktree + rebase；只改权限相关文件 |

回滚：每个阶段单独提交；如 hardline 误杀严重，`git revert` 阶段 2 的提交即可恢复 YOLO 行为，阶段 1 的纯函数可保留。脚本阶段独立，`git revert` 脚本提交即可移除删除通道。

## 8. 参考

- Hermes Agent Security 文档：<https://hermes-agent.nousresearch.com/docs/user-guide/security>
- `tools/approval.py`、`tools/approval_detection.py`、`tools/approval_floors.py`
- Hermes hardline PR #15878：<https://github.com/NousResearch/hermes-agent/pull/15878>
- Hermes Windows 审批测试：<https://github.com/NousResearch/hermes-agent/blob/main/tests/tools/test_approval_windows.py>

本计划只吸收 Hermes 的分层与检测思路；具体模式以本仓库的 Rust 实现和测试为准。
