//! Stacks that grow on demand to a limit set by the memory the program may
//! use.
//!
//! A stack is reserved as address space and committed a page at a time as
//! frames reach it, so a shallow program costs a few pages and a deep
//! recursion costs the memory its frames take. Recursion is bounded by that
//! memory, not by a frame count: past the limit the guard page below the
//! stack faults, and the runtime reports `GX0008`.

use std::sync::OnceLock;

/// The most any stack grows to, `main`'s and every goroutine's: Go's limit
/// for a goroutine on a 64-bit host. A stack is reserved address space, so
/// the bound also keeps as many goroutines as the host's mapping limit
/// allows within the address space, and a runaway recursion stops here.
pub const MAX_STACK_BYTES: usize = 1 << 30;

/// The least any stack reserves, below which a host's memory reading is not
/// trusted to size one.
pub(crate) const MIN_GROWABLE_STACK_BYTES: usize = 64 << 20;

/// Bytes of memory the program may use: the control group's limit where one
/// is set, otherwise the host's physical memory. `None` where the platform
/// does not say.
#[must_use]
pub fn memory_bytes() -> Option<u64> {
    static MEMORY: OnceLock<Option<u64>> = OnceLock::new();
    *MEMORY.get_or_init(|| {
        let physical = physical_memory_bytes();
        match (cgroup_memory_limit(), physical) {
            (Some(limit), Some(physical)) => Some(limit.min(physical)),
            (limit, physical) => limit.or(physical),
        }
    })
}

/// The stack `main` and every goroutine may grow to: [`MAX_STACK_BYTES`],
/// or half the memory the program may use where that is less, so a deep
/// recursion reports `GX0008` before the system runs out of memory.
#[must_use]
pub fn stack_limit_bytes() -> usize {
    static BYTES: OnceLock<usize> = OnceLock::new();
    *BYTES.get_or_init(|| {
        memory_bytes()
            .map_or(MAX_STACK_BYTES, |memory| {
                usize::try_from(memory / 2).unwrap_or(usize::MAX)
            })
            .clamp(MIN_GROWABLE_STACK_BYTES, MAX_STACK_BYTES)
    })
}

#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<u64> {
    let parse = |text: &str| text.trim().parse::<u64>().ok();
    // cgroup v2: the unified hierarchy names this process's group on the
    // `0::` line, and every group above it may set a tighter limit.
    if let Ok(groups) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(path) = groups.lines().find_map(|line| line.strip_prefix("0::"))
    {
        let mut limit: Option<u64> = None;
        let mut dir = std::path::PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        loop {
            if let Some(value) = std::fs::read_to_string(dir.join("memory.max"))
                .ok()
                .as_deref()
                .and_then(parse)
            {
                limit = Some(limit.map_or(value, |l| l.min(value)));
            }
            if dir == std::path::Path::new("/sys/fs/cgroup") || !dir.pop() {
                break;
            }
        }
        if limit.is_some() {
            return limit;
        }
    }
    // cgroup v1 reports "no limit" as a value near `i64::MAX`, which the
    // physical-memory bound then wins against.
    std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .ok()
        .as_deref()
        .and_then(parse)
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory_limit() -> Option<u64> {
    None
}

#[cfg(unix)]
fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: `sysconf` reads a system constant and touches no memory.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: as above.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (pages > 0 && page > 0).then(|| (pages as u64).saturating_mul(page as u64))
}

#[cfg(not(unix))]
fn physical_memory_bytes() -> Option<u64> {
    None
}

/// A stack reserved as address space and committed as frames reach it, with
/// an inaccessible guard page below it.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) struct GrowableStack {
    base: usize,
    len: usize,
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn page_size() -> usize {
    // SAFETY: `sysconf` reads a system constant and touches no memory.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
impl GrowableStack {
    /// Reserves `size` bytes of stack plus a guard page. The reservation
    /// commits nothing: `MAP_NORESERVE` keeps it out of the commit charge,
    /// and a page is backed the first time a frame touches it.
    ///
    /// # Errors
    ///
    /// The `mmap` or `mprotect` failure, typically `ENOMEM` once the address
    /// space or the mapping count is exhausted.
    pub(crate) fn new(size: usize) -> std::io::Result<Self> {
        let page = page_size();
        let size = size.div_ceil(page) * page;
        let len = size + page;
        // SAFETY: an anonymous private mapping of `len` bytes at an address
        // the kernel chooses; no existing memory is touched.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let stack = Self {
            base: base as usize,
            len,
        };
        // SAFETY: `[base + page, base + len)` lies inside the mapping made
        // above; the lowest page stays `PROT_NONE` as the guard.
        let protected = unsafe {
            libc::mprotect(
                (stack.base + page) as *mut libc::c_void,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        };
        if protected != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stack)
    }

    /// Hands every page below the top `keep` bytes back to the system, so
    /// a stack a deep recursion once filled holds only what a fresh one
    /// would.
    pub(crate) fn release_below(&self, keep: usize) {
        let page = page_size();
        let keep = keep.div_ceil(page) * page;
        let start = self.base + page;
        let end = (self.base + self.len).saturating_sub(keep);
        if end <= start {
            return;
        }
        // Every frame, compiled code's included, probes the pages it spans
        // from the top down, so a stack that reached below `end` touched the
        // page just under it. One page's residency answers for the range.
        let mut resident = 0u8;
        // SAFETY: the page `[end - page, end)` lies inside this mapping, and
        // `resident` holds the one byte `mincore` writes for one page.
        let probed = unsafe {
            libc::mincore(
                (end - page) as *mut libc::c_void,
                page,
                std::ptr::from_mut(&mut resident).cast(),
            )
        };
        if probed == 0 && resident & 1 == 0 {
            return;
        }
        // SAFETY: `[start, end)` lies inside this stack's writable mapping,
        // and no frame is live there: the stack is idle.
        unsafe {
            libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
        }
    }
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
impl Drop for GrowableStack {
    fn drop(&mut self) {
        // SAFETY: `[base, base + len)` is the mapping `new` made, and the
        // stack is no longer in use.
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.len);
        }
    }
}

// SAFETY: `base()` is the top of a writable region of `len - page` bytes and
// `limit()` its bottom, above the guard page; both stay valid until drop.
#[cfg(all(unix, not(target_arch = "wasm32")))]
unsafe impl corosensei::stack::Stack for GrowableStack {
    fn base(&self) -> corosensei::stack::StackPointer {
        corosensei::stack::StackPointer::new(self.base + self.len)
            .expect("a mapping's end is never zero")
    }

    fn limit(&self) -> corosensei::stack::StackPointer {
        corosensei::stack::StackPointer::new(self.base + page_size())
            .expect("a mapping's start plus a page is never zero")
    }
}

/// The stack type goroutines and `main` run on.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) type ProgramStack = GrowableStack;

/// Windows reserves a default stack's address space and commits it a guard
/// page at a time as it grows, which is the growth this module asks for.
#[cfg(all(not(unix), not(target_arch = "wasm32")))]
pub(crate) type ProgramStack = corosensei::stack::DefaultStack;

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    /// Whether the calling thread runs on a stack [`on_growable_stack`]
    /// switched it to.
    static ON_GROWABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the calling thread already runs on a growable stack.
#[must_use]
pub fn on_growable_stack_now() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        ON_GROWABLE.with(std::cell::Cell::get)
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// Runs `f` on a fresh stack of `bytes` that grows on demand, with the
/// recursion guard armed for it, and answers what `f` answers. A panic in
/// `f` resumes on the caller's stack. A thread already on one stays there,
/// so the pages its earlier work touched serve `f` too.
#[cfg(not(target_arch = "wasm32"))]
pub fn on_growable_stack<R>(bytes: usize, f: impl FnOnce() -> R) -> R {
    if on_growable_stack_now() {
        return f();
    }
    let Ok(stack) = ProgramStack::new(bytes) else {
        return f();
    };
    corosensei::on_stack(stack, move || {
        let _restore = GuardRestore(crate::set_stack_guard(
            crate::current_stack_ptr(),
            bytes.saturating_sub(crate::STACK_GUARD_MARGIN),
        ));
        ON_GROWABLE.with(|on| on.set(true));
        f()
    })
}

/// Puts back the recursion guard a stack switch replaced, and leaves the
/// growable stack, on every exit.
#[cfg(not(target_arch = "wasm32"))]
struct GuardRestore((usize, usize));

#[cfg(not(target_arch = "wasm32"))]
impl Drop for GuardRestore {
    fn drop(&mut self) {
        ON_GROWABLE.with(|on| on.set(false));
        crate::set_stack_guard(self.0.0, self.0.1);
    }
}

/// The browser build has no native stacks to switch.
#[cfg(target_arch = "wasm32")]
pub fn on_growable_stack<R>(_bytes: usize, f: impl FnOnce() -> R) -> R {
    f()
}
