//! Roslyn (Microsoft.CodeAnalysis.LanguageServer) transport helpers.
//!
//! Launch shape is always `dotnet <Microsoft.CodeAnalysis.LanguageServer.dll>
//! --stdio`. After `initialize` / `initialized`, the client must open a
//! solution or projects with the Microsoft-specific notifications
//! `solution/open` and `project/open` — verified against Roslyn's
//! OpenSolutionHandler / OpenProjectsHandler and the VS Code C# extension's
//! `roslynProtocol.ts`. Do not invent alternate notifications.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::transport::LspTransport;
use super::LspError;

/// DLL file name shipped by the official Roslyn language-server packages.
pub const ROSLYN_DLL_NAME: &str = "Microsoft.CodeAnalysis.LanguageServer.dll";

/// Install guidance: Roslyn LS needs a .NET 10 runtime even when the analyzed
/// project targets an older TFM. No silent download from Astrolabe.
pub const CSHARP_INSTALL_HINT: &str = "\
Microsoft.CodeAnalysis.LanguageServer (Roslyn) not found. \
Astrolabe does not download it. Install the .NET 10 runtime \
(https://dotnet.microsoft.com/download/dotnet/10.0) — required to *run* the \
language server even if the analyzed project targets an older framework — \
then obtain Microsoft.CodeAnalysis.LanguageServer.dll (NuGet package \
roslyn-language-server.<rid>, or the VS Code C# / C# Dev Kit extension) and set \
ASTROLABE_LSP_CSHARP=/path/to/Microsoft.CodeAnalysis.LanguageServer.dll. \
Launch shape: dotnet <dll> --stdio. Not OmniSharp; not community csharp-ls.";

/// Workspace entry Roslyn should load after initialize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsharpWorkspace {
    /// Prefer a single shallowest `.sln` / `.slnx`.
    Solution(PathBuf),
    /// No solution: open discovered `.csproj` files.
    Projects(Vec<PathBuf>),
}

/// Strip Roslyn document-symbol decorations so names match the syntactic index.
///
/// Roslyn 5.5+ often returns names like `Add(int, int) : int` or
/// `Name : string`. The graph stores the bare identifier (`Add`, `Name`).
///
/// Examples:
/// - `Add(int, int) : int` → `Add`
/// - `Name : string` → `Name`
/// - `ToString()` → `ToString`
/// - `Position : (int X, string Y)` → `Position`
/// - `SimpleMethod` → `SimpleMethod`
pub fn strip_roslyn_symbol_name(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() {
        return name;
    }
    // Property / field: "Name : Type". Guard parentheses only on the name
    // segment so tuple types ("(int X, string Y)") stay intact.
    if let Some((base, _)) = name.split_once(" : ") {
        let base = base.trim();
        if !base.is_empty() && !base.contains('(') {
            return base;
        }
    }
    // Method: "MethodName(params) : ReturnType" or "MethodName()"
    if let Some(paren) = name.find('(') {
        let base = name[..paren].trim();
        if !base.is_empty() {
            return base;
        }
    }
    name
}

/// Prefer the shallowest `.sln` / `.slnx` (BFS), else `.csproj` and `.vbproj`
/// files. Skips `bin`, `obj`, `.vs`, and hidden directories. Visual Basic
/// opens the same Roslyn server; there is no separate discovery env.
pub fn find_csharp_workspace(root: &Path) -> Option<CsharpWorkspace> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());
    let mut projects: Vec<PathBuf> = Vec::new();

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let lower = name_str.to_ascii_lowercase();
                if lower == "bin" || lower == "obj" || lower == ".vs" {
                    continue;
                }
                // `.vs` already skipped by hidden-dot check; keep explicit.
                dirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let lower = name_str.to_ascii_lowercase();
            if lower.ends_with(".sln") || lower.ends_with(".slnx") {
                return Some(CsharpWorkspace::Solution(path));
            }
            if lower.ends_with(".csproj") || lower.ends_with(".vbproj") {
                projects.push(path);
            }
        }
        // Stable BFS within a directory.
        dirs.sort();
        for d in dirs {
            queue.push_back(d);
        }
    }

    if projects.is_empty() {
        None
    } else {
        projects.sort();
        Some(CsharpWorkspace::Projects(projects))
    }
}

/// After `initialized`, open the solution/projects the way Roslyn requires.
pub fn open_csharp_workspace(transport: &LspTransport, root: &Path) -> Result<(), LspError> {
    match find_csharp_workspace(root) {
        Some(CsharpWorkspace::Solution(sln)) => {
            let uri = path_to_file_uri(&sln);
            transport.notify("solution/open", json!({ "solution": uri }))
        }
        Some(CsharpWorkspace::Projects(projects)) => {
            let uris: Vec<String> = projects.iter().map(|p| path_to_file_uri(p)).collect();
            transport.notify("project/open", json!({ "projects": uris }))
        }
        None => Ok(()),
    }
}

fn path_to_file_uri(path: &Path) -> String {
    super::transport::path_to_file_uri(path)
}

/// True when `path` looks like the Roslyn language-server DLL.
pub fn is_roslyn_dll(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.eq_ignore_ascii_case(ROSLYN_DLL_NAME) {
        return true;
    }
    let is_dll = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dll"));
    is_dll && name.to_ascii_lowercase().contains("languageserver")
}

/// MSBuild hook through which Roslyn's design-time builds pick up
/// [`msbuild_redirect`]'s props file. Imported by `Microsoft.Common.props`
/// right after the project's own `Directory.Build.props` and before the SDK
/// derives `IntermediateOutputPath` from `BaseIntermediateOutputPath`.
pub const MSBUILD_HOOK_ENV: &str = "CustomAfterDirectoryBuildProps";

/// Overrides where design-time build output goes (`<dir>/obj/...`). `off`
/// (or `0`, `false`, `project`) keeps MSBuild's default `obj/` in the project.
pub const ENV_ROSLYN_OBJ_DIR: &str = "ASTROLABE_ROSLYN_OBJ_DIR";

/// Environment for the Roslyn process so its design-time builds keep the
/// user's project tree clean.
///
/// Even with NuGet restore switched off, every project load runs a
/// design-time build that writes `obj/<Configuration>/<tfm>/`
/// (`AssemblyInfo.cs`, `GlobalUsings.g.cs`, `*.editorconfig`, `*.cache`)
/// into the user's project. A props file under Astrolabe's own cache points
/// `IntermediateOutputPath` (one subdirectory per project file) there
/// instead. `BaseIntermediateOutputPath` / `MSBuildProjectExtensionsPath` are
/// left alone: that is where an existing `project.assets.json` and the
/// `*.nuget.g.props` imports live, and moving them would make every package
/// reference unresolved. A project that sets `IntermediateOutputPath` itself
/// keeps its own value. A `CustomAfterDirectoryBuildProps` already in the
/// environment is still imported.
///
/// Returns the variable to set and the props path, or `None` when disabled
/// or the cache directory cannot be written (Roslyn then starts as before).
pub fn msbuild_redirect(root: &Path) -> Option<(&'static str, PathBuf)> {
    let base = match std::env::var_os(ENV_ROSLYN_OBJ_DIR).filter(|v| !v.is_empty()) {
        Some(v) => {
            let s = v.to_string_lossy().to_ascii_lowercase();
            if matches!(s.as_str(), "off" | "0" | "false" | "project") {
                return None;
            }
            PathBuf::from(v)
        }
        None => roslyn_cache_dir(root),
    };
    let chained = std::env::var_os(MSBUILD_HOOK_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let props = write_redirect_props(&base, chained.as_deref()).ok()?;
    Some((MSBUILD_HOOK_ENV, props))
}

/// `~/.astrolabe/roslyn/<repo>-<hash>`: one directory per repo root.
///
/// Unit tests (fake Roslyn transports included) keep it inside the root,
/// which is always a scratch directory there, so the suite never writes into
/// the real `$HOME`.
#[cfg(test)]
fn roslyn_cache_dir(root: &Path) -> PathBuf {
    root.join(".astrolabe-roslyn-test")
}

/// `~/.astrolabe/roslyn/<repo>-<hash>`: one directory per repo root.
#[cfg(not(test))]
fn roslyn_cache_dir(root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    let name = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let base = super::installer::default_servers_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("roslyn")
        .join(format!("{name}-{}", &hex::encode(digest)[..12]))
}

/// Write `<base>/astrolabe-obj.props` (only when its content changed) and
/// return its path.
fn write_redirect_props(base: &Path, chained: Option<&Path>) -> std::io::Result<PathBuf> {
    let obj = base.join("obj");
    std::fs::create_dir_all(&obj)?;
    let props = base.join("astrolabe-obj.props");
    let content = redirect_props_xml(&obj, chained, &props);
    if std::fs::read_to_string(&props).ok().as_deref() != Some(content.as_str()) {
        let tmp = base.join(format!("astrolabe-obj.props.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &props)?;
    }
    Ok(props)
}

fn redirect_props_xml(obj: &Path, chained: Option<&Path>, own: &Path) -> String {
    let mut dir = msbuild_literal(&obj.to_string_lossy());
    if !dir.ends_with('/') && !dir.ends_with('\\') {
        dir.push(std::path::MAIN_SEPARATOR);
    }
    let import = match chained.filter(|c| *c != own) {
        Some(c) => {
            let c = msbuild_literal(&c.to_string_lossy());
            format!("  <Import Project=\"{c}\" Condition=\"Exists('{c}')\" />\n")
        }
        None => String::new(),
    };
    // StableStringHash keeps two projects with the same file name in
    // different directories apart; the SDK appends `<tfm>/` itself.
    format!(
        "<!-- Written by Astrolabe: keeps Roslyn design-time build output out of the project's obj/. -->\n\
         <Project>\n\
         {import}  <PropertyGroup Condition=\"'$(IntermediateOutputPath)' == ''\">\n\
         \x20   <IntermediateOutputPath>{dir}$(MSBuildProjectName)-$([MSBuild]::StableStringHash($(MSBuildProjectFullPath)))/$(Configuration)/</IntermediateOutputPath>\n\
         \x20 </PropertyGroup>\n\
         </Project>\n"
    )
}

/// A path as a literal MSBuild property value inside XML: MSBuild's special
/// characters `% $ @ ' ;` are %-escaped, then the XML ones.
fn msbuild_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '$' => out.push_str("%24"),
            '@' => out.push_str("%40"),
            '\'' => out.push_str("%27"),
            ';' => out.push_str("%3B"),
            '*' => out.push_str("%2A"),
            '?' => out.push_str("%3F"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
            "astrolabe-csharp-ws-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    #[test]
    fn strip_roslyn_symbol_name_examples() {
        assert_eq!(strip_roslyn_symbol_name("Add(int, int) : int"), "Add");
        assert_eq!(strip_roslyn_symbol_name("Name : string"), "Name");
        assert_eq!(strip_roslyn_symbol_name("ToString()"), "ToString");
        assert_eq!(
            strip_roslyn_symbol_name("Position : (int X, string Y)"),
            "Position"
        );
        assert_eq!(strip_roslyn_symbol_name("SimpleMethod"), "SimpleMethod");
        assert_eq!(strip_roslyn_symbol_name("  Add(int) : void  "), "Add");
    }

    #[test]
    fn prefers_shallowest_sln_over_deeper_sln_and_csproj() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("App.sln"), "sln").unwrap();
        fs::write(dir.0.join("src").join("Deep.sln"), "sln").unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Solution(p)) => {
                assert_eq!(p.file_name().unwrap(), "App.sln");
            }
            other => panic!("expected shallow solution, got {other:?}"),
        }
    }

    #[test]
    fn prefers_slnx_when_present_at_same_depth() {
        let dir = scratch();
        fs::write(dir.0.join("App.slnx"), "slnx").unwrap();
        fs::write(dir.0.join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Solution(p)) => {
                assert!(p.extension().unwrap().to_string_lossy().starts_with("sln"));
            }
            other => panic!("expected solution, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_csproj_skipping_bin_obj_vs() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("bin")).unwrap();
        fs::create_dir_all(dir.0.join("obj")).unwrap();
        fs::create_dir_all(dir.0.join(".vs")).unwrap();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("bin").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join("obj").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join(".vs").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                assert_eq!(ps.len(), 1);
                assert_eq!(ps[0].file_name().unwrap(), "App.csproj");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_vbproj_when_no_solution() {
        let dir = scratch();
        fs::write(dir.0.join("App.vbproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                assert_eq!(ps.len(), 1);
                assert_eq!(ps[0].file_name().unwrap(), "App.vbproj");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }

    #[test]
    fn collects_vbproj_alongside_csproj() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        fs::write(dir.0.join("src").join("Lib.vbproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                let names: Vec<String> = ps
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                assert!(names.iter().any(|n| n == "App.csproj"), "{names:?}");
                assert!(names.iter().any(|n| n == "Lib.vbproj"), "{names:?}");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }

    #[test]
    fn redirect_props_points_intermediate_output_at_cache() {
        let s = scratch();
        let base = s.0.join("cache dir");
        let props = write_redirect_props(&base, None).unwrap();
        let xml = fs::read_to_string(&props).unwrap();
        assert!(base.join("obj").is_dir());
        assert!(
            xml.contains("<PropertyGroup Condition=\"'$(IntermediateOutputPath)' == ''\">"),
            "{xml}"
        );
        let obj = base.join("obj").to_string_lossy().into_owned();
        assert!(
            xml.contains(&format!(
                "<IntermediateOutputPath>{obj}{}$(MSBuildProjectName)-",
                std::path::MAIN_SEPARATOR
            )),
            "{xml}"
        );
        // Restore outputs stay where NuGet put them.
        assert!(!xml.contains("BaseIntermediateOutputPath"), "{xml}");
        assert!(!xml.contains("MSBuildProjectExtensionsPath"), "{xml}");
        assert!(!xml.contains("<Import"), "{xml}");
        // Unchanged content is not rewritten.
        let before = fs::metadata(&props).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_redirect_props(&base, None).unwrap();
        assert_eq!(fs::metadata(&props).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn redirect_props_chains_existing_hook_and_escapes() {
        let s = scratch();
        let base = s.0.join("a$b;c'd&e");
        let user = s.0.join("user.props");
        let props = write_redirect_props(&base, Some(&user)).unwrap();
        let xml = fs::read_to_string(&props).unwrap();
        let user = user.to_string_lossy().into_owned();
        assert!(
            xml.contains(&format!(
                "<Import Project=\"{user}\" Condition=\"Exists('{user}')\" />"
            )),
            "{xml}"
        );
        assert!(xml.contains("a%24b%3Bc%27d&amp;e"), "{xml}");
        // Never imports itself.
        let again = write_redirect_props(&base, Some(&props)).unwrap();
        assert!(!fs::read_to_string(again).unwrap().contains("<Import"));
    }

    #[test]
    fn test_cache_dir_stays_inside_scratch_root() {
        let s = scratch();
        let dir = roslyn_cache_dir(&s.0);
        assert!(dir.starts_with(&s.0), "{}", dir.display());
    }
}
