//! QuickJS runtime wrapper: limits, host binding, envelope extraction.
//!
//! Host functions exchange JSON **strings** at the JS↔Rust boundary (no
//! serde FromJs/IntoJs dependency on rquickjs); a small JS glue layer
//! `JSON.parse`s results for scripts. Failures are thrown in JS as
//! `__cm_host__…` / `__cm_call_limit__…` errors and classified by the outer
//! envelope wrapper.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::{Async, Func, Rest};
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Object, Promise};

use crate::error::{CodemodeError, CodemodeErrorKind};
use crate::host::{HostCallError, HostTools};

/// Sandbox resource limits. Defaults follow the codemode design doc
/// (30s / 64 MiB / 64 host calls / concurrency 4 / 256 KiB return).
#[derive(Debug, Clone)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub max_stack_bytes: usize,
    pub max_host_calls: usize,
    pub max_concurrency: usize,
    pub max_return_bytes: usize,
    pub max_console_lines: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            memory_bytes: 64 * 1024 * 1024,
            max_stack_bytes: 1024 * 1024,
            max_host_calls: 64,
            max_concurrency: 4,
            max_return_bytes: 256 * 1024,
            max_console_lines: 64,
        }
    }
}

/// Result of a successful script run.
#[derive(Debug)]
pub struct ExecOutcome {
    /// Script return value (JSON-safe; degraded to `{ "text": ... }` when the
    /// value was not serializable).
    pub value: serde_json::Value,
    /// `true` when the return value exceeded `max_return_bytes` and was
    /// replaced by a preview envelope.
    pub truncated: bool,
    /// Number of async host calls the script performed (`searchTool` +
    /// `useTool`).
    pub host_calls: u32,
    /// Captured `console.*` lines (capped). On success they stay in
    /// [`ExecOutcome::console`] (debug-only, not model-facing); on failure the
    /// last [`CONSOLE_ERROR_TAIL_LINES`] are appended to the error message.
    pub console: Vec<String>,
    /// Wall-clock milliseconds spent in the sandbox.
    pub wall_ms: u64,
}

/// Prefix used by host bindings to signal a host-call failure.
const HOST_REJECT_PREFIX: &str = "__cm_host__";
const CALL_LIMIT_PREFIX: &str = "__cm_call_limit__";
const ARG_REJECT_PREFIX: &str = "__cm_arg__";

/// How many trailing `console.*` lines are appended to a failure message so
/// the model can see recent script diagnostics (handoff 缺口6 candidate A).
/// Lines are already capped at 512 B by [`push_console_line`].
const CONSOLE_ERROR_TAIL_LINES: usize = 10;

/// User code is wrapped in an async IIFE. The envelope is always JSON-safe
/// and the outer promise resolves (not rejects) for ordinary failures; the
/// final `.then` stringifies so the Rust side can `serde_json::from_str` the
/// single await result.
fn wrap_script(code: &str) -> String {
    format!(
        r#"(async () => {{
  try {{
    const __ret = await (async () => {{
{code}
    }})();
    try {{
      return {{ ok: true, value: JSON.parse(JSON.stringify(__ret === undefined ? null : __ret)) }};
    }} catch (e) {{
      return {{ ok: true, value: {{ text: String(__ret), degraded: true }} }};
    }}
  }} catch (e) {{
    const msg = (e && e.message) ? String(e.message) : String(e);
    if (msg.indexOf("{HOST_REJECT_PREFIX}") === 0) {{
      try {{
        const p = JSON.parse(msg.slice("{HOST_REJECT_PREFIX}".length));
        return {{ ok: false, host: {{ code: p.code, message: p.message }} }};
      }} catch (_) {{}}
    }}
    if (msg.indexOf("{CALL_LIMIT_PREFIX}") === 0) {{
      return {{ ok: false, callLimit: msg.slice("{CALL_LIMIT_PREFIX}".length) }};
    }}
    if (msg.indexOf("{ARG_REJECT_PREFIX}") === 0) {{
      return {{ ok: false, argError: msg.slice("{ARG_REJECT_PREFIX}".length) }};
    }}
    const stack = (e && e.stack) ? String(e.stack) : "";
    // QuickJS self-raised fatal errors (OOM / stack overflow / interrupt) are
    // `InternalError`; user `throw new Error(...)` keeps `name === "Error"`.
    // Only the former may be re-classified by keyword on the Rust side.
    const internal = (e && e.name === "InternalError");
    return {{ ok: false, message: msg, stack: stack, internal: internal }};
  }}
}})()"#
    )
}

/// JS glue: raw Rust bindings return JSON strings (or `__cm_*` reject
/// strings); this layer turns them into the `host` / `tools` objects scripts
/// use.
const GLUE: &str = r#"
globalThis.__cm_unwrap = function (s) {
  if (typeof s === "string" && s.indexOf("__cm_") === 0) throw new Error(s);
  return JSON.parse(s);
};
globalThis.__cm_arg_reject = function (e) {
  var msg = (e && e.message) ? String(e.message) : String(e);
  throw new Error("__cm_arg__" + JSON.stringify({ code: "codemode_invalid_argument", message: msg }));
};
globalThis.host = {
  servers: function () { return __cm_unwrap(__cm_servers()); }
};
globalThis.tools = {
  searchTool: async function (arg) {
    var s;
    try { s = JSON.stringify(arg || {}); } catch (e) { __cm_arg_reject(e); }
    return __cm_unwrap(await __cm_search(s));
  },
  useTool: async function (name, input) {
    var s;
    try { s = JSON.stringify(input === undefined ? {} : input); } catch (e) { __cm_arg_reject(e); }
    return __cm_unwrap(await __cm_use(name, s));
  }
};
"#;

/// Classify a fatal error message (eval failure, envelope error, rejection).
fn classify_fatal_text(text: &str) -> CodemodeError {
    let lower = text.to_ascii_lowercase();
    if lower.contains("interrupted") {
        return CodemodeError::new(CodemodeErrorKind::Timeout, text);
    }
    if lower.contains("out of memory") || lower.contains("stack overflow") {
        return CodemodeError::new(CodemodeErrorKind::Oom, text);
    }
    CodemodeError::new(CodemodeErrorKind::Runtime, text)
}

/// Read the context's pending exception (if any) as `message\nstack`.
/// Returns `None` when there is no useful pending exception.
fn pending_exception_text<'js>(ctx: &Ctx<'js>) -> Option<std::string::String> {
    let pending = ctx.catch();
    let obj = pending.as_object()?;
    let message = obj
        .get::<_, std::string::String>("message")
        .unwrap_or_default();
    if message.is_empty() {
        return None;
    }
    let stack = obj
        .get::<_, std::string::String>("stack")
        .unwrap_or_default();
    Some(if stack.is_empty() {
        message
    } else {
        format!("{message}\n{stack}")
    })
}

/// `Promise::into_future` failures surface as "Exception generated by
/// QuickJS"; the real message lives in the context's pending exception (an
/// Error object — `JSON.stringify` of it is `{}`).
fn classify_await_error<'js>(ctx: &Ctx<'js>, err: &rquickjs::Error) -> CodemodeError {
    let text = pending_exception_text(ctx).unwrap_or_else(|| err.to_string());
    classify_fatal_text(&text)
}

fn push_console_line(console: &Mutex<Vec<String>>, max_lines: usize, line: String) {
    let mut guard = match console.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if guard.len() >= max_lines {
        return;
    }
    let mut line = line;
    if line.len() > 512 {
        let mut cut = 512;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str("…");
    }
    guard.push(line);
}

/// Last [`CONSOLE_ERROR_TAIL_LINES`] captured console lines (empty when none).
fn console_error_tail(console: &Mutex<Vec<String>>) -> Vec<String> {
    let lines = console.lock().map(|g| g.clone()).unwrap_or_default();
    let start = lines.len().saturating_sub(CONSOLE_ERROR_TAIL_LINES);
    lines[start..].to_vec()
}

/// Append a console tail to a failure message. No-op when the tail is empty
/// so existing host/syntax/timeout messages stay byte-identical without logs.
fn attach_console_tail(mut err: CodemodeError, tail: &[String]) -> CodemodeError {
    if tail.is_empty() {
        return err;
    }
    err.message = format!("{}\n\n[console]\n{}", err.message, tail.join("\n"));
    err
}

fn stringify_host_error(err: &HostCallError) -> String {
    let payload = serde_json::json!({ "code": err.code, "message": err.message });
    match serde_json::to_string(&payload) {
        Ok(json) => format!("{HOST_REJECT_PREFIX}{json}"),
        Err(_) => format!(
            "{HOST_REJECT_PREFIX}{}",
            serde_json::json!({ "code": "codemode_host_error", "message": "host error unserializable" })
        ),
    }
}

/// Concurrency gate + host-call budget. Returns the JSON string the raw
/// binding should hand back to JS: either the call's serialized value or a
/// `__cm_*` reject string.
async fn gated_host_json<T: serde::Serialize>(
    host_calls: &AtomicUsize,
    max_host_calls: usize,
    semaphore: &tokio::sync::Semaphore,
    call: impl std::future::Future<Output = Result<T, HostCallError>>,
) -> String {
    let n = host_calls.fetch_add(1, Ordering::SeqCst);
    if n >= max_host_calls {
        return format!("{CALL_LIMIT_PREFIX}host call limit of {max_host_calls} exceeded");
    }
    let _permit = match semaphore.acquire().await {
        Ok(p) => p,
        Err(_) => {
            return stringify_host_error(&HostCallError::new(
                "codemode_host_error",
                "concurrency limiter closed",
            ));
        }
    };
    match call.await {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
        Err(err) => stringify_host_error(&err),
    }
}

fn value_to_console_line<'js>(ctx: &Ctx<'js>, v: &rquickjs::Value<'js>) -> String {
    if v.is_undefined() {
        return "undefined".to_string();
    }
    if v.is_null() {
        return "null".to_string();
    }
    if let Some(s) = v.as_string() {
        if let Ok(s) = s.to_string() {
            return s;
        }
    }
    if let Ok(Some(s)) = ctx.json_stringify(v) {
        return s
            .to_string()
            .unwrap_or_else(|_| "<unprintable>".to_string());
    }
    "<unprintable>".to_string()
}

/// One `console.*` JS function. A generic fn (not an inline closure) so the
/// `'js` lifetimes of `Ctx` and `Rest<Value>` unify.
fn make_console_fn<'js>(
    ctx: &Ctx<'js>,
    max_lines: usize,
    console: Arc<Mutex<Vec<String>>>,
) -> Result<rquickjs::Function<'js>, rquickjs::Error> {
    rquickjs::Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, args: Rest<rquickjs::Value<'js>>| {
            let line = args
                .0
                .iter()
                .map(|v| value_to_console_line(&ctx, v))
                .collect::<Vec<_>>()
                .join(" ");
            push_console_line(&console, max_lines, line);
        },
    )
}

/// Bind raw `__cm_*` globals onto `ctx`. The JS [`GLUE`] builds the
/// model-visible `host` / `tools` objects on top of them.
fn bind_globals(
    ctx: &Ctx<'_>,
    host: Arc<dyn HostTools>,
    host_calls: Arc<AtomicUsize>,
    semaphore: Arc<tokio::sync::Semaphore>,
    console: Arc<Mutex<Vec<String>>>,
    limits: &Limits,
) -> Result<(), rquickjs::Error> {
    let globals = ctx.globals();

    // console.* — capped; success keeps them out of the model envelope, failure
    // appends the last few lines to the error message (缺口6).
    let max_console_lines = limits.max_console_lines;
    let console_obj = Object::new(ctx.clone())?;
    for name in ["log", "warn", "error", "info"] {
        let f = make_console_fn(ctx, max_console_lines, console.clone())?;
        console_obj.set(name, f)?;
    }
    globals.set("console", console_obj)?;

    // host.servers() — sync metadata read, JSON string over the boundary.
    {
        let host = host.clone();
        let servers = Func::from(move || host.servers().to_string());
        globals.set("__cm_servers", servers)?;
    }

    // tools.searchTool — async, JSON-string in/out.
    {
        let host = host.clone();
        let host_calls = host_calls.clone();
        let semaphore = semaphore.clone();
        let max_host_calls = limits.max_host_calls;
        let search = Async(move |_ctx: Ctx<'_>, arg_json: String| {
            let host = host.clone();
            let host_calls = host_calls.clone();
            let semaphore = semaphore.clone();
            async move {
                let arg: serde_json::Value =
                    serde_json::from_str(&arg_json).unwrap_or(serde_json::Value::Null);
                let query = arg
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let limit = arg
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(5) as usize;
                let fut = async move { host.search(query, limit).await };
                gated_host_json(&host_calls, max_host_calls, &semaphore, fut).await
            }
        });
        globals.set("__cm_search", Func::from(search))?;
    }

    // tools.useTool — async, JSON-string in/out.
    {
        let host = host.clone();
        let host_calls = host_calls.clone();
        let semaphore = semaphore.clone();
        let max_host_calls = limits.max_host_calls;
        let use_tool = Async(move |_ctx: Ctx<'_>, name: String, input_json: String| {
            let host = host.clone();
            let host_calls = host_calls.clone();
            let semaphore = semaphore.clone();
            async move {
                let input: serde_json::Value =
                    serde_json::from_str(&input_json).unwrap_or(serde_json::Value::Null);
                let fut = async move { host.use_tool(name, input).await };
                gated_host_json(&host_calls, max_host_calls, &semaphore, fut).await
            }
        });
        globals.set("__cm_use", Func::from(use_tool))?;
    }

    Ok(())
}

/// Execute `code` in a fresh sandbox with the given host bindings.
///
/// The whole run is bounded by `limits.timeout` (both a QuickJS interrupt
/// handler for CPU-bound loops and a tokio timeout for stuck host futures).
pub async fn execute(
    code: &str,
    host: Arc<dyn HostTools>,
    limits: &Limits,
) -> Result<ExecOutcome, CodemodeError> {
    let started = Instant::now();
    let deadline = started + limits.timeout;
    let host_calls = Arc::new(AtomicUsize::new(0));
    let console = Arc::new(Mutex::new(Vec::<String>::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(limits.max_concurrency.max(1)));

    let rt = AsyncRuntime::new().map_err(|e| {
        CodemodeError::new(CodemodeErrorKind::Runtime, format!("runtime init: {e}"))
    })?;
    rt.set_memory_limit(limits.memory_bytes).await;
    rt.set_max_stack_size(limits.max_stack_bytes).await;
    // Returning `true` interrupts JS execution (QuickJS interrupt contract).
    let interrupt_deadline = deadline;
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= interrupt_deadline)))
        .await;

    let ctx = AsyncContext::full(&rt).await.map_err(|e| {
        CodemodeError::new(CodemodeErrorKind::Runtime, format!("context init: {e}"))
    })?;

    let full_source = format!("{GLUE}\n{wrapped}", wrapped = wrap_script(code));
    let limits_clone = limits.clone();
    let host_calls_rt = host_calls.clone();
    let console_rt = console.clone();
    let run = async {
        ctx.async_with(async move |ctx| {
            bind_globals(
                &ctx,
                host,
                host_calls_rt,
                semaphore,
                console_rt,
                &limits_clone,
            )
            .map_err(|e| CodemodeError::new(CodemodeErrorKind::Runtime, format!("bind: {e}")))?;
            let promise: Promise =
                ctx.eval_promise(full_source.as_bytes().to_vec())
                    .map_err(|e| {
                        // Usually compile/parse time — but the wrapper's synchronous
                        // prefix can also run into the interrupt handler or OOM.
                        let detail = pending_exception_text(&ctx).unwrap_or_else(|| e.to_string());
                        let lower = detail.to_ascii_lowercase();
                        if lower.contains("interrupted") {
                            CodemodeError::new(CodemodeErrorKind::Timeout, detail)
                        } else if lower.contains("out of memory")
                            || lower.contains("stack overflow")
                        {
                            CodemodeError::new(CodemodeErrorKind::Oom, detail)
                        } else {
                            CodemodeError::new(CodemodeErrorKind::Syntax, detail)
                        }
                    })?;
            // eval_promise (JS_EVAL_FLAG_ASYNC) resolves the outer promise to
            // a `{ value: <completion> }` record; the completion itself may be
            // the IIFE's promise, which is awaited here in Rust.
            let wrapper: rquickjs::Value = promise
                .into_future::<rquickjs::Value>()
                .await
                .map_err(|e| classify_await_error(&ctx, &e))?;
            let mut inner: rquickjs::Value = match wrapper.try_into_object() {
                Ok(obj) => obj
                    .get::<_, rquickjs::Value>("value")
                    .unwrap_or(rquickjs::Value::new_undefined(ctx.clone())),
                Err(v) => v,
            };
            for _ in 0..4 {
                match inner.try_into_promise() {
                    Ok(p) => {
                        inner = p
                            .into_future::<rquickjs::Value>()
                            .await
                            .map_err(|e| classify_await_error(&ctx, &e))?;
                    }
                    Err(v) => {
                        inner = v;
                        break;
                    }
                }
            }
            let stringified = ctx
                .json_stringify(inner)
                .map_err(|e| classify_fatal_text(&e.to_string()))?;
            let envelope_json: std::string::String = match stringified {
                Some(s) => s
                    .to_string()
                    .map_err(|e| classify_fatal_text(&e.to_string()))?,
                None => "null".to_string(),
            };
            serde_json::from_str(&envelope_json)
                .map_err(|e| CodemodeError::new(CodemodeErrorKind::Runtime, e.to_string()))
        })
        .await
    };

    let envelope: serde_json::Value = match tokio::time::timeout(limits.timeout, run).await {
        Ok(Ok(envelope)) => envelope,
        Ok(Err(e)) => return Err(attach_console_tail(e, &console_error_tail(&console))),
        Err(_) => {
            return Err(attach_console_tail(
                CodemodeError::new(
                    CodemodeErrorKind::Timeout,
                    format!("script exceeded {:?}", limits.timeout),
                ),
                &console_error_tail(&console),
            ));
        }
    };

    let wall_ms = started.elapsed().as_millis() as u64;
    let host_calls = host_calls.load(Ordering::SeqCst) as u32;
    let console_lines = console.lock().map(|g| g.clone()).unwrap_or_default();

    let (value, truncated) = match envelope.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => {
            let raw =
                envelope
                    .get("value")
                    .cloned()
                    .unwrap_or_else(|| match envelope.get("text") {
                        Some(serde_json::Value::String(s)) => {
                            serde_json::json!({ "text": s.clone() })
                        }
                        _ => serde_json::Value::Null,
                    });
            fit_return(raw, limits.max_return_bytes)
        }
        Some(false) => {
            let tail = console_error_tail(&console);
            if let Some(host_err) = envelope.get("host") {
                let code = host_err
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("codemode_host_error")
                    .to_string();
                let message = host_err
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("host call failed")
                    .to_string();
                return Err(attach_console_tail(
                    CodemodeError::host(code, message),
                    &tail,
                ));
            }
            if let Some(arg_msg) = envelope.get("argError").and_then(serde_json::Value::as_str) {
                // The JS side JSON-parses `{code, message}`; surface as a
                // typed invalid-argument failure the script could have
                // caught with try/catch.
                if let Ok(p) = serde_json::from_str::<serde_json::Value>(arg_msg) {
                    let code = p
                        .get("code")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("codemode_invalid_argument")
                        .to_string();
                    let message = p
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("invalid host-call argument")
                        .to_string();
                    return Err(attach_console_tail(
                        CodemodeError::host(code, message),
                        &tail,
                    ));
                }
                return Err(attach_console_tail(
                    CodemodeError::new(CodemodeErrorKind::InvalidArgument, arg_msg),
                    &tail,
                ));
            }
            if let Some(msg) = envelope
                .get("callLimit")
                .and_then(serde_json::Value::as_str)
            {
                return Err(attach_console_tail(
                    CodemodeError::new(CodemodeErrorKind::CallLimit, msg),
                    &tail,
                ));
            }
            let message = envelope
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("script failed")
                .to_string();
            let stack = envelope
                .get("stack")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let full = if stack.is_empty() {
                message
            } else {
                format!("{message}\n{stack}")
            };
            // Keyword classification only applies to QuickJS-internal failures
            // (`InternalError`). User-thrown Errors always map to Runtime so
            // `throw new Error("stack overflow")` is not reported as OOM.
            let is_internal = envelope
                .get("internal")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return Err(attach_console_tail(
                if is_internal {
                    classify_fatal_text(&full)
                } else {
                    CodemodeError::new(CodemodeErrorKind::Runtime, full)
                },
                &tail,
            ));
        }
        _ => {
            return Err(attach_console_tail(
                CodemodeError::new(
                    CodemodeErrorKind::Runtime,
                    format!("sandbox returned an unrecognised envelope: {envelope}"),
                ),
                &console_error_tail(&console),
            ));
        }
    };

    Ok(ExecOutcome {
        value,
        truncated,
        host_calls,
        console: console_lines,
        wall_ms,
    })
}

/// Enforce the return-size budget: oversized values are replaced by a
/// `{ truncated, preview, original_bytes }` envelope.
fn fit_return(value: serde_json::Value, max_bytes: usize) -> (serde_json::Value, bool) {
    let text = match serde_json::to_string(&value) {
        Ok(t) => t,
        Err(_) => {
            return (
                serde_json::json!({ "text": "return value unserializable" }),
                true,
            );
        }
    };
    if text.len() <= max_bytes {
        return (value, false);
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (
        serde_json::json!({
            "truncated": true,
            "preview": text[..cut].to_string(),
            "original_bytes": text.len(),
        }),
        true,
    )
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
