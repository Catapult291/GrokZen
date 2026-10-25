//! Vue language server, 2.x and 3.x.
//!
//! Volar 2.x answers everything itself once `vue.hybridMode` is `false`.
//! Volar 3.x removed that mode: script-level intelligence (definition,
//! references, hover types) lives in a TypeScript server loaded with
//! `@vue/typescript-plugin`, and the Vue server itself asks the client to
//! run tsserver commands for it through `tsserver/request` notifications,
//! expecting `tsserver/response` back. Editors wire this up themselves;
//! Astrolabe is the client here, so it must too.
//!
//! [`VueBridge`] does that: it starts `typescript-language-server` with the
//! Vue plugin (the primary backend for `.vue` queries) and the Vue server
//! next to it, forwarding each `tsserver/request` to tsls via
//! `workspace/executeCommand` `typescript.tsserverRequest` and relaying the
//! response body. The version is read from `@vue/language-server`'s
//! `package.json` next to the binary; when that cannot be found the server
//! is started as 2.x and promoted to the bridge if `initialize` reports 3.x.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::transport::{find_tsserver, LspSession, LspTransport};
use super::{
    Diagnostic, FileChange, LanguageServer, Location, LspError, Position, ServerSpec, WorkspaceEdit,
};
use crate::types::{Language, RelPath};

const VUE_PKG: &str = "@vue/language-server";
const VUE_PLUGIN: &str = "@vue/typescript-plugin";

/// Start the Vue server matching the installed Volar major version.
pub(crate) fn spawn_vue(
    spec: &ServerSpec,
    root: &Path,
    timeout: Duration,
    query_timeout: Duration,
) -> Result<Box<dyn LanguageServer>, LspError> {
    match vue_major(&spec.command) {
        Some(major) if major >= 3 => Ok(Box::new(VueBridge::start(
            spec,
            root,
            timeout,
            query_timeout,
        )?)),
        Some(_) => Ok(Box::new(start_v2(spec, root, timeout, query_timeout)?)),
        None => {
            // Version unknown (a wrapper hides the package). Start the Vue
            // server once, ready for either major: the relay is installed
            // before `initialize` (2.x never uses it), the 2.x options are
            // sent (3.x ignores `hybridMode` and reads `typescript.tsdk`),
            // and `--tsdk` is passed (2.x ignores unknown flags). If it turns
            // out to be 3.x, only the TypeScript half is started next to it;
            // the Vue server is not started twice.
            let tsdk = tsdk_dir(root, &spec.command);
            let mut vue_spec = spec.clone();
            push_tsdk_arg(&mut vue_spec, tsdk.as_deref());
            let vue_transport = LspTransport::spawn_in(&vue_spec, root, timeout)?;
            let (tx, rx) = mpsc::channel::<Value>();
            vue_transport.set_notification_relay("tsserver/request", tx);
            vue_transport.initialize_with(root, None).inspect_err(|_| {
                let _ = vue_transport.shutdown();
            })?;
            let reported = vue_transport
                .server_version()
                .as_deref()
                .and_then(parse_major);
            let vue = LspSession::new(vue_transport, root.to_path_buf(), query_timeout);
            if reported.is_some_and(|m| m >= 3) {
                Ok(Box::new(VueBridge::attach(
                    spec,
                    root,
                    timeout,
                    query_timeout,
                    Arc::new(vue),
                    rx,
                )?))
            } else {
                Ok(Box::new(vue))
            }
        }
    }
}

/// `--tsdk` / `typescript.tsdk`: the TypeScript next to the found tsserver,
/// else the workspace's own.
fn tsdk_dir(root: &Path, command: &Path) -> Option<PathBuf> {
    find_tsserver(root, command)
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| {
            let local = root.join("node_modules/typescript/lib");
            local.is_dir().then_some(local)
        })
}

fn push_tsdk_arg(spec: &mut ServerSpec, tsdk: Option<&Path>) {
    if let Some(tsdk) = tsdk {
        if !spec.args.iter().any(|a| a.starts_with("--tsdk")) {
            spec.args.push(format!("--tsdk={}", tsdk.to_string_lossy()));
        }
    }
}

fn start_v2(
    spec: &ServerSpec,
    root: &Path,
    timeout: Duration,
    query_timeout: Duration,
) -> Result<LspSession, LspError> {
    let transport = LspTransport::connect(spec, root, timeout)?;
    Ok(LspSession::new(
        transport,
        root.to_path_buf(),
        query_timeout,
    ))
}

/// Major version of the Volar install behind `command`.
///
/// `ASTROLABE_VUE_LS_MAJOR` overrides (for wrappers that hide the package).
pub fn vue_major(command: &Path) -> Option<u32> {
    if let Some(v) = std::env::var_os("ASTROLABE_VUE_LS_MAJOR") {
        if let Some(m) = v.to_str().and_then(parse_major) {
            return Some(m);
        }
    }
    let pkg = vue_package_dir(command)?;
    let text = std::fs::read_to_string(pkg.join("package.json")).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    parse_major(json.get("version")?.as_str()?)
}

fn parse_major(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Directory of the `@vue/language-server` package that `command` runs.
///
/// Covers the real file behind an npm symlink (unix), the npm global prefix
/// layouts (`<prefix>/bin` + `<prefix>/lib/node_modules` on unix, shims in
/// `<prefix>` + `<prefix>/node_modules` on Windows) and a project-local
/// `node_modules/.bin` shim.
pub fn vue_package_dir(command: &Path) -> Option<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(real) = std::fs::canonicalize(command) {
        starts.push(real);
    }
    starts.push(command.to_path_buf());
    for start in &starts {
        for dir in start.ancestors().skip(1) {
            if is_vue_pkg(dir) {
                return Some(dir.to_path_buf());
            }
            for cand in [
                dir.join("node_modules").join(VUE_PKG),
                dir.join("lib").join("node_modules").join(VUE_PKG),
            ] {
                if is_vue_pkg(&cand) {
                    return Some(cand);
                }
            }
        }
    }
    None
}

fn is_vue_pkg(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("name").and_then(Value::as_str).map(|n| n == VUE_PKG))
        .unwrap_or(false)
}

/// `@vue/typescript-plugin` for the TS server: env override, the workspace's
/// own copy, then the one shipped with (or hoisted next to) the Vue server.
pub fn find_vue_ts_plugin(root: &Path, vue_pkg: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ASTROLABE_VUE_TS_PLUGIN") {
        let p = PathBuf::from(p);
        if p.join("package.json").is_file() {
            return Some(p);
        }
    }
    let mut cands = vec![root.join("node_modules").join(VUE_PLUGIN)];
    if let Some(pkg) = vue_pkg {
        cands.push(pkg.join("node_modules").join(VUE_PLUGIN));
        // Hoisted: `<node_modules>/@vue/language-server` -> `<node_modules>/@vue/typescript-plugin`
        if let Some(scope) = pkg.parent() {
            cands.push(scope.join("typescript-plugin"));
        }
    }
    cands.into_iter().find(|c| c.join("package.json").is_file())
}

/// Volar 3.x: Vue server + TS server with the Vue plugin, glued together.
pub struct VueBridge {
    ts: Arc<LspSession>,
    vue: Arc<LspSession>,
}

/// Everything needed to launch the TypeScript half.
struct TsHalf {
    spec: ServerSpec,
    options: Value,
    tsserver: Option<PathBuf>,
}

fn ts_half(spec: &ServerSpec, root: &Path) -> Result<TsHalf, LspError> {
    let vue_pkg = vue_package_dir(&spec.command);
    let plugin = find_vue_ts_plugin(root, vue_pkg.as_deref()).ok_or_else(|| {
        LspError::Startup(
            Language::Vue,
            format!(
                "Volar 3.x needs {VUE_PLUGIN} for its TypeScript half; not found next to \
                 {VUE_PKG} or in node_modules (set ASTROLABE_VUE_TS_PLUGIN, or \
                 `npm i -g @vue/language-server @vue/typescript-plugin typescript-language-server typescript`)"
            ),
        )
    })?;
    let ts_base = super::discovery::discover(Language::TypeScript).map_err(|_| {
        LspError::Startup(
            Language::Vue,
            "Volar 3.x forwards script requests to a TypeScript server; install \
             typescript-language-server (`npm i -g typescript-language-server typescript`)"
                .into(),
        )
    })?;
    let tsserver = find_tsserver(root, &ts_base.command).or_else(|| {
        vue_pkg
            .as_deref()
            .and_then(|p| find_tsserver(root, &p.join("bin").join("x")))
    });
    // Same launch as for .ts files, but tagged Vue so documents open with
    // languageId "vue", and with the plugin loaded.
    let ts_spec = ServerSpec {
        language: Language::Vue,
        command: ts_base.command.clone(),
        args: ts_base.args.clone(),
        install_hint: ts_base.install_hint.clone(),
    };
    let mut options = json!({
        "plugins": [{
            "name": VUE_PLUGIN,
            "location": plugin.to_string_lossy(),
            "languages": ["vue"],
        }],
    });
    if let Some(t) = &tsserver {
        options["tsserver"] = json!({ "path": t.to_string_lossy() });
    }
    Ok(TsHalf {
        spec: ts_spec,
        options,
        tsserver,
    })
}

impl VueBridge {
    /// Start both halves, concurrently: neither waits on the other's
    /// `initialize`.
    pub fn start(
        spec: &ServerSpec,
        root: &Path,
        timeout: Duration,
        query_timeout: Duration,
    ) -> Result<Self, LspError> {
        let ts_plan = ts_half(spec, root)?;

        // Vue half. `--tsdk` picks the TypeScript the server loads itself;
        // without it Volar 3 falls back to `require('typescript')`.
        let tsdk = ts_plan
            .tsserver
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| {
                let local = root.join("node_modules/typescript/lib");
                local.is_dir().then_some(local)
            });
        let mut vue_spec = spec.clone();
        push_tsdk_arg(&mut vue_spec, tsdk.as_deref());
        let mut vue_opts = json!({});
        if let Some(tsdk) = &tsdk {
            vue_opts["typescript"] = json!({ "tsdk": tsdk.to_string_lossy() });
        }

        let (ts_started, vue_started) = thread::scope(|scope| {
            let ts = scope.spawn(|| {
                LspTransport::connect_with(
                    &ts_plan.spec,
                    root,
                    timeout,
                    Some(ts_plan.options.clone()),
                )
            });
            let vue = (|| {
                let vue_transport = LspTransport::spawn_in(&vue_spec, root, timeout)?;
                // Relay must be installed before `initialize`: Volar may ask
                // for project info as soon as it has a document.
                let (tx, rx) = mpsc::channel::<Value>();
                vue_transport.set_notification_relay("tsserver/request", tx);
                vue_transport
                    .initialize_with(root, Some(vue_opts.clone()))
                    .inspect_err(|_| {
                        let _ = vue_transport.shutdown();
                    })?;
                Ok::<_, LspError>((vue_transport, rx))
            })();
            let ts = ts.join().unwrap_or_else(|_| {
                Err(LspError::Startup(
                    Language::Vue,
                    "TypeScript half panicked while starting".into(),
                ))
            });
            (ts, vue)
        });
        let (ts_transport, (vue_transport, rx)) = match (ts_started, vue_started) {
            (Ok(ts), Ok(vue)) => (ts, vue),
            (Ok(ts), Err(err)) => {
                let _ = ts.shutdown();
                return Err(err);
            }
            (Err(err), Ok((vue, _))) => {
                let _ = vue.shutdown();
                return Err(err);
            }
            (Err(err), Err(_)) => return Err(err),
        };
        let ts = Arc::new(LspSession::new(
            ts_transport,
            root.to_path_buf(),
            query_timeout,
        ));
        let vue = Arc::new(LspSession::new(
            vue_transport,
            root.to_path_buf(),
            query_timeout,
        ));
        spawn_relay(rx, Arc::downgrade(&ts), Arc::downgrade(&vue));
        Ok(VueBridge { ts, vue })
    }

    /// Bridge around a Vue server that is already initialized (version only
    /// learnt from `initialize`): start just the TypeScript half.
    fn attach(
        spec: &ServerSpec,
        root: &Path,
        timeout: Duration,
        query_timeout: Duration,
        vue: Arc<LspSession>,
        rx: mpsc::Receiver<Value>,
    ) -> Result<Self, LspError> {
        let started = ts_half(spec, root).and_then(|plan| {
            LspTransport::connect_with(&plan.spec, root, timeout, Some(plan.options))
        });
        let ts_transport = match started {
            Ok(t) => t,
            Err(err) => {
                let _ = vue.shutdown();
                return Err(err);
            }
        };
        let ts = Arc::new(LspSession::new(
            ts_transport,
            root.to_path_buf(),
            query_timeout,
        ));
        spawn_relay(rx, Arc::downgrade(&ts), Arc::downgrade(&vue));
        Ok(VueBridge { ts, vue })
    }

    /// Ask the Vue server only when the TS half had nothing; its answers
    /// cover template-only constructs and are slower (it builds its own
    /// project model on first use).
    fn vue_fallback<T>(&self, f: impl FnOnce(&LspSession) -> Result<T, LspError>) -> Option<T> {
        f(&self.vue).ok()
    }
}

/// How long a query waits for the Vue half once the TS half has answered.
/// The Vue server builds its own project model on first use; its extra
/// answers are worth a short wait, not the full query timeout.
const VUE_GRACE: Duration = Duration::from_millis(1500);

impl VueBridge {
    /// Run `on_ts` here and `on_vue` on a helper thread. The Vue answer is
    /// taken if it arrives by the time the TS half is done plus
    /// [`VUE_GRACE`]; a late one is dropped (its thread finishes on its own,
    /// bounded by the session's query timeout).
    fn both<T: Send + 'static>(
        &self,
        on_vue: impl FnOnce(&LspSession) -> Result<T, LspError> + Send + 'static,
        on_ts: impl FnOnce(&LspSession) -> Result<T, LspError>,
    ) -> (Result<T, LspError>, Option<T>) {
        let (tx, rx) = mpsc::channel();
        let vue = Arc::clone(&self.vue);
        let spawned = thread::Builder::new()
            .name("astrolabe-vue-query".into())
            .spawn(move || {
                let _ = tx.send(on_vue(&vue));
            });
        let ts = on_ts(&self.ts);
        let vue = match spawned {
            Ok(_) => rx.recv_timeout(VUE_GRACE).ok().and_then(Result::ok),
            Err(_) => None,
        };
        (ts, vue)
    }
}

/// TS answer plus the Vue half's extra locations, deduplicated. A TS error
/// is only returned when the Vue half had nothing either.
fn merge_locations(
    ts: Result<Vec<Location>, LspError>,
    vue: Option<Vec<Location>>,
) -> Result<Vec<Location>, LspError> {
    let extra = vue.unwrap_or_default();
    let mut out = match ts {
        Ok(v) => v,
        Err(err) if extra.is_empty() => return Err(err),
        Err(_) => Vec::new(),
    };
    for loc in extra {
        if !out.contains(&loc) {
            out.push(loc);
        }
    }
    Ok(out)
}

fn merge_diagnostics(
    ts: Result<Vec<Diagnostic>, LspError>,
    vue: Option<Vec<Diagnostic>>,
) -> Result<Vec<Diagnostic>, LspError> {
    let extra = vue.unwrap_or_default();
    let mut out = match ts {
        Ok(v) => v,
        Err(err) if extra.is_empty() => return Err(err),
        Err(_) => Vec::new(),
    };
    for d in extra {
        // Both halves can report the same type error for a template
        // expression; same range and message is one diagnostic.
        if !out
            .iter()
            .any(|o| o.path == d.path && o.range == d.range && o.message == d.message)
        {
            out.push(d);
        }
    }
    Ok(out)
}

/// Split Volar's `tsserver/request` params into `(id, command, args)`
/// items. vscode-jsonrpc wraps the tuple once more (`[[id, cmd, args]]`).
pub fn parse_tsserver_requests(params: &Value) -> Vec<(Value, String, Value)> {
    let Some(arr) = params.as_array() else {
        return Vec::new();
    };
    let one = |v: &Value| -> Option<(Value, String, Value)> {
        let a = v.as_array()?;
        let cmd = a.get(1)?.as_str()?.to_string();
        Some((
            a.first()?.clone(),
            cmd,
            a.get(2).cloned().unwrap_or(Value::Null),
        ))
    };
    if arr.first().is_some_and(Value::is_array) {
        arr.iter().filter_map(one).collect()
    } else {
        one(params).into_iter().collect()
    }
}

/// File a tsserver command is about: `{file}` objects or a positional first
/// argument, as Volar's `_vue:*` commands use.
fn request_file(args: &Value) -> Option<String> {
    args.get("file")
        .and_then(Value::as_str)
        .or_else(|| args.as_array()?.first()?.as_str())
        .map(str::to_string)
}

fn rel_under(root: &Path, file: &str) -> Option<RelPath> {
    let norm = |p: &str| p.replace('\\', "/");
    let root_s = norm(&root.to_string_lossy());
    let file_s = norm(file);
    let rest = file_s.strip_prefix(root_s.trim_end_matches('/'))?;
    rest.starts_with('/').then(|| RelPath::new(rest))
}

/// Threads answering Volar's `tsserver/request`s. Bounded: a burst of
/// requests (Volar fires several per opened document) used to start one
/// thread each.
const RELAY_WORKERS: usize = 4;

type RelayJob = (Value, String, Value);

fn spawn_relay(rx: mpsc::Receiver<Value>, ts: Weak<LspSession>, vue: Weak<LspSession>) {
    let (work_tx, work_rx) = mpsc::channel::<RelayJob>();
    let work_rx = Arc::new(Mutex::new(work_rx));
    for _ in 0..RELAY_WORKERS {
        let work_rx = Arc::clone(&work_rx);
        let ts = ts.clone();
        let vue = vue.clone();
        let _ = thread::Builder::new()
            .name("astrolabe-vue-tsserver".into())
            .spawn(move || loop {
                // Ends once the dispatcher (and with it `work_tx`) is gone.
                let job = work_rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                let Ok((id, command, args)) = job else {
                    return;
                };
                let body = match ts.upgrade() {
                    Some(ts) => forward(&ts, &command, &args),
                    None => Value::Null,
                };
                // Always answer: Volar awaits every id forever.
                if let Some(vue) = vue.upgrade() {
                    let _ = vue
                        .transport()
                        .notify("tsserver/response", tsserver_response(id, body));
                }
            });
    }
    let _ = thread::Builder::new()
        .name("astrolabe-vue-relay".into())
        .spawn(move || {
            // Ends when the Vue transport (which owns the sender) is dropped.
            while let Ok(params) = rx.recv() {
                for job in parse_tsserver_requests(&params) {
                    if work_tx.send(job).is_err() {
                        return;
                    }
                }
            }
        });
}

/// Params for `tsserver/response`. Volar's handler destructures
/// `([id, body])` from the first positional parameter, and vscode-jsonrpc
/// spreads a top-level array into positional parameters, so the tuple must be
/// wrapped once more; a flat `[id, body]` is silently dropped and the Vue
/// server then waits on that id forever.
fn tsserver_response(id: Value, body: Value) -> Value {
    json!([[id, body]])
}

fn forward(ts: &LspSession, command: &str, args: &Value) -> Value {
    // tsserver only knows files someone opened (or its project includes);
    // open the one Volar asks about so `_vue:projectInfo` etc. resolve.
    if let Some(rel) = request_file(args).and_then(|f| rel_under(ts.root(), &f)) {
        let _ = ts.ensure_open(&rel);
    }
    let reply = ts.transport().request_timeout(
        "workspace/executeCommand",
        json!({
            "command": "typescript.tsserverRequest",
            "arguments": [command, args, { "isAsync": true, "lowPriority": true }],
        }),
        ts.query_timeout(),
    );
    match reply {
        Ok(v) => v.get("body").cloned().unwrap_or(Value::Null),
        Err(err) => {
            tracing::debug!(%command, ?err, "vue bridge: tsserver request failed");
            Value::Null
        }
    }
}

fn hover_is_empty(v: &Value) -> bool {
    match v.get("contents") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o
            .get("value")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty()),
        Some(_) => false,
    }
}

impl LanguageServer for VueBridge {
    fn language(&self) -> Language {
        Language::Vue
    }

    fn references(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        // Template-only uses (component tags, slot props) are the Vue
        // server's; script uses are the TS half's. Ask both, merge.
        let path_c = path.clone();
        let (ts, vue) = self.both(
            move |v| v.references(&path_c, position),
            |t| t.references(path, position),
        );
        merge_locations(ts, vue)
    }

    fn definition(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        let found = self.ts.definition(path, position)?;
        if !found.is_empty() {
            return Ok(found);
        }
        Ok(self
            .vue_fallback(|v| v.definition(path, position))
            .unwrap_or_default())
    }

    fn hover(&self, path: &RelPath, position: Position) -> Result<Value, LspError> {
        let ts = self.ts.hover(path, position);
        if let Ok(v) = &ts {
            if !hover_is_empty(v) {
                return ts;
            }
        }
        match self.vue_fallback(|v| v.hover(path, position)) {
            Some(v) if !hover_is_empty(&v) => Ok(v),
            _ => ts,
        }
    }

    fn diagnostics(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
        // Template syntax / compiler errors come from the Vue server, type
        // errors from the TS half.
        let path_c = path.clone();
        let (ts, vue) = self.both(move |v| v.diagnostics(&path_c), |t| t.diagnostics(path));
        merge_diagnostics(ts, vue)
    }

    fn prepare_rename(
        &self,
        path: &RelPath,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, LspError> {
        self.ts.prepare_rename(path, position, new_name)
    }

    fn wait_until_ready(&self, deadline: Duration) -> bool {
        // Both halves index on their own; wait for them side by side.
        thread::scope(|scope| {
            let vue = scope.spawn(|| self.vue.wait_until_ready(deadline));
            let ts = self.ts.wait_until_ready(deadline);
            ts & vue.join().unwrap_or(false)
        })
    }

    fn is_busy(&self) -> bool {
        self.ts.is_busy() || self.vue.is_busy()
    }

    fn files_changed(&self, changes: &[FileChange]) {
        self.ts.files_changed(changes);
        self.vue.files_changed(changes);
    }

    fn memory_bytes(&self) -> Option<u64> {
        match (self.ts.memory_bytes(), self.vue.memory_bytes()) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        }
    }

    fn shutdown(&self) -> Result<(), LspError> {
        thread::scope(|scope| {
            let vue = scope.spawn(|| self.vue.shutdown());
            let ts = self.ts.shutdown();
            let vue = vue.join().unwrap_or(Ok(()));
            vue.and(ts)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astro-vue-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_pkg(dir: &Path, name: &str, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn parses_wrapped_and_flat_tsserver_requests() {
        let wrapped = json!([[1, "_vue:projectInfo", {"file": "/a.vue"}]]);
        let got = parse_tsserver_requests(&wrapped);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, json!(1));
        assert_eq!(got[0].1, "_vue:projectInfo");
        let flat = json!([7, "_vue:getComponentNames", ["/b.vue"]]);
        let got = parse_tsserver_requests(&flat);
        assert_eq!(got[0].0, json!(7));
        assert_eq!(request_file(&got[0].2).as_deref(), Some("/b.vue"));
        assert!(parse_tsserver_requests(&json!({})).is_empty());
    }

    #[test]
    fn detects_major_from_unix_and_windows_global_layouts() {
        let t = tmp("major");
        // unix: <prefix>/bin/vue-language-server, package in <prefix>/lib/node_modules
        let unix = t.join("unix");
        write_pkg(
            &unix.join("lib/node_modules/@vue/language-server"),
            VUE_PKG,
            "3.3.13",
        );
        std::fs::create_dir_all(unix.join("bin")).unwrap();
        std::fs::write(unix.join("bin/vue-language-server"), "").unwrap();
        assert_eq!(vue_major(&unix.join("bin/vue-language-server")), Some(3));
        // windows: <prefix>/vue-language-server.cmd, package in <prefix>/node_modules
        let win = t.join("win");
        write_pkg(
            &win.join("node_modules/@vue/language-server"),
            VUE_PKG,
            "2.2.12",
        );
        std::fs::write(win.join("vue-language-server.cmd"), "").unwrap();
        assert_eq!(vue_major(&win.join("vue-language-server.cmd")), Some(2));
        // plugin: shipped under the package, or hoisted next to it
        let pkg = vue_package_dir(&unix.join("bin/vue-language-server")).unwrap();
        assert!(find_vue_ts_plugin(&t, Some(&pkg)).is_none());
        write_pkg(
            &pkg.parent().unwrap().join("typescript-plugin"),
            VUE_PLUGIN,
            "3.3.13",
        );
        assert!(find_vue_ts_plugin(&t, Some(&pkg)).is_some());
        assert_eq!(vue_major(&t.join("nowhere/bin/x")), None);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn tsserver_response_is_wrapped_once_more() {
        assert_eq!(
            tsserver_response(json!(3), json!({"a": 1})),
            json!([[3, {"a": 1}]])
        );
    }

    #[test]
    fn rel_under_handles_both_separators() {
        let root = Path::new("/w/repo");
        assert_eq!(
            rel_under(root, "/w/repo/src/a.vue").map(|r| r.as_str().to_string()),
            Some("src/a.vue".into())
        );
        assert!(rel_under(root, "/w/repo2/a.vue").is_none());
        let win = Path::new(r"C:\w\repo");
        assert_eq!(
            rel_under(win, "C:/w/repo/src/a.vue").map(|r| r.as_str().to_string()),
            Some("src/a.vue".into())
        );
    }

    fn loc(path: &str, line: u32) -> Location {
        let at = Position { line, character: 0 };
        Location {
            path: RelPath::new(path),
            range: super::super::Range { start: at, end: at },
        }
    }

    #[test]
    fn references_merge_adds_vue_only_sites_once() {
        let ts = Ok(vec![loc("a.vue", 1), loc("b.ts", 0)]);
        let vue = Some(vec![loc("a.vue", 1), loc("a.vue", 6)]);
        assert_eq!(
            merge_locations(ts, vue).unwrap(),
            vec![loc("a.vue", 1), loc("b.ts", 0), loc("a.vue", 6)]
        );
        // TS failed but the Vue half answered: its answer, not the error.
        let ts: Result<Vec<Location>, LspError> = Err(LspError::Crashed);
        assert_eq!(
            merge_locations(ts, Some(vec![loc("a.vue", 6)])).unwrap(),
            vec![loc("a.vue", 6)]
        );
        // Both empty-handed: the TS error stands.
        let ts: Result<Vec<Location>, LspError> = Err(LspError::Crashed);
        assert!(merge_locations(ts, None).is_err());
    }

    #[test]
    fn diagnostics_merge_keeps_template_errors_and_drops_duplicates() {
        let d = |line: u32, msg: &str| Diagnostic {
            path: RelPath::new("a.vue"),
            range: loc("a.vue", line).range,
            severity: super::super::Severity::Error,
            message: msg.into(),
            code: None,
        };
        // tsls pushes nothing for .vue (observed with Volar 3.3.13): the
        // template compiler errors only come from the Vue half.
        let ts: Result<Vec<Diagnostic>, LspError> =
            Err(LspError::Protocol("pull diagnostic unsupported".into()));
        let key = |v: Vec<Diagnostic>| -> Vec<(u32, String)> {
            v.into_iter()
                .map(|d| (d.range.start.line, d.message))
                .collect()
        };
        let got = merge_diagnostics(ts, Some(vec![d(8, "Invalid end tag.")])).unwrap();
        assert_eq!(key(got), vec![(8, "Invalid end tag.".to_string())]);
        let got = merge_diagnostics(
            Ok(vec![d(3, "Type error")]),
            Some(vec![d(3, "Type error"), d(8, "Invalid end tag.")]),
        )
        .unwrap();
        assert_eq!(
            key(got),
            vec![
                (3, "Type error".to_string()),
                (8, "Invalid end tag.".to_string())
            ]
        );
    }

    #[test]
    fn tsdk_arg_is_added_once() {
        let mut spec = ServerSpec {
            language: Language::Vue,
            command: PathBuf::from("vue-language-server"),
            args: vec!["--stdio".into()],
            install_hint: String::new(),
        };
        push_tsdk_arg(&mut spec, Some(Path::new("/t/lib")));
        push_tsdk_arg(&mut spec, Some(Path::new("/other/lib")));
        push_tsdk_arg(&mut spec, None);
        assert_eq!(
            spec.args,
            vec!["--stdio".to_string(), "--tsdk=/t/lib".into()]
        );
    }

    #[test]
    fn empty_hover_shapes() {
        assert!(hover_is_empty(&Value::Null));
        assert!(hover_is_empty(
            &json!({"contents": {"kind": "markdown", "value": " "}})
        ));
        assert!(hover_is_empty(&json!({"contents": []})));
        assert!(!hover_is_empty(
            &json!({"contents": {"kind": "markdown", "value": "x"}})
        ));
        assert!(!hover_is_empty(&json!({"contents": "x"})));
    }
}
