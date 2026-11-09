//! Signal-driven CPU sampler.
//!
//! A profiler cannot instrument: a runtime call at every function entry
//! costs 2.7x on call-heavy code, because it is an inlining barrier.
//! Instead a timer interrupts the running thread and the handler reads
//! the stack that is already there, which costs nothing until it fires.
//!
//! Everything the handler touches is async-signal-safe: it writes raw
//! program counters into a fixed array under an atomic index, allocating
//! nothing and taking no lock. Turning those addresses into names is
//! done by whoever drains the buffer, where allocation is allowed.
//!
//! The stack walk follows the frame-pointer chain, which is why compiled
//! bodies carry `"frame-pointer"="all"`. DWARF unwinding would be more
//! precise and is not async-signal-safe.

#![allow(clippy::missing_safety_doc)]

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Frames recorded per sample. Deep enough to reach through a recursive
/// program's hot region, small enough that the buffer stays a fixed,
/// signal-safe allocation.
pub const MAX_FRAMES: usize = 32;

/// Samples held before the buffer stops accepting more. At 100 Hz this
/// is 40 seconds, and each `cpu_profile` call drains it.
///
/// The storage is a zeroed static, so it costs address space rather than
/// resident memory until a sample is actually written - but two rings at
/// this size is already about 2 MiB in every binary, which is why it is
/// sized to a profiling window rather than to the largest one imaginable.
const CAPACITY: usize = 4_096;

/// One captured stack, as raw return addresses.
#[derive(Clone, Copy)]
pub struct RawSample {
    /// Return addresses, innermost first.
    pub frames: [usize; MAX_FRAMES],
    /// How many of `frames` are populated.
    pub len: u8,
}

impl RawSample {
    const EMPTY: Self = Self {
        frames: [0; MAX_FRAMES],
        len: 0,
    };
}

/// A ring slot and its turn: on the ring's lap `L` the slot is free for a
/// recorder at turn `2L` and holds a sample for the drainer at `2L + 1`.
struct Slot {
    turn: AtomicUsize,
    sample: UnsafeCell<RawSample>,
}

/// Fixed sample storage shared between recorders and a drainer.
///
/// Not a lock: a recorder can be a signal handler, which may interrupt
/// code holding any lock in the process and so may not take one. A slot's
/// turn hands it over instead. A recorder claims a position, writes the
/// slot, then publishes it; the drainer reads only published slots and
/// frees each for the next lap. A slot still being written stops the drain
/// there, and a full ring drops new samples rather than overwrite unread
/// ones.
struct SampleRing {
    slots: [Slot; CAPACITY],
    /// The next position a recorder claims; its slot is `pos % CAPACITY`.
    head: AtomicUsize,
    /// The next position the drainer reads.
    tail: AtomicUsize,
}

// SAFETY: a slot's sample is written only by the recorder that claimed its
// position while the turn says free, and read only by the drainer that
// claimed it while the turn says published; the turn's release and acquire
// order each hand-over.
unsafe impl Sync for SampleRing {}

impl SampleRing {
    // The array lives in a static, not on a stack: `const fn` initialises
    // the storage in place.
    #[allow(
        clippy::large_stack_arrays,
        reason = "initialises a static, not a local"
    )]
    const fn new() -> Self {
        Self {
            slots: [const {
                Slot {
                    turn: AtomicUsize::new(0),
                    sample: UnsafeCell::new(RawSample::EMPTY),
                }
            }; CAPACITY],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Stores `sample`, or drops it while the ring holds `CAPACITY` samples
    /// not yet drained; answers whether the ring took it.
    fn push(&self, sample: &RawSample) -> bool {
        let mut pos = self.head.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos % CAPACITY];
            let free = 2 * (pos / CAPACITY);
            match slot.turn.load(Ordering::Acquire).cmp(&free) {
                std::cmp::Ordering::Equal => match self.head.compare_exchange_weak(
                    pos,
                    pos + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: claiming `pos` while the turn says free makes
                        // this recorder the slot's only user until it publishes.
                        unsafe { slot.sample.get().write(*sample) };
                        slot.turn.store(free + 1, Ordering::Release);
                        return true;
                    }
                    Err(now) => pos = now,
                },
                // The slot still holds the previous lap's sample.
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Greater => pos = self.head.load(Ordering::Relaxed),
            }
        }
    }

    /// Takes the published samples in order, up to the first slot a
    /// recorder has claimed but not yet published.
    fn drain(&self) -> Vec<RawSample> {
        let mut out = Vec::new();
        let mut pos = self.tail.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos % CAPACITY];
            let published = 2 * (pos / CAPACITY) + 1;
            match slot.turn.load(Ordering::Acquire).cmp(&published) {
                std::cmp::Ordering::Equal => match self.tail.compare_exchange_weak(
                    pos,
                    pos + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: claiming `pos` while the turn says published
                        // makes this drainer the slot's only user until it frees it.
                        let sample = unsafe { slot.sample.get().read() };
                        slot.turn.store(published + 1, Ordering::Release);
                        out.push(sample);
                        pos += 1;
                    }
                    Err(now) => pos = now,
                },
                std::cmp::Ordering::Less => return out,
                std::cmp::Ordering::Greater => pos = self.tail.load(Ordering::Relaxed),
            }
        }
    }
}

/// The CPU sample ring.
static SAMPLES: SampleRing = SampleRing::new();

/// Whether the timer is armed. The handler returns immediately when it
/// is not, so a signal delivered during teardown records nothing.
static SAMPLING: AtomicBool = AtomicBool::new(false);

/// Whether sampling is currently armed.
#[must_use]
pub fn is_sampling() -> bool {
    SAMPLING.load(Ordering::Relaxed)
}

/// Starts sampling at `hz`, replacing any previous rate.
///
/// # Errors
///
/// Returns the OS error when the timer or handler cannot be installed.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
pub fn start(hz: u32) -> std::io::Result<()> {
    install_handler()?;
    SAMPLING.store(true, Ordering::SeqCst);
    set_timer(hz)
}

/// Sampling is a no-op where there is no timer signal to drive it.
#[cfg(not(all(unix, not(miri), not(target_arch = "wasm32"))))]
pub fn start(_hz: u32) -> std::io::Result<()> {
    Ok(())
}

/// Stops sampling and disarms the timer.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
pub fn stop() {
    SAMPLING.store(false, Ordering::SeqCst);
    let _ = set_timer(0);
}

/// Stopping is a no-op where there is no timer to disarm.
#[cfg(not(all(unix, not(miri), not(target_arch = "wasm32"))))]
pub fn stop() {}

/// Takes everything recorded since the last drain.
#[must_use]
pub fn drain() -> Vec<RawSample> {
    SAMPLES.drain()
}

/// `micros` as the platform's `tv_usec` type. Generic so one spelling holds
/// where that type is `i32` (Darwin) and where it is `i64` (glibc, musl).
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn widen_micros<T: From<i32>>(micros: i32) -> T {
    T::from(micros)
}

#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn set_timer(hz: u32) -> std::io::Result<()> {
    // Written without naming `suseconds_t`: it is `i64` on glibc, `i32` on
    // Darwin, and deprecated on musl. An `i32` holds any microsecond count
    // this produces (at most 1_000_000), and `widen_micros` converts it to
    // whichever width the platform's `tv_usec` actually is.
    let micros: i32 = i32::try_from(1_000_000 / hz.max(1)).unwrap_or(10_000);
    let interval = libc::timeval {
        tv_sec: 0,
        tv_usec: widen_micros(if hz == 0 { 0 } else { micros }),
    };
    let spec = libc::itimerval {
        it_interval: interval,
        it_value: interval,
    };
    // SAFETY: `spec` is a fully-initialised itimerval we own; ITIMER_PROF
    // is a valid timer id.
    let rc = unsafe { libc::setitimer(libc::ITIMER_PROF, &raw const spec, std::ptr::null_mut()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn install_handler() -> std::io::Result<()> {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    let mut result = Ok(());
    ONCE.call_once(|| {
        // SAFETY: zero-initialising a sigaction is valid; the mask is
        // emptied immediately below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_sigprof as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART | libc::SA_ONSTACK;
        // SAFETY: action is a stack-local sigaction we own.
        unsafe {
            libc::sigemptyset(&raw mut action.sa_mask);
            if libc::sigaction(libc::SIGPROF, &raw const action, std::ptr::null_mut()) != 0 {
                result = Err(std::io::Error::last_os_error());
            }
        }
    });
    result
}

/// SIGPROF handler. Async-signal-safe: no allocation, no locks, no library
/// calls; stack memory is read in place only inside known bounds, and
/// otherwise through the kernel's checked copy.
///
/// # Safety
///
/// Called only by the kernel as the SIGPROF handler: `context` is null or the
/// `ucontext_t` it delivers.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
unsafe extern "C" fn on_sigprof(
    _sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    if !SAMPLING.load(Ordering::Relaxed) {
        return;
    }
    let mut sample = RawSample::EMPTY;
    // SAFETY: this handler's contract makes `context` the kernel's `ucontext_t` or null, as
    // `interrupted_registers` requires.
    let (pc, frame_pointer, stack_pointer) = unsafe { interrupted_registers(context) };
    if pc != 0 {
        sample.frames[0] = pc;
        sample.len = 1;
    }
    walk_frames(frame_pointer, signal_links(stack_pointer), &mut sample);
    if sample.len == 0 {
        return;
    }
    let _ = SAMPLES.push(&sample);
}

/// Program counter, frame pointer and stack pointer of the interrupted
/// context.
#[cfg(all(
    unix,
    not(miri),
    not(target_arch = "wasm32"),
    target_arch = "x86_64",
    target_os = "linux"
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize, usize) {
    if context.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let regs = &ctx.uc_mcontext.gregs;
    (
        regs[libc::REG_RIP as usize] as usize,
        regs[libc::REG_RBP as usize] as usize,
        regs[libc::REG_RSP as usize] as usize,
    )
}

/// The aarch64 register file names the program counter directly and keeps
/// the frame pointer in `x29`.
#[cfg(all(
    unix,
    not(miri),
    not(target_arch = "wasm32"),
    target_arch = "aarch64",
    target_os = "linux"
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize, usize) {
    if context.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let machine = &ctx.uc_mcontext;
    (
        machine.pc as usize,
        machine.regs[29] as usize,
        machine.sp as usize,
    )
}

/// Darwin reaches the register file through a pointer rather than an
/// inline struct, and names the thread state per architecture.
#[cfg(all(
    not(miri),
    target_os = "macos",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize, usize) {
    if context.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler;
    // `uc_mcontext` is checked before it is dereferenced.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    if ctx.uc_mcontext.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: the same handler contract makes `uc_mcontext` a live
    // `__darwin_mcontext64` for the duration of the handler.
    let state = unsafe { &(*ctx.uc_mcontext).__ss };
    #[cfg(target_arch = "aarch64")]
    {
        (
            state.__pc as usize,
            state.__fp as usize,
            state.__sp as usize,
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        (
            state.__rip as usize,
            state.__rbp as usize,
            state.__rsp as usize,
        )
    }
}

/// FreeBSD keeps the register file inline in `mcontext_t`, named per
/// architecture: `mc_rip` / `mc_rbp` on amd64, `gp_elr` / `x29` on arm64.
#[cfg(all(
    not(miri),
    target_os = "freebsd",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize, usize) {
    if context.is_null() {
        return (0, 0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler.
    let machine = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext };
    #[cfg(target_arch = "aarch64")]
    {
        (
            machine.mc_gpregs.gp_elr as usize,
            machine.mc_gpregs.gp_x[29] as usize,
            machine.mc_gpregs.gp_sp as usize,
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        (
            machine.mc_rip as usize,
            machine.mc_rbp as usize,
            machine.mc_rsp as usize,
        )
    }
}

/// Other platforms record the frames the walk can reach without the
/// interrupted register file.
#[cfg(all(
    unix,
    not(miri),
    not(target_arch = "wasm32"),
    not(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos", target_os = "freebsd")
    ))
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(_context: *mut libc::c_void) -> (usize, usize, usize) {
    (0, 0, 0)
}

/// How a walk may read the frame records of the stack it runs on.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
#[derive(Clone, Copy)]
enum Links {
    /// In place, inside the stack's `(lo, hi)` bounds.
    Within((usize, usize)),
    /// Through the kernel's checked copy, at or above the interrupted stack
    /// pointer.
    Copied { floor: usize },
    /// Not at all: the sample keeps only the interrupted program counter.
    Unreadable,
}

#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
impl Links {
    /// The saved frame pointer and return address of the record at
    /// `frame_pointer`, which may be anything the interrupted code left in
    /// the register.
    fn read(self, frame_pointer: usize) -> Option<(usize, usize)> {
        if frame_pointer == 0 || !frame_pointer.is_multiple_of(std::mem::align_of::<usize>()) {
            return None;
        }
        match self {
            Self::Within((lo, hi)) => {
                if frame_pointer < lo
                    || frame_pointer.saturating_add(2 * std::mem::size_of::<usize>()) > hi
                {
                    return None;
                }
                // SAFETY: the address is aligned and both words it addresses lie
                // inside the running stack's mapped bounds.
                Some(unsafe {
                    let base = frame_pointer as *const usize;
                    (base.read_volatile(), base.add(1).read_volatile())
                })
            }
            Self::Copied { floor } => {
                if frame_pointer < floor {
                    return None;
                }
                copy_link(frame_pointer)
            }
            Self::Unreadable => None,
        }
    }
}

/// The two words at `frame_pointer`, copied by the kernel, which refuses an
/// address the process cannot read instead of faulting.
#[cfg(all(not(miri), any(target_os = "linux", target_os = "android")))]
fn copy_link(frame_pointer: usize) -> Option<(usize, usize)> {
    let mut words = [0_usize; 2];
    let size = std::mem::size_of_val(&words);
    let local = libc::iovec {
        iov_base: words.as_mut_ptr().cast(),
        iov_len: size,
    };
    let remote = libc::iovec {
        iov_base: frame_pointer as *mut libc::c_void,
        iov_len: size,
    };
    // The interrupted code reads `errno` after the handler returns, so a
    // refused copy must not leave its own there.
    // SAFETY: the location is the calling thread's `errno`, valid for its life.
    let errno = unsafe { errno_location() };
    // SAFETY: `errno` points at this thread's errno, read before the copy.
    let saved = unsafe { errno.read() };
    // SAFETY: both vectors describe `size` bytes; the kernel writes only into
    // `words` and reports an unreadable `remote` as an error.
    let copied = unsafe {
        libc::process_vm_readv(libc::getpid(), &raw const local, 1, &raw const remote, 1, 0)
    };
    // SAFETY: as above; the write restores what the thread held.
    unsafe { errno.write(saved) };
    (usize::try_from(copied).ok() == Some(size)).then_some((words[0], words[1]))
}

/// The calling thread's `errno`.
///
/// # Safety
///
/// The pointer is valid only on the calling thread, while it lives.
#[cfg(all(not(miri), target_os = "linux"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: glibc and musl answer the calling thread's errno location.
    unsafe { libc::__errno_location() }
}

/// The calling thread's `errno`.
///
/// # Safety
///
/// The pointer is valid only on the calling thread, while it lives.
#[cfg(all(not(miri), target_os = "android"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: bionic answers the calling thread's errno location.
    unsafe { libc::__errno() }
}

/// Other platforms have no copy the handler may make without bounds.
#[cfg(all(
    unix,
    not(miri),
    not(target_arch = "wasm32"),
    not(any(target_os = "linux", target_os = "android"))
))]
fn copy_link(_frame_pointer: usize) -> Option<(usize, usize)> {
    None
}

/// How the SIGPROF handler reads the interrupted thread's frame records,
/// asking the C library nothing: a goroutine's stack and a thread the stack
/// guard was installed on have known bounds, and any other thread is walked
/// through the kernel's checked copy where there is one.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn signal_links(stack_pointer: usize) -> Links {
    if let Some(window) = gossamer_coro::goroutine_stack_bounds() {
        return Links::Within(window);
    }
    if let Some(window) = crate::stack_guard::installed_thread_stack_bounds() {
        return Links::Within(window);
    }
    // Darwin answers a thread asking about itself from its own thread record,
    // without a lock.
    #[cfg(target_os = "macos")]
    if let Some(window) = crate::stack_guard::current_thread_stack_bounds() {
        return Links::Within(window);
    }
    if cfg!(any(target_os = "linux", target_os = "android")) {
        Links::Copied {
            floor: stack_pointer,
        }
    } else {
        Links::Unreadable
    }
}

/// `(lo, hi)` bounds of the stack the calling code is running on, for a walk
/// outside a signal handler, which may ask the C library; asked once per
/// thread.
///
/// A goroutine runs on its own allocated stack, so its window comes from
/// the coroutine guard rather than from the OS thread. That window changes
/// as goroutines are scheduled onto a worker, so only the OS-thread bounds
/// - fixed for the life of the thread - are worth caching.
#[cfg(all(unix, not(miri), any(target_arch = "x86_64", target_arch = "aarch64")))]
fn stack_bounds() -> Option<(usize, usize)> {
    thread_local! {
        static THREAD_STACK: std::cell::Cell<Option<(usize, usize)>> =
            const { std::cell::Cell::new(None) };
    }
    if let Some(window) = gossamer_coro::goroutine_stack_bounds() {
        return Some(window);
    }
    if let Some(window) = crate::stack_guard::installed_thread_stack_bounds() {
        return Some(window);
    }
    // `Cell<Option<_>>` distinguishes "not yet asked" from "the platform
    // cannot tell us", so a platform that cannot report bounds is asked
    // once per thread rather than on every sample.
    THREAD_STACK.with(|cached| {
        if let Some(window) = cached.get() {
            return (window.1 > window.0).then_some(window);
        }
        let window = crate::stack_guard::current_thread_stack_bounds();
        cached.set(Some(window.unwrap_or((0, 0))));
        window
    })
}

/// Walks the frame-pointer chain, appending return addresses.
///
/// A frame pointer in a function compiled without one is whatever that
/// function left in the register, so every link is untrusted input that
/// `links` must prove readable before the walk uses it.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn walk_frames(mut frame_pointer: usize, links: Links, sample: &mut RawSample) {
    /// A frame larger than this is a sign the chain is not a chain.
    const MAX_FRAME_BYTES: usize = 1 << 20;

    while (sample.len as usize) < MAX_FRAMES {
        let Some((saved_fp, return_address)) = links.read(frame_pointer) else {
            return;
        };
        if return_address == 0 {
            return;
        }
        sample.frames[sample.len as usize] = return_address;
        sample.len += 1;
        // The chain climbs toward the stack base by a bounded amount.
        if saved_fp <= frame_pointer || saved_fp - frame_pointer > MAX_FRAME_BYTES {
            return;
        }
        frame_pointer = saved_fp;
    }
}

/// Bytes allocated between heap samples. Go's default; large enough that
/// the counter check is lost in allocation cost, small enough that a
/// profile of a short program is not empty.
pub const HEAP_SAMPLE_BYTES: usize = 512 * 1024;

/// Whether heap sampling is armed.
static HEAP_SAMPLING: AtomicBool = AtomicBool::new(false);

/// Bytes allocated since the last heap sample.
static HEAP_ACCUMULATOR: AtomicUsize = AtomicUsize::new(0);

/// The allocation sample ring.
static HEAP_SAMPLES: SampleRing = SampleRing::new();

/// Arms allocation sampling.
pub fn start_heap() {
    crate::c_abi::ledger::arm_instrumentation();
    HEAP_ACCUMULATOR.store(0, Ordering::SeqCst);
    HEAP_SAMPLING.store(true, Ordering::SeqCst);
}

/// Disarms allocation sampling.
pub fn stop_heap() {
    HEAP_SAMPLING.store(false, Ordering::SeqCst);
}

/// Takes every heap sample recorded since the last drain.
#[must_use]
pub fn drain_heap() -> Vec<RawSample> {
    HEAP_SAMPLES.drain()
}

/// Accounts `bytes` against the sample threshold, recording a stack when
/// it is crossed.
///
/// Called from the global allocator, so it must not allocate: doing so
/// would re-enter the allocator that called it. The frame walk writes
/// into a fixed array for exactly that reason.
///
/// `#[inline]` with the recording body out of line so a disarmed process
/// pays a relaxed load and a predicted branch inside `__rust_alloc`
/// rather than a call per allocation.
#[inline]
pub fn record_allocation(bytes: usize) {
    if !crate::c_abi::ledger::instrumentation_armed() {
        return;
    }
    record_allocation_slow(bytes);
}

/// The accumulate-and-capture half, kept out of line so the allocator's own
/// hot path carries only the gate. `start_heap_sampling` arms the shared
/// instrumentation flag, so reaching here means some instrumentation is on
/// and this checks whether it is this one.
#[cold]
#[inline(never)]
fn record_allocation_slow(bytes: usize) {
    if !HEAP_SAMPLING.load(Ordering::Relaxed) {
        return;
    }
    let total = HEAP_ACCUMULATOR.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if total < HEAP_SAMPLE_BYTES {
        return;
    }
    capture_heap_sample();
}

/// Clears the accumulator and publishes `sample`, which the platform
/// recorder has just captured.
///
/// The accumulator is cleared whether or not a stack was captured, so a
/// platform that cannot capture one accounts the next interval from zero
/// rather than re-entering the recorder on every subsequent allocation.
fn publish_heap_sample(sample: &RawSample) {
    HEAP_ACCUMULATOR.store(0, Ordering::Relaxed);
    if sample.len == 0 {
        return;
    }
    let _ = HEAP_SAMPLES.push(sample);
}

/// Establishes one frame record and records the stack above it.
///
/// The frame-pointer register only holds a frame pointer in code compiled
/// to keep one. The interpreter and the JIT keep it as a general register
/// (forcing frame pointers there costs double digits on tight loops), so
/// reading it in the recorder's caller yields whatever that function last
/// put in it. This shim owns the first link instead: its own record gives
/// the walk a return address into the allocating frame, and the chain
/// climbs from there through the frames that do keep a frame pointer, the
/// runtime archive linked into compiled binaries and Gossamer bodies,
/// which carry `"frame-pointer"="all"`.
#[cfg(all(unix, not(miri), target_arch = "x86_64"))]
#[unsafe(naked)]
extern "C" fn capture_heap_sample() {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "mov rdi, rbp",
        "call {record}",
        "pop rbp",
        "ret",
        record = sym record_from_frame,
    )
}

/// The aarch64 frame record has the same shape: the saved frame pointer
/// at the base and the return address one word above it.
#[cfg(all(unix, not(miri), target_arch = "aarch64"))]
#[unsafe(naked)]
extern "C" fn capture_heap_sample() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        "mov x0, x29",
        "bl {record}",
        "ldp x29, x30, [sp], #16",
        "ret",
        record = sym record_from_frame,
    )
}

/// Walks from the frame record its caller established.
#[cfg(all(unix, not(miri), any(target_arch = "x86_64", target_arch = "aarch64")))]
extern "C" fn record_from_frame(frame_pointer: usize) {
    let mut sample = RawSample::EMPTY;
    let links = stack_bounds().map_or(Links::Unreadable, Links::Within);
    walk_frames(frame_pointer, links, &mut sample);
    publish_heap_sample(&sample);
}

/// Windows walks with the OS unwinder rather than the frame-pointer
/// chain: `RtlCaptureStackBackTrace` reads the unwind metadata every
/// Windows binary carries, so it reaches the allocating frame without
/// requiring the runtime to keep a frame pointer. It allocates nothing,
/// which is what the allocator-internal caller requires.
///
/// `inline(never)` keeps this one frame between the allocating code and
/// the capture, which is the frame the skip count drops.
#[cfg(all(windows, not(miri)))]
#[inline(never)]
fn capture_heap_sample() {
    let mut captured: [*mut core::ffi::c_void; MAX_FRAMES] = [std::ptr::null_mut(); MAX_FRAMES];
    // SAFETY: the OS writes at most `MAX_FRAMES` entries into a buffer we
    // own; a null hash pointer asks it not to compute one.
    let depth = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(
            1,
            MAX_FRAMES as u32,
            captured.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    let mut sample = RawSample::EMPTY;
    sample.len = u8::try_from(depth).unwrap_or(0).min(MAX_FRAMES as u8);
    for (frame, address) in sample
        .frames
        .iter_mut()
        .zip(&captured[..sample.len as usize])
    {
        *frame = *address as usize;
    }
    publish_heap_sample(&sample);
}

/// Platforms with neither a frame record this code can establish nor an
/// OS unwinder account the bytes and record no stack.
#[cfg(not(any(
    all(unix, not(miri), any(target_arch = "x86_64", target_arch = "aarch64")),
    all(windows, not(miri))
)))]
fn capture_heap_sample() {
    publish_heap_sample(&RawSample::EMPTY);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorders on several threads and a drainer at once: every sample the
    /// ring took comes out once and whole, and none it dropped does.
    #[test]
    fn concurrent_recorders_and_a_drainer_hand_over_whole_samples() {
        static RING: SampleRing = SampleRing::new();
        const THREADS: usize = 4;
        const EACH: usize = 20_000;
        let done = std::sync::Arc::new(AtomicUsize::new(0));
        let mut recorders = Vec::new();
        for t in 0..THREADS {
            let done = done.clone();
            recorders.push(std::thread::spawn(move || {
                let mut taken = Vec::new();
                for i in 0..EACH {
                    let id = t * EACH + i + 1;
                    let mut sample = RawSample::EMPTY;
                    sample.frames[0] = id;
                    sample.frames[1] = !id;
                    sample.len = 2;
                    if RING.push(&sample) {
                        taken.push(id);
                    }
                }
                done.fetch_add(1, Ordering::SeqCst);
                taken
            }));
        }
        let mut drained = Vec::new();
        loop {
            let finished = done.load(Ordering::SeqCst) == THREADS;
            for sample in RING.drain() {
                assert_eq!(sample.frames[1], !sample.frames[0], "a torn sample");
                drained.push(sample.frames[0]);
            }
            if finished {
                break;
            }
        }
        drained.extend(RING.drain().iter().map(|sample| sample.frames[0]));
        let mut taken: Vec<usize> = recorders
            .into_iter()
            .flat_map(|r| r.join().expect("a recorder"))
            .collect();
        taken.sort_unstable();
        drained.sort_unstable();
        assert_eq!(drained, taken);
    }

    /// The sampler is one process-wide ring, so a test that arms, records, or
    /// drains it observes every other test doing the same. Hold this while
    /// touching that state so the arming of one test is never read as the
    /// recording of another.
    static SAMPLER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Locks the shared sampler, ignoring poisoning: a failed assertion in one
    /// test leaves the ring dirty, and the next test resets it anyway.
    fn sampler_lock() -> std::sync::MutexGuard<'static, ()> {
        SAMPLER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn draining_an_idle_sampler_yields_nothing() {
        let _guard = sampler_lock();
        stop();
        let _ = drain();
        assert!(drain().is_empty());
    }

    /// The walk treats the chain as untrusted: a null or misaligned
    /// frame pointer ends it rather than dereferencing.
    #[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
    #[test]
    fn the_walk_refuses_an_implausible_chain() {
        for links in [
            crate::stack_guard::current_thread_stack_bounds()
                .map_or(Links::Unreadable, Links::Within),
            Links::Copied { floor: 0 },
        ] {
            let mut sample = RawSample::EMPTY;
            walk_frames(0, links, &mut sample);
            assert_eq!(sample.len, 0);
            walk_frames(1, links, &mut sample);
            assert_eq!(sample.len, 0, "a misaligned frame pointer is not walked");
        }
    }

    /// A thread whose stack bounds are unknown is walked through the kernel's
    /// copy: links that address readable memory are followed in order.
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn a_copied_walk_follows_a_chain_without_bounds() {
        let mut chain = [0_usize; 6];
        let base = chain.as_ptr() as usize;
        let word = std::mem::size_of::<usize>();
        chain[0] = base + 2 * word;
        chain[1] = 0x1111;
        chain[2] = base + 4 * word;
        chain[3] = 0x2222;
        chain[5] = 0x3333;
        // The kernel reads the chain, which the compiler cannot see.
        let base = std::hint::black_box(chain.as_mut_ptr()) as usize;
        let mut sample = RawSample::EMPTY;
        walk_frames(base, Links::Copied { floor: base }, &mut sample);
        assert_eq!(
            &sample.frames[..sample.len as usize],
            &[0x1111, 0x2222, 0x3333]
        );
        let mut below = RawSample::EMPTY;
        walk_frames(base, Links::Copied { floor: base + word }, &mut below);
        assert_eq!(below.len, 0, "a link below the stack pointer is not read");
    }

    /// A copied walk that meets an address the process cannot read stops
    /// there instead of faulting.
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn a_copied_walk_stops_at_unmapped_memory() {
        let page = 4_096;
        // SAFETY: a fresh anonymous mapping, unmapped again before its address
        // is used, so nothing else can be using it.
        let gone = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(p, libc::MAP_FAILED);
            assert_eq!(libc::munmap(p, page), 0);
            p as usize
        };
        let mut sample = RawSample::EMPTY;
        walk_frames(gone, Links::Copied { floor: 0 }, &mut sample);
        assert_eq!(sample.len, 0);
    }

    /// Armed sampling records a stack once the accumulator crosses the
    /// interval. Holds the recorder to its contract on every platform that
    /// has one: the frames come from a record the recorder established
    /// itself, or from the OS unwinder, never from whatever the calling
    /// frame left in the frame-pointer register.
    #[cfg(any(
        all(unix, not(miri), any(target_arch = "x86_64", target_arch = "aarch64")),
        all(windows, not(miri))
    ))]
    #[test]
    fn armed_sampling_records_a_stack_with_frames() {
        let _guard = sampler_lock();
        stop_heap();
        let _ = drain_heap();
        start_heap();
        record_allocation(HEAP_SAMPLE_BYTES);
        record_allocation(HEAP_SAMPLE_BYTES);
        stop_heap();
        let samples = drain_heap();
        assert!(
            !samples.is_empty(),
            "an armed sampler records past the interval"
        );
        assert!(
            samples.iter().all(|s| s.len > 0),
            "every recorded sample carries at least one frame"
        );
    }

    /// The handler runs on whichever thread the timer interrupts, often one
    /// that just started or sits inside the C allocator holding its lock. It
    /// must sample such threads without asking the C library anything, or the
    /// thread waits on itself and the process stops.
    #[cfg(all(target_os = "linux", not(miri)))]
    #[test]
    fn sampling_fresh_threads_inside_malloc_completes() {
        let _guard = sampler_lock();
        stop();
        let _ = drain();
        start(1_000).expect("the sampler starts");
        for _ in 0..8 {
            let workers: Vec<_> = (0..32)
                .map(|_| {
                    std::thread::spawn(|| {
                        for i in 0..4_000_usize {
                            // Above the thread cache's sizes, so every call takes
                            // the arena lock.
                            // SAFETY: a fresh allocation is freed once, unused.
                            unsafe { libc::free(libc::malloc(4_096 + (i % 8) * 1_024)) };
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("a worker finishes");
            }
        }
        stop();
        assert!(!drain().is_empty(), "the busy workers were sampled");
    }

    /// A disarmed sampler is the default and records nothing, so a program
    /// that never asks for a heap profile keeps an empty ring.
    #[test]
    fn a_disarmed_sampler_records_nothing() {
        let _guard = sampler_lock();
        stop_heap();
        let _ = drain_heap();
        record_allocation(HEAP_SAMPLE_BYTES * 4);
        assert!(drain_heap().is_empty());
    }
}
