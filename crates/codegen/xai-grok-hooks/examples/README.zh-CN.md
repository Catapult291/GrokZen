# Hook 示例

Grok 的示例 hook。复制到 `~/.grok/hooks/` 可全局启用，或复制到 `<project>/.grok/hooks/` 以使用项目范围的 hook（需要 `/hooks-trust`）。

## 可用示例

### 1. 安全 Shell 守卫（`safe-shell.json`）

**类型：**阻断（`PreToolUse`）

在执行前拒绝明显具有破坏性的 shell 命令：
- `rm -rf /`、`sudo rm -rf`、`mkfs`、对设备执行 `dd`、fork bomb

**安装：**
```sh
mkdir -p ~/.grok/hooks/bin
cp examples/hooks/safe-shell.json ~/.grok/hooks/
cp examples/hooks/bin/safe-shell-guard.sh ~/.grok/hooks/bin/
chmod +x ~/.grok/hooks/bin/safe-shell-guard.sh
```

### 2. 禁止递归 Grep（`no-recursive-grep.json`）

**类型：**阻断（`PreToolUse`）

在 shell 执行前拒绝递归的 `grep` 调用：
- `grep -r`、`grep -R`、`grep --recursive`、`grep --dereference-recursive`、
  `grep -d recurse`、聚合选项（`grep -rn`、`grep -nri`）以及 `rgrep`

递归 grep 会把整个目录树读入内存，在大型仓库中可能导致 agent 进程因 OOM 被终止。系统 prompt 已经引导模型避开它，但 prompt 只是建议——这个 hook 将其变成硬性、确定性的阻断。请改用专用搜索工具（基于 ripgrep）。

它会小心避免误报：`ls -R | grep foo`（`-R` 属于 `ls`）、`grep -e -r file`（`-r` 是模式）和 `grep -- -r file` 都允许执行。

**安装：**
```sh
mkdir -p ~/.grok/hooks/bin
cp examples/hooks/no-recursive-grep.json ~/.grok/hooks/
cp examples/hooks/bin/no-recursive-grep-guard.py ~/.grok/hooks/bin/
chmod +x ~/.grok/hooks/bin/no-recursive-grep-guard.py
```
（需要 `PATH` 中存在 `python3`。）

### 3. 会话审计日志（`session-log.json`）

**类型：**被动（`SessionStart` + `SessionEnd`）

将会话元数据追加到 `~/.grok/session-audit.log`——事件、会话 ID、cwd、时间戳。

**安装：**
```sh
mkdir -p ~/.grok/hooks/bin
cp examples/hooks/session-log.json ~/.grok/hooks/
cp examples/hooks/bin/session-log.sh ~/.grok/hooks/bin/
chmod +x ~/.grok/hooks/bin/session-log.sh
```

### 4. 工具活动记录器（`tool-logger.json`）

**类型：**被动（`PreToolUse` + `PostToolUse`）

将所有工具调用记录到 `~/.grok/tool-activity.log`——工具名、事件类型、有效工具名和后台运行状态。

**安装：**
```sh
mkdir -p ~/.grok/hooks/bin
cp examples/hooks/tool-logger.json ~/.grok/hooks/
cp examples/hooks/bin/tool-logger.sh ~/.grok/hooks/bin/
chmod +x ~/.grok/hooks/bin/tool-logger.sh
```

### 5. Stop 门禁：完成前验证（`stop-verify.json`）

**类型：**阻断（`Stop`）

保持 agent 工作，直到 `cargo build` 通过。agent 即将结束这一轮时会运行 `Stop` hook；返回 `{"decision":"block","reason":"…"}` 会将原因反馈给模型并再运行一轮。内置上限会在连续 8 次后结束这一轮。该 hook 设置 300 秒超时，因为超时的 Stop hook 会 fail open 并允许 agent 停止。

**安装：**
```sh
mkdir -p ~/.grok/hooks/bin
cp examples/hooks/stop-verify.json ~/.grok/hooks/
cp examples/hooks/bin/stop-verify.sh ~/.grok/hooks/bin/
chmod +x ~/.grok/hooks/bin/stop-verify.sh
```

## 格式

Hook 文件使用兼容 Claude 的 JSON 格式：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/check.sh", "timeout": 5 }
        ]
      }
    ]
  }
}
```

- **事件名：**`SessionStart`、`PreToolUse`、`PostToolUse`、`Stop`、`SubagentStop`、`SessionEnd`（完整列表见[用户指南](../../xai-grok-pager/docs/user-guide/10-hooks.md)）
- **Matcher：**工具名上的 regex。Claude 名称如 `Bash`、`Read`、`Edit` 会自动扩展，同时匹配 Grok 名称（`run_terminal_cmd`、`read_file`、`search_replace`）
- **Timeout：**以秒为单位（默认：5）
- **Command：**脚本路径（相对于 hook 文件目录）或内联 shell 命令

## 脚本契约

脚本会在 **stdin** 上接收 JSON 形式的 hook 事件封装，并应将响应写到 **stdout**：

**工具门禁（`PreToolUse`）：**
```json
{"decision":"allow"}
```
或
```json
{"decision":"deny","reason":"Explanation for the user"}
```

**停止门禁（`Stop` / `SubagentStop`）：**保持 agent 工作或强制其停止：
```json
{"decision":"block","reason":"Feedback fed back to the model"}
```
```json
{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"Non-error feedback"}}
```
```json
{"continue":false,"stopReason":"Shown to the user; overrides any block"}
```
连续 8 次 continuation 后本轮结束。输入包含 `stopHookActive`（本轮已有 block 继续时为 true），因此 hook 可以主动放弃。

**退出码：**`0` = 允许 / 无决定，`2` = 拒绝（`PreToolUse`）或阻断停止，并将 stderr 作为反馈，其他值 = fail-open。stdout 上有效的决定 JSON 优先于退出码。

**被动 hook：**stdout 仅用于信息展示。成功时退出 `0`。

## 卸载

从 `~/.grok/hooks/` 删除 JSON 文件。该 hook 会在下一次会话停止运行。
