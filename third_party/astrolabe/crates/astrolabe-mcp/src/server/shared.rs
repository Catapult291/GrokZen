//! Server side of the shared index (see [`crate::shared_index`]): leader
//! session views, direct tool dispatch, and the follower's forwarding hook.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResponse, CallToolResult, ReadResourceResponse, ReadResourceResult};
use rmcp::ErrorData;
use serde_json::Value;

use super::{
    cancel, text_error, tool_timeout_from_env, AstrolabeServer, IndexState, ResourceStore,
};
use crate::context::ClientContext;
use crate::loop_guard::LoopGuard;
use crate::session::RootSession;
use crate::shared_index::{self, Forwarded, Op, SessionView};

/// Extra wait on top of the tool timeout before a silent leader is abandoned.
const LEADER_REPLY_SLACK: Duration = Duration::from_secs(30);

type Args = Option<serde_json::Map<String, Value>>;
/// The client's cancellation, polled while a forwarded call waits.
pub(crate) type Cancelled = Box<dyn Fn() -> bool + Send + Sync>;

fn params<T: serde::de::DeserializeOwned>(args: Args) -> Result<Parameters<T>, ErrorData> {
    serde_json::from_value(Value::Object(args.unwrap_or_default()))
        .map(Parameters)
        .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
}

impl AstrolabeServer {
    /// A new MCP session over the same index: shares the root sessions
    /// (graph, NameIndex, FileCache, watcher, language servers), owns fresh
    /// per-session state (legends shown once, coverage note, loop guard,
    /// dedup tables, page generations, resources) and its own context.
    pub(crate) fn session_view(&self, context: ClientContext) -> Self {
        let mut view = self.clone();
        view.structured_output = super::structured_output_for(&context);
        view.context = context;
        view.loop_guard = Arc::new(Mutex::new(LoopGuard::new(self.primary.knobs.loop_guard)));
        view.resources = Arc::new(Mutex::new(ResourceStore::new()));
        view.next_resource = Arc::new(AtomicU64::new(1));
        view.sent_ranges = Arc::new(Mutex::new(Default::default()));
        view.search_digests = Arc::new(Mutex::new(Default::default()));
        view.symbol_page_generations = Arc::new(Mutex::new(Default::default()));
        view.context_page_generations = Arc::new(Mutex::new(Default::default()));
        view.call_seq = Arc::new(AtomicU64::new(0));
        view.shown_once = Arc::new(Mutex::new(Default::default()));
        view.coverage_note_override = Some(Arc::new(Mutex::new(None)));
        // A view never forwards: it *is* the leader.
        view.shared = None;
        view
    }

    /// Coverage-note memory for this MCP session.
    pub(crate) fn coverage_slot<'a>(
        &'a self,
        session: &'a RootSession,
    ) -> &'a Arc<Mutex<Option<String>>> {
        self.coverage_note_override
            .as_ref()
            .unwrap_or(&session.coverage_note_shown)
    }

    /// Run one catalog tool by name, as the rmcp router would.
    pub(crate) fn dispatch_tool(
        &self,
        name: &str,
        args: Args,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match name {
            "search_code" => self.search_code(params(args)?),
            "find_symbol" => self.find_symbol(params(args)?),
            "resolve_context" => self.resolve_context(params(args)?),
            "get_languages" => self.get_languages(params(args)?),
            "find_references" => self.find_references(params(args)?),
            "goto_definition" => self.goto_definition(params(args)?),
            "get_diagnostics" => self.get_diagnostics(params(args)?),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("tool not found: {other}"),
                    None,
                ))
            }
        })
    }

    /// `resources/read` against this session's store.
    pub(crate) fn read_resource_local(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        match resources.get(uri) {
            Some(text) => Ok(ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::text(text, uri.to_string()),
            ])),
            None => Err(ErrorData::resource_not_found(
                format!(
                    "resource not found: {uri} (expired, evicted, or from an earlier server process; re-run the query)"
                ),
                None,
            )),
        }
    }

    /// Hash of the full (context-independent) tool catalog.
    fn tools_hash(&self) -> String {
        let tools = self.tool_router.list_all();
        let bytes = serde_json::to_vec(&tools).unwrap_or_default();
        format!("{:016x}", shared_index::fnv64(&bytes))
    }

    /// Start serving: join a running leader on the same root (no local
    /// index), become the leader, or index independently. Replaces
    /// [`Self::start_indexing`] for the stdio server.
    pub fn start_indexing_shared(&mut self) {
        #[cfg(unix)]
        if shared_index::enabled() && self.dispatch.is_none() {
            if let Some(sock) = shared_index::private_socket_path(&self.root) {
                let hello = shared_index::Hello::new(&self.root, self.tools_hash(), &self.context);
                let shared = shared_index::Shared::new(self.root.clone(), sock, hello);
                self.shared = Some(Arc::clone(&shared));
                shared.set_host(Arc::new(ServerHost(self.clone())));
                shared.boot();
                self.log_indexing();
                return;
            }
        }
        self.log_indexing();
        self.start_indexing();
    }

    /// "indexing repository", except for a follower (it indexes nothing).
    fn log_indexing(&self) {
        if self.dispatch.is_none() && !self.is_shared_follower() {
            tracing::info!(
                root = %self.root.display(),
                context = %self.context.name,
                structured = self.context.structured_or_auto_off(),
                "indexing repository"
            );
        }
    }

    /// Whether this process forwards to a leader instead of indexing.
    pub fn is_shared_follower(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.is_follower())
    }

    /// Close the shared-index socket (or the leader connection). Call before
    /// [`Self::shutdown_indexing`], which releases the indexer lock.
    pub fn shutdown_shared(&self) {
        if let Some(shared) = &self.shared {
            shared.shutdown();
        }
    }

    /// After a hand-over this process indexes on its own; let the first calls
    /// wait (within the queue budget) for the warm load instead of answering
    /// "building".
    fn wait_ready_after_handover(&self, since: Instant) {
        let until = since + shared_index::QUEUE_BUDGET;
        while Instant::now() < until {
            let building = matches!(
                *self.primary.state.read().unwrap_or_else(|e| e.into_inner()),
                IndexState::Building
            );
            if !building {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    async fn forward(&self, op: Op, cancelled: Cancelled) -> Option<Result<Value, ErrorData>> {
        let shared = Arc::clone(self.shared.as_ref()?);
        let reply_timeout = tool_timeout_from_env().map(|t| t + LEADER_REPLY_SLACK);
        let server = self.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = shared.forward(op, reply_timeout, &*cancelled);
            if matches!(outcome, Forwarded::Local) {
                if let Some(since) = shared.local_since() {
                    server.wait_ready_after_handover(since);
                }
            }
            outcome
        })
        .await
        .ok()?;
        match outcome {
            Forwarded::Local => None,
            Forwarded::Cancelled => Some(Err(ErrorData::internal_error("request cancelled", None))),
            Forwarded::Done(Ok(value)) => Some(Ok(value)),
            Forwarded::Done(Err(error)) => {
                Some(Err(serde_json::from_value(error).unwrap_or_else(|e| {
                    ErrorData::internal_error(
                        format!("bad error from shared-index leader: {e}"),
                        None,
                    )
                })))
            }
        }
    }

    /// Follower hook for `tools/call`: `None` = serve locally.
    pub(crate) async fn forward_call(
        &self,
        name: &str,
        args: Args,
        cancelled: Cancelled,
    ) -> Option<Result<CallToolResponse, ErrorData>> {
        let op = Op::Call {
            name: name.to_string(),
            args,
        };
        Some(self.forward(op, cancelled).await?.and_then(|value| {
            serde_json::from_value::<CallToolResult>(value)
                .map(Into::into)
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("bad result from shared-index leader: {e}"),
                        None,
                    )
                })
        }))
    }

    /// Follower hook for `resources/read`: `None` = serve locally.
    pub(crate) async fn forward_read(
        &self,
        uri: &str,
        cancelled: Cancelled,
    ) -> Option<Result<ReadResourceResponse, ErrorData>> {
        let op = Op::Read {
            uri: uri.to_string(),
        };
        Some(self.forward(op, cancelled).await?.and_then(|value| {
            serde_json::from_value::<ReadResourceResult>(value)
                .map(Into::into)
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("bad resource from shared-index leader: {e}"),
                        None,
                    )
                })
        }))
    }
}

/// [`shared_index::Host`] over the stdio server.
struct ServerHost(AstrolabeServer);

impl shared_index::Host for ServerHost {
    fn open_session(&self, context: ClientContext) -> Arc<dyn SessionView> {
        Arc::new(View(self.0.session_view(context)))
    }

    fn adopt_lock(&self, lock: astrolabe_core::elect::IndexerLock) {
        use crate::session::LockOrRecover;
        *self.0.primary.indexer_lock.lock_or_recover() = Some(lock);
    }

    fn start_local(&self) {
        self.0.start_indexing();
    }

    fn try_take_lock(&self) -> bool {
        // Not `refresh_leader`: it warns on every I/O error, and this is
        // polled once a second.
        use crate::session::LockOrRecover;
        let mut slot = self.0.primary.indexer_lock.lock_or_recover();
        if slot.is_some() {
            return true;
        }
        match astrolabe_core::elect::IndexerLock::try_acquire(&self.0.root) {
            Ok(Some(lock)) => {
                *slot = Some(lock);
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::debug!(%error, "shared index: indexer lock not available");
                false
            }
        }
    }

    fn release_lock(&self) {
        use crate::session::LockOrRecover;
        *self.0.primary.indexer_lock.lock_or_recover() = None;
    }
}

/// One follower session on the leader.
struct View(AstrolabeServer);

impl SessionView for View {
    fn call(&self, name: &str, args: Args, flag: Arc<AtomicBool>) -> Result<Value, Value> {
        // Same envelope as the local `call_tool`: blocking body, cooperative
        // cancel flag, per-call timeout, panic → isError result.
        let server = self.0.clone();
        let tool = name.to_string();
        let worker_flag = Arc::clone(&flag);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("astrolabe-shared-tool".into())
            .spawn(move || {
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cancel::scope(worker_flag, || server.dispatch_tool(&tool, args))
                }));
                let _ = tx.send(out);
            });
        let to_json = |r: Result<CallToolResult, ErrorData>| match r {
            Ok(result) => serde_json::to_value(result).map_err(|e| {
                serde_json::to_value(ErrorData::internal_error(e.to_string(), None))
                    .unwrap_or(Value::Null)
            }),
            Err(error) => Err(serde_json::to_value(error).unwrap_or(Value::Null)),
        };
        if spawned.is_err() {
            return to_json(Err(ErrorData::internal_error(
                "cannot spawn tool thread",
                None,
            )));
        }
        let joined = match tool_timeout_from_env() {
            Some(limit) => rx.recv_timeout(limit).map_err(|_| Some(limit)),
            None => rx.recv().map_err(|_| None),
        };
        match joined {
            Ok(Ok(result)) => to_json(result),
            Ok(Err(_panic)) => {
                tracing::error!(tool = %name, "tool body panicked (shared session)");
                to_json(Ok(self.0.finish(text_error(format!(
                    "{name} 内部错误（已记录日志）。confidence: unknown"
                )))))
            }
            Err(limit) => {
                flag.store(true, Ordering::Relaxed);
                let secs = limit.map(|d| d.as_secs()).unwrap_or_default();
                to_json(Ok(self.0.finish(text_error(format!(
                    "{name} 超过 {secs} 秒未完成，已放弃等待。请缩小范围（path_filter / aggregate / count_only）后重试；\
                     可用 ASTROLABE_TOOL_TIMEOUT_SECS 调整。confidence: unknown"
                )))))
            }
        }
    }

    fn read(&self, uri: &str) -> Result<Value, Value> {
        match self.0.read_resource_local(uri) {
            Ok(result) => serde_json::to_value(result).map_err(|_| Value::Null),
            Err(error) => Err(serde_json::to_value(error).unwrap_or(Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_view_shares_index_but_not_session_state() {
        let dir = std::env::temp_dir().join(format!("as-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "def greet():\n    return 1\n").unwrap();
        let base = AstrolabeServer::new(dir.clone());
        *base.primary.state.write().unwrap() = crate::session::build_index(&dir, false, 0);
        assert!(base.once("legend"));
        let view = base.session_view(ClientContext::builtin("cursor").unwrap());
        assert!(
            Arc::ptr_eq(&base.primary, &view.primary),
            "same root session"
        );
        assert!(view.once("legend"), "once-only state is per session");
        assert!(!view.once("legend"));
        assert!(view.structured_output, "the view uses its own context");
        assert!(view.shared.is_none(), "a view never forwards");
        let args = serde_json::json!({"query": "greet"});
        let out = view
            .dispatch_tool("find_symbol", args.as_object().cloned())
            .unwrap();
        let text = out.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("a.py"), "{text}");
        assert!(view.dispatch_tool("nope", None).is_err());
        assert!(view
            .dispatch_tool(
                "find_symbol",
                serde_json::json!({"query": 5}).as_object().cloned()
            )
            .is_err());
        base.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
