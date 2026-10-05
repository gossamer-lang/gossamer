//! `gos` - command-line entry point for the Gossamer toolchain.
//!
//! The actual subcommand surface lives in [`gossamer_cli::cli`]
//! (clap derive + dispatch table). This shim performs one
//! pre-step before delegating to the library: when a project
//! declares `[rust-bindings]`, build the per-project runner and
//! re-exec into it. The runner sets `GOSSAMER_IN_RUNNER=1` so the
//! re-entry returns immediately to in-process dispatch.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    gossamer_cli::prepare_main_thread();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match gossamer_cli::dispatch_runner_if_needed(&args) {
        gossamer_cli::DispatchOutcome::InProcess => gossamer_cli::run_main_with_args(&args),
        gossamer_cli::DispatchOutcome::Failed(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
