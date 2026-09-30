//! The synchronous trial-deletion (Bacon-Rajan) cycle collector.

use super::*;

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
        // SAFETY: `s` is a live, thread-local candidate the collector's roots keep alive.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } == COLOR_GRAY {
            continue;
        }
        // SAFETY: `h` is the header of the live, thread-local node `s`.
        unsafe { set_color(h, COLOR_GRAY) };
        // SAFETY: `s` is a live node whose children this walk visits.
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
        // SAFETY: `s` is a live, thread-local candidate the collector's roots keep alive.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } != COLOR_GRAY {
            continue;
        }
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { strong_count(h) } > 0 {
            // SAFETY: `s` is a live node.
            unsafe { scan_black(s) };
        } else {
            // SAFETY: `h` is the header of the live, thread-local node `s`.
            unsafe { set_color(h, COLOR_WHITE) };
            // Shared children were never grayed (external live edges);
            // skip them so their flag word is never even read as a color.
            // SAFETY: `s` is a live node whose children this walk visits.
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
        // SAFETY: `s` is a live, thread-local node the collector reaches.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } == COLOR_BLACK {
            continue;
        }
        // SAFETY: `h` is the header of the live, thread-local node `s`.
        unsafe { set_color(h, COLOR_BLACK) };
        // SAFETY: `s` is a live node whose children this walk visits.
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
        // SAFETY: `s` is a live, thread-local node the collector reaches.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } != COLOR_WHITE {
            continue;
        }
        // SAFETY: `h` is the header of the live, thread-local node `s`.
        unsafe { set_color(h, COLOR_BLACK) };
        // Use visit_children_raw so string children are freed via their own
        // destructor rather than silently skipped, mirroring rc_release_impl.
        // SAFETY: `s` is a garbage cycle member, whose children this walk gives back.
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
        // SAFETY: `s` is a cycle member the walk above collected.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the collected node `s`.
        if unsafe { strong_count(h) } == 0 && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0 {
            CYCLES_FREED.fetch_add(1, Ordering::Relaxed);
            freed.push(s);
            // SAFETY: `s` has no strong or weak reference left (checked above).
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
        // SAFETY: `payload` is non-null (checked above), a candidate the string probe accepts.
        if unsafe { crate::c_abi::string::is_gos_string(payload.cast()) } {
            // SAFETY: `payload` is a live string body (the probe above), whose share the edge
            // held.
            unsafe { crate::c_abi::string::gos_rt_str_free(payload.cast()) };
            continue;
        }
        // SAFETY: `payload` is a live node the edge held a share of.
        let h = unsafe { header_ptr(payload) };
        // SAFETY: `h` is the header of the live node `payload`.
        let d = unsafe { dec_strong(h) };
        if d.skip || d.next != 0 {
            continue;
        }
        if d.shared {
            fence(Ordering::Acquire);
        } else {
            // SAFETY: `h` is the header of the live, thread-local node `payload`.
            unsafe { set_color(h, COLOR_BLACK) };
        }
        // SAFETY: `payload` is the node whose children this release gives back.
        unsafe { release_children_into(payload, &mut worklist) };
        // SAFETY: `payload` is the node whose count this edge just dropped.
        unsafe { try_reclaim(payload) };
    }
}

/// Run a synchronous trial-deletion collection over the candidate buffer.
/// Reclaims unreachable cyclic RC garbage; live data and acyclic garbage are
/// untouched. Cost is proportional to the subgraph reachable from the
/// processed candidates, not the whole heap. Full drain (`budget = None`),
/// used by the explicit `runtime::collect_cycles()`.
unsafe fn collect_cycles() {
    // SAFETY: collection runs on the owning thread, whose root buffer keeps every candidate
    // alive.
    unsafe { collect_cycles_budgeted(None) };
}

/// Trial-deletion collection bounded to at most `budget` candidate roots per
/// call (`None` = drain everything). Bounding keeps an automatic collection
/// from traversing an unbounded number of independent candidate subgraphs
/// inline; leftover candidates remain buffered for the next slice. When a
/// budget is given, the adaptive `COLLECT_THRESHOLD` is updated from how much
/// this slice reclaimed - little garbage backs the threshold off, productive
/// slices snap it back to the base.
pub(super) unsafe fn collect_cycles_budgeted(budget: Option<usize>) {
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
    // SAFETY: `roots` are candidates the root buffer kept alive, taken on the owning thread.
    unsafe { collect_cycles_slice(budget, roots) };
    // SAFETY: this runs at the collection's teardown exit, which drains its queues.
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
        // SAFETY: `s` is a candidate the root buffer kept alive.
        let h = unsafe { header_ptr(s) };
        // A candidate that escaped to another goroutine after being
        // buffered: drop it from the collector. The stale buffered pin is
        // cleared atomically (its ROOTS entry is being dropped right here),
        // and the block is reclaimed if its count already fell to zero -
        // the releasing goroutine refused to free while the pin was set.
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { is_shared(h) } {
            // SAFETY: `h` is the header of a shared node, whose count is only ever accessed
            // atomically.
            let a = unsafe { AtomicU32::from_ptr(std::ptr::addr_of_mut!((*h).strong)) };
            a.fetch_and(!BUFFERED_BIT, Ordering::AcqRel);
            // SAFETY: `s` is the shared candidate just unpinned.
            unsafe { try_reclaim(s) };
            continue;
        }
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } == COLOR_PURPLE {
            // SAFETY: `s` is a live, thread-local purple candidate.
            unsafe { mark_gray(s) };
            scan_roots.push(s);
        } else {
            // SAFETY: `h` is the header of the live, thread-local node `s`.
            unsafe { set_buffered(h, false) };
            // A candidate whose count later fell to 0 *and* is black is
            // acyclic garbage whose children were already released; reclaim
            // it. A gray candidate is mid-trace (reachable from another
            // purple root) and must be left to scan/collect - never freed
            // here, or a live cycle member would be dropped.
            // SAFETY: `h` is the header of the live node `s`.
            if unsafe { color_of(h) } == COLOR_BLACK && unsafe { strong_count(h) } == 0 {
                dead.push(s);
            }
        }
    }
    for &s in &scan_roots {
        // SAFETY: `s` is a candidate the walk above grayed, still alive.
        unsafe { scan(s) };
    }
    let mut freed_nodes: Vec<*mut u8> = Vec::new();
    for s in scan_roots {
        // SAFETY: `s` is a candidate the walk above grayed, still alive.
        let h = unsafe { header_ptr(s) };
        // SAFETY: `h` is the header of the live, thread-local node `s`.
        unsafe { set_buffered(h, false) };
        // SAFETY: `h` is the header of the live node `s`.
        if unsafe { color_of(h) } == COLOR_WHITE {
            // SAFETY: `s` is a white candidate: a garbage cycle member.
            unsafe { collect_white(s, &mut freed_nodes) };
        }
    }
    // Reclaim the dead leftovers last: count 0 means nothing references them,
    // so no MarkGray traversal touched them.
    for s in dead {
        // SAFETY: `s` is a dead candidate with no references left.
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
    // SAFETY: collection runs on the calling thread, whose root buffer keeps every candidate
    // alive.
    unsafe { collect_cycles() };
}
