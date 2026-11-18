//! Language-server-protocol adapter for Gossamer.
//! Runs an LSP server over stdio with compiler and lint diagnostics,
//! quick fixes, completion, navigation, rename, semantic tokens,
//! inlay hints, symbols, folding, signature help, and formatting.
//!

#![forbid(unsafe_code)]

mod comptime;
mod inlay;
mod members;
mod navigation;
mod protocol;
mod semantic_tokens;
mod server;
mod session;
mod stdlib_index;
mod symbols;
mod synthesis;
pub mod uri;
mod workspace_index;

pub use server::{handle, run_stdio};
