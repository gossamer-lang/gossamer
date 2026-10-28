#![allow(missing_docs)]
// Counting allocations needs a `GlobalAlloc`, an unsafe trait; this test
// binary installs one and only forwards to the system allocator.
#![allow(unsafe_code)]

// Lowering a body allocates about what the body holds. A pass that
// reserved, in every block, room for the whole body's insertions grew
// with blocks times insertions, which large generated bodies turned into
// gigabytes; the peak allocation while lowering shows it.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::lower;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator unchanged; the
// counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: the caller's layout contract passes through.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
        PEAK.fetch_max(live, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from this allocator with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn lowering_a_body_of_many_blocks_and_owners_allocates_about_its_size() {
    let mut src = String::from("fn many(n: i64) -> String {\n    let mut out = \"\"\n");
    for i in 0..120 {
        src.push_str(&format!(
            "    let s{i} = f\"{{n}}-{i}\"\n    if n > {i} {{ out = s{i} }}\n"
        ));
    }
    src.push_str("    out\n}\n\nfn main() { println(many(3)) }\n");
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let (bodies, _) = lower(&src);
    let peak = PEAK.load(Ordering::Relaxed) - before;
    let statements: usize = bodies
        .iter()
        .flat_map(|body| &body.blocks)
        .map(|block| block.stmts.len())
        .sum();
    let held = statements * std::mem::size_of::<gossamer_mir::Statement>();
    assert!(
        peak < 16 * held.max(1 << 20),
        "lowering peaked at {peak} bytes for {statements} statements ({held} bytes)"
    );
}
