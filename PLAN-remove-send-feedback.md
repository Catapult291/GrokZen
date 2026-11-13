# 计划：删除 send_feedback 工具与 /feedback 功能

> 工作树：`C:\Users\bypassnro\Documents\project\GrokZen-wt-feedback`
> 分支：`remove-send-feedback`（基于 `origin/zh-dev`，起点 dee45a61）
> 用途：跨多个会话分步完成。每个会话开始前先读本文档，完成后更新「进度」一节。

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

- [ ] 逐文件清理上表错误至 `--all-targets` 清零
- [ ] `crates/codegen/xai-grok-pager/src/app/app_view.rs`：`feedback_trace_choice_latched` 字段、`feedback_trace_offer()` 方法、`shell_feedback_trace_offer` 字段——trace 卡链路已删，评估删除（检查 acp/mod.rs 仍引用）
- [ ] `src/acp/mod.rs`：`parse_feedback_trace_offer`、`feedback_trace_offer` 字段——保留（shell 仍广播）或连带删（决定后两端一致）
- [ ] `src/app/app_view_tests.rs`、`src/app/dispatch/tests/mod.rs`（test_app 里 `feedback_trace_choice_latched` 字段初始化）

### 步骤 6：pty harness 与杂项
- [ ] `crates/codegen/xai-grok-pager-pty-harness/tests/pty_e2e/`：删 `feedback_modal_pty.rs`、`feedback_draft_send_pty.rs`、`minimal/minimal_feedback_small_terminal_shows_hint.rs`、`minimal/minimal_feedback_draft_send.rs`、`plan_overlay_slash_runs_command.rs` 中的 /feedback 引用（改用其他命令）；`common.rs` 删 `open_feedback_modal`/`drive_draft_send`
- [ ] `crates/codegen/xai-grok-workspace/src/permission/hub_gate_tests.rs`：daemon_tool_table 去掉 "send_feedback"
- [ ] `crates/codegen/xai-grok-locale/locales/zh-CN-metadata.json`：删 `palette.send_feedback`、`turn.writing.feedback` 等 feedback 键（搜 `feedback` 逐个核对，仅删 /feedback 相关）
- [ ] `crates/codegen/xai-grok-shell/src/session/acp_session.rs`、`acp_session_impl/run_loop.rs`、`turn.rs`：`send_feedback_notification`（评分问卷通知）**保留**（属于评分链路）
- [ ] 根 `Cargo.toml`：确认 `xai-grok-feedback` 依赖仍被 shell/pager 使用（保留）；若最终全删则去掉成员

### 步骤 7：收尾验证
- [ ] `cargo check --workspace --all-targets` 全绿
- [ ] `cargo test -p xai-grok-tools -p xai-grok-agent -p xai-grok-shell -p xai-grok-pager` 通过
- [ ] `cargo fmt`（注意：仓库 fmt 风格，提交前统一跑）
- [ ] `rg send_feedback`、`rg '"/feedback"'` 残留清点（stage-log.md 等文档中的历史记录保留）

### 步骤 8：提交
- [ ] 工作树内 `git add -A && git commit`（信息建议：`feat: 删除 send_feedback 工具与 /feedback 反馈弹窗`）

## 进度日志

- 2026-11-14（会话 1）：完成步骤 1-3（tools/agent/shell 全绿）；步骤 4 lib 编译通过；步骤 5 已完成错误盘点（上表）。计划文档落盘 `PLAN-remove-send-feedback.md`。中途踩过的坑：`paste.rs` 曾误删 `take_deferred_send_after_paste`/`resume_deferred_send`/`try_handle_wrap_host_image_paste`/`route_popup_paste`（已从 git HEAD 恢复）；`task_result.rs` 曾误删 `if let Some(mut old_qv)` 行导致括号错配（已修）；`notes.rs` 曾误删 remember 函数群（已从原文件恢复）。
- 后续会话请从「步骤 5」继续，并把每步完成项勾上、更新进度日志。

## 已知遗留（供后续会话决策）

1. `xai-grok-shell` 的 `one_shot_feedback_gcs_config`、`team_blocks_one_shot_trace_upload`、`feedback_trace_offer` 及其测试辅助——trace 卡删除后已成死代码，可整体删（连带 `Feature::FeedbackTraceCard` 配置项与 `feedbackTraceOffer` meta 两端）。
2. `xai_grok_feedback` 库与 `session/persistence_collect_session_files_tests.rs` 的 drafts artifact 测试——若评分链路不需要 drafts 存储，可整体下线。
3. `session/mod.rs` 的 `FeedbackOutcome::SubmittedCleanupFailed`/`OutcomeUnknown` 变体是否仍被引用。
4. pager `app_view.rs` 的 `shell_feedback_trace_offer` 与 `feedback_trace_choice_latched`。
5. locale 的 `feedback.*` 键清理时注意区分评分（保留）与 /feedback（删除）。
