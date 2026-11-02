# 子代理机制收尾与交接计划

> 关联计划：`docs/SUBAGENT-IMPROVEMENT-PLAN.md`（下称“原计划”），第 5、6 节。
>
> 编写时间：2026-11-02（UTC+8）。当前分支：`fix/pager-test-clipboard-isolation`。
>
> 用途：阶段 6 的剩余验收与原计划第 5 节测试计划尚未系统执行。本文档把剩余工作拆成互不依赖的步骤，供不同对话逐个完成。**每个对话只做一个任务**：实现 → 验收 → 提交 → 回填本文档状态表。

## 0. 使用方式

1. 新对话先读本文件、原计划第 5 节与阶段 6。
2. 从 §3 取第一个“待办”任务；标为“已完成”的不要重做。
3. 动手前先跑任务里的“现状核对”，确认缺口仍在；若已被其他提交覆盖，把状态改为“已覆盖”并写明证据，跳到下一个。
4. 完成后把状态改成“已完成”，补上提交哈希与验证命令；只提交本任务的改动。
5. 约束沿用原计划：单 harness、单模型；不新增后端服务；新 schema 字段可选，缺省行为不变；不顺手格式化无关文件。

## 1. 已完成（不要重做）

| 条目 | 状态 | 提交 | 备注 |
|---|---|---|---|
| 阶段 0 基线与回归护栏 | 已完成 | `f3b49426` | spawn 结构化 debug 行 + 仅测试用的内置类型选择计数 |
| 阶段 1 委派路由 | 已完成 | `f3b49426` | 内置类型可选、`thoroughness`、`implementer`、提示与 effort/max_turns |
| 阶段 2 控制面 | 已完成 | `f3b49426` | `list_agents`、`send_subagent_message` 默认开启、`label` |
| 阶段 3 结果契约 | 已完成 | `f3b49426` | `output_schema`、`submit_result`、`structured` / `stop_reason` / `partial` |
| 阶段 4 生命周期 | 已完成 | `f3b49426` | `context: fork`、深度/总量预算、resume 放宽、max_turns partial |
| 阶段 5 运行态 snapshot | 已完成（并入阶段 2） | `f3b49426` | snapshot 字段与 `list_agents` 输出列同源 |
| 6.1 委派并发安全 | 已完成 | `231153fe` / `a9d97135` | `TaskTool` 声明 `max_concurrency = Some(16)`；并行验收测试见 `a9d97135` |
| 6.2 worktree 收尾 | 已完成 | `eaf0fe70` / `f61f131a` | 无改动自动清理、apply/discard、`worktree_base`；删除与指针清空断言见 `f61f131a` |
| 6.3 文档一致性 | 已完成 | `dccf90d9` | 中英文档与代码默认值对齐 |
| 6.4 指标 | 已完成 | `1f332c4c` | 本地汇总 + 完成事件补类型/停止原因 |

快速核对：`git log --oneline -7`。

## 2. 阶段 6 验收现状

| 验收项 | 结论 | 现有证据 | 缺口 |
|---|---|---|---|
| 批量 fan-out 不串行 | **已验收** | `parallel_foreground_spawns_overlap_and_finish_independently` 断言两个前台 spawn 都进入 `started`，`finish_one("a")` 后 b 仍在运行，两者最终都 `success`；`teardown_session_children_spares_other_sessions` 仍通过；`TaskTool` 已声明并发上限 | 无 |
| 空 worktree 不残留 | **已验收** | workspace 层 `unchanged_worktree_is_removed_and_unregistered`：干净 worktree 判定为无改动，删除后目录消失且从 `git worktree list` 注销；shell 层 `update_subagent_meta_worktree_removed_clears_the_pointer`：meta `worktree_path` 清空 | 未跑 `run_shell_child` + `MockInferenceServer` 全链路（见任务 F），边界已写入 `f61f131a` 提交说明 |
| 文档与代码默认值一致 | **已验收** | 抽查 `background=true`、`max_depth=2`、`max_total_agents=16`、`max_concurrent=32`、`worktree_base=head`，中英文档与代码一致 | 无 |
| 6.4 指标 | **已完成** | 7 个 metrics 单测；`cargo check` tools/shell 通过 | 无 |

结论：**任务 A、B 已完成，阶段 6 验收缺口已补齐**。剩余任务 F（端到端验证）与 G（既有失败基线）见 §3。

## 3. 剩余任务（按执行顺序）

### 任务 A（P0）：补 6.1 双前台 spawn 并行验收测试

**状态**：已完成（`a9d97135`）

**目标**：证明同一 assistant step 的两个前台 spawn 在协调器内重叠运行；先完成者不阻塞另一个。

**改动点**：`crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_tests.rs`

**做法**：

- 用 `harness(false, Duration::from_secs(60))`。
- 同时 `tokio::spawn` 两个 `backend.spawn(request("a", false), None)` 与 `request("b", false)`。
- 不发送 `harness.finish`（广播会同时完成两者、掩盖串行问题），先连续 `harness.started.recv()` 两次，断言两个 id 都出现。
- 用 `harness.finish_one.send("a")` 只完成 a，断言 a 的 spawn 返回且 b 的句柄仍未结束。
- 再完成 b，断言两个结果都是 `success`。

**现状核对**：

```bash
cd GrokZen
git grep -n "finish_one\|two_foreground" crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_tests.rs
```

**验收**：

```bash
cargo test -p xai-grok-tools --lib task::coordinator
cargo test -p xai-grok-tools --lib task::
```

**提交**：`test(subagent): 双前台 spawn 并行验收（阶段 6.1）`

---

### 任务 B（P0）：补 6.2 空 worktree 删除断言

**状态**：已完成（`f61f131a`）

**目标**：断言一个无改动的子代理 worktree 在完成收尾时确实被删除，且持久化指针被清空。

**改动点**：

- 首选：`crates/codegen/xai-grok-workspace/src/worktree/mod.rs` 的测试模块，新增“干净 worktree 可删除且目录消失”。
- 可选补强：`crates/codegen/xai-grok-shell/src/agent/subagent/tests/` 下用现有 `RunShellChildHarnessConfig` 跑一次 `run_shell_child`，断言 `result.worktree_path == None` 且 meta 中 `worktree_path` 为空。该路径需要 `MockInferenceServer`，成本较高；若 harness 成本过高，至少要覆盖 workspace 层组合路径并在提交说明里写清楚边界。

**做法（workspace 层）**：

1. 用已有辅助 `xai_test_utils::git::seed_repo`（需要远端时用 `seed_repo_with_remote`）与本地 `run_git_in` 建临时仓库。
2. `WorktreeBuilder` 创建 worktree。
3. 记录 `subagent_worktree_status` 基线，断言 `subagent_worktree_has_changes(...)` 为 `false`。
4. 调 `remove_subagent_worktree(&wt)`，断言 `!wt.exists()` 且 `git worktree list` 不再包含该路径。

**现状核对**：

```bash
cd GrokZen
git grep -n "workspace_helper_reports_clean_dirty_and_committed_worktrees\|remove_subagent_worktree" crates/codegen/xai-grok-workspace/src/worktree/mod.rs
```

**验收**：

```bash
cargo test -p xai-grok-workspace --lib worktree
```

**提交**：`test(subagent): 空 worktree 完成时删除并清指针（阶段 6.2）`

---

### 任务 C（P1）：补 §5 单测缺口 — `context: fork` 映射

**状态**：已完成（`d376fca1`）

**目标**：覆盖 `TaskToolInput.context` 到 `SubagentRequest` 的映射与只读默认。

**改动点**：`crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs` 的测试模块。

**用例**：

- `context = Some(Fork)` + `subagent_type = "explore"` → `request.fork_context == true`，`capability_mode == Some(ReadOnly)`。
- `context = Some(Fork)` + `subagent_type = "implementer"` → `fork_context == true`，`capability_mode == None`。
- `context = Some(Fork)` + `resume_from = Some(id)` → `fork_context == false`（resume 优先）。
- `context = None` → `fork_context == false`（已有 `model_task_spawn_omitting_context_stays_fresh`，不要重复）。

**现状核对**：

```bash
cd GrokZen
git grep -n "fork_context" crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs
```

**验收**：

```bash
cargo test -p xai-grok-tools --lib task::
```

**提交**：`test(subagent): context=fork 映射与只读默认（§5 单测）`

---

### 任务 D（P1）：补 §5 单测缺口 — `max_turns` partial 可续跑

**状态**：已完成（`73e4cc18`）

**实现发现（未决）**：coordinator 的 RootSession ChildId wake 路径（`resolve_owned_child_id` → `resolve_inactive_child_id` → `wake_completed_child`，即生产 `send_active_message` 对 ChildId 目标实际走的路径）不检查 `wake_eligible`，`stop_reason=CANCELLED` 的完成记录也会被唤醒；`wake_eligible` 只在 GrantedChild 完成目标（agent_targets.rs:94）与 Agent 型目标（agent_targets.rs:212）路径及 `resumable` 列生效。`coordinator.rs:1335` 的注释「Any other cancel path still refuses wakes」与该路径的实际行为不符。对照用例因此直接断言 `CompletedChild.wake_eligible` 字段。是否收紧 RootSession 路径属行为变更，留待后续决定。

**目标**：断言 `stop_reason = max_turns`、`partial = true` 的子代理仍可续跑（`send_subagent_message` / wake 或 `resume_from`），不被当成普通取消。

**改动点**：优先 `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/wake_tests.rs` 或 `coordinator_tests.rs`；若需要真实运行态，用 `crates/codegen/xai-grok-shell/src/agent/subagent/tests/` 的 harness。

**做法**：

- 完成一个子代理，`SubagentResult` 带 `stop_reason = MAX_TURNS`、`cancelled = true`、`partial = true`。
- 断言 `CompletedChild.wake_eligible`（通过后续 wake/send 被接纳来观察），或 `resume_from` 能创建续跑。
- 对照用例：`stop_reason = cancelled` 的子代理不被唤醒。
- 现有 `prompt_turn_result_tests::every_completion_kind_maps_to_the_terminal_contract` 只覆盖字段映射，不覆盖续跑，不要误判为已覆盖。

**现状核对**：

```bash
cd GrokZen
git grep -n "MAX_TURNS\|max_turns" crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator crates/codegen/xai-grok-shell/src/agent/subagent/tests
```

**验收**：

```bash
cargo test -p xai-grok-tools --lib task::
cargo test -p xai-grok-shell --lib agent::subagent -- --skip wake::
```

**提交**：`test(subagent): max_turns partial 可续跑（§5 单测）`

---

### 任务 E（P1）：补 §5 集成流程测试 — spawn → list → send → interrupt → resume

**状态**：已完成（`d6a53015`）

**目标**：用 mock child runner 串起控制面一条完整流程，防止各工具单独测试通过、组合后行为漂移。

**改动点**：`crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_tests.rs`（或新建独立测试文件，沿用 `Harness`）。

**步骤**：

1. spawn 一个子代理，`ListAgents` 看到 `running`。
2. `send_subagent_message` 的队列投递被接纳（现有 `admitted_messages` 通道可观察）。
3. interrupt/cancel 后，`ListAgents` 状态为 `cancelled`。
4. 再 spawn 一个正常完成，`resume_from` 复用其会话，`ListAgents` 出现新的 `running` 记录。
5. 过程中 `include_finished` / `parent_id` / `limit` 至少各断言一次。

**实现发现**：`admitted_messages` 通道只对 wake 运行接通（`TestControl.admitted_messages` 在 `wake_agent_id.is_some()` 时才为 `Some`），普通 spawn 的运行中投递无法经该通道观察。测试因此分两段覆盖：运行中投递经 admission gate 的 `admission_entered` 观察；末尾 wake 续跑后的再次投递走 `admitted_messages` 通道，断言 `(Steer, text)` 被记录且 outcome 为 `Accepted`。

**现状核对**：先查已有分散测试，避免重复：

```bash
cd GrokZen
git grep -nE "ListAgents|ListRunning|active_message|resume_from" crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator_tests.rs
```

**验收**：

```bash
cargo test -p xai-grok-tools --lib task::
```

**提交**：`test(subagent): 控制面 spawn→list→send→interrupt→resume 流程（§5 集成）`

---

### 任务 F（P2）：阶段 6 端到端验证（手工，非 CI）

**状态**：待办

**目标**：用真实会话和模型跑一轮“explore 调查 → implementer 实现 → workflow 汇总结构化结果”，验证 `list_agents` 输出与 TUI/内部 snapshot 一致。

**做法**：

1. 准备一个可改动的小仓库；发起 `explore`（只调查）、`implementer`（实现并跑测试）、`workflow`（消费 `output_schema` 的结构化结果）。
2. 观察 `list_agents` 的 `id/label/type/status/model/depth/elapsed_ms/tokens_in/tokens_out/context_pct/current_tool/queue_position/resumable`，与 Ctrl+G / dashboard 的显示对照。
3. 验证 6.4 指标：正常关闭会话后，本地日志出现一条 `subagent metrics summary`；运行期间每个子代理的 `subagent_completed` 遥测带 `subagent_type` / `stop_reason` / `partial`。
4. 结果写入本文件 §5（验证记录），不要只留在对话里。

**注意**：这一项依赖本机模型链路与真实会话，测试套件无法替代。

---

### 任务 G（P1）：处理既有测试失败基线

**状态**：待办

**目标**：把与阶段 6 无关的既有失败查清，避免后续测试计划被噪声淹没。

**已知失败（2026-11-02 本机）**：

- `xai-grok-workspace`：
  - `trust::tests::workspace_key_collapses_standalone_grok_worktree_onto_source_repo`
  - `trust::tests::workspace_key_collapses_worktree_onto_source_repo_git_root`
  - `worktree::create_root::tests::create_uses_the_path_captured_before_cwd_changes`
  - `worktree::tests::sync_fork_response_includes_strategy_report`（断言 `Grove→copy must keep a fallback reason`，疑似 Windows 上 Grove 不可用）
- `xai-grok-tools`：
  - `types::skill_discovery_tracker::skill_path_suggestion::tests::rewrites_worktree_paths_to_display_cwd_but_preserves_external_paths`
- `xai-grok-shell`：`agent::subagent` 下 `wake::` 三例在 debug 构建有既有栈溢出，使用 `-- --skip wake::`（见 `eaf0fe70` 提交说明）。

**做法**：

1. 在未改动的干净提交上复现（或确认只在本机/Windows 复现），记录命令与完整失败输出。
2. 环境相关（如 Grove 在 Windows 不可用）：在测试中加明确的 skip 条件并注明原因，禁止直接删断言。
3. 真实回归：修复并补回归测试。
4. 把结论写进本文件 §5。

**验收**：上述命令的失败数降到 0，或每一项都有“环境跳过 + 原因”的证据。

---

## 4. 命令速查

```bash
cd GrokZen

# 编译
cargo check -p xai-grok-tools --lib
cargo check -p xai-grok-shell --lib

# 单测
cargo test -p xai-grok-tools --lib task::
cargo test -p xai-grok-shell --lib agent::subagent -- --skip wake::
cargo test -p xai-grok-shell --lib config::tests::subagent
cargo test -p xai-grok-workspace --lib worktree
cargo test -p xai-tool-types
cargo test -p xai-grok-telemetry

# 格式（只检查改动文件；仓库存在与本机 rustfmt 版本不一致的既有 diff）
rustfmt --edition 2024 --check <changed-file>
```

注意：`astrolabe` 在把整个工作区当索引根时会报 “more than 20000 files”，检索请用 `git grep` 或 `read_file`/`list_dir`，不要因此停住。

## 5. 验证记录（每完成一个任务追加一行）

| 日期 | 任务 | 结果 | 命令 / 证据 |
|---|---|---|---|
| 2026-11-02 | 6.4 指标 | 通过 | `cargo test -p xai-grok-tools --lib metrics::`（7 passed）；`cargo check -p xai-grok-shell --lib` |
| 2026-11-02 | 6.2 worktree helper | 通过 | `cargo test -p xai-grok-workspace --lib workspace_helper_reports_clean_dirty_and_committed_worktrees`（1 passed） |
| 2026-11-02 | 6.2 worktree_base 配置 | 通过 | `cargo test -p xai-grok-shell --lib subagent_worktree_base`（3 passed） |
| 2026-11-02 | 6.1 双前台 spawn 并行 | 通过 | `cargo test -p xai-grok-tools --lib task::coordinator`（166 passed）；`cargo test -p xai-grok-tools --lib task::`（365 passed）；提交 `a9d97135` |
| 2026-11-02 | 6.2 空 worktree 删除与清指针 | 通过 | `cargo test -p xai-grok-workspace --lib worktree`（112 passed，4 个任务 G 已知基线失败）；`cargo test -p xai-grok-shell --lib agent::subagent -- --skip wake::`（289 passed）；提交 `f61f131a` |
| 2026-11-02 | 任务 C context=fork 映射 | 通过 | `cargo test -p xai-grok-tools --lib task::`（368 passed，0 failed，新增 3 例）；提交 `d376fca1` |
| 2026-11-02 | 任务 D max_turns partial 可续跑 | 通过 | `cargo test -p xai-grok-tools --lib task::`（370 passed，0 failed，新增 2 例）；`cargo test -p xai-grok-shell --lib agent::subagent -- --skip wake::`（289 passed）；提交 `73e4cc18` |
| 2026-11-02 | 任务 E 控制面 spawn→list→send→interrupt→resume 流程 | 通过 | `cargo test -p xai-grok-tools --lib task::`（371 passed，0 failed，新增 1 例 `control_plane_flow_spawn_list_send_interrupt_then_resume`）；提交 `d6a53015` |

## 6. 提交约定

- 一个任务一个提交，消息前缀 `feat(subagent):` / `test(subagent):` / `fix(subagent):` / `docs(subagent):`，并在正文注明“阶段 6.x”或“§5 单测/集成”。
- 提交前跑本任务的验收命令；提交说明写明通过数量与已知失败。
- 不把 `metrics`、`worktree`、`fork` 等无关改动混进同一提交。
- 完成后更新本文档 §1 / §2 / §3 的状态与 §5 验证记录，让下一个对话能直接接上。

## 7. 对话交接模板

把下面这段直接粘到新对话的开头，按需替换任务字母：

```text
仓库：C:\Users\bypassnro\Documents\project\grok\GrokZen
分支：fix/pager-test-clipboard-isolation
请先读：
1. docs/SUBAGENT-COMPLETION-PLAN.md
2. docs/SUBAGENT-IMPROVEMENT-PLAN.md 的第 5、6 节
本次只做任务 <A|B|C|D|E|F|G>。先跑该任务的“现状核对”，确认缺口仍在；
按“做法”实现，跑“验收”命令，提交并在 COMPLETION-PLAN 里回填状态与提交哈希。
不要重做已完成条目，不要顺手格式化无关文件。
```
