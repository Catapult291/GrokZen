//! Visual Basic .NET project and namespace resolution for SDK-style projects.
//!
//! Same contract as the C# resolver, for `.vb` / `.vbproj` instead of `.cs` /
//! `.csproj`. `detect` reads every `*.vbproj` outside `bin` / `obj` / `.vs`,
//! plus the nearest `Directory.Build.props`. The vbproj overrides the props.
//! SDK defaults: `RootNamespace` is the project file name, and
//! `Microsoft.NET.Sdk*` turns on default compile items (`**/*.vb`).
//!
//! Output directories are project-relative (`lib/bin`, `lib/obj`). The path
//! component `bin` is never added as a repo-wide exclude — Rust keeps sources
//! in `src/bin`, and a bare `bin` entry would hide them.
//!
//! # Import specs
//!
//! * `Imports Acme.Lib` / `ns:Acme.Lib` — namespace relationship to every file
//!   that declares `Acme.Lib`, partials included. Never collapsed to one file.
//! * `Imports Acme.Lib.Helper` when that name is not a namespace — the VB
//!   equivalent of `using static`. One file, and only when exactly one in-repo
//!   file declares the type. Two partials are not one target.
//! * `Imports H = Acme.Lib.Helper` / `alias:Acme.Lib.Helper` / `static:` — same
//!   single-file rule.
//! * `project:../lib/Lib.vbproj` — the in-repo `ProjectReference`, one file.
//!   The vbproj itself is not a graph node; the index records the reference
//!   and does not invent an edge.
//!
//! `System` and `System.*` are never targets. A name no file declares (NuGet)
//! resolves to nothing. Matching is case-insensitive, like the language.
//!
//! Element `Condition` attributes are honored only for the empty-property
//! check `'$(Name)' == ''`. Anything else is skipped rather than guessed.
//! ItemGroup-level conditions are not evaluated.

use std::collections::{BTreeMap, BTreeSet};

use crate::types::{
    join_rel, FileIndex, ImportResolution, Language, ModuleResolver, ModuleUnit, NamespaceFiles,
    ProjectFacts, ProjectMeta, ProjectRef, RelPath, TypeDecl,
};

pub struct VisualBasicResolver;

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

impl ModuleResolver for VisualBasicResolver {
    fn language(&self) -> Language {
        Language::VisualBasic
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut manifests: Vec<String> = files
            .iter()
            .filter(|p| is_vbproj(p.file_name()) && !skipped_tree(p.as_str()))
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
            let project_name = project_file_name(manifest);
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

        // One pass: which project owns each VB source (deepest project
        // directory above it; the last manifest wins a tie).
        let mut dir_owner: std::collections::HashMap<&str, &str> = Default::default();
        for (manifest, dir) in &directories {
            dir_owner.insert(dir.as_str(), manifest.as_str());
        }
        let mut buckets: std::collections::HashMap<&str, Vec<&str>> = Default::default();
        for p in files.iter() {
            let path = p.as_str();
            if p.extension() != Some("vb") || skipped_tree(path) {
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
            for path in compile_files(files, prep, bucket) {
                let Some(text) = files.read(&path) else {
                    continue;
                };
                let (namespaces, types) = extract_declarations(&text);
                for ns in namespaces {
                    if framework_namespace(&ns) {
                        continue;
                    }
                    ns_files.entry(ns).or_default().insert(path.clone());
                }
                for (ns, name) in types {
                    if framework_namespace(&ns) || name.is_empty() {
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
                if name.is_empty() || framework_namespace(&name) {
                    return ImportResolution::Unresolved;
                }
                let files = namespace_files(meta, &name);
                if !files.is_empty() {
                    // A declared namespace is never collapsed, even for one file.
                    return ImportResolution::Namespace { name, files };
                }
                // Imports of a type (VB has no separate `using static` keyword).
                match unique_type(meta, &name) {
                    Some(path) => ImportResolution::File(RelPath::new(path)),
                    None => ImportResolution::Unresolved,
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
    let lower = spec.to_ascii_lowercase();
    if lower.ends_with(".vbproj") {
        return Spec::Project(spec.to_string());
    }
    if let Some(rest) = strip_ignore_ascii_case(spec, "using ")
        .or_else(|| strip_ignore_ascii_case(spec, "imports "))
    {
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

fn namespace_files(meta: &ProjectMeta, name: &str) -> Vec<RelPath> {
    meta.namespaces
        .iter()
        .find(|n| n.namespace.eq_ignore_ascii_case(name))
        .map(|n| n.files.iter().map(RelPath::new).collect())
        .unwrap_or_default()
}

fn unique_type(meta: &ProjectMeta, qualified: &str) -> Option<String> {
    let qualified = qualified.trim().trim_end_matches(';').trim();
    if qualified.is_empty() || framework_namespace(qualified) {
        return None;
    }
    let direct = files_declaring(meta, qualified);
    if !direct.is_empty() {
        return only_one(direct);
    }
    let (head, _) = qualified.rsplit_once('.')?;
    if framework_namespace(head) {
        return None;
    }
    only_one(files_declaring(meta, head))
}

fn files_declaring(meta: &ProjectMeta, qualified: &str) -> Vec<String> {
    let mut files: Vec<String> = meta
        .type_decls
        .iter()
        .filter(|t| qualified_name(t).eq_ignore_ascii_case(qualified))
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

fn qualified_name(decl: &TypeDecl) -> String {
    if decl.namespace.is_empty() {
        decl.name.clone()
    } else {
        format!("{}.{}", decl.namespace, decl.name)
    }
}

fn framework_namespace(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case("System")
        || name.len() > "System.".len() && name[.."System.".len()].eq_ignore_ascii_case("System.")
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

fn compile_files(files: &FileIndex, prep: &Prepared, bucket: &[&str]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let dir = &prep.facts.directory;
    if prep.facts.default_compile_items {
        for &path in bucket {
            if is_project_output(path, dir, &prep.output, &prep.intermediate) {
                continue;
            }
            let rel = rel_to(path, dir);
            if prep
                .compile_remove
                .iter()
                .any(|pattern| glob_match(pattern, rel))
            {
                continue;
            }
            out.insert(path.to_string());
        }
    }
    for include in &prep.compile_include {
        add_include(files, dir, include, bucket, &mut out);
    }
    for remove in &prep.compile_remove {
        out.retain(|path| !glob_match(remove, rel_to(path, dir)));
    }
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
    if !pattern.contains('*') {
        if let Some(full) = normalize_include(dir, &pattern) {
            if files.contains(&full) && full.ends_with(".vb") && !skipped_tree(&full) {
                out.insert(full);
            }
        }
        return;
    }
    for &path in bucket {
        if glob_match(&pattern, rel_to(path, dir)) {
            out.insert(path.to_string());
        }
    }
}

fn is_project_output(path: &str, dir: &str, output: &str, intermediate: &str) -> bool {
    let rel = rel_to(path, dir);
    for root in [output, intermediate, ".vs"] {
        if rel == root || rel.starts_with(&format!("{root}/")) {
            return true;
        }
    }
    false
}

fn glob_match(pattern: &str, rel: &str) -> bool {
    let pattern = pattern.trim().replace('\\', "/");
    let pattern = pattern.trim_start_matches("./");
    let rel = rel.trim_start_matches("./");
    if let Some(prefix) = pattern.strip_suffix("/**") {
        let prefix = prefix.trim_matches('/');
        if prefix.is_empty() {
            return true;
        }
        return rel == prefix || rel.starts_with(&format!("{prefix}/"));
    }
    if let Some(idx) = pattern.rfind('*') {
        let suf = pattern[idx + 1..].trim_start_matches('/');
        let pre = pattern[..idx].trim_end_matches('*').trim_matches('/');
        let suf_ok = suf.is_empty() || rel.ends_with(suf);
        let pre_ok = pre.is_empty() || rel == pre || rel.starts_with(&format!("{pre}/"));
        return pre_ok && suf_ok;
    }
    rel == pattern
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
        if let Some(cond) = tag.attr("Condition") {
            if !condition_allows(cond, props, &tag.name) {
                continue;
            }
        }
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
                    props.compile_remove.push(remove.to_string());
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

fn condition_allows(cond: &str, props: &BuildProps, element: &str) -> bool {
    // Accept only `'$(Name)' == ''`. Other conditions are skipped, not guessed.
    let flat: String = cond
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace('"', "'");
    let Some(rest) = flat.strip_prefix("'$(") else {
        return false;
    };
    let Some((key, tail)) = rest.split_once(")'==''") else {
        return false;
    };
    if !tail.is_empty() || !key.eq_ignore_ascii_case(element) {
        return false;
    }
    match key {
        "RootNamespace" => props.root_namespace.as_deref().unwrap_or("").is_empty(),
        "ImplicitUsings" => props.implicit_usings.is_none(),
        "EnableDefaultItems" => props.enable_default_items.is_none(),
        "EnableDefaultCompileItems" => props.enable_default_compile.is_none(),
        "BaseOutputPath" | "OutputPath" => props.base_output.is_none(),
        "BaseIntermediateOutputPath" | "IntermediateOutputPath" => {
            props.base_intermediate.is_none()
        }
        _ => false,
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
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'/') || b.get(i + 1) == Some(&b'!') || b.get(i + 1) == Some(&b'?')
        {
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
            tags.push(XmlTag { name, attrs, body });
        }
    }
    tags
}

struct XmlTag {
    name: String,
    attrs: Vec<(String, String)>,
    body: String,
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

fn is_vbproj(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".vbproj")
}

fn project_file_name(path: &str) -> String {
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

fn extract_declarations(src: &str) -> (BTreeSet<String>, Vec<(String, String)>) {
    let masked = mask_vb(src);
    let toks = tokenize(&masked);
    let mut ns_stack: Vec<String> = Vec::new();
    let mut namespaces = BTreeSet::new();
    let mut types = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Tok::Ident(word) = &toks[i] else {
            i += 1;
            continue;
        };
        if eq_kw(word, "end") {
            i += 1;
            if let Some(Tok::Ident(next)) = toks.get(i) {
                if eq_kw(next, "namespace") {
                    ns_stack.pop();
                }
                // `End Class` / `End Module` / … must not start a type.
                i += 1;
            }
            continue;
        }
        if eq_kw(word, "namespace") {
            i += 1;
            let Some(name) = dotted(&toks, &mut i) else {
                continue;
            };
            ns_stack.push(name);
            namespaces.insert(current_ns(&ns_stack));
            continue;
        }
        if is_type_keyword(word) {
            let keyword = word.clone();
            i += 1;
            if eq_kw(&keyword, "delegate") {
                if let Some(Tok::Ident(next)) = toks.get(i) {
                    if eq_kw(next, "sub") || eq_kw(next, "function") {
                        i += 1;
                    }
                }
            }
            if let Some(Tok::Ident(name)) = toks.get(i) {
                if !is_type_keyword(name)
                    && !eq_kw(name, "end")
                    && !eq_kw(name, "sub")
                    && !eq_kw(name, "function")
                {
                    types.push((current_ns(&ns_stack), name.clone()));
                    i += 1;
                    continue;
                }
            }
            continue;
        }
        i += 1;
    }
    (namespaces, types)
}

fn current_ns(stack: &[String]) -> String {
    stack.join(".")
}

fn is_type_keyword(kw: &str) -> bool {
    eq_kw(kw, "class")
        || eq_kw(kw, "module")
        || eq_kw(kw, "structure")
        || eq_kw(kw, "interface")
        || eq_kw(kw, "enum")
        || eq_kw(kw, "delegate")
}

fn eq_kw(word: &str, kw: &str) -> bool {
    word.eq_ignore_ascii_case(kw)
}

#[derive(Clone, PartialEq)]
enum Tok {
    Ident(String),
    Dot,
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
        if c == b'.' {
            out.push(Tok::Dot);
            i += 1;
            continue;
        }
        if c == b'[' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b']' && b[i] != b'\n' {
                i += 1;
            }
            if start < i {
                out.push(Tok::Ident(masked[start..i].to_string()));
            }
            if i < b.len() && b[i] == b']' {
                i += 1;
            }
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < b.len() && is_ident_continue(b[i]) {
                i += 1;
            }
            out.push(Tok::Ident(masked[start..i].to_string()));
            continue;
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

fn mask_vb(src: &str) -> String {
    let mut b = src.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            blank(&mut b, start, i);
            continue;
        }
        if is_rem_comment(&b, i) {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            blank(&mut b, start, i);
            continue;
        }
        if b[i] == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'\n' {
                if b[i] == b'"' {
                    if i + 1 < b.len() && b[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            blank(&mut b, start, i);
            continue;
        }
        i += 1;
    }
    String::from_utf8_lossy(&b).into_owned()
}

fn is_rem_comment(b: &[u8], i: usize) -> bool {
    if i + 3 > b.len() {
        return false;
    }
    if !b[i..i + 3].eq_ignore_ascii_case(b"rem") {
        return false;
    }
    let boundary_before = i == 0 || !is_ident_continue(b[i - 1]);
    let after = i + 3;
    let boundary_after = after >= b.len() || b[after].is_ascii_whitespace();
    boundary_before && boundary_after
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
    use crate::index::{index_repo, IndexOptions};
    use crate::types::EdgeKind;
    use std::fs;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn sample(root: &std::path::Path) {
        write(
            root,
            "lib/Lib.vbproj",
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
            "app/App.vbproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <RootNamespace>Acme.App</RootNamespace>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\lib\Lib.vbproj" />
  </ItemGroup>
</Project>
"#,
        );
        write(
            root,
            "lib/Widget.vb",
            "Namespace Acme.Lib\nPublic Partial Class Widget\n    Public Sub A()\n    End Sub\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            "lib/Widget.More.vb",
            "Namespace Acme.Lib\nPublic Partial Class Widget\n    Public Function B() As Integer\n        Return 1\n    End Function\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            "lib/Helper.vb",
            "Namespace Acme.Lib\nPublic Module Helper\n    Public Function N() As Integer\n        Return 1\n    End Function\nEnd Module\nEnd Namespace\n",
        );
        write(
            root,
            "app/Program.vb",
            "Namespace Acme.App\nImports Acme.Lib\nImports Acme.Lib.Helper\nImports System\nImports Newtonsoft.Json\nPublic Class Program\n    Public Sub Main()\n        Dim w As New Widget()\n        Helper.N()\n    End Sub\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            "lib/SystemHack.vb",
            "Namespace System.Text\nPublic Class FakeEncoder\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            "lib/obj/Hidden.vb",
            "Namespace Acme.Lib\nClass Hidden\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            "lib/bin/Hidden.vb",
            "Namespace Acme.Lib\nClass HiddenBin\nEnd Class\nEnd Namespace\n",
        );
        write(
            root,
            ".vs/Hidden.vb",
            "Namespace Acme.Lib\nClass HiddenVs\nEnd Class\nEnd Namespace\n",
        );
        // A `bin` path component outside MSBuild output must stay indexed.
        write(
            root,
            "src/bin/Keep.vb",
            "Namespace Tools\nPublic Class Keep\nEnd Class\nEnd Namespace\n",
        );
    }

    #[test]
    fn imports_namespace_partials_project_reference_and_no_global_bin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample(root);
        let paths = [
            "lib/Lib.vbproj",
            "app/App.vbproj",
            "lib/Widget.vb",
            "lib/Widget.More.vb",
            "lib/Helper.vb",
            "app/Program.vb",
            "lib/SystemHack.vb",
            "lib/obj/Hidden.vb",
            "lib/bin/Hidden.vb",
            ".vs/Hidden.vb",
            "src/bin/Keep.vb",
        ];
        let files = FileIndex::new(root, paths.map(RelPath::new));
        let resolver = VisualBasicResolver;
        let meta = resolver.detect(&files);

        assert!(meta.excludes.iter().any(|e| e == "lib/bin"));
        assert!(meta.excludes.iter().any(|e| e == "lib/obj"));
        assert!(
            meta.excludes.iter().all(|e| e != "bin"),
            "must not globally exclude bin: {:?}",
            meta.excludes
        );
        assert!(meta.excludes.iter().all(|e| e != "src/bin"));
        assert_eq!(
            meta.project_refs,
            vec![ProjectRef {
                from: "app/App.vbproj".into(),
                to: "lib/Lib.vbproj".into(),
            }]
        );
        let acme = meta
            .namespaces
            .iter()
            .find(|n| n.namespace == "Acme.Lib")
            .unwrap();
        assert_eq!(
            acme.files,
            vec![
                "lib/Helper.vb".to_string(),
                "lib/Widget.More.vb".to_string(),
                "lib/Widget.vb".to_string(),
            ]
        );
        assert!(meta
            .namespaces
            .iter()
            .all(|n| !n.namespace.eq_ignore_ascii_case("System.Text")));

        let from = RelPath::new("app/Program.vb");
        assert_eq!(
            resolver.resolve_import(&from, "project:../lib/Lib.vbproj", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Lib.vbproj"))
        );
        match resolver.resolve_import(&from, "Imports Acme.Lib", &files, &meta) {
            ImportResolution::Namespace { name, files } => {
                assert_eq!(name, "Acme.Lib");
                let got: Vec<&str> = files.iter().map(RelPath::as_str).collect();
                assert_eq!(
                    got,
                    vec!["lib/Helper.vb", "lib/Widget.More.vb", "lib/Widget.vb"]
                );
            }
            other => panic!("namespace Imports must not collapse: {other:?}"),
        }
        assert!(resolver
            .resolve(&from, "Imports Acme.Lib", &files, &meta)
            .is_none());
        assert_eq!(
            resolver.resolve_import(&from, "Imports System", &files, &meta),
            ImportResolution::Unresolved
        );
        assert_eq!(
            resolver.resolve_import(&from, "Imports Newtonsoft.Json", &files, &meta),
            ImportResolution::Unresolved
        );
        assert_eq!(
            resolver.resolve_import(&from, "Imports Acme.Lib.Helper", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Helper.vb"))
        );
        assert_eq!(
            resolver.resolve_import(&from, "Imports H = Acme.Lib.Helper", &files, &meta),
            ImportResolution::File(RelPath::new("lib/Helper.vb"))
        );
        assert_eq!(
            resolver.resolve_import(&from, "Imports Acme.Lib.Widget", &files, &meta),
            ImportResolution::Unresolved,
            "partial Widget is two files"
        );

        let opts = IndexOptions {
            persist: false,
            call_edges: false,
            ..IndexOptions::default()
        };
        let (graph, report) = index_repo(root, &opts);
        assert!(
            graph
                .files
                .iter()
                .any(|f| f.path.as_str() == "src/bin/Keep.vb"),
            "src/bin must stay in the graph"
        );
        let id = |path: &str| {
            graph
                .files
                .iter()
                .find(|f| f.path.as_str() == path)
                .map(|f| f.id.0)
        };
        let program = id("app/Program.vb").unwrap();
        let targets = ["lib/Helper.vb", "lib/Widget.More.vb", "lib/Widget.vb"];
        for target in targets {
            let to = id(target).unwrap();
            assert!(
                graph
                    .edges
                    .iter()
                    .any(|e| e.from == program && e.to == to && e.kind == EdgeKind::Namespace),
                "missing Namespace edge to {target}"
            );
        }
        for target in ["lib/Widget.More.vb", "lib/Widget.vb"] {
            let to = id(target).unwrap();
            assert!(
                graph
                    .edges
                    .iter()
                    .all(|e| !(e.from == program && e.to == to && e.kind == EdgeKind::Import)),
                "namespace Imports must not be an Import edge to {target}"
            );
        }
        assert!(
            graph
                .files
                .iter()
                .all(|f| !f.path.as_str().ends_with(".vbproj")),
            "vbproj files are not graph nodes"
        );
        assert!(
            graph.edges.iter().all(|e| {
                let from = graph.files.iter().find(|f| f.id.0 == e.from);
                let to = graph.files.iter().find(|f| f.id.0 == e.to);
                !matches!((from, to), (Some(a), Some(b)) if a.path.as_str().ends_with(".vbproj") || b.path.as_str().ends_with(".vbproj"))
            })
        );
        let helper = id("lib/Helper.vb").unwrap();
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.from == program && e.to == helper && e.kind == EdgeKind::Import),
            "type Imports of Helper is one Import edge"
        );
        assert_eq!(
            report.import_edges,
            graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Import)
                .count()
        );
        assert!(graph.edges.iter().any(|e| e.kind == EdgeKind::Namespace));
    }
}
