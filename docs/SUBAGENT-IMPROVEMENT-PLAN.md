# Grok Build 子代理机制改进计划

> 约束：只使用一个 harness（Grok Build 本体）和一个模型。本文不涉及多 provider、外部 harness、跨模型路由。
>
> 目标：在同一模型下，让子代理**选得对、管得住、读得清、接得回**。

---

## 1. 背景

Grok Build 的子代理底座已经比较完整：协调器、并发准入与排队、前台超时自动转后台、`resume_from`、worktree 隔离、完整子转录、`SubagentStart/Stop` 钩子、按类型模型覆盖、persona 输入输出契约都已存在。

当前主要问题集中在四层：

1. **模型看不见内置类型。** `subagent_type` 只枚举插件/项目/用户自定义 agent，`general-purpose / explore / plan` 不在 schema 里，默认只能派 `general-purpose`。
2. **控制面不完整。** 没有 `list_agents`；父代理只能记住 id；`send_subagent_message` 默认关闭。
3. **结果只有散文。** `output_schema` 只存在于内部运行时，模型和 workflow 都拿不到结构化结果。
4. **运行态对模型不可读。** Ctrl+G、全屏转录、dashboard 各看一部分，且都只供人查看；模型拿不到一份带 token、当前工具、队列位置和恢复入口的机器可读 snapshot。

---

## 2. 目标与非目标

### 目标

- 模型能按任务性质选择只读调查、规划、实现三种工作模式。
- 模型能枚举自己派出的子代理，知道谁在跑、谁空闲、谁可恢复。
- 子代理结果可以被 workflow 和父代理直接消费，不需要解析自然语言。
- 子代理可以 fork 父对话、嵌套一层、在达到轮次上限后继续。
- 父代理和 workflow 能通过 `list_agents` 机器可读地枚举子代理的运行态（token、当前工具、队列位置、可恢复性），由模型自主巡检，不需要人工查看界面。
- 批量委派在同一 assistant step 内真正并行。

### 非目标

- 不引入 ACP / Codex / Claude Code 等外部子代理 provider。
- 不做跨模型路由、模型允许列表、`list_subagent_models`。
- 不重写 workflow 引擎；只让 workflow 消费结构化子代理结果。
- 不改变现有自定义 agent 的发现、权限和工具过滤规则。

---

## 3. 设计原则

1. **先让模型选对，再谈并行。** 类型、提示、工具集是第一优先级；调度优化排在后面。
2. **每个能力都要有消费者。** `output_schema` 的第一消费者是 workflow 和父代理的后续步骤，不做“为了结构化而结构化”。
3. **新增字段保持向后兼容。** 所有 schema 字段可选；缺省行为与今天一致。
4. **危险能力用预算兜底。** 深度、总子代理数、并发数、轮次上限都要有独立上限。
5. **可观测性与控制面共用一份 DTO。** `list_agents` 读同一份 snapshot，TUI 现有视图维持现状，不为 TUI 新增界面。

---

## 4. 分阶段计划

### 阶段 0：基线与回归护栏（0.5–1 人日）

**目标**：在改行为之前，先能回答“今天模型到底派了什么”。

**改动点**

- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs`
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/spawn.rs`

**内容**

- 在 spawn 成功路径增加一条结构化 debug 日志：`subagent_type`、`context`（fresh/fork）、`depth`、`background`、prompt 字节数、model、effort。
- 增加测试专用计数器，记录每个内置类型被选中的次数（当前应全部是 `general-purpose`）。
- 固化现有行为快照：默认省略 `subagent_type` 仍得到 `general-purpose`；自定义类型枚举仍可用。

**验收**

- 一次 `RUST_LOG` 运行能导出每个 spawn 的 JSON 行。
- 回归测试明确锁定“当前默认只派 general-purpose”，后续改动必须显式更新该测试。

---

### 阶段 1：委派路由（P0，2–3 人日）

**目标**：让模型在同一模型下，按任务性质选择合适的工作模式。

**1.1 内置类型进入 `subagent_type` 枚举**

改动点：

- `crates/codegen/xai-grok-agent/src/builder.rs` 的 `selectable_subagent_types()`：当前明确排除内置类型，改为在未被 toggle 关闭、未被 allowlist 排除、未被同名自定义 agent 覆盖时，加入 `general-purpose / explore / plan`。
- 复用 `xai_tool_types::BUILTIN_SUBAGENTS` 的 `SubagentDescriptor`，工具摘要用当前 session 的真实工具名渲染。
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/model_policy.rs` 的 `subagent_type_property()`：保持“省略 = general-purpose”的语义，同时把内置类型排在自定义类型之前。

**1.2 增加 `thoroughness` 参数**

改动点：

- `crates/common/xai-tool-types/src/task.rs` 的 `TaskToolInput`：新增 `thoroughness: Option<Thoroughness>`，取值 `quick | medium | very_thorough`。
- `TaskTool::run` 在 `subagent_type == "explore"` 时，把彻底度写进子任务提示的首行；其他类型忽略并给出警告日志。

**1.3 重写 general-purpose 提示，补一个 implementer 类型**

改动点：

- `crates/common/xai-tool-types/src/task.rs` 的 `GENERAL_PURPOSE_PROMPT`：当前“Strengths”全是搜索与多文件分析，缺少实现型约束。改为覆盖：先读后改、最小 diff、改完跑测试、报告改动文件与验证结果。
- 可选新增内置 `implementer`：`read-write-execute` 工具集，提示聚焦“实现 + 自测”，与偏研究的 `general-purpose` 分工。
- 修正 `EXPLORE_PROMPT` 中“按调用方给的彻底度调整”的表述，使其对应真实参数。

**1.4 按类型设置默认 effort / max_turns**

- `explore`：低 effort，`max_turns` 12。
- `plan`：中 effort，`max_turns` 20。
- `implementer` / `general-purpose`：继承会话 effort，`max_turns` 40。

**验收**

- 工具 schema 中能看到三种内置类型及各自描述。
- 评测集里“只调查、不改文件”的任务选择 `explore`；“实现并跑测试”的任务选择 `implementer` 或 `general-purpose`。
- `thoroughness` 出现在 explore 子代理的首条用户消息中。
- 省略 `subagent_type` 的行为保持不变。

---

### 阶段 2：控制面（P0，2–3 人日）

**目标**：模型不再依赖记忆中的 id 管理子代理。

**2.1 新增 `list_agents` 工具**

改动点：

- 新文件：`crates/codegen/xai-grok-tools/src/implementations/grok_build/list_agents.rs`
- 注册：`crates/codegen/xai-grok-agent/src/config.rs` 的 `grok_build_core_toolset_with`、`grok_build_plan_toolset`
- 协调器：`task/coordinator/query.rs` 已有 `ListRunning` / `Inspect`，补一个“列直接子级 + 可选后代”的查询。

输入：

```json
{
  "include_finished": true,
  "parent_id": "optional-subagent-id",
  "limit": 50
}
```

输出每行至少包含：

`id, label, type, status, model, parent_id, depth, elapsed_ms, tokens_in, tokens_out, context_pct, current_tool, queue_position, resumable`

状态映射：

- `queued`：已准入、等并发槽。
- `running`：正在跑一轮。
- `idle`：在线、轮次间隙。
- `ready`：已结束但保留可恢复会话。
- `completed / failed / cancelled`：终态。

**2.2 默认开启 `send_subagent_message`**

- 保留 feature flag 作为 kill switch，默认值改为开启。
- 更新 `16-subagents.md` 的“默认关闭”说明。
- 子代理提示中加入 sibling roster（main + 已命名兄弟），仅在开启消息工具时注入。

**2.3 增加可读 `label`**

- `TaskToolInput` 新增可选 `label`（3–5 个词），写入 `SubagentSnapshot`。
- `list_agents` 输出优先显示 label，id 作为兜底。

**验收**

- 模型可以在不记住 id 的情况下列出子代理并选择继续/打断。
- `list_agents` 与 TUI 显示同一组状态。
- 关闭 feature flag 时行为与今天一致。

---

### 阶段 3：结果契约（P1，3–5 人日）

**目标**：子代理结果可被 workflow 和父代理机器消费。

**3.1 新增 `output_schema` 参数**

- `TaskToolInput` 新增 `output_schema: Option<serde_json::Value>`。
- spawn 前做 JSON Schema 子集校验：`type / properties / required / items / enum`；不支持的关键字直接报错，避免“看起来生效、实际忽略”。
- 校验通过后写入 `SubagentRuntimeOverrides.output_schema`（字段已存在）。

**3.2 子代理侧 `submit_result` 工具**

- 仅在存在 `output_schema` 时注入。
- 工具输入为任意 JSON，服务端按 schema 校验；校验失败返回可修正的错误，让子代理重试。
- 子代理结束时若从未调用，返回文本结果并标记 `stop_reason = "no_structured_result"`。

**3.3 扩展结果模型**

改动点：`task/types.rs` 的 `SubagentResult`、`xai_tool_types::SubagentCompletedOutput`。

新增字段：

- `structured: Option<serde_json::Value>`
- `stop_reason: String`（`end_turn / max_turns / cancelled / no_structured_result / error`）
- `partial: bool`

**3.4 父级与 workflow 消费**

- 完成文本保留自然语言摘要，追加一段紧凑 JSON（或单独的 structured 输出项）。
- workflow 的 `agent()` 直接返回 `structured`，没有 schema 时维持现有文本返回。
- `max_turns` 到达上限时返回 `partial = true`，允许 `resume_from` 继续。

**验收**

- “返回 `{files: string[]}`”的子代理能被 workflow 直接解构。
- 非法 schema 在 spawn 前失败，不消耗子代理 token。
- 无 schema 的调用行为完全不变。

---

### 阶段 4：上下文与生命周期（P1，3–4 人日）

**目标**：子代理能继承该继承的上下文，也能在合理的边界内继续工作。

**4.1 暴露 `context: fresh | fork`**

- `TaskToolInput` 新增 `context`，映射到已有的 `SubagentRequest.fork_context`；`TaskTool` 当前硬编码 `false`。
- `xai-grok-shell/src/agent/subagent/mod.rs` 已有 `InitialContextSource::Forked`，复用现有实现。
- fork 子代理默认只读；需要写入时必须显式设置 `context: fork` + `subagent_type: implementer`。
- fork 固定继承父模型与会话前缀，保留缓存收益。

**4.2 深度与总量预算**

- 新增 `[subagents] max_depth`，默认 2（可选 1/2/3）；`MaxSubagentDepth` 资源已存在。
- 到顶时工具仍可见，调用返回明确错误（与 dsh / Claude Code 一致），避免模型误以为工具消失。
- 新增 `[subagents] max_total_agents`，默认 16，按根会话统计；超过时拒绝并提示“先收结果再派”。
- 保留现有 `GROK_MAX_CONCURRENT_SUBAGENTS`（默认 32）作为运行并发上限。

**4.3 放宽 resume 语义**

- `resume_from` 指向运行中子代理时，转为排队 follow-up（`send_subagent_message` 的 queue 语义），不再直接报错。
- 允许 `ready` 状态的已结束子代理在同一工作区恢复；跨父会话恢复先限制在同一 workspace。
- 同类型限制改为“工具集兼容”：只要源类型仍可解析、工具集未收窄到缺工具，就允许恢复。
- 持久化 spawn 时的有效描述符（type / model / tools / persona），恢复时复用，避免定义漂移。

**4.4 轮次上限与 partial**

- `max_turns` 到达时不再按失败处理，返回 `partial = true`、`stop_reason = "max_turns"`，并保留恢复入口。

**验收**

- fork 子代理能回答“我们刚才决定了什么”。
- 深度 2 的子代理能再派一个只读子代理；达到 `max_total_agents` 时优雅拒绝。
- `max_turns` 截断的子代理可以 resume 并继续。

---

### 阶段 5：运行态数据（P1，1–2 人日）

**目标**：让模型（而不是人）能机器可读地判断“谁在跑、卡在哪、花了多少”。

**5.1 扩展 snapshot**

改动点：`task/types.rs` 的 `SubagentSnapshot`。

新增：

- `label`、`model`、`parent_id`、`depth`
- `tokens_in`、`tokens_out`、`context_used`、`context_window`、`context_pct`
- `current_tool`、`last_activity`、`queue_position`、`resumable`

这些字段是阶段 2 `list_agents` 输出列的直接数据来源，与 2.1 一并消费；TUI（Ctrl+G、dashboard、全屏转录）维持现状，不新增面向人的子代理树面板。

**验收**

- `list_agents` 输出包含上述字段，与内部 snapshot 一致。
- 阶段 2 的工具摘要与 TUI 现有视图回归不受影响。

---

### 阶段 6：调度与收尾（P2，2–3 人日）

**目标**：批量委派真正并行，文档与行为一致。

**6.1 委派并发安全**

- `TaskTool` 的 `ToolCapabilities` 增加 `max_concurrency`（建议 16），并确认批内调度不会把 spawn 当独占屏障。
- 增加测试：同一 assistant step 的两个前台 spawn 必须同时进入 `running`；任一先完成不阻塞另一个。
- 参考 dsh 的教训：委派工具声明并发安全后，同一步的多个委派在滚动池内重叠。

**6.2 worktree 收尾**

- 子代理无改动时自动清理 worktree。
- 有改动时在完成结果中提供 apply / discard 入口。
- 可选配置 worktree 基点为 `HEAD` 或默认分支。

**6.3 文档与配置一致性**

更新 `~/.grok/docs/user-guide/16-subagents.md`：

- `background` 默认值（代码为 `true`，文档当前写 `false`）。
- 深度默认值与 `max_total_agents`。
- 内置类型可选及选择建议表。
- `list_agents`、`output_schema`、`context: fork` 的用法。
- `resume_from` 的新边界。

**6.4 指标**

- 只记录本地日志/现有遥测：按类型 spawn 数、排队时间、运行时长、tokens、失败原因分布。
- 不新增后端服务。

**验收**

- 批量 fan-out 不串行。
- 空 worktree 不残留。
- 文档描述与代码默认值一致。

---

## 5. 测试计划

### 单元测试

- `model_policy`：内置 + 自定义类型的 schema 合并、排序、allowlist/toggle 过滤。
- `TaskTool`：`thoroughness` 只对 explore 生效；`output_schema` 非法时 spawn 前失败；`context` 映射正确。
- `SubagentResult`：`stop_reason` / `partial` / `structured` 序列化。
- 状态映射：queued/running/idle/ready/completed/failed/cancelled。

### 集成测试

- 使用 mock child runner：spawn → list → send message → interrupt → resume。
- fork：父历史注入、只读工具限制。
- 深度 2：嵌套 spawn 成功；深度 3 被拒绝；`max_total_agents` 被拒绝。
- `max_turns`：partial 结果可 resume。
- 并发：两个前台 spawn 同时启动。

### 端到端

- 一个真实任务：explore 调查 → implementer 实现 → workflow 汇总结构化结果。
- `list_agents` 输出与内部 snapshot 一致；TUI 现有视图回归通过。
- 回归：自定义 agent、persona、MCP 继承、worktree 隔离不受影响。

---

## 6. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| 内置类型进入枚举后模型频繁选错 | 任务变慢或工具不足 | 描述写清“何时使用”；评测集回归；保留 toggle 关闭单个类型 |
| 深度/总量放开导致 fan-out 爆炸 | token 与并发飙升 | `max_depth` + `max_total_agents` + 现有并发队列三重上限 |
| `output_schema` 校验过严 | 子代理反复重试 | 只支持 JSON Schema 子集；错误信息指出具体路径；无 schema 时零影响 |
| fork 复制上下文导致成本上升 | 单次 spawn 变贵 | fork 默认只读、默认不继承 skills；文档标注成本 |
| 新工具增加上下文占用 | 主提示变长 | 工具描述精简；`list_agents` 输出分页；类型描述限长 |

所有阶段都通过配置开关控制，默认关闭新行为，验证后逐项开启；schema 新增字段保持可选，旧会话与旧客户端不受影响。

---

## 7. 工作量与里程碑

| 阶段 | 内容 | 估计 |
|---|---|---|
| 0 | 基线与回归护栏 | 0.5–1 人日 |
| 1 | 类型路由 + 提示 + thoroughness | 2–3 人日 |
| 2 | `list_agents` + 消息默认开启 + label | 2–3 人日 |
| 3 | `output_schema` + 结构化结果 | 3–5 人日 |
| 4 | fork + 深度/总量 + resume + partial | 3–4 人日 |
| 5 | snapshot 扩展（供 `list_agents` 消费） | 1–2 人日 |
| 6 | 并发、worktree、文档、指标 | 2–3 人日 |

合计约 14–20 人日。建议按 1 → 2 → 3 → 4 → 5 → 6 顺序推进；阶段 5 建议并入阶段 2 一起实现（5.1 的字段是 `list_agents` 输出列的直接数据来源）。

---

## 8. 附录：建议的模型侧参数

```json
{
  "prompt": "…",
  "description": "…",
  "subagent_type": "explore | plan | implementer | general-purpose | <custom>",
  "thoroughness": "quick | medium | very_thorough",
  "context": "fresh | fork",
  "output_schema": { "type": "object", "properties": {}, "required": [] },
  "label": "auth-research",
  "isolation": "none | worktree",
  "background": true,
  "resume_from": "optional-subagent-id"
}
```

`model` 与 `reasoning_effort` 在单模型约束下保持可选：`model` 继续隐藏，`reasoning_effort` 可作为同模型内的成本/速度旋钮，默认继承会话设置。

## 9. 附录：`list_agents` 输出示例

```json
{
  "agents": [
    {
      "id": "019…",
      "label": "auth-research",
      "type": "explore",
      "status": "running",
      "model": "current-model",
      "parent_id": "root",
      "depth": 1,
      "elapsed_ms": 84210,
      "tokens_in": 18234,
      "tokens_out": 1204,
      "context_pct": 37,
      "current_tool": "search_code",
      "queue_position": null,
      "resumable": false
    },
    {
      "id": "019…",
      "label": "login-tests",
      "type": "implementer",
      "status": "ready",
      "model": "current-model",
      "parent_id": "root",
      "depth": 1,
      "elapsed_ms": 301442,
      "tokens_in": 44120,
      "tokens_out": 5120,
      "context_pct": 61,
      "current_tool": null,
      "queue_position": null,
      "resumable": true
    }
  ]
}
```
