//! wasm32 backend for the in-browser Gossamer playground.
//!
//! Exposes two `wasm-bindgen` entry points consumed by the web
//! component:
//!
//! - [`run`] compiles `source` through the same parse -> resolve ->
//!   typecheck -> exhaustiveness -> lower pipeline `gos` uses, then
//!   executes `main` on the bytecode VM with stdout / stderr captured
//!   into buffers.
//! - [`check`] runs only the front-end gate and returns the structured
//!   diagnostics.
//!
//! The Cranelift JIT and the LLVM AOT tiers do not exist on
//! wasm32-unknown-unknown; the interpreter links a no-op JIT stub, so
//! every program runs on the register-based bytecode VM. Host I/O
//! (sockets, filesystem, processes, HTTP server / client, TLS, SQL) and
//! C-library codecs (bzip2, zstd) are unavailable in the browser
//! sandbox and are gated out of the linked standard library; pure
//! computation - strings, collections, math, encoding / JSON, hashing,
//! regex, iterators, formatting - is fully available.
//!
//! What the browser does provide it provides for real: the clock behind
//! `Instant` and `time::now_ms` is the host's own, and an environment
//! variable a program sets is readable back for the extent of the run.
//! What it cannot provide is reported rather than taken: a filesystem
//! call answers an error, a `process::exit` ends the run with its
//! status, and a wait no goroutine is left to end reports `GX0011` -
//! this target settles every goroutine at its spawn, so nothing runs
//! alongside the program that waits.

use std::cell::RefCell;
use std::sync::Once;

use gossamer_diagnostics::{Diagnostic, RenderOptions};
use gossamer_lex::{FileId, SourceMap};
use gossamer_resolve::resolve_source_file;
use gossamer_types::TyCtxt;
use serde::Serialize;
use wasm_bindgen::prelude::*;

const ENTRY_NAME: &str = "playground.gos";

thread_local! {
    static STDOUT_BUF: RefCell<String> = const { RefCell::new(String::new()) };
    static STDERR_BUF: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST_PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Installs the process panic hook.
///
/// wasm32-unknown-unknown has no unwinder, so a Rust panic aborts the
/// module: `catch_unwind` below never runs and `run` never returns. The
/// hook runs first, and what it records here is what [`last_panic`]
/// hands the page in place of a bare `unreachable`.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            LAST_PANIC.with(|slot| *slot.borrow_mut() = info.to_string());
            console_error_panic_hook::hook(info);
        }));
    });
}

/// Message of the panic that ended the last [`run`], or the empty
/// string when it ended without one.
#[wasm_bindgen]
#[must_use]
pub fn last_panic() -> String {
    LAST_PANIC.with(|slot| slot.borrow().clone())
}

/// Appends VM stdout into the per-thread capture buffer. Installed via
/// `gossamer_interp::set_stdout_writer`, whose `Writer` is a bare
/// `fn(&str)` pointer, so the buffer has to live in a thread-local
/// rather than a captured closure.
fn capture_stdout(text: &str) {
    STDOUT_BUF.with(|b| b.borrow_mut().push_str(text));
}

/// Appends VM stderr into the per-thread capture buffer.
fn capture_stderr(text: &str) {
    STDERR_BUF.with(|b| b.borrow_mut().push_str(text));
}

/// Captured program output plus a terminal error, returned by [`run`].
#[derive(Serialize)]
struct RunResult {
    stdout: String,
    stderr: String,
    error: Option<String>,
    fuel_used: u64,
}

/// One structured diagnostic, returned by [`check`].
#[derive(Serialize)]
struct DiagnosticInfo {
    severity: String,
    message: String,
    line: u32,
    col: u32,
    code: String,
}

/// Front-end diagnostics for a source file, returned by [`check`].
#[derive(Serialize)]
struct CheckResult {
    diagnostics: Vec<DiagnosticInfo>,
}

/// Compiles and executes `source` on the bytecode VM, capturing
/// stdout / stderr. Returns a serialized `RunResult`
/// (`{ stdout, stderr, error, fuel_used }`).
///
/// `fuel` caps loop iterations (default 100M): an unbounded loop aborts with a
/// `GX0009` execution-limit error instead of hanging the tab, and `fuel_used`
/// reports how much of the budget the run consumed.
#[wasm_bindgen]
#[must_use]
pub fn run(source: &str, fuel: Option<u64>) -> JsValue {
    const DEFAULT_FUEL: u64 = 100_000_000;
    install_panic_hook();
    let budget = fuel.unwrap_or(DEFAULT_FUEL);
    gossamer_interp::fuel::set_fuel(budget);

    STDOUT_BUF.with(|b| b.borrow_mut().clear());
    STDERR_BUF.with(|b| b.borrow_mut().clear());
    LAST_PANIC.with(|b| b.borrow_mut().clear());

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_pipeline(source)));

    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(message)) => Some(message),
        Err(payload) => Some(format!("internal error: {}", panic_payload(&payload))),
    };

    let result = RunResult {
        stdout: STDOUT_BUF.with(|b| b.borrow().clone()),
        stderr: STDERR_BUF.with(|b| b.borrow().clone()),
        error,
        fuel_used: budget.saturating_sub(gossamer_interp::fuel::fuel_remaining()),
    };
    serde_wasm_bindgen::to_value(&result).unwrap_or(JsValue::NULL)
}

/// Runs the front-end gate on `source` and returns its diagnostics as a
/// serialized `CheckResult` (`{ diagnostics: [{ severity, message,
/// line, col, code }] }`). Never executes the program.
#[wasm_bindgen]
#[must_use]
pub fn check(source: &str) -> JsValue {
    install_panic_hook();

    let augmented = gossamer_parse::autoderive::augment_source(source);
    let mut map = SourceMap::new();
    let file_id = map.add_file(ENTRY_NAME.to_string(), augmented.clone());
    let (diagnostics, _) = front_end(&augmented, file_id);

    let result = CheckResult {
        diagnostics: diagnostics
            .iter()
            .map(|diag| diagnostic_info(diag, &map))
            .collect(),
    };
    serde_wasm_bindgen::to_value(&result).unwrap_or(JsValue::NULL)
}

/// Full compile-and-run pipeline. Returns `Ok(())` on a clean run,
/// `Err(message)` for a front-end rejection or a runtime error /
/// panic. Output is captured through the installed writers, not the
/// return value.
fn run_pipeline(user_source: &str) -> Result<(), String> {
    // Synthesize `from_json` / `to_json` and other derives as real
    // source so the program has genuine methods, exactly as `gos`
    // does before checking.
    let source = gossamer_parse::autoderive::augment_source(user_source);

    gossamer_interp::set_stdout_writer(capture_stdout);
    gossamer_interp::set_stderr_writer(capture_stderr);

    // Comptime fold: evaluate `comptime { ... }` / `comptime fn` calls
    // on the VM and splice their result literals in, exactly as the
    // `gos` pre-pass does. A front-end rejection skips the fold so
    // the authoritative gate below reports it.
    let source = if source.contains("comptime") {
        let mut fold_map = SourceMap::new();
        let fold_file = fold_map.add_file(ENTRY_NAME.to_string(), source.clone());
        match front_end(&source, fold_file) {
            (_, Some((program, tcx))) => {
                gossamer_interp::fold_into_source(&program, tcx, &source, ENTRY_NAME)?
            }
            _ => source,
        }
    } else {
        source
    };

    let mut map = SourceMap::new();
    let file_id = map.add_file(ENTRY_NAME.to_string(), source.clone());

    let (diagnostics, lowered) = front_end(&source, file_id);
    let Some((program, tcx)) = lowered else {
        let mut rendered = String::new();
        for diag in &diagnostics {
            rendered.push_str(&gossamer_diagnostics::render(
                diag,
                &map,
                RenderOptions { colour: false },
            ));
            rendered.push('\n');
        }
        capture_stderr(&rendered);
        return Err(format!(
            "{} front-end error(s); refusing to execute",
            diagnostics.len()
        ));
    };

    gossamer_interp::set_program_name(ENTRY_NAME);
    gossamer_interp::set_program_args(&[]);

    let mut vm = gossamer_interp::Vm::new();
    vm.load(&program, tcx, true)
        .map_err(|err| format!("vm load failed: {err}"))?;
    drop(program);

    let call = vm.call("main", Vec::new());
    vm.release_jit_prelude();
    gossamer_interp::join_outstanding_goroutines();
    gossamer_interp::flush_runtime_stdout();

    match call {
        Ok(_) => Ok(()),
        // `process::exit` is a request to stop, not a failure: a zero
        // status ends the run as quietly as returning from `main` does.
        Err(err) if gossamer_interp::exit_status(&err) == Some(0) => Ok(()),
        Err(err) if let Some(code) = gossamer_interp::exit_status(&err) => {
            Err(format!("exited with status {code}"))
        }
        Err(err) if gossamer_interp::is_panic_error(&err) => {
            Err(gossamer_interp::panic_message(&err))
        }
        Err(err) => Err(format!("runtime error: {err}")),
    }
}

/// Parse + resolve + the shared front-end checks under the same policy as
/// `gossamer_driver::check_frontend`, minus the on-disk frontend cache
/// (irrelevant to a one-shot wasm run). On a clean gate the program is
/// lowered to HIR and returned alongside its type context with any
/// warnings; otherwise every diagnostic is returned and nothing is lowered.
fn front_end(
    source: &str,
    file_id: FileId,
) -> (Vec<Diagnostic>, Option<(gossamer_hir::HirProgram, TyCtxt)>) {
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(source, file_id);
    let mut diagnostics: Vec<Diagnostic> = parse_diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect();

    // A program that does not parse is not the program the later passes
    // see: `autoderive::augment_source` declines to synthesize from a
    // recovered tree, so the derived `fmt` / `to_string` / serde surface a
    // clean parse would carry is absent, and every pass below would report
    // its absence against a line the user wrote correctly. The parse
    // diagnostics are the actionable report.
    let parse_failed = !parse_diags.is_empty();

    let (resolutions, resolve_diags) = resolve_source_file(&sf);
    let mut tcx = TyCtxt::new();
    let earlier_fatal = !diagnostics.is_empty();
    let checks = gossamer_types::check_resolved_unit(
        &mut sf,
        &resolutions,
        &resolve_diags,
        parse_failed,
        earlier_fatal,
        &mut tcx,
        &mut (),
    );
    diagnostics.extend(checks.fatal);
    let accepted = diagnostics.is_empty();
    diagnostics.extend(checks.advisory);

    if accepted {
        let program = gossamer_hir::lower_source_file(&sf, &resolutions, &checks.table, &mut tcx);
        (diagnostics, Some((program, tcx)))
    } else {
        (diagnostics, None)
    }
}

/// Lowers a [`Diagnostic`] to the JS-facing shape, resolving its
/// primary label's byte span to a one-based line / column.
fn diagnostic_info(diag: &Diagnostic, map: &SourceMap) -> DiagnosticInfo {
    let (line, col) = diag
        .labels
        .iter()
        .find(|label| label.primary)
        .or_else(|| diag.labels.first())
        .map_or((0, 0), |label| {
            let position = map.line_col(label.location.file, label.location.span.start);
            (position.line, position.column)
        });

    DiagnosticInfo {
        severity: diag.severity.tag().to_string(),
        message: diag.title.clone(),
        line,
        col,
        code: diag.code.as_str().to_string(),
    }
}

/// Best-effort extraction of a `catch_unwind` payload's message.
fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::front_end;

    /// The playground drives the front end itself rather than going
    /// through `gossamer_driver`, so every pass the driver runs has to
    /// be run here too. Named arguments and parameter defaults are the
    /// pass that was missed: a call omitting a defaulted parameter
    /// reached the checker with fewer arguments than the function
    /// declares and was reported as an arity error, so the tour's
    /// `arguments` lesson could not run in the browser while the same
    /// program ran natively.
    #[test]
    fn a_call_omitting_a_defaulted_parameter_checks() {
        let source = concat!(
            "fn greet(name: String, greeting: String = \"hello\", excited: bool = false)",
            " -> String {\n",
            "    let line = greeting + \", \" + name\n",
            "    if excited { line + \"!\" } else { line }\n",
            "}\n\n",
            "fn main() {\n",
            "    println(\"{}\", greet(\"world\"))\n",
            "    println(\"{}\", greet(\"world\", \"hi\"))\n",
            "    println(\"{}\", greet(\"world\", excited: true))\n",
            "    println(\"{}\", greet(greeting: \"hey\", name: \"g\", excited: true))\n",
            "}\n",
        );
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file("playground.gos", source.to_string());
        let (diagnostics, lowered) = front_end(source, file_id);
        assert!(
            diagnostics.is_empty(),
            "defaults and named arguments must resolve before type checking: {:?}",
            diagnostics
                .iter()
                .map(|d| d.title.clone())
                .collect::<Vec<_>>()
        );
        assert!(lowered.is_some(), "a clean program must lower");
    }
}
