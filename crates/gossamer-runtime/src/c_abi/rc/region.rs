//! Arena regions: bump allocation for `region { }` blocks out of one reserved virtual range.

use super::*;

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
pub(super) const REGION_SLAB_BYTES: usize = 1 << 20;

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
pub(super) const REGION_ARENA_BYTES: usize = 1 << 36;
#[cfg(target_arch = "wasm32")]
pub(super) const REGION_ARENA_BYTES: usize = 1 << 30;

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
pub(super) struct ArenaFreeSlabs<const N: usize> {
    head: AtomicUsize,
    next: [AtomicUsize; N],
}

impl<const N: usize> ArenaFreeSlabs<N> {
    pub(super) const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            next: [const { AtomicUsize::new(0) }; N],
        }
    }

    #[inline]
    pub(super) fn pop(&self) -> Option<usize> {
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
    pub(super) fn push(&self, index: usize) -> bool {
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
pub(super) fn addr_in_region_arena(addr: usize, base: usize) -> bool {
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
pub(super) fn is_odd_string_repr(p: *const u8) -> bool {
    !p.is_null() && (p as usize & 1) != 0
}

/// Reserve the arena on first use. Returns the base, or `usize::MAX`
/// when virtual reservation is unavailable.
pub(super) fn region_arena_base() -> usize {
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
                // SAFETY: `reserved` is the mapping `arena_reserve` made above, which lost the
                // race and is used nowhere.
                unsafe { arena_release(reserved, REGION_ARENA_BYTES) };
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

/// # Safety
///
/// `p` and `len` name a mapping `arena_reserve` made, which nothing uses
/// afterwards.
#[cfg(unix)]
unsafe fn arena_release(p: *mut u8, len: usize) {
    // SAFETY: releasing exactly the mapping created in arena_reserve.
    unsafe { libc::munmap(p.cast(), len) };
}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made.
#[cfg(unix)]
unsafe fn arena_commit(p: *mut u8, len: usize) -> bool {
    // SAFETY: p..p+len lies inside our reservation.
    unsafe { libc::mprotect(p.cast(), len, libc::PROT_READ | libc::PROT_WRITE) == 0 }
}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made, whose
/// contents nothing reads again before they are written.
#[cfg(unix)]
unsafe fn arena_decommit(p: *mut u8, len: usize) {
    // Return the physical pages and make the range inaccessible until
    // `arena_commit` takes it again, as a decommit does on Windows: a read
    // of retired region memory faults on every platform rather than
    // answering zeros on this one.
    // SAFETY: range lies inside our reservation.
    unsafe {
        libc::madvise(p.cast(), len, libc::MADV_DONTNEED);
        libc::mprotect(p.cast(), len, libc::PROT_NONE);
    }
}

#[cfg(windows)]
fn arena_reserve(len: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: plain reservation, no aliasing.
    unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_NOACCESS).cast() }
}

/// # Safety
///
/// `p` and `len` name a mapping `arena_reserve` made, which nothing uses
/// afterwards.
#[cfg(windows)]
unsafe fn arena_release(p: *mut u8, _len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: releasing exactly the reservation from arena_reserve.
    unsafe { VirtualFree(p.cast(), 0, MEM_RELEASE) };
}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made.
#[cfg(windows)]
unsafe fn arena_commit(p: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: committing pages inside our reservation.
    !unsafe { VirtualAlloc(p.cast(), len, MEM_COMMIT, PAGE_READWRITE) }.is_null()
}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made, whose
/// contents nothing reads again before they are written.
#[cfg(windows)]
unsafe fn arena_decommit(p: *mut u8, len: usize) {
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

/// # Safety
///
/// `p` and `len` name a mapping `arena_reserve` made, which nothing uses
/// afterwards.
#[cfg(not(any(unix, windows)))]
unsafe fn arena_release(_p: *mut u8, _len: usize) {}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made.
#[cfg(not(any(unix, windows)))]
unsafe fn arena_commit(_p: *mut u8, _len: usize) -> bool {
    false
}

/// # Safety
///
/// `p` and `len` name a range inside a reservation `arena_reserve` made, whose
/// contents nothing reads again before they are written.
#[cfg(not(any(unix, windows)))]
unsafe fn arena_decommit(_p: *mut u8, _len: usize) {}

/// Carve (or re-commit) a slab of `slab_size` bytes from the arena.
/// Null when the arena is unavailable or exhausted - callers fall back
/// to headered global allocation (sound, just unoptimised).
/// Host page size, queried once. Slab offsets inside the reserved
/// arena must be page-multiples or `mprotect` / `VirtualAlloc`
/// rejects the commit - and the size is NOT universally 4 KiB
/// (macOS arm64 and some aarch64 Linux kernels use 16 KiB or 64 KiB
/// pages).
pub(super) fn os_page_size() -> usize {
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
        // SAFETY: `SYSTEM_INFO` is a plain C struct whose all-zero value is valid.
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

pub(super) fn arena_acquire(slab_size: usize) -> *mut u8 {
    let base = region_arena_base();
    if base == usize::MAX {
        return std::ptr::null_mut();
    }
    if slab_size == REGION_SLAB_BYTES {
        if let Some(index) = REGION_ARENA_FREE.pop() {
            let off = index * REGION_SLAB_BYTES;
            let p = (base + off) as *mut u8;
            // SAFETY: `p` is a free slab's range inside the reservation starting at `base`.
            if unsafe { arena_commit(p, slab_size) } {
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
    // SAFETY: `p` and `rounded` name a range inside the reservation starting at `base` (bounds
    // checked above).
    if unsafe { arena_commit(p, rounded) } {
        p
    } else {
        std::ptr::null_mut()
    }
}

/// Decommit a no-longer-needed standard slab and remember its offset
/// for re-commit. Oversized slabs are decommitted and their address
/// range retired (rare; bounded by peak oversized use).
///
/// # Safety
///
/// `p` is a slab `arena_acquire` returned, `slab_size` bytes long, which
/// nothing uses afterwards.
pub(super) unsafe fn arena_retire(p: *mut u8, slab_size: usize) {
    let base = REGION_ARENA_BASE.load(Ordering::Relaxed);
    // SAFETY: this function's contract covers `p`, `slab_size`, as `arena_decommit` requires.
    unsafe { arena_decommit(p, slab_size) };
    if slab_size == REGION_SLAB_BYTES {
        let off = p as usize - base;
        debug_assert_eq!(off % REGION_SLAB_BYTES, 0);
        if REGION_ARENA_FREE.push(off / REGION_SLAB_BYTES) {
            REGION_ARENA_FREE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Releases what a runtime handle placed in region storage holds.
type Finalizer = unsafe fn(*mut u8);

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
    /// Element buffers of this region's vectors too large to bump-allocate,
    /// as `(buffer, bytes)`. They come from the allocator, so a slab never
    /// holds one and a freed one is reused by the next iteration; a growing
    /// vector replaces its entry rather than abandoning the old buffer.
    /// Freed one by one at pop.
    buffers: Vec<(*mut u8, usize)>,
    /// Runtime handles placed in this region's storage by
    /// [`region_alloc_handle`], each with the function that releases what it
    /// holds. Run at pop, last created first, before the slabs go.
    finalizers: Vec<(*mut u8, Finalizer)>,
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

// SAFETY: region slabs are uniquely owned by the suspended goroutine. They are
// moved only between scheduler steps, when no worker can concurrently access
// them.
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
pub(super) struct BumpState {
    pub(super) base: *mut u8,
    pub(super) cur: usize,
    pub(super) end: usize,
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
    pub(super) static BUMP: std::cell::Cell<BumpState> = const { std::cell::Cell::new(BumpState::EMPTY) };
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
pub(super) fn region_active() -> bool {
    REGION_DEPTH.with(|d| d.get() > 0)
}

/// Nesting depth of the arena regions open on this thread.
pub(crate) fn region_depth() -> usize {
    REGION_DEPTH.with(std::cell::Cell::get)
}

/// Closes every region opened on this thread above `depth`, for a unit of
/// work that unwound out of regions it never reached the close of.
pub(crate) fn close_regions_above(depth: usize) {
    while region_depth() > depth {
        gos_rt_arena_pop();
    }
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
pub(super) fn region_alloc_inner_unzeroed(total: usize, count_obj: bool) -> *mut u8 {
    region_alloc_inner_impl(total, count_obj, false)
}

fn region_alloc_inner_impl(total: usize, count_obj: bool, zero: bool) -> *mut u8 {
    let need = (total + RC_ALIGN - 1) & !(RC_ALIGN - 1);
    // Hot path: bump within the innermost region's current slab - no RefCell,
    // no Vec walk, just a compare and an add on a thread-local cache.
    let ptr = BUMP.with(|b| {
        let st = b.get();
        if !st.base.is_null() && st.cur + need <= st.end {
            // SAFETY: `st.cur + need <= st.end` (checked above), so the address lies inside the
            // bump slab.
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
        // SAFETY: `ptr` addresses the `need` bytes just handed out.
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
pub(super) fn region_alloc(total: usize) -> *mut u8 {
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
            buffers: Vec::new(),
            finalizers: Vec::new(),
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

/// Places `value`, a runtime handle such as a map or set, in the innermost
/// open region, to be finalized by `finalize` at that region's pop. A handle
/// created while a region runs belongs to the region like every other value
/// made there: a region object can hold it, and the bulk free at pop must
/// release what it holds. Living in region storage, it answers the same
/// address test every free path makes, so a release after the pop reads
/// nothing. Gives `value` back when no region is open or its alignment
/// exceeds the region's.
pub(crate) fn region_alloc_handle<T>(value: T, finalize: Finalizer) -> Result<*mut T, T> {
    if std::mem::align_of::<T>() > RC_ALIGN {
        return Err(value);
    }
    let p = region_alloc_bytes(std::mem::size_of::<T>().max(1));
    if p.is_null() {
        return Err(value);
    }
    let handle = p.cast::<T>();
    // SAFETY: `p` is fresh region storage of `T`'s size at `RC_ALIGN`, which
    // covers `T`'s alignment (checked above).
    unsafe { handle.write(value) };
    REGIONS.with(|r| {
        if let Some(top) = r.borrow_mut().last_mut() {
            top.finalizers.push((p, finalize));
        }
    });
    Ok(handle)
}

/// Vec element buffers at least this large belong to the allocator rather
/// than to a slab: bump allocation buys a single large buffer nothing, and a
/// slab sized for one is decommitted at every pop and faulted back in by the
/// next iteration.
const REGION_BUFFER_BYTES: usize = 64 << 10;

/// Whether a region vector's buffer of `bytes` is one the region owns
/// through [`region_own_buffer`] rather than bump-allocates.
pub(crate) fn region_owns_buffers_of(bytes: usize) -> bool {
    bytes >= REGION_BUFFER_BYTES
}

/// Records `buf`, an allocator buffer of `bytes`, as owned by the innermost
/// region, to be freed at its pop.
pub(crate) fn region_own_buffer(buf: *mut u8, bytes: usize) {
    REGIONS.with(|r| {
        if let Some(top) = r.borrow_mut().last_mut() {
            top.buffers.push((buf, bytes));
        }
    });
}

/// Replaces `old`, a buffer an open region owns, with `new` of `bytes` in
/// that same region, innermost first; `false` when no open region owns
/// `old`. The replacement stays with the region the vector belongs to,
/// whichever region is innermost when it grows.
pub(crate) fn region_replace_buffer(old: *mut u8, new: *mut u8, bytes: usize) -> bool {
    REGIONS.with(|r| {
        let mut regions = r.borrow_mut();
        for region in regions.iter_mut().rev() {
            if let Some(entry) = region.buffers.iter_mut().find(|(p, _)| *p == old) {
                *entry = (new, bytes);
                return true;
            }
        }
        false
    })
}

/// Removes `buf` from whichever open region owns it, without freeing it,
/// for the caller to free; `false` when no open region owns `buf`.
pub(crate) fn region_disown_buffer(buf: *mut u8) -> bool {
    REGIONS.with(|r| {
        for region in r.borrow_mut().iter_mut().rev() {
            if let Some(i) = region.buffers.iter().position(|(p, _)| *p == buf) {
                region.buffers.swap_remove(i);
                return true;
            }
        }
        false
    })
}

/// Whether `GOS_ARENA_POISON` asks every pop to retire its slabs rather than
/// keep them for the next region, so that any read of a region's memory
/// after its pop faults instead of finding a recycled slab.
fn arena_poison() -> bool {
    static POISON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *POISON.get_or_init(|| std::env::var_os("GOS_ARENA_POISON").is_some())
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
    for (handle, finalize) in region.finalizers.into_iter().rev() {
        // SAFETY: each handle was placed by `region_alloc_handle` with its own
        // finalizer while this region was open, is reachable from nothing
        // after the pop, and is finalized once, here, before its slab goes.
        unsafe { finalize(handle) };
    }
    for (buf, bytes) in region.buffers {
        // SAFETY: each buffer was recorded by `region_own_buffer` or
        // `region_replace_buffer` with the size `alloc_vec_buffer` gave it,
        // belongs to a vector of this region that nothing reaches after the
        // pop, and is freed once, here.
        unsafe { crate::c_abi::vec::free_vec_buffer(buf, bytes) };
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
    let retain = if arena_poison() {
        0
    } else {
        SLAB_RETAIN.with(|r| {
            let widened = r.get().max(width).min(FREE_SLAB_CEILING);
            r.set(widened);
            widened
        })
    };
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
        // SAFETY: `base` is the slab this region acquired, `size` bytes long, which the region no
        // longer uses.
        unsafe { arena_retire(base, size) };
    }
}
