#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::alloc::Layout;
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use std::alloc::{alloc_zeroed, dealloc};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------
// Aggregate heap allocation (plain).
// ---------------------------------------------------------------
//
// `gos_rt_gc_alloc` / `gos_rt_aggr_alloc` allocate zeroed,
// 8-byte-aligned blocks for struct / tuple / array values in
// compiled Gossamer (Cranelift + LLVM tiers). The MIR drop pass
// emits `gos_rt_aggr_free` at end-of-scope for owning aggregate
// locals, reclaiming them deterministically.
//
// Recursive heap enums are reference counted (see `c_abi::rc`);
// the raw-pointer tracing collector that formerly backstopped
// escaped aggregates was removed - it could not discover live
// roots precisely under optimised LLVM. Today an aggregate that
// escapes the drop pass's analysis (stored in a long-lived
// container, returned through an opaque chain) leaks until process
// exit rather than being unsoundly collected. Converting these
// container/string/closure types onto the RC header is the
// remaining bounded-memory work.

/// `Layout::from_size_align` rejected the size + alignment pair
/// (zero size, or rounded size exceeded `isize::MAX`). Recovered to
/// a null-pointer return / silent no-op; the runtime never panics
/// across `extern "C"`.
#[derive(Debug, Clone, Copy)]
enum GcError {
    LayoutOverflow,
}

/// Word size on the supported targets; the runtime ABI hard-codes
/// 8-byte alignment for every aggregate allocation.
const WORD_BYTES: usize = std::mem::size_of::<usize>();

/// Hard ceiling on a single aggregate allocation (1 GiB). Generous
/// enough that no real program hits it, tight enough to catch
/// corruption-induced size drift before a bad `dealloc`.
const MAX_AGGR_BYTES: usize = 1 << 30;

/// Fail-closed layout for an aggregate of `size` bytes at the
/// runtime's fixed 8-byte alignment.
fn aggregate_layout(size: usize) -> Result<Layout, GcError> {
    if size == 0 || size > MAX_AGGR_BYTES {
        return Err(GcError::LayoutOverflow);
    }
    Layout::from_size_align(size, WORD_BYTES).map_err(|_| GcError::LayoutOverflow)
}

/// Zeroed storage for an aggregate block.
///
/// The Rust global-allocator facade routes every request through mimalloc's
/// aligned entry, which pads a request by 8 to 16 bytes; a five-word struct
/// then occupies the next bin up. Plain `mi_zalloc` returns the bin the size
/// asks for and guarantees 16-byte alignment, which covers the runtime's
/// 8-byte word alignment. The sanitizer and wasm builds keep the facade,
/// where the global allocator is the system one and mixing would free across
/// allocators.
#[inline]
fn aggregate_alloc_zeroed(size: usize) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        unsafe { libmimalloc_sys::mi_zalloc(size).cast() }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let Ok(layout) = aggregate_layout(size) else {
            return std::ptr::null_mut();
        };
        unsafe { alloc_zeroed(layout) }
    }
}

/// Companion to [`aggregate_alloc_zeroed`].
#[inline]
unsafe fn aggregate_free(ptr: *mut u8, layout: Layout) {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        let _ = layout;
        unsafe { libmimalloc_sys::mi_free(ptr.cast()) };
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        unsafe { dealloc(ptr, layout) };
    }
}

/// Allocates `size` zeroed, 8-byte-aligned bytes for a user
/// aggregate. Returns null on zero/oversized size; aborts on OOM
/// (panicking across the FFI boundary into compiled code is UB).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_gc_alloc(size: u64) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        if size == 0 {
            return std::ptr::null_mut();
        }
        // Route through the active arena region so a loop body's aggregates
        // (tuples, structs, arrays) are bulk-freed at `arena_pop` and their
        // individual `gos_rt_aggr_free` becomes a no-op - the same treatment
        // Vec / String backing storage already gets. Loop-region eligibility
        // proves nothing allocated inside escapes, so this cannot dangle.
        let region = crate::c_abi::rc::region_alloc_bytes(size as usize);
        if !region.is_null() {
            return region;
        }
        let Ok(layout) = aggregate_layout(size as usize) else {
            return std::ptr::null_mut();
        };
        // SAFETY: layout validated by `aggregate_layout` (size > 0,
        // <= MAX_AGGR_BYTES, 8-byte align); the allocator is thread-safe;
        // null is handled below.
        let ptr = aggregate_alloc_zeroed(layout.size());
        if ptr.is_null() {
            eprintln!(
                "gossamer runtime: OOM in gos_rt_gc_alloc (size={}, align={}); aborting",
                layout.size(),
                layout.align()
            );
            std::process::abort();
        }
        // A region-routed block is reclaimed wholesale at `arena_pop`, so it is
        // not tracked in the per-block aggregate ledger. This is the single
        // accounting site for aggregate allocation; wrappers must not add
        // their own increment.
        if !unsafe { crate::c_abi::rc::in_region_arena(ptr) } {
            crate::c_abi::ledger::aggr_inc();
        }
        ptr
    })
}

/// Aggregate allocation entry point used by struct/tuple/array
/// construction. A distinct symbol from `gos_rt_gc_alloc` so the
/// linker's identical-code-folding cannot merge them and dead-strip
/// the wrapper user IR calls.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn gos_rt_aggr_alloc(size: u64) -> *mut u8 {
    // Only the MSVC link line enables `/OPT:ICF`, so the body-distinguishing
    // side effect is confined to that target; elsewhere `#[used]` below is
    // what keeps the symbol, and the atomic would cost every allocation.
    #[cfg(target_env = "msvc")]
    {
        static ANCHOR: AtomicUsize = AtomicUsize::new(0);
        ANCHOR.fetch_add(1, Ordering::SeqCst);
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
    gos_rt_gc_alloc(size)
}

// `#[used]` pins the distinct symbol so `--gc-sections` cannot strip
// it after ICF folds the body into `gos_rt_gc_alloc`.
#[used]
static GOS_RT_AGGR_ALLOC_KEEP: extern "C" fn(u64) -> *mut u8 = gos_rt_aggr_alloc;

/// Allocates an aggregate whose surviving handle escapes (e.g. a
/// struct stored as an i64 in a HashMap). Identical to
/// `gos_rt_aggr_alloc` today; retained as a distinct symbol for
/// existing compiled artefacts.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn gos_rt_aggr_alloc_leak(size: u64) -> *mut u8 {
    static ANCHOR: AtomicUsize = AtomicUsize::new(0);
    ANCHOR.fetch_add(1, Ordering::SeqCst);
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    ffi_entry!(std::ptr::null_mut(), {
        if size == 0 {
            return std::ptr::null_mut();
        }
        let Ok(layout) = aggregate_layout(size as usize) else {
            return std::ptr::null_mut();
        };
        // SAFETY: as in `gos_rt_gc_alloc`.
        let ptr = aggregate_alloc_zeroed(layout.size());
        if ptr.is_null() {
            eprintln!(
                "gossamer runtime: OOM in gos_rt_aggr_alloc_leak (size={}, align={}); aborting",
                layout.size(),
                layout.align()
            );
            std::process::abort();
        }
        ptr
    })
}

#[used]
static GOS_RT_AGGR_ALLOC_LEAK_KEEP: extern "C" fn(u64) -> *mut u8 = gos_rt_aggr_alloc_leak;

/// Reclaims an aggregate allocated by `gos_rt_aggr_alloc` /
/// `gos_rt_gc_alloc`. Idempotent on null. `size` must match the
/// original allocation (the MIR drop pass derives it from
/// `type_slot_count(ty) * 8`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_aggr_free(ptr: *mut u8, size: u64) {
    ffi_entry!((), {
        if ptr.is_null() || size == 0 {
            return;
        }
        // Region-allocated aggregates are reclaimed wholesale at `arena_pop`;
        // an individual free would corrupt the bump arena. No-op for them.
        if unsafe { crate::c_abi::rc::in_region_arena(ptr) } {
            return;
        }
        let Ok(layout) = aggregate_layout(size as usize) else {
            return;
        };
        crate::c_abi::ledger::aggr_dec();
        // SAFETY: `ptr` came from `aggregate_alloc_zeroed` for this exact
        // size; the drop pass guarantees a single matching free.
        unsafe { aggregate_free(ptr, layout) };
    });
}

/// Retained for ABI compatibility - the tracing collector it drove
/// is removed, so this is a no-op (the drop pass reclaims
/// aggregates deterministically; escaped ones leak until exit).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_gc_reset() {
    ffi_entry!((), {});
}

/// Retained for ABI compatibility (called after `Vec::from_raw_parts`
/// takes ownership of a `gos_rt_gc_alloc` buffer). With the tracing
/// registry removed there is nothing to deregister - no-op.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_gc_deregister(_ptr: *mut u8) {
    ffi_entry!((), {});
}

/// `std::runtime::gc_collect()` - retained as a no-op (returns 0
/// bytes reclaimed). Recursive enums are reference counted and
/// aggregates are freed deterministically, so there is no
/// stop-the-world collection to trigger; a manual collect is a
/// well-defined no-op rather than a removed API.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_gc_collect() -> u64 {
    ffi_entry!(0, { 0 })
}

/// `std::runtime` allocation-count hook - returns 0 now that the
/// tracking registry is gone. Diagnostic only.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_gc_alloc_count() -> u64 {
    ffi_entry!(0, { 0 })
}

/// Legacy arena watermark - no-op (returns the "no checkpoint"
/// value). Existing compiled artefacts may still reference it.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_arena_save() -> u64 {
    ffi_entry!(0, { 0 })
}

/// Legacy arena rewind - no-op. See `gos_rt_arena_save`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_arena_restore(_saved: u64) {
    ffi_entry!((), {});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_returns_zeroed_aligned_block() {
        let p = gos_rt_gc_alloc(24);
        assert!(!p.is_null());
        assert_eq!(p as usize % WORD_BYTES, 0);
        // Zeroed.
        for i in 0..24 {
            assert_eq!(unsafe { *p.add(i) }, 0);
        }
        gos_rt_aggr_free(p, 24);
    }

    #[test]
    fn alloc_rejects_zero_and_oversized() {
        assert!(gos_rt_gc_alloc(0).is_null());
        assert!(gos_rt_gc_alloc((MAX_AGGR_BYTES as u64) + 1).is_null());
    }

    #[test]
    fn aggr_free_is_null_safe() {
        gos_rt_aggr_free(std::ptr::null_mut(), 8);
        gos_rt_gc_deregister(std::ptr::null_mut());
        gos_rt_gc_reset();
    }

    #[test]
    fn aggregate_layout_rejects_oversized() {
        assert!(aggregate_layout(0).is_err());
        assert!(aggregate_layout(MAX_AGGR_BYTES + 1).is_err());
        assert!(aggregate_layout(64).is_ok());
    }
}
