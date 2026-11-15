//! Model-context-protocol server for Gossamer.
//! Runs an MCP server over stdio (newline-delimited JSON-RPC): the
//! toolchain (check / explain / execute / build / test / fmt / doc /
//! lint / fix / audit / `feature_status`) and semantic navigation (hover /
//! definition / references / workspace symbols) as MCP tools, plus the
//! skill card as a resource and prompt. Exec tools spawn the `gos`
//! binary so user-program panics and exits never take the server down,
//! and a timeout kills everything that child started.

#![forbid(unsafe_code)]

mod exec;
mod nav;
mod protocol;
mod server;
mod tools;
mod transport;

use std::path::PathBuf;

pub use exec::ExecOutcome;

/// Server settings supplied by the embedding binary.
pub struct ServerConfig {
    /// Path to the `gos` executable the exec-family tools spawn.
    pub gos_exe: PathBuf,
}

/// Runs the server over the process's stdio streams until EOF.
pub fn run_stdio(config: ServerConfig) -> std::io::Result<()> {
    server::run(std::io::stdin().lock(), std::io::stdout(), &config)
}

/// Runs the server over arbitrary streams; test entry point.
pub fn testing_run<R: std::io::BufRead, W: std::io::Write>(
    reader: R,
    writer: W,
    config: &ServerConfig,
) -> std::io::Result<()> {
    server::run(reader, writer, config)
}

/// Runs a subprocess through the exec runner; test entry point.
pub fn testing_exec(exe: &std::path::Path, args: &[String]) -> Result<ExecOutcome, String> {
    exec::run_gos(exe, args, std::time::Duration::from_mins(1))
}
