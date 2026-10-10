#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

// wasm32 has no mimalloc backend, so it joins the
// tsan/miri/fuzzing builds in routing RC blocks through the system
// global allocator (dlmalloc) with a side size map for `dealloc`.
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};

pub(crate) mod copy_blob;
pub(crate) mod cycle;
pub(crate) mod region;

pub use copy_blob::*;
pub use cycle::*;
pub use region::*;

// ---------------------------------------------------------------
// Intrusive reference counting for compiled-tier heap objects.
// ---------------------------------------------------------------
//
// Every RC-managed heap object is laid out as `[ RcHeader | payload ]`.
// The pointer the compiled program holds points at the *payload*; the
// header sits `RC_HEADER_SIZE` bytes before it. There is no global
// allocation registry - lifetime is owned entirely by the strong
// refcount in each object's header. This replaces the raw-pointer
// tracing GC, which could not discover live roots precisely under
// optimized LLVM.
//
// retain = +1 strong; release = -1 strong, and at zero the object's
// RC-pointer children are released (iteratively) and the payload is
// destroyed. This matches the interpreter tier's `Arc`-payload
// semantics, which is the semantic oracle.
//
// Weak references (Swift-ARC model): a `Weak<T>` does not contribute to
// the strong count. The *payload* (the user's value and its child
// releases) is destroyed when `strong` hits 0; the *allocation* is freed
// only when both `strong` and `weak` hit 0, so a dangling `Weak` can
// still safely read "is the referent alive?" (`strong > 0`). See the
// hybrid memory model design (RC + cycle collector + weak refs).

/// Intrusive header prefixed to every RC-managed allocation.
///
/// The compiled program never computes this offset itself: it holds a
/// payload pointer and passes it to `gos_rt_rc_retain` / `_release` /
/// `_downgrade` / `_weak_*`, which recover the header internally. The
/// exact header size is thus a runtime-private detail.
#[repr(C)]
pub struct RcHeader {
    /// Strong reference count (low 28 bits) plus the collector flag bits.
    /// Starts at 1 on allocation.
    pub strong: u32,
    /// Weak reference count. The allocation outlives `strong == 0` whenever
    /// this is non-zero, so a `Weak` can probe liveness without reading freed
    /// memory. `AtomicU8`: concurrent downgrade/upgrade across goroutines is
    /// safe; same size and layout as `u8` so the 8-byte header is preserved.
    pub weak: AtomicU8,
    /// Enum discriminant. Lives in the header (codegen reads/writes the
    /// byte at `payload - 3`) so the payload holds only the variant's
    /// fields: a `Node(i64, Box, Box)` is 8 + 24 = 32 bytes and a
    /// two-pointer `Node(Box, Box)` is 8 + 16 = 24. Enums are capped at
    /// 256 variants by the type checker. Zero (and unread) for
    /// non-enum RC objects.
    pub disc: u8,
    /// Interned id of the child-layout descriptor blob (see
    /// `meta_intern` / `meta_of`); 0 for leaf objects with no
    /// RC-pointer children. The allocation size is not recorded at all -
    /// blocks are freed with `mi_free`, which needs only the base
    /// pointer.
    pub meta_id: u16,
}

/// 8-byte alignment is hard-coded across the runtime ABI; all payload
/// fields are word-sized and word-aligned.
pub const RC_ALIGN: usize = 8;

/// Size of [`RcHeader`], rounded to the runtime alignment. The payload
/// begins this many bytes after the allocation base.
pub const RC_HEADER_SIZE: usize = std::mem::size_of::<RcHeader>();

// The header must stay 8 bytes: every heap object pays it, so growth is a
// direct per-object RAM regression. The weak/cycle/meta fields are packed
// into the existing 8 bytes (see the field docs), never added on top.
const _: () = assert!(RC_HEADER_SIZE == 8, "RcHeader must remain 8 bytes");

// ---------------------------------------------------------------
// Type-meta blob format (a flat, self-describing `[i64]`).
// ---------------------------------------------------------------
//
// Codegen emits one such blob per RC-managed user ADT as a single
// contiguous module constant (trivial to lower in both LLVM and
// Cranelift, unlike a nested pointer-laden descriptor). The header's
// `meta` points at word 0.
//
//   [0] kind            - RC_KIND_*
//   [1] variant_count V
//   then V variant records, each variable-length:
//       disc            - discriminant value this record describes
//       child_count C   - number of RC-pointer child words
//       off_0 .. off_C  - payload WORD indices (byte offset / 8) holding
//                         RC-managed child pointers to release
//
// For an enum, `release_children` reads the live discriminant from
// payload word 0 and releases the children of the matching record. For
// a struct/tuple there is a single record and the discriminant is
// ignored.

// `meta[0]` kind discriminants live in `gossamer-abi` (the single
// source shared with the MIR lowerer that emits these blobs). Only
// `Enum` and `Struct` carry child layouts today; the heap builtins
// (string/vec/map/closure) are wired in a later phase.
pub use gossamer_abi::rc::{
    RC_KIND_CLOSURE, RC_KIND_ENUM, RC_KIND_MAP, RC_KIND_STRING, RC_KIND_STRUCT,
    RC_KIND_STRUCT_GUARDED, RC_KIND_VEC,
};

/// Count of live RC objects (allocated minus freed). Two relaxed atomic
/// RMWs per object lifecycle are measurable on tree workloads (~134M
/// increments for a binary-trees run), so production counting is gated:
/// always on in this crate's test build (the unit tests assert on it),
/// otherwise only when `GOS_RC_DEBUG` is set (the flag that also prints
/// `RC_LIVE_AT_EXIT`).
static RC_LIVE: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static RC_LIVE_TEST_ISOLATION: (
    std::sync::Mutex<Option<std::thread::ThreadId>>,
    std::sync::Condvar,
) = (std::sync::Mutex::new(None), std::sync::Condvar::new());

/// `GOS_RC_DEBUG` as read once: 0 not yet read, 1 unset, 2 set. Every RC
/// allocation and free consults it, so the settled answer is one load.
#[cfg(not(test))]
static RC_LIVE_ENABLED: AtomicU8 = AtomicU8::new(0);

#[inline]
fn rc_live_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        match RC_LIVE_ENABLED.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => rc_live_enabled_slow(),
        }
    }
}

#[cfg(not(test))]
#[cold]
#[inline(never)]
fn rc_live_enabled_slow() -> bool {
    let enabled = std::env::var_os("GOS_RC_DEBUG").is_some();
    RC_LIVE_ENABLED.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
    enabled
}

/// Under `GOS_RC_DEBUG`, reports the RC-managed objects still alive as the
/// process exits. Called once, after the goroutines have drained, so an
/// object a finished goroutine was still giving back is not counted.
pub fn report_live_at_exit() {
    if std::env::var_os("GOS_RC_DEBUG").is_none() {
        return;
    }
    let live = rc_live_count();
    let shared = rc_shared_live_count();
    let reused = rc_reuse_count();
    eprintln!("RC_LIVE_AT_EXIT={live} shared_live={shared} reused={reused}");
    if live > 0 && shared > 0 {
        // Cross-goroutine objects are excluded from the per-thread cycle
        // collector, so a shared reference cycle leaks. This is the only
        // leak class the collector cannot reach; break a back-edge with
        // `Weak` to fix it.
        eprintln!(
            "RC_HINT: {shared} live cross-goroutine object(s) at exit; a shared \
             reference cycle is not collected - break a back-edge with Weak<T>"
        );
    }
}

/// Number of RC-managed objects currently alive. Diagnostic hook;
/// meaningful only when counting is enabled (tests / `GOS_RC_DEBUG`).
pub fn rc_live_count() -> usize {
    RC_LIVE.load(Ordering::Relaxed)
}

#[cfg(test)]
fn rc_live_mutation_guard() -> std::sync::MutexGuard<'static, Option<std::thread::ThreadId>> {
    let current = std::thread::current().id();
    let (lock, cvar) = &RC_LIVE_TEST_ISOLATION;
    let mut owner = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while owner.is_some_and(|id| id != current) {
        owner = cvar
            .wait(owner)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    owner
}

#[cfg(test)]
struct RcLiveCountGuard;

#[cfg(test)]
impl Drop for RcLiveCountGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &RC_LIVE_TEST_ISOLATION;
        let mut owner = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *owner = None;
        cvar.notify_all();
    }
}

#[cfg(test)]
fn rc_live_count_guard() -> RcLiveCountGuard {
    let current = std::thread::current().id();
    let (lock, cvar) = &RC_LIVE_TEST_ISOLATION;
    let mut owner = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while owner.is_some() {
        owner = cvar
            .wait(owner)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    *owner = Some(current);
    RcLiveCountGuard
}

#[inline]
fn rc_live_inc() {
    if rc_live_enabled() {
        #[cfg(test)]
        let _guard = rc_live_mutation_guard();
        RC_LIVE.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn rc_live_dec() {
    if rc_live_enabled() {
        #[cfg(test)]
        let _guard = rc_live_mutation_guard();
        RC_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Live count of RC objects that have escaped to another goroutine
/// (`SHARED_BIT` set). Shared objects are excluded from the per-thread cycle
/// collector, so a shared object that is part of a reference cycle leaks for
/// the process lifetime. A non-zero value alongside a non-zero
/// `RC_LIVE_AT_EXIT` is the signature of a cross-goroutine cycle leak - the one
/// leak class `Weak` (not the collector) must break. Counted only when
/// diagnostics are enabled (tests / `GOS_RC_DEBUG`).
static RC_SHARED_LIVE: AtomicUsize = AtomicUsize::new(0);

/// Number of live cross-goroutine (shared) RC objects. Diagnostic hook;
/// meaningful only when counting is enabled.
pub fn rc_shared_live_count() -> usize {
    RC_SHARED_LIVE.load(Ordering::Relaxed)
}

#[inline]
fn rc_shared_inc() {
    if rc_live_enabled() {
        RC_SHARED_LIVE.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn rc_shared_dec() {
    if rc_live_enabled() {
        RC_SHARED_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Count of Perceus reuse hits: a constructor that recycled a dropped block in
/// place instead of allocating. Diagnostic / test hook; counted only when
/// diagnostics are enabled (tests / `GOS_RC_DEBUG`).
static RC_REUSE_HITS: AtomicUsize = AtomicUsize::new(0);

/// Number of in-place block reuses performed. Test/diagnostic hook.
pub fn rc_reuse_count() -> usize {
    RC_REUSE_HITS.load(Ordering::Relaxed)
}

#[inline]
fn rc_reuse_inc() {
    if rc_live_enabled() {
        RC_REUSE_HITS.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------
// Cycle-collector header bits (synchronous trial deletion).
// ---------------------------------------------------------------
//
// Reference counting cannot reclaim cycles (`A -> B -> A` never reaches
// count 0). A synchronous Bacon-Rajan trial-deletion collector reclaims
// cyclic RC garbage by tracing the object graph from a buffer of
// *candidate roots* - objects whose strong count was decremented to a
// non-zero value (the only objects that can start a cycle). It needs no
// stack scanning and no compiler root map, so it is sound under `-O3` by
// construction (it never inspects a register or spill slot), and it stays
// a no-op for the acyclic 99%: acyclic objects free immediately at count
// 0 exactly as before, and the collector runs only when the candidate
// buffer crosses a threshold.
//
// The four collector flags live in the high bits of `strong`, leaving the
// low 28 bits for the count (268M live refs is unreachable). The hot
// retain/release paths mask the count portion; the flag bits are touched
// only on the cold release-to-non-zero path and during collection, so the
// acyclic fast path pays only a mask.

/// Low 27 bits of `strong`: the actual strong reference count (134M
/// live refs is unreachable). Bit 27 is [`SHARED_BIT`]; bits 28-31 are
/// the collector flags.
const STRONG_COUNT_MASK: u32 = 0x07FF_FFFF;

/// Bit 27: the object has escaped to another goroutine (sent on a
/// channel, captured by a spawned goroutine, or passed to `go f(...)`).
/// Its strong count is then mutated with **atomic** retain/release so
/// concurrent workers can't tear the count (lost decrement → UAF /
/// double-free). Shared objects are excluded from the per-thread cycle
/// collector - their cycles leak, exactly like Rust's `Arc` (break with
/// weak refs). Set transitively over the reachable RC subgraph at the
/// escape point by [`gos_rt_rc_mark_shared`], before the value is
/// published, and never cleared. Because shared objects skip the
/// collector, the non-atomic accessors (`color_of`, `set_strong_count`,
/// …) only ever run on thread-local (non-shared) objects, so they need
/// no atomics.
const SHARED_BIT: u32 = 1 << 27;

/// Pinned strong count for process-immortal objects (unit-variant
/// singletons). Retain and release skip the count entirely: inside an
/// arena the balancing releases never run (bulk free, no walk), so a
/// counted singleton would grow monotonically and overflow the 28-bit
/// field into the collector flag bits on big workloads.
const STRONG_IMMORTAL: u32 = STRONG_COUNT_MASK;
/// Bit 31: the object sits in the cycle-collector candidate buffer.
const BUFFERED_BIT: u32 = 1 << 31;
/// Bits 28-29: trial-deletion color.
const COLOR_SHIFT: u32 = 28;
const COLOR_MASK: u32 = 0b11 << COLOR_SHIFT;
/// Bit 30: the object was bump-allocated inside an arena region. Its
/// lifetime is the region's, so retain/release are no-ops and it is freed
/// wholesale (no per-node teardown walk) when the region is popped. This is
/// the bit that lets a `region { … }` block sidestep RC's per-node
/// reclamation cost on short-lived allocation churn.
const REGION_BIT: u32 = 1 << 30;

/// One-shot claim flag for reclaiming a DEAD shared block (bit 28, aliasing
/// the low collector-color bit, which a shared object never uses: shared
/// objects are excluded from the per-thread collector and any stale color is
/// cleared at the share transition). The final strong release and the final
/// weak release can race on different goroutines with both counts reading
/// zero; whoever CAS-sets this bit owns the free, so the block is reclaimed
/// exactly once.
const SHARED_RECLAIM_BIT: u32 = 1 << COLOR_SHIFT;

// Only the test asserts read this now - the hot retain/release paths
// check `REGION_BIT` inline off their single atomic `strong` load
// (`inc_strong` / `dec_strong`) to avoid a second read.
#[cfg(test)]
#[inline]
unsafe fn is_region(h: *const RcHeader) -> bool {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    (unsafe { (*h).strong }) & REGION_BIT != 0
}

/// In active use, or already freed. The default (zeroed) color.
const COLOR_BLACK: u32 = 0;
/// Possible member of a garbage cycle (being traced).
const COLOR_GRAY: u32 = 1;
/// Confirmed member of a garbage cycle (to be collected).
const COLOR_WHITE: u32 = 2;
/// Possible root of a garbage cycle (decremented to non-zero).
const COLOR_PURPLE: u32 = 3;

#[inline]
unsafe fn strong_count(h: *const RcHeader) -> u32 {
    // Atomic (relaxed) load: identical codegen to a plain load, but safe to
    // call on a shared object whose count other goroutines mutate atomically.
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    (unsafe { load_strong(h) }) & STRONG_COUNT_MASK
}

/// Overwrite the count portion of `strong`, preserving the flag bits.
#[inline]
unsafe fn set_strong_count(h: *mut RcHeader, count: u32) {
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    let cur = unsafe { (*h).strong };
    // Immortal pin (unit-variant singletons): the count is never
    // mutated - not by retain/release, not by the cycle collector's
    // trial deletion, not by the release walk's child decrements.
    if cur & STRONG_COUNT_MASK == STRONG_IMMORTAL {
        return;
    }
    let flags = cur & !STRONG_COUNT_MASK;
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    unsafe { (*h).strong = flags | (count & STRONG_COUNT_MASK) };
}

// ---------------------------------------------------------------
// Escape-aware (atomic-on-share) strong-count operations.
// ---------------------------------------------------------------
//
// An RC object shared across goroutines (see `SHARED_BIT`) must have
// its strong count mutated atomically, or two workers releasing it
// concurrently tear the count and double-free / leak. Thread-local
// objects keep the cheap non-atomic path. Every read/write below that
// can run on a *shared* object goes through these helpers; the
// non-atomic accessors only ever see thread-local objects (shared ones
// are excluded from the cycle collector, the only other writer).

/// Relaxed atomic load of `strong` - safe to call on shared objects.
#[inline]
unsafe fn load_strong(h: *const RcHeader) -> u32 {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller). The count word is
    // only ever accessed atomically once shared, and an `AtomicU32` has the layout of the `u32`
    // it views.
    let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of!((*h).strong).cast_mut()) };
    a.load(Ordering::Relaxed)
}

/// Whether the object has escaped to another goroutine.
#[inline]
unsafe fn is_shared(h: *const RcHeader) -> bool {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    (unsafe { load_strong(h) }) & SHARED_BIT != 0
}

/// Escape-aware strong increment (the `retain` core). Atomic for shared
/// objects, a plain RMW otherwise. Region / immortal objects untouched.
#[inline]
unsafe fn inc_strong(h: *mut RcHeader) {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    let s = unsafe { load_strong(h) };
    if s & REGION_BIT != 0 || s & STRONG_COUNT_MASK == STRONG_IMMORTAL {
        return;
    }
    if s & SHARED_BIT != 0 {
        // Count is the low 27 bits; +1 cannot reach SHARED_BIT before
        // 134M live refs (unreachable). Relaxed: a retain needs
        // atomicity, not ordering.
        // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller). A shared node's
        // count is only ever accessed atomically.
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        a.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let bumped = (s & STRONG_COUNT_MASK)
        .saturating_add(1)
        .min(STRONG_COUNT_MASK);
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    unsafe { (*h).strong = (s & BUFFERED_BIT) | bumped };
}

/// Result of [`dec_strong`].
struct DecOutcome {
    /// Strong count after the decrement.
    next: u32,
    /// The object had escaped to another goroutine.
    shared: bool,
    /// Region / immortal object - no accounting happened, never reclaim.
    skip: bool,
}

/// Escape-aware strong decrement (the `release` core). Atomic (Release)
/// for shared objects so the worker that drops the last reference
/// synchronises with the others before reclaiming; plain RMW otherwise.
#[inline]
unsafe fn dec_strong(h: *mut RcHeader) -> DecOutcome {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    let s = unsafe { load_strong(h) };
    if s & REGION_BIT != 0 || s & STRONG_COUNT_MASK == STRONG_IMMORTAL {
        return DecOutcome {
            next: 1,
            shared: false,
            skip: true,
        };
    }
    // Under correct accounting a normal object's strong count never drops
    // below zero. An underflow here means a double-free or an untagged /
    // foreign pointer reaching RC dispatch (the `os::args()` class of bug):
    // surface it loudly in debug builds rather than corrupting the heap. The
    // check is debug-only, so release builds keep the branch-free fast path.
    debug_assert!(
        s & STRONG_COUNT_MASK > 0,
        "gos RC underflow: release of an object whose strong count is already 0 \
         (double-free, or an untagged/foreign pointer reached RC dispatch)"
    );
    if s & SHARED_BIT != 0 {
        // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller). A shared node's
        // count is only ever accessed atomically.
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        let prev = a.fetch_sub(1, Ordering::Release);
        return DecOutcome {
            next: (prev & STRONG_COUNT_MASK).saturating_sub(1),
            shared: true,
            skip: false,
        };
    }
    let next = (s & STRONG_COUNT_MASK).saturating_sub(1);
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    unsafe { (*h).strong = (s & !STRONG_COUNT_MASK) | (next & STRONG_COUNT_MASK) };
    DecOutcome {
        next,
        shared: false,
        skip: false,
    }
}

/// The header byte value meaning a block's weak count lives in
/// [`WEAK_OVERFLOW_COUNTS`] rather than in the byte.
const WEAK_OVERFLOW: u8 = u8::MAX;

/// Exact weak counts of the blocks observed by more weak references than the
/// header byte holds, keyed by header address. A block has an entry exactly
/// while its byte reads [`WEAK_OVERFLOW`]; both change together under this
/// lock, so the common case never touches it.
static WEAK_OVERFLOW_COUNTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<usize, u32>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Adds one weak reference to the block whose header is `h`.
#[inline]
unsafe fn inc_weak(h: *const RcHeader) {
    // SAFETY: `h` is the header of a live block (this `unsafe fn`'s caller).
    let weak = unsafe { &(*h).weak };
    loop {
        let w = weak.load(Ordering::Relaxed);
        if w < WEAK_OVERFLOW - 1 {
            if weak
                .compare_exchange_weak(w, w + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        let mut counts = WEAK_OVERFLOW_COUNTS.lock();
        match weak.load(Ordering::Relaxed) {
            WEAK_OVERFLOW => {
                if let Some(count) = counts.get_mut(&(h as usize)) {
                    *count += 1;
                    return;
                }
            }
            w if w == WEAK_OVERFLOW - 1
                && weak
                    .compare_exchange(w, WEAK_OVERFLOW, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok() =>
            {
                counts.insert(h as usize, u32::from(WEAK_OVERFLOW));
                return;
            }
            _ => {}
        }
    }
}

/// Removes one weak reference from the block whose header is `h`, answering
/// the count before the removal, so the caller reclaims the block at the
/// 1 -> 0 edge.
#[inline]
unsafe fn dec_weak(h: *const RcHeader) -> u32 {
    // SAFETY: `h` is the header of a live block (this `unsafe fn`'s caller).
    let weak = unsafe { &(*h).weak };
    loop {
        let w = weak.load(Ordering::Relaxed);
        if w == 0 {
            return 0;
        }
        if w < WEAK_OVERFLOW {
            if weak
                .compare_exchange_weak(w, w - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return u32::from(w);
            }
            continue;
        }
        let mut counts = WEAK_OVERFLOW_COUNTS.lock();
        if weak.load(Ordering::Relaxed) != WEAK_OVERFLOW {
            continue;
        }
        let Some(count) = counts.get_mut(&(h as usize)) else {
            continue;
        };
        let before = *count;
        *count -= 1;
        if *count == u32::from(WEAK_OVERFLOW - 1) {
            counts.remove(&(h as usize));
            weak.store(WEAK_OVERFLOW - 1, Ordering::Relaxed);
        }
        return before;
    }
}

/// Marks `payload` and its reachable RC subgraph as shared (escaped to
/// another goroutine), so subsequent retains/releases use atomics and
/// the cycle collector leaves them alone. Idempotent and cycle-safe
/// (stops at already-shared nodes). Called at escape points on the
/// owning thread *before* the value is published, so the walk here
/// races with no one.
unsafe fn mark_shared(payload: *mut u8) {
    let base = untag_rc(payload);
    if base.is_null() || in_region_arena(base) {
        return;
    }
    // SAFETY: `base` is non-null and outside the region arena (checked above), a candidate the
    // string probe accepts.
    if unsafe { crate::c_abi::string::is_gos_string(base.cast()) } {
        // A shared string switches to atomic refcounting (it has no RC
        // children to walk), so its concurrent clone/drop cannot tear.
        // SAFETY: `base` is a live string body (the probe above).
        unsafe { crate::c_abi::string::gos_rt_str_mark_shared(base.cast()) };
        return;
    }
    let mut work: Vec<*mut u8> = vec![base];
    while let Some(p) = work.pop() {
        if p.is_null() || in_region_arena(p) {
            continue;
        }
        // SAFETY: `p` is non-null and outside the region arena (checked above), a candidate the
        // string probe accepts.
        if unsafe { crate::c_abi::string::is_gos_string(p.cast()) } {
            // A child string switches to atomic refcounting; it has no RC
            // children of its own, so there is nothing further to walk.
            // SAFETY: `p` is a live string body (the probe above).
            unsafe { crate::c_abi::string::gos_rt_str_mark_shared(p.cast()) };
            continue;
        }
        // SAFETY: `p` is a live counted node reached from the root's children.
        let h = unsafe { header_ptr(p) };
        // SAFETY: `h` is the header of the live node `p`.
        let s = unsafe { load_strong(h) };
        // A node no strong reference holds - one a `Weak` keeps allocated -
        // has already given its children back, so there is nothing to share.
        if s & SHARED_BIT != 0
            || s & REGION_BIT != 0
            || s & STRONG_COUNT_MASK == STRONG_IMMORTAL
            || s & STRONG_COUNT_MASK == 0
        {
            continue;
        }
        // SAFETY: `h` is the header of the live node `p`, whose count is accessed atomically from
        // here on.
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        // Clear any stale collector color at the thread-local -> shared
        // transition: bit 28 of the color field doubles as the shared
        // reclaim-claim flag (`SHARED_RECLAIM_BIT`), which must start clear.
        // The walk runs pre-publish on the owning thread, so no concurrent
        // writer exists yet.
        let prev = match a.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
            Some((s & !COLOR_MASK) | SHARED_BIT)
        }) {
            Ok(prev) | Err(prev) => prev,
        };
        if prev & SHARED_BIT == 0 {
            // This object just transitioned thread-local -> shared.
            rc_shared_inc();
        }
        // SAFETY: `p` is a live counted node whose children this walk visits.
        unsafe {
            visit_children_raw(p, |c| work.push(c));
            // A container child is reached from other threads through this
            // node, so the counted values it holds switch to atomic counting
            // along with the node itself.
            visit_entries(p, |kind, child| {
                if kind != gossamer_abi::rc::RC_CHILD_RC {
                    mark_shared_child(kind, child);
                }
            });
        }
    }
}

/// Marks one child of a node or an aggregate as shared, by the kind its layout
/// entry names.
unsafe fn mark_shared_child(kind: i64, child: *mut u8) {
    match kind {
        // SAFETY: a child of kind `RC_CHILD_RC` is a live node or null.
        gossamer_abi::rc::RC_CHILD_RC => unsafe { mark_shared(child) },
        // SAFETY: a child of kind `RC_CHILD_VEC` is a live `Vec` or null.
        gossamer_abi::rc::RC_CHILD_VEC => unsafe {
            crate::c_abi::vec::gos_rt_vec_mark_shared(child.cast());
        },
        // SAFETY: a child of kind `RC_CHILD_MAP` is a live `Map` or null.
        gossamer_abi::rc::RC_CHILD_MAP => unsafe {
            crate::c_abi::map::gos_rt_map_mark_shared(child.cast());
        },
        // SAFETY: a child of kind `RC_CHILD_SET` is a live `Set` or null.
        gossamer_abi::rc::RC_CHILD_SET => unsafe {
            crate::c_abi::set::gos_rt_set_mark_shared(child.cast());
        },
        // SAFETY: a child of kind `RC_CHILD_DEQUE` is a live deque or null.
        gossamer_abi::rc::RC_CHILD_DEQUE => unsafe {
            crate::c_abi::deque::deque_mark_shared(child.cast());
        },
        // SAFETY: a child of kind `RC_CHILD_HEAP` is a live heap vec or null.
        gossamer_abi::rc::RC_CHILD_HEAP => unsafe {
            crate::c_abi::vec::gos_rt_vec_mark_shared(child.cast());
        },
        // A dead target is skipped by the walk, so only a live one turns atomic.
        // SAFETY: a child of kind `RC_CHILD_WEAK` is a block its weak share keeps allocated.
        gossamer_abi::rc::RC_CHILD_WEAK => unsafe { mark_shared(child) },
        _ => {}
    }
}

/// Takes a weak share of every `Weak` child `payload`'s meta names, for a
/// copy whose words were taken from storage keeping its own.
pub(crate) unsafe fn retain_weak_children(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entries(payload, |kind, child| {
            if kind == gossamer_abi::rc::RC_CHILD_WEAK {
                gos_rt_rc_weak_retain(child);
            }
        });
    }
}

/// The weak children `payload`'s meta names, for a walk that gives them back
/// after the blocks it frees are gone.
pub(crate) unsafe fn weak_children_of(payload: *mut u8, out: &mut Vec<*mut u8>) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entries(payload, |kind, child| {
            if kind == gossamer_abi::rc::RC_CHILD_WEAK {
                out.push(child);
            }
        });
    }
}

/// Marks every counted field of a by-value aggregate as shared before the
/// aggregate is published to another goroutine. `base` is the aggregate's own
/// storage and `meta` its `RC_KIND_STRUCT` child-word layout; the aggregate is
/// not a node, so the walk reads the layout it is handed rather than a header.
///
/// # Safety
/// `base` must name the words `meta` describes, or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_mark_shared_children(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    // SAFETY: `base` and `meta` are this shim's arguments, non-null (checked above), with `base`
    // laid out as `meta` describes (C-ABI contract).
    unsafe {
        visit_layout_entry_slots(base, meta, 0, |kind, _slot, child| {
            mark_shared_child(kind, child);
        });
    };
}

/// Marks the reachable RC subgraph of `payload` as shared across
/// goroutines. Called from the channel-send / goroutine-spawn lowering
/// so escaped objects switch to atomic reference counting. The codegen
/// gates the call on the static type (RC-managed only), so scalars
/// never reach here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_mark_shared(payload: *mut u8) {
    // SAFETY: `payload` is this shim's argument, a live counted value or null (C-ABI contract),
    // which `mark_shared` accepts.
    unsafe { mark_shared(payload) };
}

#[inline]
unsafe fn color_of(h: *const RcHeader) -> u32 {
    // Atomic (relaxed) load for the same reason as `strong_count`.
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    ((unsafe { load_strong(h) }) & COLOR_MASK) >> COLOR_SHIFT
}

#[inline]
unsafe fn set_color(h: *mut RcHeader, color: u32) {
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    let rest = unsafe { (*h).strong } & !COLOR_MASK;
    // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses during
    // the call (this `unsafe fn`'s caller).
    unsafe { (*h).strong = rest | (color << COLOR_SHIFT) };
}

#[inline]
unsafe fn is_buffered(h: *const RcHeader) -> bool {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    (unsafe { load_strong(h) }) & BUFFERED_BIT != 0
}

#[inline]
unsafe fn set_buffered(h: *mut RcHeader, on: bool) {
    if on {
        // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses
        // during the call (this `unsafe fn`'s caller).
        unsafe { (*h).strong |= BUFFERED_BIT };
    } else {
        // SAFETY: `h` is the header of a live, thread-local node that nothing else accesses
        // during the call (this `unsafe fn`'s caller).
        unsafe { (*h).strong &= !BUFFERED_BIT };
    }
}

/// Whether `payload`'s *live* shape holds at least one RC-pointer child -
/// the precondition for being a cycle member. An object with no current
/// children (a leaf, an empty-variant enum, a struct of scalars) can never
/// start a cycle, so it is never buffered as a candidate and frees
/// immediately at count 0 like any acyclic object.
#[inline]
unsafe fn has_rc_children(payload: *mut u8) -> bool {
    let mut found = false;
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe { visit_rc_children(payload, |_| found = true) };
    found
}

/// Base candidate-buffer size that arms an automatic collection. Tuned so
/// the collector runs rarely and only when cyclic garbage is plausibly
/// accumulating; acyclic workloads never fill it (nothing is buffered). The
/// effective threshold is adaptive (see `COLLECT_THRESHOLD`): it grows when
/// collections keep finding little garbage so a churn of live DAGs is not
/// rescanned on every few-thousand decrements.
const COLLECT_THRESHOLD_BASE: usize = 10_000;

/// Cap the adaptive threshold can grow to. A workload that buffers many
/// surviving-decrement objects which are nearly all live (DAGs, shared
/// subtrees) backs off to this before scanning again.
const COLLECT_THRESHOLD_MAX: usize = 160_000;

/// Maximum candidate roots an *automatic* collection processes in one slice,
/// so a single auto-collect never traverses an unbounded number of
/// independent candidate subgraphs inline on the mutator. Leftover candidates
/// stay buffered and are handled by the next slice. (A single candidate whose
/// own cyclic subgraph is large is still traced in full within its slice -
/// bounding one trial-deletion trace mid-flight would require a concurrent
/// collector, which the zero-pause policy rules out.) Explicit
/// `runtime::collect_cycles()` ignores this and fully drains.
const COLLECT_SLICE_ROOTS: usize = 2_048;

thread_local! {
    /// Candidate roots: objects whose strong count was decremented to a
    /// non-zero value. Deduplicated by the `BUFFERED_BIT`. Thread-local so
    /// buffering needs no lock on the release hot path; the collector runs
    /// on the same thread over its own candidates.
    static ROOTS: std::cell::RefCell<RootBuffer> = std::cell::RefCell::new(RootBuffer::default());
    /// Adaptive arming threshold for automatic collection on this thread.
    /// Doubles (capped at `COLLECT_THRESHOLD_MAX`) after a slice that
    /// reclaimed little, and snaps back to `COLLECT_THRESHOLD_BASE` after a
    /// productive slice. This is what stops a live-DAG workload from paying a
    /// scan every `COLLECT_THRESHOLD_BASE` surviving decrements.
    static COLLECT_THRESHOLD: std::cell::Cell<usize> =
        const { std::cell::Cell::new(COLLECT_THRESHOLD_BASE) };
}

#[derive(Default)]
struct RootBuffer {
    items: Vec<*mut u8>,
    /// Keyed by object address, so the hash is over a pointer the allocator
    /// already spread out. A live-DAG workload can hold this at the
    /// `COLLECT_THRESHOLD_MAX` bound, where the per-entry cost of a
    /// SipHash table is what the buffer's footprint is made of.
    positions: rustc_hash::FxHashMap<usize, usize>,
}

impl RootBuffer {
    fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn push(&mut self, payload: *mut u8) {
        let key = payload as usize;
        if self.positions.contains_key(&key) {
            return;
        }
        self.positions.insert(key, self.items.len());
        self.items.push(payload);
    }

    fn remove(&mut self, payload: *mut u8) -> bool {
        let key = payload as usize;
        let Some(index) = self.positions.remove(&key) else {
            return false;
        };
        self.items.swap_remove(index);
        if let Some(moved) = self.items.get(index) {
            self.positions.insert(*moved as usize, index);
        }
        true
    }

    fn take_tail(&mut self, count: usize) -> Vec<*mut u8> {
        let start = self.items.len().saturating_sub(count);
        let tail = self.items.split_off(start);
        for payload in &tail {
            self.positions.remove(&(*payload as usize));
        }
        tail
    }

    fn take_all(&mut self) -> Vec<*mut u8> {
        self.positions.clear();
        std::mem::take(&mut self.items)
    }

    fn remove_all(&mut self, removed: &std::collections::HashSet<usize>) {
        self.items
            .retain(|payload| !removed.contains(&(*payload as usize)));
        self.positions.clear();
        self.positions.extend(
            self.items
                .iter()
                .enumerate()
                .map(|(index, payload)| (*payload as usize, index)),
        );
    }
}

/// Total cyclic objects reclaimed by the collector. Diagnostic / test hook.
static CYCLES_FREED: AtomicUsize = AtomicUsize::new(0);

/// Number of cyclic RC objects reclaimed so far. Test/diagnostic hook.
pub fn rc_cycles_freed() -> usize {
    CYCLES_FREED.load(Ordering::Relaxed)
}

/// Record `payload` as a possible cycle root: strong count was just
/// decremented but is still non-zero, so it may be part of a cycle whose
/// only remaining references are internal. Buffered once (deduplicated by
/// the header bit); the buffer auto-collects when it crosses the threshold.
unsafe fn possible_root(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let h = unsafe { header_ptr(payload) };
    // Objects that have escaped to another goroutine are excluded from
    // the per-thread cycle collector - touching their flag bits here is a
    // non-atomic write that would race a concurrent worker's atomic
    // retain/release, and even reading their payload slots races the
    // owning goroutine's mutations. Their cycles leak (like `Arc`);
    // break with weak refs.
    // SAFETY: `h` is the header of the live node `payload`.
    if unsafe { is_shared(h) } {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    if !unsafe { has_rc_children(payload) } {
        return;
    }
    // Color purple marks a candidate; skip if already a tracked root.
    // SAFETY: `h` is the header of the live node `payload`.
    if unsafe { color_of(h) } == COLOR_PURPLE {
        return;
    }
    // SAFETY: `h` is the header of the live, thread-local node `payload` (not shared, checked
    // above).
    unsafe { set_color(h, COLOR_PURPLE) };
    // SAFETY: `h` is the header of the live node `payload`.
    if unsafe { is_buffered(h) } {
        return;
    }
    // SAFETY: `h` is the header of the live, thread-local node `payload`.
    unsafe { set_buffered(h, true) };
    let over = ROOTS.with(|r| {
        let mut roots = r.borrow_mut();
        roots.push(payload);
        roots.len() >= COLLECT_THRESHOLD.with(std::cell::Cell::get)
    });
    if over {
        // Automatic collection processes a bounded slice and adapts the
        // threshold; it never drains the whole buffer in one inline pass.
        // SAFETY: collection runs on the owning thread, whose root buffer keeps every candidate
        // alive.
        unsafe { collect_cycles_budgeted(Some(COLLECT_SLICE_ROOTS)) };
    }
}

/// Reclaim a zero-count object immediately when its buffered candidate still
/// lives in the thread-local queue. Indexed removal makes this O(1), which is
/// essential for long acyclic lists whose nodes briefly survive alias
/// releases. A candidate already extracted into an active collector slice is
/// not in the queue, so its pin remains intact until that slice finishes.
unsafe fn try_reclaim_zero(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    if unsafe { is_buffered(h) } {
        let removed = ROOTS.with(|roots| roots.borrow_mut().remove(payload));
        if removed {
            // SAFETY: `h` is the header of the live, thread-local node `payload`.
            unsafe { set_buffered(h, false) };
        }
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe { try_reclaim(payload) };
}

// ---------------------------------------------------------------
// Size-class free-list (recycling slab) allocator.
// ---------------------------------------------------------------
//
// RC objects are small and overwhelmingly uniform-sized (a tree's nodes,
// a graph's adjacency cells), allocated and freed in tight churn. Routing
// every node through libc `malloc`/`free` is the dominant cost on
// allocation-heavy workloads; Rust's reference implementations of the
// same programs use a bump arena. This caches freed blocks per size class
// and hands them straight back on the next allocation of that class - a
// pop/push instead of a malloc/free round-trip.
//
// Blocks are recycled by *byte size* (rounded to `CLASS_STEP`), never by
// type: a freed block of N bytes can back any later N-byte allocation
// regardless of which ADT it held, because the allocator only manages raw
// storage (the header + payload are fully rewritten on reuse).
//
// Soundness: RC objects migrate across threads (channels), so a freed
// block may be returned on a different thread than it was taken from. The
// free-lists are therefore global, sharded to bound lock contention. Each
// shard holds raw addresses as `usize` (Send + Sync); the per-class cap
// returns surplus blocks to the OS so a one-time large burst cannot pin
// memory forever.

/// Rounds `total` bytes to its size class. Returns `(rounded_bytes,
/// Some(class_index))` when poolable, or `(rounded, None)` for oversized
/// allocations that bypass the pool. The rounded byte size equals
/// `units * SIZE_UNIT`.
/// Allocate `total` zeroed bytes for an RC block. Calls mimalloc's plain
/// `mi_zalloc` directly instead of going through the Rust global-allocator
/// facade: the facade routes every allocation through
/// `mi_zalloc_aligned(size, align)`, and mimalloc v3's aligned entry pads
/// the request by 8-16 bytes - a 48-byte RC node then occupies a 64-byte
/// block, a flat ~25% RAM tax on every RC object. Plain `mi_zalloc`
/// returns exactly the requested bin and guarantees 16-byte alignment,
/// which covers `RC_ALIGN`. Under ThreadSanitizer the global allocator is
/// the system one, so the facade is kept (mixing would free across
/// allocators).
#[inline]
fn rc_block_alloc_zeroed(total: usize) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // SAFETY: `mi_zalloc` accepts any size and answers null on failure.
        unsafe { libmimalloc_sys::mi_zalloc(total).cast() }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let Ok(layout) = Layout::from_size_align(total, RC_ALIGN) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `layout` has the non-zero size of a header plus payload.
        let base = unsafe { alloc_zeroed(layout) };
        if !base.is_null() {
            tsan_sizes().lock().insert(base as usize, total);
        }
        base
    }
}

/// mimalloc's small-object ceiling (`MI_SMALL_SIZE_MAX`): a request at or
/// below it may take the entry that skips the size-class dispatch.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
const MI_SMALL_SIZE_MAX: usize = 128 * std::mem::size_of::<usize>();

/// Like [`rc_block_alloc_zeroed`] without the zero fill, for callers
/// that provably write every byte.
#[inline]
fn rc_block_alloc_unzeroed(total: usize) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        if total <= MI_SMALL_SIZE_MAX {
            // SAFETY: `mi_malloc_small` accepts a size up to `MI_SMALL_SIZE_MAX` (checked above)
            // and answers null on failure.
            unsafe { libmimalloc_sys::mi_malloc_small(total).cast() }
        } else {
            // SAFETY: `mi_malloc` accepts any size and answers null on failure.
            unsafe { libmimalloc_sys::mi_malloc(total).cast() }
        }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        rc_block_alloc_zeroed(total)
    }
}

/// Free an RC block allocated by [`rc_block_alloc_zeroed`].
#[inline]
unsafe fn rc_block_free(base: *mut u8) {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // SAFETY: this `unsafe fn`'s caller passes `base` a block from `rc_block_alloc_*` that
        // nothing uses afterwards.
        unsafe { libmimalloc_sys::mi_free(base.cast()) };
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        // The system allocator needs the original layout back; recover
        // the size from the tsan-only side map populated at allocation.
        let total = tsan_sizes()
            .lock()
            .remove(&(base as usize))
            .unwrap_or(RC_HEADER_SIZE);
        if let Ok(layout) = Layout::from_size_align(total, RC_ALIGN) {
            // SAFETY: `base` was allocated with this layout by the allocation path above, and
            // this is its one free.
            unsafe { dealloc(base, layout) };
        }
    }
}

/// Byte sizes of live RC blocks, ThreadSanitizer builds only (the
/// system allocator's `dealloc` needs the original layout; production
/// builds free through `mi_free`, which doesn't).
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
fn tsan_sizes() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, usize>> {
    static SIZES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    SIZES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

// ---------------------------------------------------------------
// Meta interning: blob pointer <-> u16 id.
// ---------------------------------------------------------------
//
// Metas are per-TYPE module constants - a program has a handful of
// distinct ones - so the header stores a 16-bit id instead of the
// 8-byte pointer. Reads (`meta_of`, on every release walk) are a single
// relaxed load from an append-only table; writes intern through a map
// with a per-thread single-entry memo, which hits ~always because
// allocation sites repeat the same type.

// `RcHeader::meta_id` is u16. Reserve the all-ones value as an internal
// exhaustion sentinel, so a full table is an allocation failure rather than
// silently turning a managed aggregate into a leaf and leaking its children.
const META_TABLE_CAP: usize = u16::MAX as usize;

/// Append-only id -> blob-pointer table. Slot 0 is permanently null. It holds a
/// slot for every `u16` id, so a header's id indexes it without a bounds check;
/// the sentinel's slot is never assigned and reads null.
static META_TABLE: [std::sync::atomic::AtomicUsize; META_TABLE_CAP + 1] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; META_TABLE_CAP + 1];

static META_IDS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<usize, u16>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
static META_NEXT: AtomicUsize = AtomicUsize::new(1);

/// Slots in the per-thread intern cache. A program allocates a handful of
/// shapes in any one loop - a node and the option that holds it, a row and its
/// element - and interleaves them, so a single remembered pair is displaced on
/// every other call and every allocation then takes the table lock.
const META_MEMO_SLOTS: usize = 16;

thread_local! {
    /// Recently interned (pointer, id) pairs, direct-mapped by pointer. One
    /// cell per slot, so a lookup reads only the slot it maps to.
    static META_MEMO: [std::cell::Cell<(usize, u16)>; META_MEMO_SLOTS] =
        const { [const { std::cell::Cell::new((0, 0)) }; META_MEMO_SLOTS] };
}

/// Cache slot for a blob pointer. Blobs are distinct allocations, so the bits
/// above the alignment are what tell two of them apart.
#[inline]
const fn meta_memo_slot(key: usize) -> usize {
    (key >> 4) & (META_MEMO_SLOTS - 1)
}

/// Interned id for a meta blob. A type's id is resolved through the table once
/// per thread; every later allocation of that type is the cache compare below,
/// inlined into the allocator.
#[inline]
fn meta_intern(meta: *const i64) -> Option<u16> {
    if meta.is_null() {
        return Some(0);
    }
    let key = meta as usize;
    let slot = meta_memo_slot(key);
    if let Some(id) = META_MEMO.with(|cache| {
        let entry = cache[slot].get();
        (entry.0 == key).then_some(entry.1)
    }) {
        return Some(id);
    }
    meta_intern_slow(key, slot)
}

#[cold]
#[inline(never)]
fn meta_intern_slow(key: usize, slot: usize) -> Option<u16> {
    let mut ids = META_IDS.lock();
    let id = if let Some(&id) = ids.get(&key) {
        id
    } else {
        let next = META_NEXT.fetch_add(1, Ordering::Relaxed);
        if next >= META_TABLE_CAP {
            return None;
        }
        let id = next as u16;
        META_TABLE[next].store(key, Ordering::Release);
        ids.insert(key, id);
        id
    };
    drop(ids);
    META_MEMO.with(|cache| cache[slot].set((key, id)));
    Some(id)
}

/// The child-layout blob for a header, or null for leaves.
#[inline]
unsafe fn meta_of(h: *const RcHeader) -> *const i64 {
    // SAFETY: `h` is the header of a live node (this `unsafe fn`'s caller).
    let id = unsafe { (*h).meta_id } as usize;
    META_TABLE[id].load(Ordering::Acquire) as *const i64
}

#[inline]
unsafe fn header_ptr(payload: *mut u8) -> *mut RcHeader {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node, whose header sits
    // `RC_HEADER_SIZE` bytes before it.
    unsafe { payload.sub(RC_HEADER_SIZE) as *mut RcHeader }
}

/// Allocate an RC-managed object with `size` payload bytes and the given
/// child-layout `meta` (may be null for leaves). Returns a pointer to the
/// zeroed payload with strong count 1, or null on allocation failure.
// The RC primitives are deliberately NOT wrapped in `ffi_entry!`
// (catch_unwind): they are called once per allocation / copy / drop and
// the per-call unwind-guard setup dominates their cost. They are also
// panic-free across the FFI boundary - pointer arithmetic and atomics
// never unwind, and the only allocator failure paths (`alloc_zeroed`
// returning null, `Vec` growth) `abort` rather than unwind. Keeping them
// bare is what makes RC-managed code fast.
/// Allocate a TAGGED-repr enum node (discriminant in pointer bits, no
/// header byte consulted at match time). Inside an active region the
/// node is completely HEADERLESS - `size` payload bytes, bump-allocated,
/// bulk-freed at pop, identified by the arena range check (never by a
/// header) - a two-pointer tree node costs exactly 16 bytes. Outside a
/// region this is a normal reference-counted allocation (the header
/// carries counts; the disc bits still live in the pointer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_tagged(size: u64, meta: *const i64) -> *mut u8 {
    // Hot path: bump within the innermost region's current slab touching only
    // the BUMP thread-local. `BUMP.base` is null whenever no region holds a
    // slab (every `arena_pop` restores it), so a hit unambiguously means
    // "a region is active and has room" - no separate `REGION_DEPTH` probe is
    // needed on the allocation-heavy path. Tagged region nodes are headerless,
    // unzeroed (the constructor stores every slot), and not `RC_LIVE`-counted,
    // matching the prior `region_alloc_inner_unzeroed(.., false)`.
    let need = (size as usize + RC_ALIGN - 1) & !(RC_ALIGN - 1);
    let hit = BUMP.with(|b| {
        let st = b.get();
        if !st.base.is_null() && st.cur + need <= st.end {
            // SAFETY: `st.cur + need <= st.end` (checked above), so the address lies inside the
            // bump slab.
            let p = unsafe { st.base.add(st.cur) };
            b.set(BumpState {
                cur: st.cur + need,
                ..st
            });
            Some(p)
        } else {
            None
        }
    });
    if let Some(p) = hit {
        crate::c_abi::ledger::rc_alloc(size as usize, need, true, false);
        return p;
    }
    // Miss: a region is active but its current slab is full / not yet acquired,
    // or no region is active at all. Disambiguate with the depth probe only now.
    if region_active() {
        let p = region_alloc_inner_unzeroed(size as usize, false);
        if !p.is_null() {
            crate::c_abi::ledger::rc_alloc(size as usize, need, true, false);
            return p;
        }
        // Arena unavailable: fall through to the headered global path.
    }
    // Tagged-enum constructors store every payload slot, so the global
    // path also skips the payload memset (the header is written field
    // by field below).
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let in_region = false;
    let _ = in_region;
    let base = rc_block_alloc_unzeroed(total);
    if base.is_null() {
        // SAFETY: `meta` is this shim's argument, null or a live meta table (C-ABI contract).
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is non-null (checked above), a fresh block of at least a header's size.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        // SAFETY: `base` is the fresh block from the global allocator.
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, false, false);
    // SAFETY: the block holds the header followed by the payload.
    unsafe { base.add(RC_HEADER_SIZE) }
}

/// A reference-counted allocation that never lands in a region, for a runtime
/// value whose lifetime no region scope bounds.
pub(crate) unsafe fn rc_alloc_global(size: u64, meta: *const i64) -> *mut u8 {
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let base = rc_block_alloc_zeroed(total);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is non-null (checked above), a fresh block of at least a header's size.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        // SAFETY: `base` is the fresh block from the global allocator.
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, false, false);
    // SAFETY: the block holds the header followed by the payload.
    unsafe { base.add(RC_HEADER_SIZE) }
}

/// A runtime object the runtime allocates as a counted node: compiled code
/// shares and releases it like any other node, and its last release drops it.
pub trait ManagedHandle: Sized + Send + Sync + 'static {
    /// The `RC_KIND_FINALIZED` layout naming this type's finalizer.
    fn meta() -> *const i64;
}

/// Implements [`ManagedHandle`] for a runtime handle type.
macro_rules! managed_handle {
    ($ty:ty) => {
        impl $crate::c_abi::rc::ManagedHandle for $ty {
            fn meta() -> *const i64 {
                static META: std::sync::OnceLock<[i64; 2]> = std::sync::OnceLock::new();
                META.get_or_init(|| $crate::c_abi::rc::finalized_meta::<$ty>())
                    .as_ptr()
            }
        }
    };
}
pub(crate) use managed_handle;

/// Drops the `T` a finalized node holds.
///
/// The value is moved out of the node, so its block can be reclaimed at once.
/// A value whose drop gives back shares of other nodes would re-enter the
/// release walk that is reclaiming this one, so inside a teardown frame the
/// drop waits for the outermost frame's exit, as an owned container child
/// does.
///
/// # Safety
///
/// `payload` is the payload of a dead node [`alloc_managed`] built for `T`.
unsafe extern "C" fn drop_managed<T: 'static>(payload: *mut u8) {
    // SAFETY: the payload holds the `T` `alloc_managed` wrote, moved out once,
    // when the node's last share is released.
    let value = unsafe { payload.cast::<T>().read() };
    if TEARDOWN_DEPTH.with(std::cell::Cell::get) == 0 {
        drop(value);
    } else {
        PENDING_FINALIZERS.with(|q| q.borrow_mut().push(Box::new(move || drop(value))));
    }
}

/// The `RC_KIND_FINALIZED` layout for `T`.
#[must_use]
pub fn finalized_meta<T: 'static>() -> [i64; 2] {
    let finalizer: unsafe extern "C" fn(*mut u8) = drop_managed::<T>;
    [
        gossamer_abi::rc::RC_KIND_FINALIZED,
        finalizer as usize as i64,
    ]
}

/// Allocates `value` as the payload of a counted node holding one share for
/// the caller. The node never lives in an arena region: a region frees its
/// blocks wholesale, which would skip the finalizer.
pub fn alloc_managed<T: ManagedHandle>(value: T) -> *mut T {
    const { assert!(std::mem::align_of::<T>() <= RC_ALIGN) };
    let Some(meta_id) = meta_intern(T::meta()) else {
        return std::ptr::null_mut();
    };
    let payload = rc_alloc_heap(std::mem::size_of::<T>(), meta_id).cast::<T>();
    if !payload.is_null() {
        // SAFETY: `payload` is a fresh, suitably aligned block of `size_of::<T>()` bytes.
        unsafe { payload.write(value) };
    }
    payload
}

/// Runs the finalizer of a dead `RC_KIND_FINALIZED` node; any other node has
/// none.
///
/// # Safety
///
/// `payload` is a node whose strong count just reached zero.
unsafe fn finalize(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a node.
    let meta = unsafe { meta_of(header_ptr(payload)) };
    // SAFETY: a non-null meta table's first word is its kind.
    if meta.is_null() || unsafe { *meta } != gossamer_abi::rc::RC_KIND_FINALIZED {
        return;
    }
    // SAFETY: a finalized layout's second word is its finalizer's address.
    let addr = unsafe { *meta.add(1) } as usize;
    // SAFETY: `addr` is the `drop_managed::<T>` `finalized_meta` stored for this node's type.
    let finalizer: unsafe extern "C" fn(*mut u8) =
        unsafe { std::mem::transmute(std::ptr::with_exposed_provenance::<()>(addr)) };
    // SAFETY: the node is dead and its payload holds the `T` the finalizer drops.
    unsafe { finalizer(payload) };
}

/// A zeroed counted block of `size` payload bytes, outside any region, with
/// one strong share and the layout `meta_id`.
fn rc_alloc_heap(size: usize, meta_id: u16) -> *mut u8 {
    let total = size.saturating_add(RC_HEADER_SIZE);
    let base = rc_block_alloc_zeroed(total);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is non-null (checked above), a fresh block of at least a header's size.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        // SAFETY: `base` is the fresh block from the global allocator.
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size, usable, false, false);
    // SAFETY: the block holds the header followed by the payload.
    unsafe { base.add(RC_HEADER_SIZE) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc(size: u64, meta: *const i64) -> *mut u8 {
    // Exact-size request: mimalloc's bins serve it without padding and
    // `mi_free` recovers everything from the pointer, so neither a size
    // field nor class rounding is needed.
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    // Inside a `region { … }` the object is bump-allocated and freed
    // wholesale at pop - tag it so retain/release stay no-ops and the
    // teardown walk never touches it.
    let in_region = region_active();
    let base = if in_region {
        region_alloc(total)
    } else {
        rc_block_alloc_zeroed(total)
    };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is non-null (checked above), a fresh block of at least a header's size.
    unsafe {
        (*h).strong = if in_region { 1 | REGION_BIT } else { 1 };
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    let usable = if in_region {
        total
    } else if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        // SAFETY: `base` is the fresh block from the global allocator.
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, in_region, false);
    // SAFETY: the block holds the header followed by the payload.
    unsafe { base.add(RC_HEADER_SIZE) }
}

/// Shared, pinned singleton for a payload-less enum variant with discriminant
/// `tag`. Unit variants carry no fields and are only read (the match reads the
/// tag at offset 0), so every `Tree::Leaf`-style construction shares one heap
/// node instead of allocating per use - a large RAM win for recursive enums
/// (full binary trees are ~half leaves). The node is allocated GLOBALLY (never
/// in an arena region, which would free it wholesale at pop and leave the
/// cached pointer dangling), and its base reference pins it for the process
/// lifetime; callers treat the pointer as a borrow, so the enclosing
/// aggregate's store retains it and teardown releases it (balanced).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_enum_unit(tag: i64) -> *mut u8 {
    use std::sync::atomic::{AtomicPtr, Ordering};
    const N: usize = 256;
    static SINGLETONS: [AtomicPtr<u8>; N] = [const { AtomicPtr::new(std::ptr::null_mut()) }; N];

    // Global (non-region) allocation of a tag-only RC node, pinned at strong=1.
    let alloc_global = |tag: i64| -> *mut u8 {
        let total = 8usize.saturating_add(RC_HEADER_SIZE);
        let base = rc_block_alloc_zeroed(total);
        if base.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `base` is non-null (checked above), a fresh block of a header plus eight
        // payload bytes.
        unsafe {
            let h = base as *mut RcHeader;
            (*h).strong = STRONG_IMMORTAL;
            (*h).weak = AtomicU8::new(0);
            // Unit-variant singleton: the discriminant lives in the
            // header byte the compiled match reads. The 8-byte payload
            // stays zeroed spare space.
            (*h).disc = u8::try_from(tag).unwrap_or(0);
            (*h).meta_id = 0;
            let payload = base.add(RC_HEADER_SIZE);
            rc_live_inc();
            let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
                rc_block_usable_size(base)
            } else {
                0
            };
            crate::c_abi::ledger::rc_alloc(8, usable, false, false);
            payload
        }
    };

    if !(0..N as i64).contains(&tag) {
        // Out-of-range discriminant: fall back to a fresh global node.
        // Uncached, so the caller's single ownership reclaims it.
        return alloc_global(tag);
    }
    // Every return hands the caller an OWNED share (+1): the drop pass
    // treats the destination local as owned (release at death) exactly
    // like any other constructor result, and a bare `let x = Enum::Unit`
    // binding must not strip the cache's pin when it dies. The cache
    // insert itself holds the initial strong=1 pin, so the singleton's
    // count never reaches zero.
    let slot = &SINGLETONS[tag as usize];
    let existing = slot.load(Ordering::Acquire);
    if !existing.is_null() {
        // SAFETY: `existing` is the cached singleton, pinned for the process.
        unsafe { gos_rt_rc_retain(existing) };
        return existing;
    }
    let fresh = alloc_global(tag);
    if fresh.is_null() {
        return fresh;
    }
    match slot.compare_exchange(
        std::ptr::null_mut(),
        fresh,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            // SAFETY: `fresh` is the singleton just installed.
            unsafe { gos_rt_rc_retain(fresh) };
            fresh
        }
        Err(winner) => {
            // Lost the race - drop the redundant node, share the winner's.
            // SAFETY: `fresh` lost the race, so this call holds its only share.
            unsafe { gos_rt_rc_release(fresh) };
            // SAFETY: `winner` is the installed singleton, pinned for the process.
            unsafe { gos_rt_rc_retain(winner) };
            winner
        }
    }
}

/// Increment the strong count of an RC object. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_retain(payload: *mut u8) {
    let payload = untag_rc(payload);
    if is_odd_string_repr(payload) {
        // SAFETY: `payload` is an odd-tagged string representation (the probe above).
        unsafe { crate::c_abi::string::gos_rt_str_retain(payload.cast()) };
        return;
    }
    // Region-arena objects are bulk-freed at pop and may be HEADERLESS:
    // never touch their memory from the accounting paths.
    if in_region_arena(payload) {
        return;
    }
    if payload.is_null() {
        return;
    }
    crate::c_abi::ledger::benchmark_arc_retain();
    // No string check here: a string body is odd-addressed and was routed
    // above, and `untag_rc` leaves every surviving pointer 8-aligned, which
    // the string carrier's low-bit shape can never match.
    // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    let h = unsafe { header_ptr(payload) };
    // `inc_strong` reads `strong` atomically and dispatches: region /
    // immortal objects are no-ops; escaped (shared) objects bump the
    // count atomically; thread-local objects take the cheap non-atomic
    // bump (count up, color back to black, buffered bit preserved).
    // SAFETY: `h` is the header of this shim's live counted argument (C-ABI contract).
    unsafe { inc_strong(h) };
}

/// Decrement the strong count; at zero, release RC-pointer children
/// (iteratively, to bound stack depth on deep structures) and free the
/// block (unless a weak ref still observes it). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_release(payload: *mut u8) {
    let payload = untag_rc(payload);
    if is_odd_string_repr(payload) {
        // SAFETY: `payload` is an odd-tagged string representation (the probe above), whose share
        // this release gives back.
        unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
        return;
    }
    // Region-arena objects are bulk-freed at pop and may be HEADERLESS:
    // never touch their memory from the accounting paths.
    if in_region_arena(payload) {
        return;
    }
    if !payload.is_null() {
        crate::c_abi::ledger::benchmark_arc_release();
    }
    // SAFETY: `payload` is this shim's counted argument, null or live, whose share it gives back
    // (C-ABI contract).
    unsafe { rc_release_impl(payload) };
}

/// Release for an exclusive tree teardown that must NOT defer to the cycle
/// collector. Identical to [`gos_rt_rc_release`] except a node that survives
/// the decrement is never buffered as a cycle candidate, and a node that
/// reaches zero has its buffered bit cleared before reclamation - so the block
/// is freed immediately instead of waiting for a collection slice that may
/// never run before exit. The caller (the VM's native-enum tree teardown) owns
/// the whole tree exclusively and has already cleared child slots, so there is
/// no live cycle to observe and no child to double-release. Null-safe.
pub unsafe fn rc_release_no_buffer(payload: *mut u8) {
    let payload = untag_rc(payload);
    if payload.is_null() || in_region_arena(payload) {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` live; non-null, checked above.
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        // SAFETY: `payload` is a live string body (the probe above), whose share this release
        // gives back.
        unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` live; non-null, checked above.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    let d = unsafe { dec_strong(h) };
    if d.skip {
        return;
    }
    if d.next != 0 {
        // Survived: deliberately do NOT buffer as a cycle root. A tree being
        // torn down exclusively has no live cycle for the collector to find.
        return;
    }
    if d.shared {
        fence(Ordering::Acquire);
        // A shared object's flag bits are never mutated non-atomically;
        // `try_reclaim` takes the atomic claim path. A stale buffered pin
        // defers the free to the owning thread's next collection slice.
        // SAFETY: `payload` is a live node whose count just reached zero.
        unsafe { try_reclaim(payload) };
        return;
    }
    // SAFETY: `h` is the header of the live, thread-local node `payload`.
    unsafe { set_color(h, COLOR_BLACK) };
    // Clear any buffered pin a prior release may have set so `try_reclaim`
    // frees the block now rather than leaving it for the collector. The pin
    // and the candidate-buffer entry are dropped together: a freed block
    // must never linger in `ROOTS`, or a later collection dereferences it.
    // SAFETY: `h` is the header of the live node `payload`.
    if unsafe { is_buffered(h) } {
        ROOTS.with(|r| {
            r.borrow_mut().remove(payload);
        });
        // SAFETY: `h` is the header of the live, thread-local node `payload`.
        unsafe { set_buffered(h, false) };
    }
    // Child slots are already cleared by the caller, so there are no
    // children to release here.
    // SAFETY: `payload` is a live node whose count just reached zero.
    unsafe { try_reclaim(payload) };
}

/// Strong reference count of an RC-managed node, or `0` for a node the
/// release path leaves untouched (null, region-arena, immortal unit
/// singleton, or a gos string). Diagnostic / teardown helper for the
/// in-process JIT, which owns a freshly-built tree of nodes and must reclaim
/// each one fully even when the compiled tier's `?` accounting left it
/// over-retained; never emitted by codegen.
/// Whether `payload`'s object counts its shares atomically because it has
/// escaped to another goroutine. Reads a header bit no accessor otherwise
/// exposes, so the escape points that must set it can be asserted.
#[cfg(test)]
#[must_use]
pub(crate) unsafe fn rc_payload_is_shared(payload: *mut u8) -> bool {
    let base = untag_rc(payload);
    if base.is_null() || in_region_arena(base) {
        return false;
    }
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe { is_shared(header_ptr(base)) }
}

pub unsafe fn rc_strong_count(payload: *mut u8) -> i64 {
    let payload = untag_rc(payload);
    if payload.is_null() || in_region_arena(payload) {
        return 0;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` live; non-null, checked above.
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        return 0;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` live; non-null, checked above.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    let count = unsafe { strong_count(h) };
    if count == STRONG_IMMORTAL {
        return 0;
    }
    i64::from(count)
}

/// Create a weak reference from a strong-held payload: increment the weak
/// count and return the same pointer (now carrying weak ownership). Does not
/// touch the strong count. Null-safe (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_downgrade(payload: *mut u8) -> *mut u8 {
    // Preserve the caller's pointer bit-for-bit (a tagged-repr enum's
    // disc lives in it; the weak round-trip must hand it back). Mask
    // only for header access.
    let base = untag_rc(payload);
    // Region-arena objects are bulk-freed at pop and may be HEADERLESS:
    // never touch their memory from the accounting paths.
    if in_region_arena(base) {
        return std::ptr::null_mut();
    }
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `base` is non-null (checked above) and this shim's live counted argument (C-ABI
    // contract).
    let h = unsafe { header_ptr(base) };
    // SAFETY: `h` is the header of that live node.
    unsafe { inc_weak(h) };
    payload
}

/// Increment the weak count (copying a `Weak`). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_retain(payload: *mut u8) {
    let payload = untag_rc(payload);
    // Region-arena objects are bulk-freed at pop and may be HEADERLESS:
    // never touch their memory from the accounting paths.
    if in_region_arena(payload) {
        return;
    }
    if payload.is_null() {
        return;
    }
    // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of this shim's live argument.
    unsafe { inc_weak(h) };
}

/// Decrement the weak count; if both strong and weak counts are now zero,
/// free the (already payload-destroyed) allocation. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_release(payload: *mut u8) {
    let payload = untag_rc(payload);
    // Region-arena objects are bulk-freed at pop and may be HEADERLESS:
    // never touch their memory from the accounting paths.
    if in_region_arena(payload) {
        return;
    }
    if payload.is_null() {
        return;
    }
    // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    let h = unsafe { header_ptr(payload) };
    // `dec_weak` returns the old weak count; the new value is prev - 1. When
    // prev == 1 the count just reached 0, so the allocation can be reclaimed
    // if nothing else pins it.
    // SAFETY: `h` is the header of this shim's live weak referent (C-ABI contract).
    if unsafe { dec_weak(h) } == 1 {
        // SAFETY: `payload` is the weak referent, whose last weak share just went.
        unsafe { try_reclaim(payload) };
    }
}

/// Shared core of the weak-upgrade entry points: take a fresh strong
/// reference iff the referent is still alive. Returns the caller's pointer
/// verbatim (tag bits included) on success, null once the referent is dead.
/// For a shared referent the take is a CAS from a non-zero count, so two
/// goroutines racing an upgrade against the final release can never revive
/// a dead object (and the liveness check and the count bump are one atomic
/// step, not a check-then-act).
unsafe fn weak_upgrade_take(payload: *mut u8) -> *mut u8 {
    let base = untag_rc(payload);
    if in_region_arena(base) {
        return std::ptr::null_mut();
    }
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `base` is non-null (checked above), a block the weak reference keeps allocated.
    let h = unsafe { header_ptr(base) };
    // SAFETY: `h` is the header of the block the weak reference keeps allocated.
    let s = unsafe { load_strong(h) };
    let count = s & STRONG_COUNT_MASK;
    if count == 0 {
        return std::ptr::null_mut();
    }
    if s & SHARED_BIT != 0 {
        // CAS loop: atomically upgrade only while the strong count remains
        // non-zero. Two goroutines upgrading the same weak reference
        // simultaneously must not both succeed after the referent dies.
        // SAFETY: `h` is that header; a shared node's count is only ever accessed atomically.
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        let mut cur = s;
        loop {
            if cur & STRONG_COUNT_MASK == 0 {
                return std::ptr::null_mut();
            }
            match a.compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    } else {
        // Thread-local object: no concurrent writer exists.
        // SAFETY: `h` is the header of a thread-local node, which no other thread writes.
        unsafe { set_strong_count(h, count.saturating_add(1)) };
        // Color black so a later cycle scan treats the revived object as
        // live. Shared objects never enter the collector and their color
        // bits carry the reclaim-claim flag, so only the thread-local path
        // recolors.
        // SAFETY: `h` is the header of a thread-local node, which no other thread writes.
        unsafe { set_color(h, COLOR_BLACK) };
    }
    payload
}

/// Attempt to obtain a strong reference from a weak one. If the referent is
/// still alive (`strong > 0`), increment the strong count and return the
/// payload; otherwise return null (the `None` shape). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_upgrade(payload: *mut u8) -> *mut u8 {
    // SAFETY: `payload` is this shim's weak argument, a block its weak share keeps allocated
    // (C-ABI contract).
    unsafe { weak_upgrade_take(payload) }
}

/// Upgrade a weak reference to `Option<T>` for the language-level
/// `w.upgrade()`. Returns the packed `{disc, payload}` pair discriminated
/// as `Some` (disc 0) carrying the payload pointer when the referent is
/// still alive (`strong > 0`), or `None` (disc 1) once it has been
/// reclaimed.
///
/// The `Some` payload carries a fresh strong reference, taken atomically
/// (CAS from a non-zero count for shared referents), so the value stays
/// alive for the caller even when another goroutine drops the last other
/// strong reference concurrently. The MIR lowering pins that reference in
/// a frame-owned shadow local (`gos_rt_weak_opt_payload`) released at
/// scope exit, mirroring the interpreter's `Some(value)` clone which lives
/// until its binding dies. Null-safe (returns `None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_upgrade_opt(payload: *mut u8) -> i128 {
    // SAFETY: `payload` is this shim's weak argument, a block its weak share keeps allocated
    // (C-ABI contract).
    let taken = unsafe { weak_upgrade_take(payload) };
    if taken.is_null() {
        crate::c_abi::result::pack_result(1, 0)
    } else {
        crate::c_abi::result::pack_result(0, taken as i64)
    }
}

/// Free a block's allocation only when nothing pins it: no strong refs, no
/// weak refs, and not awaiting the cycle collector. The single funnel every
/// release path goes through, so each block is freed exactly once. For a
/// thread-local block the checks need no atomicity (single mutator); a
/// shared block routes through the CAS claim in [`try_reclaim_shared`].
unsafe fn try_reclaim(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    let s = unsafe { load_strong(h) };
    if s & SHARED_BIT != 0 {
        // SAFETY: `h` is `payload`'s header, and the node is shared.
        unsafe { try_reclaim_shared(payload, h) };
        return;
    }
    if s & STRONG_COUNT_MASK == 0
        && s & BUFFERED_BIT == 0
        // SAFETY: `h` is the header of the live node `payload`.
        && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0
    {
        // SAFETY: `payload` has no strong, weak, or buffered reference left (checked above).
        unsafe { free_block(payload) };
    }
}

/// Reclaim a dead shared block exactly once. The final strong release and
/// the final weak release can race on different goroutines, both observing
/// `strong == 0 && weak == 0`; the free is claimed by a CAS setting
/// [`SHARED_RECLAIM_BIT`], so exactly one claimant frees.
///
/// The claim conditions are stable once observed: a dead shared object's
/// strong count can never rise again (upgrades CAS from a non-zero count
/// only), and with both counts zero no reference exists from which a new
/// weak could be minted, so `weak` can never leave zero after it is read
/// under `strong == 0`. A buffered pin defers the free to the owning
/// thread's collection slice, which clears the pin and re-runs the claim.
unsafe fn try_reclaim_shared(payload: *mut u8, h: *mut RcHeader) {
    // SAFETY: this `unsafe fn`'s caller passes `h` the header of the live, shared node `payload`.
    let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
    let mut cur = a.load(Ordering::Acquire);
    loop {
        if cur & STRONG_COUNT_MASK != 0 || cur & BUFFERED_BIT != 0 || cur & SHARED_RECLAIM_BIT != 0
        {
            return;
        }
        // SAFETY: `h` is the header of the live node `payload`.
        if unsafe { (*h).weak.load(Ordering::Acquire) } != 0 {
            return;
        }
        match a.compare_exchange_weak(
            cur,
            cur | SHARED_RECLAIM_BIT,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
    // SAFETY: the claim above made this call the one that frees `payload`, which nothing
    // references.
    unsafe { free_block(payload) };
}

/// Usable byte capacity of an RC block (header + payload), recovered from the
/// allocator. Used by the reuse path to confirm a recycled block is large
/// enough before re-homing a constructor into it.
#[inline]
unsafe fn rc_block_usable_size(base: *mut u8) -> usize {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // SAFETY: this `unsafe fn`'s caller passes `base` a live block from the global allocator.
        unsafe { libmimalloc_sys::mi_usable_size(base.cast()) }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        tsan_sizes()
            .lock()
            .get(&(base as usize))
            .copied()
            .unwrap_or(0)
    }
}

/// Release the RC-pointer children named by `payload`'s header meta (string
/// children freed through the string path, RC-node children released, each
/// cascading through its own iterative release). Used by the reuse path, which
/// keeps the parent block alive for recycling but must still drop its children
/// exactly as a normal release would.
unsafe fn release_rc_children(payload: *mut u8) {
    use gossamer_abi::rc::{RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_VEC};
    // SAFETY: this `unsafe fn`'s caller passes `payload` a node whose count reached zero.
    unsafe { finalize(payload) };
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    // One pass over the meta covers every child kind. The reuse frame holds
    // no teardown state of its own, so a Vec child is freed here rather than
    // queued for an exit this path does not have; a Map child is queued on
    // the same terms as every other release path, and the cascading
    // `gos_rt_rc_release` below drains it at its own teardown exit.
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entries(payload, |kind, child| match kind {
            RC_CHILD_RC => {
                let c = untag_rc(child);
                if crate::c_abi::string::is_gos_string(c.cast()) {
                    crate::c_abi::string::gos_rt_str_free(c.cast());
                } else {
                    gos_rt_rc_release(c);
                }
            }
            RC_CHILD_VEC => crate::c_abi::map::gos_rt_vec_free(child.cast()),
            RC_CHILD_MAP => queue_map_child(child),
            gossamer_abi::rc::RC_CHILD_SET => queue_set_child(child),
            gossamer_abi::rc::RC_CHILD_DEQUE => queue_deque_child(child),
            gossamer_abi::rc::RC_CHILD_HEAP => crate::c_abi::map::gos_rt_vec_free(child.cast()),
            gossamer_abi::rc::RC_CHILD_ITER => drop_iter_child(child, false),
            gossamer_abi::rc::RC_CHILD_ITER_PAIR => drop_iter_child(child, true),
            gossamer_abi::rc::RC_CHILD_WEAK => gos_rt_rc_weak_release(child),
            gossamer_abi::rc::RC_CHILD_ERROR_FIELDS => drop(Box::from_raw(
                child.cast::<crate::c_abi::errors::ErrorFields>(),
            )),
            _ => {}
        });
    }
}

/// Perceus reuse (the `drop` half): like [`gos_rt_rc_release`], but when
/// `payload` is the unique last owner of a thread-local, non-region, weak-free,
/// unbuffered block, its RC children are released and the bare block base is
/// RETURNED for in-place reuse by a same-size constructor
/// ([`gos_rt_rc_alloc_reuse`]) instead of being freed. Returns null in every
/// other case (survived a decrement, escaped to a goroutine, region-allocated,
/// weak-pinned, buffered as a cycle candidate, or a string), having performed
/// the normal release.
///
/// A returned block is neither freed nor `RC_LIVE`-decremented: the paired
/// `alloc_reuse` re-homes the same live slot. The caller MUST pass a non-null
/// token to `gos_rt_rc_alloc_reuse` on every path or the block leaks - the MIR
/// reuse pass only emits the pair when the constructor unconditionally follows.
/// Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_drop_reuse(payload: *mut u8) -> *mut u8 {
    let payload = untag_rc(payload);
    if payload.is_null() || in_region_arena(payload) {
        return std::ptr::null_mut();
    }
    // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        // SAFETY: `payload` is a live string body (the probe above), whose share this release
        // gives back.
        unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
        return std::ptr::null_mut();
    }
    // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    let d = unsafe { dec_strong(h) };
    if d.skip {
        return std::ptr::null_mut();
    }
    if d.next != 0 {
        if !d.shared {
            // SAFETY: `payload` is this shim's argument, live for the call (C-ABI contract);
            // non-null, checked above.
            unsafe { possible_root(payload) };
        }
        return std::ptr::null_mut();
    }
    if d.shared {
        fence(Ordering::Acquire);
    } else {
        // Shared flag bits are only ever mutated atomically; the color is
        // meaningless for shared objects (they never enter the collector).
        // SAFETY: `h` is the header of the live, thread-local node `payload`.
        unsafe { set_color(h, COLOR_BLACK) };
    }
    // SAFETY: `payload` is the live node whose count just reached zero.
    unsafe { release_rc_children(payload) };
    // Reuse only a thread-local, weak-free, unbuffered block; anything else is
    // reclaimed normally (try_reclaim frees iff unpinned).
    // SAFETY: `h` is the header of the live node `payload`.
    if !d.shared && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0 && !unsafe { is_buffered(h) }
    {
        return h as *mut u8;
    }
    // SAFETY: `payload` is the node whose count just reached zero.
    unsafe { try_reclaim(payload) };
    std::ptr::null_mut()
}

/// Perceus reuse (the `alloc` half): allocate by reusing a block returned from
/// [`gos_rt_rc_drop_reuse`], or fall back to a fresh allocation when the token
/// is null or unsuitable. On reuse the header is reset (strong 1, no weak, disc
/// 0, the given `meta`) and the payload zeroed, leaving the block identical to
/// a fresh [`gos_rt_rc_alloc`]. An active region (the new object must be
/// bump-allocated and bulk-freed), or a token too small for `size`, frees the
/// token and allocates fresh.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_reuse(
    token: *mut u8,
    size: u64,
    meta: *const i64,
) -> *mut u8 {
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    if token.is_null() {
        // SAFETY: `meta` is this shim's argument, null or a live meta table (C-ABI contract).
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    // SAFETY: `token` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    // SAFETY: `token` is non-null (checked above), a block `gos_rt_rc_drop_reuse` handed back.
    if region_active() || unsafe { rc_block_usable_size(token) } < total {
        // Free the recycled block (its children are already released) and make
        // a fresh allocation. `free_block` takes a payload pointer.
        // SAFETY: `token` is a block `gos_rt_rc_drop_reuse` handed back, released of its
        // children, which nothing references.
        unsafe { free_block(token.add(RC_HEADER_SIZE)) };
        // SAFETY: `meta` is this shim's argument, null or a live meta table (C-ABI contract).
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let h = token as *mut RcHeader;
    // SAFETY: `token` is a block of at least `total` usable bytes (checked above) that this call
    // owns.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    // SAFETY: the block holds the header followed by the payload.
    let payload = unsafe { token.add(RC_HEADER_SIZE) };
    // SAFETY: the block's usable size covers the header and `size` payload bytes (checked above).
    unsafe { std::ptr::write_bytes(payload, 0, size as usize) };
    rc_reuse_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        // SAFETY: `token` is this shim's argument, live for the call (C-ABI contract); non-null,
        // checked above.
        unsafe { rc_block_usable_size(token) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, false, true);
    payload
}

/// Iterative release: maintain an explicit worklist of payloads whose
/// strong count must be decremented. When a count reaches zero, release its
/// RC-pointer children and reclaim the block. A non-zero result may be a
/// cycle root, so it is buffered for the collector. Iterative (not
/// recursive) so a deep tree/list cannot overflow the runtime's own stack.
unsafe fn rc_release_impl(root: *mut u8) {
    if root.is_null() {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `root` live; non-null, checked above.
    if unsafe { crate::c_abi::string::is_gos_string(root.cast()) } {
        // SAFETY: `root` is a live string body (the probe above), whose share this release gives
        // back.
        unsafe { crate::c_abi::string::gos_rt_str_free(root.cast()) };
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `root` live; non-null, checked above.
    let h = unsafe { header_ptr(root) };
    // Region objects are freed wholesale at region pop - never individually.
    // Skipping the decrement-and-walk here is exactly what eliminates the
    // per-node teardown cost for `region { … }` allocations.
    // `dec_strong` reads `strong` atomically and dispatches: region /
    // immortal objects are no-ops; escaped (shared) objects decrement
    // atomically (Release); thread-local objects take the cheap
    // non-atomic decrement.
    // SAFETY: `h` is the header of the live node `root`.
    let d = unsafe { dec_strong(h) };
    if d.skip {
        return;
    }
    if d.next != 0 {
        // Survived the decrement. Thread-local objects become cycle
        // candidates; shared objects are excluded from the per-thread
        // collector (their cycles leak, like `Arc` - break with weak refs).
        if !d.shared {
            // SAFETY: this `unsafe fn`'s caller passes `root` live; non-null, checked above.
            unsafe { possible_root(root) };
        }
        return;
    }
    // Last reference. For a shared object an Acquire fence pairs with the
    // other workers' Release decrements so this thread sees all their
    // writes before tearing it down (now exclusively owned - count 0).
    if d.shared {
        fence(Ordering::Acquire);
    } else {
        // SAFETY: `h` is the header of the live, thread-local node `root`.
        unsafe { set_color(h, COLOR_BLACK) };
    }
    // SAFETY: `h` is the header of the live node `root`.
    let meta = unsafe { meta_of(h) };
    // Leaf fast path: a childless object (no RC-pointer children, the
    // overwhelming common case - every enum payload-free variant, every
    // leaf node) is reclaimed directly. This avoids touching the worklist
    // at all, so the dominant release shape never allocates or recurses.
    if meta.is_null() {
        // SAFETY: `root` is the node whose count just reached zero.
        unsafe { try_reclaim_zero(root) };
        return;
    }
    // Internal node: walk children iteratively (bounds stack depth on deep
    // structures). Reuse a thread-local worklist buffer - allocating a
    // fresh `Vec` per release call was a malloc/free on every node teardown
    // (millions, for tree workloads), dwarfing the actual reclamation.
    //
    // Owned Vec children are only QUEUED during the walk and freed once the
    // outermost teardown frame exits: `gos_rt_vec_free` can release RC-node
    // elements, re-entering this function (or the collector mid-phase), and
    // the thread-local worklist must be free for that nested walk to borrow.
    teardown_enter();
    RELEASE_WORKLIST.with(|cell| {
        let mut worklist = cell.borrow_mut();
        worklist.clear();
        // Single fused pass over the meta: string-tagged children free
        // through the tag-checking string path, RC children join the
        // release worklist, and owned containers are queued for the
        // outermost teardown exit.
        // SAFETY: `root` is the dead node, whose children this release gives back.
        unsafe { release_children_into(root, &mut worklist) };
        let _ = meta;
        // SAFETY: `h` is `root`'s header, and `root` is dead.
        unsafe { reclaim_dead(root, h, d.shared) };
        while let Some(payload) = worklist.pop() {
            if payload.is_null() {
                continue;
            }
            // SAFETY: `payload` is a live child node the release walk reached.
            let h = unsafe { header_ptr(payload) };
            // SAFETY: `h` is the header of the live node `payload`.
            let d = unsafe { dec_strong(h) };
            if d.skip {
                continue;
            }
            if d.next != 0 {
                if !d.shared {
                    // SAFETY: `payload` is a live, thread-local node.
                    unsafe { possible_root(payload) };
                }
                continue;
            }
            if d.shared {
                fence(Ordering::Acquire);
            } else {
                // SAFETY: `h` is the header of the live, thread-local node `payload`.
                unsafe { set_color(h, COLOR_BLACK) };
            }
            // SAFETY: `payload` is the dead node, whose children this release gives back.
            unsafe {
                release_children_into(payload, &mut worklist);
            }
            // SAFETY: `h` is `payload`'s header, and `payload` is dead.
            unsafe { reclaim_dead(payload, h, d.shared) };
        }
    });
    // SAFETY: this runs at the outermost teardown exit, which drains its queues.
    unsafe { teardown_exit() };
}

/// Reclaims a block whose strong count just reached zero and whose children
/// have been handed to the walk. A thread-local block that no weak reference
/// and no collector buffer pins is freed on the spot, from the header word
/// already read; every other shape takes the general claim.
///
/// The header is unchanged since the decrement: handing children off only
/// queues them, and a child string's free never touches an RC header.
#[inline]
unsafe fn reclaim_dead(payload: *mut u8, h: *mut RcHeader, shared: bool) {
    if !shared
        // SAFETY: this `unsafe fn`'s caller passes `h` the header of the dead node `payload`.
        && unsafe { (*h).strong } & BUFFERED_BIT == 0
        // SAFETY: `h` is the header of the dead node `payload`.
        && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0
    {
        rc_live_dec();
        // SAFETY: the node is thread-local, unbuffered, and weak-free (checked above), so this
        // frees its last reference.
        unsafe { rc_block_free(block_base(h)) };
        return;
    }
    // SAFETY: `payload` is a dead node.
    unsafe { try_reclaim_zero(payload) };
}

/// The allocation base of the block `h` heads. A copy blob outside the arena
/// has its owner word in front of its header; every other block starts at the
/// header.
#[inline]
unsafe fn block_base(h: *mut RcHeader) -> *mut u8 {
    // SAFETY: this `unsafe fn`'s caller passes `h` a live node's header.
    if unsafe { (*h).disc } == COPY_BLOB_DISC && !in_copy_blob_arena(h.cast()) {
        // SAFETY: a copy blob outside its arena carries its owner in the bytes before the header.
        unsafe { (h as *mut u8).sub(COPY_BLOB_OWNER_BYTES) }
    } else {
        h.cast::<u8>()
    }
}

/// Fused child dispatch for the worklist loop: strings are freed
/// immediately, RC children are appended to `worklist`, and owned Vec and
/// Map children are queued for release at the outermost teardown exit.
///
/// One pass over the node's meta covers all three kinds. A node's children
/// are read once per teardown, which is what keeps the per-node cost
/// proportional to the children it has rather than to the kinds it might
/// have had. A guarded copy blob names only copy-blob children, which are
/// never strings or containers, so its walk pushes each validated child
/// straight onto the worklist.
#[allow(
    clippy::inline_always,
    reason = "runs once per node of every teardown walk: left to the heuristic, LLVM keeps it out of line at its three call sites, which callgrind measured as a call per node on a tree workload"
)]
#[inline(always)]
unsafe fn release_children_into(payload: *mut u8, worklist: &mut Vec<*mut u8>) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a node whose count reached zero.
    unsafe { finalize(payload) };
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    // SAFETY: `meta` is non-null (checked above), the node's meta table.
    if unsafe { *meta } == RC_KIND_STRUCT_GUARDED {
        // SAFETY: `payload` is laid out as `meta` describes.
        unsafe { visit_guarded_children(payload, meta, |child| worklist.push(child)) };
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe { release_structural_children_into(payload, worklist) };
}

#[inline(never)]
unsafe fn release_structural_children_into(payload: *mut u8, worklist: &mut Vec<*mut u8>) {
    use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_RC, RC_CHILD_WORD_MASK};
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    // SAFETY: `meta` is non-null (checked above), the node's meta table.
    let kind = unsafe { *meta };
    if kind != RC_KIND_ENUM && kind != RC_KIND_STRUCT {
        // SAFETY: `payload` is laid out as `meta` describes.
        unsafe {
            visit_entry_slots(payload, |child_kind, _slot, child| {
                release_child_of_kind(child_kind, child, worklist);
            });
        }
        return;
    }
    // The meta walk of `visit_entry_slots`, written out so each counted child
    // joins the worklist in the loop that finds it: a teardown reads every
    // node's children once, and a callback per child is most of that read.
    // SAFETY: a meta table's second word is its variant count.
    let variant_count = unsafe { *meta.add(1) };
    let target_disc = if kind == RC_KIND_ENUM {
        // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
        i64::from(unsafe { (*header_ptr(payload)).disc })
    } else {
        0
    };
    let mut idx: usize = 2;
    for _ in 0..variant_count.max(0) {
        // SAFETY: `idx` stays inside the meta table the variant count describes.
        let disc = unsafe { *meta.add(idx) };
        // SAFETY: `idx + 1` stays inside the meta table the variant count describes.
        let child_count = usize::try_from(unsafe { *meta.add(idx + 1) }).unwrap_or(0);
        if kind == RC_KIND_STRUCT || disc == target_disc {
            for j in 0..child_count {
                // SAFETY: the child entries follow the variant header inside the meta table.
                let entry = unsafe { *meta.add(idx + 2 + j) };
                let child_kind = entry >> RC_CHILD_KIND_SHIFT;
                let word = usize::try_from(entry & RC_CHILD_WORD_MASK).unwrap_or(0);
                // SAFETY: each child entry names a word inside the payload.
                let slot = unsafe { payload.add(word * 8) };
                // SAFETY: `slot` is that word.
                let child = unsafe { crate::c_abi::vec::slot_read_word(slot) };
                if child.is_null() {
                    continue;
                }
                if child_kind == RC_CHILD_RC {
                    let c = untag_rc(child);
                    // SAFETY: `c` is a non-null child word, a candidate the string probe accepts.
                    if unsafe { crate::c_abi::string::is_gos_string(c.cast()) } {
                        // SAFETY: `c` is a live string body (the probe above), whose share the
                        // dead parent held.
                        unsafe { crate::c_abi::string::gos_rt_str_free(c.cast()) };
                    } else {
                        worklist.push(c);
                    }
                } else {
                    // SAFETY: `child` is the dead parent's child of `child_kind`, held at `slot`.
                    unsafe { release_gated_child(child_kind, slot, child, worklist) };
                }
            }
            return;
        }
        idx += 2 + child_count;
    }
}

/// Releases a structural child that is not a plain counted pointer: a carrier
/// payload word gated on its discriminant, or an owned container.
#[cold]
unsafe fn release_gated_child(
    child_kind: i64,
    slot: *mut u8,
    child: *mut u8,
    worklist: &mut Vec<*mut u8>,
) {
    match gossamer_abi::rc::rc_child_blob_gate(child_kind) {
        Some(gate) => {
            // SAFETY: a gated carrier child's discriminant is the word before its payload.
            let disc = unsafe { slot.cast::<i64>().sub(1).read_unaligned() };
            // SAFETY: `child` is the carrier's payload word, which the gate check reads.
            if (gate < 0 || disc == gate) && unsafe { is_copy_blob(child) } {
                // SAFETY: `child` is a copy blob the dead parent held a share of.
                unsafe { release_child_of_kind(gossamer_abi::rc::RC_CHILD_RC, child, worklist) };
            }
        }
        // SAFETY: `child` is the dead parent's child of `child_kind`.
        None => unsafe { release_child_of_kind(child_kind, child, worklist) },
    }
}

/// Hands one child of a dead node to its release: a string is freed, a counted
/// node joins the worklist, and a container is queued for the outermost
/// teardown exit.
unsafe fn release_child_of_kind(kind: i64, child: *mut u8, worklist: &mut Vec<*mut u8>) {
    use gossamer_abi::rc::{RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_VEC};
    match kind {
        RC_CHILD_RC => {
            let c = untag_rc(child);
            // SAFETY: `c` is a non-null child word, a candidate the string probe accepts.
            if unsafe { crate::c_abi::string::is_gos_string(c.cast()) } {
                // SAFETY: `c` is a live string body (the probe above), whose share the dead
                // parent held.
                unsafe { crate::c_abi::string::gos_rt_str_free(c.cast()) };
            } else {
                worklist.push(c);
            }
        }
        RC_CHILD_VEC => queue_vec_child(child),
        RC_CHILD_MAP => queue_map_child(child),
        gossamer_abi::rc::RC_CHILD_SET => queue_set_child(child),
        gossamer_abi::rc::RC_CHILD_DEQUE => queue_deque_child(child),
        gossamer_abi::rc::RC_CHILD_HEAP => queue_vec_child(child),
        gossamer_abi::rc::RC_CHILD_ITER => queue_iter_child(child, false),
        gossamer_abi::rc::RC_CHILD_ITER_PAIR => queue_iter_child(child, true),
        // A weak share frees nothing but the target's block, once its strong
        // count is gone too, so it is given back on the spot.
        // SAFETY: an `RC_CHILD_WEAK` child is a block the dead parent's weak share kept
        // allocated.
        gossamer_abi::rc::RC_CHILD_WEAK => unsafe { gos_rt_rc_weak_release(child) },
        gossamer_abi::rc::RC_CHILD_ERROR_FIELDS => {
            // SAFETY: an `RC_CHILD_ERROR_FIELDS` child is the boxed field list its error owns
            // alone.
            drop(unsafe { Box::from_raw(child.cast::<crate::c_abi::errors::ErrorFields>()) });
        }
        _ => {}
    }
}

thread_local! {
    /// Reused scratch buffer for the iterative release walk. A fresh `Vec`
    /// per `rc_release_impl` call was a malloc/free on every node teardown.
    /// Not re-entered: the walk calls no user code.
    static RELEASE_WORKLIST: std::cell::RefCell<Vec<*mut u8>> =
        std::cell::RefCell::new(Vec::with_capacity(64));
    /// Owned Vec children of dead nodes, queued during release / collection
    /// walks and freed only at the outermost teardown exit. Freeing a Vec
    /// can cascade into RC-node releases, so it must never run while the
    /// release worklist is borrowed or the collector is mid-phase.
    static PENDING_VEC_FREES: std::cell::RefCell<Vec<*mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Owned `Map` children of dead nodes, queued on the same terms as
    /// [`PENDING_VEC_FREES`]: freeing a map releases every entry it holds,
    /// which re-enters the release path.
    static PENDING_MAP_FREES: std::cell::RefCell<Vec<*mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Owned `Set` children of dead nodes, on the same terms as
    /// [`PENDING_MAP_FREES`].
    static PENDING_SET_FREES: std::cell::RefCell<Vec<*mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Owned `Deque` / `Queue` / `Stack` children of dead nodes, on the same
    /// terms as [`PENDING_MAP_FREES`].
    static PENDING_DEQUE_FREES: std::cell::RefCell<Vec<*mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Lazy iterator shares held by dead nodes, with whether each is a pair
    /// handle. Dropping the last share releases the sequence the iterator
    /// reads, which re-enters the release path.
    static PENDING_ITER_FREES: std::cell::RefCell<Vec<(*mut u8, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Values of dead managed nodes whose drop waits for the outermost
    /// teardown exit, on the same terms as [`PENDING_MAP_FREES`].
    static PENDING_FINALIZERS: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Nesting depth of teardown frames (release walks / collection
    /// slices) on this thread; pending Vec frees drain when it reaches 0.
    static TEARDOWN_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Queue a dead node's owned Vec child for release at the outermost
/// teardown exit.
fn queue_vec_child(v: *mut u8) {
    PENDING_VEC_FREES.with(|q| q.borrow_mut().push(v));
}

/// Queue a dead node's owned `Map` child for release at the outermost
/// teardown exit.
fn queue_map_child(m: *mut u8) {
    PENDING_MAP_FREES.with(|q| q.borrow_mut().push(m));
}

/// Queue a dead node's owned `Set` child for release at the outermost
/// teardown exit.
fn queue_set_child(s: *mut u8) {
    PENDING_SET_FREES.with(|q| q.borrow_mut().push(s));
}

/// Queue a dead node's owned deque child for release at the outermost
/// teardown exit.
fn queue_deque_child(d: *mut u8) {
    PENDING_DEQUE_FREES.with(|q| q.borrow_mut().push(d));
}

/// Queue a dead node's lazy iterator share for release at the outermost
/// teardown exit.
fn queue_iter_child(iter: *mut u8, pair: bool) {
    PENDING_ITER_FREES.with(|q| q.borrow_mut().push((iter, pair)));
}

/// Gives up one share of a lazy iterator child.
unsafe fn drop_iter_child(iter: *mut u8, pair: bool) {
    // SAFETY: this `unsafe fn`'s caller passes `iter` a lazy iterator holding the share this
    // gives back.
    unsafe { lazy_children::drop_share(iter, pair) };
}

/// Shares of the lazy iterator handles a value can hold as children.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod lazy_children {
    /// Gives up one share of `iter`.
    pub(crate) unsafe fn drop_share(iter: *mut u8, pair: bool) {
        if pair {
            // SAFETY: this `unsafe fn`'s caller passes `iter` a pair iterator holding the share
            // this gives back.
            unsafe { crate::c_abi::gos_rt_lazy_iter_drop_pair_i64(iter.cast()) };
        } else {
            // SAFETY: this `unsafe fn`'s caller passes `iter` a lazy iterator holding the share
            // this gives back.
            unsafe { crate::c_abi::gos_rt_lazy_iter_drop_i64(iter.cast()) };
        }
    }

    /// Takes one more share of `iter`.
    pub(crate) unsafe fn retain(iter: *mut u8, pair: bool) {
        if pair {
            // SAFETY: this `unsafe fn`'s caller passes `iter` a live pair iterator.
            unsafe { crate::c_abi::lazy_iter_pair_retain(iter.cast()) };
        } else {
            // SAFETY: this `unsafe fn`'s caller passes `iter` a live lazy iterator.
            unsafe { crate::c_abi::lazy_iter_retain(iter.cast()) };
        }
    }
}

/// The lazy iterator shims are native-only, so on wasm no value holds a lazy
/// iterator child and there is no share to take or give up.
#[cfg(target_arch = "wasm32")]
pub(crate) mod lazy_children {
    pub(crate) unsafe fn drop_share(_iter: *mut u8, _pair: bool) {}

    pub(crate) unsafe fn retain(_iter: *mut u8, _pair: bool) {}
}

/// Enter a teardown frame (release walk or collection slice).
fn teardown_enter() {
    TEARDOWN_DEPTH.with(|d| d.set(d.get() + 1));
}

/// Exit a teardown frame; at depth 0 drain the queued Vec frees. Each
/// free is popped before running so a cascade that re-enters the release
/// path (and queues or drains more) observes a consistent queue.
unsafe fn teardown_exit() {
    let depth = TEARDOWN_DEPTH.with(|d| {
        let next = d.get().saturating_sub(1);
        d.set(next);
        next
    });
    if depth != 0 {
        return;
    }
    loop {
        let next = PENDING_VEC_FREES.with(|q| q.borrow_mut().pop());
        let Some(v) = next else { break };
        // SAFETY: each queued vec is one a dead node owned alone.
        unsafe { crate::c_abi::map::gos_rt_vec_free(v.cast()) };
    }
    loop {
        let next = PENDING_MAP_FREES.with(|q| q.borrow_mut().pop());
        let Some(m) = next else { break };
        // SAFETY: each queued map is one a dead node owned alone.
        unsafe { crate::c_abi::map::gos_rt_map_free(m.cast()) };
    }
    loop {
        let next = PENDING_SET_FREES.with(|q| q.borrow_mut().pop());
        let Some(s) = next else { break };
        // SAFETY: each queued set is one a dead node owned alone.
        unsafe { crate::c_abi::map::gos_rt_set_free(s.cast()) };
    }
    loop {
        let next = PENDING_DEQUE_FREES.with(|q| q.borrow_mut().pop());
        let Some(d) = next else { break };
        // SAFETY: each queued deque is one a dead node owned alone.
        unsafe { crate::c_abi::deque::gos_rt_deque_free(d.cast()) };
    }
    loop {
        let next = PENDING_ITER_FREES.with(|q| q.borrow_mut().pop());
        let Some((iter, pair)) = next else { break };
        // SAFETY: each queued iterator carries the share a dead node held.
        unsafe { drop_iter_child(iter, pair) };
    }
    loop {
        let next = PENDING_FINALIZERS.with(|q| q.borrow_mut().pop());
        let Some(finalize) = next else { break };
        finalize();
    }
}

/// Call `f` for each non-null RC-pointer child of `payload`, per its
/// type-meta blob. Walks the flat `[i64]` blob documented above. The single
/// edge-traversal primitive shared by the RC release walk, the cycle
/// collector's trial-deletion, and the GC mark - one edge map, three
/// consumers.
unsafe fn visit_rc_children(payload: *mut u8, mut f: impl FnMut(*mut u8)) {
    // Type metas list every heap child a node owns, and an enum or
    // struct can own a *String* child - whose allocation carries the
    // string tag header, not an `RcHeader`. Feeding one to the count /
    // color machinery reads garbage, so the RC-graph walk yields only
    // RC-headered children; the release path reclaims string children
    // through [`visit_string_children`].
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_children_raw(payload, |child| {
            // A tagged nullary enum is non-null in its stored representation
            // (for example `Tree::Nil` is `0x2`) but has no allocation behind
            // it.  The cycle collector dereferences every graph edge, so it
            // must not receive the untagged null value.
            if !child.is_null() && !crate::c_abi::string::is_gos_string(child.cast()) {
                f(child);
            }
        });
    }
}

unsafe fn visit_children_raw(payload: *mut u8, mut raw_f: impl FnMut(*mut u8)) {
    // Child words may carry tagged-repr enum pointers; consumers work
    // on payload bases (strings stay odd and untouched). Only kind-0
    // (RC-node / String) entries reach the callback - container children
    // (`RC_CHILD_VEC`) are not RC nodes, so the count / color machinery
    // must never touch them; the teardown paths walk those separately
    // through [`visit_vec_children`].
    let mut f = |c: *mut u8| raw_f(untag_rc(c));
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entries(payload, |kind, child| {
            if kind == gossamer_abi::rc::RC_CHILD_RC {
                f(child);
            }
        });
    }
}

/// Replaces every owned `Map`, `Set`, deque, and heap child of `payload` with
/// one of its own, and takes a share of every lazy iterator child. A map, a
/// set, and a deque carry no reference count, and a heap is written in place,
/// so a copy that kept the source's handle would leave one store under two
/// owners; an iterator's holders advance one cursor, so a copy shares it.
unsafe fn clone_map_children(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entry_slots(payload, |kind, slot, child| {
            if slot.is_null() {
                return;
            }
            let cloned: *mut u8 = match kind {
                gossamer_abi::rc::RC_CHILD_ITER => {
                    lazy_children::retain(child, false);
                    return;
                }
                gossamer_abi::rc::RC_CHILD_ITER_PAIR => {
                    lazy_children::retain(child, true);
                    return;
                }
                gossamer_abi::rc::RC_CHILD_MAP => {
                    crate::c_abi::gos_rt_map_clone(child.cast()).cast()
                }
                gossamer_abi::rc::RC_CHILD_SET => {
                    crate::c_abi::set::gos_rt_set_clone(child.cast()).cast()
                }
                gossamer_abi::rc::RC_CHILD_DEQUE => {
                    crate::c_abi::deque::gos_rt_deque_clone(child.cast()).cast()
                }
                gossamer_abi::rc::RC_CHILD_HEAP => {
                    crate::c_abi::gos_rt_vec_clone(child.cast()).cast()
                }
                _ => return,
            };
            crate::c_abi::vec::slot_write_word(slot, cloned);
        });
    }
}

/// Walks the children an element copy's slot-children meta names, yielding
/// each as the child kind the structural walks dispatch on.
unsafe fn visit_slot_children_meta(
    payload: *mut u8,
    meta: *const i64,
    mut f: impl FnMut(i64, *mut u8, *mut u8),
) {
    use crate::c_abi::vec::vec_elem_kind;
    use gossamer_abi::rc::{RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_SET, RC_CHILD_VEC};
    // SAFETY: this `unsafe fn`'s caller passes `meta` a slot-children table, whose second word is
    // its entry count.
    let count = usize::try_from(unsafe { *meta.add(1) }).unwrap_or(0);
    for i in 0..count {
        // SAFETY: `i` is below the entry count, inside the table.
        let entry = unsafe { meta.add(2 + i * 4) };
        let (gate, disc_word, word, child_kind) =
            // SAFETY: each entry is four words.
            unsafe { (*entry, *entry.add(1), *entry.add(2), *entry.add(3)) };
        if gate >= 0 {
            // SAFETY: `disc_word` names a word inside the payload.
            let disc = unsafe {
                payload
                    .add(usize::try_from(disc_word).unwrap_or(0) * 8)
                    .cast::<i64>()
                    .read_unaligned()
            };
            if disc != gate {
                continue;
            }
        }
        let kind = match u8::try_from(child_kind) {
            Ok(vec_elem_kind::STRING | vec_elem_kind::RC_NODE) => RC_CHILD_RC,
            Ok(vec_elem_kind::VEC) => RC_CHILD_VEC,
            Ok(vec_elem_kind::MAP) => RC_CHILD_MAP,
            Ok(vec_elem_kind::SET) => RC_CHILD_SET,
            Ok(vec_elem_kind::DEQUE) => gossamer_abi::rc::RC_CHILD_DEQUE,
            Ok(vec_elem_kind::HEAP) => gossamer_abi::rc::RC_CHILD_HEAP,
            Ok(vec_elem_kind::ITER) => gossamer_abi::rc::RC_CHILD_ITER,
            Ok(vec_elem_kind::ITER_PAIR) => gossamer_abi::rc::RC_CHILD_ITER_PAIR,
            Ok(vec_elem_kind::WEAK) => gossamer_abi::rc::RC_CHILD_WEAK,
            _ => continue,
        };
        // SAFETY: each entry names a word inside the payload.
        let slot = unsafe { payload.add(usize::try_from(word).unwrap_or(0) * 8) };
        // SAFETY: `slot` is that word.
        let child = unsafe { crate::c_abi::vec::slot_read_word(slot) };
        if !child.is_null() {
            f(kind, slot, child);
        }
    }
}

/// Call `f` for each non-null `RC_CHILD_VEC` child of `payload` - a
/// `*mut GosVec` the node owns (the constructor retained the node's
/// share). Teardown frees these through `gos_rt_vec_free`; co-owning
/// paths (copy, match-binding materialisation) retain them.
///
/// The teardown paths reach every child kind through
/// [`release_children_into`] instead, in one pass; this stays for the copy
/// and collection paths, which want one kind at a time.
unsafe fn visit_vec_children(payload: *mut u8, mut f: impl FnMut(*mut u8)) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe {
        visit_entries(payload, |kind, child| {
            if kind == gossamer_abi::rc::RC_CHILD_VEC {
                f(child);
            }
        });
    }
}

/// Walk `payload`'s meta child entries, yielding `(kind, child_ptr)` for
/// each non-null child word. Entries pack the payload word index in the
/// low 32 bits and the child kind above (`gossamer_abi::rc`); guarded
/// metas keep their dedicated pair walk and yield kind 0.
unsafe fn visit_entries(payload: *mut u8, mut f: impl FnMut(i64, *mut u8)) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    unsafe { visit_entry_slots(payload, |kind, _slot, child| f(kind, child)) };
}

/// Same walk as [`visit_entries`], but the callback also receives the address
/// of the payload word holding the child. A kind whose copy takes a value of
/// its own - [`gossamer_abi::rc::RC_CHILD_MAP`] - writes the new handle back
/// through that address.
unsafe fn visit_entry_slots(payload: *mut u8, mut f: impl FnMut(i64, *mut u8, *mut u8)) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    // SAFETY: `meta` is non-null (checked above), the node's meta table.
    let kind = unsafe { *meta };
    if kind == RC_KIND_STRUCT_GUARDED {
        // SAFETY: `payload` is laid out as `meta` describes.
        unsafe {
            visit_guarded_children(payload, meta, |c| {
                f(gossamer_abi::rc::RC_CHILD_RC, std::ptr::null_mut(), c);
            });
        };
        return;
    }
    if kind == gossamer_abi::rc::RC_KIND_SLOT_CHILDREN {
        // SAFETY: `payload` is laid out as `meta` describes.
        unsafe { visit_slot_children_meta(payload, meta, f) };
        return;
    }
    // Only Enum and Struct carry child layouts today. String / Vec / Map
    // / Closure layouts are wired in a later phase and never reach here.
    if kind != RC_KIND_ENUM && kind != RC_KIND_STRUCT {
        return;
    }
    let target_disc = if kind == RC_KIND_ENUM {
        // SAFETY: this `unsafe fn`'s caller passes `payload` a live node.
        i64::from(unsafe { (*header_ptr(payload)).disc })
    } else {
        0
    };
    // SAFETY: `payload` is laid out as `meta` describes.
    unsafe { visit_layout_entry_slots(payload, meta, target_disc, f) };
}

/// Walks the child entries an `RC_KIND_ENUM` / `RC_KIND_STRUCT` layout names
/// over the words at `payload`, for the record `target_disc` selects (a struct
/// layout has one record and ignores it).
unsafe fn visit_layout_entry_slots(
    payload: *mut u8,
    meta: *const i64,
    target_disc: i64,
    mut f: impl FnMut(i64, *mut u8, *mut u8),
) {
    use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_WORD_MASK};
    // SAFETY: this `unsafe fn`'s caller passes `meta` a live meta table and `payload` laid out as
    // it describes.
    let kind = unsafe { *meta };
    // SAFETY: a meta table's second word is its variant count.
    let variant_count = unsafe { *meta.add(1) };
    let mut idx: usize = 2;
    for _ in 0..variant_count.max(0) {
        // SAFETY: `idx` stays inside the meta table the variant count describes.
        let disc = unsafe { *meta.add(idx) };
        // SAFETY: `idx + 1` stays inside the meta table the variant count describes.
        let child_count = unsafe { *meta.add(idx + 1) };
        let matches = kind == RC_KIND_STRUCT || disc == target_disc;
        if matches {
            for j in 0..child_count.max(0) {
                // SAFETY: the child entries follow the variant header inside the meta table.
                let entry = unsafe { *meta.add(idx + 2 + j as usize) };
                let child_kind = entry >> RC_CHILD_KIND_SHIFT;
                let word = entry & RC_CHILD_WORD_MASK;
                // Aggregate slots cross the C ABI as pointer-sized integer
                // words. Reconstruct exposed provenance explicitly instead
                // of treating those integer bits as a Rust pointer load.
                // SAFETY: each child entry names a word inside the payload.
                let slot = unsafe { payload.add((word as usize) * 8) };
                // SAFETY: `slot` is that word.
                let child = unsafe { crate::c_abi::vec::slot_read_word(slot) };
                if child.is_null() {
                    continue;
                }
                match gossamer_abi::rc::rc_child_blob_gate(child_kind) {
                    // A carrier field's payload word names a copy blob only on
                    // the arm the gate names, so the carrier's discriminant,
                    // the word before the payload, is read first.
                    Some(gate) => {
                        // SAFETY: a gated carrier child's discriminant is the word before its
                        // payload.
                        let disc = unsafe { slot.cast::<i64>().sub(1).read_unaligned() };
                        // SAFETY: `child` is the carrier's payload word.
                        if (gate < 0 || disc == gate) && unsafe { is_copy_blob(child) } {
                            f(gossamer_abi::rc::RC_CHILD_RC, slot, child);
                        }
                    }
                    None => f(child_kind, slot, child),
                }
            }
            return;
        }
        idx += 2 + child_count.max(0) as usize;
    }
}

/// Free an RC block's underlying allocation. Called when the block is no
/// longer observed by any strong *or* weak reference. The payload's children
/// must already have been released (at the strong→0 transition). The byte
/// size is recovered from the header's `size_u` (or the oversized side table).
#[inline]
unsafe fn free_block(payload: *mut u8) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` a node with no references left.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is that node's header.
    if unsafe { load_strong(h) } & SHARED_BIT != 0 {
        // A shared object is being reclaimed: keep the live-shared diagnostic
        // count in step. (A shared cycle never reaches here, which is exactly
        // what the non-zero exit count surfaces.)
        rc_shared_dec();
    }
    // SAFETY: `h` is that node's header.
    let base = unsafe { block_base(h) };
    rc_live_dec();
    // Straight back to mimalloc - see `gos_rt_rc_alloc` for why a custom
    // slab/pool is not used (measured net-neutral), and
    // `rc_block_alloc_zeroed` for why the call is direct.
    // SAFETY: `base` is the node's block, which nothing references.
    unsafe { rc_block_free(base) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct CountsDrops(&'static std::sync::atomic::AtomicUsize);

    impl Drop for CountsDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    managed_handle!(CountsDrops);

    #[test]
    fn a_managed_node_is_dropped_once_with_its_last_share() {
        static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let node = alloc_managed(CountsDrops(&DROPS)).cast::<u8>();
        // SAFETY: `node` is the live managed node built above; each release gives back one of
        // the two shares it holds.
        unsafe {
            gos_rt_rc_retain(node);
            gos_rt_rc_release(node);
            assert_eq!(DROPS.load(Ordering::SeqCst), 0, "a share is still held");
            gos_rt_rc_release(node);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn region_arena_rejects_every_pointer_when_no_arena_reserved() {
        // Regression: with the uninitialised base (0) the range test must
        // report NO pointer as region memory - even ones below the 64 GiB
        // reserve size. The old bare `wrapping_sub` classified every low
        // heap pointer as in-region, which neutralised RC retain/release on
        // platforms whose allocator hands out low addresses (Windows),
        // producing use-after-free in any handler that retains a heap value.
        assert!(!addr_in_region_arena(0x1000, 0));
        assert!(!addr_in_region_arena(REGION_ARENA_BYTES - 1, 0));
        assert!(!addr_in_region_arena(0xdead_beef, 0));
        // Failed-reservation sentinel disables regions just the same.
        assert!(!addr_in_region_arena(0x2000, usize::MAX));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn region_arena_matches_only_the_reserved_window() {
        let base = 0x7000_0000_0000usize;
        assert!(addr_in_region_arena(base, base));
        assert!(addr_in_region_arena(base + REGION_ARENA_BYTES - 1, base));
        assert!(!addr_in_region_arena(base - 1, base));
        assert!(!addr_in_region_arena(base + REGION_ARENA_BYTES, base));
        // A pointer below the reserve (the Windows-heap shape) is outside it.
        assert!(!addr_in_region_arena(0x1_0000, base));
    }

    #[test]
    fn arena_overflow_recycler_reuses_offsets_without_a_lock() {
        let recycler = ArenaFreeSlabs::<4>::new();
        assert_eq!(recycler.pop(), None, "new recycler is empty");
        assert!(recycler.push(0));
        assert!(recycler.push(3));
        assert_eq!(recycler.pop(), Some(3), "last retired slab is reused first");
        assert_eq!(recycler.pop(), Some(0));
        assert_eq!(recycler.pop(), None);
        assert!(!recycler.push(4), "out-of-range offsets are rejected");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "40k cross-thread contention rounds are covered by native/TSan; Miri scheduling is prohibitively slow"
    )]
    fn arena_overflow_recycler_does_not_duplicate_a_slab_under_contention() {
        const SLOTS: usize = 8;
        const WORKERS: usize = 4;
        const ROUNDS: usize = 10_000;
        let recycler = Arc::new(ArenaFreeSlabs::<SLOTS>::new());
        let in_use: Arc<[AtomicUsize; SLOTS]> =
            Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
        for index in 0..SLOTS {
            assert!(recycler.push(index));
        }

        let mut workers = Vec::new();
        for _ in 0..WORKERS {
            let recycler = Arc::clone(&recycler);
            let in_use = Arc::clone(&in_use);
            workers.push(std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let index = loop {
                        if let Some(index) = recycler.pop() {
                            break index;
                        }
                        std::thread::yield_now();
                    };
                    assert_eq!(
                        in_use[index].compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire),
                        Ok(0),
                        "an ABA race handed one slab to two workers"
                    );
                    in_use[index].store(0, Ordering::Release);
                    assert!(recycler.push(index));
                }
            }));
        }
        for worker in workers {
            worker
                .join()
                .expect("overflow recycler worker must not panic");
        }
        let mut recovered = [false; SLOTS];
        for _ in 0..SLOTS {
            let index = recycler.pop().expect("every retired slab is recovered");
            assert!(!recovered[index], "slab appears in the recycler twice");
            recovered[index] = true;
        }
        assert!(recovered.into_iter().all(std::convert::identity));
        assert_eq!(recycler.pop(), None);
    }

    fn count_guard() -> RcLiveCountGuard {
        rc_live_count_guard()
    }

    /// A nested pop hands the allocator back to the parent region, so an
    /// allocation made after it lands on a slab that outlives the pop rather
    /// than on one the pop is retiring.
    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn arena_pop_resumes_the_parent_region_before_retiring_slabs() {
        let _guard = count_guard();
        gos_rt_arena_push();
        if !region_is_active() {
            return;
        }
        let outer = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(in_region_arena(outer));

        gos_rt_arena_push();
        let inner = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(in_region_arena(inner));
        gos_rt_arena_pop();

        assert!(region_is_active(), "the parent region is still open");
        let after = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(
            in_region_arena(after),
            "an allocation after the nested pop belongs to the parent region"
        );
        assert!(
            in_region_arena(outer),
            "the parent's own object is untouched"
        );
        gos_rt_arena_pop();
        assert!(!region_is_active());
    }

    /// A standard slab starts on a slab boundary whatever was carved before
    /// it, which is what lets its offset name its index in the free list: two
    /// slabs under one index would each be recycled onto the other's memory.
    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn a_carve_leaves_the_cursor_on_a_slab_boundary() {
        let base = region_arena_base();
        if base == usize::MAX {
            return;
        }
        let oversized = arena_acquire(REGION_SLAB_BYTES + os_page_size());
        if oversized.is_null() {
            return;
        }
        let standard = arena_acquire(REGION_SLAB_BYTES);
        assert!(!standard.is_null(), "the arena still has room for a slab");
        assert_eq!(
            (standard as usize - base) % REGION_SLAB_BYTES,
            0,
            "a standard slab starts on a slab boundary"
        );
        // SAFETY: `standard` is the slab acquired above, which nothing uses after this.
        unsafe { arena_retire(standard, REGION_SLAB_BYTES) };
        // SAFETY: `oversized` is the slab acquired above, which nothing uses after this.
        unsafe { arena_retire(oversized, REGION_SLAB_BYTES + os_page_size()) };
    }

    /// A pop with no region open still balances the depth it was called at.
    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn arena_pop_without_a_region_leaves_no_region_active() {
        let _guard = count_guard();
        gos_rt_arena_pop();
        assert!(!region_is_active());
        gos_rt_arena_push();
        if !region_is_active() {
            return;
        }
        gos_rt_arena_pop();
        assert!(!region_is_active());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn suspended_arena_state_moves_with_its_coroutine() {
        let _guard = count_guard();
        gos_rt_arena_push();
        if !region_is_active() {
            // Targets without virtual-memory reservations intentionally fall
            // back to ordinary RC allocation and have no arena state to move.
            return;
        }
        let ptr = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(in_region_arena(ptr));

        let state = take_arena_state();
        assert!(
            !region_is_active(),
            "a yielded worker must not retain the suspended handler arena"
        );
        install_arena_state(state);
        assert!(region_is_active());
        assert!(
            in_region_arena(ptr),
            "the resumed handler must retain ownership of its existing slab"
        );
        gos_rt_arena_pop();
        assert!(!region_is_active());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn request_arena_guard_closes_unbalanced_handler_regions() {
        let _guard = count_guard();
        let request = RequestArenaGuard::new();
        gos_rt_arena_push();
        if !region_is_active() {
            drop(request);
            return;
        }
        let ptr = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(in_region_arena(ptr));
        drop(request);
        assert!(
            !region_is_active(),
            "timeout/cancellation cleanup must not retain a handler region"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn dropped_suspended_arena_state_reclaims_handler_regions() {
        let _guard = count_guard();
        gos_rt_arena_push();
        if !region_is_active() {
            return;
        }
        let ptr = crate::c_abi::gc::gos_rt_gc_alloc(64);
        assert!(in_region_arena(ptr));
        let state = take_arena_state();
        drop(state);
        assert!(
            !region_is_active(),
            "cancelling a parked handler must release its detached arena"
        );
    }

    /// Allocate via the runtime entry and write the discriminant into
    /// the header byte (mirroring `gos_enum_set_disc`).
    unsafe fn alloc_with_disc(payload_words: usize, disc: i64, meta: *const i64) -> *mut u8 {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let p = unsafe { gos_rt_rc_alloc((payload_words * 8) as u64, meta) };
        assert!(!p.is_null());
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { (*header_ptr(p)).disc = u8::try_from(disc).unwrap_or(0) };
        p
    }

    unsafe fn set_child(parent: *mut u8, word: usize, child: *mut u8) {
        // The walks read a child slot as an integer word with its provenance
        // exposed, so the slot is written the same way.
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::vec::slot_write_word(parent.add(word * 8), child) };
    }

    unsafe fn strong_of(payload: *mut u8) -> usize {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { strong_count(header_ptr(payload)) as usize }
    }

    // Struct node with one i64 field (word 0) and one RC-pointer child
    // (word 1): kind=STRUCT, V=1, [disc0 cc1 off1].
    fn node_meta() -> Vec<i64> {
        vec![RC_KIND_STRUCT, 1, 0, 1, 1]
    }

    /// Set `parent`'s child slot (word 1) to `child` and retain it, as the
    /// compiled tier's `gos_store` does when an object gains a child edge.
    unsafe fn link(parent: *mut u8, child: *mut u8) {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { set_child(parent, 1, child) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_rc_retain(child) };
    }

    /// Move `child` into `parent`'s slot: set the edge without a retain,
    /// transferring the existing reference (the unique-ownership shape, like
    /// `Node { child: existing_b }` where `b` is used once and not aliased).
    unsafe fn move_child(parent: *mut u8, child: *mut u8) {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { set_child(parent, 1, child) };
    }

    /// Drain any candidates left buffered by earlier tests on this thread so
    /// each cycle test starts from a clean candidate buffer (`ROOTS` is
    /// thread-local and tests share worker threads).
    fn fresh_cycle_state() {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_collect_cycles() };
    }

    #[test]
    fn two_node_cycle_is_collected() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let freed_base = rc_cycles_freed();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            let b = gos_rt_rc_alloc(16, meta.as_ptr());
            link(a, b);
            link(b, a);
            // Drop both external handles: a pure-RC heap now leaks the cycle.
            gos_rt_rc_release(a);
            gos_rt_rc_release(b);
            assert_eq!(rc_live_count(), base + 2, "cycle leaks under plain RC");
            gos_rt_collect_cycles();
        }
        assert_eq!(rc_live_count(), base, "cycle collector reclaims the cycle");
        assert_eq!(rc_cycles_freed(), freed_base + 2);
    }

    #[test]
    fn self_cycle_is_collected() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            link(a, a);
            gos_rt_rc_release(a);
            assert_eq!(rc_live_count(), base + 1);
            gos_rt_collect_cycles();
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn externally_referenced_cycle_survives_collection() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            let b = gos_rt_rc_alloc(16, meta.as_ptr());
            link(a, b);
            link(b, a);
            // An outside owner keeps `a` (and transitively the cycle) live.
            gos_rt_rc_retain(a);
            gos_rt_rc_release(a);
            gos_rt_rc_release(b);
            gos_rt_collect_cycles();
            assert_eq!(rc_live_count(), base + 2, "live cycle is not collected");
            assert_eq!(strong_of(a), 2, "counts restored after trial deletion");
            assert_eq!(strong_of(b), 1);
            // Drop the external owner: now it is garbage and collectable.
            gos_rt_rc_release(a);
            gos_rt_collect_cycles();
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn uniquely_owned_acyclic_release_never_buffers() {
        let _g = count_guard();
        fresh_cycle_state();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let b = gos_rt_rc_alloc(16, meta.as_ptr());
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            // `a` uniquely owns `b` (the reference was moved in, not aliased):
            // dropping `a` frees both straight at count 0, so neither ever
            // survives a decrement and nothing is buffered - the collector
            // stays a no-op on the unique-ownership (benchmark) shape.
            move_child(a, b);
            gos_rt_rc_release(a);
            let buffered = ROOTS.with(|r| r.borrow().len());
            assert_eq!(buffered, 0, "unique-ownership drop must not buffer");
        }
    }

    #[test]
    fn budgeted_collection_processes_a_slice_and_leaves_the_rest() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let freed_base = rc_cycles_freed();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // Five independent two-node cycles -> ten buffered candidates.
            for _ in 0..5 {
                let a = gos_rt_rc_alloc(16, meta.as_ptr());
                let b = gos_rt_rc_alloc(16, meta.as_ptr());
                link(a, b);
                link(b, a);
                gos_rt_rc_release(a);
                gos_rt_rc_release(b);
            }
            assert_eq!(
                rc_live_count(),
                base + 10,
                "five cycles leak under plain RC"
            );
            assert_eq!(
                ROOTS.with(|r| r.borrow().len()),
                10,
                "ten candidates buffered"
            );

            // A bounded slice of four candidates reclaims exactly the two
            // cycles it covers and leaves the other six buffered.
            collect_cycles_budgeted(Some(4));
            assert_eq!(rc_cycles_freed(), freed_base + 4, "slice frees its 4 nodes");
            assert_eq!(rc_live_count(), base + 6, "six nodes still live");
            assert_eq!(ROOTS.with(|r| r.borrow().len()), 6, "six candidates remain");

            // An explicit full collection drains everything that is left.
            gos_rt_collect_cycles();
            assert_eq!(rc_live_count(), base, "remaining cycles reclaimed");
            assert_eq!(ROOTS.with(|r| r.borrow().len()), 0, "buffer drained");
        }
    }

    #[test]
    fn budgeted_slices_eventually_collect_all_cycles() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            for _ in 0..8 {
                let a = gos_rt_rc_alloc(16, meta.as_ptr());
                let b = gos_rt_rc_alloc(16, meta.as_ptr());
                link(a, b);
                link(b, a);
                gos_rt_rc_release(a);
                gos_rt_rc_release(b);
            }
            // Repeated small slices reclaim the whole backlog with no leftover.
            let mut guard = 0;
            while ROOTS.with(|r| !r.borrow().is_empty()) {
                collect_cycles_budgeted(Some(3));
                guard += 1;
                assert!(
                    guard < 100,
                    "slices must drain the buffer, not loop forever"
                );
            }
            assert_eq!(rc_live_count(), base, "all 8 cycles reclaimed via slices");
        }
    }

    #[test]
    fn drop_reuse_recycles_unique_block_in_place() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // A uniquely-owned leaf: drop_reuse hands back its block.
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            let block = gos_rt_rc_drop_reuse(a);
            assert!(!block.is_null(), "unique block is offered for reuse");
            assert_eq!(
                rc_live_count(),
                base + 1,
                "reuse keeps the slot live (no free)"
            );
            // alloc_reuse re-homes the SAME block (same address, reset header).
            let b = gos_rt_rc_alloc_reuse(block, 16, meta.as_ptr());
            assert_eq!(b, a, "reuse returns the recycled block, no new allocation");
            assert_eq!(strong_of(b), 1, "reused block starts at strong 1");
            assert_eq!(rc_live_count(), base + 1, "still exactly one live object");
            gos_rt_rc_release(b);
            assert_eq!(rc_live_count(), base, "released after reuse");
        }
    }

    #[test]
    fn drop_reuse_declines_shared_object() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            // Mark shared (escaped to a goroutine): must NOT be reused.
            gos_rt_rc_mark_shared(a);
            let block = gos_rt_rc_drop_reuse(a);
            assert!(
                block.is_null(),
                "shared object is freed normally, never reused"
            );
            assert_eq!(rc_live_count(), base, "shared drop frees the block");
        }
    }

    #[test]
    fn drop_reuse_releases_children_then_recycles_parent() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // parent uniquely owns child via word 1.
            let child = gos_rt_rc_alloc(16, meta.as_ptr());
            let parent = gos_rt_rc_alloc(16, meta.as_ptr());
            move_child(parent, child);
            assert_eq!(rc_live_count(), base + 2);
            // Reusing parent must free the child (cascade) but keep parent's block.
            let block = gos_rt_rc_drop_reuse(parent);
            assert_eq!(
                block,
                header_ptr(parent) as *mut u8,
                "parent block returned"
            );
            assert_eq!(rc_live_count(), base + 1, "child freed, parent slot kept");
            let fresh = gos_rt_rc_alloc_reuse(block, 16, meta.as_ptr());
            assert_eq!(fresh, parent, "parent block reused");
            gos_rt_rc_release(fresh);
            assert_eq!(rc_live_count(), base, "all reclaimed");
        }
    }

    #[test]
    fn alloc_reuse_null_token_allocates_fresh() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc_reuse(std::ptr::null_mut(), 16, meta.as_ptr());
            assert!(!p.is_null());
            assert_eq!(rc_live_count(), base + 1, "null token allocates fresh");
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base);
        }
    }

    #[test]
    fn allocation_telemetry_separates_payload_header_and_reuse() {
        let before = crate::c_abi::ledger::rc_alloc_stats();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let first = gos_rt_rc_alloc(16, std::ptr::null());
            assert!(!first.is_null());
            let token = gos_rt_rc_drop_reuse(first);
            assert!(!token.is_null());
            let reused = gos_rt_rc_alloc_reuse(token, 16, std::ptr::null());
            assert!(!reused.is_null());
            gos_rt_rc_release(reused);
        }
        let after = crate::c_abi::ledger::rc_alloc_stats();
        assert!(after.0 > before.0, "fresh allocation must be counted");
        assert!(after.2 > before.2, "Perceus reuse must be counted");
        assert!(
            after.3 >= before.3 + 32,
            "both payload requests are retained"
        );
        let heap_or_reuse = (after.0 - before.0) + (after.2 - before.2);
        assert!(
            after.4 - before.4 >= after.3 - before.3 + heap_or_reuse * RC_HEADER_SIZE as u64,
            "usable storage includes the fixed RC header"
        );
    }

    #[test]
    fn drop_reuse_declines_aliased_object() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let a = gos_rt_rc_alloc(16, meta.as_ptr());
            gos_rt_rc_retain(a); // a second owner: not unique
            let block = gos_rt_rc_drop_reuse(a);
            assert!(block.is_null(), "still-referenced object is not reused");
            assert_eq!(
                rc_live_count(),
                base + 1,
                "object survives (one owner left)"
            );
            gos_rt_rc_release(a);
            assert_eq!(rc_live_count(), base);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn region_allocs_are_freed_wholesale_at_pop() {
        let _g = count_guard();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_arena_push();
            assert!(region_active());
            let mut ptrs = Vec::new();
            for _ in 0..1000 {
                let p = gos_rt_rc_alloc(16, meta.as_ptr());
                assert!(!p.is_null());
                assert!(is_region(header_ptr(p)), "region alloc must tag REGION_BIT");
                ptrs.push(p);
            }
            assert_eq!(rc_live_count(), base + 1000);
            // retain/release on region objects are no-ops: they neither free
            // nor clobber the REGION bit.
            gos_rt_rc_retain(ptrs[0]);
            gos_rt_rc_release(ptrs[0]);
            assert!(is_region(header_ptr(ptrs[0])));
            assert_eq!(
                rc_live_count(),
                base + 1000,
                "region objects not freed early"
            );
            gos_rt_arena_pop();
            assert!(!region_active());
        }
        assert_eq!(rc_live_count(), base, "pop frees the whole region");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn region_tree_freed_without_per_node_teardown() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_arena_push();
            // Build a parent that owns a child entirely inside the region.
            let child = gos_rt_rc_alloc(16, meta.as_ptr());
            let parent = gos_rt_rc_alloc(16, meta.as_ptr());
            move_child(parent, child);
            // "Consume" the parent (count would hit zero for a heap object,
            // triggering a teardown walk). For a region object this is a
            // no-op - the child is NOT freed here.
            gos_rt_rc_release(parent);
            assert_eq!(rc_live_count(), base + 2, "region release must not free");
            let buffered = ROOTS.with(|r| r.borrow().len());
            assert_eq!(buffered, 0, "region objects never enter the cycle buffer");
            gos_rt_arena_pop();
        }
        assert_eq!(
            rc_live_count(),
            base,
            "pop reclaims parent + child together"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // arena uses mmap with non-RW protections; Miri can't model it
    fn region_oversized_alloc_gets_its_own_slab() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_arena_push();
            // Larger than the default slab - must still allocate, on its own slab.
            let big = gos_rt_rc_alloc((REGION_SLAB_BYTES as u64) * 2, std::ptr::null());
            assert!(!big.is_null());
            assert!(is_region(header_ptr(big)));
            gos_rt_arena_pop();
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn shared_acyclic_node_is_not_collected_while_referenced() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // Two parents share one child (a diamond, no cycle). The child is
            // owned only by the parents (its construction handle is released).
            let child = gos_rt_rc_alloc(16, meta.as_ptr());
            let p1 = gos_rt_rc_alloc(16, meta.as_ptr());
            let p2 = gos_rt_rc_alloc(16, meta.as_ptr());
            link(p1, child);
            link(p2, child);
            gos_rt_rc_release(child);
            // Dropping one parent decrements the shared child to a non-zero
            // count → buffered as a candidate, but it is live and survives.
            gos_rt_rc_release(p1);
            gos_rt_collect_cycles();
            assert_eq!(rc_live_count(), base + 2, "shared live child survives");
            assert_eq!(strong_of(child), 1);
            // Drop the last parent: child reaches 0 (deferred while buffered),
            // reclaimed at the next collection.
            gos_rt_rc_release(p2);
            gos_rt_collect_cycles();
        }
        assert_eq!(rc_live_count(), base);
    }

    unsafe fn weak_of(payload: *mut u8) -> usize {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { (*header_ptr(payload)).weak.load(Ordering::Relaxed) as usize }
    }

    #[test]
    fn downgrade_increments_weak_not_strong() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            let w = gos_rt_rc_downgrade(p);
            assert_eq!(w, p, "downgrade returns the same payload pointer");
            assert_eq!(strong_of(p), 1);
            assert_eq!(weak_of(p), 1);
            // Strong release destroys the payload but the block lingers
            // because a weak ref still observes it.
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base + 1, "block lingers while weak > 0");
            assert_eq!(strong_of(p), 0);
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base, "block freed once weak hits 0");
    }

    #[test]
    fn upgrade_while_alive_returns_payload_and_bumps_strong() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            let up = gos_rt_rc_weak_upgrade(p);
            assert_eq!(up, p, "upgrade of a live referent yields the payload");
            assert_eq!(strong_of(p), 2, "upgrade adds a strong ref");
            gos_rt_rc_release(p);
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base + 1, "lingers on the weak ref");
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn upgrade_opt_packs_some_when_alive_taking_a_strong_reference() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            let opt = gos_rt_rc_weak_upgrade_opt(p);
            assert_eq!(
                crate::c_abi::result::gos_rt_result_disc(opt),
                0,
                "alive referent is Some"
            );
            assert_eq!(
                crate::c_abi::result::gos_rt_result_payload(opt),
                p as i64,
                "Some carries the payload pointer"
            );
            assert_eq!(
                strong_of(p),
                2,
                "upgrade_opt takes a fresh strong reference for the Some payload"
            );
            // The shadow local's scope-end release balances the take.
            gos_rt_rc_release(p);
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base + 1, "lingers on the weak ref");
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn upgrade_opt_boxes_none_after_last_strong_release() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            gos_rt_rc_release(p);
            let opt = gos_rt_rc_weak_upgrade_opt(p);
            assert_eq!(
                crate::c_abi::result::gos_rt_result_disc(opt),
                1,
                "dead referent is None"
            );
            assert_eq!(crate::c_abi::result::gos_rt_result_payload(opt), 0);
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn upgrade_after_last_strong_release_returns_null() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base + 1);
            let up = gos_rt_rc_weak_upgrade(p);
            assert!(up.is_null(), "upgrade of a dead referent yields null");
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn allocation_lingers_until_weak_count_zero() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            gos_rt_rc_weak_retain(p);
            assert_eq!(weak_of(p), 2);
            gos_rt_rc_release(p);
            assert_eq!(rc_live_count(), base + 1);
            gos_rt_rc_weak_release(p);
            assert_eq!(rc_live_count(), base + 1, "still one outstanding weak");
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    /// The exact weak count of `payload`, wherever it lives.
    unsafe fn weak_count(payload: *mut u8) -> usize {
        // SAFETY: the caller passes a block its weak or strong share keeps allocated.
        let h = unsafe { header_ptr(payload) };
        // SAFETY: as above.
        let byte = unsafe { weak_of(payload) };
        if byte == usize::from(WEAK_OVERFLOW) {
            WEAK_OVERFLOW_COUNTS
                .lock()
                .get(&(h as usize))
                .map_or(byte, |c| *c as usize)
        } else {
            byte
        }
    }

    #[test]
    fn weak_counts_past_the_header_byte_stay_exact_and_reclaim() {
        let _g = count_guard();
        let base = rc_live_count();
        for weaks in [253usize, 254, 255, 256, 300, 10_000] {
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe {
                let p = gos_rt_rc_alloc(8, std::ptr::null());
                for _ in 0..weaks {
                    gos_rt_rc_weak_retain(p);
                }
                assert_eq!(weak_count(p), weaks, "{weaks} weaks counted exactly");
                gos_rt_rc_release(p);
                assert_eq!(rc_live_count(), base + 1, "weaks keep the dead block");
                for left in (1..weaks).rev() {
                    gos_rt_rc_weak_release(p);
                    if left == 300 || left == 255 || left == 254 || left == 1 {
                        assert_eq!(weak_count(p), left, "{weaks} weaks, {left} left");
                    }
                }
                gos_rt_rc_weak_release(p);
                assert_eq!(
                    rc_live_count(),
                    base,
                    "{weaks} weaks: last release reclaims"
                );
            }
        }
        assert!(
            WEAK_OVERFLOW_COUNTS.lock().is_empty(),
            "no overflow entry outlives its block"
        );
    }

    #[test]
    fn weak_overflow_survives_concurrent_retains_and_releases() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: the block is marked shared before other threads touch it, and every thread
        // gives back exactly the weak shares it took.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_mark_shared(p);
            for _ in 0..250 {
                gos_rt_rc_weak_retain(p);
            }
            let addr = p as usize;
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(move || {
                        let p = addr as *mut u8;
                        for _ in 0..2_000 {
                            gos_rt_rc_weak_retain(p);
                            gos_rt_rc_weak_retain(p);
                            gos_rt_rc_weak_release(p);
                            gos_rt_rc_weak_release(p);
                        }
                    });
                }
            });
            assert_eq!(weak_count(p), 250);
            gos_rt_rc_release(p);
            for _ in 0..250 {
                gos_rt_rc_weak_release(p);
            }
            assert_eq!(rc_live_count(), base);
        }
    }

    #[test]
    fn strong_release_with_outstanding_weak_still_releases_children() {
        let _g = count_guard();
        let base = rc_live_count();
        let meta = tree_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let l0 = alloc_with_disc(1, 0, meta.as_ptr());
            let l1 = alloc_with_disc(1, 0, meta.as_ptr());
            let node = alloc_with_disc(2, 1, meta.as_ptr());
            set_child(node, 0, l0);
            set_child(node, 1, l1);
            gos_rt_rc_downgrade(node);
            assert_eq!(rc_live_count(), base + 3);
            // Last strong release frees the two children immediately; the
            // node block lingers for the weak observer.
            gos_rt_rc_release(node);
            assert_eq!(rc_live_count(), base + 1, "children freed, node lingers");
            gos_rt_rc_weak_release(node);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn weak_funcs_are_null_safe() {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            assert!(gos_rt_rc_downgrade(std::ptr::null_mut()).is_null());
            assert!(gos_rt_rc_weak_upgrade(std::ptr::null_mut()).is_null());
            gos_rt_rc_weak_retain(std::ptr::null_mut());
            gos_rt_rc_weak_release(std::ptr::null_mut());
        }
    }

    #[test]
    fn oversized_block_round_trips() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // A multi-MiB payload: no size is recorded anywhere any more
            // (mi_free recovers the block from the pointer), so this pins
            // that large blocks still alloc, retain state, and free.
            let big = (u16::MAX as u64) * 16 + 4096;
            let p = gos_rt_rc_alloc(big, std::ptr::null());
            assert!(!p.is_null());
            assert_eq!(strong_of(p), 1);
            gos_rt_rc_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn alloc_starts_at_strong_one_and_tracks_live() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            assert!(!p.is_null());
            assert_eq!(strong_of(p), 1);
            assert_eq!(rc_live_count(), base + 1);
            gos_rt_rc_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn retain_and_release_adjust_strong_count() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_retain(p);
            assert_eq!(strong_of(p), 2);
            gos_rt_rc_release(p);
            assert_eq!(strong_of(p), 1);
            assert_eq!(rc_live_count(), base + 1);
            gos_rt_rc_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn null_safe() {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_rc_retain(std::ptr::null_mut());
            gos_rt_rc_release(std::ptr::null_mut());
        }
    }

    #[test]
    fn leaf_with_no_meta_frees_cleanly() {
        let _g = count_guard();
        let base = rc_live_count();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    // Flat-blob meta for enum `Tree`: Leaf (disc 0, no children),
    // Node (disc 1, children at payload words 1 and 2).
    //   kind=ENUM, V=2, [disc0 cc0] [disc1 cc2 off1 off2]
    fn tree_meta() -> Vec<i64> {
        vec![
            RC_KIND_ENUM,
            2,
            /* Leaf */ 0,
            0,
            /* Node */ 1,
            2,
            0,
            1,
        ]
    }

    #[test]
    fn release_frees_recursive_tree() {
        let _g = count_guard();
        let base = rc_live_count();
        let meta = tree_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let l0 = alloc_with_disc(1, 0, meta.as_ptr());
            let l1 = alloc_with_disc(1, 0, meta.as_ptr());
            let node = alloc_with_disc(2, 1, meta.as_ptr());
            set_child(node, 0, l0);
            set_child(node, 1, l1);
            assert_eq!(rc_live_count(), base + 3);
            gos_rt_rc_release(node);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn cycle_collection_ignores_tagged_nullary_enum_children() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = tree_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let node = alloc_with_disc(2, 1, meta.as_ptr());
            // Keep a self-cycle alive long enough to enter the cycle
            // collector, alongside a tagged `Tree::Nil` child (`0x2`).
            set_child(node, 0, node);
            gos_rt_rc_retain(node);
            set_child(node, 1, 2usize as *mut u8);
            gos_rt_rc_release(node);
            gos_rt_collect_cycles();
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn shared_child_survives_until_last_owner() {
        let _g = count_guard();
        let base = rc_live_count();
        let meta = tree_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let shared = alloc_with_disc(1, 0, meta.as_ptr());
            let l1 = alloc_with_disc(1, 0, meta.as_ptr());
            let node = alloc_with_disc(2, 1, meta.as_ptr());
            set_child(node, 0, shared);
            set_child(node, 1, l1);
            // A second owner of `shared`.
            gos_rt_rc_retain(shared);
            assert_eq!(strong_of(shared), 2);

            gos_rt_rc_release(node);
            // node + l1 freed; shared survives with one owner.
            assert_eq!(rc_live_count(), base + 1);
            assert_eq!(strong_of(shared), 1);

            gos_rt_rc_release(shared);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "million-node stack-safety stress is covered natively; Miri allocator smoke tests remain enabled"
    )]
    fn deep_list_release_is_iterative() {
        // Struct list node: one child pointer at payload word 0.
        //   kind=STRUCT, V=1, [disc0 cc1 off0]
        let meta: Vec<i64> = vec![RC_KIND_STRUCT, 1, 0, 1, 0];

        let _g = count_guard();
        let base = rc_live_count();
        let depth = 1_000_000usize;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let mut head = std::ptr::null_mut::<u8>();
            for _ in 0..depth {
                let node = gos_rt_rc_alloc(8, meta.as_ptr());
                assert!(!node.is_null());
                set_child(node, 0, head);
                head = node;
            }
            assert_eq!(rc_live_count(), base + depth);
            // Recursive release would overflow the stack here.
            gos_rt_rc_release(head);
        }
        assert_eq!(rc_live_count(), base);
    }

    // Struct node with one i64 field (word 0) and two RC-pointer children
    // (words 1 and 2): kind=STRUCT, V=1, [disc0 cc2 off1 off2].
    fn two_child_meta() -> Vec<i64> {
        vec![RC_KIND_STRUCT, 1, 0, 2, 1, 2]
    }

    unsafe fn link_at(parent: *mut u8, word: usize, child: *mut u8) {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { set_child(parent, word, child) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_rc_retain(child) };
    }

    #[test]
    fn shared_child_is_an_external_live_edge_for_the_collector() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = two_child_meta();
        // The object borrows a pointer to this meta, so it must outlive S.
        let s_meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // Shared object S with one extra owner (standing in for the
            // other goroutine's handle).
            let s = gos_rt_rc_alloc(16, s_meta.as_ptr());
            gos_rt_rc_mark_shared(s);
            assert!(is_shared(header_ptr(s)));

            // Thread-local garbage cycle A <-> B where A also owns an edge
            // to S. The collector must trace and reclaim the cycle without
            // ever trial-deleting (or recoloring) S, then release the freed
            // A's edge to S for real.
            let a = gos_rt_rc_alloc(24, meta.as_ptr());
            let b = gos_rt_rc_alloc(24, meta.as_ptr());
            link_at(a, 1, b);
            link_at(b, 1, a);
            link_at(a, 2, s);
            assert_eq!(strong_of(s), 2, "one handle here, one edge from A");
            gos_rt_rc_release(a);
            gos_rt_rc_release(b);
            assert_eq!(rc_live_count(), base + 3, "cycle + S leak under plain RC");
            gos_rt_collect_cycles();
            assert_eq!(rc_live_count(), base + 1, "cycle reclaimed, S survives");
            assert_eq!(strong_of(s), 1, "the freed node's edge to S released");
            gos_rt_rc_release(s);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn buffered_then_shared_zero_count_candidate_is_reclaimed_immediately() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            // X owns child C, and X survives a decrement while thread-local,
            // so it is buffered as a cycle candidate (pinning its block).
            let c = gos_rt_rc_alloc(16, meta.as_ptr());
            let x = gos_rt_rc_alloc(16, meta.as_ptr());
            move_child(x, c);
            gos_rt_rc_retain(x);
            gos_rt_rc_release(x);
            assert!(is_buffered(header_ptr(x)), "surviving decrement buffers X");

            // X escapes to another goroutine, then its last owner drops it.
            // The release tears down C and indexed candidate removal clears
            // X's stale thread-local pin immediately. No collection slice is
            // needed once the strong count is known to be zero.
            gos_rt_rc_mark_shared(x);
            gos_rt_rc_release(x);
            assert_eq!(rc_live_count(), base, "C and X reclaimed immediately");
            gos_rt_collect_cycles();
        }
        assert_eq!(
            rc_live_count(),
            base,
            "collection clears the stale pin and reclaims the dead shared block"
        );
    }

    #[test]
    fn dead_shared_block_with_weak_is_reclaimed_by_the_last_weak_release() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        // The object borrows a pointer to this meta, so it must outlive S.
        let s_meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let s = gos_rt_rc_alloc(16, s_meta.as_ptr());
            gos_rt_rc_mark_shared(s);
            let w = gos_rt_rc_downgrade(s);
            assert!(!w.is_null());
            gos_rt_rc_release(s);
            assert_eq!(rc_live_count(), base + 1, "weak pins the dead block");
            // Upgrading the dead shared referent must fail via the CAS path.
            assert!(gos_rt_rc_weak_upgrade(w).is_null());
            let opt = gos_rt_rc_weak_upgrade_opt(w);
            assert_eq!((opt as u64) as i64, 1, "upgrade_opt reports None");
            gos_rt_rc_weak_release(w);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn weak_upgrade_opt_takes_an_owned_strong_reference() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        // The object borrows a pointer to this meta, so it must outlive S.
        let s_meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let s = gos_rt_rc_alloc(16, s_meta.as_ptr());
            let w = gos_rt_rc_downgrade(s);
            let opt = gos_rt_rc_weak_upgrade_opt(w);
            assert_eq!((opt as u64) as i64, 0, "Some");
            let payload = (opt >> 64) as i64 as usize as *mut u8;
            assert_eq!(payload, s, "payload is the referent");
            assert_eq!(strong_of(s), 2, "upgrade took a fresh strong reference");
            // The shadow local's scope-end release balances the take.
            gos_rt_rc_release(payload);
            gos_rt_rc_release(s);
            gos_rt_rc_weak_release(w);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn guarded_copy_blob_is_recognised_without_a_side_table() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = [RC_KIND_STRUCT_GUARDED, 0];
        let source = [17_u64];
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let first = gos_rt_rc_alloc_copy(8, meta.as_ptr(), source.as_ptr().cast());
            assert!(is_copy_blob(first), "an allocated blob is recognised");
            assert!(
                in_copy_blob_arena(first) || copy_blob_owner(first).is_some(),
                "a blob is either in the arena or carries its owner word"
            );
            assert_eq!(meta_of(header_ptr(first)), meta.as_ptr(), "child layout");
            assert_eq!(first.cast::<u64>().read(), 17);
            gos_rt_rc_release(first);

            let second = gos_rt_rc_alloc_copy(8, meta.as_ptr(), source.as_ptr().cast());
            assert!(is_copy_blob(second));
            assert_eq!(meta_of(header_ptr(second)), meta.as_ptr(), "child layout");
            gos_rt_rc_release(second);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn copy_blob_outside_the_arena_is_recognised_by_its_owner_word() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = [RC_KIND_STRUCT_GUARDED, 0];
        let meta_id = meta_intern(meta.as_ptr()).expect("meta id");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let header = owner_blob_header(RC_HEADER_SIZE + 8, true);
            assert!(!header.is_null());
            (*header).strong = 1;
            (*header).weak = AtomicU8::new(0);
            (*header).disc = COPY_BLOB_DISC;
            (*header).meta_id = meta_id;
            rc_live_inc();
            let payload = header.cast::<u8>().add(RC_HEADER_SIZE);
            assert!(!in_copy_blob_arena(payload));
            assert!(is_copy_blob(payload), "the owner word marks it");
            assert_eq!(
                block_base(header),
                header.cast::<u8>().sub(COPY_BLOB_OWNER_BYTES)
            );
            gos_rt_rc_release(payload);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn forged_blob_header_outside_the_arena_is_not_a_copy_blob() {
        let meta = [RC_KIND_STRUCT_GUARDED, 0];
        let meta_id = meta_intern(meta.as_ptr()).expect("meta id");
        // A plain block whose second word reads like a blob header, as an
        // element copy or any other untagged allocation might by chance.
        let mut words = [0_u64; 4];
        words[1] = 1 | (u64::from(COPY_BLOB_DISC) << 40) | (u64::from(meta_id) << 48);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let payload = unsafe { words.as_mut_ptr().add(2).cast::<u8>() };
        assert!(!in_copy_blob_arena(payload));
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert!(!unsafe { is_copy_blob(payload) });
    }

    /// Goroutine-shaped stress: worker threads churn atomic retains /
    /// releases and weak upgrades on shared objects while this thread
    /// builds and drops thread-local cycles referencing them, running the
    /// collector throughout. The collector must never mutate the shared
    /// objects' counts or flags non-atomically (a lost update here shows
    /// up as a wrong final count, a premature free, or a crash).
    #[test]
    #[cfg_attr(miri, ignore)] // spawns real threads over a large iteration count
    fn collector_races_shared_churn_without_corruption() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let n_shared = 4usize;
        let n_threads = 4usize;
        let iters = 20_000usize;
        let cycles = 400usize;
        let meta = two_child_meta();
        // Each object stores a borrowed pointer to its meta, so the meta
        // buffers must outlive every object that references them - the
        // shared set lives for the whole test, so its meta does too.
        let shared_meta = node_meta();
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let shared: Vec<usize> = (0..n_shared)
                .map(|_| {
                    let s = gos_rt_rc_alloc(16, shared_meta.as_ptr());
                    gos_rt_rc_mark_shared(s);
                    s as usize
                })
                .collect();
            let workers: Vec<std::thread::JoinHandle<()>> = (0..n_threads)
                .map(|t| {
                    let shared = shared.clone();
                    std::thread::spawn(move || {
                        let p = shared[t % shared.len()] as *mut u8;
                        let w = gos_rt_rc_downgrade(p);
                        for _ in 0..iters {
                            gos_rt_rc_retain(p);
                            let up = gos_rt_rc_weak_upgrade(w);
                            if !up.is_null() {
                                gos_rt_rc_release(up);
                            }
                            gos_rt_rc_release(p);
                        }
                        gos_rt_rc_weak_release(w);
                    })
                })
                .collect();
            // Meanwhile: thread-local garbage cycles, each holding an edge
            // into the shared set, collected in slices while the workers run.
            for i in 0..cycles {
                let a = gos_rt_rc_alloc(24, meta.as_ptr());
                let b = gos_rt_rc_alloc(24, meta.as_ptr());
                link_at(a, 1, b);
                link_at(b, 1, a);
                link_at(a, 2, shared[i % shared.len()] as *mut u8);
                gos_rt_rc_release(a);
                gos_rt_rc_release(b);
                if i % 16 == 0 {
                    gos_rt_collect_cycles();
                }
            }
            gos_rt_collect_cycles();
            for h in workers {
                h.join().expect("worker panicked");
            }
            for &s in &shared {
                assert_eq!(
                    strong_of(s as *mut u8),
                    1,
                    "every trial deletion / release of the shared object balanced"
                );
                gos_rt_rc_release(s as *mut u8);
            }
        }
        assert_eq!(rc_live_count(), base, "no leak and no double-free");
    }

    /// Whether a runtime string counts its shares atomically.
    unsafe fn string_is_shared(s: *const std::ffi::c_char) -> bool {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
        rc & crate::c_abi::string::STR_SHARED != 0
    }

    /// A closure environment holding a boxed struct capture: marking the
    /// environment shared reaches the strings inside the struct.
    #[test]
    fn mark_shared_reaches_strings_inside_a_boxed_struct_capture() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let struct_meta: Vec<i64> = vec![RC_KIND_STRUCT, 1, 0, 2, 0, 1];
        let env_meta: Vec<i64> = vec![RC_KIND_STRUCT, 1, 0, 1, 1];
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let dir = crate::c_abi::string::test_gos_str("/tmp/datadir-marker-string");
            let web = crate::c_abi::string::test_gos_str("/tmp/web-marker-string");
            let words: [i64; 2] = [dir as i64, web as i64];
            let boxed = gos_rt_enum_box_aggr(16, struct_meta.as_ptr(), words.as_ptr().cast());
            let env = gos_rt_rc_alloc(16, env_meta.as_ptr());
            set_child(env, 1, boxed);
            assert!(!string_is_shared(dir) && !string_is_shared(web));
            gos_rt_rc_mark_shared(env);
            assert!(string_is_shared(dir), "the struct's first String is shared");
            assert!(
                string_is_shared(web),
                "the struct's second String is shared"
            );
            gos_rt_rc_release(env);
            crate::c_abi::string::gos_rt_str_free_typed(dir.cast_mut());
            crate::c_abi::string::gos_rt_str_free_typed(web.cast_mut());
        }
        assert_eq!(rc_live_count(), base, "environment and box are reclaimed");
    }

    /// A node's `Vec` child is reached from other threads through the node, so
    /// marking the node shared marks the strings the vector holds.
    #[test]
    fn mark_shared_reaches_strings_inside_a_vec_child() {
        let _g = count_guard();
        fresh_cycle_state();
        let vec_child = gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT;
        let meta: Vec<i64> = vec![RC_KIND_STRUCT, 1, 0, 1, vec_child];
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let tag = crate::c_abi::string::test_gos_str("tag-marker-string");
            let tags = crate::c_abi::vec::gos_rt_vec_new_typed(
                8,
                crate::c_abi::vec::vec_elem_kind::STRING,
            );
            crate::c_abi::vec::gos_rt_vec_push_i64(tags, tag as i64);
            let node = gos_rt_rc_alloc(8, meta.as_ptr());
            set_child(node, 0, tags.cast());
            gos_rt_rc_mark_shared(node);
            assert!(string_is_shared(tag), "a Vec child's String is shared");
        }
    }

    /// A by-value aggregate is marked in place, by the layout it is handed.
    #[test]
    fn aggregate_mark_shared_marks_counted_fields_in_place() {
        let vec_child = gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT;
        let meta: Vec<i64> = vec![RC_KIND_STRUCT, 1, 0, 2, 0, vec_child | 2];
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            let dir = crate::c_abi::string::test_gos_str("aggregate-dir-marker");
            let tag = crate::c_abi::string::test_gos_str("aggregate-tag-marker");
            let tags = crate::c_abi::vec::gos_rt_vec_new_typed(
                8,
                crate::c_abi::vec::vec_elem_kind::STRING,
            );
            crate::c_abi::vec::gos_rt_vec_push_i64(tags, tag as i64);
            let mut words: [i64; 3] = [dir as i64, 7, tags as i64];
            gos_rt_aggr_mark_shared_children(words.as_mut_ptr().cast(), meta.as_ptr());
            assert!(string_is_shared(dir), "the String field is shared");
            assert!(string_is_shared(tag), "the Vec field's String is shared");
            assert_eq!(words[1], 7, "a scalar word is left alone");
        }
    }
}
