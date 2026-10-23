//! End-to-end tests for the name resolver driven by parsed AST fixtures.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::{PrimitiveTy, Resolution, ResolveError, resolve_source_file};

fn parse(source: &str) -> gossamer_ast::SourceFile {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "parse errors: {diags:?}");
    sf
}

#[test]
fn retained_resolver_oom_reproducer_terminates() {
    const REPRO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/resolve/oom-14d3e69bb91be48852f7693451d6081e1ef06af7"
    ));
    let source = std::str::from_utf8(REPRO).expect("retained resolver artifact is UTF-8");
    let mut map = SourceMap::new();
    let file = map.add_file("resolve-oom-repro.gos", source.to_owned());
    let (ast, _parse_diagnostics) = parse_source_file(source, file);
    let _ = resolve_source_file(&ast);
}

#[test]
fn simple_hello_world_resolves_without_diagnostics() {
    let source = "use std::strings\n\nfn main() {\n    strings::repeat(\"a\", 2)\n}\n";
    let sf = parse(source);
    let (resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert!(!resolutions.is_empty());
}

#[test]
fn undefined_name_produces_unresolved_diagnostic() {
    let source = "fn main() { xyzzy }\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert_eq!(diags.len(), 1);
    assert!(matches!(
        diags[0].error,
        ResolveError::UnresolvedName { ref name } if name == "xyzzy"
    ));
}

#[test]
fn a_type_path_to_a_missing_member_of_a_module_is_unresolved() {
    let source = "mod render {\n    pub struct Path { n: i64 }\n}\n\
                  type Canvas = render::Canvas\n\
                  struct Painter { canvas: render::Canvas }\n\
                  fn main() {}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    let missing: Vec<&str> = diags
        .iter()
        .filter_map(|d| match &d.error {
            ResolveError::UnresolvedName { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(missing, ["render::Canvas", "render::Canvas"], "{diags:?}");
}

#[test]
fn misspelled_stdlib_member_suggests_the_module_own_export() {
    let source = "use std::iter\n\nfn main() { let _ = iter::fliter(|x: i64| x > 1, #[1, 2]) }\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    let unresolved = diags
        .iter()
        .find(|d| matches!(d.error, ResolveError::UnresolvedName { .. }))
        .expect("misspelled stdlib member is unresolved");
    assert_eq!(
        unresolved.in_scope_candidate.as_deref(),
        Some("iter::filter")
    );
}

#[test]
fn duplicate_top_level_items_report_diagnostic() {
    let source = "fn foo() {}\nfn foo() {}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(
        diags
            .iter()
            .any(|d| matches!(&d.error, ResolveError::DuplicateItem { name } if name == "foo")),
        "expected duplicate-item diagnostic, got: {diags:?}"
    );
}

#[test]
fn primitive_types_are_always_in_scope() {
    let source = "fn add(x: i32, y: i32) -> i32 { x }\n";
    let sf = parse(source);
    let (resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    let found_primitive = resolutions.sorted_entries().iter().any(|(_, res)| {
        matches!(
            res,
            Resolution::Primitive(PrimitiveTy::Int(gossamer_resolve::IntWidth::W32))
        )
    });
    assert!(found_primitive, "expected i32 primitive resolution");
}

#[test]
fn forward_reference_between_items_resolves() {
    let source = "fn main() { helper() }\nfn helper() {}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn let_binding_shadows_and_resolves_to_local() {
    let source = "fn main() {\n    let x = 1\n    let y = x\n}\n";
    let sf = parse(source);
    let (resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    let has_local = resolutions
        .sorted_entries()
        .iter()
        .any(|(_, res)| matches!(res, Resolution::Local(_)));
    assert!(has_local, "expected local resolution for `x`");
}

#[test]
fn use_list_imports_each_name_into_scope() {
    let source = "use std::sync::{AtomicU64, WaitGroup}\n\nfn main() {\n    AtomicU64::new(0)\n    WaitGroup::new()\n}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn use_of_an_item_no_module_exports_reports_it() {
    for source in [
        "use std::sync::Frobnicator\n",
        "use std::sync::{Mutex, Frobnicator}\n",
    ] {
        let sf = parse(source);
        let (_resolutions, diags) = resolve_source_file(&sf);
        let tags: Vec<&str> = diags.iter().map(|d| d.error.tag()).collect();
        assert_eq!(tags, ["unknown-std-item"], "for {source:?}");
    }
}

#[test]
fn use_of_a_real_stdlib_item_resolves() {
    for source in [
        "use std::sync::Mutex\n",
        "use std::sync::{Mutex, WaitGroup, channel}\n",
        "use std::{env, fs}\n",
        "use std::encoding::json::Value\n",
        "use std::collections::{Map, Set}\n",
        "use std::io as printing\n",
        "use std::fs::DirInfo\n",
    ] {
        let sf = parse(source);
        let (_resolutions, diags) = resolve_source_file(&sf);
        assert!(
            diags.is_empty(),
            "unexpected diagnostics for {source:?}: {diags:?}"
        );
    }
}

#[test]
fn imported_name_resolves_to_import_resolution() {
    let source = "use std::strings\n\nfn main() {\n    strings::repeat(\"a\", 2)\n}\n";
    let sf = parse(source);
    let (resolutions, _diags) = resolve_source_file(&sf);
    let has_import = resolutions
        .sorted_entries()
        .iter()
        .any(|(_, res)| matches!(res, Resolution::Import { .. }));
    assert!(has_import, "expected import resolution for `strings`");
}

#[test]
fn qualified_stdlib_module_requires_import() {
    let source = "fn main() {\n    env::args()\n}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(
        diags.iter().any(
            |diag| matches!(&diag.error, ResolveError::UnresolvedName { name } if name == "env")
        ),
        "expected unimported stdlib module to be unresolved: {diags:?}"
    );
}

#[test]
fn qualified_stdlib_module_resolves_after_import() {
    let source = "use std::env\n\nfn main() {\n    env::args()\n}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn full_stdlib_item_import_resolves_bound_tail_name() {
    let source =
        "use std::iter::skip_while\n\nfn main() {\n    skip_while(|x: i64| x < 3, [1, 2, 3])\n}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn bogus_multisegment_use_path_is_rejected() {
    for source in ["use iter\n", "use stp::iter\n", "use whatever::iter\n"] {
        let sf = parse(source);
        let (_resolutions, diags) = resolve_source_file(&sf);
        assert!(
            diags
                .iter()
                .any(|diag| matches!(diag.error, ResolveError::UnknownModulePath { .. })),
            "expected unknown module path diagnostic for {source:?}: {diags:?}"
        );
    }
}

#[test]
fn relative_use_paths_are_not_stdlib_typo_checked() {
    let source = "fn helper(a: i64, b: i64) -> i64 { a + b }\n\
#[cfg(test)]\n\
mod tests {\n\
    use super::helper as add\n\
    #[test]\n\
    fn adds() { let _ = add(1, 2) }\n\
}\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn example_programs_resolve_without_diagnostics() {
    for name in ["hello_world.gos", "line_count.gos", "web_server.gos"] {
        let path = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).expect("read example");
        let sf = parse(&source);
        let (_resolutions, diags) = resolve_source_file(&sf);
        let unresolved: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d.error, ResolveError::UnresolvedName { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{path} unresolved: {unresolved:?}");
    }
}

#[test]
fn use_names_a_local_module_from_the_crate_root() {
    // The bundler inlines a sibling file as `mod options { ... }`, so a
    // `use` may name it directly, through `root::`, or through `crate::`.
    let body = "mod options {\n    enum Colorize { Always, Never }\n    fn tag() -> i64 { 1 }\n}\n";
    for path in [
        "use options::Colorize",
        "use root::options::Colorize",
        "use crate::options::Colorize",
        "use options::{Colorize, tag}",
    ] {
        let source = format!("{path}\n\n{body}fn main() {{ }}\n");
        let sf = parse(&source);
        let (_, diags) = resolve_source_file(&sf);
        assert!(
            diags.is_empty(),
            "{path}: unexpected diagnostics: {diags:?}"
        );
    }
}

#[test]
fn use_names_a_nested_local_module() {
    let source = "use example::options::tag\n\nmod example {\n    mod options {\n        fn tag() -> i64 { 1 }\n    }\n}\nfn main() { }\n";
    let sf = parse(source);
    let (_, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn use_of_an_unknown_module_path_still_reports() {
    let source = "use nowhere::Thing\n\nfn main() { }\n";
    let sf = parse(source);
    let (_, diags) = resolve_source_file(&sf);
    assert!(
        diags
            .iter()
            .any(|d| matches!(d.error, ResolveError::UnknownModulePath { .. })),
        "expected UnknownModulePath, got: {diags:?}"
    );
}

#[test]
fn renamed_container_names_report_their_replacement() {
    // The rename hint used to reach only `use` declarations, so a bare
    // `HashSet<i64>` in a signature or a `HashSet::new()` call reported a
    // plain missing name and left the reader to guess the new spelling.
    for source in [
        "fn f(s: HashSet<i64>) -> i64 { 0 }\nfn main() { }\n",
        "fn main() { let _s = HashSet::new() }\n",
        "fn f(m: HashMap<String, i64>) -> i64 { 0 }\nfn main() { }\n",
        "fn main() { let _d = VecDeque::new() }\n",
    ] {
        let sf = parse(source);
        let (_, diags) = resolve_source_file(&sf);
        assert!(
            diags
                .iter()
                .any(|d| matches!(d.error, ResolveError::RemovedStdItem { .. })),
            "no rename hint for:\n{source}\n{diags:?}"
        );
    }
}

#[test]
fn out_of_line_mod_without_a_body_is_rejected() {
    // The bundler blanks `mod name` when it inlines the module from the
    // project layout, so one that reaches the resolver names a module
    // nothing supplies - rejecting it here keeps the failure at check
    // time instead of an unbound name at run time.
    let source = "mod helper\nfn main() { helper::hi() }\n";
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(
        diags.iter().any(
            |d| matches!(&d.error, ResolveError::MissingModuleSource { name } if name == "helper")
        ),
        "expected a missing-module-source diagnostic, got: {diags:?}"
    );
}

#[test]
fn module_relative_paths_resolve_against_the_enclosing_module() {
    // `self::child::item`, a bare `child::item`, and `super::sibling::item`
    // all name items registered under their path from the unit root.
    let source = concat!(
        "mod outer {\n",
        "    pub mod child { pub fn value() -> i64 { 1 } }\n",
        "    pub fn all() -> i64 {\n",
        "        self::child::value() + child::value() + super::other::value()\n",
        "    }\n",
        "}\n",
        "mod other { pub fn value() -> i64 { 2 } }\n",
        "fn main() { println(\"{}\", outer::all()) }\n",
    );
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
}

#[test]
fn a_private_module_on_the_path_is_named_by_the_diagnostic() {
    // The blocked name is `pub`; the private `nest` module is the only
    // place a `pub` would unblock it, so it is what the report names.
    let source = concat!(
        "mod deep {\n",
        "    mod nest { pub fn nested() -> i64 { 1 } }\n",
        "}\n",
        "fn main() { println(\"{}\", deep::nest::nested()) }\n",
    );
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    assert!(
        diags.iter().any(|d| matches!(
            &d.error,
            ResolveError::PrivateItem { name, kind, .. } if name == "nest" && *kind == "module"
        )),
        "expected the private module to be named, got: {diags:?}"
    );
}

#[test]
fn a_std_macro_named_as_a_value_path_is_rejected() {
    for (source, macro_name) in [
        (
            "use std::fmt\n\nfn main() {\n    fmt::println(\"x\")\n}\n",
            "println",
        ),
        (
            "use std::fmt\n\nfn main() {\n    let _ = fmt::format(\"x\")\n}\n",
            "format",
        ),
        (
            "use std::panic\n\nfn main() {\n    panic::panic(\"x\")\n}\n",
            "panic",
        ),
    ] {
        let sf = parse(source);
        let (_resolutions, diags) = resolve_source_file(&sf);
        assert!(
            diags.iter().any(|diag| matches!(
                &diag.error,
                ResolveError::StdMacroAsValue { name, .. } if name == macro_name
            )),
            "expected a macro-as-value diagnostic for {source:?}: {diags:?}"
        );
    }
}

/// The bundled shape a project compiles as: `src/engine/mod.gos` and its
/// sibling files become nested inline modules, and a `use` written inside one
/// is hoisted to the file's top with the module it was written in recorded on
/// it. The three slots are the file's own level, `mod engine`, and
/// `mod engine::plan`.
fn bundled(at_root: &str, in_engine: &str, in_plan: &str) -> String {
    format!(
        "{at_root}\n\
         pub mod engine {{\n\
             {in_engine}\n\
             pub mod plan {{ {in_plan} }}\n\
             pub mod filter {{ pub fn keep(n: i64) -> bool {{ n > 1 }} }}\n\
             pub struct Handle {{ pub n: i64 }}\n\
             pub enum Colour {{ Red, Blue }}\n\
             pub const LIMIT: i64 = 4\n\
             pub fn run() -> i64 {{ 1 }}\n\
         }}\n\
         fn main() {{ }}\n"
    )
}

fn unknown_module_paths(source: &str) -> Vec<String> {
    let sf = parse(source);
    let (_resolutions, diags) = resolve_source_file(&sf);
    diags
        .iter()
        .filter_map(|d| match &d.error {
            ResolveError::UnknownModulePath { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_relative_use_naming_a_module_of_this_unit_resolves() {
    for spelled in [
        "use crate::engine",
        "use crate::engine::filter",
        "use crate::engine::filter::keep",
        "use crate::engine::Handle",
        "use crate::engine::Colour::Red",
        "use crate::engine::LIMIT",
        "use crate::engine::run",
    ] {
        assert!(
            unknown_module_paths(&bundled(spelled, "", "")).is_empty(),
            "{spelled} names something this unit declares"
        );
    }
    // Written inside `mod engine`, where the anchor is that module.
    for spelled in [
        "use self::filter",
        "use self::filter as f",
        "use crate::engine::filter",
    ] {
        assert!(
            unknown_module_paths(&bundled("", spelled, "")).is_empty(),
            "{spelled} names a module of the package it is written in"
        );
    }
    // Written inside `mod engine::plan`, whose `super` is `engine`.
    for spelled in [
        "use super::filter",
        "use super::filter::keep",
        "use self::super::run",
    ] {
        assert!(
            unknown_module_paths(&bundled("", "", spelled)).is_empty(),
            "{spelled} names a module of the package it is written in"
        );
    }
}

#[test]
fn a_relative_use_naming_nothing_is_rejected_where_it_is_written() {
    // Binding a name nothing declares stands in front of every path headed by
    // it, so the use site resolves against the import, reports nothing, and
    // leaves the failure to run time.
    for (spelled, reported) in [
        ("use crate::missing", "crate::missing"),
        ("use crate::missing as m", "crate::missing"),
        ("use crate::engien", "crate::engien"),
        ("use crate::engine::nowhere", "crate::engine::nowhere"),
        ("use super::src::engine", "super::src::engine"),
    ] {
        assert_eq!(
            unknown_module_paths(&bundled(spelled, "", "")),
            vec![reported.to_string()],
            "{spelled} names nothing"
        );
    }
    assert_eq!(
        unknown_module_paths(&bundled("", "", "use super::nowhere")),
        vec!["super::nowhere".to_string()],
        "`super` from `engine::plan` is `engine`, which declares no `nowhere`"
    );
}

#[test]
fn a_relative_use_keeps_the_anchor_of_the_module_it_was_written_in() {
    // A `use` inside a `mod { }` body is hoisted to the file's imports, so the
    // module it was written in travels with it: `super::filter` names
    // `engine::filter` there and nothing at the file's own level.
    assert!(unknown_module_paths(&bundled("", "", "use super::filter")).is_empty());
    assert_eq!(
        unknown_module_paths(&bundled("use super::filter", "", "")),
        vec!["super::filter".to_string()],
        "the same spelling at the file's own level reaches above the unit"
    );
}

#[test]
fn a_misspelled_relative_use_names_the_module_this_unit_declares() {
    let sf = parse(&bundled("use crate::engien", "", ""));
    let (_resolutions, diags) = resolve_source_file(&sf);
    let unknown = diags
        .iter()
        .find(|d| matches!(d.error, ResolveError::UnknownModulePath { .. }))
        .expect("the path names no module");
    assert_eq!(
        unknown.in_scope_candidate.as_deref(),
        Some("crate::engine"),
        "the candidate is drawn from this unit, not from the standard library"
    );
}

#[test]
fn a_relative_use_may_name_what_the_unit_imported_from_outside_it() {
    // `use super::add` inside a `mod tests { }` reaches a name the enclosing
    // module bound with an import of its own, whose items live in a package or
    // a Rust binding rather than in this unit.
    let source = "use std::fs\n\
                  pub mod tests { use super::fs }\n\
                  fn main() { }\n";
    assert!(unknown_module_paths(source).is_empty());
}

#[test]
fn an_aliased_relative_use_binds_the_module_it_names() {
    // The alias has to record the module's path from the unit root, which is
    // the key its items registered under, or every path through the alias is
    // unbound where it is dispatched.
    let sf = parse(&bundled("", "use self::filter as f", ""));
    let (resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert_eq!(
        resolutions.module_alias("f"),
        Some("engine::filter"),
        "the alias leads to the module's path from the unit root"
    );
}

#[test]
fn a_packages_own_crate_paths_survive_being_embedded_as_a_dependency() {
    // The bundler wraps a path dependency's source in a module of its own, so
    // the package's `crate::` paths no longer start at the unit root. They
    // still name the package's own modules.
    let source = "pub mod dep {\n\
                  use crate::engine\n\
                  pub mod engine { pub fn open() -> i64 { 1 } }\n\
                  }\n\
                  fn main() { }\n";
    assert!(unknown_module_paths(source).is_empty());
    // The same spelling in the consuming unit, which declares no `engine`.
    let consumer = "use crate::engine\n\
                    pub mod dep { pub mod engine { pub fn open() -> i64 { 1 } } }\n\
                    fn main() { }\n";
    assert_eq!(
        unknown_module_paths(consumer),
        vec!["crate::engine".to_string()]
    );
}

#[test]
fn a_relative_use_list_is_checked_entry_by_entry() {
    // Every entry binds a name of its own, so each is validated as the whole
    // path the list's root and that entry's segments spell.
    assert!(unknown_module_paths(&bundled("use crate::{engine}", "", "")).is_empty());
    assert!(unknown_module_paths(&bundled("use crate::engine::{run, filter}", "", "")).is_empty());
    assert!(unknown_module_paths(&bundled("", "", "use super::{filter, run}")).is_empty());
    assert_eq!(
        unknown_module_paths(&bundled("use crate::{engine, missing}", "", "")),
        vec!["crate::missing".to_string()],
        "the entry that names nothing is the one reported"
    );
    assert_eq!(
        unknown_module_paths(&bundled("use crate::engine::{run, nowhere}", "", "")),
        vec!["crate::engine::nowhere".to_string()]
    );
}

#[test]
fn a_use_list_entry_naming_a_module_binds_that_module() {
    // Without the alias record, a path through the name the entry introduced
    // is unbound where it is dispatched.
    let sf = parse(&bundled("use crate::engine::{filter}", "", ""));
    let (resolutions, diags) = resolve_source_file(&sf);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    assert_eq!(resolutions.module_alias("filter"), Some("engine::filter"));
}
