//! End-to-end regressions for the resolvers-a audit (go, java, kotlin, lua,
//! dart, csharp). Each test builds a tiny repo and checks the graph edges.

use astrolabe_core::index::{index_repo, IndexOptions};
use astrolabe_core::types::EdgeKind;
use std::fs;
use std::path::Path;
use std::time::Instant;

fn write(root: &Path, rel: &str, c: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, c).unwrap();
}

/// `"Import|Namespace from -> to"` for every non-call edge, sorted.
fn edges(root: &Path) -> Vec<String> {
    let opts = IndexOptions {
        persist: false,
        ..IndexOptions::default()
    };
    let (g, _) = index_repo(root, &opts);
    let name = |id: u32| {
        g.files
            .iter()
            .find(|f| f.id.0 == id)
            .map(|f| f.path.as_str().to_string())
            .unwrap()
    };
    let mut v: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.kind != EdgeKind::Call)
        .map(|e| format!("{:?} {} -> {}", e.kind, name(e.from), name(e.to)))
        .collect();
    v.sort();
    v
}

fn has(v: &[String], line: &str) -> bool {
    v.iter().any(|l| l == line)
}

fn no_self_loops(v: &[String]) {
    for l in v {
        let (a, b) = l.split_once(' ').unwrap().1.split_once(" -> ").unwrap();
        assert_ne!(a, b, "self-referencing edge: {l}");
    }
}

#[test]
fn go_same_module_name_keeps_nearest_module() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    for x in ["a", "b"] {
        write(r, &format!("examples/{x}/go.mod"), "module example\n");
        write(r, &format!("examples/{x}/util/util.go"), "package util\n");
        write(
            r,
            &format!("examples/{x}/main.go"),
            "package main\nimport \"example/util\"\nfunc main(){ util.F() }\n",
        );
    }
    let e = edges(r);
    assert!(
        has(&e, "Import examples/a/main.go -> examples/a/util/util.go"),
        "{e:#?}"
    );
    assert!(
        has(&e, "Import examples/b/main.go -> examples/b/util/util.go"),
        "{e:#?}"
    );
    assert!(!has(
        &e,
        "Import examples/b/main.go -> examples/a/util/util.go"
    ));
}

#[test]
fn go_import_links_every_file_of_the_package() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "go.mod", "module m\n");
    write(r, "p/a.go", "package p\nfunc A(){}\n");
    write(r, "p/b.go", "package p\nfunc B(){}\n");
    write(r, "p/b_test.go", "package p\n");
    write(
        r,
        "main.go",
        "package main\nimport \"m/p\"\nfunc main(){ p.B() }\n",
    );
    let e = edges(r);
    assert!(has(&e, "Import main.go -> p/a.go"), "{e:#?}");
    assert!(has(&e, "Import main.go -> p/b.go"), "{e:#?}");
    assert!(!has(&e, "Import main.go -> p/b_test.go"));
}

#[test]
fn go_replace_to_local_dir_without_gomod() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(
        r,
        "go.mod",
        "module m\n\nrequire example.com/x v0.0.0\nreplace example.com/x => ./third/x\n",
    );
    write(r, "third/x/x.go", "package x\n");
    write(
        r,
        "main.go",
        "package main\nimport \"example.com/x\"\nfunc main(){ x.F() }\n",
    );
    let e = edges(r);
    assert!(has(&e, "Import main.go -> third/x/x.go"), "{e:#?}");
}

#[test]
fn java_wildcard_and_java_imports_kotlin() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(
        r,
        "src/main/java/com/y/One.java",
        "package com.y;\npublic class One {}\n",
    );
    write(
        r,
        "src/main/java/com/y/Two.java",
        "package com.y;\npublic class Two {}\n",
    );
    write(
        r,
        "src/main/kotlin/com/z/KtThing.kt",
        "package com.z\nclass KtThing\n",
    );
    write(
        r,
        "src/main/java/com/x/App.java",
        "package com.x;\nimport com.y.*;\nimport com.z.KtThing;\npublic class App {}\n",
    );
    let e = edges(r);
    assert!(
        has(
            &e,
            "Import src/main/java/com/x/App.java -> src/main/java/com/y/One.java"
        ),
        "{e:#?}"
    );
    assert!(
        has(
            &e,
            "Import src/main/java/com/x/App.java -> src/main/java/com/y/Two.java"
        ),
        "{e:#?}"
    );
    assert!(
        has(
            &e,
            "Import src/main/java/com/x/App.java -> src/main/kotlin/com/z/KtThing.kt"
        ),
        "{e:#?}"
    );
}

#[test]
fn self_referencing_edges_are_not_created() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(
        r,
        "src/main/java/com/x/A.java",
        "package com.x;\nimport static com.x.A.CONST;\npublic class A { static final int CONST = 1; }\n",
    );
    write(
        r,
        "src/main/kotlin/q/Self.kt",
        "package q\nimport q.Self.Companion.x\nclass Self { companion object { val x = 1 } }\n",
    );
    write(
        r,
        "lua/foo/bar.lua",
        "local b = require('bar')\nreturn {}\n",
    );
    let e = edges(r);
    no_self_loops(&e);
}

#[test]
fn kotlin_expect_actual_all_linked() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    for s in ["common", "android", "ios"] {
        write(
            r,
            &format!("shared/src/{s}Main/kotlin/p/Foo.kt"),
            "package p\nclass Foo\n",
        );
    }
    write(
        r,
        "app/src/main/kotlin/App.kt",
        "package app\nimport p.Foo\nclass App\n",
    );
    let e = edges(r);
    for s in ["common", "android", "ios"] {
        assert!(
            has(
                &e,
                &format!("Import app/src/main/kotlin/App.kt -> shared/src/{s}Main/kotlin/p/Foo.kt")
            ),
            "{e:#?}"
        );
    }
}

#[test]
fn dart_pubspec_under_build_directory_is_a_package() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "tool/build/tools/pubspec.yaml", "name: tools\n");
    write(r, "tool/build/tools/lib/t.dart", "class T {}\n");
    write(r, "app/pubspec.yaml", "name: app\n");
    write(
        r,
        "app/lib/a.dart",
        "import 'package:tools/t.dart';\nclass A {}\n",
    );
    let e = edges(r);
    assert!(
        has(&e, "Import app/lib/a.dart -> tool/build/tools/lib/t.dart"),
        "{e:#?}"
    );
}

#[test]
fn lua_rockspec_single_line_modules() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(
        r,
        "x-1.0-1.rockspec",
        "build = { type = \"builtin\", modules = { [\"m.a\"]=\"lua/foo/bar.lua\", [\"m.b\"]=\"lua/foo/baz.lua\" } }\n",
    );
    write(r, "lua/foo/bar.lua", "return {}\n");
    write(r, "lua/foo/baz.lua", "return {}\n");
    write(
        r,
        "lua/main.lua",
        "local a = require('m.a')\nlocal b = require('m.b')\n",
    );
    let e = edges(r);
    assert!(has(&e, "Import lua/main.lua -> lua/foo/bar.lua"), "{e:#?}");
    assert!(has(&e, "Import lua/main.lua -> lua/foo/baz.lua"), "{e:#?}");
}

#[test]
fn lua_library_code_does_not_match_sibling_for_external_module() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "lua/foo/bar.lua", "local j = require('json')\n");
    write(r, "lua/foo/json.lua", "return {}\n");
    let e = edges(r);
    assert!(
        !has(&e, "Import lua/foo/bar.lua -> lua/foo/json.lua"),
        "{e:#?}"
    );
}

#[test]
fn csharp_remove_list_midpath_glob_system_ns_delegate_and_raw_string() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "App/App.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\">\n<ItemGroup>\n<Compile Remove=\"Gen1.cs;Gen2.cs\" />\n<Compile Remove=\"Auto/**/Out/*.cs\" />\n</ItemGroup></Project>");
    write(r, "App/Gen1.cs", "namespace Gen { class G1 {} }\n");
    write(r, "App/Gen2.cs", "namespace Gen { class G2 {} }\n");
    write(r, "App/Keep.cs", "namespace Gen { class K {} }\n");
    write(
        r,
        "App/Auto/x/y/Out/Skip.cs",
        "namespace AutoNs { class S {} }\n",
    );
    write(
        r,
        "App/Auto/x/Keep2.cs",
        "namespace AutoNs { class K2 {} }\n",
    );
    write(
        r,
        "App/UsesGen.cs",
        "using Gen;\nusing AutoNs;\nnamespace App { class U {} }\n",
    );
    // The repo implements its own System.* namespace.
    write(
        r,
        "App/Dyn.cs",
        "namespace System.Linq.Dynamic { public class Parser {} }\n",
    );
    write(
        r,
        "App/UsesDyn.cs",
        "using System.Linq.Dynamic;\nusing System;\nnamespace App { class V {} }\n",
    );
    // delegate type name and $""" raw interpolated string.
    write(
        r,
        "App/D.cs",
        "namespace App { public delegate void Handler(int x); public class Plain {} }\n",
    );
    write(
        r,
        "App/UsesD.cs",
        "using static App.Handler;\nnamespace App { class X {} }\n",
    );
    write(r, "App/Raw2.cs", "namespace App { class R { string s = $\"\"\"\n  { \"a\": \"b\" }\n  \"\"\"; } class AfterRaw2 {} }\n");
    write(
        r,
        "App/UsesRaw.cs",
        "using static App.AfterRaw2;\nnamespace App { class Y {} }\n",
    );
    let e = edges(r);
    assert!(
        !e.iter()
            .any(|l| l.contains("-> App/Gen1.cs") || l.contains("-> App/Gen2.cs")),
        "{e:#?}"
    );
    assert!(has(&e, "Namespace App/UsesGen.cs -> App/Keep.cs"), "{e:#?}");
    assert!(
        !e.iter().any(|l| l.contains("Auto/x/y/Out/Skip.cs")),
        "{e:#?}"
    );
    assert!(
        has(&e, "Namespace App/UsesGen.cs -> App/Auto/x/Keep2.cs"),
        "{e:#?}"
    );
    assert!(has(&e, "Namespace App/UsesDyn.cs -> App/Dyn.cs"), "{e:#?}");
    assert!(has(&e, "Import App/UsesD.cs -> App/D.cs"), "{e:#?}");
    assert!(has(&e, "Import App/UsesRaw.cs -> App/Raw2.cs"), "{e:#?}");
}

/// One huge namespace: edges per importer are capped. Prints before/after
/// numbers when run with `--nocapture`.
#[test]
fn csharp_huge_namespace_edges_are_capped_but_nearest_kept() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(
        r,
        "P/P.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>",
    );
    let n = 400usize;
    for i in 0..n {
        write(
            r,
            &format!("P/d{}/F{i}.cs", i % 20),
            &format!("using Big;\nnamespace Big {{ public class C{i} {{ }} }}\n"),
        );
    }
    let e = edges(r);
    let ns = e.iter().filter(|l| l.starts_with("Namespace")).count();
    println!("namespace edges: {ns} (uncapped would be {})", n * (n - 1));
    assert!((n * 63..=n * 64).contains(&ns), "{ns}");
    // Nearest first: a file links to same-directory declarations.
    assert!(has(&e, "Namespace P/d0/F0.cs -> P/d0/F20.cs"));
}

/// Detect-phase timing on many projects. `cargo test -p astrolabe-core
/// --test resolvers_audit_a csharp_many_projects -- --ignored --nocapture`
#[test]
#[ignore]
fn csharp_many_projects_detect_is_linear() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let (p, f) = (300usize, 40usize);
    for i in 0..p {
        write(
            r,
            &format!("P{i}/P{i}.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>",
        );
        for j in 0..f {
            write(
                r,
                &format!("P{i}/sub/F{j}.cs"),
                &format!("namespace P{i}.Sub {{ public class C{j} {{ }} }}\n"),
            );
        }
    }
    let t = Instant::now();
    let opts = IndexOptions {
        persist: false,
        ..IndexOptions::default()
    };
    let (g, _) = index_repo(r, &opts);
    println!(
        "csharp {p} projects x {f} files = {} files: index {:?}",
        g.files.len(),
        t.elapsed()
    );
}

/// VB mirrors the C# per-project bucket fix: 300 projects x 40 files used to
/// be quadratic. Generous wall bound so it only trips on a regression.
#[test]
fn vb_many_projects_detect_is_linear() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let (p, f) = (300usize, 40usize);
    for i in 0..p {
        write(
            r,
            &format!("P{i}/P{i}.vbproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>",
        );
        for j in 0..f {
            write(
                r,
                &format!("P{i}/sub/F{j}.vb"),
                &format!("Namespace P{i}.Sub\nPublic Class C{j}\nEnd Class\nEnd Namespace\n"),
            );
        }
    }
    let t = Instant::now();
    let opts = IndexOptions {
        persist: false,
        ..IndexOptions::default()
    };
    let (g, _) = index_repo(r, &opts);
    let el = t.elapsed();
    println!("vb {p}x{f} = {} files: {el:?}", g.files.len());
    assert!(el.as_secs() < 15, "VB detect not linear: {el:?}");
}
