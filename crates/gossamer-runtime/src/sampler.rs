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

/// Fixed sample storage shared between a recorder and a drainer.
///
/// Not a lock: the recorder can be a signal handler, which may interrupt
/// code holding any lock in the process and so may not take one. Access
/// is serialized by the paired index instead - a recorder writes only the
/// slot its `fetch_add` claimed, and a drainer reads only slots below an
/// index it has swapped to zero.
struct SampleRing {
    slots: UnsafeCell<[RawSample; CAPACITY]>,
}

// SAFETY: the index protocol above gives every access a slot no other
// thread is touching; the cell only removes the compiler's aliasing
// assumption, it does not add sharing.
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
            slots: UnsafeCell::new([RawSample::EMPTY; CAPACITY]),
        }
    }

    /// Writes `sample` into `slot`, which the caller has claimed.
    fn write(&self, slot: usize, sample: &RawSample) {
        if slot >= CAPACITY {
            return;
        }
        // SAFETY: `slot` was claimed by the caller's `fetch_add` and is
        // in bounds, so no other writer addresses it.
        unsafe {
            self.slots
                .get()
                .cast::<RawSample>()
                .add(slot)
                .write(*sample);
        }
    }

    /// Reads `slot`, which no recorder is writing.
    fn read(&self, slot: usize) -> RawSample {
        // SAFETY: the drainer reset the index before reading, so slots
        // below `taken` are complete and unclaimed.
        unsafe { self.slots.get().cast::<RawSample>().add(slot).read() }
    }
}

/// The CPU sample ring.
static SAMPLES: SampleRing = SampleRing::new();

/// Next slot to write. Wraps; a drain reads up to the current index.
static WRITE_INDEX: AtomicUsize = AtomicUsize::new(0);

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
    WRITE_INDEX.store(0, Ordering::SeqCst);
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
    let taken = WRITE_INDEX.swap(0, Ordering::SeqCst).min(CAPACITY);
    let mut out = Vec::with_capacity(taken);
    for slot in 0..taken {
        let sample = SAMPLES.read(slot);
        if sample.len > 0 {
            out.push(sample);
        }
    }
    out
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

/// SIGPROF handler. Async-signal-safe: no allocation, no locks, no
/// library calls beyond reading memory it has bounds-checked.
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
    let (pc, frame_pointer) = unsafe { interrupted_registers(context) };
    if pc != 0 {
        sample.frames[0] = pc;
        sample.len = 1;
    }
    walk_frames(frame_pointer, &mut sample);
    if sample.len == 0 {
        return;
    }
    let slot = WRITE_INDEX.fetch_add(1, Ordering::SeqCst);
    if slot >= CAPACITY {
        // Buffer full until the next drain. Dropping is better than
        // wrapping over samples the drainer is about to read.
        WRITE_INDEX.store(CAPACITY, Ordering::SeqCst);
        return;
    }
    SAMPLES.write(slot, &sample);
}

/// Program counter and frame pointer of the interrupted context.
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
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize) {
    if context.is_null() {
        return (0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let regs = &ctx.uc_mcontext.gregs;
    (
        regs[libc::REG_RIP as usize] as usize,
        regs[libc::REG_RBP as usize] as usize,
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
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize) {
    if context.is_null() {
        return (0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let machine = &ctx.uc_mcontext;
    (machine.pc as usize, machine.regs[29] as usize)
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
unsafe fn interrupted_registers(context: *mut libc::c_void) -> (usize, usize) {
    if context.is_null() {
        return (0, 0);
    }
    // SAFETY: the kernel hands a `ucontext_t` to an SA_SIGINFO handler;
    // `uc_mcontext` is checked before it is dereferenced.
    let ctx = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    if ctx.uc_mcontext.is_null() {
        return (0, 0);
    }
    // SAFETY: the same handler contract makes `uc_mcontext` a live
    // `__darwin_mcontext64` for the duration of the handler.
    let state = unsafe { &(*ctx.uc_mcontext).__ss };
    #[cfg(target_arch = "aarch64")]
    {
        (state.__pc as usize, state.__fp as usize)
    }
    #[cfg(target_arch = "x86_64")]
    {
        (state.__rip as usize, state.__rbp as usize)
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
        any(target_os = "linux", target_os = "macos")
    ))
))]
/// # Safety
///
/// `context` is null or the `ucontext_t` the kernel passed to the signal
/// handler running this.
unsafe fn interrupted_registers(_context: *mut libc::c_void) -> (usize, usize) {
    (0, 0)
}

/// `(lo, hi)` bounds of the stack the calling code is running on, cached
/// per thread.
///
/// A goroutine runs on its own allocated stack, so its window comes from
/// the coroutine guard rather than from the OS thread. That window changes
/// as goroutines are scheduled onto a worker, so only the OS-thread bounds
/// - fixed for the life of the thread - are worth caching.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn current_stack_bounds() -> Option<(usize, usize)> {
    thread_local! {
        static THREAD_STACK: std::cell::Cell<Option<(usize, usize)>> =
            const { std::cell::Cell::new(None) };
    }
    if let Some(window) = gossamer_coro::goroutine_stack_bounds() {
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
/// Every step is validated against the stack the walk is running on: a
/// frame pointer in a function compiled without one is whatever that
/// function left in the register, so every link is untrusted input and
/// must be proven to address readable stack memory before it is read.
/// Without bounds the walk cannot be made safe, so it does not run.
#[cfg(all(unix, not(miri), not(target_arch = "wasm32")))]
fn walk_frames(mut frame_pointer: usize, sample: &mut RawSample) {
    /// A frame larger than this is a sign the chain is not a chain.
    const MAX_FRAME_BYTES: usize = 1 << 20;

    let Some((lo, hi)) = current_stack_bounds() else {
        return;
    };
    // Both words of the link must lie inside the window.
    let readable = |fp: usize| {
        fp != 0
            && fp.is_multiple_of(std::mem::align_of::<usize>())
            && fp >= lo
            && fp.saturating_add(2 * std::mem::size_of::<usize>()) <= hi
    };

    while (sample.len as usize) < MAX_FRAMES {
        if !readable(frame_pointer) {
            return;
        }
        // SAFETY: `readable` proved the address is aligned and that both
        // words it addresses lie inside the running stack's mapped bounds.
        let (saved_fp, return_address) = unsafe {
            let base = frame_pointer as *const usize;
            (base.read_volatile(), base.add(1).read_volatile())
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

/// Next heap slot to write.
static HEAP_INDEX: AtomicUsize = AtomicUsize::new(0);

/// Arms allocation sampling.
pub fn start_heap() {
    crate::c_abi::ledger::arm_instrumentation();
    HEAP_INDEX.store(0, Ordering::SeqCst);
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
    let taken = HEAP_INDEX.swap(0, Ordering::SeqCst).min(CAPACITY);
    let mut out = Vec::with_capacity(taken);
    for slot in 0..taken {
        let sample = HEAP_SAMPLES.read(slot);
        if sample.len > 0 {
            out.push(sample);
        }
    }
    out
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
    let slot = HEAP_INDEX.fetch_add(1, Ordering::SeqCst);
    if slot >= CAPACITY {
        HEAP_INDEX.store(CAPACITY, Ordering::SeqCst);
        return;
    }
    HEAP_SAMPLES.write(slot, sample);
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
    walk_frames(frame_pointer, &mut sample);
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
        let mut sample = RawSample::EMPTY;
        walk_frames(0, &mut sample);
        assert_eq!(sample.len, 0);
        walk_frames(1, &mut sample);
        assert_eq!(sample.len, 0, "a misaligned frame pointer is not walked");
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
