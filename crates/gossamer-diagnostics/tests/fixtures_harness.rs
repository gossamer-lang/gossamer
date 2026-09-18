//! Stream B.3 - acceptance harness that asserts each fixture under
//! `tests/fixtures/` emits its declared `// ERROR: G???` code.
//! Every fixture carries a `// ERROR: <CODE>` comment on the first
//! line. The harness runs parse + resolve + type-check on the
//! fixture, collects all diagnostics, and asserts the expected code
//! is present. Extra diagnostics are tolerated so recovery can land
//! freely.

use std::fs;
use std::path::PathBuf;

use gossamer_ast::{ItemKind, SourceFile};
use gossamer_diagnostics::Diagnostic;
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::{ResolveError, resolve_source_file};
use gossamer_types::{TyCtxt, check_arena_escapes, check_parallel_adapters, typecheck_source_file};

fn collect_diagnostics(source: &str, file_name: &str) -> Vec<Diagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file(file_name.to_string(), source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    let mut out: Vec<Diagnostic> = parse_diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect();

    let (resolutions, resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let in_scope = collect_names(&sf);
    // Every resolver diagnostic, matching what `gos check` reports: a
    // whitelist here would silently exclude any new one from fixturing.
    for diag in &resolve_diags {
        out.push(diag.to_diagnostic(&in_scope));
    }

    let mut tcx = TyCtxt::new();
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    for diag in &type_diags {
        out.push(diag.to_diagnostic());
    }

    for diag in check_arena_escapes(&sf, &resolutions, &table, &tcx) {
        out.push(diag.to_diagnostic());
    }

    for diag in check_parallel_adapters(&sf, &resolutions, &table, &tcx) {
        out.push(diag.to_diagnostic());
    }

    out
}

fn collect_names(sf: &SourceFile) -> Vec<&str> {
    let mut out = Vec::new();
    for item in &sf.items {
        let name = match &item.kind {
            ItemKind::Fn(decl) => decl.name.name.as_str(),
            ItemKind::Struct(decl) => decl.name.name.as_str(),
            ItemKind::Enum(decl) => decl.name.name.as_str(),
            ItemKind::Trait(decl) => decl.name.name.as_str(),
            ItemKind::TypeAlias(decl) => decl.name.name.as_str(),
            ItemKind::Const(decl) => decl.name.name.as_str(),
            ItemKind::Static(decl) => decl.name.name.as_str(),
            ItemKind::Mod(decl) => decl.name.name.as_str(),
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => continue,
        };
        out.push(name);
    }
    out
}

fn extract_expected_code(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("// ERROR:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn run_fixture(path: &PathBuf) {
    let source = fs::read_to_string(path).expect("read fixture");
    let expected = extract_expected_code(&source)
        .unwrap_or_else(|| panic!("fixture {} lacks a `// ERROR:` marker", path.display()));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("fixture file name");
    let diagnostics = collect_diagnostics(&source, file_name);
    assert!(
        diagnostics.iter().any(|d| d.code.as_str() == expected),
        "fixture {} expected code {expected}, got {:?}",
        path.display(),
        diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect::<Vec<_>>()
    );
}

macro_rules! fixture_test {
    ($name:ident, $file:expr) => {
        #[test]
        fn $name() {
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join($file);
            run_fixture(&base);
        }
    };
}

fixture_test!(gp0001_unexpected_token, "GP0001_unexpected_token.gos");
fixture_test!(gr0001_unresolved_name, "GR0001_unresolved_name.gos");
fixture_test!(gr0003_duplicate_item, "GR0003_duplicate_item.gos");
fixture_test!(gt0001_type_mismatch, "GT0001_type_mismatch.gos");
fixture_test!(gm0003_arena_escape, "GM0003_arena_escape.gos");
fixture_test!(gr0017_unknown_loop_label, "GR0017_unknown_loop_label.gos");
fixture_test!(gt0070_unknown_impl_trait, "GT0070_unknown_impl_trait.gos");
fixture_test!(
    gt0071_trait_in_type_position,
    "GT0071_trait_in_type_position.gos"
);
fixture_test!(
    gt0072_impl_item_not_in_trait,
    "GT0072_impl_item_not_in_trait.gos"
);
fixture_test!(
    gt0073_conflicting_trait_impl,
    "GT0073_conflicting_trait_impl.gos"
);
fixture_test!(
    gl0055_undeclared_return_value,
    "GL0055_undeclared_return_value.gos"
);

#[test]
fn all_fixtures_have_error_marker() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let entries: Vec<_> = fs::read_dir(&dir)
        .expect("read fixtures dir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext == "gos")
        })
        .collect();
    assert!(!entries.is_empty(), "must have at least one fixture");
    for entry in entries {
        let source = fs::read_to_string(entry.path()).unwrap();
        let expected = extract_expected_code(&source);
        assert!(
            expected.is_some(),
            "fixture {} is missing `// ERROR:` marker",
            entry.path().display()
        );
    }
}

#[test]
fn did_you_mean_suggests_a_close_match() {
    let source = "fn banana() -> i64 { 1i64 }\nfn main() { let _ = bananaa }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("typo.gos", source.to_string());
    let (sf, _) = parse_source_file(source, file);
    let (_res, diags) = resolve_source_file(&sf);
    let in_scope = collect_names(&sf);
    let rendered: Vec<Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d.error, ResolveError::UnresolvedName { .. }))
        .map(|d| d.to_diagnostic(&in_scope))
        .collect();
    assert!(
        !rendered.is_empty(),
        "expected an unresolved-name diagnostic"
    );
    assert!(
        rendered
            .iter()
            .any(|d| d.suggestions.iter().any(|s| s.replacement == "banana")),
        "did-you-mean should suggest `banana`; got {:?}",
        rendered
            .iter()
            .map(|d| d.suggestions.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn did_you_mean_suggests_prelude_type_case_match() {
    let source = "fn main() { let map = Btreemap::from({\"example\": 1}) }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("prelude-typo.gos", source.to_string());
    let (sf, _) = parse_source_file(source, file);
    let (_res, diags) = resolve_source_file(&sf);
    let in_scope = collect_names(&sf);
    let rendered: Vec<Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d.error, ResolveError::UnresolvedName { .. }))
        .map(|d| d.to_diagnostic(&in_scope))
        .collect();
    assert!(
        rendered
            .iter()
            .any(|d| d.suggestions.iter().any(|s| s.replacement == "BTreeMap")),
        "did-you-mean should suggest `BTreeMap`; got {:?}",
        rendered
            .iter()
            .map(|d| d.suggestions.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn parser_recovers_and_reports_multiple_errors_in_one_file() {
    let source = "fn main() { let x = ; let y = ; }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("multi.gos", source.to_string());
    let (_sf, diags) = parse_source_file(source, file);
    let codes: Vec<&str> = diags
        .iter()
        .map(|d| d.to_diagnostic().code.as_str())
        .collect();
    assert!(
        codes.len() >= 2,
        "recovery should surface more than one parse error, got {codes:?}",
    );
}
