#![allow(missing_docs)]

//! The C boundary end to end: `unsafe extern` declarations, `ffi::Ptr` and
//! foreign types, out-parameters, foreign memory, callbacks, handles, the
//! `ffi = false` refusal, the diagnostics, and real C libraries, each run on
//! the bytecode VM, the JIT, and a native build against a C fixture library
//! the suite compiles.

mod common;

#[path = "ffi/acceptance.rs"]
mod acceptance;
#[path = "ffi/callbacks.rs"]
mod callbacks;
#[path = "ffi/compile_fail.rs"]
mod compile_fail;
#[path = "ffi/errno.rs"]
mod errno;
#[path = "ffi/handles.rs"]
mod handles;
#[path = "ffi/memory.rs"]
mod memory;
#[path = "ffi/opaque.rs"]
mod opaque;
#[path = "ffi/opt_in.rs"]
mod opt_in;
#[path = "ffi/out_params.rs"]
mod out_params;
#[path = "ffi/scheduling.rs"]
mod scheduling;
#[path = "ffi/support.rs"]
mod support;
