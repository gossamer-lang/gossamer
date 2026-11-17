//! `fs::read_dir` lists without per-entry metadata exactly when nothing the
//! program does can observe `size` or `modified_ms`.

use gossamer_driver::check_frontend;
use gossamer_hir::{HirExpr, HirExprKind, HirItemKind, for_each_child_expr, lower_source_file};
use gossamer_lex::SourceMap;

/// The leaf the lowered `read_dir` wrapper calls for `source`.
fn read_dir_leaf(source: &str) -> String {
    let augmented = gossamer_parse::autoderive::augment_source(source);
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", augmented);
    let outcome = check_frontend(map.source(file), file);
    assert!(
        outcome.diagnostics.is_empty(),
        "front end: {:?}",
        outcome.diagnostics
    );
    let mut checked = outcome.checked;
    let program = lower_source_file(
        &checked.sf,
        &checked.resolutions,
        &checked.table,
        &mut checked.tcx,
    );
    let body = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "__gos_fs_read_dir" => f.body.as_ref(),
            _ => None,
        })
        .expect("the read_dir wrapper is injected");
    let mut found = Vec::new();
    for stmt in &body.block.stmts {
        if let gossamer_hir::HirStmtKind::Let { init: Some(e), .. }
        | gossamer_hir::HirStmtKind::Expr { expr: e, .. } = &stmt.kind
        {
            leaves_in(e, &mut found);
        }
    }
    if let Some(tail) = &body.block.tail {
        leaves_in(tail, &mut found);
    }
    assert_eq!(found.len(), 1, "one leaf call: {found:?}");
    found.remove(0)
}

fn leaves_in(expr: &HirExpr, found: &mut Vec<String>) {
    if let HirExprKind::Path { segments, .. } = &expr.kind
        && let [only] = segments.as_slice()
        && only.name.starts_with("__gos_fs_read_dir_")
    {
        found.push(only.name.clone());
    }
    if let HirExprKind::Block(block) = &expr.kind {
        for stmt in &block.stmts {
            if let gossamer_hir::HirStmtKind::Let { init: Some(e), .. }
            | gossamer_hir::HirStmtKind::Expr { expr: e, .. } = &stmt.kind
            {
                leaves_in(e, found);
            }
        }
        if let Some(tail) = &block.tail {
            leaves_in(tail, found);
        }
        return;
    }
    for_each_child_expr(expr, &mut |child| leaves_in(child, found));
}

const NAMES: &str = "__gos_fs_read_dir_names_raw";
const FULL: &str = "__gos_fs_read_dir_raw";

fn program(body: &str) -> String {
    format!("use std::fs\nuse std::encoding::json\n\n{body}\n")
}

#[test]
fn names_and_kinds_alone_list_without_metadata() {
    let source = program(
        r#"fn main() {
    let mut entries = fs::read_dir("/tmp").unwrap_or(#[])
    entries.sort_by_key(|e| e.name)
    let dirs = entries.filter(|e| e.is_dir).map(|e| e.path)
    for e in entries { if e.is_file { println(e.name) } }
    println(dirs.len())
}"#,
    );
    assert_eq!(read_dir_leaf(&source), NAMES);
}

#[test]
fn a_program_function_reading_names_lists_without_metadata() {
    let source = program(
        r#"fn label(e: fs::DirInfo) -> String { e.name }
fn main() {
    let entries = fs::read_dir("/tmp").unwrap_or(#[])
    println(label(entries[0]))
}"#,
    );
    assert_eq!(read_dir_leaf(&source), NAMES);
}

#[test]
fn a_pattern_naming_only_names_lists_without_metadata() {
    let source = program(
        r#"fn main() {
    let entries = fs::read_dir("/tmp").unwrap_or(#[])
    let fs::DirInfo { name, is_dir, .. } = entries[0]
    println(f"{name} {is_dir}")
}"#,
    );
    assert_eq!(read_dir_leaf(&source), NAMES);
}

#[test]
fn every_observation_of_metadata_keeps_the_full_listing() {
    for body in [
        // A field read.
        r#"for e in fs::read_dir("/tmp").unwrap_or(#[]) { println(e.size) }"#,
        r#"println(fs::read_dir("/tmp").unwrap_or(#[]).map(|e| e.modified_ms).sum())"#,
        // A pattern naming a metadata field.
        r#"let fs::DirInfo { size, .. } = fs::read_dir("/tmp").unwrap_or(#[])[0]
    println(size)"#,
        // Whole-value uses: formatting, comparison, sorting, serialization.
        r#"println(f"{fs::read_dir("/tmp").unwrap_or(#[])}")"#,
        r#"let a = fs::read_dir("/tmp").unwrap_or(#[])
    println(a[0] == a[0])"#,
        r#"let mut a = fs::read_dir("/tmp").unwrap_or(#[])
    a.sort()
    println(a.len())"#,
        r#"println(json::encode(fs::read_dir("/tmp").unwrap_or(#[])).is_ok())"#,
    ] {
        let source = program(&format!("fn main() {{\n    {body}\n}}"));
        assert_eq!(read_dir_leaf(&source), FULL, "{body}");
    }
}

#[test]
fn metadata_read_through_a_program_function_or_generic_keeps_the_full_listing() {
    for source in [
        r#"fn stamp(e: fs::DirInfo) -> i64 { e.modified_ms }
fn main() { println(stamp(fs::read_dir("/tmp").unwrap_or(#[])[0])) }"#,
        r#"fn show<T>(x: T) -> String { f"{x}" }
fn main() { println(show(fs::read_dir("/tmp").unwrap_or(#[])[0])) }"#,
        r#"struct Holder { info: fs::DirInfo }
fn main() { println(f"{Holder { info: fs::read_dir("/tmp").unwrap_or(#[])[0] }}") }"#,
    ] {
        assert_eq!(read_dir_leaf(&program(source)), FULL, "{source}");
    }
}
