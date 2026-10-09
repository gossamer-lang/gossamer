//! Vec element buffers large enough to take straight from the kernel.
//!
//! A doubling vector hands back every buffer it outgrows. Inside the
//! allocator those buffers stay committed for its purge delay, and the next
//! phase of a program reuses them only where its own sizes happen to fit the
//! holes they left, so two spellings of the same program can differ in peak
//! memory by whole buffers. A mapped buffer is a region of its own instead:
//! it grows in place (`mremap` on Linux), and once freed it waits in a small
//! cache from which any later large buffer is served, resized to fit, with
//! the pages it already touched. A cached mapping goes back to the kernel
//! after the allocator's purge delay, or sooner when a request the cache
//! cannot serve needs new pages.

use std::time::Instant;

/// Buffers of at least this many bytes are mapped. Below it the allocator's
/// own size classes serve and reuse them well.
#[cfg(all(
    any(unix, windows),
    not(any(tsan, miri, fuzzing, target_arch = "wasm32"))
))]
pub(crate) const MAPPED_BYTES: usize = 1 << 20;

/// No mapping without a kernel to ask, or under a sanitizer that must see
/// every allocation through the allocator.
#[cfg(not(all(
    any(unix, windows),
    not(any(tsan, miri, fuzzing, target_arch = "wasm32"))
)))]
pub(crate) const MAPPED_BYTES: usize = usize::MAX;

/// Bytes in front of a mapped buffer recording the mapping's length. A
/// multiple of 64 so the buffer keeps the alignment of the page it starts on
/// to a cache line.
const HEADER: usize = 64;

/// Whether a buffer of `bytes` is mapped. The decision depends on the size
/// alone, so the free of a buffer, which passes the size it was allocated
/// with, reaches the same branch as its allocation.
#[inline]
pub(crate) fn is_mapped(bytes: usize) -> bool {
    bytes >= MAPPED_BYTES
}

/// The length a mapping holding `bytes` of buffer needs: whole pages.
fn needed_len(bytes: usize) -> usize {
    let page = page_size();
    bytes
        .saturating_add(HEADER)
        .div_ceil(page)
        .saturating_mul(page)
}

/// The mapping a buffer at `p` lives in, and its length.
///
/// # Safety
///
/// `p` is a buffer [`map`] or [`remap`] answered and has not been freed.
unsafe fn mapping_of(p: *mut u8) -> (*mut u8, usize) {
    // SAFETY: the buffer starts `HEADER` bytes into its mapping, whose first
    // word records the mapping's length.
    unsafe {
        let base = p.sub(HEADER);
        (base, base.cast::<usize>().read())
    }
}

/// The buffer inside a mapping at `base` of `len` bytes, its header written.
///
/// # Safety
///
/// `base` is a writable mapping of `len` bytes.
unsafe fn buffer_in(base: *mut u8, len: usize) -> *mut u8 {
    // SAFETY: the mapping holds at least `HEADER` bytes.
    unsafe {
        base.cast::<usize>().write(len);
        base.add(HEADER)
    }
}

/// Bytes a mapped buffer at `p` can hold, the slack of its mapping included.
///
/// # Safety
///
/// `p` is a live buffer [`map`] or [`remap`] answered.
pub(crate) unsafe fn usable(p: *const u8) -> usize {
    // SAFETY: forwarded from the caller.
    let (_, len) = unsafe { mapping_of(p.cast_mut()) };
    len - HEADER
}

/// A mapped buffer of at least `bytes`.
pub(crate) fn map(bytes: usize) -> *mut u8 {
    let need = needed_len(bytes);
    let (base, len) = take_cached(need).unwrap_or_else(|| (os::map(need), need));
    // SAFETY: `base` is a writable mapping of `len` bytes.
    unsafe { buffer_in(base, len) }
}

/// Frees a mapped buffer: into the cache while the allocator holds freed
/// memory at all, back to the kernel otherwise.
///
/// # Safety
///
/// `p` is a live buffer [`map`] or [`remap`] answered, unused after the call.
pub(crate) unsafe fn unmap(p: *mut u8) {
    // SAFETY: forwarded from the caller.
    let (base, len) = unsafe { mapping_of(p) };
    let delay = crate::allocator_purge_delay();
    if delay.is_zero() {
        // SAFETY: `base` is a whole live mapping of `len` bytes, now unused.
        unsafe { os::unmap(base, len) };
        return;
    }
    let mut cache = CACHE.lock();
    expire(&mut cache, delay);
    cache.push(Cached {
        base,
        len,
        freed_at: Instant::now(),
    });
    drop(cache);
    ensure_purger(delay);
}

/// Whether a purger thread is running.
static PURGER_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Starts the thread that returns cached mappings once their delay expires,
/// unless one is running. A process that frees a large buffer and then waits
/// (on a sleep, a channel, or its input) makes no allocator call that would
/// apply the delay, and the scheduler's watchdog runs only once goroutines
/// do; this thread covers it, and exits once the cache is empty.
fn ensure_purger(delay: std::time::Duration) {
    use std::sync::atomic::Ordering;
    if PURGER_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gos-purge".to_string())
        .stack_size(64 << 10)
        .spawn(move || {
            loop {
                std::thread::sleep(delay);
                crate::purge_expired_allocator_memory();
                if !CACHE.lock().is_empty() {
                    continue;
                }
                PURGER_RUNNING.store(false, Ordering::Release);
                // A mapping cached between the check and the store saw a
                // running purger and started none, so this one keeps going.
                if CACHE.lock().is_empty() || PURGER_RUNNING.swap(true, Ordering::AcqRel) {
                    return;
                }
            }
        });
    if spawned.is_err() {
        // Without a thread the cache still empties at the next large
        // allocation or free, or from the watchdog.
        PURGER_RUNNING.store(false, Ordering::Release);
    }
}

/// Grows a mapped buffer to hold `new` bytes, keeping its first `old`.
///
/// # Safety
///
/// `p` is a live buffer [`map`] or [`remap`] answered holding `old` bytes,
/// `new > old`, and `p` is not used after the call.
pub(crate) unsafe fn remap(p: *mut u8, old: usize, new: usize) -> *mut u8 {
    // SAFETY: forwarded from the caller.
    let (base, len) = unsafe { mapping_of(p) };
    let need = needed_len(new);
    if need <= len {
        return p;
    }
    // SAFETY: `base` is a whole live mapping of `len` bytes holding the
    // buffer's `old` bytes past its header, unused after the call.
    let (base, len) = unsafe { os::grow(base, len, need, HEADER + old) };
    // SAFETY: `base` is a writable mapping of `len` bytes.
    unsafe { buffer_in(base, len) }
}

/// Returns to the kernel every cached mapping freed longer ago than the
/// allocator's purge delay. The scheduler's watchdog calls this, through
/// `purge_expired_allocator_memory`, for a process that has gone quiet.
pub(crate) fn purge_expired() {
    let mut cache = CACHE.lock();
    expire(&mut cache, crate::allocator_purge_delay());
}

struct Cached {
    base: *mut u8,
    len: usize,
    freed_at: Instant,
}

/// Freed mappings, oldest first.
struct Cache(Vec<Cached>);

// SAFETY: a cached mapping is owned by the cache alone; its address is
// handed to exactly one taker, under the lock.
unsafe impl Send for Cache {}

impl std::ops::Deref for Cache {
    type Target = Vec<Cached>;
    fn deref(&self) -> &Vec<Cached> {
        &self.0
    }
}

impl std::ops::DerefMut for Cache {
    fn deref_mut(&mut self) -> &mut Vec<Cached> {
        &mut self.0
    }
}

static CACHE: parking_lot::Mutex<Cache> = parking_lot::const_mutex(Cache(Vec::new()));

fn expire(cache: &mut Cache, delay: std::time::Duration) {
    cache.retain(|c| {
        if c.freed_at.elapsed() < delay {
            return true;
        }
        // SAFETY: a cached mapping is whole, live, and referenced by nothing.
        unsafe { os::unmap(c.base, c.len) };
        false
    });
}

/// A cached mapping of at least `need` bytes: the smallest that fits, or the
/// largest grown to fit.
///
/// Growing one means asking the kernel for pages while others sit cached, so
/// first the oldest other cached mappings, together at least as large as the
/// growth, go back: the cache never adds to a peak that a request it cannot
/// serve has to raise anyway.
fn take_cached(need: usize) -> Option<(*mut u8, usize)> {
    let mut cache = CACHE.lock();
    expire(&mut cache, crate::allocator_purge_delay());
    let fits = cache
        .iter()
        .enumerate()
        .filter(|(_, c)| c.len >= need)
        .min_by_key(|(_, c)| c.len)
        .map(|(i, _)| i);
    if let Some(i) = fits {
        let entry = cache.remove(i);
        return Some((entry.base, entry.len));
    }
    let largest = cache
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| c.len)
        .map(|(i, _)| i)?;
    let entry = cache.remove(largest);
    let mut released = 0;
    while released < need - entry.len && !cache.is_empty() {
        let oldest = cache.remove(0);
        released += oldest.len;
        // SAFETY: a cached mapping is whole, live, and referenced by nothing.
        unsafe { os::unmap(oldest.base, oldest.len) };
    }
    drop(cache);
    // SAFETY: the cached mapping is whole, live, and now owned here; its
    // contents are dead, so nothing of it needs keeping.
    Some(unsafe { os::grow(entry.base, entry.len, need, 0) })
}

/// The process cannot continue without the buffer it asked for; report it the
/// way the global allocator reports an exhausted heap.
fn out_of_memory(bytes: usize) -> ! {
    let layout = std::alloc::Layout::from_size_align(bytes.max(1), 8)
        .unwrap_or_else(|_| std::alloc::Layout::new::<u64>());
    std::alloc::handle_alloc_error(layout)
}

#[cfg(all(unix, not(any(tsan, miri, fuzzing, target_arch = "wasm32"))))]
fn page_size() -> usize {
    static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE.get_or_init(|| {
        // SAFETY: `sysconf` reads a process constant.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(4096).max(4096)
    })
}

/// `VirtualAlloc` hands out whole allocation-granularity regions.
#[cfg(all(windows, not(any(tsan, miri, fuzzing))))]
fn page_size() -> usize {
    65536
}

#[cfg(not(all(
    any(unix, windows),
    not(any(tsan, miri, fuzzing, target_arch = "wasm32"))
)))]
fn page_size() -> usize {
    4096
}

#[cfg(all(unix, not(any(tsan, miri, fuzzing, target_arch = "wasm32"))))]
mod os {
    /// A fresh, zeroed, writable mapping of `len` bytes.
    pub(super) fn map(len: usize) -> *mut u8 {
        // SAFETY: an anonymous private mapping at a kernel-chosen address
        // touches no existing memory.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            super::out_of_memory(len);
        }
        p.cast()
    }

    /// # Safety
    ///
    /// `base` is a whole live mapping of `len` bytes, unused after the call.
    pub(super) unsafe fn unmap(base: *mut u8, len: usize) {
        // SAFETY: forwarded from the caller.
        unsafe { libc::munmap(base.cast(), len) };
    }

    /// Grows a mapping of `len` bytes to `need`, keeping its first `keep`
    /// bytes; the kernel moves the pages rather than copying them.
    ///
    /// # Safety
    ///
    /// `base` is a whole live mapping of `len < need` bytes, unused after.
    #[cfg(target_os = "linux")]
    pub(super) unsafe fn grow(
        base: *mut u8,
        len: usize,
        need: usize,
        _keep: usize,
    ) -> (*mut u8, usize) {
        // SAFETY: forwarded from the caller; `MREMAP_MAYMOVE` may relocate
        // the mapping, whose old address is not used after.
        let p = unsafe { libc::mremap(base.cast(), len, need, libc::MREMAP_MAYMOVE) };
        if p == libc::MAP_FAILED {
            super::out_of_memory(need);
        }
        (p.cast(), need)
    }

    /// Grows a mapping of `len` bytes to `need` through a fresh mapping,
    /// copying its first `keep` bytes, where the kernel cannot move pages.
    ///
    /// # Safety
    ///
    /// `base` is a whole live mapping of `len < need` bytes, unused after.
    #[cfg(not(target_os = "linux"))]
    pub(super) unsafe fn grow(
        base: *mut u8,
        len: usize,
        need: usize,
        keep: usize,
    ) -> (*mut u8, usize) {
        let p = map(need);
        // SAFETY: `base` holds `len >= keep` bytes and `p` `need > len`;
        // distinct mappings.
        unsafe {
            std::ptr::copy_nonoverlapping(base, p, keep);
            unmap(base, len);
        }
        (p, need)
    }
}

#[cfg(all(windows, not(any(tsan, miri, fuzzing))))]
mod os {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// A fresh, zeroed, writable region of `len` bytes.
    pub(super) fn map(len: usize) -> *mut u8 {
        // SAFETY: reserving and committing fresh pages at a system-chosen
        // address touches no existing memory.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            super::out_of_memory(len);
        }
        p.cast()
    }

    /// # Safety
    ///
    /// `base` is the base of a whole live region, unused after the call.
    pub(super) unsafe fn unmap(base: *mut u8, _len: usize) {
        // SAFETY: `MEM_RELEASE` with size 0 frees the region `base` starts.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }

    /// Grows a region of `len` bytes to `need` through a fresh region,
    /// copying its first `keep` bytes.
    ///
    /// # Safety
    ///
    /// `base` is a whole live region of `len < need` bytes, unused after.
    pub(super) unsafe fn grow(
        base: *mut u8,
        len: usize,
        need: usize,
        keep: usize,
    ) -> (*mut u8, usize) {
        let p = map(need);
        // SAFETY: `base` holds `len >= keep` bytes and `p` `need > len`;
        // distinct regions.
        unsafe {
            std::ptr::copy_nonoverlapping(base, p, keep);
            unmap(base, len);
        }
        (p, need)
    }
}

/// Builds without a mapping primitive never classify a buffer as mapped, so
/// nothing here is reached.
#[cfg(not(all(
    any(unix, windows),
    not(any(tsan, miri, fuzzing, target_arch = "wasm32"))
)))]
mod os {
    pub(super) fn map(len: usize) -> *mut u8 {
        super::out_of_memory(len)
    }

    /// # Safety
    ///
    /// Never called: no buffer is mapped in this build.
    pub(super) unsafe fn unmap(_base: *mut u8, _len: usize) {}

    /// # Safety
    ///
    /// Never called: no buffer is mapped in this build.
    pub(super) unsafe fn grow(
        _base: *mut u8,
        _len: usize,
        need: usize,
        _keep: usize,
    ) -> (*mut u8, usize) {
        super::out_of_memory(need)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grown_mapping_keeps_its_bytes() {
        assert!(!is_mapped(MAPPED_BYTES.saturating_sub(1)));
        if MAPPED_BYTES == usize::MAX {
            return;
        }
        let p = map(MAPPED_BYTES);
        // SAFETY: `p` holds at least `MAPPED_BYTES` bytes.
        unsafe { p.write_bytes(0x5a, MAPPED_BYTES) };
        // SAFETY: `p` holds `MAPPED_BYTES` live bytes and is not used after.
        let q = unsafe { remap(p, MAPPED_BYTES, MAPPED_BYTES * 2) };
        // SAFETY: `q` holds the first `MAPPED_BYTES` bytes it kept.
        let kept = unsafe { std::slice::from_raw_parts(q, MAPPED_BYTES) };
        assert!(kept.iter().all(|&b| b == 0x5a));
        // SAFETY: `q` holds at least `2 * MAPPED_BYTES` bytes.
        assert!(unsafe { usable(q) } >= MAPPED_BYTES * 2);
        // SAFETY: the grown part of a mapping is writable.
        unsafe { q.add(MAPPED_BYTES * 2 - 1).write(1) };
        // SAFETY: `q` is a live buffer `remap` answered, unused after.
        unsafe { unmap(q) };
    }

    #[test]
    fn a_freed_mapping_serves_the_next_large_buffer() {
        if MAPPED_BYTES == usize::MAX || crate::allocator_purge_delay().is_zero() {
            return;
        }
        let p = map(MAPPED_BYTES * 2);
        // SAFETY: `p` is a live buffer `map` answered, unused after.
        unsafe { unmap(p) };
        let q = map(MAPPED_BYTES);
        // SAFETY: `q` is live.
        assert!(unsafe { usable(q) } >= MAPPED_BYTES);
        // SAFETY: `q` is a live buffer `map` answered, unused after.
        unsafe { unmap(q) };
        purge_expired();
    }
}
