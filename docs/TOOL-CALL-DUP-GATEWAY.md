# 工具调用重复 · 网关侧方案（方法 1）

状态：待实施　日期：2026-11-04 (UTC+8)  
对侧文档：`docs/TOOL-CALL-DUP-GROK-BUILD.md`（方法 2–4）  
本文件供**网关/模型链路工作区**接续；Grok Build 侧只读对侧文档即可。

---

## 1. 问题（两侧共用结论）

两会话 `01a1a463-dfd5-7891-a8b2-fd227e3e00a2`、`01a1a46e-08f9-72f0-b54d-d866e00c20f9` 中，同一轮 assistant 消息内出现**相同 `name+arguments`、不同 `call_id`** 的多条工具调用（最多 8 条/组），并伴随：

- 正文残留原始 `function=` / `tool_call` XML（参数错位，如 `path` 内嵌 `{"query":...}`）；
- 参数流损坏签名：`{"context_lines": {}}`、嵌套 map、参数中间插入换行；
- `events.jsonl` 的 `tool_started` 与结构化 `tool_calls` 条数一致 → **真执行了，不是 UI 画重**。

**根因在网关/上游参数流**：工具参数 JSON 被截断或改写后，模型侧反复重发同一调用；Grok Build 再缺同轮去重与副作用闸，界面上就连成一片。

链路（本机约定）：`grok-mode-boost(8471) → AxonHub(8090) → 上游渠道`。

---

## 2. 方法总表与推进顺序（回答：能否并行）

| # | 方法 | 归属 | 可否与对侧并行 | 本侧内部顺序 |
|---|------|------|----------------|--------------|
| **1** | **稳定参数流（8471 → AxonHub 8090）** | **网关侧（本文件）** | **可以**，不依赖方法 2–4 | 本侧唯一主项，无 1a/1b 强制先后 |
| 2 | 同轮按 `(name, arguments)` 去重 | Grok Build | 可与方法 1 并行 | 对侧：建议先于或与 3 同批 |
| 3 | 副作用工具执行闸（同参已在跑则跳过） | Grok Build | 可与方法 1 并行 | 对侧：2 之后或与 2 同批 |
| 4 | 展示层折叠正文里的原始 tool_call XML | Grok Build | 可与方法 1 并行 | 对侧：任意时刻，不阻塞 2/3 |

**结论：**

- **方法 1 与方法 2/3/4 没有前后依赖，两边工作区可同时开工。**
- 方法 1 是**治本**：参数流不稳时，2–4 只能减少重复的执行与噪音，模型仍会浪费轮次、正文仍可能吐 XML。
- 方法 2–4 是**客户端纵深防御**：1 未完成前就能显著改善体验；1 完成后仍建议保留（防上游回归、防其它渠道）。
- 网关侧**不阻塞**等 Grok Build；Grok Build **也不阻塞**等网关。合并/发版可各自节奏，验收时做一次端到端联测即可。

---

## 3. 本侧任务：方法 1 — 稳定参数流

### 3.1 目标

模型流式返回的工具调用参数，在到达 Grok Build 采样层之前保持**完整、单次、可解析的 JSON**；不再出现截断、错位、同参多 `call_id` 的重发放大。

### 3.2 已知故障签名（回归时对照）

上游/网关日志与会话回放中出现任一即视为参数流仍坏：

1. `Failed to parse arguments for tool ... missing field ...` + `Your original arguments: {}`
2. 参数 JSON 在键后 EOF：`{"path": ` / `{"path": {}`
3. 参数值中间被插入换行：`.grok/stage-log\n.md`、`timeout_ms: 5\n000`
4. 同一轮内相同 `name+arguments` 出现多个 `call_id`（chat_history 可统计）
5. assistant `content` 含字面 `function=` / `tool_call` XML，且与结构化 `tool_calls` 同参

会话侧统计脚本思路（只读，网关工作区可复用）：对 `chat_history.jsonl` 中每条 `type=="assistant"`，按 `(name, arguments)` 分组，`count>1` 即命中签名 4。

### 3.3 建议工作项

1. **分段定位**  
   - 8471（grok-mode-boost）是否改写/缓冲/重放 SSE；  
   - AxonHub 8090 是否合并 chunk、重试上游时重复下发 `function_call` / `tool_call` delta；  
   - 上游渠道本身是否在参数未闭合时吐出多段 item。

2. **透传与单次下发**  
   - 工具参数 delta：只拼接、不二次解析再广播；  
   - 同一 `call_id` 的 `output_item_added` / `function_call` 只允许一次；  
   - 上游重试时：新 `call_id` 或明确丢弃重复 item，禁止同参多 id 并行进入下游。

3. **拒绝坏参数回放**  
   - 若历史请求里已带损坏的 `tool_calls`，网关侧可标记/剥离后再转发（与 Build 侧 `sanitize_tool_arguments` 对齐，但**不要两边同时静默改写同一字段**以免对不齐；约定：Build 负责请求侧 sanitize，网关负责流式响应不放大损坏）。

4. **可观测**  
   - 指标：参数 JSON 解析失败率、同轮同参多 id 次数、`function_call_arguments.delta` 后无 `.done` 的比例；  
   - 告警阈值先参考两会话基线，再收紧。

### 3.4 验收（端到端，需对侧配合时联测）

| 检查 | 通过标准 |
|------|----------|
| 正常多工具并行 | 不同 `name`/不同 `args` 仍并行，**不被误去重** |
| 构造参数截断 | 网关不放大为多条同参 `call_id`；Build 侧至多 1 条执行 |
| 回放两会话同类负载 | `chat_history` 同轮 `(name,args)` 重复组 = 0（或仅历史脏数据，新轮为 0） |
| 正文 | 新轮 assistant content 不再出现字面 `function=` 工具 XML（模型行为，网关侧至少不注入、不重复拼接） |

### 3.5 明确不在本侧

- 同轮去重、副作用执行闸、UI 折叠 → 全部见对侧 `TOOL-CALL-DUP-GROK-BUILD.md`。  
- 不要在网关「替 Build 执行去重后仍下发多条」——去重决策权在 Build 派发层；网关职责是**不制造重复**。

---

## 4. 与方法 2–4 的关系（写给网关工作区）

```
[网关方法1 参数流稳定]     ──可并行──     [Build 方法2 去重]
        │                                      │
        │                                      ├──方法3 执行闸（建议同批或紧随）
        │                                      └──方法4 展示折叠（随时）
        └──────── 两端都合入后做一次联测 ────────┘
```

- **无强制串行。**  
- 若只能先做一边：**先做本侧方法 1**（减少根因流量），或 **先做 Build 方法 2+3**（立刻止血重复执行）——按人力并行最优。  
- 联测用例与故障签名以本文 §3.2 为准，避免两侧各写一套口径。

---

## 5. 会话证据索引（只读）

| 会话 | 路径（本机） |
|------|----------------|
| GrokZen | `C:\Users\bypassnro\.grok\sessions\C%3A%5CUsers%5Cbypassnro%5CDocuments%5Cproject%5Cgrok%5CGrokZen\01a1a463-dfd5-7891-a8b2-fd227e3e00a2` |
| translate | `C:\Users\bypassnro\.grok\sessions\C%3A%5CUsers%5Cbypassnro%5CDocuments%5Cproject%5Ctranslate\01a1a46e-08f9-72f0-b54d-d866e00c20f9` |

关键文件：`chat_history.jsonl`（结构化重复与正文 XML）、`events.jsonl`（`tool_started` 条数）、`updates.jsonl`。
