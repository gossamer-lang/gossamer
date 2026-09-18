use gossamer_hir::{lift_closures, lower_source_file};
use gossamer_lex::SourceMap;
use gossamer_mir::{Body, lower_program};
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

/// Lowers `source` through the same front end and MIR passes a build runs.
pub(crate) fn lower(source: &str) -> (Vec<Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(diagnostics.is_empty(), "typecheck: {diagnostics:?}");
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let hir = lift_closures(hir, &mut tcx);
    let bodies = lower_program(&hir, &mut tcx);
    (bodies, tcx)
}
