//! Runtime ownership for finalized in-process JIT allocations.
//!
//! Cranelift owns the provider adapter only while a module is being built.
//! The adapter and the runtime artifact share this heap, so dropping the
//! module releases compiler metadata without invalidating finalized entries.

#![allow(unsafe_code)]

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use cranelift_jit::{BranchProtection, JITMemoryKind, JITMemoryProvider};
use cranelift_module::ModuleResult;
use parking_lot::Mutex;

const BUILDING: u8 = 0;
const FINALIZED: u8 = 1;
const DETACHED: u8 = 2;
const RETIRED: u8 = 3;

/// Shared owner of every mapping referenced by finalized native code.
///
/// The mutex is used only for allocation, finalization, and destruction.
/// Calling a finalized entry does not touch this object or acquire a lock.
pub(crate) struct NativeCodeHeap {
    memory: Mutex<ModuleMemory>,
    state: AtomicU8,
}

impl NativeCodeHeap {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            memory: Mutex::new(ModuleMemory::default()),
            state: AtomicU8::new(BUILDING),
        })
    }

    pub(crate) fn provider(heap: &Arc<Self>) -> GossamerJitMemoryProvider {
        GossamerJitMemoryProvider {
            heap: Arc::clone(heap),
        }
    }

    /// Marks the point after the module and all module-owned metadata have
    /// been destroyed. Entry pointers may be published only after this call.
    pub(crate) fn mark_detached(&self) {
        let previous =
            self.state
                .compare_exchange(FINALIZED, DETACHED, Ordering::AcqRel, Ordering::Acquire);
        debug_assert_eq!(previous, Ok(FINALIZED));
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.state.load(Ordering::Acquire) == DETACHED
    }

    /// Copies `bytes` into read-only pages of this module's reservation, so
    /// they lie within reach of a 32-bit offset from its code, and answers
    /// their address. They live as long as the code does.
    pub(crate) fn place_read_only(&self, bytes: &[u8]) -> io::Result<usize> {
        self.memory.lock().place_read_only(bytes)
    }
}

impl Drop for NativeCodeHeap {
    fn drop(&mut self) {
        let previous = self.state.swap(RETIRED, Ordering::AcqRel);
        debug_assert!(matches!(previous, BUILDING | FINALIZED | DETACHED));
        // SAFETY: the heap can be dropped only after the provider adapter and
        // every artifact owner are gone. Gossamer keeps the artifact alive
        // while an entry is reachable and tears worker VMs down only after
        // their native calls complete.
        unsafe { self.memory.get_mut().free_memory() };
    }
}

/// Temporary adapter owned by `JITModule` during compilation.
pub(crate) struct GossamerJitMemoryProvider {
    heap: Arc<NativeCodeHeap>,
}

impl JITMemoryProvider for GossamerJitMemoryProvider {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        if self.heap.state.load(Ordering::Acquire) != BUILDING {
            return Err(io::Error::other(
                "cannot allocate into a finalized Gossamer JIT artifact",
            ));
        }
        self.heap.memory.lock().allocate(size, align, kind)
    }

    unsafe fn free_memory(&mut self) {
        self.heap.state.store(RETIRED, Ordering::Release);
        // SAFETY: this method carries the provider trait's caller contract.
        unsafe { self.heap.memory.lock().free_memory() };
    }

    fn finalize(&mut self, branch_protection: BranchProtection) -> ModuleResult<()> {
        if self.heap.state.load(Ordering::Acquire) != BUILDING {
            return Err(cranelift_module::ModuleError::Backend(anyhow::anyhow!(
                "Gossamer JIT artifact was already finalized"
            )));
        }
        self.heap.memory.lock().finalize(branch_protection)?;
        self.heap.state.store(FINALIZED, Ordering::Release);
        Ok(())
    }
}

/// Address space reserved for one module's code and data. Cranelift's
/// non-PIC code model reaches a module's own functions and data through
/// 32-bit PC-relative offsets, so every allocation of a module must lie
/// within 2 GiB of every other; one reservation below that bound holds them
/// all. Only the pages a segment uses are committed.
const MODULE_RESERVATION: usize = 1 << 30;

/// The least a new segment commits, so a module of many small functions
/// takes few protection changes.
const MIN_SEGMENT: usize = 64 << 10;

/// What a segment's pages become when the module is finalized.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Code,
    ReadOnly,
    Writable,
}

impl From<JITMemoryKind> for Purpose {
    fn from(kind: JITMemoryKind) -> Self {
        match kind {
            JITMemoryKind::Executable => Self::Code,
            JITMemoryKind::ReadOnly => Self::ReadOnly,
            JITMemoryKind::Writable => Self::Writable,
        }
    }
}

/// A run of committed pages of one purpose, handed out front to back.
struct Segment {
    start: usize,
    len: usize,
    used: usize,
    purpose: Purpose,
    finalized: bool,
}

/// One module's memory: a single reservation, committed segment by segment.
#[derive(Default)]
pub(crate) struct ModuleMemory {
    /// Start of the reservation, 0 until the first allocation.
    base: usize,
    /// Bytes of the reservation given to segments.
    committed: usize,
    segments: Vec<Segment>,
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

impl ModuleMemory {
    fn reserve(&mut self) -> io::Result<()> {
        if self.base == 0 {
            self.base = reserve_address_space(MODULE_RESERVATION)?;
        }
        Ok(())
    }

    fn place_read_only(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.reserve()?;
        let len = align_up(bytes.len().max(1), region::page::size());
        let start = self.commit_more(len)?;
        // SAFETY: `start..start + len` was just committed read-write and is
        // handed to nothing else.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), start as *mut u8, bytes.len());
            region::protect(start as *const u8, len, region::Protection::READ)
                .map_err(io::Error::other)?;
        }
        self.segments.push(Segment {
            start,
            len,
            used: bytes.len(),
            purpose: Purpose::ReadOnly,
            finalized: true,
        });
        Ok(start)
    }

    /// Commits `len` more bytes at the end of what segments hold.
    fn commit_more(&mut self, len: usize) -> io::Result<usize> {
        if self.committed + len > MODULE_RESERVATION {
            return Err(io::Error::other(
                "a JIT module outgrew the address space reserved for it",
            ));
        }
        let start = self.base + self.committed;
        commit_read_write(start, len)?;
        self.committed += len;
        Ok(start)
    }
}

impl JITMemoryProvider for ModuleMemory {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        let page = region::page::size();
        let align = usize::try_from(align)
            .ok()
            .filter(|align| align.is_power_of_two() && *align <= page)
            .ok_or_else(|| io::Error::other("JIT alignment above a page"))?;
        let purpose = Purpose::from(kind);
        self.reserve()?;
        if let Some(segment) = self.segments.iter_mut().find(|s| {
            s.purpose == purpose && !s.finalized && align_up(s.used, align) + size <= s.len
        }) {
            let at = align_up(segment.used, align);
            segment.used = at + size;
            return Ok((segment.start + at) as *mut u8);
        }
        let end = self.base + self.committed;
        let extends_last = self
            .segments
            .last()
            .is_some_and(|s| s.purpose == purpose && !s.finalized && s.start + s.len == end);
        if extends_last {
            let last = self.segments.len() - 1;
            let at = align_up(self.segments[last].used, align);
            let grow = align_up(at + size - self.segments[last].len, page);
            self.commit_more(grow)?;
            let segment = &mut self.segments[last];
            segment.len += grow;
            segment.used = at + size;
            return Ok((segment.start + at) as *mut u8);
        }
        let len = align_up(size, page).max(MIN_SEGMENT);
        let start = self.commit_more(len)?;
        self.segments.push(Segment {
            start,
            len,
            used: size,
            purpose,
            finalized: false,
        });
        Ok(start as *mut u8)
    }

    unsafe fn free_memory(&mut self) {
        if self.base != 0 {
            // SAFETY: the caller guarantees nothing runs or reads in this
            // module's memory any more; `base` is the reservation's start.
            unsafe { release_address_space(self.base, MODULE_RESERVATION) };
        }
        self.base = 0;
        self.committed = 0;
        self.segments.clear();
    }

    fn finalize(&mut self, branch_protection: BranchProtection) -> ModuleResult<()> {
        #[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
        let _ = branch_protection;
        let backend = |what: &str, e: &dyn std::fmt::Display| {
            cranelift_module::ModuleError::Backend(anyhow::anyhow!("{what}: {e}"))
        };
        let mut wrote_code = false;
        for segment in self.segments.iter_mut().filter(|s| !s.finalized) {
            let ptr = segment.start as *mut u8;
            match segment.purpose {
                Purpose::Code => {
                    // SAFETY: the segment's pages are committed and hold the
                    // code the module just wrote.
                    unsafe {
                        wasmtime_jit_icache_coherence::clear_cache(ptr.cast(), segment.len)
                            .map_err(|e| backend("clearing the instruction cache", &e))?;
                        region::protect(ptr, segment.len, region::Protection::READ_EXECUTE)
                            .map_err(|e| backend("making code executable", &e))?;
                    }
                    // Branch Target Identification exists only on aarch64 Linux.
                    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
                    protect_branch_targets(ptr, segment.len, branch_protection)
                        .map_err(|e| backend("marking code for branch protection", &e))?;
                    wrote_code = true;
                }
                Purpose::ReadOnly => {
                    // SAFETY: as above, for the module's constant data.
                    unsafe {
                        region::protect(ptr, segment.len, region::Protection::READ)
                            .map_err(|e| backend("making data read-only", &e))?;
                    }
                }
                Purpose::Writable => {}
            }
            segment.finalized = true;
        }
        if wrote_code {
            wasmtime_jit_icache_coherence::pipeline_flush_mt()
                .map_err(|e| backend("flushing the instruction pipeline", &e))?;
        }
        Ok(())
    }
}

/// Branch Target Identification, where the code was compiled for it and the
/// CPU has it.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn protect_branch_targets(
    ptr: *mut u8,
    len: usize,
    branch_protection: BranchProtection,
) -> io::Result<()> {
    const PROT_BTI: libc::c_int = 0x10;
    if branch_protection != BranchProtection::BTI || !std::arch::is_aarch64_feature_detected!("bti")
    {
        return Ok(());
    }
    // SAFETY: `ptr..ptr + len` is a committed code segment of this module.
    if unsafe {
        libc::mprotect(
            ptr.cast(),
            len,
            libc::PROT_READ | libc::PROT_EXEC | PROT_BTI,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn reserve_address_space(len: usize) -> io::Result<usize> {
    // Linux charges a private mapping against the commit limit when it is
    // made writable unless it was reserved `MAP_NORESERVE`; other systems
    // charge pages only as they are touched and take no such flag.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let reserve = libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let reserve = 0;
    // SAFETY: an anonymous inaccessible mapping at an address the kernel
    // chooses; no existing memory is touched.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | reserve,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(base as usize)
}

#[cfg(unix)]
fn commit_read_write(start: usize, len: usize) -> io::Result<()> {
    // SAFETY: `start..start + len` lies inside this module's reservation and
    // holds nothing yet.
    if unsafe {
        libc::mprotect(
            start as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// # Safety
/// `base` is a reservation of `len` bytes nothing uses any more.
#[cfg(unix)]
unsafe fn release_address_space(base: usize, len: usize) {
    // SAFETY: per this function's contract.
    unsafe { libc::munmap(base as *mut libc::c_void, len) };
}

// Windows reserves and commits in separate steps: a reservation costs no
// commit charge, and each segment commits only its own pages.
#[cfg(windows)]
fn reserve_address_space(len: usize) -> io::Result<usize> {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: a fresh reservation at an address the system chooses.
    let base = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_NOACCESS) };
    if base.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(base as usize)
}

#[cfg(windows)]
fn commit_read_write(start: usize, len: usize) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: `start..start + len` lies inside this module's reservation.
    let at = unsafe { VirtualAlloc(start as *const _, len, MEM_COMMIT, PAGE_READWRITE) };
    if at.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// # Safety
/// `base` is a reservation nothing uses any more.
#[cfg(windows)]
unsafe fn release_address_space(base: usize, _len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: per this function's contract; a whole reservation is released
    // with size 0.
    unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc(memory: &mut ModuleMemory, size: usize, kind: JITMemoryKind) -> usize {
        memory.allocate(size, 16, kind).expect("allocation") as usize
    }

    #[test]
    fn every_allocation_of_a_module_lies_inside_one_reservation() {
        let mut memory = ModuleMemory::default();
        let mut seen = Vec::new();
        for round in 0..64 {
            seen.push(alloc(
                &mut memory,
                3000 + round * 97,
                JITMemoryKind::Executable,
            ));
            seen.push(alloc(&mut memory, 200, JITMemoryKind::ReadOnly));
            seen.push(alloc(&mut memory, 40, JITMemoryKind::Writable));
        }
        seen.push(alloc(&mut memory, 5 << 20, JITMemoryKind::Executable));
        let (low, high) = (seen.iter().min().unwrap(), seen.iter().max().unwrap());
        assert!(*low >= memory.base && *high < memory.base + MODULE_RESERVATION);
        assert!(high - low < 1 << 31, "two allocations out of 32-bit reach");
        for (i, a) in seen.iter().enumerate() {
            assert_eq!(a % 16, 0, "allocation {i} misaligned");
        }
        // SAFETY: nothing was handed out to run.
        unsafe { memory.free_memory() };
        assert_eq!(memory.base, 0);
    }

    #[test]
    fn written_bytes_survive_finalize_and_code_runs() {
        let mut memory = ModuleMemory::default();
        let code = alloc(&mut memory, 16, JITMemoryKind::Executable) as *mut u8;
        let data = alloc(&mut memory, 8, JITMemoryKind::ReadOnly) as *mut u64;
        // `mov eax, 42; ret` on x86_64; `mov w0, #42; ret` on aarch64.
        #[cfg(target_arch = "x86_64")]
        let body: &[u8] = &[0xb8, 42, 0, 0, 0, 0xc3];
        #[cfg(target_arch = "aarch64")]
        let body: &[u8] = &[0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];
        // SAFETY: both allocations are committed read-write until finalize.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), code, body.len());
            data.write(7);
        }
        memory.finalize(BranchProtection::None).expect("finalize");
        // SAFETY: `code` now holds a complete function of this signature,
        // and `data` stays readable.
        let (answer, stored) = unsafe {
            let f: extern "C" fn() -> u32 = std::mem::transmute(code);
            (f(), data.read())
        };
        assert_eq!((answer, stored), (42, 7));
        // SAFETY: the function above has returned.
        unsafe { memory.free_memory() };
    }
}
