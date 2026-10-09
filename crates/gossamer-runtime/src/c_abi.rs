//! C-ABI runtime surface linked into every native Gossamer program.
//! Every symbol in this module is exported under the `gos_rt_*`
//! prefix so the Cranelift codegen can call them by name. Failure
//! modes are documented per symbol; they never panic across the FFI
//! boundary.
//!
//! # The C-ABI contract
//!
//! Compiled code calls a `gos_rt_*` shim only with arguments of the
//! types its registry row names, and the `SAFETY` comments in this
//! module cite that as "the C-ABI contract":
//!
//! - A pointer or handle argument is null or addresses a live value of
//!   the declared type (a `GosVec`, a string body, a `GosMap`, ...) for
//!   the whole call. A shim whose argument may be null checks it.
//! - A pointer passed beside an element count (a fixed array, a byte
//!   window) addresses that many initialized elements of the width the
//!   shim reads, and a container's slots hold live values of its
//!   element type.
//! - A word argument that holds a value of a counted type (a `String`,
//!   a `Vec`, a node) holds a live one, and a carrier's payload holds a
//!   live value of the shape its arm names.
//! - A value a consuming call receives arrives with the share the call
//!   gives back.
//! - Nothing else reads or writes a value a shim writes through for the
//!   duration of the call: a value shared between goroutines serializes
//!   access with its own lock.
//! - A function address is the entry of a compiled function of the
//!   signature the shim calls it through, and its environment is live
//!   for as long as the shim may call it.
//!
//! A shim converts a handle argument once, at its entry, into a typed
//! view: `as_ref` for a handle that is a Rust value (a `GosMap`, a
//! `GosSet`, a `GosJson`), `vec::VecView::of` or
//! `vec::StrVecView::of` for a `Vec`, whose elements it then reads
//! through bounds-checked safe code. That conversion is the `unsafe`
//! step the contract above justifies; a callable word is turned back
//! into a function pointer through `code_address`.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
// FFI signatures must match the Cranelift / LLVM call sites
// exactly. Keep these allows at file scope rather than dotting
// per-call-site annotations across the C-ABI surface:
// `similar_names` covers `argc`/`argv` Unix convention;
// `many_single_char_names` covers `p`/`n`/`k` in tight memory
// helpers; `items_after_statements` permits inner helper fns
// alongside the call site they document; `same_length_and_capacity`
// fires on `Vec::from_raw_parts(p, n, n)` reconstructing exact
// allocations; `cast_lossless` would force `i64::from(x)` shapes
// that obscure hot-path arithmetic; `doc_markdown` would force
// backticks around every C-ABI symbol name in summary lines.
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
// Pointer casts in this file all reinterpret memory the runtime
// allocates through `gos_rt_gc_alloc`, which is 8-byte aligned, or
// `Vec`-backed buffers (whose alignment matches the elem type). The
// linter cannot see the upstream alignment guarantee and would fire
// on every cast; concentrating the allow at file scope keeps the
// individual sites readable.
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
// Mutable statics back the C-ABI / LLVM-inlined surface (`STDOUT_BUF`,
// `STDOUT_LEN`, etc. - see `stdout_buffer_globals.md`). The lowerer
// emits load/store directly against these symbols, so they have to
// remain `static mut`; the lint flags every read but the contract
// is documented at each declaration.
#![allow(static_mut_refs)]
// Several runtime helpers were `unsafe extern "C"` because they
// touched the (now-retired) bump arena's thread-local, mutated
// `Box::into_raw`-leaked storage, or wrapped `Vec::from_raw_parts`
// reclamation. The migration to `Box::into_raw`-only allocation
// (fix_architecture_ownership.md Stage 4) made several call paths
// safe at the function level - `gos_rt_result_new` is the
// loudest. Keep the existing `unsafe { ... }` wrappers in callers
// for now; the rustc warning is silenced here so the lint comes
// back when we tighten the fn-level `unsafe` story (Stage 6).

/// The code address `addr` names, carrying the provenance its function
/// exposed when compiled code stored it as a word.
///
/// A callable crosses the C ABI as an integer (a closure environment's first
/// word, a registered handler address), and turning an integer back into a
/// function pointer by `transmute` alone yields one with no provenance; every
/// call through such a word goes through this first.
#[inline]
pub(crate) fn code_address(addr: usize) -> *const () {
    std::ptr::with_exposed_provenance(addr)
}

/// Wraps an FFI body in `catch_unwind`, since a Rust panic may not cross an
/// `extern "C"` boundary into compiled Gossamer code.
///
/// A Gossamer fault raised inside the body cannot unwind out of a shim that
/// does not unwind, and its result has no value to answer, so it ends the
/// program as a fault on `main` does. A Rust panic is the shim failing an
/// internal check of its own: it has no value to answer either, and one
/// made up would read as a success or an ordinary error, so it ends the
/// program as `GX0015`. Invalid input is the shim's to answer as an `Err`,
/// never through a panic. A shim that raises a fault the program can contain
/// is `extern "C-unwind"` and uses `ffi_entry_passthrough!`.
macro_rules! ffi_entry {
    ($body:block) => {{
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body));
        match result {
            Ok(v) => v,
            Err(payload) if payload.is::<gossamer_coro::GosPanic>() => {
                let text = payload
                    .downcast_ref::<gossamer_coro::GosPanic>()
                    .map_or_else(String::new, |p| p.0.clone());
                $crate::c_abi::panic::fatal_program_fault("GX0005", "panic: ", &text, false)
            }
            Err(payload) => $crate::c_abi::runtime_fault(&*payload),
        }
    }};
}

/// [`ffi_entry`] for a shim a Gossamer fault may legitimately cross.
///
/// A handler-dispatch shim sits between the server loop and Gossamer code,
/// and a panicking handler is the server's to report - it answers 500 and
/// names the request. Catching the fault here would replace that answer, so
/// a Gossamer panic is re-raised and only a fault raised by the shim itself
/// ends the program.
macro_rules! ffi_entry_passthrough {
    ($body:block) => {{
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body));
        match result {
            Ok(v) => v,
            Err(payload) if payload.is::<gossamer_coro::GosPanic>() => {
                std::panic::resume_unwind(payload)
            }
            Err(payload) => $crate::c_abi::runtime_fault(&*payload),
        }
    }};
}

/// Ends the program for a Rust panic a runtime shim raised: the shim failed an
/// internal check and has no value to answer.
#[cold]
pub(crate) fn runtime_fault(payload: &(dyn std::any::Any + Send)) -> ! {
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    };
    crate::c_abi::panic::fatal_program_fault("GX0015", "runtime fault: ", &msg, false)
}

// ---------------------------------------------------------------
// SyncRawPtr<T> - `repr(transparent)` newtype around `*mut T` that
// is structurally `Send + Sync`. Used as the field type wherever a
// Gossamer runtime container holds a raw pointer that needs to
// cross goroutine / thread boundaries (channels, scheduler queues,
// shared static tables). Centralises the `unsafe impl Send + Sync`
// declaration into one audited site so individual container types
// (`GosVec`, `GosArrIter`, `GosError`, ...) auto-derive Send+Sync
// from their field composition without each one writing its own
// `unsafe impl`. Layout equals `*mut T` exactly - codegen field
// offsets are unchanged. Method surface mirrors `*mut T` and a
// `Deref<Target = *mut T>` impl makes most read-call sites compile
// unchanged. Pointer-validity / aliasing contracts remain the
// caller's responsibility - this newtype only declares thread-
// transferability, not freedom from data races.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct SyncRawPtr<T>(pub *mut T);

// SAFETY: see the type-level documentation above. All cross-thread
// transfer sites are audited by inspection of the containing
// struct's API - this impl declares the FFI handle can move
// between threads, not that the pointee can be mutated concurrently.
unsafe impl<T> Send for SyncRawPtr<T> {}
// SAFETY: as for `Send`: the impl lets the handle be shared between threads;
// the owning type's API serializes every access to the pointee.
unsafe impl<T> Sync for SyncRawPtr<T> {}

impl<T> SyncRawPtr<T> {
    pub const NULL: Self = Self(std::ptr::null_mut());
    pub const fn new(p: *mut T) -> Self {
        Self(p)
    }
    pub const fn from_const(p: *const T) -> Self {
        Self(p.cast_mut())
    }
    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
    pub fn as_const_ptr(&self) -> *const T {
        self.0.cast_const()
    }
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

impl<T> std::ops::Deref for SyncRawPtr<T> {
    type Target = *mut T;
    fn deref(&self) -> &*mut T {
        &self.0
    }
}

impl<T> Default for SyncRawPtr<T> {
    fn default() -> Self {
        Self::NULL
    }
}

impl<T> From<*mut T> for SyncRawPtr<T> {
    fn from(p: *mut T) -> Self {
        Self(p)
    }
}

impl<T> From<*const T> for SyncRawPtr<T> {
    fn from(p: *const T) -> Self {
        Self::from_const(p)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod archive;
pub mod args;
pub mod atomic;
pub mod binding_callback;
pub mod binding_wire;
pub mod btmap;
pub mod bufio;
pub mod bytes_builder;
#[cfg(not(target_arch = "wasm32"))]
pub mod bzip2_codec;
pub mod chan;
pub mod cohort;
pub mod combinator;
pub mod command;
pub mod concat;
pub mod container_heap;
pub mod context;
pub mod coverage;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_aead;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_ecdsa;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_extra;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_jwt;
pub mod csv;
pub mod deque;
pub mod desc_cmp;
pub mod desc_format;
pub mod dynamic;
pub mod encoding;
pub mod errors;
pub mod exec;
pub mod exit_hooks;
pub mod fd;
pub mod ffi;
pub mod flag;
pub mod fn_registry;
pub mod fs;
pub mod gc;
pub mod go;
#[cfg(not(target_arch = "wasm32"))]
pub mod gzip;
pub mod hash;
pub mod heap_i64;
pub mod heap_u8;
#[cfg(not(target_arch = "wasm32"))]
pub mod http3;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_bridges;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_client;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_middleware;
pub mod http_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_request_values;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_security;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_server_handle;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_stream_writer;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_ws;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_ws_accept;
#[cfg(not(target_arch = "wasm32"))]
pub mod image;
pub mod io_handles;
#[cfg(not(target_arch = "wasm32"))]
pub mod iter;
pub mod iter_cross;
pub mod json;
pub mod json_escape;
pub mod lcg;
pub mod ledger;
pub mod len;
pub mod lifecycle;
pub mod map;
pub(crate) mod map_table;
pub mod math;
pub mod math_big;
pub mod math_rand;
#[cfg(not(target_arch = "wasm32"))]
pub mod metrics;
pub mod mime;
pub mod mutex;
pub mod net_ip;
#[cfg(not(target_arch = "wasm32"))]
pub mod net_tcp;
pub mod net_udp;
pub mod net_unix;
pub mod netip;
pub mod os_user;
pub mod panic;
pub mod par;
pub mod pprof_rt;
pub mod print;
pub mod rc;
pub mod regex;
pub mod registry_key;
pub mod result;
pub mod rwlock;
pub mod set;
pub(crate) mod set_table;
pub mod shared;
pub mod signal;
pub mod slog;
pub(crate) mod slot_key;
pub mod sort;
pub mod sql;
pub mod strconv;
pub mod stream;
pub mod string;
pub mod strings;
pub mod sync_extra;
pub mod sync_map;
pub mod sync_vec;
pub mod testing;
pub mod thread_rt;
pub mod time;
pub mod toml_enc;
pub mod trace;
pub mod unicode;
pub mod url;
pub mod utf16;
pub mod utf8;
#[cfg(not(target_arch = "wasm32"))]
pub mod uuid;
pub mod validate;
pub mod vec;
pub mod wg;
#[cfg(not(target_arch = "wasm32"))]
pub mod x509;
pub mod xml_codec;
pub mod yaml_enc;

pub use encoding::*;
#[cfg(not(target_arch = "wasm32"))]
pub use iter::*;
pub use sql::*;
pub use unicode::*;
pub use utf8::*;

#[cfg(not(target_arch = "wasm32"))]
pub use archive::*;
pub use args::*;
pub use atomic::*;
pub use binding_wire::*;
pub use btmap::*;
pub use bufio::*;
pub use bytes_builder::*;
#[cfg(not(target_arch = "wasm32"))]
pub use bzip2_codec::*;
pub use chan::*;
pub use combinator::*;
pub use concat::*;
pub use container_heap::*;
pub use context::*;
pub use coverage::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_aead::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_ecdsa::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_extra::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_jwt::*;
pub use csv::*;
pub use deque::*;
pub use desc_format::*;
pub use dynamic::*;
pub use errors::*;
pub use exec::*;
pub use flag::*;
pub use fs::*;
pub use gc::*;
pub use go::*;
#[cfg(not(target_arch = "wasm32"))]
pub use gzip::*;
pub use hash::*;
pub use heap_i64::*;
pub use heap_u8::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_bridges::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_client::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_middleware::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_request_values::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_security::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_server::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_ws::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_ws_accept::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http3::*;
#[cfg(not(target_arch = "wasm32"))]
pub use image::*;
pub use io_handles::*;
pub use iter_cross::*;
pub use json::*;
pub use json_escape::*;
pub use lcg::*;
pub use len::*;
pub use map::*;
pub use math::*;
pub use math_big::*;
pub use math_rand::*;
#[cfg(not(target_arch = "wasm32"))]
pub use metrics::*;
pub use mime::*;
pub use mutex::*;
pub use net_ip::*;
#[cfg(not(target_arch = "wasm32"))]
pub use net_tcp::*;
pub use net_udp::*;
pub use net_unix::*;
pub use netip::*;
pub use os_user::*;
pub use panic::*;
pub use pprof_rt::*;
pub use print::*;
pub use rc::*;
pub use regex::*;
pub use result::*;
pub use rwlock::*;
pub use set::*;
pub use shared::*;
pub use signal::*;
pub use slog::*;
pub use sort::*;
pub use strconv::*;
pub use stream::*;
pub use string::*;
pub use strings::*;
pub use sync_extra::*;
pub use sync_map::*;
pub use sync_vec::*;
pub use testing::*;
pub use thread_rt::*;
pub use time::*;
pub use toml_enc::*;
pub use trace::*;
pub use url::*;
pub use utf16::*;
#[cfg(not(target_arch = "wasm32"))]
pub use uuid::*;
pub use validate::*;
pub use vec::*;
pub use wg::*;
#[cfg(not(target_arch = "wasm32"))]
pub use x509::*;
pub use xml_codec::*;
pub use yaml_enc::*;

/// Wakes every signal and descriptor wait so each re-checks whether the
/// cohort it runs under was cancelled. Cohort cancellation on every tier
/// calls this: those waits block outside the scheduler's parking, where a
/// cohort's own waiter list cannot reach them.
pub fn wake_cancellable_waits() {
    signal::wake_waiters();
    fd::wake_waits();
}

#[cfg(test)]
mod ffi_entry_fault_tests {
    use std::process::Command;

    const CHILD_ENV: &str = "GOSSAMER_FFI_ENTRY_FAULT_CHILD";

    fn failing_result() -> i128 {
        ffi_entry!({ std::hint::black_box(None::<i128>).expect("internal check failed") })
    }

    fn failing_pointer() -> *mut u8 {
        ffi_entry!({
            let slots: [*mut u8; 0] = [];
            let index = slots.len();
            slots[index]
        })
    }

    fn failing_scalar() -> i64 {
        ffi_entry!({
            let digits = "not a number";
            digits.parse::<i64>().expect("digits parse")
        })
    }

    fn failing_unit() {
        ffi_entry!({
            panic!("unit shim failed");
        });
    }

    fn failing_passthrough() -> bool {
        ffi_entry_passthrough!({
            std::hint::black_box(Err::<bool, &str>("broken")).expect("passthrough state")
        })
    }

    /// Runs the shim named in the child environment, which must not return.
    #[test]
    fn child_entry() {
        let Ok(which) = std::env::var(CHILD_ENV) else {
            return;
        };
        match which.as_str() {
            "result" => println!("answered {}", failing_result()),
            "pointer" => println!("answered {:?}", failing_pointer()),
            "scalar" => println!("answered {}", failing_scalar()),
            "unit" => {
                failing_unit();
                println!("answered unit");
            }
            "passthrough" => println!("answered {}", failing_passthrough()),
            _ => {}
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns this test binary as a child; Miri cannot run a process
    fn a_shim_that_panics_ends_the_program_whatever_it_returns() {
        if std::env::var(CHILD_ENV).is_ok() {
            return;
        }
        let exe = std::env::current_exe().expect("test binary path");
        for which in ["result", "pointer", "scalar", "unit", "passthrough"] {
            let out = Command::new(&exe)
                .args([
                    "--exact",
                    "c_abi::ffi_entry_fault_tests::child_entry",
                    "--nocapture",
                ])
                .env(CHILD_ENV, which)
                .output()
                .expect("spawn test child");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(101), "{which}: {stdout} {stderr}");
            assert!(
                stderr.contains("error[GX0015]: runtime fault: "),
                "{which}: {stderr}"
            );
            assert!(
                !stdout.contains("answered"),
                "{which} answered a value: {stdout}"
            );
        }
    }
}
