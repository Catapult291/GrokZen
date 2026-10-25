//! Locate a language-server binary before anyone tries to start one.
//!
//! A missing server is a **reportable state** ([`LspError::Unavailable`]), not
//! silence and not a guessed fallback. Every [`ServerSpec`] therefore carries
//! an [`ServerSpec::install_hint`] the agent can show the user.
//!
//! ## Discovery ladder
//!
//! 1. `ASTROLABE_LSP_*` env override
//! 2. system `PATH`
//! 3. local cache under `~/.astrolabe/servers/{name}/{version}/`
//! 4. if [`crate::lsp::installer::AutoInstallMode::On`], [`ensure_installed`]
//! 5. else [`LspError::Unavailable`] with a structured [`NeedsInstall`] hint
//!
//! [`ensure_installed`] is **never** called unless mode is `On`
//! (`ASTROLABE_AUTO_INSTALL=on`). Default mode is `prompt`.
//!
//! ## Testability
//!
//! Lookups take an explicit [`Discovery`] (PATH directories + override map).
//! Production code calls [`discover`] / [`probe_report`], which snapshot the
//! process environment once. Tests inject a fake PATH built under
//! `std::env::temp_dir()` and **must not** call `std::env::set_var`, so they
//! stay isolated from other parallel tests. `Discovery::new` disables cache
//! probing and auto-install so existing unit tests stay hermetic.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::types::Language;

use super::installer::{
    self, ensure_installed_with, AutoInstallMode, BackgroundInstall, InstallContext, InstallSpec,
    Installs, NeedsInstall, UreqFetcher,
};
use super::{LspError, ServerSpec};

/// Languages this module probes, in the order MCP reports should list them.
pub const PROBED_LANGUAGES: &[Language] = &[
    Language::Python,
    Language::Go,
    Language::Java,
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::C,
    Language::Cpp,
    Language::ObjC,
    Language::ObjCpp,
    Language::Swift,
    Language::Php,
    Language::Vue,
    Language::Dart,
    Language::CSharp,
    Language::VisualBasic,
    Language::Kotlin,
    Language::Shell,
    Language::Ruby,
    Language::Lua,
];

/// Snapshot of PATH and override variables used for one probe.
///
/// Constructed once, then reused. Never mutates process-global environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    path_dirs: Vec<PathBuf>,
    env: BTreeMap<String, String>,
    windows: bool,
    /// `None` skips cache probing (default for [`Discovery::new`] / tests).
    cache_root: Option<PathBuf>,
    auto_install: AutoInstallMode,
    /// Install roots searched for an unpacked kotlin-lsp distribution when
    /// none is on PATH. Empty for [`Discovery::new`] (hermetic tests).
    kotlin_roots: Vec<PathBuf>,
    /// How long a probe waits for a background auto-install before answering
    /// "in progress". Zero (default): never wait inside the probe.
    install_wait: std::time::Duration,
    /// Working directory for the rustup-proxy check (`rust-toolchain.toml`
    /// is resolved there); `None` uses the process cwd.
    project_root: Option<PathBuf>,
}

/// Registry key of one artifact install.
fn install_key(spec: &InstallSpec, cache_root: &Path) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        cache_root.display(),
        spec.name,
        spec.version
    )
}

/// One language's probe outcome. Missing is a row, not an omitted row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub language: Language,
    /// First available server, if any.
    pub spec: Option<ServerSpec>,
    /// Always non-empty. When `spec` is `None`, this is how to install; when
    /// `Some`, it repeats the chosen server's hint.
    pub install_hint: String,
    /// Structured install payload for MCP when the binary is missing.
    pub needs_install: Option<NeedsInstall>,
}

impl ProbeResult {
    pub fn available(&self) -> bool {
        self.spec.is_some()
    }

    /// One-line status for MCP tools.
    pub fn summary(&self) -> String {
        match &self.spec {
            Some(spec) if spec.args.is_empty() => {
                format!("{}: {}", self.language.name(), spec.command.display())
            }
            Some(spec) => format!(
                "{}: {} {}",
                self.language.name(),
                spec.command.display(),
                spec.args.join(" ")
            ),
            None => format!(
                "{}: unavailable. {}",
                self.language.name(),
                self.install_hint
            ),
        }
    }
}

struct Candidate {
    bin: &'static str,
    args: &'static [&'static str],
    install: &'static str,
}

const PYTHON: &[Candidate] = &[
    Candidate {
        bin: "pyright-langserver",
        args: &["--stdio"],
        install: "npm i -g pyright",
    },
    Candidate {
        bin: "basedpyright-langserver",
        args: &["--stdio"],
        install: "npm i -g basedpyright",
    },
    Candidate {
        bin: "pylsp",
        args: &[],
        install: "pip install python-lsp-server",
    },
    Candidate {
        bin: "jedi-language-server",
        args: &[],
        install: "pip install jedi-language-server",
    },
    Candidate {
        bin: "ruff",
        args: &["server"],
        install: "pip install ruff",
    },
    Candidate {
        bin: "ruff-lsp",
        args: &[],
        install: "pip install ruff-lsp  # deprecated; prefer `pip install ruff` then `ruff server`",
    },
];

const GO: &[Candidate] = &[Candidate {
    bin: "gopls",
    args: &[],
    install: "go install golang.org/x/tools/gopls@latest",
}];

const JAVA: &[Candidate] = &[Candidate {
    bin: "jdtls",
    args: &[],
    install: "brew install jdtls",
}];

const RUST: &[Candidate] = &[Candidate {
    bin: "rust-analyzer",
    args: &[],
    install: "rustup component add rust-analyzer",
}];

const TYPESCRIPT: &[Candidate] = &[
    Candidate {
        bin: "typescript-language-server",
        args: &["--stdio"],
        install: "npm i -g typescript-language-server typescript@5  # TypeScript 7 (native) ships no tsserver.js; keep a 5.x next to the server or set ASTROLABE_TSSERVER_PATH",
    },
    Candidate {
        bin: "vtsls",
        args: &["--stdio"],
        install: "npm i -g @vtsls/language-server",
    },
];

const PHP: &[Candidate] = &[
    Candidate {
        bin: "intelephense",
        args: &["--stdio"],
        // Free features work without a key. Premium requires a purchased
        // INTELEPHENSE_LICENSE_KEY (LSP initializationOptions.licenceKey).
        // Do not pirate or share paid license keys.
        install: "npm i -g intelephense  # free tier works without a key; set a purchased INTELEPHENSE_LICENSE_KEY for premium (no pirated keys)",
    },
    Candidate {
        bin: "phpactor",
        args: &["language-server"],
        install: "composer global require phpactor/phpactor  # MIT; needs PHP 8.1+. Or download phpactor.phar from https://github.com/phpactor/phpactor/releases and run: php phpactor.phar language-server",
    },
];

const C_FAMILY: &[Candidate] = &[
    Candidate {
        bin: "clangd",
        // Serena launch: `--background-index` so cross-file refs work.
        args: &["--background-index"],
        install: "clangd not found on PATH. Install LLVM clangd, then ensure `clangd` is on PATH:\n  \
macOS:  brew install llvm  # add $(brew --prefix llvm)/bin to PATH\n  \
Ubuntu/Debian: sudo apt-get install clangd\n  \
Fedora/RHEL: sudo dnf install clang-tools-extra\n  \
Arch: sudo pacman -S clang\n  \
Or download a release from https://clangd.llvm.org/installation\n\
Prefer a compile_commands.json at the repo root (CMake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON).\n\
Override with ASTROLABE_LSP_CXX=/path/to/clangd (or ASTROLABE_LSP_C).\n\
With ASTROLABE_AUTO_INSTALL=on, Astrolabe can download a pinned clangd into ~/.astrolabe/servers/.",
    },
    Candidate {
        bin: "ccls",
        args: &[],
        install: "ccls (clangd alternative): brew install ccls  # or see https://github.com/MaskRay/ccls/wiki/Build\n\
Also accepts compile_commands.json at the repo root.\n\
Override with ASTROLABE_LSP_CXX=/path/to/ccls.",
    },
];

/// Swift — sourcekit-lsp only (RFC primary; ships with Xcode / CLT).
const APPLE_SWIFT: &[Candidate] = &[Candidate {
    bin: "sourcekit-lsp",
    args: &[],
    install: "xcode-select --install  # Xcode CLT ships sourcekit-lsp; or install full Xcode / Swift toolchain",
}];

/// ObjC/ObjC++ — clangd first, sourcekit-lsp second.
///
/// sourcekit-lsp only forwards ObjC to the clangd it bundles, and for a
/// SwiftPM/Xcode-less checkout it has no flags to give it. Astrolabe starts
/// clangd directly so it can hand it a synthetic `compile_commands.json`
/// (`-x objective-c`, `-fobjc-arc`, header search paths; see `apple.rs`).
const APPLE_OBJC: &[Candidate] = &[
    Candidate {
        bin: "clangd",
        args: &["--background-index"],
        install: "brew install llvm  # or Xcode CLT (`xcode-select --install`); Linux: apt-get install clangd",
    },
    Candidate {
        bin: "sourcekit-lsp",
        args: &[],
        install: "xcode-select --install  # Xcode CLT ships sourcekit-lsp; or install full Xcode",
    },
];

// Volar 2.x runs alone (hybridMode=false). Volar 3.x is detected and run as
// a dual server by `lsp::vue_bridge` (typescript-language-server with
// @vue/typescript-plugin, relaying Volar's `tsserver/request`).
const VUE: &[Candidate] = &[Candidate {
    bin: "vue-language-server",
    args: &["--stdio"],
    install: "npm i -g @vue/language-server typescript-language-server typescript  # Volar 3.x is bridged to typescript-language-server + @vue/typescript-plugin; 2.x runs with hybridMode=false",
}];

/// Kotlin — JetBrains kotlin-lsp first, fwcd kotlin-language-server fallback.
///
/// `intellij-server` is the launcher name inside the standalone archive since
/// v262.4739 (`kotlin-lsp.sh` is a deprecated forwarder); Homebrew links it as
/// `kotlin-lsp`. Unpacked archives and editor-extension copies that are not
/// on PATH are found through [`super::kotlin::well_known_roots`].
const KOTLIN: &[Candidate] = &[
    Candidate {
        bin: "kotlin-lsp",
        args: &["--stdio"],
        install: KOTLIN_LSP_INSTALL,
    },
    Candidate {
        bin: "intellij-server",
        args: &["--stdio"],
        install: KOTLIN_LSP_INSTALL,
    },
    Candidate {
        bin: "kotlin-language-server",
        args: &[],
        install: "kotlin-language-server (fwcd): brew install kotlin-language-server  # or https://github.com/fwcd/kotlin-language-server/releases",
    },
];

const KOTLIN_LSP_INSTALL: &str = "kotlin-lsp (JetBrains): brew install JetBrains/utils/kotlin-lsp  # or unpack the standalone archive from https://github.com/Kotlin/kotlin-lsp/releases and set ASTROLABE_LSP_KOTLIN to the unpacked directory or its bin/intellij-server. First open of a Gradle project imports it (minutes on large repos; cached afterwards); Astrolabe answers from its syntax index meanwhile";

const SHELL: &[Candidate] = &[Candidate {
    bin: "bash-language-server",
    args: &["start"],
    install: "npm i -g bash-language-server  # optional: shellcheck on PATH for diagnostics; set ASTROLABE_LSP_SHELL",
}];

const RUBY: &[Candidate] = &[
    Candidate {
        bin: "ruby-lsp",
        // The launcher boots in degraded mode instead of exiting 78 when a
        // repo has a Gemfile but no Gemfile.lock (fresh clones, gems that
        // never commit the lock), and when some bundle gems fail to install.
        args: &["--use-launcher"],
        install: "gem install ruby-lsp  # set ASTROLABE_LSP_RUBY",
    },
    Candidate {
        bin: "solargraph",
        args: &["stdio"],
        install: "gem install solargraph",
    },
];

const LUA: &[Candidate] = &[Candidate {
    bin: "lua-language-server",
    args: &[],
    install: "brew install lua-language-server  # or https://github.com/LuaLS/lua-language-server/releases; set ASTROLABE_LSP_LUA",
}];

const DART: &[Candidate] = &[Candidate {
    bin: "dart",
    args: &["language-server"],
    install:
        "dart language-server ships with the Dart SDK (https://dart.dev/get-dart); a Flutter SDK also provides it",
}];

/// Roslyn LS is a DLL launched via `dotnet <dll> --stdio`. The "bin" is the
/// DLL file name discovered on PATH (file presence, not +x). OmniSharp and
/// community csharp-ls are intentionally not candidates.
const CSHARP: &[Candidate] = &[Candidate {
    bin: "Microsoft.CodeAnalysis.LanguageServer.dll",
    args: &["--stdio"],
    install: super::csharp::CSHARP_INSTALL_HINT,
}];

const JAVA_UNAVAILABLE: &str = "\
jdtls not found on PATH. Eclipse JDT Language Server (eclipse.jdt.ls) is the \
mainstream Java LSP, but it is not a single static binary.

Install the wrapper so `jdtls` is on PATH (stdio is the default transport):
  brew install jdtls
  # or download a release from https://github.com/eclipse-jdtls/eclipse.jdt.ls/releases \
and add its bin/ directory to PATH (requires Java 17+)

A unique per-project workspace directory is required at launch:
  jdtls -data /path/to/unique/workspace

Astrolabe does not auto-assemble the raw JVM launch, which looks like:
  java -Declipse.application=org.eclipse.jdt.ls.core.id1 \\
       -Dosgi.bundles.defaultStartLevel=4 \\
       -Declipse.product=org.eclipse.jdt.ls.core.product \\
       -jar plugins/org.eclipse.equinox.launcher_<version>.jar \\
       -configuration ./config_{linux|mac|win} \\
       -data /path/to/unique/workspace
The launcher jar version, OS-specific config directory, and a unique -data \
path must match the unpacked distribution; a shared -data directory corrupts \
the index.

Override with ASTROLABE_LSP_JAVA=/path/to/jdtls";

/// `off` keeps a private gopls per session instead of the shared daemon.
pub const GOPLS_REMOTE_ENV: &str = "ASTROLABE_GOPLS_REMOTE";

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

impl Discovery {
    /// Snapshot `PATH` and `ASTROLABE_LSP_*` from the current process.
    pub fn from_process() -> Self {
        let mut env = BTreeMap::new();
        for key in override_keys_all()
            .iter()
            .copied()
            .chain(["PATHEXT", GOPLS_REMOTE_ENV])
        {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.to_string(), value);
            }
        }
        Self {
            path_dirs: parse_path(std::env::var_os("PATH").unwrap_or_default()),
            env,
            windows: cfg!(windows),
            cache_root: Some(installer::default_servers_dir()),
            auto_install: AutoInstallMode::from_env(),
            kotlin_roots: super::kotlin::well_known_roots(
                home_dir().as_deref(),
                |k| std::env::var(k).ok(),
                cfg!(windows),
            ),
            install_wait: std::time::Duration::ZERO,
            project_root: None,
        }
    }

    /// Probe against an explicit PATH string (same encoding as the `PATH` env var).
    ///
    /// Hermetic for tests: no cache root, auto-install [`AutoInstallMode::Off`].
    pub fn new(path: impl AsRef<OsStr>) -> Self {
        Self {
            path_dirs: parse_path(path),
            env: BTreeMap::new(),
            windows: cfg!(windows),
            cache_root: None,
            auto_install: AutoInstallMode::Off,
            kotlin_roots: Vec::new(),
            install_wait: std::time::Duration::ZERO,
            project_root: None,
        }
    }

    /// Add or replace one environment variable (overrides or `PATHEXT`).
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Project root used as the cwd when checking a rustup proxy.
    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = Some(root.into());
        self
    }

    /// `Some(hint)` when `command` (found for `language`) is a rustup proxy
    /// that cannot run, see `rustup_proxy.rs`. Rust only.
    fn broken_proxy(&self, language: Language, command: &Path) -> Option<String> {
        (language == Language::Rust)
            .then(|| super::rustup_proxy::proxy_problem(command, self.project_root.as_deref()))
            .flatten()
    }

    /// Add an install root searched for a kotlin-lsp distribution.
    pub fn with_kotlin_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.kotlin_roots.push(root.into());
        self
    }

    /// Enable / override the servers cache root used by the discovery ladder.
    pub fn with_cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }

    /// Override [`AutoInstallMode`] (tests / session policy).
    pub fn with_auto_install(mut self, mode: AutoInstallMode) -> Self {
        self.auto_install = mode;
        self
    }

    /// Let a probe wait up to `wait` for a background auto-install.
    pub fn with_install_wait(mut self, wait: std::time::Duration) -> Self {
        self.install_wait = wait;
        self
    }

    pub fn auto_install_mode(&self) -> AutoInstallMode {
        self.auto_install
    }

    pub fn cache_root(&self) -> Option<&Path> {
        self.cache_root.as_deref()
    }

    /// First available [`ServerSpec`] for `language`, or [`LspError::Unavailable`].
    pub fn discover(&self, language: Language) -> Result<ServerSpec, LspError> {
        let probed = self.probe(language);
        match probed.spec {
            Some(spec) => Ok(spec),
            None => Err(LspError::Unavailable(language, probed.install_hint)),
        }
    }

    /// Probe one language. Absence is `spec: None`, never an error.
    ///
    /// Ladder: env override → PATH → cache → (if On) install → Unavailable hint.
    pub fn probe(&self, language: Language) -> ProbeResult {
        let mut probed = self.probe_ladder(language);
        if let Some(spec) = probed.spec.as_mut() {
            self.share_gopls(spec);
        }
        probed
    }

    /// gopls: run as a forwarder to the shared per-user daemon
    /// (`-remote=auto`), so several Astrolabe sessions on one machine share
    /// one gopls (and its type-checked packages) instead of one each. The
    /// daemon exits about a minute after its last client disconnects.
    /// `ASTROLABE_GOPLS_REMOTE=off` keeps a private gopls per session; any
    /// other non-empty value is passed through as the `-remote` address.
    fn share_gopls(&self, spec: &mut ServerSpec) {
        if spec.language != Language::Go
            || bin_stem(&spec.command) != "gopls"
            || spec.args.iter().any(|a| a.starts_with("-remote"))
        {
            return;
        }
        let setting = self
            .env
            .get(GOPLS_REMOTE_ENV)
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        let remote = match setting.to_ascii_lowercase().as_str() {
            "" | "auto" | "on" | "1" | "true" | "yes" => "auto".to_string(),
            "off" | "0" | "false" | "no" | "none" => return,
            _ => setting,
        };
        spec.args.insert(0, format!("-remote={remote}"));
    }

    fn probe_ladder(&self, language: Language) -> ProbeResult {
        if matches!(language, Language::CSharp | Language::VisualBasic) {
            return self.probe_roslyn(language);
        }
        if let Some((key, value)) = self.override_value(language) {
            let resolved = self.resolve_override(&value).or_else(|| {
                (language == Language::Kotlin && Path::new(&value).is_dir())
                    .then(|| super::kotlin::launcher_in(Path::new(&value), self.windows))
                    .flatten()
            });
            match resolved {
                Some(command) => {
                    if let Some(hint) = self.broken_proxy(language, &command) {
                        return self.unavailable(language, hint);
                    }
                    let spec = spec_for_resolved(language, command);
                    let spec = self.kotlin_upgrade(spec);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
                None => {
                    let hint = format!(
                        "{key} is set to `{value}` but that executable was not found. {}",
                        self.missing_hint(language)
                    );
                    return self.unavailable(language, hint);
                }
            }
        }

        // A rustup proxy that cannot run: keep looking (cache, install), and
        // say why if nothing else is found.
        let mut proxy_hint = None;
        for candidate in candidates(language) {
            if candidate.bin == "kotlin-language-server" {
                // Unpacked JetBrains distributions beat the fwcd fallback.
                if let Some(found) = self.probe_kotlin_roots() {
                    return found;
                }
            }
            if let Some(command) = self.find_on_path(candidate.bin) {
                if let Some(hint) = self.broken_proxy(language, &command) {
                    proxy_hint = Some(hint);
                    continue;
                }
                let spec = spec_from_candidate(language, command, candidate);
                let spec = self.kotlin_upgrade(spec);
                let install_hint = spec.install_hint.clone();
                return ProbeResult {
                    language,
                    spec: Some(spec),
                    install_hint,
                    needs_install: None,
                };
            }
        }

        // Cache: ~/.astrolabe/servers/{name}/{version}/…
        if let Some(cache_root) = &self.cache_root {
            for candidate in candidates(language) {
                if let Some(command) = installer::find_cached_binary(
                    cache_root,
                    candidate.bin,
                    Path::new(candidate.bin),
                ) {
                    let spec = spec_from_candidate(language, command, candidate);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
                // Also try common bin/ layout used by packaged releases.
                let rel = PathBuf::from("bin").join(candidate.bin);
                if let Some(command) =
                    installer::find_cached_binary(cache_root, candidate.bin, &rel)
                {
                    let spec = spec_from_candidate(language, command, candidate);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
            }
        }

        // Auto-install only when mode is On (never for Prompt/Off). The
        // download runs on a background thread (once per artifact, see
        // `installer::Installs`): this probe runs inside the pool's start gate
        // and inside `get_languages`, which must not wait minutes on it.
        if self.auto_install == AutoInstallMode::On {
            if let Some(install_spec) = catalog_install_spec(language) {
                if let Some(cache_root) = &self.cache_root {
                    match self.background_install(&install_spec, cache_root) {
                        BackgroundInstall::Ready(command) => {
                            let cands = candidates(language);
                            let candidate = cands.first().expect("catalog langs have candidates");
                            let spec = spec_from_candidate(language, command, candidate);
                            let install_hint = spec.install_hint.clone();
                            return ProbeResult {
                                language,
                                spec: Some(spec),
                                install_hint,
                                needs_install: None,
                            };
                        }
                        BackgroundInstall::Pending { elapsed } => {
                            let hint = format!(
                                "{} of {} (started {}s ago); the language server starts once \
                                 it finishes. {}",
                                installer::INSTALL_PENDING,
                                install_spec.name,
                                elapsed.as_secs(),
                                self.missing_hint(language)
                            );
                            return self.unavailable(language, hint);
                        }
                        BackgroundInstall::Failed(err) => {
                            let hint = format!(
                                "auto-install of {} failed: {err}. {}",
                                install_spec.name,
                                self.missing_hint(language)
                            );
                            return self.unavailable(language, hint);
                        }
                    }
                }
            }
        }

        let hint = proxy_hint.unwrap_or_else(|| self.missing_hint(language));
        self.unavailable(language, hint)
    }

    /// Poll (or start) the background install of `spec` under `cache_root`.
    fn background_install(&self, spec: &InstallSpec, cache_root: &Path) -> BackgroundInstall {
        let key = install_key(spec, cache_root);
        let spec = spec.clone();
        let cache_root = cache_root.to_path_buf();
        Installs::global().poll_or_start(&key, self.install_wait, move || {
            let fetcher = UreqFetcher::default();
            let ctx = InstallContext {
                cache_root,
                fetcher: &fetcher,
                offline: std::env::var(installer::ENV_OFFLINE)
                    .map(|v| {
                        matches!(
                            v.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    })
                    .unwrap_or(false),
                mirror_github: std::env::var(installer::ENV_MIRROR_GITHUB)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                lock_timeout: std::time::Duration::from_secs(120),
            };
            ensure_installed_with(&spec, &ctx).map_err(|e| e.to_string())
        })
    }

    /// Upgrade a kotlin-lsp spec that points at the deprecated wrapper (or a
    /// symlink to it) to the `bin/intellij-server` next to it. Other
    /// languages pass through untouched.
    fn kotlin_upgrade(&self, mut spec: ServerSpec) -> ServerSpec {
        if spec.language != Language::Kotlin || !super::kotlin::is_jetbrains_server(&spec.command) {
            return spec;
        }
        spec.command = super::kotlin::prefer_launcher(&spec.command, self.windows);
        if spec.args.is_empty() {
            spec.args = vec!["--stdio".to_string()];
        }
        spec
    }

    /// kotlin-lsp unpacked somewhere off PATH (archive, Homebrew libexec,
    /// editor extension, Astrolabe's own server cache).
    fn probe_kotlin_roots(&self) -> Option<ProbeResult> {
        let cache = self
            .cache_root
            .as_ref()
            .map(|c| vec![c.join("kotlin-lsp"), c.join("kotlin-server")])
            .unwrap_or_default();
        let command = cache
            .iter()
            .chain(self.kotlin_roots.iter())
            .find_map(|root| super::kotlin::launcher_in(root, self.windows))?;
        let spec = self.kotlin_upgrade(ServerSpec {
            language: Language::Kotlin,
            command,
            args: vec!["--stdio".to_string()],
            install_hint: KOTLIN_LSP_INSTALL.to_string(),
        });
        Some(ProbeResult {
            language: Language::Kotlin,
            install_hint: spec.install_hint.clone(),
            spec: Some(spec),
            needs_install: None,
        })
    }

    fn unavailable(&self, language: Language, hint: String) -> ProbeResult {
        let server = primary_bin(language);
        let cache_root = self
            .cache_root
            .clone()
            .unwrap_or_else(installer::default_servers_dir);
        let needs = installer::needs_install_for(
            language,
            hint.clone(),
            server,
            catalog_install_spec(language),
            self.auto_install,
            &cache_root,
        );
        ProbeResult {
            language,
            spec: None,
            install_hint: needs.message(),
            needs_install: Some(needs),
        }
    }

    /// Probe every supported language. Order is [`PROBED_LANGUAGES`].
    pub fn probe_report(&self) -> Vec<ProbeResult> {
        PROBED_LANGUAGES
            .iter()
            .map(|lang| self.probe(*lang))
            .collect()
    }

    fn override_value(&self, language: Language) -> Option<(String, String)> {
        for key in env_override_keys(language) {
            if let Some(value) = self.env.get(*key) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(((*key).to_string(), trimmed.to_string()));
                }
            }
        }
        None
    }

    fn resolve_override(&self, value: &str) -> Option<PathBuf> {
        let path = Path::new(value);
        if let Some(found) = self.existing_command(path) {
            return Some(found);
        }
        if is_bare_name(path) {
            return self.find_on_path(value);
        }
        None
    }

    fn existing_command(&self, path: &Path) -> Option<PathBuf> {
        if let Some(found) = self.runnable_path(path) {
            return Some(found);
        }
        if self.windows && path.extension().is_none() {
            for ext in self.pathext() {
                let with_ext = path.with_extension(ext.trim_start_matches('.'));
                if let Some(found) = self.runnable_path(&with_ext) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn find_on_path(&self, bin: &str) -> Option<PathBuf> {
        let names = command_names(bin, self.windows, &self.pathext_joined());
        for dir in &self.path_dirs {
            for name in &names {
                let candidate = dir.join(name);
                if let Some(found) = self.runnable_path(&candidate) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Like [`is_runnable`], but when `windows` is set also accepts a
    /// case-insensitive filename match in the same directory.
    ///
    /// Real Windows PATH search is case-insensitive. Tests force
    /// `Discovery.windows = true` on Linux CI, where the filesystem is
    /// case-sensitive and `PATHEXT` defaults to uppercase (`.CMD`) while
    /// shims are often written as `.cmd`. Without this fallback the
    /// `windows_cmd_suffix_is_accepted` unit test fails on Linux runners.
    fn runnable_path(&self, path: &Path) -> Option<PathBuf> {
        if is_runnable(path) {
            return Some(path.to_path_buf());
        }
        if !self.windows {
            return None;
        }
        let name = path.file_name()?.to_string_lossy();
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
        let entries = std::fs::read_dir(parent).ok()?;
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            if !entry_name
                .to_string_lossy()
                .eq_ignore_ascii_case(name.as_ref())
            {
                continue;
            }
            let candidate = entry.path();
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn pathext_joined(&self) -> String {
        self.env
            .get("PATHEXT")
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_PATHEXT)
            .to_string()
    }

    fn pathext(&self) -> Vec<String> {
        parse_pathext(&self.pathext_joined())
    }

    fn missing_hint(&self, language: Language) -> String {
        if language == Language::Java {
            return JAVA_UNAVAILABLE.to_string();
        }
        let cands = candidates(language);
        if cands.is_empty() {
            let key = env_override_keys(language)
                .first()
                .copied()
                .unwrap_or("ASTROLABE_LSP");
            return format!(
                "AST-only: no language-server discovery candidates wired for {}.                  Or set {} to an executable when support lands.",
                language.name(),
                key
            );
        }
        let bins: Vec<&str> = cands.iter().map(|c| c.bin).collect();
        let installs: Vec<&str> = cands.iter().map(|c| c.install).collect();
        let extra = match language {
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                " TypeScript 7's native `tsc --lsp --stdio` is not auto-detected \
                  because older `tsc` binaries do not speak LSP; set \
                  ASTROLABE_LSP_TYPESCRIPT to that executable if you want it."
            }
            Language::Php => {
                " Intelephense premium features need a purchased INTELEPHENSE_LICENSE_KEY;                   never use a pirated key. Phpactor is MIT and needs PHP 8.1+."
            }
            Language::Vue => {
                " Volar (`@vue/language-server`) is the P0 Vue server; 3.x is run as a bridge: typescript-language-server with @vue/typescript-plugin answers script queries and relays Volar's tsserver requests."
            }
            Language::Swift => {
                " sourcekit-lsp is the RFC-primary Swift server; it ships with                   Xcode / Command Line Tools (`xcode-select --install`)."
            }
            Language::ObjC | Language::ObjCpp => {
                " clangd is the primary ObjC server: Astrolabe writes a synthetic                   compile_commands.json (ObjC flags, header paths) when the repo has none,                   and ObjC-looking .h headers are routed here too. sourcekit-lsp is the fallback."
            }
            Language::Dart => {
                " dart language-server 随 Dart SDK 自带；Flutter 工程安装 Flutter SDK 即可获得。"
            }
            Language::CSharp | Language::VisualBasic => {
                " Roslyn Microsoft.CodeAnalysis.LanguageServer via `dotnet <dll> --stdio`; requires .NET 10 runtime to run the server (analyzed projects may target older frameworks). Not OmniSharp; not community csharp-ls. Visual Basic uses the same server and ASTROLABE_LSP_CSHARP."
            }
            _ => "",
        };
        format!(
            "none of {} found on PATH. Install one of:\n  {}\nOr set {} to the executable.{}",
            bins.join(", "),
            installs.join("\n  "),
            env_override_keys(language)[0],
            extra
        )
    }

    /// C#: `ASTROLABE_LSP_CSHARP` (DLL or bare name) then a user-installed
    /// `Microsoft.CodeAnalysis.LanguageServer.dll` on PATH. Launch is always
    /// `dotnet <dll> --stdio`. No OmniSharp / csharp-ls / silent download.
    fn probe_roslyn(&self, language: Language) -> ProbeResult {
        if let Some((key, value)) = self.override_value(language) {
            match self.resolve_csharp_dll(&value) {
                Some(dll) => match self.require_dotnet_for_roslyn(language, &dll) {
                    Ok(spec) => {
                        let install_hint = spec.install_hint.clone();
                        return ProbeResult {
                            language,
                            spec: Some(spec),
                            install_hint,
                            needs_install: None,
                        };
                    }
                    Err(hint) => return self.unavailable(language, hint),
                },
                None => {
                    let hint = format!(
                        "{key} is set to `{value}` but that Roslyn language-server DLL was not found. {}",
                        self.missing_hint(language)
                    );
                    return self.unavailable(language, hint);
                }
            }
        }

        for candidate in candidates(language) {
            if let Some(dll) = self.find_file_on_path(candidate.bin) {
                match self.require_dotnet_for_roslyn(language, &dll) {
                    Ok(spec) => {
                        let install_hint = spec.install_hint.clone();
                        return ProbeResult {
                            language,
                            spec: Some(spec),
                            install_hint,
                            needs_install: None,
                        };
                    }
                    Err(hint) => return self.unavailable(language, hint),
                }
            }
        }

        self.unavailable(language, self.missing_hint(language))
    }

    fn resolve_csharp_dll(&self, value: &str) -> Option<PathBuf> {
        let path = Path::new(value);
        if path.is_absolute() || path.components().count() > 1 {
            let found = self.existing_file(path)?;
            // Accept the official DLL name or any *.dll the user pointed at.
            if found
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
            {
                return Some(found);
            }
            return None;
        }
        // Bare name: search PATH for the DLL file (not requiring +x).
        self.find_file_on_path(value)
    }

    fn require_dotnet_for_roslyn(
        &self,
        language: Language,
        dll: &Path,
    ) -> Result<ServerSpec, String> {
        let Some(dotnet) = self.find_on_path("dotnet") else {
            return Err(format!(
                "found Roslyn DLL at {} but `dotnet` is not on PATH. {}",
                dll.display(),
                super::csharp::CSHARP_INSTALL_HINT
            ));
        };
        Ok(csharp_spec(language, dotnet, dll.to_path_buf()))
    }

    /// PATH lookup that only requires a regular file (Roslyn DLL is not +x).
    fn find_file_on_path(&self, name: &str) -> Option<PathBuf> {
        for dir in &self.path_dirs {
            let candidate = dir.join(name);
            if let Some(found) = self.existing_file(&candidate) {
                return Some(found);
            }
            if !self.windows {
                continue;
            }
            // Case-insensitive filename match for Windows-shaped tests on Linux.
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                if !entry_name.to_string_lossy().eq_ignore_ascii_case(name) {
                    continue;
                }
                if let Some(found) = self.existing_file(&entry.path()) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn existing_file(&self, path: &Path) -> Option<PathBuf> {
        let Ok(meta) = path.metadata() else {
            return None;
        };
        meta.is_file().then(|| path.to_path_buf())
    }
}

impl Default for Discovery {
    /// Empty PATH, auto-install off — safe for `StubInstaller::default()` and tests.
    fn default() -> Self {
        Self::new("")
    }
}

/// Discover using the current process environment.
pub fn discover(language: Language) -> Result<ServerSpec, LspError> {
    Discovery::from_process().discover(language)
}

/// Probe every supported language using the current process environment.
pub fn probe_report() -> Vec<ProbeResult> {
    Discovery::from_process().probe_report()
}

/// Primary override variable for `language` (`ASTROLABE_LSP_PYTHON`, …).
pub fn env_override_var(language: Language) -> &'static str {
    env_override_keys(language).first().copied().unwrap_or("")
}

fn candidates(language: Language) -> &'static [Candidate] {
    match language {
        Language::Python => PYTHON,
        Language::Go => GO,
        Language::Java => JAVA,
        Language::Rust => RUST,
        Language::TypeScript | Language::Tsx | Language::JavaScript => TYPESCRIPT,
        Language::Php => PHP,
        Language::C | Language::Cpp => C_FAMILY,
        Language::Swift => APPLE_SWIFT,
        Language::ObjC | Language::ObjCpp => APPLE_OBJC,
        Language::Vue => VUE,
        Language::Dart => DART,
        Language::CSharp | Language::VisualBasic => CSHARP,
        Language::Kotlin => KOTLIN,
        Language::Shell => SHELL,
        Language::Ruby => RUBY,
        Language::Lua => LUA,
    }
}

fn env_override_keys(language: Language) -> &'static [&'static str] {
    match language {
        Language::Python => &["ASTROLABE_LSP_PYTHON"],
        Language::Go => &["ASTROLABE_LSP_GO"],
        Language::Java => &["ASTROLABE_LSP_JAVA"],
        Language::Rust => &["ASTROLABE_LSP_RUST"],
        Language::TypeScript => &["ASTROLABE_LSP_TYPESCRIPT"],
        Language::Tsx => &["ASTROLABE_LSP_TSX", "ASTROLABE_LSP_TYPESCRIPT"],
        Language::JavaScript => &["ASTROLABE_LSP_JAVASCRIPT", "ASTROLABE_LSP_TYPESCRIPT"],
        Language::Php => &["ASTROLABE_LSP_PHP"],
        Language::C => &["ASTROLABE_LSP_C", "ASTROLABE_LSP_CXX"],
        Language::Cpp => &["ASTROLABE_LSP_CXX", "ASTROLABE_LSP_C"],
        Language::ObjC => &["ASTROLABE_LSP_OBJC", "ASTROLABE_LSP_SWIFT"],
        Language::ObjCpp => &["ASTROLABE_LSP_OBJCPP", "ASTROLABE_LSP_SWIFT"],
        Language::Swift => &["ASTROLABE_LSP_SWIFT"],
        Language::Vue => &["ASTROLABE_LSP_VUE"],
        Language::Dart => &["ASTROLABE_LSP_DART"],
        Language::CSharp | Language::VisualBasic => &["ASTROLABE_LSP_CSHARP"],
        Language::Kotlin => &["ASTROLABE_LSP_KOTLIN"],
        Language::Shell => &["ASTROLABE_LSP_SHELL"],
        Language::Ruby => &["ASTROLABE_LSP_RUBY"],
        Language::Lua => &["ASTROLABE_LSP_LUA"],
    }
}

fn override_keys_all() -> &'static [&'static str] {
    &[
        "ASTROLABE_LSP_PYTHON",
        "ASTROLABE_LSP_GO",
        "ASTROLABE_LSP_JAVA",
        "ASTROLABE_LSP_RUST",
        "ASTROLABE_LSP_TYPESCRIPT",
        "ASTROLABE_LSP_TSX",
        "ASTROLABE_LSP_JAVASCRIPT",
        "ASTROLABE_LSP_PHP",
        "ASTROLABE_LSP_C",
        "ASTROLABE_LSP_CXX",
        "ASTROLABE_LSP_OBJC",
        "ASTROLABE_LSP_OBJCPP",
        "ASTROLABE_LSP_SWIFT",
        "ASTROLABE_LSP_VUE",
        "ASTROLABE_LSP_DART",
        "ASTROLABE_LSP_CSHARP",
        "ASTROLABE_LSP_KOTLIN",
        "ASTROLABE_LSP_SHELL",
        "ASTROLABE_LSP_RUBY",
        "ASTROLABE_LSP_LUA",
    ]
}

fn primary_bin(language: Language) -> &'static str {
    candidates(language)
        .first()
        .map(|c| c.bin)
        .unwrap_or(language.name())
}

/// Optional download catalog entry. Absent means Prompt/On still surface a
/// manual hint but cannot call [`crate::lsp::ensure_installed`] until a release
/// matrix fills the URL + official sha256.
pub fn catalog_install_spec(language: Language) -> Option<InstallSpec> {
    match language {
        // Official clangd GitHub release (Serena pins 19.1.2). Only used when
        // AutoInstallMode::On — Prompt/Off never download.
        Language::C | Language::Cpp => clangd_install_spec(),
        // Dart language-server ships with the Dart / Flutter SDK; no standalone
        // download matrix exists. Prompt-only: manual install guidance.
        Language::Dart => None,
        // Roslyn LS is optional/external; never silent-download.
        // Visual Basic opens the same server. No second env var, no download.
        Language::CSharp | Language::VisualBasic => None,
        _ => None,
    }
}

/// Pinned clangd 19.1.2 artifact for the current host (SHA-256 of the archive).
fn clangd_install_spec() -> Option<InstallSpec> {
    const VERSION: &str = "19.1.2";
    // Checksums from Serena's clangd_language_server.py (official release assets).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-linux-{VERSION}.zip"
            ),
            sha256: "7c09614eff857d590e4502ef516f035ff94cfb8b795de14ece5afbc53a206caf".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        })
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-mac-{VERSION}.zip"
            ),
            sha256: "d3b329b3f58602c57ca6501d255147af1bccad3691b1cb0c12c258fcd2da1be3".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        })
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-mac-{VERSION}.zip"
            ),
            sha256: "d3b329b3f58602c57ca6501d255147af1bccad3691b1cb0c12c258fcd2da1be3".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        })
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-windows-{VERSION}.zip"
            ),
            sha256: "5b6ceb0f85d63fa0c2c9aab31c29bebd41dc11da1f160ef21bc2fea93270a20d".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd.exe")),
        })
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        None
    }
}

fn csharp_spec(language: Language, dotnet: PathBuf, dll: PathBuf) -> ServerSpec {
    ServerSpec {
        language,
        command: dotnet,
        args: vec![dll.to_string_lossy().into_owned(), "--stdio".to_string()],
        install_hint: super::csharp::CSHARP_INSTALL_HINT.to_string(),
    }
}

fn spec_from_candidate(language: Language, command: PathBuf, candidate: &Candidate) -> ServerSpec {
    ServerSpec {
        language,
        command,
        args: candidate.args.iter().map(|s| (*s).to_string()).collect(),
        install_hint: candidate.install.to_string(),
    }
}

fn spec_for_resolved(language: Language, command: PathBuf) -> ServerSpec {
    let stem = bin_stem(&command);
    if let Some(candidate) = find_candidate_by_bin(&stem) {
        return spec_from_candidate(language, command, candidate);
    }
    let args = extra_args_for_unknown_stem(&stem);
    ServerSpec {
        language,
        command,
        args,
        install_hint: format!("user override via {}", env_override_var(language)),
    }
}

fn find_candidate_by_bin(bin: &str) -> Option<&'static Candidate> {
    PYTHON
        .iter()
        .chain(GO)
        .chain(JAVA)
        .chain(RUST)
        .chain(TYPESCRIPT)
        .chain(PHP)
        .chain(C_FAMILY)
        .chain(APPLE_SWIFT)
        .chain(APPLE_OBJC)
        .chain(VUE)
        .chain(DART)
        .chain(CSHARP)
        .chain(KOTLIN)
        .chain(SHELL)
        .chain(RUBY)
        .chain(LUA)
        .find(|c| c.bin == bin)
}

fn extra_args_for_unknown_stem(stem: &str) -> Vec<String> {
    match stem {
        "tsc" | "tsgo" => vec!["--lsp".into(), "--stdio".into()],
        "clangd" => vec!["--background-index".into()],
        _ => Vec::new(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn bin_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    strip_windows_ext(&name)
}

fn strip_windows_ext(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [".cmd", ".exe", ".bat", ".com"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return name[..stripped.len()].to_string();
        }
    }
    name.to_string()
}

fn command_names(bin: &str, windows: bool, pathext: &str) -> Vec<String> {
    let mut names = vec![bin.to_string()];
    if !windows {
        return names;
    }
    if has_pathext_suffix(bin, pathext) {
        return names;
    }
    for ext in parse_pathext(pathext) {
        names.push(format!("{bin}{ext}"));
    }
    names
}

fn parse_pathext(pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|ext| {
            if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            }
        })
        .collect()
}

fn has_pathext_suffix(name: &str, pathext: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    parse_pathext(pathext)
        .iter()
        .any(|ext| lower.ends_with(&ext.to_ascii_lowercase()))
}

fn parse_path(path: impl AsRef<OsStr>) -> Vec<PathBuf> {
    std::env::split_paths(path.as_ref())
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn is_bare_name(path: &Path) -> bool {
    path.components().count() == 1
}

fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Lets the pool drive discovery without depending on this module's concrete
/// type.
impl super::pool::DiscoverServers for Discovery {
    fn discover(
        &self,
        language: crate::types::Language,
    ) -> Result<super::ServerSpec, super::LspError> {
        Discovery::discover(self, language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Scratch {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-lsp-discovery-{}-{}",
            process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir for discovery tests");
        Scratch(dir)
    }

    fn write_fake_executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write fake executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod +x");
        }
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        };
        write_fake_executable(&path);
        path
    }

    fn discovery_with_dirs(dirs: &[&Path]) -> Discovery {
        let path = std::env::join_paths(dirs).expect("join PATH");
        Discovery::new(path)
    }

    fn unavailable_hint(err: LspError) -> (Language, String) {
        match err {
            LspError::Unavailable(lang, hint) => (lang, hint),
            other => panic!("expected Unavailable, got {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn broken_rustup_proxy_is_not_ready() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        let rustup = dir.0.join("rustup");
        fs::write(
            &rustup,
            "#!/bin/sh\necho \"error: rustup could not choose a version of rust-analyzer to run, because one wasn't specified explicitly, and no default is configured.\" >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
        fs::hard_link(&rustup, dir.0.join("rust-analyzer")).unwrap();
        let probe = discovery_with_dirs(&[&dir.0]).probe(Language::Rust);
        assert!(!probe.available(), "{probe:?}");
        assert!(
            probe.install_hint.contains("rustup default stable"),
            "{}",
            probe.install_hint
        );
        assert!(probe.needs_install.is_some());
        let err = discovery_with_dirs(&[&dir.0])
            .discover(Language::Rust)
            .unwrap_err();
        assert!(err.to_string().contains("rustup proxy"), "{err}");
        // Explicit override pointing at the proxy: same answer.
        let probe = Discovery::new("")
            .with_env(
                "ASTROLABE_LSP_RUST",
                dir.0.join("rust-analyzer").display().to_string(),
            )
            .probe(Language::Rust);
        assert!(!probe.available());
        // Other languages are not affected by the check.
        fake_bin(&dir.0, "gopls");
        assert!(discovery_with_dirs(&[&dir.0])
            .probe(Language::Go)
            .available());
    }

    #[test]
    fn missing_binary_is_unavailable_with_install_command() {
        let d = Discovery::new("");
        let err = d.discover(Language::Python).unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Python);
        assert!(!hint.is_empty());
        assert!(
            hint.contains("npm i -g pyright"),
            "install_hint must contain an executable install command, got: {hint}"
        );
        assert!(hint.contains("ASTROLABE_LSP_PYTHON"));
    }

    #[test]
    fn empty_path_install_hints_cover_every_language() {
        let d = Discovery::new("");
        for language in PROBED_LANGUAGES {
            let result = d.probe(*language);
            assert!(result.spec.is_none(), "{language:?} should be missing");
            assert!(!result.install_hint.is_empty());
            match language {
                Language::Python => assert!(result.install_hint.contains("npm i -g pyright")),
                Language::Go => {
                    assert!(result
                        .install_hint
                        .contains("go install golang.org/x/tools/gopls@latest"))
                }
                Language::Java => {
                    assert!(result.install_hint.contains("brew install jdtls"));
                    assert!(result.install_hint.contains("java -"));
                    assert!(result.install_hint.contains("-data"));
                    assert!(result.install_hint.contains("org.eclipse.equinox.launcher"));
                }
                Language::Rust => {
                    assert!(result
                        .install_hint
                        .contains("rustup component add rust-analyzer"))
                }
                Language::TypeScript | Language::Tsx | Language::JavaScript => {
                    assert!(result
                        .install_hint
                        .contains("npm i -g typescript-language-server"))
                }
                Language::Php => {
                    assert!(result.install_hint.contains("npm i -g intelephense"));
                    assert!(result.install_hint.contains("phpactor"));
                    assert!(
                        result.install_hint.contains("INTELEPHENSE_LICENSE_KEY")
                            || result.install_hint.contains("purchased"),
                        "license guidance required, got: {}",
                        result.install_hint
                    );
                }
                Language::C | Language::Cpp => {
                    assert!(result.install_hint.to_ascii_lowercase().contains("clangd"))
                }
                Language::ObjC | Language::ObjCpp | Language::Swift => {
                    assert!(result.install_hint.contains("sourcekit-lsp"))
                }
                Language::Vue => {
                    assert!(result.install_hint.contains("@vue/language-server"))
                }
                Language::Dart => {
                    assert!(result.install_hint.contains("Dart SDK"));
                    assert!(result.install_hint.contains("dart language-server"));
                }
                Language::CSharp | Language::VisualBasic => {
                    assert!(result
                        .install_hint
                        .contains("Microsoft.CodeAnalysis.LanguageServer"));
                    assert!(result.install_hint.contains(".NET 10"));
                    assert!(result.install_hint.contains("ASTROLABE_LSP_CSHARP"));
                    assert!(result.install_hint.contains("dotnet"));
                }
                Language::Kotlin => assert!(result.install_hint.contains("kotlin-lsp")),
                Language::Shell => assert!(result.install_hint.contains("bash-language-server")),
                Language::Ruby => assert!(result.install_hint.contains("ruby-lsp")),
                Language::Lua => assert!(result.install_hint.contains("lua-language-server")),
            }
            assert!(
                result.needs_install.is_some(),
                "{language:?} must export NeedsInstall"
            );
        }
    }

    #[test]
    fn finds_first_priority_binary_on_path() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Python)
            .unwrap();
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("pyright-langserver"),
            "priority must prefer pyright over pylsp, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert_eq!(spec.language, Language::Python);
        assert!(spec.install_hint.contains("npm i -g pyright"));
    }

    #[test]
    fn skips_missing_higher_priority_and_uses_next() {
        let dir = scratch();
        fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("pylsp"));
        assert!(spec.args.is_empty());
        assert!(spec.install_hint.contains("pip install python-lsp-server"));
    }

    #[test]
    fn env_override_wins_over_path_priority() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        let pylsp = fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", pylsp.to_string_lossy())
            .discover(Language::Python)
            .unwrap();
        assert_eq!(spec.command, pylsp);
        assert!(spec.args.is_empty());
    }

    #[test]
    fn env_override_bare_name_searches_path() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "jedi-language-server");
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", "jedi-language-server")
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("jedi-language-server"));
    }

    #[test]
    fn env_override_missing_is_unavailable_not_silent() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        let missing = dir.0.join("does-not-exist");
        let err = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", missing.to_string_lossy())
            .discover(Language::Python)
            .unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Python);
        assert!(hint.contains("ASTROLABE_LSP_PYTHON"));
        assert!(
            hint.contains("does-not-exist"),
            "hint should mention the missing override path, got: {hint}"
        );
        assert!(hint.contains("npm i -g pyright"));
    }

    #[test]
    fn stdio_args_for_known_servers() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "typescript-language-server");
        fake_bin(&dir.0, "vtsls");
        fake_bin(&dir.0, "gopls");
        fake_bin(&dir.0, "rust-analyzer");
        fake_bin(&dir.0, "ruff");
        let d = discovery_with_dirs(&[&dir.0]);

        assert_eq!(d.discover(Language::Python).unwrap().args, vec!["--stdio"]);
        assert_eq!(
            d.discover(Language::TypeScript).unwrap().args,
            vec!["--stdio"]
        );
        assert_eq!(d.discover(Language::Go).unwrap().args, vec!["-remote=auto"]);
        assert_eq!(
            d.discover(Language::Rust).unwrap().args,
            Vec::<String>::new()
        );

        let ruff_only = scratch();
        fake_bin(&ruff_only.0, "ruff");
        assert_eq!(
            discovery_with_dirs(&[&ruff_only.0])
                .discover(Language::Python)
                .unwrap()
                .args,
            vec!["server"]
        );
    }

    #[test]
    fn typescript_javascript_and_tsx_share_servers() {
        let dir = scratch();
        fake_bin(&dir.0, "vtsls");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::TypeScript, Language::Tsx, Language::JavaScript] {
            let spec = d.discover(language).unwrap();
            assert_eq!(spec.language, language);
            assert!(spec
                .command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("vtsls"));
            assert_eq!(spec.args, vec!["--stdio"]);
        }
    }

    #[test]
    fn javascript_override_falls_back_to_typescript_var() {
        let dir = scratch();
        let vtsls = fake_bin(&dir.0, "vtsls");
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_TYPESCRIPT", vtsls.to_string_lossy())
            .discover(Language::JavaScript)
            .unwrap();
        assert_eq!(spec.command, vtsls);
    }

    #[test]
    fn java_without_jdtls_explains_wrapper_and_java_jar() {
        let err = Discovery::new("").discover(Language::Java).unwrap_err();
        let (_, hint) = unavailable_hint(err);
        assert!(hint.contains("brew install jdtls"));
        assert!(hint.contains("java -Declipse.application"));
        assert!(hint.contains("-jar"));
        assert!(hint.contains("-data"));
        assert!(hint.contains("config_{linux|mac|win}"));
    }

    #[test]
    fn java_finds_jdtls_wrapper_when_present() {
        let dir = scratch();
        fake_bin(&dir.0, "jdtls");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Java)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("jdtls"));
        assert!(spec.args.is_empty());
    }

    /// gopls joins the shared daemon unless switched off; an explicit
    /// address passes through.
    #[test]
    fn gopls_shares_one_daemon_unless_switched_off() {
        let dir = scratch();
        fake_bin(&dir.0, "gopls");
        let d = discovery_with_dirs(&[&dir.0]);
        assert_eq!(d.discover(Language::Go).unwrap().args, vec!["-remote=auto"]);
        assert!(d.probe(Language::Go).summary().contains("-remote=auto"));
        for off in ["off", "0", "false", "NO"] {
            let d = discovery_with_dirs(&[&dir.0]).with_env(GOPLS_REMOTE_ENV, off);
            assert!(d.discover(Language::Go).unwrap().args.is_empty(), "{off}");
        }
        let d = discovery_with_dirs(&[&dir.0]).with_env(GOPLS_REMOTE_ENV, "unix;/tmp/g.sock");
        assert_eq!(
            d.discover(Language::Go).unwrap().args,
            vec!["-remote=unix;/tmp/g.sock"]
        );
        // An override pointing at gopls gets the same treatment; other
        // binaries for Go do not.
        let d = discovery_with_dirs(&[]).with_env(
            "ASTROLABE_LSP_GO",
            dir.0.join("gopls").display().to_string(),
        );
        assert_eq!(d.discover(Language::Go).unwrap().args, vec!["-remote=auto"]);
        fake_bin(&dir.0, "my-go-ls");
        let d = discovery_with_dirs(&[]).with_env(
            "ASTROLABE_LSP_GO",
            dir.0.join("my-go-ls").display().to_string(),
        );
        assert!(d.discover(Language::Go).unwrap().args.is_empty());
    }

    #[test]
    fn probe_report_covers_every_language_and_is_deterministic() {
        let dir = scratch();
        fake_bin(&dir.0, "gopls");
        let d = discovery_with_dirs(&[&dir.0]);
        let first = d.probe_report();
        let second = d.probe_report();
        assert_eq!(first, second);
        assert_eq!(first.len(), PROBED_LANGUAGES.len());
        let langs: Vec<Language> = first.iter().map(|r| r.language).collect();
        assert_eq!(langs, PROBED_LANGUAGES);
        for row in &first {
            assert!(!row.install_hint.is_empty());
        }
        let go = first.iter().find(|r| r.language == Language::Go).unwrap();
        assert!(go.available());
        let rust = first.iter().find(|r| r.language == Language::Rust).unwrap();
        assert!(!rust.available());
        assert!(rust.summary().contains("unavailable"));
    }

    #[test]
    fn process_snapshot_reports_every_language() {
        // Does not compare two snapshots: other tests in the crate must not
        // mutate process env, but we still avoid depending on that here.
        let report = probe_report();
        assert_eq!(report.len(), PROBED_LANGUAGES.len());
        let langs: Vec<Language> = report.iter().map(|r| r.language).collect();
        assert_eq!(langs, PROBED_LANGUAGES);
        for row in &report {
            assert!(!row.install_hint.is_empty());
        }
    }

    #[test]
    fn windows_cmd_suffix_is_accepted() {
        let dir = scratch();
        let cmd = dir.0.join("pyright-langserver.cmd");
        write_fake_executable(&cmd);
        let mut d = Discovery::new(dir.0.as_os_str());
        d.windows = true;
        let spec = d.discover(Language::Python).unwrap();
        assert!(
            spec.command
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("pyright-langserver.cmd"),
            "expected a .cmd shim, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
    }

    #[test]
    fn command_names_include_windows_extensions() {
        let names = command_names("gopls", true, DEFAULT_PATHEXT);
        assert_eq!(names[0], "gopls");
        assert!(
            names.contains(&"gopls.EXE".to_string())
                || names.iter().any(|n| n.eq_ignore_ascii_case("gopls.exe"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("gopls.cmd")));
        let unix = command_names("gopls", false, DEFAULT_PATHEXT);
        assert_eq!(unix, vec!["gopls".to_string()]);
        let already = command_names("gopls.cmd", true, DEFAULT_PATHEXT);
        assert_eq!(already, vec!["gopls.cmd".to_string()]);
    }

    #[test]
    fn higher_priority_in_later_path_dir_still_wins() {
        let first = scratch();
        let second = scratch();
        fake_bin(&first.0, "pylsp");
        fake_bin(&second.0, "pyright-langserver");
        let spec = discovery_with_dirs(&[&first.0, &second.0])
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("pyright-langserver"));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_skipped() {
        let dir = scratch();
        fs::write(dir.0.join("gopls"), b"not executable").unwrap();
        let err = discovery_with_dirs(&[&dir.0])
            .discover(Language::Go)
            .unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Go);
        assert!(hint.contains("go install golang.org/x/tools/gopls@latest"));
    }

    #[test]
    fn tsc_override_gets_lsp_stdio_args() {
        let dir = scratch();
        let tsc = fake_bin(&dir.0, "tsc");
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_TYPESCRIPT", tsc.to_string_lossy())
            .discover(Language::TypeScript)
            .unwrap();
        assert_eq!(spec.args, vec!["--lsp", "--stdio"]);
    }

    #[test]
    fn finds_vue_language_server_as_volar() {
        let dir = scratch();
        fake_bin(&dir.0, "vue-language-server");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Vue)
            .expect("vue-language-server on PATH");
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("vue-language-server"),
            "got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert_eq!(spec.language, Language::Vue);
        assert!(
            spec.install_hint.contains("@vue/language-server"),
            "install hint should point at @vue/language-server, got {}",
            spec.install_hint
        );
        // Volar 3.x is bridged through typescript-language-server, so the
        // hint must name it.
        assert!(spec.install_hint.contains("typescript-language-server"));
    }

    #[test]
    fn env_override_var_names_are_stable() {
        assert_eq!(env_override_var(Language::Python), "ASTROLABE_LSP_PYTHON");
        assert_eq!(env_override_var(Language::Go), "ASTROLABE_LSP_GO");
        assert_eq!(env_override_var(Language::Java), "ASTROLABE_LSP_JAVA");
        assert_eq!(env_override_var(Language::Rust), "ASTROLABE_LSP_RUST");
        assert_eq!(
            env_override_var(Language::TypeScript),
            "ASTROLABE_LSP_TYPESCRIPT"
        );
        assert_eq!(env_override_var(Language::Tsx), "ASTROLABE_LSP_TSX");
        assert_eq!(
            env_override_var(Language::JavaScript),
            "ASTROLABE_LSP_JAVASCRIPT"
        );
        assert_eq!(env_override_var(Language::Php), "ASTROLABE_LSP_PHP");
        assert_eq!(env_override_var(Language::C), "ASTROLABE_LSP_C");
        assert_eq!(env_override_var(Language::Cpp), "ASTROLABE_LSP_CXX");
        assert_eq!(env_override_var(Language::Swift), "ASTROLABE_LSP_SWIFT");
        assert_eq!(env_override_var(Language::ObjC), "ASTROLABE_LSP_OBJC");
        assert_eq!(env_override_var(Language::ObjCpp), "ASTROLABE_LSP_OBJCPP");
        assert_eq!(env_override_var(Language::Vue), "ASTROLABE_LSP_VUE");
        assert_eq!(env_override_var(Language::Dart), "ASTROLABE_LSP_DART");
        assert_eq!(env_override_var(Language::CSharp), "ASTROLABE_LSP_CSHARP");
    }

    #[test]
    fn php_prefers_intelephense_then_phpactor() {
        let dir = scratch();
        fake_bin(&dir.0, "intelephense");
        fake_bin(&dir.0, "phpactor");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Php)
            .unwrap();
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("intelephense"),
            "priority must prefer intelephense, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert!(spec.install_hint.contains("npm i -g intelephense"));
    }

    #[test]
    fn php_falls_back_to_phpactor_language_server() {
        let dir = scratch();
        fake_bin(&dir.0, "phpactor");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Php)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("phpactor"));
        assert_eq!(spec.args, vec!["language-server"]);
        assert!(spec.install_hint.contains("phpactor"));
    }

    #[test]
    fn cache_root_is_consulted_after_path_miss() {
        let cache = scratch();
        let version = cache.0.join("clangd").join("18.1.3");
        fs::create_dir_all(&version).unwrap();
        let bin = fake_bin(&version, "clangd");
        fs::write(version.join(".installed"), "ok").unwrap();
        let d = Discovery::new("")
            .with_cache_root(&cache.0)
            .with_auto_install(AutoInstallMode::Off);
        let spec = d.discover(Language::Cpp).unwrap();
        assert_eq!(spec.command, bin);
        assert!(d.probe(Language::Cpp).needs_install.is_none());
    }

    #[test]
    fn prompt_mode_never_auto_installs() {
        let cache = scratch();
        let d = Discovery::new("")
            .with_cache_root(&cache.0)
            .with_auto_install(AutoInstallMode::Prompt);
        let result = d.probe(Language::Cpp);
        assert!(result.spec.is_none());
        let needs = result.needs_install.expect("structured hint");
        assert_eq!(needs.hint.mode, AutoInstallMode::Prompt);
    }

    #[test]
    fn auto_install_in_progress_answers_at_once_and_starts_no_second_download() {
        let Some(spec) = catalog_install_spec(Language::Cpp) else {
            return; // no pinned artifact for this host
        };
        let cache = scratch();
        let key = install_key(&spec, &cache.0);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let bin = cache.0.join("fake-clangd");
        let bin_done = bin.clone();
        // Stand-in for a slow download already running for this artifact.
        let first = Installs::global().poll_or_start(&key, std::time::Duration::ZERO, move || {
            let _ = release_rx.recv();
            fs::write(&bin_done, "x").map_err(|e| e.to_string())?;
            Ok(bin_done)
        });
        assert!(matches!(first, BackgroundInstall::Pending { .. }));

        let d = Discovery::new("")
            .with_cache_root(&cache.0)
            .with_auto_install(AutoInstallMode::On);
        let started = std::time::Instant::now();
        let probed = d.probe(Language::Cpp);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "probe waited on the download: {:?}",
            started.elapsed()
        );
        assert!(probed.spec.is_none());
        assert!(
            probed.install_hint.contains(installer::INSTALL_PENDING),
            "{}",
            probed.install_hint
        );

        release_tx.send(()).unwrap();
        let d = d.with_install_wait(std::time::Duration::from_secs(5));
        let ready = d
            .discover(Language::Cpp)
            .expect("installed by the background job");
        assert_eq!(ready.command, bin);
    }

    #[test]
    fn new_discovery_defaults_are_hermetic() {
        let d = Discovery::new("");
        assert!(d.cache_root().is_none());
        assert_eq!(d.auto_install_mode(), AutoInstallMode::Off);
    }

    #[test]
    fn sourcekit_for_swift_and_clangd_for_objc_family() {
        let dir = scratch();
        fake_bin(&dir.0, "sourcekit-lsp");
        fake_bin(&dir.0, "clangd");
        let d = discovery_with_dirs(&[&dir.0]);
        let stem = |language| {
            let spec = d.discover(language).unwrap();
            assert_eq!(spec.language, language);
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert!(stem(Language::Swift).starts_with("sourcekit-lsp"));
        for language in [Language::ObjC, Language::ObjCpp] {
            assert!(stem(language).starts_with("clangd"), "{language:?}");
        }
    }

    #[test]
    fn objc_falls_back_to_sourcekit_when_clangd_missing() {
        let dir = scratch();
        fake_bin(&dir.0, "sourcekit-lsp");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::ObjC, Language::ObjCpp] {
            let spec = d.discover(language).unwrap();
            assert!(
                spec.command
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("sourcekit-lsp"),
                "{language:?} should fall back to sourcekit-lsp, got {:?}",
                spec.command
            );
        }
    }

    #[test]
    fn swift_without_sourcekit_is_unavailable() {
        let dir = scratch();
        fake_bin(&dir.0, "clangd");
        let d = discovery_with_dirs(&[&dir.0]);
        let err = d.discover(Language::Swift).unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Swift);
        assert!(hint.contains("sourcekit-lsp") || hint.contains("xcode-select"));
    }

    #[test]
    fn from_path_recognizes_apple_extensions() {
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.m")),
            Some(Language::ObjC)
        );
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.mm")),
            Some(Language::ObjCpp)
        );
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.swift")),
            Some(Language::Swift)
        );
        // .h stays C (foundation); ObjC headers need companion/.m awareness later.
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.h")),
            Some(Language::C)
        );
    }

    #[test]
    fn prefers_clangd_over_ccls_for_c_and_cpp() {
        let dir = scratch();
        fake_bin(&dir.0, "clangd");
        fake_bin(&dir.0, "ccls");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::C, Language::Cpp] {
            let spec = d.discover(language).unwrap();
            assert!(
                spec.command
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("clangd"),
                "{language:?} should prefer clangd, got {:?}",
                spec.command
            );
            assert_eq!(spec.args, vec!["--background-index"]);
            assert!(
                spec.install_hint.to_ascii_lowercase().contains("clangd"),
                "hint should mention clangd, got {}",
                spec.install_hint
            );
        }
    }

    #[test]
    fn c_family_falls_back_to_ccls() {
        let dir = scratch();
        fake_bin(&dir.0, "ccls");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Cpp)
            .unwrap();
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("ccls"),
            "got {:?}",
            spec.command
        );
        assert!(spec.args.is_empty());
    }

    #[test]
    fn finds_dart_language_server_on_path() {
        let dir = scratch();
        fake_bin(&dir.0, "dart");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Dart)
            .expect("dart on PATH");
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("dart"),
            "got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["language-server"]);
        assert_eq!(spec.language, Language::Dart);
        assert!(spec.install_hint.contains("Dart SDK"));
    }

    #[test]
    fn dart_discovery_without_binary_needs_install_with_guidance() {
        let d = Discovery::new("");
        let probed = d.probe(Language::Dart);
        assert!(!probed.available());
        assert!(probed.install_hint.contains("dart language-server"));
        assert!(probed.install_hint.contains("Dart SDK"));
        assert!(probed.install_hint.contains("ASTROLABE_LSP_DART"));
        let needs = probed.needs_install.expect("structured hint");
        assert_eq!(needs.hint.language, Language::Dart);
    }

    #[test]
    fn dart_env_override_is_respected() {
        let dir = scratch();
        let dart = fake_bin(&dir.0, "dart");
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_DART", dart.to_string_lossy())
            .discover(Language::Dart)
            .unwrap();
        assert_eq!(spec.command, dart);
        assert_eq!(spec.args, vec!["language-server"]);
    }

    #[test]
    fn csharp_env_override_dll_launches_via_dotnet_stdio() {
        let dir = scratch();
        let dotnet = fake_bin(&dir.0, "dotnet");
        let dll = dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        fs::write(&dll, b"fake-dll").unwrap();
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_CSHARP", dll.to_string_lossy())
            .discover(Language::CSharp)
            .expect("env dll + dotnet on PATH");
        assert_eq!(spec.command, dotnet);
        assert_eq!(
            spec.args,
            vec![dll.to_string_lossy().into_owned(), "--stdio".to_string()]
        );
        assert_eq!(spec.language, Language::CSharp);
        assert!(spec.install_hint.contains(".NET 10"));
    }

    #[test]
    fn csharp_finds_dll_candidate_on_path() {
        let dir = scratch();
        let dotnet = fake_bin(&dir.0, "dotnet");
        let dll = dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        fs::write(&dll, b"fake-dll").unwrap();
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::CSharp)
            .expect("dll + dotnet on PATH");
        assert_eq!(spec.command, dotnet);
        assert_eq!(spec.args[0], dll.to_string_lossy());
        assert_eq!(spec.args[1], "--stdio");
    }

    #[test]
    fn csharp_env_override_wins_over_path_dll() {
        let path_dir = scratch();
        let env_dir = scratch();
        fake_bin(&path_dir.0, "dotnet");
        fake_bin(&env_dir.0, "dotnet");
        let path_dll = path_dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        let env_dll = env_dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        fs::write(&path_dll, b"path").unwrap();
        fs::write(&env_dll, b"env").unwrap();
        let spec = discovery_with_dirs(&[&path_dir.0, &env_dir.0])
            .with_env("ASTROLABE_LSP_CSHARP", env_dll.to_string_lossy())
            .discover(Language::CSharp)
            .unwrap();
        assert_eq!(spec.args[0], env_dll.to_string_lossy());
    }

    #[test]
    fn csharp_without_dll_needs_install_mentions_net10() {
        let d = Discovery::new("");
        let probed = d.probe(Language::CSharp);
        assert!(!probed.available());
        assert!(probed
            .install_hint
            .contains("Microsoft.CodeAnalysis.LanguageServer"));
        assert!(probed.install_hint.contains(".NET 10"));
        assert!(probed.install_hint.contains("ASTROLABE_LSP_CSHARP"));
        assert!(probed.install_hint.contains("dotnet"));
        assert!(
            !probed
                .install_hint
                .to_ascii_lowercase()
                .contains("omnisharp")
                || probed.install_hint.contains("Not OmniSharp")
        );
        let needs = probed.needs_install.expect("structured hint");
        assert_eq!(needs.hint.language, Language::CSharp);
    }

    #[test]
    fn csharp_dll_without_dotnet_is_unavailable() {
        let dir = scratch();
        let dll = dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        fs::write(&dll, b"fake-dll").unwrap();
        // PATH has the dll but no dotnet.
        let err = discovery_with_dirs(&[&dir.0])
            .discover(Language::CSharp)
            .unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::CSharp);
        assert!(hint.contains("dotnet"), "{hint}");
        assert!(
            hint.contains(".NET 10") || hint.contains("Microsoft.CodeAnalysis"),
            "{hint}"
        );
    }

    #[test]
    fn visualbasic_uses_the_csharp_roslyn_server() {
        let dir = scratch();
        let dotnet = fake_bin(&dir.0, "dotnet");
        let dll = dir.0.join("Microsoft.CodeAnalysis.LanguageServer.dll");
        fs::write(&dll, b"fake-dll").unwrap();
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_CSHARP", dll.to_string_lossy())
            .discover(Language::VisualBasic)
            .expect("env dll + dotnet on PATH");
        assert_eq!(spec.command, dotnet);
        assert_eq!(
            spec.args,
            vec![dll.to_string_lossy().into_owned(), "--stdio".to_string()]
        );
        assert_eq!(spec.language, Language::VisualBasic);
        assert!(spec.install_hint.contains("ASTROLABE_LSP_CSHARP"));

        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::VisualBasic)
            .expect("dll + dotnet on PATH");
        assert_eq!(spec.command, dotnet);
        assert_eq!(spec.args[0], dll.to_string_lossy());
        assert_eq!(spec.args[1], "--stdio");
        assert_eq!(spec.language, Language::VisualBasic);
    }

    #[cfg(unix)]
    #[test]
    fn kotlin_path_symlink_to_deprecated_wrapper_runs_intellij_server() {
        let tmp = scratch();
        let dist = tmp.0.join("kotlin-server-263.6379.0");
        fs::create_dir_all(dist.join("bin")).unwrap();
        write_fake_executable(&dist.join("kotlin-lsp.sh"));
        write_fake_executable(&dist.join("bin/intellij-server"));
        let path_dir = tmp.0.join("path");
        fs::create_dir_all(&path_dir).unwrap();
        std::os::unix::fs::symlink(dist.join("kotlin-lsp.sh"), path_dir.join("kotlin-lsp"))
            .unwrap();
        let spec = Discovery::new(&path_dir)
            .discover(Language::Kotlin)
            .unwrap();
        assert_eq!(
            fs::canonicalize(&spec.command).unwrap(),
            fs::canonicalize(dist.join("bin/intellij-server")).unwrap()
        );
        assert_eq!(spec.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn kotlin_intellij_server_on_path_is_accepted() {
        let tmp = scratch();
        write_fake_executable(&tmp.0.join("intellij-server"));
        let spec = Discovery::new(&tmp.0).discover(Language::Kotlin).unwrap();
        assert!(spec.command.ends_with("intellij-server"));
        assert_eq!(spec.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn kotlin_override_may_name_the_unpacked_directory() {
        let tmp = scratch();
        let dist = tmp.0.join("kotlin-server-263.6379.0");
        fs::create_dir_all(dist.join("bin")).unwrap();
        write_fake_executable(&dist.join("bin/intellij-server"));
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_KOTLIN", dist.to_string_lossy())
            .discover(Language::Kotlin)
            .unwrap();
        assert_eq!(spec.command, dist.join("bin/intellij-server"));
        assert_eq!(spec.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn kotlin_unpacked_distribution_beats_fwcd_fallback() {
        let tmp = scratch();
        let path_dir = tmp.0.join("path");
        fs::create_dir_all(&path_dir).unwrap();
        write_fake_executable(&path_dir.join("kotlin-language-server"));
        let ext = tmp.0.join("extensions");
        let launcher =
            ext.join("jetbrains.kotlin-server-0.0.13-linux-x64/server/bin/intellij-server");
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        write_fake_executable(&launcher);
        let spec = Discovery::new(&path_dir)
            .with_kotlin_root(&ext)
            .discover(Language::Kotlin)
            .unwrap();
        assert_eq!(spec.command, launcher);
        // Without the root, fwcd is still the fallback.
        let spec = Discovery::new(&path_dir)
            .discover(Language::Kotlin)
            .unwrap();
        assert!(spec.command.ends_with("kotlin-language-server"));
        assert!(spec.args.is_empty());
    }
}
