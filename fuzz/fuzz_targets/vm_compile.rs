#![no_main]
//! Fuzz target: bytecode compilation of arbitrary well-formed
//! programs.
//!
//! Walks the front end through `Vm::load`, which lowers HIR to
//! bytecode chunks but does **not** execute `main`. Execution is
//! deliberately out of scope here: running arbitrary user code
//! under libFuzzer pulls in scheduler + netpoller + stdout side
//! effects that make the runner noisy and slow. `Vm::load` is
//! the boundary where the bytecode compiler runs, and that is
//! the surface most likely to harbour panics on adversarial but
//! well-typed input.

use libfuzzer_sys::fuzz_target;

use gossamer_hir::lower_source_file;
use gossamer_interp::Vm;
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fuzz_target!(|data: &[u8]| {
    // Keep fuzzing on the production lexer lifecycle. Reclaiming global
    // symbols between callbacks was unsound for retained compiled programs.
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if source.len() > 32 * 1024 {
        return;
    }
    let mut map = SourceMap::new();
    let file = map.add_file("fuzz.gos", source.to_string());
    let (mut sf, diags) = parse_source_file(source, file);
    if !diags.is_empty() {
        return;
    }
    let (mut resolutions, r_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    if !r_diags.is_empty() {
        return;
    }
    let mut tcx = TyCtxt::new();
    let (table, t_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    if !t_diags.is_empty() {
        return;
    }
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    // `Vm::load` is the bytecode compile boundary. `enable_inlining`
    // is on so the user-function inliner - part of the `gos` /
    // `gos build` compile path - is included in the fuzzed surface.
    // We swallow the `RuntimeResult` - a clean error is fine; a panic
    // is the regression libFuzzer is hunting for.
    let _ = vm.load(&hir, tcx, true);
});
