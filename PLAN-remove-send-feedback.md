# 计划：删除 send_feedback 工具与 /feedback 功能

> 工作树：`C:\Users\bypassnro\Documents\project\GrokZen-wt-feedback`
> 分支：`remove-send-feedback`（基于 `origin/zh-dev`，起点 dee45a61）
> 用途：跨多个会话分步完成。每个会话开始前先读本文档，完成后更新「进度」一节。
> 提示：后续会话删代码时建议优先用内容锚点（search_replace）而非行号切片。

## 目标

从 GrokZen 代码库中移除两样东西，并让 `cargo check/test` 全绿：

1. **`send_feedback` 模型工具**（`xai-grok-tools` 内置工具，parent grok-build 会话专属）
2. **`/feedback` 用户命令与反馈弹窗**（`xai-grok-pager` 的 slash 命令 + `feedback_modal` 视图 + 草稿管线 + trace 上传卡片）

## 删除边界（重要，防误删）

**删**：send_feedback 工具、`/feedback` 命令、反馈弹窗（Drafts/Write/Trace 卡）、反馈草稿存储的 UI 侧（`x.ai/feedback/drafts/*` ACP 方法、`FeedbackDraftSendRequest` 等 wire 类型、predraft 保存）、`x.ai/feedback/upload-trace` 一次性上传授权链。

**保留**（与 `/feedback` 无关的评分/问卷链路）：
- `xai_grok_feedback` 库（feedback_drafts.json 存储实现、taxonomy）——shell 的评分扩展与 pty harness 仍在用；后续会话可评估删除，但当前会话先不删
- `session/feedback.rs` 启发式问卷（FeedbackRequest 通知）、`extensions/feedback.rs` 的 `x.ai/feedback`（评分）与 `x.ai/feedback/dismiss`、`feedback_manager.rs`、`feedback_client.rs`
- `x.ai/btw`、`x.ai/review/*`、`feedback_trace_offer` 相关 meta（pager/shell 仍在消费）
- `ToolKind::Feedback` 枚举变体（开放分类学；避免序列化/schema 连锁变更）——只是不再有任何工具使用它

## 分步计划（按依赖顺序）

### 步骤 1：xai-grok-tools ✅（已完成）
- [x] 删 `implementations/grok_build/send_feedback.rs` + `send_feedback_tests.rs`
- [x] `implementations/grok_build/mod.rs`：去 mod/pub use
- [x] `types/tool_io.rs`：删 `ToolInput::SendFeedback` 变体 + import
- [x] `registry/types.rs`：去 `register::<SendFeedbackTool>()` 与 drafts_file_path 注入
- [x] `types/template_renderer.rs`：删 `feedback_drafts_path` 字段与 `with_feedback_drafts_path`
- [x] `tool_taxonomy.rs`：WRITING_TOOL_WIRE_NAMES 去掉 send_feedback
- [x] `normalization.rs`、`permission_access.rs`：删变体匹配与 dynamic send_feedback 分支（含测试）
- [x] `implementations/grok_build/task/types.rs`、`media_gen_limits.rs`：`ToolKind::Feedback` 从 match 中移除（前者），media_gen_limits 保留（后者仍有 ToolKind 穷尽 match——注意：task/types.rs 的 `allowed_tool_kinds` 里 Feedback 已删；media_gen_limits 的 `max_calls_per_batch` 保留 Feedback 分支）
- [x] `cargo check -p xai-grok-tools` ✅

### 步骤 2：xai-grok-agent ✅（已完成）
- [x] `config.rs`：`grok_build_core_toolset_with(include_workflow)` 合并参数；`default_grok_build_toolset`/`default_agent_toolset`/`general_purpose_toolset` 适配
- [x] `builder.rs`：删 parent-only 注入块（`is_parent_grok_build` 改 `let _ =`）、subagent strip 块简化（只留 AskUser）
- [x] 测试：删 `send_feedback_exposure_is_grok_build_only`、`defaulted_agent_definitions_omit_send_feedback`；`pager_flag_combinations` 删 send_feedback 断言；`subagent_audience_never_receives` 删 kindless 两个用例与 send_feedback 断言
- [x] `cargo check -p xai-grok-agent` ✅

### 步骤 3：xai-grok-shell ✅（已完成）
- [x] 删 `extensions/feedback_drafts.rs`、`extensions/feedback_trace.rs`、`extensions/feedback_tests.rs`
- [x] `extensions/mod.rs`：去三个 mod 声明（保留 `pub mod feedback`）
- [x] `extensions/feedback.rs`：handle 去掉 drafts/upload-trace 分支；handle_feedback 只留直接提交（`success: true`、`trace_upload_token: None`）；删 `parse_feedback`/`parse_draft_send_request`/`draft_send_input`/`delete_sent_draft`/`is_feedback_outcome_unknown`/`request_may_have_arrived`；删 `#[cfg(test)] mod tests`
- [x] `agent/mvp_agent/mod.rs`：删 `feedback_trace_upload_grants` 字段
- [x] `agent/mvp_agent/agent_ops.rs`：删构造处初始化 + `issue/consume_feedback_trace_upload_grant`（保留 `feedback_trace_offer`、`team_blocks_one_shot_trace_upload`、`one_shot_feedback_gcs_config`——后者已无调用方，可在后续会话删）
- [x] `agent/mvp_agent/tests.rs`：删 trace 门测试群（原 2486-3104 行）与 `make_trace_card_eligible`
- [x] `session/acp_types.rs`：删 `FeedbackDraftUpdateRequest`/`FeedbackDraftSendRequest`/`FeedbackDraftEditedBody`/`FeedbackTraceUploadIntent`
- [x] `session/acp_session_impl/tool_calls.rs`：`access_kind_for_resolved_tool` 还原为纯 `AccessKind::from`；删 SendFeedback/Dynamic(SEND_FEEDBACK_TOOL_NAME) 的 label 分支
- [x] `agent/mvp_agent/acp_agent.rs`：ext_method 分发收敛为 `"x.ai/feedback" | "x.ai/feedback/dismiss" | "x.ai/btw"`；`"feedbackTraceOffer"` meta 保留
- [x] `cargo check -p xai-grok-shell` ✅

### 步骤 4：xai-grok-pager（lib 目标 ✅ 已编译通过；test 目标进行中）
- [x] 删 `slash/commands/feedback.rs`、`views/feedback_modal/`（整个目录）、`app/dispatch/inline_feedback.rs`
- [x] `slash/commands/mod.rs`：去 `pub mod feedback` 与 `Arc::new(feedback::FeedbackCommand)` 注册
- [x] `views/mod.rs`、`app/dispatch/mod.rs`：去模块声明
- [x] `app/actions.rs`：删 `Action::{OpenFeedbackModal, SubmitFeedbackModal, RequestFeedbackDraft, SendFeedback}`、`Effect::{SendFeedback, FeedbackDraftRequest, UploadFeedbackTrace, RehydrateFeedbackImage}`、`TaskResult::{FeedbackComplete, FeedbackFailed, FeedbackDraft*Complete, FeedbackTraceUploaded, FeedbackImageRehydrated}`、类型 `FeedbackSendOrigin`/`DraftFeedbackBody`/`FeedbackTraceChoice`、`ClipboardPasteTarget::FeedbackModal`、`ClipboardPasteOwner::FeedbackPane`
- [x] `app/dispatch/router.rs`：删对应 action 分支与 notes 导入
- [x] `app/dispatch/notes.rs`：删 `dispatch_open_feedback_modal`/`dispatch_submit_feedback_modal`/`dispatch_send_feedback`/`commit_feedback`/`feedback_send_effect`/`log_trace_consent_selected`/`keep_unsent_feedback_report`/`UnsentFeedbackReport`/`FEEDBACK_THANKS_NOTICE`/`FEEDBACK_TRACE_UPLOAD_TIMEOUT_MS`/`sharing_lock_allows_trace_offer`/`feedback_notice`/`refuse_disabled_feedback`/`encode_feedback_image_slice`；**保留** remember/btw/recap 函数群
- [x] `app/dispatch/task_result.rs`：删 6 个 Feedback* 分支、`displaced_draft_feedback_notice`、`drain_clipboard_target` 的 hold_feedback_submit 参数与 FeedbackModal 分支、`show_clipboard_toast` 的 FeedbackModal 分支
- [x] `app/dispatch/mod.rs`：去 `FEEDBACK_TRACE_UPLOAD_TIMEOUT_MS` re-export
- [x] `app/dispatch/prompt.rs`：删 SendFeedback 拒绝分支、OpenFeedbackModal 接管分支、carries_images/sends_typed_text 中的 SendFeedback
- [x] `app/effects/mod.rs`：删 4 个 Effect 分支 + `UploadTraceRequest` 结构 + FeedbackModal images_dir 分支
- [x] `app/agent_view/mod.rs`：删 `feedback_modal`/`pending_feedback_trace_uploads`/`parked_feedback_trace_consents` 字段与 import
- [x] `app/agent_view/modals.rs`：删 feedback 弹窗处理函数群（17-275 行）+ `install_local_question` 去掉 displace
- [x] `app/agent_view/input.rs`：删 5 处 feedback_modal 输入分支
- [x] `app/agent_view/render.rs`：删 2 处
- [x] `app/agent_view/paste.rs`：删 `enqueue_feedback_modal_attachment_probe`/`complete_feedback_modal_attachment_paste`/`FeedbackPane` 测试；**保留** take_deferred_send_after_paste/resume_deferred_send（曾误删，已恢复）
- [x] `app/agent_view/session.rs`：删 3 字段初始化
- [x] `app/event_loop.rs`：删 `active_feedback_modal_open` 与 2 处使用 + 测试
- [x] `app/turn_completion.rs`：删 displace_feedback_modal
- [x] `app/acp_handler/interactions.rs`、`permissions.rs`、`agent_view/elicitation.rs`、`dispatch/turn.rs`：删 displace 调用
- [x] `app/modals.rs`、`views/modal.rs`：删 PaletteCommand::OpenFeedbackModal + "Send Feedback" 入口
- [x] `acp/tracker.rs`：删 send_feedback 工具渲染分支与 ToolKind::Feedback spinner 标签
- [x] `minimal/api.rs`：删 feedback_modal 访问器/setter/判断
- [x] `cargo check -p xai-grok-pager`（lib）✅

### 步骤 5：pager 测试目标（下个会话从这里继续）
当前 `cargo check -p xai-grok-pager --all-targets` 余 188 错，按文件分布（2026-11-14 实测）：

| 文件 | 错误数 | 说明 |
|---|---|---|
| `src/app/dispatch/tests/notes.rs` | 168 | 绝大部分是 feedback 用例；逐个删 feedback 测试，保留 remember/btw/recap 用例（`dispatch_send_remember_note*`、`dispatch_send_btw`、recap 相关） |
| `src/app/turn_completion/tests.rs` | 4 | trace offer / feedback modal 相关用例 |
| `src/app/dispatch/tests/task_result.rs` | 4 | Feedback* TaskResult 用例 |
| `src/app/dispatch/tests/billing.rs` | 4 | `feedback_modal` 构造用例 |
| `src/app/dispatch/queue.rs`（文件内 cfg(test)） | 4 | `edited_queued_bare_feedback_opens_the_modal` 等 /feedback 队列用例 |
| `src/app/effects/tests.rs` | 3 | 引用已删的 `UploadTraceRequest` 等 |
| `src/app/dispatch/tests/router.rs` | 1 | `send_feedback_clears_active_ephemeral_tip` |

- [x] 逐文件清理上表错误至 `--all-targets` 清零（2026-11-14 会话 2）
- [x] `crates/codegen/xai-grok-pager/src/app/app_view.rs`：`feedback_trace_choice_latched` 字段、`feedback_trace_offer()` 方法、`shell_feedback_trace_offer` 字段——已删（pager 侧已无任何消费方；`apply_auth_meta` 中的赋值与构造初始化一并删）
- [x] `src/acp/mod.rs`：`parse_feedback_trace_offer`、`InitializedAgent.feedback_trace_offer`、`AcpConnection.feedback_trace_offer`——已连带删（shell 侧 `feedbackTraceOffer` meta 广播暂留，属遗留项 1，后续会话两端一起删）
- [x] `src/app/app_view_tests.rs`、`src/app/dispatch/tests/mod.rs`（test_app 里 `feedback_trace_choice_latched` 字段初始化）——已删
- [x] `event_loop.rs` 的 `app.shell_feedback_trace_offer = connection.feedback_trace_offer` 赋值——已删

### 步骤 6：pty harness 与杂项 ✅（已完成，2026-11-14 会话 3）
- [x] `crates/codegen/xai-grok-pager-pty-harness/tests/pty_e2e/`：删 `feedback_modal_pty.rs`、`feedback_draft_send_pty.rs`、minimal 下 4 个 feedback 测试（`minimal_feedback_modal_opens_and_inline_sends.rs`、`minimal_feedback_small_terminal_shows_hint.rs`、`minimal_feedback_draft_send.rs`、`minimal_feedback_write_send_and_dismiss.rs`，计划原列 2 个实际有 4 个）；`pty_e2e_config_ui.rs` 去 2 个 mod、`minimal/mod.rs` 去 4 个 mod；`plan_overlay_slash_runs_command.rs` 的 `/feedback <text>` 改为 `/rename <title>`（本地可观测：ack 文案 + border title + 不进模型断言；拒绝计数由 POST 计数改为 inference 计数）；`common.rs` 删 6 个 feedback 辅助（`read_feedback_drafts`/`enable_feedback_posting`/`FEEDBACK_MODAL_PLACEHOLDER_SENTINEL`/`open_feedback_modal`/`wait_for_feedback_post`/`drive_draft_send`）与注释残留；`minimal_parked_plan_commits_to_scrollback.rs` 注释措辞更新
- [x] `xai-grok-pager-minimal`（计划遗漏，实际必要）：删 `src/feedback.rs`（引用步骤 4 已删的 `views::feedback_modal`）；`lib.rs` 去 mod+doc、`guard.rs` 去 include_str；`live.rs` 删 `feedback_modal_mut` 渲染分支；`overlay.rs` 的 `is_live_region_modal_active` 去掉 feedback 分支、测试 `is_live_region_modal_active_ignores_prompt_modals` 重写、注释清理
- [x] `crates/codegen/xai-grok-workspace/src/permission/hub_gate_tests.rs`：daemon_tool_table 去 "send_feedback" 用例
- [x] `crates/codegen/xai-grok-locale/locales/zh-CN-metadata.json`：删 121 个 /feedback 相关键（`feedback.modal.*` 全族、`feedback.trace.*`、`feedback.images.*`、`palette.send_feedback`、`slash.command.feedback.*`、`turn.writing.feedback`、`minimal.feedback.too_small`、`prompt.placeholder/info.feedback`、`welcome.hero.subtitle`、`privacy.feedback_disabled`、`feedback.send_failed/thanks/text_required/question_label/no_active_session/blocked.*/placeholder.detailed/outcome_unknown/cancelled_interrupted/sent_without_trace`）；**保留** `permission.reject_feedback`（权限按钮）、`feedback.survey`（评分 telemetry span 名，非 locale 键）；`slash.command.compact.shell_arg_placeholder` 与 `slash.command.feedback.shell_arg_placeholder` 均无代码引用，一并删
- [x] `xai-grok-pager/src/app/dispatch/notes.rs`：删死 import `xai_grok_feedback::{FeedbackDraftStore, FeedbackSource, FeedbackTaxonomy, derive_title, structured_feedback}`；`xai-grok-pager/Cargo.toml` 去 `xai-grok-feedback` 依赖（pager 已无使用点）
- [x] `send_feedback_notification`（`acp_session.rs:1410`，评分问卷）**保留**；shell 的 `/feedback` builtin（`slash_commands.rs` BuiltinCommand → `execute_feedback_command` → `feedback_manager`）属评分链路保留；`xai_grok_feedback` 库仍被 shell（feedback_manager/persistence）使用，根 Cargo.toml 成员与 shell 依赖保留
- [x] 修复遗留损坏：`agent/mvp_agent/tests.rs` 全文件行首带行号前缀（步骤 5 会话重建文件的笔误，HEAD 即损坏；基线 66d9b9aa 完好）——按基线内容剥离前缀重建（8913 行，与步骤 5 已删 trace 测试群一致），`cargo check -p xai-grok-shell --all-targets` 通过

### 步骤 7：收尾验证 ✅（2026-11-14 会话 4）

- [x] `cargo check --workspace --all-targets` 全绿（0 error；余 2 个基线已有 warning：`builder.rs` 的 `ToolConfig` 未用 import、`builtin_names` 未用变量）
- [x] `cargo test -p xai-grok-pager -p xai-grok-agent -p xai-grok-tools`：pager 10404 个 lib 单测 + 全部集成测试通过，agent 全通过；tools 仅剩 2 个 Windows 平台预存在失败（`cgroup_memory_test` 依赖 Linux cgroupv2；`path_suggestions_production::pattern3_*` 断言 Unix 路径分隔符）
- [x] `cargo test -p xai-grok-shell`：**运行并记录**，31 个 lib 测试 + 15 个集成目标失败；失败点全部位于本分支未改动的模块，根因是该社区版 `Distribution::current()` 关闭 `AccountLogin`（登录存储、账号目录、trace token 刷新类测试必然失败）与 Windows 平台差异，见下方清单
- [x] `cargo fmt`：`cargo fmt --all` 在本机报 `os error 206`（路径过长），改为对改动包逐一 `cargo fmt -p ...`，全部通过
- [x] 残留清点（`send_feedback` / `/feedback`）：代码侧仅剩评分链路与 shell `/feedback` builtin；详见下方「残留清单」
- [x] 步骤 7 顺带清理的测试残留：
  - `acp/tracker_tests.rs`：删 `send_feedback_update_renders_feedback_drafted`、`zh_localization_feedback_writing_label_keeps_ordinal_and_tool_identity`
  - `app/queue_edit.rs`：删 `edit_local_feedback_carries_the_submitted_images`
  - `app/dispatch/tests/notes.rs`：删 `minimal_voice_refusal_is_a_scrollback_block`（voice 拒绝只在已删的 `/feedback` 的 `submission_refusal` 上触发）
  - `app/agent_view/plan.rs`：4 个用例的 `/feedback <text>` 改为 `/btw <text>`（`/feedback` 已不再是 pager builtin）
  - `views/modal.rs`：删死映射 `"Send Feedback" => "palette.send_feedback"`；`agent/builder.rs` 白名单去 `send_feedback`
  - `views/welcome/hero_box.rs`：副标题去掉 `/feedback` 推销语，并在 `zh-CN-metadata.json` 恢复 `welcome.hero.subtitle`（步骤 6 删键后中文界面会回退英文）
  - `xai-grok-tools/permission_access.rs`：删重复的 `#[test]` 属性
- [x] 环境操作：C 盘剩余 24 GB 时测试编译因资源不足失败，清理 `GrokZen` 与 `GrokZen-wt-feedback` 的 `target/debug/incremental`（约 126 GB，可重建缓存）后重跑通过

#### 残留清单

- `send_feedback`：只剩 shell 的 `send_feedback_notification`（评分问卷，按计划保留）。
- `/feedback`：
  - **保留（评分链路）**：shell `slash_commands.rs` builtin → `slash_exec.rs` → `feedback_manager.rs` / `feedback_client.rs`；`extensions/feedback.rs` 的 `x.ai/feedback*`；`acp_agent.rs` ext 分发。
  - **遗留死链（可后续会话处理）**：`feedbackTraceOffer` meta（shell `agent_ops.rs` / login `meta.rs`）、`Feature::FeedbackTraceCard` 描述（config-types `registry.rs`、config `remote_settings.rs`）、pager `dispatch/prompt_ack.rs` 的 "run /feedback" 提示、pager `views/welcome` 之外的 `/feedback` 注释（`agent_view/cta.rs`、`agent_view/mod.rs`）。
  - **文档**：`xai-grok-pager/docs/user-guide/` 与 `docs/tutorial/` 仍完整描述已删除的反馈表单（04-slash-commands、03-keyboard-shortcuts、19-plan-mode、21-terminal-support、26-config-reference 及 zh-CN 对应页）；历史 changelog 按约定保留。
  - **测试支撑**：pty harness 与 test-support 的 mock `POST /v1/feedback` 端点（评分链路仍在用）。
  - **telemetry**：`xai-grok-telemetry/events/feedback.rs`、`events/consent.rs` 中反馈弹窗/trace 卡事件定义已无发射方。

### 步骤 8：提交 ✅（已完成）
- [x] 工作树内 `git add -A && git commit`（信息建议：`feat: 删除 send_feedback 工具与 /feedback 反馈弹窗`）——分支共 4 个提交（55259565、e30981b8、e9f9ca9d、76f2e70a）

## 进度日志

- 2026-11-14（会话 5）：分支已合并入 `zh-dev`（主工作区直接 `git merge remove-send-feedback`，ort 策略自动合并，无冲突）。合并后 `cargo check` 全 workspace 通过。分支使命完成，worktree `GrokZen-wt-feedback` 可择期移除。
- 2026-11-14（会话 4）：完成步骤 7。`cargo check --workspace --all-targets` 全绿；pager/agent/tools 测试除 2 个 Windows 平台预存在失败外全通过（pager lib 10404 通过）。shell 套件按计划运行：31 个 lib 测试 + 15 个集成目标失败，全部落在未改动模块，根因是社区版关闭 `AccountLogin` 与 Windows 平台差异（无登录存储、无内置模型目录、cgroup/路径断言），已逐类记录在步骤 7。顺带清掉 5 个遗漏的 feedback 测试，并把 plan.rs 里 4 个用例的 `/feedback` 换成 `/btw`；修复 hero 副标题与中文 locale 回退。`cargo fmt --all` 在本机报路径过长，改为按包 fmt。测试编译一度因 C 盘仅剩 24 GB 失败（页面文件/mmap 报错），清理两处 `target/debug/incremental`（约 126 GB）后恢复。
- 2026-11-14（会话 1）：完成步骤 1-3（tools/agent/shell 全绿）；步骤 4 lib 编译通过；步骤 5 已完成错误盘点（上表）。计划文档落盘 `PLAN-remove-send-feedback.md`。中途踩过的坑：`paste.rs` 曾误删 `take_deferred_send_after_paste`/`resume_deferred_send`/`try_handle_wrap_host_image_paste`/`route_popup_paste`（已从 git HEAD 恢复）；`task_result.rs` 曾误删 `if let Some(mut old_qv)` 行导致括号错配（已修）；`notes.rs` 曾误删 remember 函数群（已从原文件恢复）。
- 2026-11-14（会话 3）：完成步骤 6。pty harness 删 6 个 feedback e2e 文件（顶层 2 + minimal 4）+ mod 注册，`plan_overlay_slash_runs_command.rs` 改用 `/rename` 驱动，`common.rs` 删 6 个 feedback 辅助；连带发现并清理 `xai-grok-pager-minimal` 的 feedback 渲染层（`src/feedback.rs` 等 5 处，步骤 4 删 `views::feedback_modal` 后该 crate 必然编译不过）；workspace hub_gate 测试表、locale 121 键、pager 死 import 与 Cargo 依赖一并处理。步骤 7 的 `cargo check --workspace --all-targets` 已提前全绿（其中修掉 `mvp_agent/tests.rs` 全文件行号前缀损坏）。
- 2026-11-14（会话 2）：完成步骤 5。`dispatch/tests/notes.rs` 从 2564 行删到 694 行：43 个 feedback/trace/draft 测试按 `#[test]` 起始行 + 括号匹配整函数删除，同时删掉只被它们引用的辅助（`open_feedback_modal`、`confirm_trace_choice`、`expect_single_modal_post`、`park_send_this_session`、`Arrange` 类型与孤儿 doc 注释）；`test_pasted_png`/`test_pasted_image`/`composer_image`/`agent_ref`/`send_minimal_btw`/`esc` 被 remember/btw/recap 用例引用，保留。其余文件按表清理：turn_completion/tests.rs（`hook_denied_finalize_displaces_feedback_*`）、task_result.rs（`doctor_planning_displaces_feedback_*`）、billing.rs（`free_usage_upsell_displaces_feedback_*`）、queue.rs（`edited_queued_bare_feedback_opens_the_modal` + `run_edited_feedback_uses_row_text_*`）、effects/tests.rs（`upload_trace_request_*` 2 个）、router.rs（`send_feedback_clears_active_ephemeral_tip`）、notes.rs 内 `minimal_voice_refusal` 的 `feedback_modal` 断言行。遗留项 4（app_view trace offer）已按上方清单删除，`--all-targets` 清零。
- 后续会话请从「步骤 7」继续，并把每步完成项勾上、更新进度日志。

## 已知遗留（供后续会话决策）

1. `xai-grok-shell` 的 `one_shot_feedback_gcs_config`、`team_blocks_one_shot_trace_upload`、`feedback_trace_offer` 及其测试辅助——trace 卡删除后已成死代码，可整体删（连带 `Feature::FeedbackTraceCard` 配置项与 `feedbackTraceOffer` meta 两端）。
2. `xai_grok_feedback` 库与 `session/persistence_collect_session_files_tests.rs` 的 drafts artifact 测试——若评分链路不需要 drafts 存储，可整体下线。
3. `session/mod.rs` 的 `FeedbackOutcome::SubmittedCleanupFailed`/`OutcomeUnknown` 变体是否仍被引用。
4. ~~pager `app_view.rs` 的 `shell_feedback_trace_offer` 与 `feedback_trace_choice_latched`~~（步骤 5 已删）。
5. locale 的 `feedback.*` 键清理时注意区分评分（保留）与 /feedback（删除）——步骤 6 已完成。
6. pager user-guide / tutorial 文档中的 `/feedback` 表单说明、telemetry 的反馈弹窗事件定义、`prompt_ack.rs` 的 `/feedback` 提示，以及 `agent_view/cta.rs` 等处的过期注释，可后续统一清理。
