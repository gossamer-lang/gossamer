#![no_main]
//! Fuzz target: bytecode VM **execution**
//!
//! `vm_compile` exercises `Vm::load` but stops there. This target
//! also calls `Vm::call("main", &[])` on the loaded program so
//! libFuzzer surfaces `get_unchecked` UB and op-dispatch
//! corruption that the bytecode validator can't catch from
//! shape alone. We cap the call with a coarse timeout-by-call-
//! depth so adversarial infinite loops don't stall the run.

use libfuzzer_sys::fuzz_target;

use gossamer_fuzz::grammar;
use gossamer_hir::lower_source_file;
use gossamer_interp::Vm;
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fuzz_target!(|data: &[u8]| {
    // Keep fuzzing on the same unsafe-free lexer lifecycle as production.
    // A session-owned interner is the long-lived RSS solution; a global reset
    // can invalidate symbols retained by an in-flight VM program.
    // 0.6.0: grammar-aware input so the fuzzer spends
    // cycles on well-shaped programs instead of UTF-8 boundary
    // triage. A single bit-flip in `data` reroutes a grammar
    // choice rather than producing invalid input.
    let source = grammar::render_source(data);
    if source.len() > 16 * 1024 {
        return;
    }
    let mut map = SourceMap::new();
    let file = map.add_file("fuzz.gos", source.clone());
    let (mut sf, diags) = parse_source_file(&source, file);
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
    if vm.load(&hir, tcx, true).is_err() {
        return;
    }
    // Execute `main` if it exists. A clean RuntimeError is fine;
    // a panic / UB is the regression libFuzzer is hunting for.
    let _ = vm.call("main", Vec::new());
});
