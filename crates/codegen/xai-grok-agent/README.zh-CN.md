# `xai-grok-agent`

Agent 构建器、定义解析和系统提示组装。

此 crate 从 `xai-grok-shell` 中提取出一等的 `Agent` 类型。一个 `Agent` 将工具、系统提示、system-reminder 策略、压缩策略和模型配置打包成单个可移植对象，任何宿主都可以使用——无论宿主是 `xai-grok-shell`、另一个进程内宿主，还是无头批处理运行器。

## 快速开始

### 从定义文件开始

Agent 定义是带 YAML frontmatter 的 **Markdown 文件**，存放在 `.grok/agents/`（项目级）或 `~/.grok/agents/`（用户级）。

```rust
use xai_grok_agent::{AgentDefinition, AgentBuilder};
use xai_grok_tools::notification::ToolNotificationHandle;

// 1. Parse the definition file
let def = AgentDefinition::from_file(".grok/agents/code-reviewer.md")?;

// 2. Build the agent
let agent = AgentBuilder::new(cwd, None, ToolNotificationHandle::noop())
    .from_definition(def)
    .build()
    .await?;

// 3. Use it
println!("Agent: {}", agent.name());
println!("Prompt: {}", agent.system_prompt());
let tool_defs = agent.tool_definitions().await;
```

### 以编程方式创建（无文件）

```rust
let agent = AgentBuilder::new(cwd, None, ToolNotificationHandle::noop())
    .with_name("my-agent")
    .with_description("A custom agent")
    .with_tools(vec!["read_file".into(), "grep".into()])
    .build()
    .await?;
```

### 发现所有定义

```rust
use xai_grok_agent::discovery;

// Find all .md files in .grok/agents/ directories
let definitions = discovery::discover(&cwd);

// Find a specific agent by name (checks built-ins, then user dirs)
let reviewer = discovery::by_name("code-reviewer");

// Find with project-level priority
let agent = discovery::by_name_in_cwd("my-agent", &cwd);
```

## Agent 定义文件格式

Agent 定义是带 YAML frontmatter 的 Markdown 文件：

```markdown
---
name: my-agent
description: What this agent does
# ... additional config fields
---

System prompt body goes here...
```

`---` 分隔符之间的 **frontmatter** 是 YAML 配置。结束 `---` 之后的 **body** 是系统提示内容。

### 最小示例（扩展基础模板）

```markdown
---
name: code-reviewer
description: Reviews code for quality and security
tools:
  - read_file
  - grep
  - list_dir
permissionMode: plan
---

You are a senior code reviewer. Analyze code and provide
actionable feedback organized by severity.
```

使用 `promptMode: extend`（默认值）时，body 会追加到基础模板之后；该模板包含工具调用约定、格式规则和用户信息。作者只需编写与 persona 相关的内容。

### 完整提示覆盖

```markdown
---
name: custom-agent
description: Agent with full control over the system prompt
promptMode: full
tools:
  - read_file
  - search_replace
  - run_terminal_cmd
---

You are a custom agent.

Use ${{ tools.read_file }} to read files.
Use ${{ tools.search_replace }} to edit files.

${%- if tools.run_terminal_cmd %}
Use ${{ tools.run_terminal_cmd }} for shell commands.
${%- endif %}

<user_info>
OS: ${{ os_name }}
Shell: ${{ shell_path }}
Working Directory: ${{ working_directory }}
Date: ${{ current_date }}
</user_info>
```

使用 `promptMode: full` 时，body 就是完整的系统提示，并通过 MiniJinja 渲染，使用自定义的 `${{ }}`/`${% %}` 分隔符（避免与正文中的字面量 `{{ }}` 冲突）。

### 带完成要求（编排模式）

```markdown
---
name: orchestrator-worker
description: Worker agent that must signal completion before ending a turn
completionRequirement:
  tool: complete_task
  reminder: >
    You stopped without calling `complete_task`.
    Please continue and call it when done.
  recovery:
    maxRetries: 5
    baseDelayMs: 5000
    maxDelayMs: 60000
toolConfig:
  wait_for_instruction:
    retry:
      maxRetries: 1440
      baseDelayMs: 5000
      maxDelayMs: 30000
---

You are a worker agent in an orchestrated multi-agent workflow.
You MUST call `complete_task` before ending your response.
```

## Frontmatter schema 参考

所有 frontmatter 键均使用 **camelCase**。

| 字段 | 类型 | 必需 | 默认值 | 说明 |
|---|---|---|---|---|
| `name` | `string` | **是** | — | 唯一的 agent ID（小写、连字符） |
| `description` | `string` | **是** | — | 何时/为何使用此 agent |
| `promptMode` | `string` | 否 | `"extend"` | `"extend"` 或 `"full"` |
| `tools` | `string[]` | 否 | 继承全部 | 工具允许列表。省略 = 全部工具；`[]` = 无工具 |
| `disallowedTools` | `string[]` | 否 | `[]` | 拒绝列表（优先于 `tools`） |
| `permissionMode` | `string` | 否 | `"default"` | `"default"`、`"acceptEdits"`、`"dontAsk"`、`"plan"` |
| `skills` | `string[]` | 否 | `[]` | 预加载的 skill 名称 |
| `agentsMd` | `bool` | 否 | `true` | 发现并注入 AGENTS.md 文件 |
| `outputFormat` | `string` | 否 | `"default"` | `"default"` 或 `"concise"` |
| `bash` | `object` | 否 | defaults | Bash 工具配置覆盖项 |
| `bash.timeoutSecs` | `float` | 否 | `120.0` | Bash 命令超时 |
| `bash.outputByteLimit` | `int` | 否 | `200000` | 最大输出字节数 |
| `bash.cmdPrefix` | `string` | 否 | `null` | 命令前缀 |
| `toolNameOverrides` | `map<string,string>` | 否 | `{}` | canonical → model-facing 名称映射 |
| `paramNameOverrides` | `map<string,map>` | 否 | `{}` | 每个工具的参数名称映射 |
| `completionRequirement` | `object` | 否 | `null` | 在本轮结束前必须调用的工具 |
| `completionRequirement.tool` | `string` | 是* | — | canonical 工具名 |
| `completionRequirement.reminder` | `string` | 是* | — | 未调用时的提醒文本 |
| `completionRequirement.recovery` | `object` | 否 | `null` | harness 的恢复策略 |
| `toolConfig` | `map<string,object>` | 否 | `{}` | 每个工具的执行配置 |
| `toolConfig.*.retry` | `object` | 否 | `null` | 工具重试配置 |

* 仅在设置了 `completionRequirement` 时必需。

## 提示组装

```
promptMode: extend                     promptMode: full
──────────────────                     ─────────────────
1. Base template (MiniJinja)           1. Markdown body (MiniJinja, ${{ }}/${% %})
   (tool conventions, formatting,      2. AGENTS.md section (if agentsMd: true)
    user_info, background tasks)       3. Skills section
2. Markdown body (appended raw)
3. AGENTS.md section (if agentsMd: true)
4. Skills section
```

### 模板变量（full 模式）

| 变量 | 说明 |
|---|---|
| `${{ tools.read_file }}` | `read_file` 的解析名称（若禁用则为空） |
| `${{ tools.search_replace }}` | `search_replace` 的解析名称 |
| `${{ tools.run_terminal_cmd }}` | `run_terminal_cmd` 的解析名称 |
| `${{ tools.grep }}` | `grep` 的解析名称 |
| `${{ tools.list_dir }}` | `list_dir` 的解析名称 |
| `${{ tools.todo_write }}` | `todo_write` 的解析名称 |
| `${{ tools.skill }}` | `skill` 的解析名称 |
| `${{ tools.get_task_output }}` | `get_task_output` 的解析名称 |
| `${{ tools.kill_task }}` | `kill_task` 的解析名称 |
| `${{ tools.web_search }}` | `web_search` 的解析名称 |
| `${{ os_name }}` | 操作系统（例如 `"macos"`、`"linux"`） |
| `${{ shell_path }}` | Shell 路径（例如 `"/bin/zsh"`） |
| `${{ working_directory }}` | 工作区路径 |
| `${{ current_date }}` | 用户本地时区中的当前日期（`YYYY-MM-DD`） |

条件语句：`${%- if tools.todo_write %}...${%- endif %}`——工具禁用时会省略整个区块。

## 发现规则

Agent 定义从多个位置发现，并按以下优先级处理：

1. **项目级（最高优先级）：**`.grok/agents/*.md`——从 `cwd` 向上遍历到 git 仓库根目录。距离 `cwd` 更近的文件优先。
2. **用户级：**`~/.grok/agents/*.md`
3. **兼容路径（最低优先级）：**用户主目录下额外的 vendor agent 目录（启用时）
4. **内置：**`default_grok_build()`、`browser_use()`

按名称去重，确保最高优先级的定义胜出。例如，项目的 `.grok/agents/code-reviewer.md` 会遮蔽同名的用户级定义。

## Crate 关系

```
┌──────────────────┐
│  xai-grok-agent  │  ← This crate
│  (Agent, Builder, │
│   Definition)     │
└────────┬─────────┘
         │ depends on
         ▼
┌──────────────────┐
│  xai-grok-tools  │
│  (ToolBridge,    │
│   ToolRegistry,  │
│   ToolState)     │
└────────▲─────────┘
         │ depends on
┌────────┴─────────┐
│  xai-grok-shell  │  uses AgentBuilder to create
│  (session host)  │  Agent during session setup
└──────────────────┘
```

- **`xai-grok-tools`：**提供 `ToolBridge`、`ToolRegistry`、`ToolState`、`SystemReminderLayer` 和工具实现。`xai-grok-agent` 依赖它进行工具设置。
- **`xai-grok-shell`：**应用 shell。在创建会话时使用 `AgentBuilder` 构造 `Agent`。shell 从 `xai-grok-agent` 重新导出部分模块（AGENTS.md 发现、skill 发现和基础提示渲染）。

## 内置 Agent

| 名称 | 提示模式 | 说明 |
|---|---|---|
| `grok-build` | extend | 用于软件工程任务的默认 agent |
| `browser-use` | full | 网页浏览和交互 agent |

## 错误处理

`AgentBuilder::build()` 返回 `Result<Agent, AgentBuildError>`：

| 错误 | 发生时机 |
|---|---|
| `ParseError` | YAML 错误、缺少 `---`、类型错误 |
| `MissingField` | 缺少必需字段（`name`/`description`） |
| `UnknownToolOverride` | `toolNameOverrides` 引用了不存在的工具 |
| `IoError` | 发现 AGENTS.md/skills 时读取文件错误 |
| `MiniJinjaError` | 模板渲染失败 |

未知的 frontmatter 字段会被**静默忽略**，以实现向前兼容——为较新版本编写的定义可以在较旧版本上工作。

## 开发

```bash
# Check
cargo check -p xai-grok-agent

# Test
cargo test -p xai-grok-agent

# Clippy
cargo clippy -p xai-grok-agent --fix --allow-dirty

# Format
cargo fmt --all
```
