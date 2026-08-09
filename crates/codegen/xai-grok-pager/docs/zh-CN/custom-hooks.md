# 自定义钩子指南

钩子让你可以在 Grok 会话的关键时刻运行自定义脚本或 HTTP 请求——例如工具运行前后、会话开始或结束时，或者智能体发送通知时。

它们非常适合自动化、安全检查、日志记录、通知，以及与自己的工具集成。

## 为什么使用钩子？

常见用例：

- **安全防护**：在执行前拦截 `rm -rf /` 等危险命令。
- **审计日志**：将每次工具使用或会话记录到文件或外部服务。
- **通知**：长时间运行的任务完成时发送 Slack/Discord 消息。
- **自动格式化**：编辑后自动运行 `cargo fmt` 或 `prettier`。
- **环境设置**：在会话开始时导出机密或设置变量。
- **自定义工作流**：在特定事件上触发构建、测试或部署。

## 快速开始

1. 创建钩子目录：

   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. 创建一个简单的钩子文件，例如 `~/.grok/hooks/session-start.json`：

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
            { "type": "command", "command": "echo \"🚀 Grok session started in $(pwd)\"" }
           ]
         }
       ]
     }
   }
   ```

3. 启动（或重启）Grok 会话。钩子会在 `SessionStart` 上自动运行。

   试试看：在非 VS Code 系列中按 `Ctrl+L`（或者在任何地方运行 `/hooks`——在 VS Code / Cursor / Windsurf / Zed 中推荐此方式），然后查看“钩子”选项卡确认它已加载。

## 钩子位置

钩子会从多个位置发现（并合并）：

| 范围 | 路径 | 受信任？ | 备注 |
|---|---|---|---|
| 全局 | `~/.grok/hooks/*.json` | 始终 | 适合个人钩子 |
| 全局 | `~/.claude/settings.json` | 始终 | Claude Code 兼容性 |
| 项目 | `<project>/.grok/hooks/*.json` | 需要信任 | 按仓库自动化 |
| 项目 | `<project>/.claude/settings.json` | 需要信任 | Claude 兼容性 |
| 配置 | `config.toml`、`managed_config.toml`、`requirements.toml` | 始终 | 在你的（或组织的）配置中随附的钩子 |
| 插件 | 已安装插件内部捆绑 | 按插件 | 团队共享钩子 |

配置文件钩子使用 TOML 形式的同一模式；详情参阅[钩子用户指南](../user-guide/zh-CN/10-hooks.md#hooks-in-config-files)。

**信任项目**：第一次打开包含钩子的项目时，打开钩子模态界面（在非 VS Code 系列中按 `Ctrl+L`，或在包括 VS Code 系列在内的任意终端运行 `/hooks`），或者运行 `/hooks-trust`（与 `--trust` 相同的文件夹信任门禁，记录在 `~/.grok/trusted_folders.toml`）。这样可防止不受信任的仓库运行任意代码。

## 钩子 JSON 格式

每个 `.json` 文件可以定义多个钩子：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safety-check.sh", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "bin/log-activity.sh" }
        ]
      }
    ]
  }
}
```

关键字段：

- **事件名称**（顶层键）：`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Stop`、`Notification`、`SessionEnd` 等。
- **matcher**（可选）：针对事件的匹配值测试的正则表达式——工具事件中是工具名称，其他事件中是各事件对应的值（参见用户指南的“钩子”章节）。为空 = 匹配所有内容。
- **type**：`"command"`（运行脚本或 shell 单行命令）或 `"http"`（将事件 POST 到 URL）。
- **command**：可执行文件路径（相对于 JSON 文件）或内联 shell 命令。
- **timeout**：终止钩子前的秒数（默认值：`5`；`Stop`/`SubagentStop` 门禁默认值：`600`）。钩子超时会故障开放（fail-open）。

**工具名称别名**：`Bash`、`Edit`、`Read` 等 Claude 风格名称会自动匹配 Grok 的内部名称（`run_terminal_cmd`、`search_replace`、`read_file`）。

## 编写钩子脚本

### 输入

完整事件会以 JSON 形式通过 **stdin** 发送。下面是 `PreToolUse` 钩子的示例：

```json
{
  "hookEventName": "pre_tool_use",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "toolName": "run_terminal_cmd",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

### 输出（用于 `PreToolUse` 等阻塞钩子）

将 JSON 写入 **stdout**：

- 允许：`{"decision": "allow"}`
- 拒绝：`{"decision": "deny", "reason": "Unsafe command detected"}`

**退出代码**（行为因钩子类型而异）：

- `0`——成功/允许（用于阻塞钩子）
- `2`——显式拒绝（`PreToolUse`），或阻止停止并通过 stderr 提供反馈（`Stop`/`SubagentStop`；参见用户指南中的“停止决策控制”）
- 任何其他代码（包括超时/崩溃/缺少环境变量）——**故障开放（fail-open）**：失败会记录并显示在钩子回滚区中，但不会阻止工具调用。若要阻止工具调用，请在 stdout 上返回 JSON `{"decision":"deny","reason":"..."}`。

### 被动钩子

对于 `SessionStart` 或 `PostToolUse` 等事件，stdout 会被忽略。成功时只需退出 0。

### 有用的环境变量

Grok 会向每个钩子进程注入以下变量：

- `GROK_HOOK_EVENT`——事件名称（例如 `pre_tool_use`、`session_start`、`post_tool_use`）
- `GROK_HOOK_NAME`——此钩子的完整配置名称
- `GROK_SESSION_ID`——当前会话标识符
- `GROK_WORKSPACE_ROOT`——工作区根目录的绝对路径

对于插件提供的钩子，还会设置以下变量：

- `GROK_PLUGIN_ROOT`——插件安装目录的绝对路径
- `GROK_PLUGIN_DATA`——插件可写数据目录的绝对路径

这些由运行器和插件注入的变量始终优先。尝试通过 `env` 字段覆盖保留的运行器键时，会在加载时剥离这些值（并记录警告）。对于插件钩子，`GROK_PLUGIN_ROOT` 和 `GROK_PLUGIN_DATA` 也会覆盖用户为这些键提供的值。

### 自定义环境变量（`env` 字段）

每个处理器可以声明要注入子进程的额外环境变量：

```json
{
  "type": "command",
  "command": "bin/check.sh",
  "env": {
    "MY_API_TOKEN": "secret-here",
    "LOG_LEVEL": "debug"
  }
}
```

值必须是**字符串**——JSON 数字和布尔值目前无法解析（如有需要请将它们放在引号中）。

对于插件钩子，插件适配器还会注入 `GROK_PLUGIN_ROOT` 和 `GROK_PLUGIN_DATA`。这些键会覆盖用户为相同名称声明的值（插件契约不可协商）。

### 变量替换

`command` 和 `url` 字符串支持在配置加载时替换 `$VAR` 和 `${VAR}`：

```json
{
  "type": "command",
  "command": "${HOME}/.config/grok-hooks/check.sh"
}
```

每个引用的查找顺序：

1. 处理器自己的 `env` 映射。
2. 当前进程环境（Grok 自身可见的环境）。

如果两处都未设置某个引用，则按字面保留（例如 `${UNSET}` 仍是字面字符串）。运行时 `sh -c` 分支可能会在变量后来被设置时解析它；否则运行器会拒绝生成进程，并给出清晰的 “required env var(s) not set” 错误。

对于 HTTP 钩子，`url` 还会在请求时（SSRF 验证前立即）再次展开，因此插件注入的变量（如 `${GROK_PLUGIN_ROOT}/check`）会根据插件实际路径解析。

#### 参数展开修饰符

POSIX 参数展开形式——`${VAR:-default}`、`${VAR-default}`、`${VAR:=x}`、`${VAR:?msg}`、`${VAR:+x}`、`${VAR%pat}`、`${VAR#pat}`、`${VAR/pat/repl}`、`${VAR:N:M}`——在加载时**永不**展开，而是原样留给运行时 `sh -c` 分支处理。这样可以避免加载时展开器与 POSIX shell 语义之间的细微差异（尤其是 `:-` 的空字符串行为）。

如果钩子命令包含 shell 元字符（空格、管道、`&&`、重定向、`$` 等），运行器会通过 `sh -c` 路由，你会获得完整的 shell 展开语义。如果命令是不含元字符的裸路径，运行器会直接生成进程——但路径中的 `$VAR` / `${VAR}` 引用仍会在加载时解析，因此像 `${HOME}/bin/check.sh` 这样的直接执行路径无需包装在 `sh -c` 中即可工作。

#### 不会展开的内容

- **`matcher`** 是正则表达式（`$` 是行尾锚点）。它永远不会进行环境变量展开——替换 `$VAR` 会静默改变正则语义，并很可能产生无效模式。若需要动态 matcher，请在写入时生成 JSON 文件。
- **`timeout`** 是数字，无内容可展开。
- **`env` 映射本身的值**——这些值会按字面存储并原样传给子进程，因此 `"BAR": "${HOME}/x"` 会向子进程环境注入字面字符串 `${HOME}/x`。

## 在 TUI 中管理钩子

在非 VS Code 系列中按 `Ctrl+L`（或在任何地方运行 `/hooks`）打开“钩子与插件”模态界面。

在**钩子**选项卡中，你可以：

- `l`——重新加载所有钩子
- `a`——按路径添加自定义钩子（非常适合测试）
- `e`——启用/禁用
- `r`——移除
- `Space`——展开分组

来自 `~/.grok/hooks/` 的钩子会显示在**全局**下，项目钩子显示在**项目**下，其他来源同理。

## HTTP 钩子

不使用本地脚本，而是调用远程端点：

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

完整事件封装会以 JSON 形式 POST。适用于 Webhook、分析或无服务器函数。

## 最佳实践

1. **保持钩子快速**——长时间运行的钩子会阻塞 UI（尽可能使用后台 `&` 或异步方式）。
2. **使用明确的 `deny` 来阻止**——任何错误（超时、崩溃、缺少环境变量等）都会故障开放，因此崩溃的钩子不会阻止工具调用。要强制执行策略，钩子必须运行完成，并在 stdout 上输出 `{"decision":"deny","reason":"..."}`。
3. **使用绝对路径或相对于钩子文件的路径**——JSON 旁的 `bin/` 脚本具有可移植性。
4. **使用 `Ctrl+L`（非 VS Code 系列）/`/hooks` 进行测试**——在依赖钩子之前验证加载和匹配。
5. **将项目钩子纳入版本控制**——提交 `.grok/hooks/`（但绝不要提交机密）。

## 安全说明

- 全局钩子（`~/.grok/...`）以你的用户权限运行——请像对待 shell 脚本一样对待它们。
- 项目钩子需要显式信任（运行 `/hooks-trust` 或使用模态界面），以防止恶意仓库发起供应链攻击。
- HTTP 钩子会发送会话数据——只使用可信端点。

## 故障排除

- **钩子没有运行？** → 在非 VS Code 系列按 `Ctrl+L`（或在任何地方运行 `/hooks`），查看它是否已加载并匹配。
- **项目钩子被忽略？** → 先信任项目。
- **找不到脚本？** → 检查路径是否相对于 `.json` 文件，以及脚本是否可执行（`chmod +x`）。
- **看到错误？** → 检查 pager 日志（通常位于 tracing 窗格或 `~/.grok/logs`）。

## 更多示例

查看 `xai-grok-hooks` crate 中的内置示例：

- [安全 Shell 防护](../../../xai-grok-hooks/examples/hooks/safe-shell.json)
- [禁止递归 Grep](../../../xai-grok-hooks/examples/hooks/no-recursive-grep.json)——硬阻止 `grep -r`/`grep -R`/`rgrep`（OOM 防护）
- [会话审计日志](../../../xai-grok-hooks/examples/hooks/session-log.json)
- [工具活动记录器](../../../xai-grok-hooks/examples/hooks/tool-logger.json)

将它们复制到 `~/.grok/hooks/` 并按需自定义。

## 完整参考

有关完整事件列表、匹配器语义、信任模型和高级细节，请参阅[钩子用户指南](../user-guide/zh-CN/10-hooks.md)。

---

*祝你玩得开心！* 如果你构建了很酷的东西，可以考虑将其作为插件分享。
