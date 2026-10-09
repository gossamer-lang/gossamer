#![no_main]
//! Fuzz target: full front-end through HIR + MIR lowering, then
//! a pass through the MIR optimiser.
//!
//! Catches the class of bug where an optimisation pass corrupts
//! the body in a way the structural verifier would reject, or leaves it
//! releasing a handle twice or reading one after its release on some path.
//! We run `verify_body` and the reference-count verifier after `optimise`
//! so a regression in either surfaces as a fuzz failure.

use libfuzzer_sys::fuzz_target;

use gossamer_hir::lower_source_file;
use gossamer_lex::SourceMap;
use gossamer_mir::{lower_program, optimise, rc_verify::verify_rc, verify::verify_body};
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fuzz_target!(|data: &[u8]| {
    // The lexer is deliberately unsafe-free. Its process-wide symbol table
    // retains spellings for the fuzz process lifetime; the fuzzer process is
    // bounded by its runner and long-lived compiler sessions will move to a
    // session-owned interner rather than reclaiming live symbols unsafely.
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
        // Lowering is undefined on inputs that didn't parse
        // cleanly. The parse fuzz target covers that path.
        return;
    }
    let (resolutions, r_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    if !r_diags.is_empty() {
        return;
    }
    let mut tcx = TyCtxt::new();
    let (table, t_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    if !t_diags.is_empty() {
        return;
    }
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut bodies = lower_program(&hir, &mut tcx);
    for body in &mut bodies {
        // `optimise` itself calls `debug_verify_body` between
        // passes under `debug_assertions`. We additionally run a
        // post-pass verify here so a release-mode fuzz run also
        // catches structural drift.
        optimise(body, &tcx);
        if verify_body(body).is_err() {
            // Verifier-rejected bodies are a real bug: the
            // optimiser should preserve structural invariants.
            // Panic so libFuzzer reports the input as a crash.
            panic!("MIR verifier rejected body `{}` after optimise()", body.name);
        }
        if let Err(faults) = verify_rc(body) {
            panic!("reference-count verifier rejected body `{}`: {faults:?}", body.name);
        }
    }
});
