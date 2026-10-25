//! Detect a broken toolchain environment behind `get_diagnostics` output.
//!
//! When the build cache is read-only, the module proxy is unreachable, the
//! interpreter/venv is wrong or `node_modules` was never installed, language
//! servers report a flood of cascade errors (`could not import fmt`, dozens of
//! `undefined: X`, `Cannot find module 'express'`). Those are not code errors;
//! listing them costs tokens and invites the model to "fix" correct code.
//! [`detect`] spots the root-cause line so the tool can answer with a short
//! "环境异常，诊断不可信" instead.

use astrolabe_core::lsp::Diagnostic;
use std::collections::BTreeSet;

/// A detected environment fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvFault {
    /// Short Chinese description of what is broken.
    pub kind: &'static str,
    /// What to run instead of trusting the diagnostics.
    pub hint: &'static str,
    /// Index (into the input slice) of the diagnostic that names the root cause.
    pub root: usize,
}

const GO_HINT: &str =
    "请直接用 go build ./... / go test 验证，或先修好 Go 环境（GOCACHE 可写、模块可下载）。";
const RUST_HINT: &str = "请直接用 cargo check / cargo test 验证，或先修好 Rust 工具链（rustup component add rust-src）。";
const TS_HINT: &str =
    "请先安装依赖（npm/pnpm/yarn install）或直接用 tsc --noEmit / 项目测试命令验证。";
const PY_HINT: &str =
    "请确认语言服务用的是项目的解释器/虚拟环境并已安装依赖，或直接运行项目测试验证。";
const GENERIC_HINT: &str = "请直接运行项目的构建/测试命令验证。";

/// Messages that only ever come from a broken environment, any language.
const GENERIC_PATTERNS: &[(&str, &str)] = &[
    ("read-only file system", "文件系统只读"),
    ("permission denied", "权限不足（permission denied）"),
    ("eacces", "权限不足（EACCES）"),
    ("erofs", "文件系统只读"),
    ("no space left on device", "磁盘已满"),
    (
        "executable file not found in $path",
        "找不到工具链可执行文件",
    ),
    ("command not found", "找不到工具链可执行文件"),
];

/// Go: gopls / go list messages that mean the toolchain, cache or module
/// download is broken rather than the code.
const GO_PATTERNS: &[(&str, &str)] = &[
    (
        "loading compiled go files from cache",
        "Go 构建缓存不可读写（GOCACHE）",
    ),
    (
        "failed to initialize build cache",
        "Go 构建缓存不可读写（GOCACHE）",
    ),
    ("build cache is required", "Go 构建缓存未配置（GOCACHE）"),
    ("gocache is not defined", "Go 构建缓存未配置（GOCACHE）"),
    (
        "missing metadata for import of",
        "gopls 缺包元数据（go list/构建缓存失败）",
    ),
    (
        "module lookup disabled by goproxy=off",
        "Go 模块无法下载（GOPROXY=off）",
    ),
    ("cannot find goroot", "找不到 Go 工具链（GOROOT）"),
    ("go: command not found", "找不到 go 命令"),
    ("exec: \"go\"", "找不到 go 命令"),
    (
        "gotoolchain=local",
        "Go 版本不满足 go.mod 要求且禁止自动下载工具链",
    ),
    (
        "no packages found for open file",
        "gopls 没加载到这个文件所在的包（模块/工作区配置）",
    ),
    (
        "gopls was not able to find modules",
        "gopls 找不到模块（工作区配置）",
    ),
];

/// Network failures; only an env fault when they show up in diagnostics.
const NETWORK_PATTERNS: &[&str] = &[
    "dial tcp",
    "connection refused",
    "i/o timeout",
    "no such host",
    "tls handshake timeout",
    "proxy.golang.org",
    "network is unreachable",
];

const RUST_PATTERNS: &[(&str, &str)] = &[
    (
        "can't find crate for `std`",
        "Rust 标准库缺失（sysroot/rust-src）",
    ),
    (
        "can't find crate for `core`",
        "Rust 标准库缺失（sysroot/rust-src）",
    ),
    (
        "unresolved import `std",
        "Rust 标准库缺失（sysroot/rust-src）",
    ),
    (
        "unresolved import `core",
        "Rust 标准库缺失（sysroot/rust-src）",
    ),
    ("failed to load workspace", "rust-analyzer 加载工作区失败"),
    ("failed to run `cargo metadata`", "cargo metadata 失败"),
    ("cargo metadata", "cargo metadata 失败"),
    ("proc-macro server", "rust-analyzer 过程宏服务异常"),
    ("proc macro server", "rust-analyzer 过程宏服务异常"),
    (
        "is not installed for the toolchain",
        "Rust 工具链组件未安装",
    ),
    (
        "rustup could not choose a version",
        "rustup 没有可用的工具链",
    ),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Go,
    Rust,
    Ts,
    Python,
    Other,
}

fn lang_of(path: &str) -> Lang {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("go") => Lang::Go,
        Some("rs") => Lang::Rust,
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "vue" | "svelte") => {
            Lang::Ts
        }
        Some("py" | "pyi") => Lang::Python,
        _ => Lang::Other,
    }
}

fn hint_for(lang: Lang) -> &'static str {
    match lang {
        Lang::Go => GO_HINT,
        Lang::Rust => RUST_HINT,
        Lang::Ts => TS_HINT,
        Lang::Python => PY_HINT,
        Lang::Other => GENERIC_HINT,
    }
}

/// Look for an environment fault. `path` is the file the tool was asked
/// about (used to pick the language when diagnostics carry other paths).
pub(crate) fn detect(path: &str, diags: &[Diagnostic]) -> Option<EnvFault> {
    let lang = lang_of(path);
    let lower: Vec<String> = diags.iter().map(|d| d.message.to_lowercase()).collect();
    let fault = |kind, root| {
        Some(EnvFault {
            kind,
            hint: hint_for(lang),
            root,
        })
    };

    // 1. Language-specific root-cause messages.
    let specific: &[(&str, &str)] = match lang {
        Lang::Go => GO_PATTERNS,
        Lang::Rust => RUST_PATTERNS,
        _ => &[],
    };
    for (i, msg) in lower.iter().enumerate() {
        if let Some((_, kind)) = specific.iter().find(|(p, _)| msg.contains(p)) {
            return fault(kind, i);
        }
    }

    // 2. Go-specific shapes.
    if lang == Lang::Go {
        for (i, msg) in lower.iter().enumerate() {
            if NETWORK_PATTERNS.iter().any(|p| msg.contains(p)) {
                return fault("Go 模块下载失败（网络/GOPROXY 不通）", i);
            }
        }
        // `could not import fmt`: the standard library never fails to import
        // in a healthy toolchain.
        let mut third_party = BTreeSet::new();
        let mut first_third_party = None;
        for (i, d) in diags.iter().enumerate() {
            if let Some(pkg) = go_broken_import(&d.message) {
                if go_is_stdlib(pkg) {
                    return fault("Go 标准库无法导入（工具链/构建缓存异常）", i);
                }
                third_party.insert(pkg.to_string());
                first_third_party.get_or_insert(i);
            }
        }
        if third_party.len() >= 3 {
            return fault(
                "大量依赖包无法导入（模块未下载/构建缓存异常）",
                first_third_party?,
            );
        }
    }

    // 3. TypeScript / JavaScript.
    if lang == Lang::Ts {
        let mut modules = BTreeSet::new();
        let mut first_module = None;
        for (i, d) in diags.iter().enumerate() {
            let code = d.code.as_deref().unwrap_or("");
            if code == "2318" || lower[i].starts_with("cannot find global type") {
                return fault("TypeScript 标准库（lib）缺失或 TypeScript 安装损坏", i);
            }
            if code == "2688" || lower[i].starts_with("cannot find type definition file for") {
                return fault("类型声明依赖未安装（node_modules 缺失）", i);
            }
            if code == "2307" || code == "2792" {
                if let Some(m) = quoted(&d.message, '\'') {
                    if !m.starts_with('.') && !m.starts_with('/') {
                        modules.insert(m.to_string());
                        first_module.get_or_insert(i);
                    }
                }
            }
        }
        if modules.len() >= 3 {
            return fault(
                "依赖未安装（多个第三方模块找不到，node_modules 缺失）",
                first_module?,
            );
        }
    }

    // 4. Python: several third-party imports unresolved => wrong interpreter
    // or venv not installed.
    if lang == Lang::Python {
        let mut modules = BTreeSet::new();
        let mut first = None;
        for (i, d) in diags.iter().enumerate() {
            if d.code.as_deref() == Some("reportMissingImports")
                || (lower[i].starts_with("import \"")
                    && lower[i].ends_with("could not be resolved"))
            {
                if let Some(m) = quoted(&d.message, '"') {
                    if !m.starts_with('.') {
                        modules.insert(m.split('.').next().unwrap_or(m).to_string());
                        first.get_or_insert(i);
                    }
                }
            }
        }
        if modules.len() >= 3 {
            return fault(
                "Python 解释器/虚拟环境不对或依赖未安装（多个第三方包无法解析）",
                first?,
            );
        }
    }

    // 5. Generic OS-level failures.
    for (i, msg) in lower.iter().enumerate() {
        if let Some((_, kind)) = GENERIC_PATTERNS.iter().find(|(p, _)| msg.contains(p)) {
            return fault(kind, i);
        }
    }
    None
}

/// Diagnostics that stay trustworthy even when the environment is broken:
/// parser errors do not depend on the toolchain, cache or dependencies.
pub(crate) fn is_syntax_error(diag: &Diagnostic) -> bool {
    let msg = diag.message.trim_start();
    let lower = msg.to_lowercase();
    if lower.starts_with("syntax error") {
        return true;
    }
    match lang_of(diag.path.as_str()) {
        // go/parser: "expected ';', found foo", "expected declaration, ..."
        Lang::Go => lower.starts_with("expected "),
        // TypeScript reserves 1xxx codes for syntax errors.
        Lang::Ts => diag.code.as_deref().is_some_and(|c| {
            c.len() == 4 && c.starts_with('1') && c.bytes().all(|b| b.is_ascii_digit())
        }),
        Lang::Python => diag.code.is_none() && lower.starts_with("expected "),
        _ => false,
    }
}

/// `could not import fmt (missing metadata …)` -> `fmt`.
fn go_broken_import(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("could not import ")?;
    let pkg = rest.split_whitespace().next()?;
    Some(pkg.trim_matches('"'))
}

/// Standard-library import paths have no dot in their first element.
fn go_is_stdlib(pkg: &str) -> bool {
    let first = pkg.split('/').next().unwrap_or(pkg);
    !first.is_empty() && !first.contains('.')
}

fn quoted(message: &str, quote: char) -> Option<&str> {
    let start = message.find(quote)? + 1;
    let len = message[start..].find(quote)?;
    Some(&message[start..start + len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::lsp::{Position, Range, Severity};
    use astrolabe_core::RelPath;

    /// Parse recorded get_diagnostics output lines back into diagnostics.
    pub(crate) fn parse_recorded(text: &str) -> Vec<Diagnostic> {
        text.lines()
            .filter_map(|line| {
                let line = line.strip_prefix('@')?;
                let (loc, rest) = line.split_once(" [")?;
                let (path, lineno) = loc.rsplit_once(':')?;
                let (tag, message) = rest.split_once("] ")?;
                let mut parts = tag.split_whitespace();
                let severity = match parts.next()? {
                    "error" => Severity::Error,
                    "warning" => Severity::Warning,
                    "information" => Severity::Information,
                    _ => Severity::Hint,
                };
                let line_no = lineno.parse::<u32>().ok()?.saturating_sub(1);
                Some(Diagnostic {
                    path: RelPath::new(path),
                    range: Range {
                        start: Position {
                            line: line_no,
                            character: 0,
                        },
                        end: Position {
                            line: line_no,
                            character: 1,
                        },
                    },
                    severity,
                    message: message.to_string(),
                    code: parts.next().map(str::to_string),
                })
            })
            .collect()
    }

    fn fixture(name: &str) -> Vec<Diagnostic> {
        let path = format!("{}/testdata/diag_env/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let diags = parse_recorded(&text);
        assert!(!diags.is_empty(), "{name} parsed to nothing");
        diags
    }

    fn detect_fixture(path: &str, name: &str) -> Option<(EnvFault, String)> {
        let diags = fixture(name);
        detect(path, &diags).map(|f| {
            let root = diags[f.root].message.clone();
            (f, root)
        })
    }

    #[test]
    fn go_recorded_gocache_flood_is_env_fault() {
        let (fault, root) = detect_fixture("args.go", "go_gocache_args.txt").expect("fault");
        assert!(fault.kind.contains("GOCACHE"), "{fault:?}");
        assert!(
            root.starts_with("loading compiled Go files from cache"),
            "{root}"
        );
        assert!(fault.hint.contains("go build"));
    }

    #[test]
    fn go_recorded_stdlib_broken_import_is_env_fault() {
        let (fault, root) =
            detect_fixture("args_test.go", "go_broken_import_args_test.txt").expect("fault");
        assert!(root.starts_with("could not import fmt"), "{root}");
        assert!(
            fault.kind.contains("元数据") || fault.kind.contains("标准库"),
            "{fault:?}"
        );
    }

    #[test]
    fn go_readonly_cache_and_network() {
        let (fault, _) = detect_fixture("main.go", "go_ro_gocache_build.txt").expect("ro");
        assert!(fault.kind.contains("GOCACHE"), "{fault:?}");
        let (fault, root) = detect_fixture("main.go", "go_goproxy_network.txt").expect("net");
        assert!(fault.kind.contains("网络"), "{fault:?}");
        assert!(root.contains("dial tcp"));
    }

    #[test]
    fn go_real_errors_are_not_env_faults() {
        assert_eq!(detect_fixture("main.go", "go_real_errors.txt"), None);
        // go.sum drift is a real, fixable project error (go mod tidy).
        assert_eq!(detect_fixture("main.go", "go_missing_gosum.txt"), None);
    }

    #[test]
    fn go_mass_third_party_broken_imports() {
        let diags = parse_recorded(
            "@a.go:3 [error BrokenImport] could not import github.com/a/b (no required module provides package)\n\
             @a.go:4 [error BrokenImport] could not import github.com/c/d (no required module provides package)\n\
             @a.go:5 [error BrokenImport] could not import golang.org/x/e (no required module provides package)\n",
        );
        let fault = detect("a.go", &diags).expect("fault");
        assert_eq!(fault.root, 0);
        let two = &diags[..2];
        assert_eq!(
            detect("a.go", two),
            None,
            "one or two missing deps can be real"
        );
    }

    #[test]
    fn ts_env_faults_and_real_errors() {
        let (fault, root) = detect_fixture("a.ts", "ts_node_modules_missing.txt").expect("nm");
        assert!(fault.kind.contains("node_modules"), "{fault:?}");
        assert!(root.contains("'express'"));
        let (fault, _) = detect_fixture("a.ts", "ts_nolib.txt").expect("nolib");
        assert!(fault.kind.contains("lib"), "{fault:?}");
        assert_eq!(detect_fixture("a.ts", "ts_real_errors.txt"), None);
    }

    #[test]
    fn python_env_faults_and_real_errors() {
        let (fault, root) = detect_fixture("a.py", "py_interpreter_missing.txt").expect("py");
        assert!(fault.kind.contains("解释器"), "{fault:?}");
        assert!(root.contains("numpy"), "{root}");
        assert_eq!(detect_fixture("a.py", "py_real_errors.txt"), None);
    }

    #[test]
    fn rust_env_faults_and_real_errors() {
        let (fault, _) = detect_fixture("src/main.rs", "rust_sysroot_missing.txt").expect("rs");
        assert!(fault.kind.contains("标准库"), "{fault:?}");
        assert_eq!(detect_fixture("src/main.rs", "rust_real_errors.txt"), None);
    }

    #[derive(Debug)]
    struct Recorded(Vec<Diagnostic>);

    impl crate::precise_tools::PreciseCapability for Recorded {
        fn references(
            &self,
            _symbol: &str,
            _path: Option<&str>,
            _position: Option<Position>,
            _exact: bool,
        ) -> astrolabe_core::lsp::Precise<Vec<astrolabe_core::lsp::Location>> {
            astrolabe_core::lsp::Precise::exact(Vec::new())
        }
        fn definition(
            &self,
            _path: &str,
            _symbol: Option<&str>,
            _line: Option<u32>,
            _character: Option<u32>,
        ) -> astrolabe_core::lsp::Precise<Vec<astrolabe_core::lsp::Location>> {
            astrolabe_core::lsp::Precise::exact(Vec::new())
        }
        fn diagnostics(&self, _path: &str) -> astrolabe_core::lsp::Precise<Vec<Diagnostic>> {
            astrolabe_core::lsp::Precise::exact(self.0.clone())
        }
    }

    fn run_tool(path: &str, diags: Vec<Diagnostic>) -> (String, serde_json::Value) {
        let params = serde_json::from_value(serde_json::json!({ "path": path })).unwrap();
        let result = crate::precise_tools::run_get_diagnostics(params, &Recorded(diags));
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        (text, result.structured_content.unwrap_or_default())
    }

    #[test]
    fn tool_output_for_recorded_e2_floods_is_short() {
        // E2-Cnew-r1 T14: 10027 B of BrokenImport + undefined cascade.
        let (text, meta) = run_tool("args_test.go", fixture("go_broken_import_args_test.txt"));
        assert!(text.contains("环境异常，诊断不可信"), "{text}");
        assert!(
            text.contains("根因：@args_test.go:18 [error BrokenImport] could not import fmt"),
            "{text}"
        );
        assert!(text.contains("go build"), "{text}");
        assert!(!text.contains("undefined: executeCommand"), "{text}");
        assert!(text.len() < 700, "{} bytes: {text}", text.len());
        assert_eq!(meta["confidence"], "unknown");
        assert_eq!(meta["suppressed"], 152);
        assert_eq!(meta["matches"], 0);

        // E2-Cnew-r1 T13: 928 B, GOCACHE miss + 12 undefined.
        let (text, meta) = run_tool("args.go", fixture("go_gocache_args.txt"));
        assert!(
            text.contains("根因：@args.go:1 [error] loading compiled Go files from cache"),
            "{text}"
        );
        assert!(text.contains("另有 12 条"), "{text}");
        assert_eq!(meta["suppressed"], 12);
    }

    #[test]
    fn tool_keeps_syntax_errors_and_real_output() {
        let mut diags = fixture("go_gocache_args.txt");
        diags.extend(parse_recorded(
            "@args.go:40 [error] expected ';', found foo\n",
        ));
        let (text, meta) = run_tool("args.go", diags);
        assert!(text.contains("仍可信"), "{text}");
        assert!(
            text.contains("@args.go:40 [error] expected ';', found foo"),
            "{text}"
        );
        assert_eq!(meta["matches"], 1);

        // Healthy environment: untouched, every diagnostic listed.
        let (text, meta) = run_tool("main.go", fixture("go_real_errors.txt"));
        assert!(!text.contains("环境异常"), "{text}");
        assert!(text.contains("undefined: Commnd"), "{text}");
        assert_eq!(meta["matches"], 4);
        assert!(meta.get("environment_fault").is_none());
    }

    #[test]
    fn syntax_errors_survive() {
        let go = fixture("go_real_errors.txt");
        let kept: Vec<_> = go.iter().filter(|d| is_syntax_error(d)).collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].message.starts_with("expected ';'"));
        let ts = fixture("ts_real_errors.txt");
        assert_eq!(ts.iter().filter(|d| is_syntax_error(d)).count(), 1);
    }
}
