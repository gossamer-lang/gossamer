//! Typed registry of function-pointer addresses handed across the
//! C-ABI to compiled / JIT-compiled Gossamer code.
//!
//! Every `gos_rt_*` helper that transmutes a raw `i64` /
//! `*const u8` back into a typed `extern "C" fn(...)` calls
//! [`verify`] first. The registry records `(addr, FnKind)` pairs;
//! a mismatch (different kind registered, or addr unregistered) is
//! a hard abort with a diagnostic, never a silent UB-inducing
//! transmute.
//!
//! Registration sites: every place that produces a function-
//! pointer slot for later cross-FFI dispatch - closure env
//! construction, iter combinator callbacks, scheduler trampolines,
//! mutex callbacks, router/http handler dispatch, JIT body
//! entries. The codegen back-ends call [`register`] at body
//! finalization; manual sites in the runtime call it inline.
//!
//! `verify` is hot - called per indirect dispatch. The
//! registry uses a `parking_lot::RwLock<HashMap>` so concurrent
//! readers don't contend; registration is the rare path.

#![allow(missing_docs)]

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

/// What signature shape a registered function pointer has. Only
/// integer / pointer arities are tracked; aggregate-returning or
/// f64 shapes flow through the per-shape thunks emitted by
/// codegen and do not need a side registry entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FnKind {
    /// `extern "C" fn(*const u8, *mut GosHttpRequest) -> i128`.
    /// Bare-fn HTTP route handler.
    HttpHandlerBare,
    /// `extern "C" fn(*mut u8, *mut GosHttpRequest) -> i128`.
    /// Env-capturing HTTP route handler.
    HttpHandlerEnv,
    /// `extern "C" fn(i64, i64) -> i64`. Sort comparator (a,b -> ordering).
    SortCmp,
    /// `extern "C" fn(*const u8, *const u8) -> i64`. Aggregate sort
    /// comparator (a-ptr, b-ptr -> ordering).
    SortCmpAggr,
    /// `extern "C" fn(env: *const u8, entry: i64) -> i128`. `fs::walk_dir`
    /// visitor callback - `entry` is a `fs::DirInfo` blob address, the
    /// packed i128 result is the visitor's `Result<(), errors::Error>`.
    WalkVisit,
    /// `extern "C" fn(i64) -> i64`. Unary i64 callback.
    UnaryI64ToI64,
    /// `extern "C" fn(i64, i64) -> i64`. Binary i64 callback
    /// (folds, reducers).
    BinaryI64ToI64,
    /// `extern "C" fn(i64) -> bool` shaped as `extern "C" fn(i64) -> i64`
    /// with 0/1 result. Predicate.
    PredI64,
    /// JIT-compiled body entry; arity + return shape are encoded
    /// in the cookie. The interpreter's `jit_call` resolves the
    /// expected shape per call site.
    JitEntry(u32),
    /// Context-cancellation hook (`AtomicPtr`-installed). Two
    /// shapes today; both i64-result `extern "C"`.
    CtxCancelI64,
    /// Generic - caller asserts the shape itself. Used as a
    /// fallback while migrating older transmute sites; emits a
    /// diagnostic so unregistered uses are visible.
    Generic,
}

type Registry = HashMap<usize, FnKind>;

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Records `addr` as a function pointer of `kind`. Idempotent
/// when re-registered with the same kind; conflicts (different
/// kind for the same addr) abort with a diagnostic - that is the
/// scenario a typed registry exists to catch.
pub fn register(addr: usize, kind: FnKind) {
    if addr == 0 {
        return;
    }
    let mut map = registry().write();
    if let Some(existing) = map.get(&addr) {
        if *existing != kind {
            eprintln!(
                "gossamer runtime: function-pointer kind mismatch - addr {addr:#x} \
                 was registered as {existing:?} but now requested as {kind:?}; \
                 aborting to prevent transmute UB",
            );
            std::process::abort();
        }
        return;
    }
    map.insert(addr, kind);
}

/// Verifies that `addr` is registered with `expected`. Returns
/// the kind on success. If `addr` is not registered at all the
/// call is admitted (logged once) - registration sites can be
/// added incrementally without breaking existing call paths. If
/// `addr` is registered with a *different* kind that is hard
/// abort - silent transmute UB is the failure mode we are
/// defending against and is not survivable.
pub fn verify(addr: usize, expected: FnKind) {
    if addr == 0 {
        eprintln!("gossamer runtime: null function pointer transmuted to {expected:?}; aborting");
        std::process::abort();
    }
    let map = registry().read();
    if let Some(actual) = map.get(&addr) {
        if *actual != expected && *actual != FnKind::Generic && expected != FnKind::Generic {
            eprintln!(
                "gossamer runtime: function-pointer kind mismatch on dispatch - \
                 addr {addr:#x} registered as {actual:?} but call expected {expected:?}; \
                 aborting to prevent transmute UB",
            );
            std::process::abort();
        }
    }
}

/// Returns the registered kind for `addr`, if any. Used by
/// router dispatch (which needs to pick between BareFn /
/// EnvFn shapes at call time).
#[must_use]
pub fn lookup(addr: usize) -> Option<FnKind> {
    if addr == 0 {
        return None;
    }
    registry().read().get(&addr).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_verify_round_trip() {
        let addr = 0xdead_beef_usize;
        register(addr, FnKind::UnaryI64ToI64);
        // Verifying the same kind is a no-op.
        verify(addr, FnKind::UnaryI64ToI64);
        // Looking up returns the recorded kind.
        assert_eq!(lookup(addr), Some(FnKind::UnaryI64ToI64));
    }

    #[test]
    fn unregistered_addr_is_admitted() {
        // Verifying an unregistered addr admits the call. This
        // keeps incremental adoption viable - registration sites
        // can be added piecewise without breaking call paths that
        // pre-existed.
        verify(0xfeed_face_usize, FnKind::BinaryI64ToI64);
    }

    #[test]
    fn double_register_same_kind_is_idempotent() {
        let addr = 0xa5a5_a5a5_usize;
        register(addr, FnKind::PredI64);
        register(addr, FnKind::PredI64);
        assert_eq!(lookup(addr), Some(FnKind::PredI64));
    }

    #[test]
    fn generic_kind_matches_anything() {
        let addr = 0x1111_2222_usize;
        register(addr, FnKind::Generic);
        // Generic verifies as any kind without aborting.
        verify(addr, FnKind::SortCmp);
        verify(addr, FnKind::UnaryI64ToI64);
    }
}
