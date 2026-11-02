<a id="subagents-and-personas"></a>
# 子智能体与 Persona

子智能体是独立的子会话，可以并行处理任务。每个子智能体都有自己的上下文窗口，因此主智能体可以委派研究、实现、测试和代码审查，而不消耗自己的上下文。子智能体完成后，会向父级报告摘要。

默认启用子智能体。

---

<a id="agents-vs-personas"></a>
## 智能体与 Persona 的区别

智能体和 Persona 都可以定制行为，但作用层级不同：

| | **智能体（Agents）** | **Persona** |
|---|---|---|
| **配置内容** | 整个会话：模型、工具、提示模式、系统提示 | 添加到子智能体提示中的行为覆盖层 |
| **作用范围** | 主会话或子智能体 | 仅子智能体 |
| **设置方式** | 启动时设置，或通过智能体定义（`.md` 文件，位于 `.grok/agents/` 或 `~/.grok/agents/`）设置 | 在 `config.toml`（`[subagents.personas]`）中设置，或在 `.grok/personas/` 下的 `.toml` 文件中设置；在解析子智能体时应用 |
| **控制内容** | 模型、工具可用性、提示正文、技能 | 语气、输出格式、任务重点以及输入/输出契约 |
| **编辑者** | 你——在智能体模态框中，或通过编辑文件来创建、删除或切换 | 你——在配置或文件中定义自定义 Persona；内置 Persona 只读 |
| **示例** | `grok-build`、`explore`、`plan` | `researcher`、`concise` |

智能体定义会话本身；Persona 决定子智能体在会话中的行为。子智能体始终以某种智能体类型（例如 `general-purpose`）运行，解析时还可以在其上叠加 Persona。

在智能体模态框中管理二者。使用 `/config-agents`（别名 `/agents`）打开模态框，或直接用 `/personas` 打开 Personas 标签页。模态框有两个标签页：**Agents** 和 **Personas**。

---

<a id="disabling-subagents"></a>
## 禁用子智能体

可通过环境变量或配置文件禁用子智能体：

```bash
export GROK_SUBAGENTS=0              # 环境变量
```

```toml
# ~/.grok/config.toml
[subagents]
enabled = false
```

---

<a id="how-subagents-work"></a>
## 子智能体的工作方式

当主智能体确定要委派工作时，会调用 `spawn_subagent` 工具启动子会话。子会话具有：

- 独立于父级的上下文窗口
- 由其智能体类型和可选能力模式决定的工具集
- 在解析过程中应用的可选 Persona 指令

子智能体完成后，父级会收到子智能体的输出（通常是摘要）。

---

<a id="built-in-agent-types"></a>
## 内置智能体类型

内置类型可以直接按名称选择。省略 `subagent_type` 时仍然使用 `general-purpose`。某个类型被 `[subagents.toggle]` 关闭、被父级 `Agent(...)` 允许列表排除，或存在同名自定义智能体时，它不会出现在枚举里。

| 类型 | 选择场景 | 工具集 | 默认 effort / max_turns |
| --- | --- | --- | --- |
| `general-purpose` | 研究与实现混合、跨多步的任务；不确定选哪个时的默认值。 | 全部工具（读写、执行、搜索、网页、计划）。 | 继承会话 / 40 |
| `explore` | 只做调查：找文件、读代码、追调用链，不修改任何文件。 | 只读：读文件、列目录、`search_code`、`resolve_context`、`find_symbol`、`find_references`、`goto_definition`、`get_diagnostics`；无 shell。 | `low` / 12 |
| `plan` | 需要实现设计：探索代码并产出分步方案。 | 只读工具加 `todo_write`；无 shell、无编辑。 | `medium` / 20 |
| `implementer` | 要实现并自测：改代码、跑测试、构建或静态检查。 | 完整的读写执行工具集（不含 workflow）。 | 继承会话 / 40 |

选择建议：只调查选 `explore`；只出方案选 `plan`；实现并验证选 `implementer`；研究加实现或任务边界不清选 `general-purpose`。自定义类型按名称选择，例如 `my-plugin:reviewer`。

工具 schema 按上述顺序先列内置类型，再列项目/用户/插件定义的智能体。父智能体的 `tools` 列表通过 `Agent(...)` 限制可启动类型时，枚举中只列出允许的类型。传入未知类型会导致调用失败，并返回有效类型列表。

### `thoroughness`

`thoroughness` 只对 `explore` 生效：`quick`（只看明显匹配）、`medium`（顺带检查附近调用点与约定）、`very_thorough`（追完所有相关路径并报告边界情况）。Grok Build 会把它写成子任务提示的第一行 `Thoroughness: <level>.`。其他类型收到该参数会记录警告并忽略。

---

<a id="personas"></a>
## Persona

Persona 是一种命名的行为覆盖层。它的指令会作为 `<system-reminder>` 注入子智能体对话，从而塑造语气、输出格式和任务重点，但不会改变子智能体的智能体类型、模型或工具。

可在 `config.toml` 或 `.toml` 文件中定义 Persona：

```toml
[subagents.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build 会按优先级顺序从以下位置发现基于文件的 Persona：

- `.grok/personas/*.toml`（项目）
- `~/.grok/personas/*.toml`（用户）
- 内置 Persona 目录（最低优先级）

每个文件定义一个 Persona，文件名（不含扩展名）成为 Persona 名称。内联 `config.toml` Persona 的优先级高于文件。只发现 `.toml` 文件。

在智能体模态框的 Personas 标签页（`/personas`）中管理 Persona。内置 Persona 为只读；你定义的 Persona 可编辑。

> **注意：** Grok Build 通过子智能体解析和角色应用 Persona，而不是通过 `spawn_subagent` 参数应用。主智能体生成子级时不会传递 Persona 名称。

<a id="persona-fields"></a>
### Persona 字段

| 字段               | 说明                                                          |
| ------------------- | ------------------------------------------------------------ |
| `instructions`      | 以内联指令文本作为 Persona 层应用。               |
| `instructions_file` | 指令文件的路径；在生成时加载，并在 `instructions` 之后合并。 |
| `description`       | Persona 目录中显示的简短摘要。若未提供，则回退为 `instructions` 的第一段。 |
| `inputs` / `outputs`| 声明的输入和输出契约（见下文）。                     |
| `model`             | 使用 Persona 时应用的模型覆盖。                    |
| `reasoning_effort`  | 使用 Persona 时应用的推理力度。                  |
| `default_isolation` | 默认隔离模式（`none` 或 `worktree`）。                      |

<a id="input-output-contracts"></a>
### 输入/输出契约

Persona 可以声明它所需的输入和它产生的输出。父智能体读取这些声明来了解应提供哪些上下文以及应期待哪些工件。这样就可以串联 Persona：一个 Persona 的输出文件成为下一个 Persona 的输入：

```toml
[[subagents.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagents.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

每个字段都有 `name`、`io_type`（默认值为 `file`）、`required` 标志和 `description`。

<a id="persona-resolution"></a>
### Persona 解析

应用 Persona 时，Grok Build 按以下顺序解析生效的模型和推理力度，优先级从高到低：

1. 生成时的显式覆盖
2. 角色默认值
3. Persona 默认值
4. 父会话

隔离遵循前三步的相同顺序，但默认值为 `none`（无工作树），而不是继承父会话。

如果请求了 Persona 但无法解析——找不到、没有指令，或其 `instructions_file` 无法读取——生成会失败。

---

<a id="spawning-subagents"></a>
## 生成子智能体

主智能体调用 `spawn_subagent` 工具。参数如下：

| 参数 | 说明 |
| --- | --- |
| `prompt` | 给子智能体的完整任务提示。设置 `output_schema` 时，要求它以单个符合 schema 的 JSON 值结束。 |
| `description` | 任务的简短描述。 |
| `label` | 3–5 个词的显示标签，写入 `list_agents` 输出与子智能体界面；省略时用 id。 |
| `subagent_type` | `general-purpose`（默认）、`explore`、`plan`、`implementer` 或自定义类型名。 |
| `thoroughness` | 仅 `explore` 使用：`quick`、`medium` 或 `very_thorough`。其他类型忽略。 |
| `background` | 默认 `true`：立即返回子智能体 ID，稍后用 `get_command_or_subagent_output` 取结果，完成时也会收到通知。设为 `false` 时前台等待；超过前台预算会自动转后台并返回 ID。 |
| `isolation` | `none`（共享工作区，默认）或 `worktree`（隔离的 Git 工作树）。 |
| `context` | `fresh`（默认，空记录）或 `fork`（复制父级对话作为起点）。fork 子代理继承父模型，默认只读，除非类型是 `implementer`；设置 `resume_from` 时忽略。 |
| `output_schema` | 可选的 JSON Schema，约束子代理的最终答案。见下文。 |
| `resume_from` | 继续一个已结束的子智能体对话。传入其子智能体 ID。 |
| `cwd` | 子智能体的工作目录。与 `isolation: worktree` 互斥；设置 `resume_from` 时忽略（恢复的子级继承源目录）。 |
| `model` | 可选的模型 slug。默认继承父级；设置 `resume_from` 时忽略（沿用源模型）。 |

后台运行子智能体时，稍后使用 `get_command_or_subagent_output` 获取其结果。

### `output_schema`

传入一个 JSON Schema 对象后，子代理的最后一轮必须以单个 JSON 值结束，Grok Build 按 schema 校验：

- 支持的关键字：`type`、`properties`、`required`、`items`、`enum`、`additionalProperties`、`minItems`、`maxItems`、`minLength`、`maxLength`、`minimum`、`maximum`、`description`、`$defs`、`definitions`、`$ref`。
- 含其他关键字或结构非法的 schema（例如裸 `true`/`false`）在 spawn 前直接报错，不消耗子代理 token。
- 校验失败，或子代理没有给出 JSON 时，该子代理以失败结束并返回具体原因。
- 通过校验的结果放在结果的 `structured` 字段中，父级与 workflow 可以直接解构；省略 `output_schema` 时仍返回自由文本，行为不变。

<a id="sending-messages-to-subagents"></a>
### 向子智能体发送消息

`send_subagent_message` 默认开启。用 `GROK_ACTIVE_AGENT_MESSAGES=0` 或 `[features] active_agent_messages = false` 关闭（作为 kill switch）。根会话可以向自己拥有的子智能体发送后续消息；开关启用后，获授权的子智能体也可获得该工具，其提示会注入 `<agent-roster>`，列出主代理和已命名的兄弟子代理。

- `subagent_id: "parent"` 指向该子智能体仍活动的父子智能体。
- 持久智能体 ID 指向另一个本地子智能体；符合条件的已完成子智能体会以相同身份恢复。

父级为根会话的子智能体不能向根会话发消息。精选工具集不会提供该工具；不允许此类工具的能力模式也会将其移除。子级发送方每个发送方—目标对最多有 4 条在途消息，每次尝试最多向外发送 32 条，超过限制返回 `QuotaExceeded`。

非活动子智能体始终被唤醒，并以该消息作为下一轮。对活动子智能体，可选 `delivery` 参数控制投递：`steer`（默认）在下一个安全点进入当前轮次；`queue` 等待成为受保护的后续轮次；`interject` 是紧急插话，在最早安全点优先于待处理的引导消息送达，并打断阻塞的后台等待。只结束等待调用，后台工作继续运行。

活动但处于轮次间隙的子智能体会将 `steer` 或 `interject` 转成一个受保护排队轮次并开始处理。旧的 `queue: true` 仍等价于 `delivery: "queue"`；两者同时提供时以 `delivery` 为准。

记录将每次发送显示为一行 Message：结果动词、子智能体标签（类型、persona 或角色）、弯引号包围的描述（只取首行，最多 40 个字符）。行会区分已发送、已排队、已插话、发送中、被拒绝或未确认；发送中显示动画圆点；发送给父级时标为 parent。

折叠行不显示消息和原因。按右方向键（Vim 模式 `l`/`e`）展开请求投递方式、完整消息及拒绝/未确认原因，按左方向键（`h`）折叠。Enter、Ctrl+F 或双击可打开对应子智能体视图。若子智能体不在本会话中创建（例如无界面导出或其他会话的 ID），以 ID 最后 8 个字符标注 `subagent …xxxxxxxx`，展开显示原始 `Subagent ID:`，且无法打开视图。

<a id="listing-subagents"></a>
### 枚举子智能体（`list_agents`）

`list_agents` 列出当前会话派出的子智能体，模型不必记住 id。

| 参数 | 说明 |
| --- | --- |
| `include_finished` | 是否包含已结束的子代理，默认 `true`。 |
| `parent_id` | 只列该子代理的后代；省略时列整棵会话树。 |
| `limit` | 返回行数上限，1–50，默认 50。 |

每行包含 `id`、`label`、`type`、`status`、`model`、`parent_id`、`depth`、`elapsed_ms`、`tokens_in`、`tokens_out`、`context_pct`、`current_tool`、`queue_position`、`resumable`。

| `status` | 含义 |
| --- | --- |
| `queued` | 已准入，等待并发槽。 |
| `running` | 正在跑一轮。 |
| `idle` | 在线，处于轮次间隙。 |
| `ready` | 已结束，保留可恢复会话。 |
| `completed` / `failed` / `cancelled` | 终态。 |

`resumable` 为 true 的行（通常是 `ready`）可以直接用 `resume_from` 继续；`failed` / `cancelled` 行可能只保留部分记录，恢复不保证成功。用 `id` 配合 `send_subagent_message` 给存活的子代理发消息，或用 `kill_command_or_subagent` 终止。workflow 派生的子代理不出现在列表里。

---

<a id="capability-modes"></a>
## 能力模式

能力模式不是创建子智能体时的参数。子智能体可用的工具由其**智能体类型**以及**角色 / 定义的默认值**决定。`general-purpose` 与 `implementer` 使用完整工具集；内置 `explore` 和 `plan` 类型只读取和搜索，不能运行 shell 或编辑文件。

| 模式         | 读取 | 写入 | 执行 | 说明                                  |
| ------------ | ---- | ---- | -------- | -------------------------------------------- |
| `read-only`  | 是   | 否   | 否       | 读取、搜索和检查（也包括网页搜索和 LSP）；不允许文件编辑或 shell。 |
| `read-write` | 是   | 是   | 否       | 读取，以及创建、编辑、删除和移动文件。不允许 shell。 |
| `execute`    | 是   | 否   | 是       | 读取，以及运行 shell 命令和后台任务。不允许文件编辑。 |
| `all`        | 是   | 是   | 是       | 不受限的工具访问；`general-purpose` 的默认值。 |

---

<a id="context-inheritance"></a>
## 上下文继承

<a id="resume-from"></a>
### `resume_from`

`resume_from` 参数允许新的子智能体从已结束的子智能体继续，这对多阶段工作流很有用：

1. 生成一个研究子智能体来调查问题。
2. 生成第二个子智能体，并将第一个子智能体的 ID 设置为 `resume_from`，使其获得完整的研究上下文。

边界：

- 源子智能体必须已结束（`completed` / `failed` / `cancelled`），且属于当前父会话；其会话记录必须仍在当前 workspace 的会话目录中。`list_agents` 输出的 `resumable` 反映这一点。
- 源仍在运行时不再直接报错：`prompt` 会按 `send_subagent_message` 的 `queue` 语义排队，作为它的下一轮执行，调用返回说明文本。
- 恢复出的子智能体继承源的记录、工具状态、模型和子智能体类型；传入的 `subagent_type` 与 `model` 被忽略，系统提示和工具按当前定义重新渲染。若当前定义缺少源运行用过的工具，只记录警告。
- 源对话必须能装进模型上下文窗口；超出恢复上限时恢复失败，提示先压缩或改用更大上下文的模型。
- `max_turns` 截断的子智能体返回 `partial = true`，可以用 `resume_from` 继续。
- 设置 `resume_from` 时 `context` 与 `cwd` 被忽略。

<a id="mcp-inheritance"></a>
### MCP 继承

默认情况下，子智能体继承父会话中**已经连接**的 MCP 服务器。这包括本地 stdio/HTTP 服务器和插件提供的智能体（例如 `my-plugin:reviewer`）。子级通过 `search_tool` / `use_tool` 以与父级相同的方式发现和调用这些工具。

通过智能体 frontmatter 的 `mcpInheritance` 控制继承：

| 值 | 效果 |
| ----- | ------ |
| `all`（省略时的默认值） | 继承父级已连接的每个 MCP 服务器 |
| `none` | 不继承父级 MCP 服务器 |
| `named: [server, …]` | 仅继承列出的服务器名称 |
| `except: [server, …]` | 继承父级除列出名称外的所有服务器 |

示例：

```yaml
---
name: research-only
description: Read MCP tools but not internal connectors
tools: search_tool, use_tool, Read
mcpInheritance:
  except:
    - internal-tools
---
```

**插件智能体**也会以相同方式继承父级 MCP。出于安全原因，它们仍不能：

- 在智能体 frontmatter 中声明自己的 `mcpServers`（会带警告忽略）
- 在智能体 frontmatter 中声明钩子
- 设置 `permissionMode: bypassPermissions`

插件捆绑的 MCP 服务器（插件 `.mcp.json`）在插件受信任后仍附加到**父级/会话**，而不是仅在子级 frontmatter 中声明。参见[插件](09-plugins.md)和[MCP 服务器](07-mcp-servers.md)。

---

<a id="isolation-worktree-mode"></a>
## 隔离：工作树模式

对于会修改文件的任务，请使用 `isolation: worktree` 在隔离的 Git 工作树中运行子智能体。这可避免子级编辑与父级冲突：

- 子智能体在自己的工作树副本中工作。
- 其变更在合并前都与父级隔离。
- 完成时若工作树没有任何改动，Grok Build 直接删除它，并清掉子代理记录里的路径。
- 有改动时保留工作树，结果包含 `<worktree_path>` 与 `<worktree_actions>`：
  - apply：`x.ai/git/worktree/apply`，参数 `{"worktreePath": "…", "mode": "merge"}`（`mode: "overwrite"` 直接覆盖父级文件）。
  - discard：`x.ai/git/worktree/remove`，参数 `{"worktreePath": "…", "force": true}`。
- 开启 `[features] subagent_worktree_snapshot` 时，有改动的工作树会先快照再删除。

工作树基点由 `[subagents] worktree_base` 控制：`head`（默认，取父工作树当前 `HEAD`）或 `default_branch`（取仓库默认分支）。环境变量 `GROK_SUBAGENTS_WORKTREE_BASE` 优先于配置文件。

---

<a id="configuration"></a>
## 配置

<a id="per-type-toggles-and-model-overrides"></a>
### 按类型切换和模型覆盖

禁用特定智能体类型，或将其路由到其他模型：

```toml
[subagents.toggle]
explore = true                       # 默认——省略以保持启用
plan = false                         # 禁用 plan 子智能体

[subagents.models]
explore = "grok-build"               # 将 explore 路由到特定模型
```

按类型的模型覆盖适用于任何父级。没有覆盖时，子智能体继承父级模型。

<a id="depth-total-and-concurrency"></a>
### 深度、总量与并发

```toml
[subagents]
max_depth = 2            # 默认 2；1 表示只有根会话可以派发
max_total_agents = 16    # 每个根会话累计上限（排队、运行、已完成都计入）
max_concurrent = 32      # 同时运行上限
limit_behavior = "queue" # queue（默认）或 fail
```

对应环境变量：`GROK_SUBAGENTS_MAX_DEPTH`、`GROK_SUBAGENTS_MAX_TOTAL_AGENTS`、`GROK_MAX_CONCURRENT_SUBAGENTS`、`GROK_SUBAGENT_LIMIT_BEHAVIOR`。这些上限不能设为 0 来关闭；小于 1 的值会被压到 1。

<a id="custom-roles-and-personas"></a>
### 自定义角色和 Persona

定义带有自身能力和模型默认值的自定义角色：

```toml
[subagents.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-build"
prompt_file = ".grok/prompts/researcher.md"
```

定义带行为指令的自定义 Persona：

```toml
[subagents.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".grok/personas/concise.md"  # 或从文件加载
```

Grok Build 还会从 `.grok/roles/*.toml` 发现角色，从 `.grok/personas/*.toml` 发现 Persona。内联 `config.toml` 定义的优先级高于文件。

---

<a id="the-tasks-pane-tui"></a>
## 任务窗格（TUI）

Grok Build 会在智能体屏幕的侧窗格中显示运行中和已完成的工作：

- 按 `Ctrl+G` 切换任务窗格，其中列出活动和已完成的子智能体以及带状态的后台命令。
- 按 `Ctrl+T` 切换独立的待办窗格。

要查看可用的智能体类型和 Persona，请用 `Ctrl+P` 打开命令面板，然后选择 **Manage Agents**（`/config-agents`）。

子智能体会出现在任务窗格顶部自己可折叠的“Subagents”分组中。

---

<a id="viewing-subagents-in-the-tui"></a>
## 在 TUI 中查看子智能体

交互式 TUI 的多个位置都会显示子智能体：

<a id="scrollback-parent-conversation-history"></a>
### 回滚区（父级对话历史）

生成子智能体时，父级回滚区会添加一个紧凑的生命周期块：

- `Subagent running: "do the thing" (Implementer · grok-3) · Thinking`
- 对于后台子智能体：`Subagent started: "..."`

运行中时，该块显示实时活动后缀（例如“Running: cargo test”“Compacting”“Retrying (2/3)”），取自子级的轮次跟踪器。项目符号会根据状态动画显示（或着色）。

按 **Enter**（或 Ctrl-F）可打开子智能体的完整记录。

对于阻塞式子智能体，子级完成时同一条记录会更新项目符号颜色。对于后台子智能体，还会追加 `Subagent completed/failed/cancelled in Xs: "..."` 块。

<a id="tasks-pane-ctrl-g"></a>
### 任务窗格（Ctrl+G）

如上所述，任务窗格按“Subagents”分组，显示加载动画、耗时和终止/检查入口。按 `h` 切换隐藏已完成与显示全部。

### 停靠面板（启用时）

提示框上方的停靠面板列出子智能体。焦点在停靠面板时，`h` 使用与任务窗格相同的完成项过滤；左/右方向键折叠或展开分区标题。

<a id="fullscreen-framed-view-the-child-transcript"></a>
### 全屏框架视图（子级记录）

从回滚区、任务窗格或仪表盘行打开子智能体时，带边框的视图替代父视图，显示子级完整记录。标题包含状态、标签、粗体描述、模型、恢复/分叉徽章、活动与耗时，以及 [✗] 关闭按钮。子级的回滚、思考和工具调用在框架内显示；父级任务、待办、停靠和目录面板暂时隐藏。

此视图只用于观察。输入框完全隐藏（高度为零），不能聚焦、输入、暂存或发送后续提示。提示仍归父会话所有；要引导子智能体，请关闭视图，从父级调用 `send_subagent_message`。

仍可滚动、折叠、复制、打开链接和块查看器。`Ctrl+C` 仅取消**当前子级**轮次，不取消父级；`Ctrl+.` / `Ctrl+X` 打开子级快捷键帮助。标题中的 Dashboard 和 `‹`/`›` 仍操作父级。空闲时在块查看器按 Enter 会把选中行引用到父级输入框并关闭视图。

仅根会话可用的快捷键在这里不会执行，也不会透传给父级：命令面板、模型/会话选择器、设置、扩展、始终批准、转后台、外部编辑器和 Shift+Tab 模式循环均只触发静默重绘，不显示提示。若显示提示队列，它只是只读镜像，不能编辑、立即发送或删除；队列 RPC 始终以父会话为目标。

在普通回滚区按 `q`、`Esc` 或点击 [✗] 返回。若正在搜索，先关闭搜索，再按一次退出子视图。`Ctrl+Q` 始终退出 Grok，不会被吞掉；VS Code 系列使用对应退出键 `Ctrl+D`。关闭后父回滚区仍显示子智能体状态。

---

<a id="depth-limits"></a>
## 深度限制

子智能体可以再派子智能体，默认最多嵌套两层：根会话（深度 0）→ 第一层子智能体（深度 1）→ 第二层（深度 2）。`[subagents] max_depth`（或 `GROK_SUBAGENTS_MAX_DEPTH`）调整上限；设为 1 时只有根会话能派发。

到达深度上限时 `spawn_subagent` 仍然可见，调用会返回明确的深度限制错误（`Subagent depth limit exceeded (current depth: …, max: …)`），而不是让工具静默消失。

除深度外还有两个上限：每个根会话累计最多 `max_total_agents`（默认 16）个子代理，同时运行最多 `max_concurrent`（默认 32）个。累计上限超限时 spawn 被拒绝，并提示先收集已有结果或用 `resume_from` 复用；并发上限超限时默认排队，`limit_behavior = "fail"` 改为直接失败。

---

<a id="when-to-use-subagents"></a>
## 何时使用子智能体

**适合的场景：**

- 父级继续其他工作时研究代码库
- 父级实现功能时并行运行测试
- 提交前审查生成的变更
- 委派彼此独立、互不依赖的任务

**不适合的场景：**

- 父级可以直接处理的简单任务
- 需要与用户紧密往返的任务，因为子智能体自主运行，不适合交互式交流
- 上下文准备成本超过并行收益的任务
