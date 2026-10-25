//! C# project and namespace resolution for SDK-style projects.
//!
//! `detect` reads every `*.csproj` outside `bin` / `obj` / `.vs`, plus the
//! nearest `Directory.Build.props` walking upward from the project directory
//! (MSBuild `GetPathOfFileAbove` — one file, the closest one). The csproj
//! overrides the props. SDK defaults fill whatever is still empty:
//! `RootNamespace` is the project file name, and `Microsoft.NET.Sdk*` turns
//! on default compile items (`.cs` and `.csx` under the project). In-repo
//! `.razor` and `.cshtml` are included in that set as well, but only after
//! [`crate::parse::extract_razor_csharp`] pulls C# out of them. Raw markup is
//! never scanned. That is what lets a namespace declared in `@code` satisfy a
//! `_ViewImports.cshtml` `@using`. Names no file declares (NuGet, the BCL
//! including `System.*`) resolve to nothing. A `System.*` namespace that this
//! repo declares itself (dotnet/runtime, `System.Reactive`) is a normal
//! namespace and is linked. This is Astrolabe's index set, not an MSBuild
//! `Compile` item.
//!
//! `Compile Include` / `Remove` take `;`-separated lists and MSBuild globs:
//! `*` and `?` stay within one path segment, `**` spans segments, also in the
//! middle (`Gen/**/Auto/*.cs`). Each project's files are bucketed once by
//! owning project directory, so `detect` is linear in files, not
//! projects x files.
//!
//! # Import specs
//!
//! The parser (other branch) should emit either a real using line or one of
//! these prefixes. Both are accepted:
//!
//! * `using Acme.Lib;` / `ns:Acme.Lib` / `global using Acme.Lib;` —
//!   namespace relationship. Every file that declares `Acme.Lib`, partial
//!   types included. Never a single-file import, even when only one file
//!   declares it.
//! * `using static Acme.Lib.Helper;` / `static:Acme.Lib.Helper` — one file,
//!   and only when exactly one in-repo file declares that type. Two partials
//!   are not one target.
//! * `using W = Acme.Lib.Helper;` / `alias:Acme.Lib.Helper` — same rule.
//! * `project:../lib/Lib.csproj` — the in-repo `ProjectReference`, one file.
//!
//! A name no file declares (NuGet, the BCL, implicit usings) resolves to
//! nothing. A namespace declared by more than `NAMESPACE_FANOUT_CAP` files
//! links to the nearest ones only (see `namespace_files`).
//!
//! # ProjectReference is metadata, not a graph edge
//!
//! `ProjectMeta::project_refs` records each in-repo `<ProjectReference>` as a
//! manifest-to-manifest pair and `project:` specs resolve through it, but
//! `.csproj` files are not parsed source files, so no node exists for them
//! and `assemble_graph` adds no edge for these pairs. Project-level
//! dependencies show up in the graph only through the `using` namespace
//! edges of the files that actually use the referenced project.
//!
//! `Condition` attributes (on the element and on every enclosing
//! `PropertyGroup` / `ItemGroup` / `When`) are evaluated three-valued:
//! `==`, `!=`, `!`, `and`, `or`, parentheses and quoted literals, with only
//! the tracked properties known. `Exists(..)`, `$(Configuration)`,
//! `$(TargetFramework)` and the like are Unknown. For `Compile`, False
//! drops the element, True applies it, and Unknown is kept conservatively:
//! an `Include` still adds files, a `Remove` does not remove. Property
//! elements apply only on True (an Unknown group condition applies as if
//! absent).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::types::{
    join_rel, FileIndex, ImportResolution, Language, ModuleResolver, ModuleUnit, NamespaceFiles,
    ProjectFacts, ProjectMeta, ProjectRef, RelPath, TypeDecl,
};

pub struct CSharpResolver;

/// Most declaring files one `using Namespace;` links to; see
/// [`namespace_files`].
const NAMESPACE_FANOUT_CAP: usize = 64;

const SDK_USINGS: &[&str] = &[
    "System",
    "System.Collections.Generic",
    "System.IO",
    "System.Linq",
    "System.Net.Http",
    "System.Threading",
    "System.Threading.Tasks",
];

const WEB_USINGS: &[&str] = &[
    "System.Net.Http.Json",
    "Microsoft.AspNetCore.Builder",
    "Microsoft.AspNetCore.Hosting",
    "Microsoft.AspNetCore.Http",
    "Microsoft.AspNetCore.Routing",
    "Microsoft.Extensions.Configuration",
    "Microsoft.Extensions.DependencyInjection",
    "Microsoft.Extensions.Hosting",
    "Microsoft.Extensions.Logging",
];

const WORKER_USINGS: &[&str] = &[
    "Microsoft.Extensions.Configuration",
    "Microsoft.Extensions.DependencyInjection",
    "Microsoft.Extensions.Hosting",
    "Microsoft.Extensions.Logging",
];

#[derive(Default)]
struct BuildProps {
    sdk: String,
    root_namespace: Option<String>,
    implicit_usings: Option<bool>,
    enable_default_items: Option<bool>,
    enable_default_compile: Option<bool>,
    base_output: Option<String>,
    base_intermediate: Option<String>,
    project_ref_includes: Vec<String>,
    compile_include: Vec<String>,
    compile_remove: Vec<String>,
    global_usings: Vec<String>,
    global_static_usings: Vec<String>,
}

impl ModuleResolver for CSharpResolver {
    fn language(&self) -> Language {
        Language::CSharp
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut manifests: Vec<String> = files
            .iter()
            .filter(|p| is_csproj(p.file_name()) && !skipped_tree(p.as_str()))
            .map(|p| p.as_str().to_string())
            .collect();
        manifests.sort();

        let mut projects: Vec<ProjectFacts> = Vec::new();
        let mut project_refs: Vec<ProjectRef> = Vec::new();
        let mut excludes: BTreeSet<String> = BTreeSet::new();
        let mut prepared: Vec<Prepared> = Vec::new();

        if !manifests.is_empty() {
            excludes.insert(".vs".into());
        }

        for manifest in &manifests {
            let Some(csproj) = files.read(manifest) else {
                continue;
            };
            let dir = RelPath::new(manifest).dir().to_string();
            let mut props = BuildProps::default();
            if let Some(text) = nearest_directory_build_props(&dir, files) {
                apply_xml(&text, &mut props);
            }
            apply_xml(&csproj, &mut props);

            let sdk_style = props.sdk.contains("Microsoft.NET.Sdk");
            let default_items = props.enable_default_items.unwrap_or(sdk_style);
            let default_compile = match props.enable_default_compile {
                Some(value) => value && default_items,
                None => default_items,
            };
            let project_name = csproj_name(manifest);
            let root_namespace = props
                .root_namespace
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| project_name.clone());
            let output = output_dir(props.base_output.as_deref(), "bin");
            let intermediate = output_dir(props.base_intermediate.as_deref(), "obj");
            excludes.insert(join_dir(&dir, &output));
            excludes.insert(join_dir(&dir, &intermediate));
            excludes.insert(join_dir(&dir, ".vs"));

            let mut implicit = Vec::new();
            if props.implicit_usings == Some(true) {
                implicit.extend(
                    implicit_usings_for(&props.sdk)
                        .into_iter()
                        .map(str::to_string),
                );
            }

            let facts = ProjectFacts {
                manifest: manifest.clone(),
                directory: dir.clone(),
                sdk: props.sdk.clone(),
                root_namespace: root_namespace.clone(),
                implicit_usings: implicit,
                global_usings: props.global_usings.clone(),
                global_static_usings: props.global_static_usings.clone(),
                default_compile_items: default_compile,
            };

            for include in &props.project_ref_includes {
                if let Some(target) = normalize_include(&dir, include) {
                    if files.contains(&target) && target != *manifest {
                        project_refs.push(ProjectRef {
                            from: manifest.clone(),
                            to: target,
                        });
                    }
                }
            }

            prepared.push(Prepared {
                facts,
                compile_include: props.compile_include,
                compile_remove: props.compile_remove,
                output,
                intermediate,
            });
            projects.push(prepared.last().unwrap().facts.clone());
        }

        let directories: Vec<(String, String)> = projects
            .iter()
            .map(|p| (p.manifest.clone(), p.directory.clone()))
            .collect();

        // One pass: which project owns each C# source (deepest project
        // directory above it; the last manifest wins a tie, as before).
        let mut dir_owner: HashMap<&str, &str> = HashMap::new();
        for (manifest, dir) in &directories {
            dir_owner.insert(dir.as_str(), manifest.as_str());
        }
        let mut buckets: HashMap<&str, Vec<&str>> = HashMap::new();
        for p in files.iter() {
            let path = p.as_str();
            if !is_csharp_source(path) || skipped_tree(path) {
                continue;
            }
            let mut dir = p.dir();
            let owner = loop {
                if let Some(m) = dir_owner.get(dir) {
                    break Some(*m);
                }
                if dir.is_empty() {
                    break None;
                }
                dir = dir.rfind('/').map_or("", |i| &dir[..i]);
            };
            if let Some(m) = owner {
                buckets.entry(m).or_default().push(path);
            }
        }

        let mut ns_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut type_decls: BTreeSet<TypeDecl> = BTreeSet::new();
        let mut source_roots: BTreeSet<String> = BTreeSet::new();

        for prep in &prepared {
            if prep.facts.default_compile_items || !prep.compile_include.is_empty() {
                source_roots.insert(prep.facts.directory.clone());
            }
            let bucket = buckets
                .get(prep.facts.manifest.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            // Read + extract in parallel (chunked to bound memory); the merge
            // into the sets below stays sequential, in the same path order.
            let compiled = compile_files(files, prep, bucket);
            for chunk in compiled.chunks(crate::types::READ_CHUNK) {
                use rayon::prelude::*;
                let extracted: Vec<Option<Declarations>> = chunk
                    .par_iter()
                    .map(|path| {
                        let text = files.read(path)?;
                        let text = declarations_source(path, text);
                        Some(extract_declarations(&text))
                    })
                    .collect();
                for (path, got) in chunk.iter().zip(extracted) {
                    let Some((namespaces, types)) = got else {
                        continue;
                    };
                    // Namespaces the repo itself declares are kept even under
                    // `System.*` (dotnet/runtime, System.Reactive, ...): a name
                    // is only treated as framework when no file here declares it.
                    for ns in namespaces {
                        ns_files.entry(ns).or_default().insert(path.clone());
                    }
                    for (ns, name) in types {
                        if name.is_empty() {
                            continue;
                        }
                        type_decls.insert(TypeDecl {
                            namespace: ns,
                            name,
                            file: path.clone(),
                        });
                    }
                }
            }
        }

        let mut modules: Vec<ModuleUnit> = projects
            .iter()
            .filter(|p| !p.root_namespace.is_empty())
            .map(|p| ModuleUnit {
                name: p.root_namespace.clone(),
                dir: p.directory.clone(),
            })
            .collect();
        modules.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.name.cmp(&b.name)));
        modules.dedup();

        project_refs.sort();
        project_refs.dedup();
        projects.sort_by(|a, b| a.manifest.cmp(&b.manifest));

        let namespaces = ns_files
            .into_iter()
            .map(|(namespace, files)| NamespaceFiles {
                namespace,
                files: files.into_iter().collect(),
            })
            .collect();

        ProjectMeta {
            source_roots: source_roots.into_iter().collect(),
            modules,
            excludes: excludes.into_iter().collect(),
            overrides: Vec::new(),
            projects,
            project_refs,
            namespaces,
            type_decls: type_decls.into_iter().collect(),
            scope_index: Default::default(),
        }
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        match self.resolve_import(from, spec, files, meta) {
            ImportResolution::File(path) => Some(path),
            ImportResolution::Files(_)
            | ImportResolution::Namespace { .. }
            | ImportResolution::Unresolved => None,
        }
    }

    fn resolve_import(
        &self,
        from: &RelPath,
        spec: &str,
        _files: &FileIndex,
        meta: &ProjectMeta,
    ) -> ImportResolution {
        match classify_spec(spec) {
            Spec::Empty => ImportResolution::Unresolved,
            Spec::Project(raw) => match resolve_project(from, &raw, meta) {
                Some(path) => ImportResolution::File(path),
                None => ImportResolution::Unresolved,
            },
            Spec::Static(name) | Spec::Alias(name) => match unique_type(meta, &name) {
                Some(path) => ImportResolution::File(RelPath::new(path)),
                None => ImportResolution::Unresolved,
            },
            Spec::Namespace(name) => {
                if name.is_empty() {
                    return ImportResolution::Unresolved;
                }
                // Framework namespaces resolve to nothing unless this repo
                // declares them itself.
                let files = namespace_files(meta, &name, from);
                if files.is_empty() {
                    ImportResolution::Unresolved
                } else {
                    ImportResolution::Namespace { name, files }
                }
            }
        }
    }
}

struct Prepared {
    facts: ProjectFacts,
    compile_include: Vec<String>,
    compile_remove: Vec<String>,
    output: String,
    intermediate: String,
}

enum Spec {
    Empty,
    Project(String),
    Static(String),
    Alias(String),
    Namespace(String),
}

fn classify_spec(spec: &str) -> Spec {
    let spec = spec
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim()
        .trim_end_matches(';')
        .trim();
    let mut spec = spec;
    loop {
        if let Some(rest) = strip_ignore_ascii_case(spec, "global ") {
            spec = rest.trim_start();
            continue;
        }
        break;
    }
    if spec.is_empty() {
        return Spec::Empty;
    }
    if let Some(rest) = spec.strip_prefix("project:") {
        return Spec::Project(rest.trim().to_string());
    }
    if let Some(rest) = spec.strip_prefix("ns:") {
        return Spec::Namespace(rest.trim().to_string());
    }
    if let Some(rest) = spec.strip_prefix("static:") {
        return Spec::Static(rest.trim().to_string());
    }
    if let Some(rest) = spec.strip_prefix("alias:") {
        return Spec::Alias(rest.trim().to_string());
    }
    if spec.to_ascii_lowercase().ends_with(".csproj") {
        return Spec::Project(spec.to_string());
    }
    if let Some(rest) = strip_ignore_ascii_case(spec, "using ") {
        let rest = rest.trim_start().trim_end_matches(';').trim();
        if let Some(rest) = strip_ignore_ascii_case(rest, "static ") {
            return Spec::Static(rest.trim().trim_end_matches(';').trim().to_string());
        }
        if let Some((_, target)) = rest.split_once('=') {
            return Spec::Alias(target.trim().trim_end_matches(';').trim().to_string());
        }
        return Spec::Namespace(rest.to_string());
    }
    if spec.contains('/') || spec.contains('\\') {
        return Spec::Empty;
    }
    Spec::Namespace(spec.to_string())
}

fn resolve_project(from: &RelPath, raw: &str, meta: &ProjectMeta) -> Option<RelPath> {
    let owner = project_owning(from.as_str(), meta)?;
    let raw = raw.trim().trim_matches('"').replace('\\', "/");
    for pref in &meta.project_refs {
        if pref.from != owner.manifest {
            continue;
        }
        let mut candidates = Vec::new();
        if let Some(joined) = join_rel(from.dir(), &raw) {
            candidates.push(joined);
        }
        if let Some(joined) = join_rel(&owner.directory, &raw) {
            candidates.push(joined);
        }
        let bare = RelPath::new(&raw);
        if !bare.as_str().is_empty() {
            candidates.push(bare.as_str().to_string());
        }
        if candidates.iter().any(|c| c == &pref.to) {
            return Some(RelPath::new(&pref.to));
        }
    }
    None
}

fn project_owning<'a>(path: &str, meta: &'a ProjectMeta) -> Option<&'a ProjectFacts> {
    meta.projects
        .iter()
        .filter(|p| p.manifest == path || is_under(path, &p.directory))
        .max_by_key(|p| p.directory.len())
}

/// Files declaring `name` (the importer itself is dropped only when the
/// cap applies; `assemble_graph` filters self-edges). A namespace with more than
/// [`NAMESPACE_FANOUT_CAP`] declaring files links only to the cap nearest
/// ones (longest shared directory prefix, then path order), so a single huge
/// namespace costs O(importers x cap) edges instead of O(importers x files).
/// Nothing in the query layer reads `Namespace` edges; `Import` edges, which
/// carry dependents and PageRank, are not affected.
fn namespace_files(meta: &ProjectMeta, name: &str, from: &RelPath) -> Vec<RelPath> {
    let Ok(i) = meta
        .namespaces
        .binary_search_by(|n| n.namespace.as_str().cmp(name))
    else {
        return Vec::new();
    };
    let all = &meta.namespaces[i].files;
    let mut picked: Vec<&String> = all.iter().collect();
    if picked.len() > NAMESPACE_FANOUT_CAP {
        picked.retain(|f| f.as_str() != from.as_str());
        let from_dir = from.dir();
        picked.select_nth_unstable_by(NAMESPACE_FANOUT_CAP, |a, b| {
            shared_dirs(b, from_dir)
                .cmp(&shared_dirs(a, from_dir))
                .then_with(|| a.cmp(b))
        });
        picked.truncate(NAMESPACE_FANOUT_CAP);
        picked.sort();
    }
    picked.into_iter().map(RelPath::new).collect()
}

/// Number of leading directory components `path` shares with `dir`.
fn shared_dirs(path: &str, dir: &str) -> usize {
    let pd = path.rsplit_once('/').map_or("", |(d, _)| d);
    pd.split('/')
        .zip(dir.split('/'))
        .take_while(|(a, b)| a == b && !a.is_empty())
        .count()
}

fn unique_type(meta: &ProjectMeta, qualified: &str) -> Option<String> {
    let qualified = qualified.trim().trim_end_matches(';').trim();
    if qualified.is_empty() {
        return None;
    }
    let direct = files_declaring(meta, qualified);
    if !direct.is_empty() {
        return only_one(direct);
    }
    let (head, _) = qualified.rsplit_once('.')?;
    only_one(files_declaring(meta, head))
}

fn files_declaring(meta: &ProjectMeta, qualified: &str) -> Vec<String> {
    // `type_decls` is sorted by (namespace, name, file) (BTreeSet order in
    // `detect`) and a declared name is a single identifier, so the only split
    // of `qualified` that can match is at its last dot: binary-search that
    // (namespace, name) range instead of scanning every declaration.
    let (ns, name) = match qualified.rsplit_once('.') {
        Some((ns, name)) => (ns, name),
        None => ("", qualified),
    };
    let decls = &meta.type_decls;
    let key = |t: &TypeDecl| (t.namespace.as_str().cmp(ns)).then_with(|| t.name.as_str().cmp(name));
    let start = decls.partition_point(|t| key(t).is_lt());
    let mut files: Vec<String> = decls[start..]
        .iter()
        .take_while(|t| key(t).is_eq())
        .filter(|t| decl_is(t, qualified))
        .map(|t| t.file.clone())
        .collect();
    files.sort();
    files.dedup();
    files
}

fn only_one(files: Vec<String>) -> Option<String> {
    if files.len() == 1 {
        files.into_iter().next()
    } else {
        None
    }
}

/// `qualified_name(decl) == qualified` without allocating.
fn decl_is(decl: &TypeDecl, qualified: &str) -> bool {
    if decl.namespace.is_empty() {
        return decl.name == qualified;
    }
    qualified.len() == decl.namespace.len() + 1 + decl.name.len()
        && qualified.starts_with(decl.namespace.as_str())
        && qualified[decl.namespace.len()..].starts_with('.')
        && qualified.ends_with(decl.name.as_str())
}

fn implicit_usings_for(sdk: &str) -> Vec<&'static str> {
    let mut out: Vec<&str> = Vec::new();
    if sdk.contains("Microsoft.NET.Sdk.Web") {
        out.extend(SDK_USINGS);
        out.extend(WEB_USINGS);
    } else if sdk.contains("Microsoft.NET.Sdk.Worker") {
        out.extend(SDK_USINGS);
        out.extend(WORKER_USINGS);
    } else if sdk.contains("Microsoft.NET.Sdk") {
        out.extend(SDK_USINGS);
    }
    out
}

/// Compile items of one project. `bucket` is every C# source (not under
/// `bin`/`obj`/`.vs`) that the project owns, computed once for all projects.
fn compile_files(files: &FileIndex, prep: &Prepared, bucket: &[&str]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let dir = &prep.facts.directory;
    let remove = Patterns::new(&prep.compile_remove);
    if prep.facts.default_compile_items {
        for &path in bucket {
            if is_project_output(path, dir, &prep.output, &prep.intermediate) {
                continue;
            }
            if remove.is_match(rel_to(path, dir)) {
                continue;
            }
            out.insert(path.to_string());
        }
    }
    for include in &prep.compile_include {
        add_include(files, dir, include, bucket, &mut out);
    }
    out.retain(|path| !remove.is_match(rel_to(path, dir)));
    out.into_iter().filter(|path| !skipped_tree(path)).collect()
}

fn add_include(
    files: &FileIndex,
    dir: &str,
    pattern: &str,
    bucket: &[&str],
    out: &mut BTreeSet<String>,
) {
    let pattern = pattern.replace('\\', "/");
    let mut wildcards: Vec<String> = Vec::new();
    // MSBuild lists are `;`-separated.
    for item in pattern.split(';').map(str::trim).filter(|i| !i.is_empty()) {
        if item.contains('*') || item.contains('?') {
            wildcards.push(item.to_string());
        } else if let Some(full) = normalize_include(dir, item) {
            if files.contains(&full) && is_csharp_source(&full) && !skipped_tree(&full) {
                out.insert(full);
            }
        }
    }
    if wildcards.is_empty() {
        return;
    }
    let set = Patterns::new(&wildcards);
    for &path in bucket {
        if set.is_match(rel_to(path, dir)) {
            out.insert(path.to_string());
        }
    }
}

fn is_project_output(path: &str, dir: &str, output: &str, intermediate: &str) -> bool {
    let rel = rel_to(path, dir);
    [output, intermediate, ".vs"].into_iter().any(|root| {
        rel == root
            || rel
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// MSBuild item patterns: `;`-separated, `*` and `?` stay inside one path
/// segment, `**` spans segments (also in the middle: `Foo/**/Bar/*.cs`).
/// Patterns without wildcards are exact relative paths.
struct Patterns {
    exact: BTreeSet<String>,
    globs: globset::GlobSet,
}

impl Patterns {
    fn new(list: &[String]) -> Self {
        let mut exact = BTreeSet::new();
        let mut builder = globset::GlobSetBuilder::new();
        for raw in list {
            for item in raw.split(';') {
                let item = item.trim().replace('\\', "/");
                let item = item.trim_start_matches("./");
                if item.is_empty() {
                    continue;
                }
                if item.contains('*') || item.contains('?') {
                    if let Ok(g) = globset::GlobBuilder::new(item)
                        .literal_separator(true)
                        .build()
                    {
                        builder.add(g);
                    }
                } else {
                    exact.insert(item.to_string());
                }
            }
        }
        let globs = builder
            .build()
            .unwrap_or_else(|_| globset::GlobSet::empty());
        Patterns { exact, globs }
    }

    fn is_match(&self, rel: &str) -> bool {
        let rel = rel.trim_start_matches("./");
        self.exact.contains(rel) || self.globs.is_match(rel)
    }
}

fn nearest_directory_build_props(project_dir: &str, files: &FileIndex) -> Option<String> {
    let mut dir = project_dir.to_string();
    loop {
        if let Some(text) = props_in_dir(&dir, files) {
            return Some(text);
        }
        if dir.is_empty() {
            return None;
        }
        dir = match dir.rfind('/') {
            Some(i) => dir[..i].to_string(),
            None => String::new(),
        };
    }
}

fn props_in_dir(dir: &str, files: &FileIndex) -> Option<String> {
    let direct = join_dir(dir, "Directory.Build.props");
    if files.contains(&direct) {
        return files.read(&direct);
    }
    let hit = files
        .entries(dir)
        .iter()
        .find(|p| p.file_name().eq_ignore_ascii_case("Directory.Build.props"))?;
    files.read(hit.as_str())
}

fn apply_xml(xml: &str, props: &mut BuildProps) {
    for tag in xml_tags(xml) {
        // Enclosing group conditions: definitely False drops the element.
        let group = tag
            .conds
            .iter()
            .map(|c| eval_condition(c, props))
            .fold(Tri::True, tri_and);
        if group == Tri::False {
            continue;
        }
        let own = match tag.attr("Condition") {
            Some(cond) => eval_condition(cond, props),
            None => Tri::True,
        };
        if own == Tri::False {
            continue;
        }
        let is_compile = tag.name == "Compile";
        // Compile keeps Unknown (see module docs); everything else needs True.
        if !is_compile && own != Tri::True {
            continue;
        }
        let certain = own == Tri::True && group == Tri::True;
        match tag.name.as_str() {
            "Project" => {
                if let Some(sdk) = tag.attr("Sdk") {
                    if !sdk.is_empty() {
                        props.sdk = sdk.to_string();
                    }
                }
            }
            "RootNamespace" => set_text(&tag, |v| props.root_namespace = Some(v)),
            "ImplicitUsings" => {
                if let Some(v) = tag.body_text().and_then(|t| parse_bool(&t)) {
                    props.implicit_usings = Some(v);
                }
            }
            "EnableDefaultItems" => {
                if let Some(v) = tag.body_text().and_then(|t| parse_bool(&t)) {
                    props.enable_default_items = Some(v);
                }
            }
            "EnableDefaultCompileItems" => {
                if let Some(v) = tag.body_text().and_then(|t| parse_bool(&t)) {
                    props.enable_default_compile = Some(v);
                }
            }
            "BaseOutputPath" | "OutputPath" => {
                if props.base_output.is_none() || tag.name == "BaseOutputPath" {
                    set_text(&tag, |v| props.base_output = Some(v));
                }
            }
            "BaseIntermediateOutputPath" | "IntermediateOutputPath" => {
                if props.base_intermediate.is_none() || tag.name == "BaseIntermediateOutputPath" {
                    set_text(&tag, |v| props.base_intermediate = Some(v));
                }
            }
            "ProjectReference" => {
                if let Some(include) = tag.attr("Include") {
                    props.project_ref_includes.push(include.to_string());
                }
            }
            "Compile" => {
                if let Some(include) = tag.attr("Include") {
                    props.compile_include.push(include.to_string());
                }
                if let Some(remove) = tag.attr("Remove") {
                    if certain {
                        props.compile_remove.push(remove.to_string());
                    }
                }
            }
            "Using" => {
                if let Some(include) = tag.attr("Include") {
                    let include = include.trim().to_string();
                    if include.is_empty() {
                        continue;
                    }
                    let is_static = tag.attr("Static").and_then(parse_bool).unwrap_or(false);
                    if is_static {
                        props.global_static_usings.push(include);
                    } else {
                        props.global_usings.push(include);
                    }
                }
            }
            _ => {}
        }
    }
}

fn set_text(tag: &XmlTag, set: impl FnOnce(String)) {
    if let Some(text) = tag.body_text() {
        if !text.is_empty() {
            set(text);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

fn tri_and(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn tri_or(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::False, Tri::False) => Tri::False,
        _ => Tri::Unknown,
    }
}

fn tri_not(a: Tri) -> Tri {
    match a {
        Tri::True => Tri::False,
        Tri::False => Tri::True,
        Tri::Unknown => Tri::Unknown,
    }
}

/// Value of a tracked property; unset reads as empty. Other names: unknown.
fn lookup_prop(name: &str, props: &BuildProps) -> Option<String> {
    let b = |v: Option<bool>| Some(v.map(|x| x.to_string()).unwrap_or_default());
    match name.to_ascii_lowercase().as_str() {
        "rootnamespace" => Some(props.root_namespace.clone().unwrap_or_default()),
        "implicitusings" => b(props.implicit_usings),
        "enabledefaultitems" => b(props.enable_default_items),
        "enabledefaultcompileitems" => b(props.enable_default_compile),
        "baseoutputpath" | "outputpath" => Some(props.base_output.clone().unwrap_or_default()),
        "baseintermediateoutputpath" | "intermediateoutputpath" => {
            Some(props.base_intermediate.clone().unwrap_or_default())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CTok {
    Str(String),
    Word(String),
    LParen,
    RParen,
    Eq,
    Ne,
    Not,
    Comma,
    Bad,
}

fn lex_condition(cond: &str) -> Vec<CTok> {
    let c: Vec<char> = cond.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        match c[i] {
            ch if ch.is_whitespace() => i += 1,
            '(' => {
                out.push(CTok::LParen);
                i += 1;
            }
            ')' => {
                out.push(CTok::RParen);
                i += 1;
            }
            ',' => {
                out.push(CTok::Comma);
                i += 1;
            }
            '=' if c.get(i + 1) == Some(&'=') => {
                out.push(CTok::Eq);
                i += 2;
            }
            '!' if c.get(i + 1) == Some(&'=') => {
                out.push(CTok::Ne);
                i += 2;
            }
            '!' => {
                out.push(CTok::Not);
                i += 1;
            }
            q @ ('\'' | '"') => {
                let mut j = i + 1;
                while j < c.len() && c[j] != q {
                    j += 1;
                }
                if j >= c.len() {
                    out.push(CTok::Bad);
                    break;
                }
                out.push(CTok::Str(c[i + 1..j].iter().collect()));
                i = j + 1;
            }
            ch if ch.is_alphanumeric() || ch == '_' => {
                let mut j = i;
                while j < c.len() && (c[j].is_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
                out.push(CTok::Word(c[i..j].iter().collect()));
                i = j;
            }
            _ => {
                out.push(CTok::Bad);
                break;
            }
        }
    }
    out
}

/// Expand `$(Name)` in a quoted literal. `None` when any piece is unknown
/// (`@(..)`, `%(..)`, untracked property).
fn expand_literal(text: &str, props: &BuildProps) -> Option<String> {
    if text.contains("@(") || text.contains("%(") {
        return None;
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("$(") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = after.find(')')?;
        out.push_str(&lookup_prop(after[..end].trim(), props)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

struct CondParser<'a> {
    toks: Vec<CTok>,
    pos: usize,
    props: &'a BuildProps,
}

enum Operand {
    Lit(Option<String>),
    Tri(Tri),
}

impl CondParser<'_> {
    fn peek_word(&self, w: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(CTok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn or(&mut self) -> Tri {
        let mut v = self.and();
        while self.peek_word("or") {
            self.pos += 1;
            let r = self.and();
            v = tri_or(v, r);
        }
        v
    }

    fn and(&mut self) -> Tri {
        let mut v = self.not();
        while self.peek_word("and") {
            self.pos += 1;
            let r = self.not();
            v = tri_and(v, r);
        }
        v
    }

    fn not(&mut self) -> Tri {
        if self.toks.get(self.pos) == Some(&CTok::Not) {
            self.pos += 1;
            return tri_not(self.not());
        }
        self.cmp()
    }

    fn operand(&mut self) -> Operand {
        match self.toks.get(self.pos).cloned() {
            Some(CTok::Str(s)) => {
                self.pos += 1;
                Operand::Lit(expand_literal(&s, self.props))
            }
            Some(CTok::LParen) => {
                self.pos += 1;
                let v = self.or();
                if self.toks.get(self.pos) == Some(&CTok::RParen) {
                    self.pos += 1;
                    Operand::Tri(v)
                } else {
                    self.pos = self.toks.len();
                    Operand::Tri(Tri::Unknown)
                }
            }
            Some(CTok::Word(_)) => {
                // `Exists(..)`, `HasTrailingSlash(..)`, bare words: Unknown.
                self.pos += 1;
                if self.toks.get(self.pos) == Some(&CTok::LParen) {
                    let mut depth = 0;
                    while let Some(t) = self.toks.get(self.pos) {
                        match t {
                            CTok::LParen => depth += 1,
                            CTok::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    self.pos += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        self.pos += 1;
                    }
                }
                Operand::Tri(Tri::Unknown)
            }
            _ => {
                self.pos = self.toks.len();
                Operand::Tri(Tri::Unknown)
            }
        }
    }

    fn cmp(&mut self) -> Tri {
        let left = self.operand();
        let op = match self.toks.get(self.pos) {
            Some(CTok::Eq) => Some(true),
            Some(CTok::Ne) => Some(false),
            _ => None,
        };
        let Some(eq) = op else {
            return match left {
                Operand::Tri(t) => t,
                Operand::Lit(Some(s)) => match parse_bool(&s) {
                    Some(true) => Tri::True,
                    Some(false) => Tri::False,
                    None => Tri::Unknown,
                },
                Operand::Lit(None) => Tri::Unknown,
            };
        };
        self.pos += 1;
        let right = self.operand();
        match (left, right) {
            (Operand::Lit(Some(a)), Operand::Lit(Some(b))) => {
                if a.trim().eq_ignore_ascii_case(b.trim()) == eq {
                    Tri::True
                } else {
                    Tri::False
                }
            }
            _ => Tri::Unknown,
        }
    }
}

/// Three-valued MSBuild condition. Anything unparsed or unknown is Unknown.
fn eval_condition(cond: &str, props: &BuildProps) -> Tri {
    let toks = lex_condition(&decode_xml(cond));
    if toks.contains(&CTok::Bad) || toks.is_empty() {
        return Tri::Unknown;
    }
    let mut p = CondParser {
        toks,
        pos: 0,
        props,
    };
    let v = p.or();
    if p.pos < p.toks.len() {
        Tri::Unknown
    } else {
        v
    }
}

fn output_dir(configured: Option<&str>, default: &str) -> String {
    let raw = configured.unwrap_or(default).trim();
    let raw = raw.split('$').next().unwrap_or(raw);
    let raw = raw.replace('\\', "/");
    let raw = raw.trim_matches('/');
    let top = raw.split('/').next().unwrap_or(default);
    if top.is_empty() {
        default.to_string()
    } else {
        top.to_string()
    }
}

fn normalize_include(project_dir: &str, raw: &str) -> Option<String> {
    let mut raw = raw.trim().replace('\\', "/");
    if raw.contains("$(") {
        let dir = if project_dir.is_empty() {
            String::new()
        } else {
            format!("{project_dir}/")
        };
        for var in [
            "$(MSBuildProjectDirectory)/",
            "$(MSBuildProjectDirectory)",
            "$(ProjectDir)",
            "$(MSBuildThisFileDirectory)",
        ] {
            raw = raw.replace(
                var,
                if var.ends_with('/') {
                    &dir
                } else {
                    project_dir
                },
            );
        }
        raw = raw.replace("//", "/");
    }
    if raw.contains('$') || raw.starts_with('/') {
        return None;
    }
    join_rel(project_dir, &raw)
}

fn xml_tags(input: &str) -> Vec<XmlTag> {
    let input = strip_xml_comments(input);
    let b = input.as_bytes();
    let mut i = 0;
    let mut tags = Vec::new();
    // Open elements and their `Condition`, innermost last.
    let mut open: Vec<(String, Option<String>)> = Vec::new();
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'/') {
            let close_start = i + 2;
            i += 1;
            while i < b.len() && b[i] != b'>' {
                i += 1;
            }
            let close = input[close_start..i.min(b.len())].trim();
            if let Some(pos) = open.iter().rposition(|(n, _)| n == close) {
                open.truncate(pos);
            }
            i = (i + 1).min(b.len());
            continue;
        }
        if b.get(i + 1) == Some(&b'!') || b.get(i + 1) == Some(&b'?') {
            i += 1;
            while i < b.len() && b[i] != b'>' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            continue;
        }
        let start = i + 1;
        i += 1;
        while i < b.len() && b[i] != b'>' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let head = &input[start..i];
        let self_closing = head.trim_end().ends_with('/');
        let head = head.trim_end().trim_end_matches('/').trim();
        let (name, attrs) = split_tag(head);
        i += 1;
        let body_start = i;
        if !self_closing {
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
        }
        let body = if self_closing {
            String::new()
        } else {
            decode_xml(&input[body_start..i])
        };
        if !name.is_empty() {
            let conds: Vec<String> = open.iter().filter_map(|(_, c)| c.clone()).collect();
            if !self_closing {
                let own = attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("Condition"))
                    .map(|(_, v)| v.clone());
                open.push((name.clone(), own));
            }
            tags.push(XmlTag {
                name,
                attrs,
                body,
                conds,
            });
        }
    }
    tags
}

struct XmlTag {
    name: String,
    attrs: Vec<(String, String)>,
    body: String,
    /// `Condition`s of the enclosing elements, outermost first.
    conds: Vec<String>,
}

impl XmlTag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn body_text(&self) -> Option<String> {
        let text = self.body.trim();
        if text.is_empty() || text.contains('<') {
            None
        } else {
            Some(text.to_string())
        }
    }
}

fn split_tag(head: &str) -> (String, Vec<(String, String)>) {
    let mut chars = head.chars().peekable();
    let mut name = String::new();
    while let Some(c) = chars.peek().copied() {
        if c.is_whitespace() {
            break;
        }
        name.push(c);
        chars.next();
    }
    let rest: String = chars.collect();
    (name, parse_attrs(&rest))
}

fn parse_attrs(rest: &str) -> Vec<(String, String)> {
    let b = rest.as_bytes();
    let mut i = 0;
    let mut attrs = Vec::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = rest[start..i].trim().to_string();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' || key.is_empty() {
            break;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let quote = b[i];
        if quote != b'"' && quote != b'\'' {
            break;
        }
        i += 1;
        let vstart = i;
        while i < b.len() && b[i] != quote {
            i += 1;
        }
        let value = decode_xml(&rest[vstart..i]);
        if i < b.len() {
            i += 1;
        }
        attrs.push((key, value));
    }
    attrs
}

fn strip_xml_comments(input: &str) -> String {
    let b = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' && b.get(i + 1) == Some(&b'!') && b.get(i + 2) == Some(&b'-') {
            i += 4;
            while i + 2 < b.len() && !(b[i] == b'-' && b[i + 1] == b'-' && b[i + 2] == b'>') {
                i += 1;
            }
            i = (i + 3).min(b.len());
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    // `as char` is only correct for ASCII. XML we care about is ASCII markup;
    // non-ASCII bytes inside text are rare in csproj. Rebuild properly if a
    // byte was non-ascii by falling back to the original when needed.
    if input.is_ascii() {
        out
    } else {
        strip_xml_comments_utf8(input)
    }
}

fn strip_xml_comments_utf8(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 4..];
        if let Some(end) = rest.find("-->") {
            rest = &rest[end + 3..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn decode_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "enable" | "enabled" => Some(true),
        "false" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// `.cs` and `.csx` share `Language::CSharp` and are compile items, so a
/// `using` inside a script can become a namespace edge. `.razor` / `.cshtml`
/// are the same language after [`declarations_source`] extracts C#.
fn is_csharp_source(path: &str) -> bool {
    matches!(
        RelPath::new(path).extension(),
        Some("cs" | "csx" | "razor" | "cshtml")
    )
}

/// `.razor` / `.cshtml` contribute the extracted C# only. `.cs` and `.csx`
/// are unchanged.
fn declarations_source(path: &str, text: String) -> String {
    if matches!(RelPath::new(path).extension(), Some("razor" | "cshtml")) {
        crate::parse::extract_razor_csharp(&text)
    } else {
        text
    }
}

fn is_csproj(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".csproj")
}

fn csproj_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.trim_end_matches(' ')
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(name)
        .to_string()
}

fn skipped_tree(path: &str) -> bool {
    path.split('/')
        .any(|seg| seg == "bin" || seg == "obj" || seg == ".vs")
}

fn is_under(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return !path.is_empty();
    }
    path == dir || path.starts_with(&format!("{dir}/"))
}

fn rel_to<'a>(path: &'a str, dir: &str) -> &'a str {
    if dir.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{dir}/")).unwrap_or(path)
    }
}

fn join_dir(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{base}/{rest}"),
    }
}

fn strip_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// `(namespaces, (namespace, type name) pairs)` declared by one source file.
type Declarations = (BTreeSet<String>, Vec<(String, String)>);

fn extract_declarations(src: &str) -> Declarations {
    let masked = mask_csharp(src);
    let toks = tokenize(&masked);
    let mut ns_stack: Vec<String> = Vec::new();
    let mut file_scoped: Option<String> = None;
    let mut frames: Vec<bool> = Vec::new();
    let mut namespaces = BTreeSet::new();
    let mut types = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Ident(kw) if kw == "namespace" => {
                i += 1;
                let Some(name) = dotted(&toks, &mut i) else {
                    continue;
                };
                if matches!(toks.get(i), Some(Tok::Semi)) {
                    file_scoped = Some(name.clone());
                    namespaces.insert(name);
                    i += 1;
                } else if matches!(toks.get(i), Some(Tok::BraceOpen)) {
                    ns_stack.push(name);
                    frames.push(true);
                    namespaces.insert(current_ns(&file_scoped, &ns_stack));
                    i += 1;
                }
            }
            Tok::Ident(kw) if kw == "delegate" => {
                // `delegate R Name<T>(...)`: the name is the last identifier
                // outside `<>` before the parameter list. `delegate (..) {}`
                // and `delegate { }` are anonymous methods.
                i += 1;
                let (mut depth, mut last) = (0i32, None);
                while let Some(tok) = toks.get(i) {
                    match tok {
                        Tok::Ident(n) if depth == 0 => last = Some(n.clone()),
                        Tok::Lt => depth += 1,
                        Tok::Gt => depth = (depth - 1).max(0),
                        Tok::ParenOpen => break,
                        Tok::Semi | Tok::BraceOpen | Tok::BraceClose => {
                            last = None;
                            break;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if let Some(name) = last {
                    types.push((current_ns(&file_scoped, &ns_stack), name));
                }
            }
            Tok::Ident(kw)
                if is_type_keyword(kw)
                    && !(matches!(kw.as_str(), "class" | "struct")
                        && i > 0
                        && matches!(toks[i - 1], Tok::Colon | Tok::Comma)) =>
            {
                // (`where T : class, new()` is a constraint, not a type.)
                let keyword = kw.clone();
                i += 1;
                if keyword == "record" {
                    if let Some(Tok::Ident(next)) = toks.get(i) {
                        if next == "class" || next == "struct" || next == "interface" {
                            i += 1;
                        }
                    }
                }
                if let Some(Tok::Ident(name)) = toks.get(i) {
                    let ns = current_ns(&file_scoped, &ns_stack);
                    types.push((ns, name.clone()));
                    i += 1;
                }
            }
            Tok::BraceOpen => {
                frames.push(false);
                i += 1;
            }
            Tok::BraceClose => {
                if frames.pop() == Some(true) {
                    ns_stack.pop();
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (namespaces, types)
}

fn current_ns(file_scoped: &Option<String>, stack: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if stack.is_empty() {
        return file_scoped.clone().unwrap_or_default();
    }
    if let Some(base) = file_scoped {
        if !base.is_empty() {
            parts.push(base);
        }
    }
    parts.extend(stack.iter().map(String::as_str));
    parts.join(".")
}

fn is_type_keyword(kw: &str) -> bool {
    matches!(
        kw,
        "class" | "struct" | "interface" | "enum" | "delegate" | "record"
    )
}

#[derive(Clone, PartialEq)]
enum Tok {
    Ident(String),
    Dot,
    Semi,
    BraceOpen,
    BraceClose,
    ParenOpen,
    Lt,
    Gt,
    Colon,
    Comma,
}

fn tokenize(masked: &str) -> Vec<Tok> {
    let b = masked.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'{' => out.push(Tok::BraceOpen),
            b'}' => out.push(Tok::BraceClose),
            b';' => out.push(Tok::Semi),
            b'.' => out.push(Tok::Dot),
            b'(' => out.push(Tok::ParenOpen),
            b'<' => out.push(Tok::Lt),
            b'>' => out.push(Tok::Gt),
            b':' => out.push(Tok::Colon),
            b',' => out.push(Tok::Comma),
            _ => {
                if c == b'@' && i + 1 < b.len() && is_ident_start(b[i + 1]) {
                    i += 1;
                }
                if i < b.len() && is_ident_start(b[i]) {
                    let start = i;
                    i += 1;
                    while i < b.len() && is_ident_continue(b[i]) {
                        i += 1;
                    }
                    out.push(Tok::Ident(masked[start..i].to_string()));
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn dotted(toks: &[Tok], i: &mut usize) -> Option<String> {
    let Tok::Ident(first) = toks.get(*i)? else {
        return None;
    };
    let mut name = first.clone();
    *i += 1;
    while *i + 1 < toks.len() && toks[*i] == Tok::Dot {
        if let Tok::Ident(seg) = &toks[*i + 1] {
            name.push('.');
            name.push_str(seg);
            *i += 2;
        } else {
            break;
        }
    }
    Some(name)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn mask_csharp(src: &str) -> String {
    let mut b = src.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let start = i;
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            blank(&mut b, start, i);
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            blank(&mut b, start, i);
            continue;
        }
        if let Some(end) = raw_string_end(&b, i) {
            blank(&mut b, i, end);
            i = end;
            continue;
        }
        if let Some(end) = string_end(&b, i) {
            blank(&mut b, i, end);
            i = end;
            continue;
        }
        i += 1;
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// End of a raw string literal starting at `i`: optional `$`s, then a run of
/// `n >= 3` quotes, closed by a run of `n` quotes. Covers `"""x"""`,
/// `$"""x{y}"""`, `$$"""x"""` and longer quote fences.
fn raw_string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < b.len() && b[j] == b'$' {
        j += 1;
    }
    let q = b[j..].iter().take_while(|&&c| c == b'"').count();
    if q < 3 {
        return None;
    }
    let mut k = j + q;
    while k < b.len() {
        if b[k] == b'"' {
            let run = b[k..].iter().take_while(|&&c| c == b'"').count();
            if run >= q {
                return Some(k + run);
            }
            k += run;
        } else {
            k += 1;
        }
    }
    Some(b.len())
}

fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    let mut interpolated = false;
    let mut verbatim = false;
    if b[j] == b'$' {
        interpolated = true;
        j += 1;
        if j < b.len() && b[j] == b'@' {
            verbatim = true;
            j += 1;
        }
    } else if b[j] == b'@' {
        verbatim = true;
        j += 1;
        if j < b.len() && b[j] == b'$' {
            interpolated = true;
            j += 1;
        }
    }
    if interpolated || verbatim {
        if j < b.len() && b[j] == b'"' {
            return Some(skip_quoted(b, j, verbatim, interpolated));
        }
        return None;
    }
    if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
        return Some(skip_plain(b, i));
    }
    None
}

fn skip_plain(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut i = i + 1;
    while i < b.len() && b[i] != q && b[i] != b'\n' {
        if b[i] == b'\\' {
            i = (i + 2).min(b.len());
            continue;
        }
        i += 1;
    }
    if i < b.len() && b[i] == q {
        i + 1
    } else {
        i
    }
}

fn skip_quoted(b: &[u8], mut i: usize, verbatim: bool, interpolated: bool) -> usize {
    i += 1;
    let mut depth = 0i32;
    while i < b.len() {
        if !verbatim && depth == 0 && b[i] == b'\\' {
            i = (i + 2).min(b.len());
            continue;
        }
        if interpolated && b[i] == b'{' {
            if i + 1 < b.len() && b[i + 1] == b'{' {
                i += 2;
                continue;
            }
            depth += 1;
            i += 1;
            continue;
        }
        if interpolated && b[i] == b'}' && depth > 0 {
            depth -= 1;
            i += 1;
            continue;
        }
        if b[i] == b'"' && depth == 0 {
            if verbatim && i + 1 < b.len() && b[i + 1] == b'"' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

fn blank(b: &mut [u8], start: usize, end: usize) {
    for byte in &mut b[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn two_projects_namespace_partials_and_system_using() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Directory.Build.props",
            r#"<Project>
  <PropertyGroup>
    <ImplicitUsings>enable</ImplicitUsings>
    <RootNamespace>Acme</RootNamespace>
  </PropertyGroup>
</Project>
"#,
        );
        write(
            root,
            "lib/Lib.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <RootNamespace>Acme.Lib</RootNamespace>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
  </ItemGroup>
</Project>
"#,
        );
        write(
            root,
            "app/App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\lib\Lib.csproj" />
  </ItemGroup>
</Project>
"#,
        );
        write(
            root,
            "lib/Widget.cs",
            "namespace Acme.Lib;\npublic partial class Widget { public void A() {} }\n",
        );
        write(
            root,
            "lib/Widget.More.cs",
            "namespace Acme.Lib;\npublic partial class Widget { public void B() {} }\n",
        );
        write(
            root,
            "lib/Helper.cs",
            "namespace Acme.Lib;\npublic static class Helper { public static int N => 1; }\n",
        );
        write(
            root,
            "app/Program.cs",
            "namespace Acme.App;\nusing Acme.Lib;\nusing System;\nclass Program { static void Main() {} }\n",
        );
        write(
            root,
            "lib/SystemHack.cs",
            "namespace System.Text;\npublic class FakeEncoder {}\n",
        );
        write(
            root,
            "lib/obj/Hidden.cs",
            "namespace Acme.Lib;\nclass Hidden {}\n",
        );
        write(
            root,
            "lib/bin/Hidden.cs",
            "namespace Acme.Lib;\nclass HiddenBin {}\n",
        );
        write(
            root,
            ".vs/Hidden.cs",
            "namespace Acme.Lib;\nclass HiddenVs {}\n",
        );

        let paths = [
            "Directory.Build.props",
            "lib/Lib.csproj",
            "app/App.csproj",
            "lib/Widget.cs",
            "lib/Widget.More.cs",
            "lib/Helper.cs",
            "app/Program.cs",
            "lib/SystemHack.cs",
            "lib/obj/Hidden.cs",
            "lib/bin/Hidden.cs",
            ".vs/Hidden.cs",
        ];
        let files = FileIndex::new(root, paths.map(RelPath::new));
        let resolver = CSharpResolver;
        let meta = resolver.detect(&files);

        let lib = meta
            .projects
            .iter()
            .find(|p| p.manifest == "lib/Lib.csproj")
            .unwrap();
        let app = meta
            .projects
            .iter()
            .find(|p| p.manifest == "app/App.csproj")
            .unwrap();
        assert_eq!(lib.root_namespace, "Acme.Lib");
        assert_eq!(app.root_namespace, "Acme");
        assert!(lib.default_compile_items);
        assert!(app.default_compile_items);
        assert!(lib.implicit_usings.iter().any(|u| u == "System"));
        assert!(lib.implicit_usings.iter().any(|u| u == "System.Linq"));
        assert!(meta.source_roots.iter().any(|r| r == "lib"));
        assert!(meta.source_roots.iter().any(|r| r == "app"));
        assert!(meta.excludes.iter().any(|e| e == "lib/bin"));
        assert!(meta.excludes.iter().any(|e| e == "lib/obj"));
        assert!(meta.excludes.iter().any(|e| e == "lib/.vs"));
        assert!(meta.excludes.iter().any(|e| e == ".vs"));
        assert_eq!(
            meta.project_refs,
            vec![ProjectRef {
                from: "app/App.csproj".into(),
                to: "lib/Lib.csproj".into(),
            }]
        );

        let acme_lib = meta
            .namespaces
            .iter()
            .find(|n| n.namespace == "Acme.Lib")
            .unwrap();
        assert_eq!(
            acme_lib.files,
            vec![
                "lib/Helper.cs".to_string(),
                "lib/Widget.More.cs".to_string(),
                "lib/Widget.cs".to_string(),
            ]
        );
        // A System.* namespace the repo declares itself is kept.
        assert!(meta.namespaces.iter().any(|n| n.namespace == "System.Text"));

        let from = RelPath::new("app/Program.cs");
        let project = resolver.resolve_import(&from, "project:../lib/Lib.csproj", &files, &meta);
        assert_eq!(
            project,
            ImportResolution::File(RelPath::new("lib/Lib.csproj")),
            "project reference is a single file import"
        );
        assert_eq!(
            resolver.resolve(&from, "project:../lib/Lib.csproj", &files, &meta),
            Some(RelPath::new("lib/Lib.csproj"))
        );

        let ns = resolver.resolve_import(&from, "using Acme.Lib;", &files, &meta);
        match ns {
            ImportResolution::Namespace { name, files } => {
                assert_eq!(name, "Acme.Lib");
                let got: Vec<&str> = files.iter().map(RelPath::as_str).collect();
                assert_eq!(
                    got,
                    vec!["lib/Helper.cs", "lib/Widget.More.cs", "lib/Widget.cs"]
                );
            }
            other => panic!("namespace using must not collapse to one file: {other:?}"),
        }
        assert!(
            resolver
                .resolve(&from, "using Acme.Lib;", &files, &meta)
                .is_none(),
            "resolve() must not pick a single file for a namespace using"
        );

        assert_eq!(
            resolver.resolve_import(&from, "using System;", &files, &meta),
            ImportResolution::Unresolved
        );
        assert_eq!(
            resolver.resolve_import(&from, "using System.Linq;", &files, &meta),
            ImportResolution::Unresolved
        );
        assert_eq!(
            resolver.resolve_import(&from, "using Newtonsoft.Json;", &files, &meta),
            ImportResolution::Unresolved
        );
        match resolver.resolve_import(&from, "using System.Text;", &files, &meta) {
            ImportResolution::Namespace { files, .. } => {
                assert_eq!(files, vec![RelPath::new("lib/SystemHack.cs")]);
            }
            other => panic!("repo-declared System.* namespace must resolve: {other:?}"),
        }

        assert_eq!(
            resolver.resolve_import(&from, "using static Acme.Lib.Helper;", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Helper.cs"))
        );
        assert_eq!(
            resolver.resolve_import(&from, "using H = Acme.Lib.Helper;", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Helper.cs"))
        );
        assert_eq!(
            resolver.resolve_import(&from, "static:Acme.Lib.Helper.N", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Helper.cs"))
        );
        assert_eq!(
            resolver.resolve_import(&from, "using static Acme.Lib.Widget;", &files, &meta),
            ImportResolution::Unresolved,
            "partial Widget lives in two files, so using static is not one edge"
        );

        symbols_if_parser_present();
    }

    fn symbols_if_parser_present() {
        let pool = crate::parse::ParserPool::new();
        let src = "namespace Acme.Lib;\nusing static Acme.Lib.Helper;\nusing IO = System.IO;\nusing System.Text;\npublic partial class Widget { }\n";
        match pool.parse(Language::CSharp, &RelPath::new("Widget.cs"), src) {
            Ok(parsed) => {
                assert!(
                    parsed.symbols.iter().any(|s| s.name == "Widget"),
                    "C# parser is wired but did not see Widget: {:?}",
                    parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                );
                assert!(
                    parsed.imports.iter().any(|s| s == "static:Acme.Lib.Helper"),
                    "indexer must emit static: specs, got {:?}",
                    parsed.imports
                );
                assert!(
                    parsed.imports.iter().any(|s| s == "alias:System.IO"),
                    "indexer must emit alias: specs, got {:?}",
                    parsed.imports
                );
                assert!(
                    parsed.imports.iter().any(|s| s == "System.Text"),
                    "namespace using must stay a bare name, got {:?}",
                    parsed.imports
                );
            }
            Err(crate::parse::ParseError::NoGrammar(_)) => {
                // The language branch has not landed the grammar in this worktree.
            }
            Err(err) => panic!("unexpected C# parse error: {err}"),
        }
    }

    #[test]
    fn compile_conditions_are_evaluated_or_kept_conservatively() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "c/C.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
  </PropertyGroup>
  <ItemGroup>
    <Compile Include="Always.cs" />
    <Compile Include="OffElement.cs" Condition="'$(RootNamespace)' != ''" />
    <Compile Include="OnElement.cs" Condition="'$(RootNamespace)' == '' and '1' == '1'" />
    <Compile Include="UnknownElement.cs" Condition="'$(Configuration)' == 'Debug'" />
  </ItemGroup>
  <ItemGroup Condition="'a' == 'b'">
    <Compile Include="OffGroup.cs" />
  </ItemGroup>
  <ItemGroup Condition="!Exists('x.txt')">
    <Compile Include="UnknownGroup.cs" />
    <Compile Remove="Always.cs" />
  </ItemGroup>
  <ItemGroup Condition="'$(Configuration)' == 'Debug' Or '1' == '1'">
    <Compile Include="OrTrue.cs" />
  </ItemGroup>
  <ItemGroup Condition="'1' == '1'">
    <Compile Remove="Removed.cs" />
    <Compile Include="Removed.cs" />
  </ItemGroup>
  <ItemGroup Condition="'$(Configuration)' == 'Debug'">
    <Compile Remove="OnElement.cs" />
  </ItemGroup>
</Project>
"#,
        );
        let names = [
            "Always",
            "OffElement",
            "OnElement",
            "UnknownElement",
            "OffGroup",
            "UnknownGroup",
            "OrTrue",
            "Removed",
        ];
        let mut paths = vec!["c/C.csproj".to_string()];
        for n in names {
            let rel = format!("c/{n}.cs");
            write(root, &rel, &format!("namespace N;\nclass {n} {{}}\n"));
            paths.push(rel);
        }
        let files = FileIndex::new(root, paths.iter().map(|p| RelPath::new(p.as_str())));
        let meta = CSharpResolver.detect(&files);
        let ns = meta.namespaces.iter().find(|n| n.namespace == "N").unwrap();
        let got: BTreeSet<&str> = ns.files.iter().map(|f| f.as_str()).collect();
        let want: BTreeSet<&str> = [
            "c/Always.cs",
            "c/OnElement.cs",
            "c/UnknownElement.cs",
            "c/UnknownGroup.cs",
            "c/OrTrue.cs",
        ]
        .into_iter()
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn default_compile_items_off_keeps_only_explicit_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "only/Only.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
  </PropertyGroup>
  <ItemGroup>
    <Compile Include="Only.cs" />
  </ItemGroup>
</Project>
"#,
        );
        write(root, "only/Only.cs", "namespace N;\nclass A {}\n");
        write(root, "only/Other.cs", "namespace N;\nclass B {}\n");
        let files = FileIndex::new(
            root,
            ["only/Only.csproj", "only/Only.cs", "only/Other.cs"].map(RelPath::new),
        );
        let meta = CSharpResolver.detect(&files);
        let project = &meta.projects[0];
        assert!(!project.default_compile_items);
        assert_eq!(project.root_namespace, "Only");
        assert!(project.implicit_usings.is_empty());
        let ns = meta.namespaces.iter().find(|n| n.namespace == "N").unwrap();
        assert_eq!(ns.files, vec!["only/Only.cs".to_string()]);
        let from = RelPath::new("only/Only.cs");
        match CSharpResolver.resolve_import(&from, "ns:N", &files, &meta) {
            ImportResolution::Namespace { files, .. } => {
                assert_eq!(files, vec![RelPath::new("only/Only.cs")]);
            }
            other => panic!("one declaring file is still a namespace relationship: {other:?}"),
        }
    }

    #[test]
    fn nearest_directory_build_props_overrides_parent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Directory.Build.props",
            "<Project><PropertyGroup><RootNamespace>ParentNs</RootNamespace></PropertyGroup></Project>",
        );
        write(
            root,
            "nested/Directory.Build.props",
            "<Project><PropertyGroup><RootNamespace>ChildNs</RootNamespace></PropertyGroup></Project>",
        );
        write(
            root,
            "nested/App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
        );
        let files = FileIndex::new(
            root,
            [
                "Directory.Build.props",
                "nested/Directory.Build.props",
                "nested/App.csproj",
            ]
            .map(RelPath::new),
        );
        let meta = CSharpResolver.detect(&files);
        assert_eq!(meta.projects[0].root_namespace, "ChildNs");
    }

    #[test]
    fn web_sdk_implicit_usings_include_aspnetcore() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "web/Web.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <ImplicitUsings>enable</ImplicitUsings>
  </PropertyGroup>
</Project>"#,
        );
        let files = FileIndex::new(root, ["web/Web.csproj"].map(RelPath::new));
        let meta = CSharpResolver.detect(&files);
        let usings = &meta.projects[0].implicit_usings;
        assert!(usings.iter().any(|u| u == "System.Linq"));
        assert!(usings.iter().any(|u| u == "Microsoft.AspNetCore.Builder"));
    }

    #[test]
    fn csx_default_compile_using_becomes_namespace_edge() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "app/App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <RootNamespace>Acme.App</RootNamespace>
  </PropertyGroup>
</Project>"#,
        );
        write(
            root,
            "app/Script.csx",
            "namespace Acme.Scripts;\npublic class Box {}\n",
        );
        write(
            root,
            "app/Program.cs",
            "namespace Acme.App;\nusing Acme.Scripts;\nclass Program { static void Main() {} }\n",
        );
        write(
            root,
            "app/bin/Skip.csx",
            "namespace Acme.Scripts;\npublic class Hidden {}\n",
        );
        write(
            root,
            "app/obj/Skip.csx",
            "namespace Acme.Scripts;\npublic class HiddenObj {}\n",
        );
        write(
            root,
            "app/.vs/Skip.csx",
            "namespace Acme.Scripts;\npublic class HiddenVs {}\n",
        );
        let files = FileIndex::new(
            root,
            [
                "app/App.csproj",
                "app/Script.csx",
                "app/Program.cs",
                "app/bin/Skip.csx",
                "app/obj/Skip.csx",
                "app/.vs/Skip.csx",
            ]
            .map(RelPath::new),
        );
        let meta = CSharpResolver.detect(&files);
        let ns = meta
            .namespaces
            .iter()
            .find(|n| n.namespace == "Acme.Scripts")
            .unwrap();
        assert_eq!(ns.files, vec!["app/Script.csx".to_string()]);
        let from = RelPath::new("app/Program.cs");
        match CSharpResolver.resolve_import(&from, "using Acme.Scripts;", &files, &meta) {
            ImportResolution::Namespace { files, .. } => {
                assert_eq!(files, vec![RelPath::new("app/Script.csx")]);
            }
            other => panic!("script using should be a namespace edge: {other:?}"),
        }
    }

    #[test]
    fn explicit_csx_compile_include_is_a_namespace_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "only/Only.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
  </PropertyGroup>
  <ItemGroup>
    <Compile Include="Extra.csx" />
    <Compile Include="glob/*.csx" />
  </ItemGroup>
</Project>"#,
        );
        write(root, "only/Extra.csx", "namespace N;\nclass Scripted {}\n");
        write(root, "only/Other.csx", "namespace N;\nclass Skipped {}\n");
        write(
            root,
            "only/glob/Also.csx",
            "namespace N;\nclass Globbed {}\n",
        );
        write(
            root,
            "only/glob/Skip.cs",
            "namespace Other;\nclass NotGlob {}\n",
        );
        let files = FileIndex::new(
            root,
            [
                "only/Only.csproj",
                "only/Extra.csx",
                "only/Other.csx",
                "only/glob/Also.csx",
                "only/glob/Skip.cs",
            ]
            .map(RelPath::new),
        );
        let meta = CSharpResolver.detect(&files);
        let ns = meta.namespaces.iter().find(|n| n.namespace == "N").unwrap();
        assert_eq!(
            ns.files,
            vec![
                "only/Extra.csx".to_string(),
                "only/glob/Also.csx".to_string(),
            ]
        );
    }
}
