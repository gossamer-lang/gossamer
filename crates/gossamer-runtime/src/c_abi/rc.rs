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

// wasm32 has no mimalloc backend, so it joins the
// tsan/miri/fuzzing builds in routing RC blocks through the system
// global allocator (dlmalloc) with a side size map for `dealloc`.
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};

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

#[cfg(not(test))]
static RC_LIVE_ENABLED: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("GOS_RC_DEBUG").is_some());

#[inline]
fn rc_live_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        *RC_LIVE_ENABLED
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
    (unsafe { load_strong(h) }) & STRONG_COUNT_MASK
}

/// Overwrite the count portion of `strong`, preserving the flag bits.
#[inline]
unsafe fn set_strong_count(h: *mut RcHeader, count: u32) {
    let cur = unsafe { (*h).strong };
    // Immortal pin (unit-variant singletons): the count is never
    // mutated - not by retain/release, not by the cycle collector's
    // trial deletion, not by the release walk's child decrements.
    if cur & STRONG_COUNT_MASK == STRONG_IMMORTAL {
        return;
    }
    let flags = cur & !STRONG_COUNT_MASK;
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
    let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of!((*h).strong).cast_mut()) };
    a.load(Ordering::Relaxed)
}

/// Whether the object has escaped to another goroutine.
#[inline]
unsafe fn is_shared(h: *const RcHeader) -> bool {
    (unsafe { load_strong(h) }) & SHARED_BIT != 0
}

/// Escape-aware strong increment (the `retain` core). Atomic for shared
/// objects, a plain RMW otherwise. Region / immortal objects untouched.
#[inline]
unsafe fn inc_strong(h: *mut RcHeader) {
    let s = unsafe { load_strong(h) };
    if s & REGION_BIT != 0 || s & STRONG_COUNT_MASK == STRONG_IMMORTAL {
        return;
    }
    if s & SHARED_BIT != 0 {
        // Count is the low 27 bits; +1 cannot reach SHARED_BIT before
        // 134M live refs (unreachable). Relaxed: a retain needs
        // atomicity, not ordering.
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        a.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let bumped = (s & STRONG_COUNT_MASK)
        .saturating_add(1)
        .min(STRONG_COUNT_MASK);
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
        let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
        let prev = a.fetch_sub(1, Ordering::Release);
        return DecOutcome {
            next: (prev & STRONG_COUNT_MASK).saturating_sub(1),
            shared: true,
            skip: false,
        };
    }
    let next = (s & STRONG_COUNT_MASK).saturating_sub(1);
    unsafe { (*h).strong = (s & !STRONG_COUNT_MASK) | (next & STRONG_COUNT_MASK) };
    DecOutcome {
        next,
        shared: false,
        skip: false,
    }
}

/// Saturating weak increment. Mirrors the strong path's `STRONG_IMMORTAL`
/// pin: once the 8-bit weak count reaches `u8::MAX` it is never bumped
/// again, so it can never wrap to a small value and let `try_reclaim`
/// free a block that outstanding `Weak`s still observe. A pinned block
/// leaks rather than risk a use-after-free, the same conservative choice
/// the strong count makes at saturation.
#[inline]
unsafe fn inc_weak(h: *const RcHeader) {
    let _ = unsafe {
        (*h).weak
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                if w == u8::MAX { None } else { Some(w + 1) }
            })
    };
}

/// Saturating weak decrement. Returns the previous count when the count
/// was actually decremented (so the caller can reclaim at the 1 -> 0
/// edge), or `None` when the count is pinned at `u8::MAX`. A pinned count
/// is never decremented: lowering it could later reach 0 while weaks that
/// were dropped from the saturated total still observe the block, so the
/// block stays pinned (leaked) for good.
#[inline]
unsafe fn dec_weak(h: *const RcHeader) -> Option<u8> {
    unsafe {
        (*h).weak
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                if w == u8::MAX {
                    None
                } else {
                    Some(w.saturating_sub(1))
                }
            })
            .ok()
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
    if base.is_null() || unsafe { in_region_arena(base) } {
        return;
    }
    if unsafe { crate::c_abi::string::is_gos_string(base.cast()) } {
        // A shared string switches to atomic refcounting (it has no RC
        // children to walk), so its concurrent clone/drop cannot tear.
        unsafe { crate::c_abi::string::gos_rt_str_mark_shared(base.cast()) };
        return;
    }
    let mut work: Vec<*mut u8> = vec![base];
    while let Some(p) = work.pop() {
        if p.is_null() || unsafe { in_region_arena(p) } {
            continue;
        }
        if unsafe { crate::c_abi::string::is_gos_string(p.cast()) } {
            // A child string switches to atomic refcounting; it has no RC
            // children of its own, so there is nothing further to walk.
            unsafe { crate::c_abi::string::gos_rt_str_mark_shared(p.cast()) };
            continue;
        }
        let h = unsafe { header_ptr(p) };
        let s = unsafe { load_strong(h) };
        if s & SHARED_BIT != 0 || s & REGION_BIT != 0 || s & STRONG_COUNT_MASK == STRONG_IMMORTAL {
            continue;
        }
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
        unsafe {
            visit_children_raw(p, |c| work.push(c));
        }
    }
}

/// Marks the reachable RC subgraph of `payload` as shared across
/// goroutines. Called from the channel-send / goroutine-spawn lowering
/// so escaped objects switch to atomic reference counting. The codegen
/// gates the call on the static type (RC-managed only), so scalars
/// never reach here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_mark_shared(payload: *mut u8) {
    unsafe { mark_shared(payload) };
}

#[inline]
unsafe fn color_of(h: *const RcHeader) -> u32 {
    // Atomic (relaxed) load for the same reason as `strong_count`.
    ((unsafe { load_strong(h) }) & COLOR_MASK) >> COLOR_SHIFT
}

#[inline]
unsafe fn set_color(h: *mut RcHeader, color: u32) {
    let rest = unsafe { (*h).strong } & !COLOR_MASK;
    unsafe { (*h).strong = rest | (color << COLOR_SHIFT) };
}

#[inline]
unsafe fn is_buffered(h: *const RcHeader) -> bool {
    (unsafe { load_strong(h) }) & BUFFERED_BIT != 0
}

#[inline]
unsafe fn set_buffered(h: *mut RcHeader, on: bool) {
    if on {
        unsafe { (*h).strong |= BUFFERED_BIT };
    } else {
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
    let h = unsafe { header_ptr(payload) };
    // Objects that have escaped to another goroutine are excluded from
    // the per-thread cycle collector - touching their flag bits here is a
    // non-atomic write that would race a concurrent worker's atomic
    // retain/release, and even reading their payload slots races the
    // owning goroutine's mutations. Their cycles leak (like `Arc`);
    // break with weak refs.
    if unsafe { is_shared(h) } {
        return;
    }
    if !unsafe { has_rc_children(payload) } {
        return;
    }
    // Color purple marks a candidate; skip if already a tracked root.
    if unsafe { color_of(h) } == COLOR_PURPLE {
        return;
    }
    unsafe { set_color(h, COLOR_PURPLE) };
    if unsafe { is_buffered(h) } {
        return;
    }
    unsafe { set_buffered(h, true) };
    let over = ROOTS.with(|r| {
        let mut roots = r.borrow_mut();
        roots.push(payload);
        roots.len() >= COLLECT_THRESHOLD.with(std::cell::Cell::get)
    });
    if over {
        // Automatic collection processes a bounded slice and adapts the
        // threshold; it never drains the whole buffer in one inline pass.
        unsafe { collect_cycles_budgeted(Some(COLLECT_SLICE_ROOTS)) };
    }
}

/// Reclaim a zero-count object immediately when its buffered candidate still
/// lives in the thread-local queue. Indexed removal makes this O(1), which is
/// essential for long acyclic lists whose nodes briefly survive alias
/// releases. A candidate already extracted into an active collector slice is
/// not in the queue, so its pin remains intact until that slice finishes.
unsafe fn try_reclaim_zero(payload: *mut u8) {
    let h = unsafe { header_ptr(payload) };
    if unsafe { is_buffered(h) } {
        let removed = ROOTS.with(|roots| roots.borrow_mut().remove(payload));
        if removed {
            unsafe { set_buffered(h, false) };
        }
    }
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
        unsafe { libmimalloc_sys::mi_zalloc(total).cast() }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let Ok(layout) = Layout::from_size_align(total, RC_ALIGN) else {
            return std::ptr::null_mut();
        };
        let base = unsafe { alloc_zeroed(layout) };
        if !base.is_null() {
            tsan_sizes().lock().insert(base as usize, total);
        }
        base
    }
}

/// Like [`rc_block_alloc_zeroed`] without the zero fill, for callers
/// that provably write every byte.
#[inline]
fn rc_block_alloc_unzeroed(total: usize) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        unsafe { libmimalloc_sys::mi_malloc(total).cast() }
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

/// Append-only id -> blob-pointer table. Slot 0 is permanently null.
static META_TABLE: [std::sync::atomic::AtomicUsize; META_TABLE_CAP] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; META_TABLE_CAP];

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
    let id = unsafe { (*h).meta_id } as usize;
    META_TABLE[id].load(Ordering::Acquire) as *const i64
}

#[inline]
unsafe fn header_ptr(payload: *mut u8) -> *mut RcHeader {
    unsafe { payload.sub(RC_HEADER_SIZE) as *mut RcHeader }
}

// ---------------------------------------------------------------
// Arena regions (`region { … }`).
// ---------------------------------------------------------------
//
// A region is a bump allocator on a stack of large slabs. While a region
// is active, `gos_rt_rc_alloc` allocates from it and tags the object with
// `REGION_BIT`; retain/release on such objects are no-ops, and the whole
// region is freed in O(slabs) at `gos_rt_arena_pop` - never a per-node
// teardown walk. The compiler guarantees no region object outlives the
// pop (region-block results are RC-free and region values cannot be
// assigned to outer bindings), so the bulk free is sound.

/// Default slab size; one `mmap`-backed glibc allocation amortised over
/// many node allocations. A single oversized object gets its own slab.
const REGION_SLAB_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------
// Region arena: one reserved virtual range for every region slab.
// ---------------------------------------------------------------
//
// Region objects can be HEADERLESS (16-byte tree nodes), so the
// accounting entries cannot read a header bit to decide "no-op".
// Instead every region slab is carved out of a single reserved
// virtual range, and `in_region(ptr)` is a subtract + compare against
// a cached global - no memory access into the object. If the reserve
// fails (exotic environment), regions disable themselves
// (`gos_rt_arena_push` no-ops) and everything stays reference
// counted with headers - slower, never unsound.

/// Virtual reservation size. Address space only; pages are committed
/// slab-by-slab as regions actually allocate. On 32-bit wasm32 a
/// 64 GiB (`1 << 36`) reservation overflows `usize` and there is no
/// virtual-memory primitive anyway, so the value is an inert smaller
/// constant there - the arena never reserves (see the
/// `not(any(unix, windows))` `arena_reserve`), so regions stay
/// reference-counted.
#[cfg(not(target_arch = "wasm32"))]
const REGION_ARENA_BYTES: usize = 1 << 36;
#[cfg(target_arch = "wasm32")]
const REGION_ARENA_BYTES: usize = 1 << 30;

/// Base address of the reserved range. 0 = not yet initialised;
/// `usize::MAX` = reservation failed (regions disabled).
static REGION_ARENA_BASE: AtomicUsize = AtomicUsize::new(0);
/// Bump offset of the next never-used slab within the reserve.
static REGION_ARENA_NEXT: AtomicUsize = AtomicUsize::new(0);
/// Maximum number of standard slabs the reserved arena can hold.  The
/// overflow recycler addresses slabs by this index, so it never needs to
/// allocate a bookkeeping node or take a global allocator-side lock.
const REGION_ARENA_STANDARD_SLABS: usize = REGION_ARENA_BYTES / REGION_SLAB_BYTES;

/// Bits used for the one-based slab index in [`ArenaFreeSlabs::head`].  The
/// remaining bits are an ABA generation.  The reservation contains at most
/// 2^16 standard slabs, so 17 bits leave zero as the empty-list sentinel.
const ARENA_FREE_INDEX_BITS: usize = 17;
const ARENA_FREE_INDEX_MASK: usize = (1 << ARENA_FREE_INDEX_BITS) - 1;

/// Lock-free stack of decommitted standard slab offsets.
///
/// A normal region never reaches this stack: it retains a small committed
/// per-thread cache in `FREE_SLABS`.  When that cache overflows, the former
/// global `Mutex<Vec<usize>>` serialised unrelated container destruction and
/// later allocation.  This fixed-size, tagged Treiber stack stores the link
/// outside decommitted memory, so it remains readable after `madvise` /
/// `VirtualFree(MEM_DECOMMIT)`.  The tag in `head` prevents an ABA pop from
/// handing the same slab to two workers.
struct ArenaFreeSlabs<const N: usize> {
    head: AtomicUsize,
    next: [AtomicUsize; N],
}

impl<const N: usize> ArenaFreeSlabs<N> {
    const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            next: [const { AtomicUsize::new(0) }; N],
        }
    }

    #[inline]
    fn pop(&self) -> Option<usize> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let id = head & ARENA_FREE_INDEX_MASK;
            if id == 0 {
                return None;
            }
            let index = id - 1;
            // The index is produced only by `push`.  Keep this defensive
            // check so a corrupted process never indexes the static table.
            if index >= N {
                return None;
            }
            let next = self.next[index].load(Ordering::Relaxed);
            let tagged_next =
                (head.wrapping_add(1 << ARENA_FREE_INDEX_BITS) & !ARENA_FREE_INDEX_MASK) | next;
            match self.head.compare_exchange_weak(
                head,
                tagged_next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(index),
                Err(actual) => head = actual,
            }
        }
    }

    #[inline]
    fn push(&self, index: usize) -> bool {
        if index >= N {
            return false;
        }
        let id = index + 1;
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            self.next[index].store(head & ARENA_FREE_INDEX_MASK, Ordering::Relaxed);
            let tagged_id =
                (head.wrapping_add(1 << ARENA_FREE_INDEX_BITS) & !ARENA_FREE_INDEX_MASK) | id;
            match self.head.compare_exchange_weak(
                head,
                tagged_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => head = actual,
            }
        }
    }
}

/// Decommitted standard-size slabs available for re-commit.  This is global
/// only in reach, not in lock ownership: regular container allocation and
/// destruction use the per-thread cache, and overflow uses this atomic stack.
static REGION_ARENA_FREE: ArenaFreeSlabs<REGION_ARENA_STANDARD_SLABS> = ArenaFreeSlabs::new();

/// Diagnostic counters for the shared overflow recycler.  They let focused
/// stress tests prove that overflow recycling remains lock-free without
/// instrumenting the ordinary per-thread slab cache.
static REGION_ARENA_FREE_PUSHES: AtomicUsize = AtomicUsize::new(0);
static REGION_ARENA_FREE_POPS: AtomicUsize = AtomicUsize::new(0);

/// Number of `(retired, reacquired)` standard region slabs that passed
/// through the shared overflow recycler since process start.
#[must_use]
pub fn region_arena_overflow_reuse_counts() -> (usize, usize) {
    (
        REGION_ARENA_FREE_PUSHES.load(Ordering::Relaxed),
        REGION_ARENA_FREE_POPS.load(Ordering::Relaxed),
    )
}

/// True when `ptr` points into region-arena memory. One cached global
/// load plus the pure [`addr_in_region_arena`] range test.
#[inline]
pub(crate) fn in_region_arena(ptr: *const u8) -> bool {
    addr_in_region_arena(ptr as usize, REGION_ARENA_BASE.load(Ordering::Relaxed))
}

/// Pure range test for [`in_region_arena`], split out so the sentinel
/// handling is unit-testable without mutating the global base.
///
/// `base` carries two sentinels that are NOT live reservations: `0`
/// (no arena reserved yet) and `usize::MAX` (reservation failed). In
/// either state no pointer is region memory, so the range check must be
/// skipped entirely. The check is only meaningful once `base` is a real
/// reservation: with `base == 0` the subtraction is the identity, so the
/// bare range test would classify every pointer below `REGION_ARENA_BYTES`
/// (64 GiB) as in-region. That holds for the low heap addresses Windows
/// hands out, which silently turned `gos_rt_rc_retain`/`release` into
/// no-ops there - a use-after-free, since structural frees still ran.
#[inline]
fn addr_in_region_arena(addr: usize, base: usize) -> bool {
    if base == 0 || base == usize::MAX {
        return false;
    }
    addr.wrapping_sub(base) < REGION_ARENA_BYTES
}

/// Strip a tagged-repr enum's discriminant bits (pointer bits 1-2)
/// from an RC pointer. String bodies are deliberately ODD pointers and
/// pass through untouched; every other heap pointer is 8-aligned, so
/// the mask is a no-op for untagged values.
#[inline]
pub(crate) fn untag_rc(p: *mut u8) -> *mut u8 {
    if p as usize & 1 == 0 {
        (p as usize & !7) as *mut u8
    } else {
        p
    }
}

/// String bodies intentionally have an odd payload address, while every
/// headered RC allocation is at least 8-byte aligned. Check that representation
/// bit before attempting any RC-header access: an unrecognised binding/static
/// string must be conservatively ignored by its string drop path, never
/// reinterpreted as an unaligned `RcHeader`.
#[inline]
fn is_odd_string_repr(p: *const u8) -> bool {
    !p.is_null() && (p as usize & 1) != 0
}

/// Reserve the arena on first use. Returns the base, or `usize::MAX`
/// when virtual reservation is unavailable.
fn region_arena_base() -> usize {
    let cur = REGION_ARENA_BASE.load(Ordering::Acquire);
    if cur != 0 {
        return cur;
    }
    let reserved = arena_reserve(REGION_ARENA_BYTES);
    let val = if reserved.is_null() {
        usize::MAX
    } else {
        reserved as usize
    };
    match REGION_ARENA_BASE.compare_exchange(0, val, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => val,
        Err(winner) => {
            // Lost the race; release our reservation.
            if !reserved.is_null() {
                arena_release(reserved, REGION_ARENA_BYTES);
            }
            winner
        }
    }
}

#[cfg(unix)]
fn arena_reserve(len: usize) -> *mut u8 {
    // SAFETY: anonymous PROT_NONE reservation; no file, no aliasing.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        p.cast()
    }
}

#[cfg(unix)]
fn arena_release(p: *mut u8, len: usize) {
    // SAFETY: releasing exactly the mapping created in arena_reserve.
    unsafe { libc::munmap(p.cast(), len) };
}

#[cfg(unix)]
fn arena_commit(p: *mut u8, len: usize) -> bool {
    // SAFETY: p..p+len lies inside our reservation.
    unsafe { libc::mprotect(p.cast(), len, libc::PROT_READ | libc::PROT_WRITE) == 0 }
}

#[cfg(unix)]
fn arena_decommit(p: *mut u8, len: usize) {
    // Return the physical pages; keep the address range reserved.
    // SAFETY: range lies inside our reservation.
    unsafe {
        libc::madvise(p.cast(), len, libc::MADV_DONTNEED);
    }
}

#[cfg(windows)]
fn arena_reserve(len: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: plain reservation, no aliasing.
    unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_NOACCESS).cast() }
}

#[cfg(windows)]
fn arena_release(p: *mut u8, _len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: releasing exactly the reservation from arena_reserve.
    unsafe { VirtualFree(p.cast(), 0, MEM_RELEASE) };
}

#[cfg(windows)]
fn arena_commit(p: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: committing pages inside our reservation.
    !unsafe { VirtualAlloc(p.cast(), len, MEM_COMMIT, PAGE_READWRITE) }.is_null()
}

#[cfg(windows)]
fn arena_decommit(p: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: decommitting pages inside our reservation.
    unsafe { VirtualFree(p.cast(), len, MEM_DECOMMIT) };
}

// Targets with no virtual-memory reservation primitive (wasm32). The
// arena disables itself: `arena_reserve` returns null, so
// `region_arena_base` records `usize::MAX` and every region allocation
// falls back to headered reference-counted global allocation - sound,
// just without the bump-allocation optimisation.
#[cfg(not(any(unix, windows)))]
fn arena_reserve(_len: usize) -> *mut u8 {
    std::ptr::null_mut()
}

#[cfg(not(any(unix, windows)))]
fn arena_release(_p: *mut u8, _len: usize) {}

#[cfg(not(any(unix, windows)))]
fn arena_commit(_p: *mut u8, _len: usize) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
fn arena_decommit(_p: *mut u8, _len: usize) {}

/// Carve (or re-commit) a slab of `slab_size` bytes from the arena.
/// Null when the arena is unavailable or exhausted - callers fall back
/// to headered global allocation (sound, just unoptimised).
/// Host page size, queried once. Slab offsets inside the reserved
/// arena must be page-multiples or `mprotect` / `VirtualAlloc`
/// rejects the commit - and the size is NOT universally 4 KiB
/// (macOS arm64 and some aarch64 Linux kernels use 16 KiB or 64 KiB
/// pages).
fn os_page_size() -> usize {
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    let cached = PAGE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    #[cfg(unix)]
    // SAFETY: sysconf is async-signal-safe and has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
    #[cfg(windows)]
    let size = {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: GetSystemInfo fills the struct; no preconditions.
        unsafe { GetSystemInfo(&raw mut info) };
        (info.dwPageSize as usize).max(4096)
    };
    // Targets with no OS page-size query (wasm32). The arena is
    // disabled on these, so this is only a sane default to keep the
    // accounting math well-formed.
    #[cfg(not(any(unix, windows)))]
    let size = 65536;
    PAGE.store(size, Ordering::Relaxed);
    size
}

fn arena_acquire(slab_size: usize) -> *mut u8 {
    let base = region_arena_base();
    if base == usize::MAX {
        return std::ptr::null_mut();
    }
    if slab_size == REGION_SLAB_BYTES {
        if let Some(index) = REGION_ARENA_FREE.pop() {
            let off = index * REGION_SLAB_BYTES;
            let p = (base + off) as *mut u8;
            if arena_commit(p, slab_size) {
                REGION_ARENA_FREE_POPS.fetch_add(1, Ordering::Relaxed);
                return p;
            }
            return std::ptr::null_mut();
        }
    }
    // Every carve advances the cursor by a whole number of standard slabs, so
    // a standard slab always starts on a slab boundary and its offset names
    // its index in the free list. An oversized slab rounds the same way: what
    // the rounding leaves behind is reserved address space, not memory, since
    // only the pages a slab commits are ever backed. A slab is a whole number
    // of pages at every page size a host reports, so the commit stays
    // page-aligned.
    debug_assert_eq!(
        REGION_SLAB_BYTES % os_page_size(),
        0,
        "a slab has to be a whole number of pages for the commit to be accepted"
    );
    let slab_mask = REGION_SLAB_BYTES - 1;
    let rounded = (slab_size + slab_mask) & !slab_mask;
    let off = REGION_ARENA_NEXT.fetch_add(rounded, Ordering::Relaxed);
    if off + rounded > REGION_ARENA_BYTES {
        return std::ptr::null_mut();
    }
    let p = (base + off) as *mut u8;
    if arena_commit(p, rounded) {
        p
    } else {
        std::ptr::null_mut()
    }
}

/// Decommit a no-longer-needed standard slab and remember its offset
/// for re-commit. Oversized slabs are decommitted and their address
/// range retired (rare; bounded by peak oversized use).
fn arena_retire(p: *mut u8, slab_size: usize) {
    let base = REGION_ARENA_BASE.load(Ordering::Relaxed);
    arena_decommit(p, slab_size);
    if slab_size == REGION_SLAB_BYTES {
        let off = p as usize - base;
        debug_assert_eq!(off % REGION_SLAB_BYTES, 0);
        if REGION_ARENA_FREE.push(off / REGION_SLAB_BYTES) {
            REGION_ARENA_FREE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct RegionSlabs {
    /// `(base, layout_size)` for each slab, freed at pop.
    slabs: Vec<(*mut u8, usize)>,
    /// Saved bump state (base, cur offset, slab end) for this region while it
    /// is NOT the innermost one. The innermost region's live bump lives in the
    /// `BUMP` cache instead, so the hot allocation path touches no `RefCell`.
    saved: BumpState,
    /// Objects committed to this region before it was suspended (the live
    /// innermost region's uncommitted count is in `BUMP_OBJS`).
    objs: usize,
    /// Heap allocations this region owns: a copy of region-backed bytes goes
    /// to the heap so a recycled slab cannot land on its own source, and the
    /// slab sweep at pop cannot reclaim it. Freed one by one at pop.
    promoted: Vec<*mut std::ffi::c_char>,
}

/// Arena state owned by one running goroutine.
///
/// Region allocation uses thread-local fast-path caches while a goroutine is
/// executing, but a parked coroutine can be resumed on a different worker
/// when its original worker retires.  Moving this state at the coroutine
/// boundary keeps the active region, its slab ownership, and its pending RC
/// accounting with the coroutine instead of with the worker that happened to
/// run it last.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) struct ArenaState {
    regions: Vec<RegionSlabs>,
    depth: usize,
    bump: BumpState,
    bump_objs: usize,
}

// Region slabs are uniquely owned by the suspended goroutine. They are moved
// only between scheduler steps, when no worker can concurrently access them.
unsafe impl Send for ArenaState {}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl ArenaState {
    pub(crate) const fn empty() -> Self {
        Self {
            regions: Vec::new(),
            depth: 0,
            bump: BumpState::EMPTY,
            bump_objs: 0,
        }
    }
}

impl Drop for ArenaState {
    fn drop(&mut self) {
        if self.depth == 0 {
            return;
        }
        // A scheduler can discard a parked task during shutdown or future
        // cancellation support. Reattach its detached state briefly so the
        // normal pop path performs the RC accounting and slab recycling; a
        // raw `Vec<RegionSlabs>` drop would leak committed slabs instead.
        let state = std::mem::replace(self, Self::empty());
        install_arena_state(state);
        while region_active() {
            gos_rt_arena_pop();
        }
        let empty = take_arena_state();
        debug_assert!(empty.regions.is_empty());
        debug_assert_eq!(empty.depth, 0);
    }
}

/// Thread-local bump-pointer cache: `(base, cur, end)` of the innermost
/// region's current slab. A null `base` means "allocate a slab on next use".
/// This is what makes a region allocation a handful of inline instructions
/// (compare + add) rather than a `RefCell` borrow + `Vec` walk.
#[derive(Clone, Copy)]
struct BumpState {
    base: *mut u8,
    cur: usize,
    end: usize,
}

impl BumpState {
    const EMPTY: BumpState = BumpState {
        base: std::ptr::null_mut(),
        cur: 0,
        end: 0,
    };
}

/// Hard ceiling on recycled standard-size slabs kept per thread, whatever the
/// measured demand. Bounds what a thread that spikes once keeps resident.
const FREE_SLAB_CEILING: usize = 64;

thread_local! {
    /// Stack of suspended regions on this thread (the innermost region's live
    /// bump is in `BUMP`). Only touched on push/pop/slab-exhaustion.
    static REGIONS: std::cell::RefCell<Vec<RegionSlabs>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Pool of freed standard-size (`REGION_SLAB_BYTES`) slabs, reused by the
    /// next `arena_push` instead of re-`mmap`ing. Bounded by `SLAB_RETAIN`.
    static FREE_SLABS: std::cell::RefCell<Vec<*mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Widest single region this thread has closed, in standard-size slabs,
    /// clamped to `FREE_SLAB_CEILING`. Auto-regions on a fine-grained loop
    /// reopen a region of about the width the last one reached, so retaining
    /// that many slabs is what turns the next `arena_push` into a bump-pointer
    /// reset. Retaining fewer decommits slabs the next push immediately faults
    /// back in; retaining a fixed number more holds committed pages a thread
    /// whose regions are narrow never uses. The pool holds only slabs the
    /// thread had already committed at its own peak, so honouring the measured
    /// width never pushes resident memory past that peak.
    static SLAB_RETAIN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Nesting depth of active regions. `> 0` ⇒ a region is open; checked once
    /// per allocation (a `Cell` read) to route to the region bump.
    static REGION_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Live bump state of the innermost region (see `BumpState`).
    static BUMP: std::cell::Cell<BumpState> = const { std::cell::Cell::new(BumpState::EMPTY) };
    /// RC objects bump-allocated into the innermost region since it became
    /// innermost (reconciled into `RegionSlabs::objs` at push, into `RC_LIVE`
    /// at pop).
    static BUMP_OBJS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Detaches the active arena from this worker at a coroutine yield boundary.
/// The caller must later install the returned state before resuming that same
/// coroutine. Worker-local recycled slabs deliberately stay local: they hold
/// no live allocations and are only an allocation cache.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn take_arena_state() -> ArenaState {
    let regions = REGIONS.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let depth = REGION_DEPTH.with(|d| d.replace(0));
    let bump = BUMP.with(|b| b.replace(BumpState::EMPTY));
    let bump_objs = BUMP_OBJS.with(|o| o.replace(0));
    ArenaState {
        regions,
        depth,
        bump,
        bump_objs,
    }
}

/// Installs a suspended coroutine's arena on the worker about to resume it.
/// Scheduler task steps are serialized, so the worker TLS must be empty here;
/// retaining another task's state would cross-contaminate request ownership.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn install_arena_state(mut state: ArenaState) {
    debug_assert!(REGIONS.with(|r| r.borrow().is_empty()));
    debug_assert_eq!(REGION_DEPTH.with(std::cell::Cell::get), 0);
    debug_assert!(BUMP.with(std::cell::Cell::get).base.is_null());
    let regions = std::mem::take(&mut state.regions);
    let depth = std::mem::replace(&mut state.depth, 0);
    let bump = std::mem::replace(&mut state.bump, BumpState::EMPTY);
    let bump_objs = std::mem::replace(&mut state.bump_objs, 0);
    REGIONS.with(|r| *r.borrow_mut() = regions);
    REGION_DEPTH.with(|d| d.set(depth));
    BUMP.with(|b| b.set(bump));
    BUMP_OBJS.with(|o| o.set(bump_objs));
}

/// Cleans up any arena regions a request opened but did not close. This guard
/// is deliberately request-scoped rather than connection-scoped: a suspended
/// handler retains its region until it actually returns or unwinds, while a
/// malformed request, write timeout, or connection shutdown cannot leave an
/// unbalanced raw `arena_push` pinned on the worker.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) struct RequestArenaGuard {
    initial_depth: usize,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl RequestArenaGuard {
    pub(crate) fn new() -> Self {
        Self {
            initial_depth: REGION_DEPTH.with(std::cell::Cell::get),
        }
    }
}

impl Drop for RequestArenaGuard {
    fn drop(&mut self) {
        while REGION_DEPTH.with(std::cell::Cell::get) > self.initial_depth {
            gos_rt_arena_pop();
        }
    }
}

/// Acquire a slab of `slab_size` bytes. Standard-size slabs come from the
/// recycling pool when available (no `mmap`); oversized ones are always
/// freshly allocated. Bytes are NOT zeroed - `region_alloc_inner` zeroes
/// each handed-out allocation, so a recycled slab needs no bulk clear.
fn acquire_slab(slab_size: usize) -> *mut u8 {
    if slab_size == REGION_SLAB_BYTES {
        if let Some(s) = FREE_SLABS.with(|p| p.borrow_mut().pop()) {
            return s;
        }
    }
    // Slabs come exclusively from the reserved arena so `in_region_arena`
    // can identify region memory without touching the object. Null
    // (arena unavailable/exhausted) makes the region allocation fail and
    // the caller fall back to headered global allocation.
    arena_acquire(slab_size)
}

#[inline]
fn region_active() -> bool {
    REGION_DEPTH.with(|d| d.get() > 0)
}

/// Public: is an arena region active on this thread? Used by the Vec/String
/// allocators to route their backing storage through the region so it is
/// freed wholesale at pop (and so their `free` becomes a no-op).
#[must_use]
pub fn region_is_active() -> bool {
    region_active()
}

/// Public: bump `n` zeroed, `RC_ALIGN`-aligned bytes from the active region,
/// or null if no region is active. The bytes are freed wholesale at
/// `arena_pop` - callers must NOT individually free them.
#[must_use]
pub fn region_alloc_bytes(n: usize) -> *mut u8 {
    if n == 0 || !region_active() {
        return std::ptr::null_mut();
    }
    // Raw bytes (Vec/String backing) are not RC_LIVE-counted, so don't bump
    // the region's RC-object tally - doing so underflows RC_LIVE at pop.
    region_alloc_inner(n, false)
}

/// Bump `total` zeroed, `RC_ALIGN`-aligned bytes from the innermost active
/// region. `count_obj` increments the region's RC-object tally (used to
/// reconcile `RC_LIVE` at pop) - true for RC payloads, false for raw
/// Vec/String backing bytes (which are not `RC_LIVE`-counted). Returns null
/// only on allocation failure. Caller guarantees a region is active.
fn region_alloc_inner(total: usize, count_obj: bool) -> *mut u8 {
    region_alloc_inner_impl(total, count_obj, true)
}

/// Like [`region_alloc_inner`] but lets fully-initializing callers skip
/// the zero fill (a tagged-repr enum constructor stores every payload
/// slot, so pre-zeroing doubles its write traffic for nothing).
fn region_alloc_inner_unzeroed(total: usize, count_obj: bool) -> *mut u8 {
    region_alloc_inner_impl(total, count_obj, false)
}

fn region_alloc_inner_impl(total: usize, count_obj: bool, zero: bool) -> *mut u8 {
    let need = (total + RC_ALIGN - 1) & !(RC_ALIGN - 1);
    // Hot path: bump within the innermost region's current slab - no RefCell,
    // no Vec walk, just a compare and an add on a thread-local cache.
    let ptr = BUMP.with(|b| {
        let st = b.get();
        if !st.base.is_null() && st.cur + need <= st.end {
            let p = unsafe { st.base.add(st.cur) };
            b.set(BumpState {
                cur: st.cur + need,
                ..st
            });
            p
        } else {
            std::ptr::null_mut()
        }
    });
    let ptr = if ptr.is_null() {
        let p = region_alloc_slow(need);
        if p.is_null() {
            return std::ptr::null_mut();
        }
        p
    } else {
        ptr
    };
    // Zero the handed-out bytes: slabs may be recycled or freshly (un-zeroed)
    // allocated, and codegen relies on every allocation starting zeroed -
    // except for callers that provably overwrite every byte.
    if zero {
        unsafe { std::ptr::write_bytes(ptr, 0, need) };
    }
    if count_obj {
        BUMP_OBJS.with(|o| o.set(o.get() + 1));
    }
    ptr
}

/// Cold path: the current slab can't fit `need`. Acquire a fresh slab, record
/// it on the innermost region, point the bump cache at it, and carve `need`.
#[cold]
fn region_alloc_slow(need: usize) -> *mut u8 {
    let slab_size = need.max(REGION_SLAB_BYTES);
    let base = acquire_slab(slab_size);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    REGIONS.with(|r| {
        let mut regions = r.borrow_mut();
        let region = regions.last_mut().expect("region_alloc with no region");
        region.slabs.push((base, slab_size));
    });
    BUMP.with(|b| {
        b.set(BumpState {
            base,
            cur: need,
            end: slab_size,
        });
    });
    base
}

/// Region bump for an RC payload (counted against `RC_LIVE`).
fn region_alloc(total: usize) -> *mut u8 {
    region_alloc_inner(total, true)
}

/// Open a new arena region. Allocations until the matching
/// [`gos_rt_arena_pop`] are bump-allocated and freed wholesale.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_arena_push() {
    if region_arena_base() == usize::MAX {
        // Virtual reservation unavailable: regions disable themselves and
        // every allocation stays reference counted (headered). The matching
        // pop no-ops via the empty REGIONS stack.
        return;
    }
    // Suspend the current innermost region's live bump into its `RegionSlabs`
    // entry, then open a fresh region with an empty bump.
    let saved = BUMP.with(std::cell::Cell::get);
    let pending_objs = BUMP_OBJS.with(|o| o.replace(0));
    REGIONS.with(|r| {
        let mut regions = r.borrow_mut();
        if let Some(top) = regions.last_mut() {
            top.saved = saved;
            top.objs += pending_objs;
        }
        regions.push(RegionSlabs {
            slabs: Vec::new(),
            saved: BumpState::EMPTY,
            objs: 0,
            promoted: Vec::new(),
        });
    });
    BUMP.with(|b| b.set(BumpState::EMPTY));
    REGION_DEPTH.with(|d| d.set(d.get() + 1));
}

/// Records `body` as a heap allocation the innermost region owns and frees at
/// pop. A promotion made with no region open is an ordinary heap string.
pub(crate) fn region_track_promoted(body: *mut std::ffi::c_char) {
    REGIONS.with(|r| {
        if let Some(top) = r.borrow_mut().last_mut() {
            top.promoted.push(body);
        }
    });
}

/// Close the innermost region: free/recycle every slab in O(slabs). No
/// per-object teardown walk runs - the escape analysis guarantees nothing in
/// the region is referenced after pop.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_arena_pop() {
    let pending_objs = BUMP_OBJS.with(|o| o.replace(0));
    // Take the region out in one borrow. Everything the pop then does -
    // freeing the strings it owns, recycling or retiring its slabs - can
    // re-enter the allocator and open a region of its own, so none of it may
    // run while `REGIONS` is borrowed.
    let Some(region) = REGIONS.with(|r| r.borrow_mut().pop()) else {
        REGION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        return;
    };
    // Hand the allocator back to the parent before anything is freed, so an
    // allocation made during this teardown lands on a region that outlives it
    // rather than on a slab this pop is about to retire. The depth moves with
    // the bump so `region_active` and `REGIONS` agree throughout.
    let bump = REGIONS.with(|r| r.borrow().last().map_or(BumpState::EMPTY, |top| top.saved));
    BUMP.with(|b| b.set(bump));
    REGION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    for body in region.promoted {
        // SAFETY: each body was recorded by `region_track_promoted` while this
        // region was open, is reachable from nothing after the pop (the escape
        // analysis is what licenses the region), and is freed once, here.
        unsafe { crate::c_abi::string::free_promoted_string(body) };
    }
    if rc_live_enabled() {
        #[cfg(test)]
        let _guard = rc_live_mutation_guard();
        RC_LIVE.fetch_sub(region.objs + pending_objs, Ordering::Relaxed);
    }
    let width = region
        .slabs
        .iter()
        .filter(|(_, size)| *size == REGION_SLAB_BYTES)
        .count();
    let retain = SLAB_RETAIN.with(|r| {
        let widened = r.get().max(width).min(FREE_SLAB_CEILING);
        r.set(widened);
        widened
    });
    for (base, size) in region.slabs {
        // Recycle standard-size slabs into the thread-local pool so the
        // next region of this width reuses them without an mmap.
        if size == REGION_SLAB_BYTES {
            let kept = FREE_SLABS.with(|p| {
                let mut pool = p.borrow_mut();
                if pool.len() < retain {
                    pool.push(base);
                    true
                } else {
                    false
                }
            });
            if kept {
                continue;
            }
        }
        arena_retire(base, size);
    }
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
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let h = base as *mut RcHeader;
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, false, false);
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
        unsafe { rc_block_usable_size(base) }
    } else {
        0
    };
    crate::c_abi::ledger::rc_alloc(size as usize, usable, in_region, false);
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
            unsafe { gos_rt_rc_retain(fresh) };
            fresh
        }
        Err(winner) => {
            // Lost the race - drop the redundant node, share the winner's.
            unsafe { gos_rt_rc_release(fresh) };
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
    let h = unsafe { header_ptr(payload) };
    // `inc_strong` reads `strong` atomically and dispatches: region /
    // immortal objects are no-ops; escaped (shared) objects bump the
    // count atomically; thread-local objects take the cheap non-atomic
    // bump (count up, color back to black, buffered bit preserved).
    unsafe { inc_strong(h) };
}

/// Decrement the strong count; at zero, release RC-pointer children
/// (iteratively, to bound stack depth on deep structures) and free the
/// block (unless a weak ref still observes it). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_release(payload: *mut u8) {
    let payload = untag_rc(payload);
    if is_odd_string_repr(payload) {
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_release_no_buffer(payload: *mut u8) {
    let payload = untag_rc(payload);
    if payload.is_null() || in_region_arena(payload) {
        return;
    }
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
        return;
    }
    let h = unsafe { header_ptr(payload) };
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
        unsafe { try_reclaim(payload) };
        return;
    }
    unsafe { set_color(h, COLOR_BLACK) };
    // Clear any buffered pin a prior release may have set so `try_reclaim`
    // frees the block now rather than leaving it for the collector. The pin
    // and the candidate-buffer entry are dropped together: a freed block
    // must never linger in `ROOTS`, or a later collection dereferences it.
    if unsafe { is_buffered(h) } {
        ROOTS.with(|r| {
            r.borrow_mut().remove(payload);
        });
        unsafe { set_buffered(h, false) };
    }
    // Child slots are already cleared by the caller, so there are no
    // children to release here.
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
    if base.is_null() || unsafe { in_region_arena(base) } {
        return false;
    }
    unsafe { is_shared(header_ptr(base)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_strong_count(payload: *mut u8) -> i64 {
    let payload = untag_rc(payload);
    if payload.is_null() || in_region_arena(payload) {
        return 0;
    }
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        return 0;
    }
    let h = unsafe { header_ptr(payload) };
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
    let h = unsafe { header_ptr(base) };
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
    let h = unsafe { header_ptr(payload) };
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
    let h = unsafe { header_ptr(payload) };
    // `dec_weak` returns the old weak count; the new value is prev - 1. When
    // prev == 1 the count just reached 0, so the allocation can be reclaimed
    // if nothing else pins it. A count pinned at `u8::MAX` yields `None` and
    // is never decremented, so a saturated block is never reclaimed.
    if unsafe { dec_weak(h) } == Some(1) {
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
    let h = unsafe { header_ptr(base) };
    let s = unsafe { load_strong(h) };
    let count = s & STRONG_COUNT_MASK;
    if count == 0 {
        return std::ptr::null_mut();
    }
    if s & SHARED_BIT != 0 {
        // CAS loop: atomically upgrade only while the strong count remains
        // non-zero. Two goroutines upgrading the same weak reference
        // simultaneously must not both succeed after the referent dies.
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
        unsafe { set_strong_count(h, count.saturating_add(1)) };
        // Color black so a later cycle scan treats the revived object as
        // live. Shared objects never enter the collector and their color
        // bits carry the reclaim-claim flag, so only the thread-local path
        // recolors.
        unsafe { set_color(h, COLOR_BLACK) };
    }
    payload
}

/// Attempt to obtain a strong reference from a weak one. If the referent is
/// still alive (`strong > 0`), increment the strong count and return the
/// payload; otherwise return null (the `None` shape). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_upgrade(payload: *mut u8) -> *mut u8 {
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
    let taken = unsafe { weak_upgrade_take(payload) };
    if taken.is_null() {
        crate::c_abi::vec::pack_result(1, 0)
    } else {
        crate::c_abi::vec::pack_result(0, taken as i64)
    }
}

/// Free a block's allocation only when nothing pins it: no strong refs, no
/// weak refs, and not awaiting the cycle collector. The single funnel every
/// release path goes through, so each block is freed exactly once. For a
/// thread-local block the checks need no atomicity (single mutator); a
/// shared block routes through the CAS claim in [`try_reclaim_shared`].
unsafe fn try_reclaim(payload: *mut u8) {
    let h = unsafe { header_ptr(payload) };
    let s = unsafe { load_strong(h) };
    if s & SHARED_BIT != 0 {
        unsafe { try_reclaim_shared(payload, h) };
        return;
    }
    if s & STRONG_COUNT_MASK == 0
        && s & BUFFERED_BIT == 0
        && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0
    {
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
    let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
    let mut cur = a.load(Ordering::Acquire);
    loop {
        if cur & STRONG_COUNT_MASK != 0 || cur & BUFFERED_BIT != 0 || cur & SHARED_RECLAIM_BIT != 0
        {
            return;
        }
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
    unsafe { free_block(payload) };
}

/// Usable byte capacity of an RC block (header + payload), recovered from the
/// allocator. Used by the reuse path to confirm a recycled block is large
/// enough before re-homing a constructor into it.
#[inline]
unsafe fn rc_block_usable_size(base: *mut u8) -> usize {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
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
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    // One pass over the meta covers every child kind. The reuse frame holds
    // no teardown state of its own, so a Vec child is freed here rather than
    // queued for an exit this path does not have; a Map child is queued on
    // the same terms as every other release path, and the cascading
    // `gos_rt_rc_release` below drains it at its own teardown exit.
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
    if payload.is_null() || unsafe { in_region_arena(payload) } {
        return std::ptr::null_mut();
    }
    if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
        unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
        return std::ptr::null_mut();
    }
    let h = unsafe { header_ptr(payload) };
    let d = unsafe { dec_strong(h) };
    if d.skip {
        return std::ptr::null_mut();
    }
    if d.next != 0 {
        if !d.shared {
            unsafe { possible_root(payload) };
        }
        return std::ptr::null_mut();
    }
    if d.shared {
        fence(Ordering::Acquire);
    } else {
        // Shared flag bits are only ever mutated atomically; the color is
        // meaningless for shared objects (they never enter the collector).
        unsafe { set_color(h, COLOR_BLACK) };
    }
    unsafe { release_rc_children(payload) };
    // Reuse only a thread-local, weak-free, unbuffered block; anything else is
    // reclaimed normally (try_reclaim frees iff unpinned).
    if !d.shared && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0 && !unsafe { is_buffered(h) }
    {
        return h as *mut u8;
    }
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
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    if region_active() || unsafe { rc_block_usable_size(token) } < total {
        // Free the recycled block (its children are already released) and make
        // a fresh allocation. `free_block` takes a payload pointer.
        unsafe { free_block(token.add(RC_HEADER_SIZE)) };
        return unsafe { gos_rt_rc_alloc(size, meta) };
    }
    let h = token as *mut RcHeader;
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    let payload = unsafe { token.add(RC_HEADER_SIZE) };
    unsafe { std::ptr::write_bytes(payload, 0, size as usize) };
    rc_reuse_inc();
    let usable = if crate::c_abi::ledger::rc_alloc_stats_enabled() {
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
    if unsafe { crate::c_abi::string::is_gos_string(root.cast()) } {
        unsafe { crate::c_abi::string::gos_rt_str_free(root.cast()) };
        return;
    }
    let h = unsafe { header_ptr(root) };
    // Region objects are freed wholesale at region pop - never individually.
    // Skipping the decrement-and-walk here is exactly what eliminates the
    // per-node teardown cost for `region { … }` allocations.
    // `dec_strong` reads `strong` atomically and dispatches: region /
    // immortal objects are no-ops; escaped (shared) objects decrement
    // atomically (Release); thread-local objects take the cheap
    // non-atomic decrement.
    let d = unsafe { dec_strong(h) };
    if d.skip {
        return;
    }
    if d.next != 0 {
        // Survived the decrement. Thread-local objects become cycle
        // candidates; shared objects are excluded from the per-thread
        // collector (their cycles leak, like `Arc` - break with weak refs).
        if !d.shared {
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
        unsafe { set_color(h, COLOR_BLACK) };
    }
    let meta = unsafe { meta_of(h) };
    // Leaf fast path: a childless object (no RC-pointer children, the
    // overwhelming common case - every enum payload-free variant, every
    // leaf node) is reclaimed directly. This avoids touching the worklist
    // at all, so the dominant release shape never allocates or recurses.
    if meta.is_null() {
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
        unsafe { release_children_into(root, &mut worklist) };
        let _ = meta;
        unsafe { try_reclaim_zero(root) };
        while let Some(payload) = worklist.pop() {
            if payload.is_null() {
                continue;
            }
            let h = unsafe { header_ptr(payload) };
            let d = unsafe { dec_strong(h) };
            if d.skip {
                continue;
            }
            if d.next != 0 {
                if !d.shared {
                    unsafe { possible_root(payload) };
                }
                continue;
            }
            if d.shared {
                fence(Ordering::Acquire);
            } else {
                unsafe { set_color(h, COLOR_BLACK) };
            }
            unsafe {
                release_children_into(payload, &mut worklist);
            }
            unsafe { try_reclaim_zero(payload) };
        }
    });
    unsafe { teardown_exit() };
}

/// Fused child dispatch for the worklist loop: strings are freed
/// immediately, RC children are appended to `worklist`, and owned Vec and
/// Map children are queued for release at the outermost teardown exit.
///
/// One pass over the node's meta covers all three kinds. A node's children
/// are read once per teardown, which is what keeps the per-node cost
/// proportional to the children it has rather than to the kinds it might
/// have had.
unsafe fn release_children_into(payload: *mut u8, worklist: &mut Vec<*mut u8>) {
    use gossamer_abi::rc::{RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_VEC};
    unsafe {
        visit_entries(payload, |kind, child| match kind {
            RC_CHILD_RC => {
                let c = untag_rc(child);
                if crate::c_abi::string::is_gos_string(c.cast()) {
                    crate::c_abi::string::gos_rt_str_free(c.cast());
                } else {
                    worklist.push(c);
                }
            }
            RC_CHILD_VEC => queue_vec_child(child),
            RC_CHILD_MAP => queue_map_child(child),
            _ => {}
        });
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
        unsafe { crate::c_abi::map::gos_rt_vec_free(v.cast()) };
    }
    loop {
        let next = PENDING_MAP_FREES.with(|q| q.borrow_mut().pop());
        let Some(m) = next else { break };
        unsafe { crate::c_abi::map::gos_rt_map_free(m.cast()) };
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
    unsafe {
        visit_entries(payload, |kind, child| {
            if kind == gossamer_abi::rc::RC_CHILD_RC {
                f(child);
            }
        });
    }
}

/// Replaces every owned `Map` child of `payload` with a table of its own.
/// A `GosMap` carries no reference count, so a copy that kept the source's
/// handle would leave one table under two owners.
unsafe fn clone_map_children(payload: *mut u8) {
    unsafe {
        visit_entry_slots(payload, |kind, slot, child| {
            if kind != gossamer_abi::rc::RC_CHILD_MAP || slot.is_null() {
                return;
            }
            let cloned = crate::c_abi::gos_rt_map_clone(child.cast());
            slot.write_unaligned((cloned as *mut u8).expose_provenance());
        });
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
    unsafe { visit_entry_slots(payload, |kind, _slot, child| f(kind, child)) };
}

/// Same walk as [`visit_entries`], but the callback also receives the address
/// of the payload word holding the child. A kind whose copy takes a value of
/// its own - [`gossamer_abi::rc::RC_CHILD_MAP`] - writes the new handle back
/// through that address.
unsafe fn visit_entry_slots(payload: *mut u8, mut f: impl FnMut(i64, *mut usize, *mut u8)) {
    use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_WORD_MASK};
    let meta = unsafe { meta_of(header_ptr(payload)) };
    if meta.is_null() {
        return;
    }
    let kind = unsafe { *meta };
    let variant_count = unsafe { *meta.add(1) };
    if kind == RC_KIND_STRUCT_GUARDED {
        unsafe {
            visit_guarded_children(payload, meta, |c| {
                f(gossamer_abi::rc::RC_CHILD_RC, std::ptr::null_mut(), c);
            });
        };
        return;
    }
    // Only Enum and Struct carry child layouts today. String / Vec / Map
    // / Closure layouts are wired in a later phase and never reach here.
    if kind != RC_KIND_ENUM && kind != RC_KIND_STRUCT {
        return;
    }
    let target_disc = if kind == RC_KIND_ENUM {
        i64::from(unsafe { (*header_ptr(payload)).disc })
    } else {
        0
    };
    let mut idx: usize = 2;
    for _ in 0..variant_count.max(0) {
        let disc = unsafe { *meta.add(idx) };
        let child_count = unsafe { *meta.add(idx + 1) };
        let matches = kind == RC_KIND_STRUCT || disc == target_disc;
        if matches {
            for j in 0..child_count.max(0) {
                let entry = unsafe { *meta.add(idx + 2 + j as usize) };
                let child_kind = entry >> RC_CHILD_KIND_SHIFT;
                let word = entry & RC_CHILD_WORD_MASK;
                // Aggregate slots cross the C ABI as pointer-sized integer
                // words. Reconstruct exposed provenance explicitly instead
                // of treating those integer bits as a Rust pointer load.
                let slot = unsafe { payload.add((word as usize) * 8).cast::<usize>() };
                let raw = unsafe { slot.read_unaligned() };
                let child: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
                if !child.is_null() {
                    f(child_kind, slot, child);
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
unsafe fn free_block(payload: *mut u8) {
    let h = unsafe { header_ptr(payload) };
    // A copy blob's explicit carrier changes the allocation base. Ordinary
    // RC objects still start at their compact header.
    let copy_base = if unsafe { (*h).disc } == COPY_BLOB_DISC {
        // Copy blobs carry their allocation base in the explicit owner that
        // precedes the ordinary compact RC header.  Other guarded objects are
        // ordinary RC allocations and keep `h` as their base.
        Some(unsafe { (h as *mut u8).sub(COPY_BLOB_OWNER_BYTES) })
    } else {
        None
    };
    if unsafe { load_strong(h) } & SHARED_BIT != 0 {
        // A shared object is being reclaimed: keep the live-shared diagnostic
        // count in step. (A shared cycle never reaches here, which is exactly
        // what the non-zero exit count surfaces.)
        rc_shared_dec();
    }
    let base = copy_base.unwrap_or(h.cast::<u8>());
    rc_live_dec();
    // Straight back to mimalloc - see `gos_rt_rc_alloc` for why a custom
    // slab/pool is not used (measured net-neutral), and
    // `rc_block_alloc_zeroed` for why the call is direct.
    unsafe { rc_block_free(base) };
}

// ---------------------------------------------------------------
// Escaped value-aggregate copy-blobs (deterministic reclamation).
// ---------------------------------------------------------------
//
// When a multi-slot struct value flows into a `Some(..)`/`Ok(..)` payload
// on the LLVM tier, the backend makes a heap copy of its flat slots.
// Those copies are single-owner value snapshots; `gos_rt_rc_alloc_copy`
// puts them under reference counting with an `RC_KIND_STRUCT_GUARDED`
// meta so the MIR drop pass can release them deterministically when the
// owning aggregate slot dies.
//
// A guarded payload may also hold a non-copy pointer (map-get result,
// construction aggregate, or borrow). Copy blobs therefore carry a real
// owner immediately before their compact `RcHeader`; the carrier includes the
// ABI version, destructor identity, allocation generation, and the exact
// guarded metadata used for the copy. This replaces the former address-keyed
// fallback.
#[repr(C)]
struct CopyBlobOwner {
    abi_version: u16,
    kind: u16,
    destructor: u32,
    generation: u64,
    meta: *const i64,
}

const COPY_BLOB_OWNER_VERSION: u16 = 1;
const COPY_BLOB_OWNER_KIND: u16 = 3;
const COPY_BLOB_OWNER_DTOR: u32 = 1;
const COPY_BLOB_DISC: u8 = 0xCB;
const COPY_BLOB_OWNER_BYTES: usize = std::mem::size_of::<CopyBlobOwner>();
static NEXT_COPY_BLOB_GENERATION: AtomicUsize = AtomicUsize::new(1);

/// Identities a goroutine claims in one go. A blob's identity has to be
/// unique across the process, which one process-wide counter gives at the
/// cost of a read-modify-write on every allocation. Claiming a block at a
/// time keeps the uniqueness and leaves the allocation path a thread-local
/// increment.
const COPY_BLOB_GENERATION_BLOCK: usize = 1 << 16;

thread_local! {
    /// The identities this thread still holds: the next one to hand out and
    /// the end of its claimed block.
    static COPY_BLOB_GENERATIONS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

fn next_copy_blob_generation() -> u64 {
    COPY_BLOB_GENERATIONS.with(|held| {
        let (next, end) = held.get();
        if next < end {
            held.set((next + 1, end));
            return next as u64;
        }
        let base =
            NEXT_COPY_BLOB_GENERATION.fetch_add(COPY_BLOB_GENERATION_BLOCK, Ordering::Relaxed);
        held.set((base + 1, base + COPY_BLOB_GENERATION_BLOCK));
        base as u64
    })
}

/// Whether `payload` could be a managed allocation this module may inspect.
///
/// An `Option` / `Result` payload word is untyped: it carries a pointer for a
/// managed value and a plain scalar for `Option<i64>` and friends. Reading a
/// header out of a scalar is a wild dereference, so a word that cannot be an
/// allocation address is rejected before any load. Managed allocations come
/// from the allocator with at least pointer alignment and sit well above the
/// first page, which no small integer or `-1` sentinel satisfies.
#[inline]
fn payload_may_be_managed(payload: *const u8) -> bool {
    let address = payload as usize;
    address >= MIN_MANAGED_ADDRESS
        && address.is_multiple_of(std::mem::align_of::<usize>())
        && address != usize::MAX
}

/// Lowest address a managed allocation may occupy. The first page is never
/// mapped, so any word below it is a scalar payload rather than a pointer.
const MIN_MANAGED_ADDRESS: usize = 0x1000;

#[inline]
unsafe fn copy_blob_owner(payload: *mut u8) -> Option<&'static CopyBlobOwner> {
    if !payload_may_be_managed(payload) {
        return None;
    }
    let header = unsafe { header_ptr(payload) };
    if unsafe { (*header).disc } != COPY_BLOB_DISC {
        return None;
    }
    let owner = unsafe {
        &*((header as *mut u8)
            .sub(COPY_BLOB_OWNER_BYTES)
            .cast::<CopyBlobOwner>())
    };
    (owner.abi_version == COPY_BLOB_OWNER_VERSION
        && owner.kind == COPY_BLOB_OWNER_KIND
        && owner.destructor == COPY_BLOB_OWNER_DTOR
        && !owner.meta.is_null())
    .then_some(owner)
}

#[inline]
unsafe fn is_copy_blob(payload: *mut u8) -> bool {
    unsafe { copy_blob_owner(payload).is_some() }
}

/// Walk the `(disc_word, payload_word)` pairs of an `RC_KIND_STRUCT_GUARDED`
/// meta over the aggregate slots at `base`, calling `f` for each child that
/// is live (negative disc word, or the disc word reads 0), non-null, and a
/// copy blob with a validated owner. `base` may be a heap payload or a stack slot - the
/// walk only reads the flat words the meta names.
unsafe fn visit_guarded_children(base: *mut u8, meta: *const i64, mut f: impl FnMut(*mut u8)) {
    let entry_count = unsafe { *meta.add(1) };
    for i in 0..entry_count.max(0) {
        let gate = unsafe { *meta.add(2 + (i as usize) * 3) };
        let disc_word = unsafe { *meta.add(3 + (i as usize) * 3) };
        let payload_word = unsafe { *meta.add(4 + (i as usize) * 3) };
        // `gate` is the discriminant value under which the payload word
        // holds a copy-blob pointer (0 = Ok/Some side, 1 = Err side);
        // negative means unconditional (both sides are blobs).
        if gate >= 0 {
            let disc = unsafe { *(base.add(disc_word as usize * 8) as *const i64) };
            if disc != gate {
                continue;
            }
        }
        let slot = unsafe { base.add(payload_word as usize * 8).cast::<usize>() };
        let raw = unsafe { slot.read_unaligned() };
        let child: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
        if !child.is_null() && unsafe { is_copy_blob(child) } {
            f(child);
        }
    }
}

/// Allocate an RC copy-blob, memcpy `size` bytes from `src`, retain the
/// guarded children the copy now shares with its source, and attach an
/// explicit owner carrier. Inside a `region` block the bytes are
/// bump-allocated and freed wholesale at pop, so it has no individual owner
/// and its children are not retained (region objects never run the
/// per-node teardown walk).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_copy(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    unsafe { rc_alloc_from(size, meta, src, true) }
}

/// Allocates the same blob as [`gos_rt_rc_alloc_copy`] and takes the source's
/// share of the children rather than minting one.
///
/// The caller is giving up the words it copied here - its own walk over them
/// is what the compiler removed alongside this call - so the children keep the
/// count they already had and the blob is the one holding it.
///
/// # Safety
/// `src` must name `size` readable bytes laid out for `meta`, and the caller
/// must not release the children of those words afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_move(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    unsafe { rc_alloc_from(size, meta, src, false) }
}

unsafe fn rc_alloc_from(
    size: u64,
    meta: *const i64,
    src: *const u8,
    retain_children: bool,
) -> *mut u8 {
    let in_region = region_active();
    if in_region {
        let payload = unsafe { gos_rt_rc_alloc(size, std::ptr::null()) };
        if !payload.is_null() && !src.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
        }
        return payload;
    }
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let total = COPY_BLOB_OWNER_BYTES
        .saturating_add(RC_HEADER_SIZE)
        .saturating_add(size as usize);
    // Every byte of the block is written below - the owner carrier, the
    // header, and the payload the copy fills - so the zero fill would be
    // overwritten wholesale. A source that covers the payload is the only
    // shape that holds; anything else keeps the zeroed block.
    let base = if src.is_null() {
        rc_block_alloc_zeroed(total)
    } else {
        rc_block_alloc_unzeroed(total)
    };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let owner = base.cast::<CopyBlobOwner>();
    let header = unsafe { base.add(COPY_BLOB_OWNER_BYTES).cast::<RcHeader>() };
    unsafe {
        owner.write(CopyBlobOwner {
            abi_version: COPY_BLOB_OWNER_VERSION,
            kind: COPY_BLOB_OWNER_KIND,
            destructor: COPY_BLOB_OWNER_DTOR,
            generation: next_copy_blob_generation(),
            meta,
        });
        (*header).strong = 1;
        (*header).weak = AtomicU8::new(0);
        (*header).disc = COPY_BLOB_DISC;
        (*header).meta_id = meta_id;
    }
    rc_live_inc();
    let payload = unsafe { (header as *mut u8).add(RC_HEADER_SIZE) };
    if payload.is_null() || src.is_null() {
        return payload;
    }
    unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
    if meta.is_null() {
        // A leaf blob: its words are scalars, so the copy shares no RC
        // child with the source and there is nothing to retain. The
        // interning above already accepts null as "no child layout".
        return payload;
    }
    if !retain_children {
        // The source gave up its share of these words, so the blob holds the
        // count they already carried.
        return payload;
    }
    unsafe {
        if *meta == gossamer_abi::rc::RC_KIND_STRUCT_GUARDED {
            visit_guarded_children(payload, meta, |child| {
                gos_rt_rc_retain(child);
            });
        } else {
            // Map-owned aggregate copies use ordinary structural metadata so
            // their direct String / RC and Vec fields are retained along with
            // the copied words. `visit_entries` reads this blob from the
            // freshly initialised header and dispatches each child kind.
            visit_entries(payload, |kind, child| {
                if kind == gossamer_abi::rc::RC_CHILD_VEC {
                    crate::c_abi::gos_rt_vec_retain(child.cast());
                } else if kind != gossamer_abi::rc::RC_CHILD_MAP {
                    gos_rt_rc_retain(child);
                }
            });
            clone_map_children(payload);
        }
    }
    payload
}

/// Release the guarded children held in the aggregate slots at `base`
/// (a stack aggregate dying or being overwritten). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_release_children(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    unsafe {
        visit_guarded_children(base, meta, |child| {
            gos_rt_rc_release(child);
        });
    }
}

/// Retain the guarded children held in the aggregate slots at `base`
/// (a stack aggregate that was just whole-copied). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_retain_children(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    unsafe {
        visit_guarded_children(base, meta, |child| {
            gos_rt_rc_retain(child);
        });
    }
}

/// Heap-box a multi-slot aggregate that is a user-enum variant payload:
/// allocate an RC cell carrying `meta` (an `RC_KIND_STRUCT` child-word list),
/// copy `size` bytes from `src`, and retain every RC child the box now
/// co-owns. The enum's variant meta lists this box's slot as a child, so the
/// enum's release frees the box; the box's own release walk then reclaims its
/// `String` / RC-node children exactly once. The retain balances the source
/// aggregate's scope-end teardown release, so the box keeps a live reference
/// even after the constructing frame returns. Inside a `region { … }` the
/// bytes are bump-allocated and freed wholesale at pop, so the box is
/// meta-less and its children are not retained (region objects never run the
/// per-node teardown walk).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_enum_box_aggr(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    let in_region = region_active();
    let payload = unsafe { gos_rt_rc_alloc(size, if in_region { std::ptr::null() } else { meta }) };
    if payload.is_null() || src.is_null() {
        return payload;
    }
    unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
    if in_region {
        return payload;
    }
    unsafe {
        visit_children_raw(payload, |c| {
            gos_rt_rc_retain(c);
        });
        // The copy co-owns any Vec child alongside its source.
        visit_vec_children(payload, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
        clone_map_children(payload);
    }
    payload
}

/// Copy a by-value aggregate's slot bytes into the RC cell a `Weak` observes:
/// allocate a headered cell carrying `meta` (an `RC_KIND_STRUCT` child-word
/// list), copy `size` bytes from `src`, and retain every RC child the cell now
/// co-owns. The caller's frame owns the returned strong reference and releases
/// it at scope end, at which point the outstanding weak count keeps the cell
/// allocated until the last `Weak` is released.
///
/// The cell is always allocated globally, never bump-allocated in an enclosing
/// `arena { … }`: weak liveness is per-object, and a region block reclaims its
/// slab wholesale without individual accounting.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_cell(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let base = rc_block_alloc_zeroed(total);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is a fresh zeroed block of at least `RC_HEADER_SIZE`.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    crate::c_abi::ledger::rc_alloc(size as usize, 0, false, false);
    // SAFETY: the payload begins one header past the block base.
    let payload = unsafe { base.add(RC_HEADER_SIZE) };
    if src.is_null() {
        return payload;
    }
    // SAFETY: `src` addresses `size` bytes of the source aggregate's slots and
    // the fresh cell cannot overlap it.
    unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
    // SAFETY: the header's meta describes the payload words just written.
    unsafe {
        visit_children_raw(payload, |c| {
            gos_rt_rc_retain(c);
        });
        visit_vec_children(payload, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
        clone_map_children(payload);
    }
    payload
}

/// Retain every RC child `payload` names through its header meta - a `String`
/// (`gos_rt_str_retain`) or RC-node (`gos_rt_rc_retain`) child. Used after a
/// multi-slot aggregate enum payload is materialised by value into a match
/// binding: the binding co-owns the box's children, so its scope-end teardown
/// release is balanced by this retain. `payload` is the box pointer; its
/// children are the same pointers the binding now aliases. Null-safe; a
/// region-arena box is left untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_retain_children(payload: *mut u8) {
    let base = untag_rc(payload);
    if base.is_null() || in_region_arena(base) {
        return;
    }
    unsafe {
        visit_children_raw(base, |c| {
            gos_rt_rc_retain(c);
        });
        // The binding co-owns any Vec child alongside the box.
        visit_vec_children(base, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
    }
}

/// Zero the `(disc, payload)` word pairs a guarded meta names within the
/// aggregate slots at `base`. Entry-block initialisation: without it the
/// first release-before-reassignment walk would read stack garbage, and a
/// garbage word that happens to equal a live copy-blob address would be
/// spuriously released. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_zero_guarded(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    let entry_count = unsafe { *meta.add(1) };
    for i in 0..entry_count.max(0) {
        let gate = unsafe { *meta.add(2 + (i as usize) * 3) };
        let disc_word = unsafe { *meta.add(3 + (i as usize) * 3) };
        let payload_word = unsafe { *meta.add(4 + (i as usize) * 3) };
        if gate >= 0 && disc_word >= 0 {
            // Write a discriminant that fails every gate (no entry gates
            // on a negative disc), so an accidental read of the
            // not-yet-assigned field never sees a live payload. For the
            // Option/Result encoding -1 is no valid variant; the real
            // first assignment overwrites it.
            unsafe { *(base.add(disc_word as usize * 8) as *mut i64) = -1 };
        }
        unsafe { *(base.add(payload_word as usize * 8) as *mut i64) = 0 };
    }
}

/// Release the payload of the by-value `{disc, payload}` Option/Result at
/// `slot` when it carries a validated copy-blob owner. Companion to
/// [`gos_rt_option_slot_retain`]; used when an option holder dies, is
/// overwritten, or an owning field slot is replaced. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_slot_release(slot: *const i64) {
    if slot.is_null() {
        return;
    }
    let payload = unsafe { *(slot.add(1) as *const *mut u8) };
    if !payload.is_null() && unsafe { is_copy_blob(payload) } {
        unsafe { gos_rt_rc_release(payload) };
        // Null the payload word so a second release of the same slot
        // (consumption-site release + the unconditional return-sweep)
        // is a no-op instead of a double-free - the same null-out
        // discipline the local-release pass uses. A later allocation can
        // reuse this address, so the explicit null-out
        // remains the second-release guard for this slot.
        unsafe { *slot.add(1).cast_mut() = 0 };
    }
}

/// Retain the payload of the by-value `{disc, payload}` Option/Result at
/// `slot` when the discriminant reads 0 (`Some`/`Ok`) and the payload is a
/// copy-blob owner. Used when an aliased option value is stored into
/// an owning aggregate slot. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_slot_retain(slot: *const i64) {
    if slot.is_null() {
        return;
    }
    let payload = unsafe { *(slot.add(1) as *const *mut u8) };
    if !payload.is_null() && unsafe { is_copy_blob(payload) } {
        unsafe { gos_rt_rc_retain(payload) };
    }
}

// ---------------------------------------------------------------
// Trial-deletion cycle collector (Bacon-Rajan, synchronous).
// ---------------------------------------------------------------
//
// All four phases trace the RC object graph from the candidate buffer via
// `visit_rc_children` (the same meta-blob edge map the release walk uses).
// They are iterative (explicit stacks) so a large cyclic component cannot
// overflow the runtime stack. They never inspect a stack frame, register,
// or spill slot, so they are sound under `-O3`.

/// MarkGray: trial-delete internal references by decrementing the strong
/// count of every child reachable from `root`, marking the subgraph gray.
/// After this, a node's count reflects only references from *outside* the
/// traced subgraph.
///
/// A shared (escaped) child is an external live edge: the per-thread
/// collector never trial-deletes through the shared boundary. Decrementing
/// a shared child here would transiently zero a live count that another
/// goroutine's release could observe (freeing a live object), and every
/// recolor of it would be a non-atomic RMW racing that goroutine's atomic
/// retain/release (lost update). The edge is instead released for real if
/// and when the referencing node is freed ([`collect_white`]).
unsafe fn mark_gray(root: *mut u8) {
    let mut stack = vec![root];
    while let Some(s) = stack.pop() {
        let h = unsafe { header_ptr(s) };
        if unsafe { color_of(h) } == COLOR_GRAY {
            continue;
        }
        unsafe { set_color(h, COLOR_GRAY) };
        unsafe {
            visit_rc_children(s, |t| {
                let th = header_ptr(t);
                if load_strong(th) & SHARED_BIT != 0 {
                    return;
                }
                set_strong_count(th, strong_count(th).saturating_sub(1));
                stack.push(t);
            });
        }
    }
}

/// Scan: any gray node still holding an external reference (count > 0) is
/// live - restore its subgraph to black. Gray nodes that reached count 0
/// are cyclic garbage - paint them white and recurse.
unsafe fn scan(root: *mut u8) {
    let mut stack = vec![root];
    while let Some(s) = stack.pop() {
        let h = unsafe { header_ptr(s) };
        if unsafe { color_of(h) } != COLOR_GRAY {
            continue;
        }
        if unsafe { strong_count(h) } > 0 {
            unsafe { scan_black(s) };
        } else {
            unsafe { set_color(h, COLOR_WHITE) };
            // Shared children were never grayed (external live edges);
            // skip them so their flag word is never even read as a color.
            unsafe {
                visit_rc_children(s, |t| {
                    if load_strong(header_ptr(t)) & SHARED_BIT == 0 {
                        stack.push(t);
                    }
                });
            }
        }
    }
}

/// ScanBlack: restore the counts MarkGray trial-deleted for a live subgraph
/// and repaint it black.
unsafe fn scan_black(root: *mut u8) {
    let mut stack = vec![root];
    while let Some(s) = stack.pop() {
        let h = unsafe { header_ptr(s) };
        if unsafe { color_of(h) } == COLOR_BLACK {
            continue;
        }
        unsafe { set_color(h, COLOR_BLACK) };
        unsafe {
            visit_rc_children(s, |t| {
                let th = header_ptr(t);
                if load_strong(th) & SHARED_BIT != 0 {
                    // Shared child: never trial-deleted by mark_gray, so
                    // there is nothing to restore (and its flag word must
                    // never see a non-atomic RMW).
                    return;
                }
                set_strong_count(th, strong_count(th).saturating_add(1));
                if color_of(th) != COLOR_BLACK {
                    stack.push(t);
                }
            });
        }
    }
}

/// CollectWhite: free the confirmed garbage cycle. White nodes are gathered
/// (repainting black to dedupe), then their allocations reclaimed - unless a
/// weak reference still pins one, in which case the payload is already dead
/// and the block lingers for the last weak release. Each reclaimed payload is
/// appended to `freed` so a bounded (sliced) collection can drop it from the
/// still-buffered candidate set before any later slice dereferences it.
///
/// White is a definitive verdict (scan proved zero external references), so a
/// white node is freed regardless of its buffered bit. In a full drain no
/// white node is ever still buffered (every candidate's bit is cleared before
/// CollectWhite runs), so the buffered case only arises when a garbage
/// component straddles the slice boundary - its leftover member is freed here
/// and removed from the buffer by the caller's reconciliation.
unsafe fn collect_white(root: *mut u8, freed: &mut Vec<*mut u8>) {
    let mut stack = vec![root];
    let mut to_free: Vec<*mut u8> = Vec::new();
    while let Some(s) = stack.pop() {
        let h = unsafe { header_ptr(s) };
        if unsafe { color_of(h) } != COLOR_WHITE {
            continue;
        }
        unsafe { set_color(h, COLOR_BLACK) };
        // Use visit_children_raw so string children are freed via their own
        // destructor rather than silently skipped, mirroring rc_release_impl.
        unsafe {
            visit_children_raw(s, |c| {
                // `visit_children_raw` stays branch-free for the regular
                // release path.  The collector, unlike that path, must not
                // dereference an untagged nullary-enum value.
                if c.is_null() {
                    return;
                }
                if crate::c_abi::string::is_gos_string(c.cast()) {
                    crate::c_abi::string::gos_rt_str_free(c.cast());
                } else if is_shared(header_ptr(c)) {
                    // The dying node's edge into the shared heap was never
                    // trial-deleted (see `mark_gray`); release it for real.
                    release_shared_edge(c);
                } else {
                    stack.push(c);
                }
            });
            // Owned Vec children sit outside the RC graph (never
            // trial-deleted); queue the dying node's share for release at
            // the outermost teardown exit - a Vec free can cascade into
            // RC releases, which must not run mid-collection.
            visit_vec_children(s, queue_vec_child);
        }
        to_free.push(s);
    }
    for s in to_free {
        let h = unsafe { header_ptr(s) };
        if unsafe { strong_count(h) } == 0 && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0 {
            CYCLES_FREED.fetch_add(1, Ordering::Relaxed);
            freed.push(s);
            unsafe { free_block(s) };
        }
    }
}

/// Release one strong edge from a freed garbage node into the shared heap.
/// The collector never trial-deletes through a shared boundary (see
/// [`mark_gray`]), so a garbage node's edge to a shared child is still
/// counted and must be released like a mutator release - atomically - when
/// the node is freed. Uses a local worklist rather than
/// [`rc_release_impl`]: the collector can run inside `rc_release_impl`'s
/// thread-local `RELEASE_WORKLIST` borrow, which must not be re-entered.
///
/// Children of a shared object are themselves shared (`mark_shared` walks
/// the whole reachable subgraph at the escape point), so the cascade stays
/// on the atomic path. A thread-local node reached through such an edge is
/// still torn down correctly, but is never buffered as a cycle candidate:
/// buffering could arm a nested collection mid-phase.
unsafe fn release_shared_edge(root: *mut u8) {
    let mut worklist: Vec<*mut u8> = vec![root];
    while let Some(payload) = worklist.pop() {
        if payload.is_null() {
            continue;
        }
        if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
            unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
            continue;
        }
        let h = unsafe { header_ptr(payload) };
        let d = unsafe { dec_strong(h) };
        if d.skip || d.next != 0 {
            continue;
        }
        if d.shared {
            fence(Ordering::Acquire);
        } else {
            unsafe { set_color(h, COLOR_BLACK) };
        }
        unsafe { release_children_into(payload, &mut worklist) };
        unsafe { try_reclaim(payload) };
    }
}

/// Run a synchronous trial-deletion collection over the candidate buffer.
/// Reclaims unreachable cyclic RC garbage; live data and acyclic garbage are
/// untouched. Cost is proportional to the subgraph reachable from the
/// processed candidates, not the whole heap. Full drain (`budget = None`),
/// used by the explicit `runtime::collect_cycles()`.
unsafe fn collect_cycles() {
    unsafe { collect_cycles_budgeted(None) };
}

/// Trial-deletion collection bounded to at most `budget` candidate roots per
/// call (`None` = drain everything). Bounding keeps an automatic collection
/// from traversing an unbounded number of independent candidate subgraphs
/// inline; leftover candidates remain buffered for the next slice. When a
/// budget is given, the adaptive `COLLECT_THRESHOLD` is updated from how much
/// this slice reclaimed - little garbage backs the threshold off, productive
/// slices snap it back to the base.
unsafe fn collect_cycles_budgeted(budget: Option<usize>) {
    let roots: Vec<*mut u8> = ROOTS.with(|r| {
        let mut buf = r.borrow_mut();
        let len = buf.len();
        match budget {
            // Take a tail slice, leaving earlier candidates buffered. The
            // front is exactly the state the buffer would hold had those
            // releases not yet crossed the threshold, so slicing is
            // equivalent to a not-yet-fired smaller buffer - sound.
            Some(n) if n < len => buf.take_tail(n),
            _ => buf.take_all(),
        }
    });
    if roots.is_empty() {
        return;
    }
    // The whole slice is one teardown frame: owned Vec children of freed
    // cycle members queue during `collect_white` and drain at the exit
    // below (or at the enclosing release walk's exit when the collection
    // fired mid-release).
    teardown_enter();
    unsafe { collect_cycles_slice(budget, roots) };
    unsafe { teardown_exit() };
}

unsafe fn collect_cycles_slice(budget: Option<usize>, roots: Vec<*mut u8>) {
    let candidates = roots.len();
    let freed_before = CYCLES_FREED.load(Ordering::Relaxed);
    // MarkRoots: trace gray from each still-purple candidate; drop stale
    // candidates (revived to black, or already dead awaiting free).
    let mut scan_roots: Vec<*mut u8> = Vec::new();
    let mut dead: Vec<*mut u8> = Vec::new();
    for s in roots {
        let h = unsafe { header_ptr(s) };
        // A candidate that escaped to another goroutine after being
        // buffered: drop it from the collector. The stale buffered pin is
        // cleared atomically (its ROOTS entry is being dropped right here),
        // and the block is reclaimed if its count already fell to zero -
        // the releasing goroutine refused to free while the pin was set.
        if unsafe { is_shared(h) } {
            let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
            a.fetch_and(!BUFFERED_BIT, Ordering::AcqRel);
            unsafe { try_reclaim(s) };
            continue;
        }
        if unsafe { color_of(h) } == COLOR_PURPLE {
            unsafe { mark_gray(s) };
            scan_roots.push(s);
        } else {
            unsafe { set_buffered(h, false) };
            // A candidate whose count later fell to 0 *and* is black is
            // acyclic garbage whose children were already released; reclaim
            // it. A gray candidate is mid-trace (reachable from another
            // purple root) and must be left to scan/collect - never freed
            // here, or a live cycle member would be dropped.
            if unsafe { color_of(h) } == COLOR_BLACK && unsafe { strong_count(h) } == 0 {
                dead.push(s);
            }
        }
    }
    for &s in &scan_roots {
        unsafe { scan(s) };
    }
    let mut freed_nodes: Vec<*mut u8> = Vec::new();
    for s in scan_roots {
        let h = unsafe { header_ptr(s) };
        unsafe { set_buffered(h, false) };
        if unsafe { color_of(h) } == COLOR_WHITE {
            unsafe { collect_white(s, &mut freed_nodes) };
        }
    }
    // Reclaim the dead leftovers last: count 0 means nothing references them,
    // so no MarkGray traversal touched them.
    for s in dead {
        unsafe { try_reclaim(s) };
    }
    // Reconciliation: a garbage component reachable from this slice may include
    // members still sitting in the leftover candidate buffer (collected here as
    // part of the component). Drop their now-dangling pointers from the buffer
    // so a later slice never dereferences freed memory. Only a bounded slice
    // can leave a non-empty buffer; a full drain emptied it up front, so the
    // retain is a cheap no-op there.
    if !freed_nodes.is_empty() {
        let dropped: std::collections::HashSet<usize> =
            freed_nodes.iter().map(|p| *p as usize).collect();
        ROOTS.with(|r| r.borrow_mut().remove_all(&dropped));
    }
    // Adapt the automatic arming threshold by this slice's yield. An explicit
    // full drain (budget None) does not perturb it.
    if budget.is_some() {
        let freed = CYCLES_FREED
            .load(Ordering::Relaxed)
            .wrapping_sub(freed_before);
        COLLECT_THRESHOLD.with(|t| {
            // Reclaimed under 1/8 of the candidates scanned: a mostly-live
            // candidate graph, so double the threshold (capped) to stop
            // rescanning it. Otherwise reset to the eager base.
            if freed.saturating_mul(8) < candidates {
                t.set(t.get().saturating_mul(2).min(COLLECT_THRESHOLD_MAX));
            } else {
                t.set(COLLECT_THRESHOLD_BASE);
            }
        });
    }
}

/// Run the cycle collector now, reclaiming any unreachable cyclic RC
/// garbage accumulated in the candidate buffer. Exposed to user code as
/// `runtime::collect_cycles()`; also triggered automatically when the
/// candidate buffer crosses its threshold.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_collect_cycles() {
    unsafe { collect_cycles() };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
        arena_retire(standard, REGION_SLAB_BYTES);
        arena_retire(oversized, REGION_SLAB_BYTES + os_page_size());
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
        let p = unsafe { gos_rt_rc_alloc((payload_words * 8) as u64, meta) };
        assert!(!p.is_null());
        unsafe { (*header_ptr(p)).disc = u8::try_from(disc).unwrap_or(0) };
        p
    }

    unsafe fn set_child(parent: *mut u8, word: usize, child: *mut u8) {
        let slot = unsafe { parent.add(word * 8) as *mut *mut u8 };
        unsafe { *slot = child };
    }

    unsafe fn strong_of(payload: *mut u8) -> usize {
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
        unsafe { set_child(parent, 1, child) };
        unsafe { gos_rt_rc_retain(child) };
    }

    /// Move `child` into `parent`'s slot: set the edge without a retain,
    /// transferring the existing reference (the unique-ownership shape, like
    /// `Node { child: existing_b }` where `b` is used once and not aliased).
    unsafe fn move_child(parent: *mut u8, child: *mut u8) {
        unsafe { set_child(parent, 1, child) };
    }

    /// Drain any candidates left buffered by earlier tests on this thread so
    /// each cycle test starts from a clean candidate buffer (`ROOTS` is
    /// thread-local and tests share worker threads).
    fn fresh_cycle_state() {
        unsafe { gos_rt_collect_cycles() };
    }

    #[test]
    fn two_node_cycle_is_collected() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let freed_base = rc_cycles_freed();
        let meta = node_meta();
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
        unsafe { (*header_ptr(payload)).weak.load(Ordering::Relaxed) as usize }
    }

    #[test]
    fn downgrade_increments_weak_not_strong() {
        let _g = count_guard();
        let base = rc_live_count();
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
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            let opt = gos_rt_rc_weak_upgrade_opt(p);
            assert_eq!(
                crate::c_abi::vec::gos_rt_result_disc(opt),
                0,
                "alive referent is Some"
            );
            assert_eq!(
                crate::c_abi::vec::gos_rt_result_payload(opt),
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
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            gos_rt_rc_downgrade(p);
            gos_rt_rc_release(p);
            let opt = gos_rt_rc_weak_upgrade_opt(p);
            assert_eq!(
                crate::c_abi::vec::gos_rt_result_disc(opt),
                1,
                "dead referent is None"
            );
            assert_eq!(crate::c_abi::vec::gos_rt_result_payload(opt), 0);
            gos_rt_rc_weak_release(p);
        }
        assert_eq!(rc_live_count(), base);
    }

    #[test]
    fn upgrade_after_last_strong_release_returns_null() {
        let _g = count_guard();
        let base = rc_live_count();
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

    #[test]
    fn weak_count_saturates_instead_of_wrapping() {
        // The weak count is an 8-bit field. More than 255 live `Weak`s drive
        // it past `u8::MAX`; a bare wrapping `fetch_add` would roll the count
        // back to a small value (300 increments -> 44, or exactly 256 -> 0),
        // and a wrap to 0 would let `try_reclaim` free a block that 256
        // outstanding weaks still observe (use-after-free). Saturating the
        // count pins it at `u8::MAX` and leaks the block instead.
        let _g = count_guard();
        let base = rc_live_count();
        const WEAKS: usize = 300;
        unsafe {
            let p = gos_rt_rc_alloc(8, std::ptr::null());
            for _ in 0..WEAKS {
                gos_rt_rc_weak_retain(p);
            }
            // Pinned at the maximum, not wrapped to `WEAKS % 256` (= 44).
            assert_eq!(weak_of(p), u8::MAX as usize, "weak count pins at u8::MAX");

            // The referent dies, but the block must survive: every one of the
            // 300 weaks still observes it.
            gos_rt_rc_release(p);
            assert_eq!(strong_of(p), 0, "strong release destroyed the payload");
            assert_eq!(
                rc_live_count(),
                base + 1,
                "saturated weak count pins the block; it is never reclaimed \
                 while weaks may still observe it"
            );

            // A pinned count is immortal: releasing weaks never decrements it,
            // so `try_reclaim` can never be re-enabled and free the block out
            // from under the remaining (uncounted) weaks.
            for _ in 0..WEAKS {
                gos_rt_rc_weak_release(p);
            }
            assert_eq!(
                weak_of(p),
                u8::MAX as usize,
                "pinned weak count is never decremented"
            );
            assert_eq!(
                rc_live_count(),
                base + 1,
                "the block stays pinned (leaked) rather than risk a use-after-free"
            );
        }
    }

    #[test]
    fn strong_release_with_outstanding_weak_still_releases_children() {
        let _g = count_guard();
        let base = rc_live_count();
        let meta = tree_meta();
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
        unsafe {
            gos_rt_rc_retain(std::ptr::null_mut());
            gos_rt_rc_release(std::ptr::null_mut());
        }
    }

    #[test]
    fn leaf_with_no_meta_frees_cleanly() {
        let _g = count_guard();
        let base = rc_live_count();
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
        unsafe { set_child(parent, word, child) };
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
    fn guarded_copy_blob_owns_a_versioned_carrier_without_a_side_table() {
        let _g = count_guard();
        fresh_cycle_state();
        let base = rc_live_count();
        let meta = [RC_KIND_STRUCT_GUARDED, 0];
        let source = [17_u64];
        unsafe {
            let first = gos_rt_rc_alloc_copy(8, meta.as_ptr(), source.as_ptr().cast());
            let first_owner = copy_blob_owner(first).expect("copy blob owner");
            assert_eq!(first_owner.meta, meta.as_ptr());
            assert_ne!(first_owner.generation, 0);
            assert_eq!(unsafe { first.cast::<u64>().read() }, 17);
            let first_generation = first_owner.generation;
            gos_rt_rc_release(first);

            let second = gos_rt_rc_alloc_copy(8, meta.as_ptr(), source.as_ptr().cast());
            let second_owner = copy_blob_owner(second).expect("copy blob owner");
            assert_ne!(second_owner.generation, first_generation);
            gos_rt_rc_release(second);
        }
        assert_eq!(rc_live_count(), base);
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
}
