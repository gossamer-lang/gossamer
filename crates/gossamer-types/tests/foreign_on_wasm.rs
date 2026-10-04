//! A foreign declaration active when building for wasm32 is GT0101: the
//! target has no native library to call into. The cfg target is set once
//! per process, so this case runs in a test binary of its own.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, TypeError, typecheck_source_file};

fn diagnostics(source: &str) -> Vec<TypeError> {
    let mut map = SourceMap::new();
    let file = map.add_file("foreign-on-wasm.gos".to_string(), source.to_string());
    let (parsed, parse_diagnostics) = parse_source_file(source, file);
    assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:#?}");
    let (resolutions, _) = resolve_source_file(&parsed);
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    diagnostics.into_iter().map(|d| d.error).collect()
}

#[test]
fn a_foreign_declaration_active_for_wasm32_is_gt0101_and_a_gated_one_is_not() {
    gossamer_resolve::set_cfg_target_triple("wasm32-unknown-unknown");
    let active = diagnostics("unsafe extern \"C\" { fn abs(x: i32) -> i32 }\nfn main() {}\n");
    assert!(
        active
            .iter()
            .any(|e| matches!(e, TypeError::Foreign(gossamer_types::ForeignError::OnWasm { name }) if name == "abs")),
        "{active:#?}"
    );
    let gated = diagnostics(
        "#[cfg(not(target_family = \"wasm\"))]\nunsafe extern \"C\" { fn abs(x: i32) -> i32 }\nfn main() {}\n",
    );
    assert!(
        !gated.iter().any(|e| matches!(
            e,
            TypeError::Foreign(gossamer_types::ForeignError::OnWasm { .. })
        )),
        "{gated:#?}"
    );
}
