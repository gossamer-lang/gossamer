//! The global allocator a native build's binding archive takes.

#![allow(
    unsafe_code,
    reason = "a global allocator over the runtime's C entries"
)]

use std::alloc::{GlobalAlloc, Layout};

unsafe extern "C" {
    fn gos_rt_binding_heap_alloc(size: usize, align: usize) -> *mut u8;
    fn gos_rt_binding_heap_alloc_zeroed(size: usize, align: usize) -> *mut u8;
    fn gos_rt_binding_heap_dealloc(ptr: *mut u8, size: usize, align: usize);
    fn gos_rt_binding_heap_realloc(
        ptr: *mut u8,
        size: usize,
        align: usize,
        new_size: usize,
    ) -> *mut u8;
}

/// Allocates from the Gossamer runtime linked into the same program.
///
/// A native build links the bindings as one archive beside the runtime's,
/// each with its own `std`. The archive `gos build` generates installs this
/// as its `#[global_allocator]`, so memory one side allocates is freed by the
/// allocator that made it whichever copy of `std` frees it. A `gos run`
/// library is a whole program of its own and keeps the default allocator.
#[derive(Debug, Default, Clone, Copy)]
pub struct RuntimeAllocator;

// Each method is `#[inline]`, so only an archive that installs the allocator
// references the runtime's entries.
// SAFETY: each method forwards to the runtime's allocator, which upholds
// the `GlobalAlloc` contract.
unsafe impl GlobalAlloc for RuntimeAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `GlobalAlloc` contract.
        unsafe { gos_rt_binding_heap_alloc(layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `GlobalAlloc` contract.
        unsafe { gos_rt_binding_heap_alloc_zeroed(layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's `GlobalAlloc` contract.
        unsafe { gos_rt_binding_heap_dealloc(ptr, layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's `GlobalAlloc` contract.
        unsafe { gos_rt_binding_heap_realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
