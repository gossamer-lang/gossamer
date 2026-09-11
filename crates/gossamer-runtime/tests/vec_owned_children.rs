//! Leak regression tests for the `AGGR_OWNED` slot-children mechanism
//! and the `STRING`-typed materializer conversions: every vec returned
//! by a string-bearing materializer shim must reclaim its embedded
//! c-strings (and nested vecs) when freed WITHOUT full iteration - the
//! ABI-level shape of a Gossamer `for … { break }`.
//!
//! Leak-freedom is asserted through the allocation ledger
//! (`c_abi::ledger::STR_LIVE` / `VEC_LIVE`): the live counters must
//! return to their baselines once everything a test allocated is freed.
//! The ledger counters are process-global, so every test serialises on
//! [`LEDGER_LOCK`].

use std::ffi::{CStr, CString};
use std::sync::atomic::Ordering;

use gossamer_runtime::c_abi::encoding::gos_rt_pem_decode_all_raw;
use gossamer_runtime::c_abi::ledger::{STR_LIVE, VEC_LIVE};
use gossamer_runtime::c_abi::map::{
    gos_rt_map_free, gos_rt_map_insert_i64_i64, gos_rt_map_insert_str_i64, gos_rt_map_keys_str,
    gos_rt_map_len, gos_rt_map_new, gos_rt_map_new_with_capacity_typed,
    gos_rt_map_or_insert_str_i64, gos_rt_map_pop_i64, gos_rt_map_remove_i64,
    gos_rt_map_set_vec_values, gos_rt_vec_free,
};
use gossamer_runtime::c_abi::rc::{
    gos_rt_arena_pop, gos_rt_arena_push, gos_rt_rc_alloc_copy, gos_rt_rc_release, gos_rt_rc_retain,
    region_is_active,
};
use gossamer_runtime::c_abi::regex::{
    gos_rt_regex_captures, gos_rt_regex_captures_all, gos_rt_regex_compile, gos_rt_regex_find_all,
    gos_rt_regex_split,
};
use gossamer_runtime::c_abi::signal::gos_rt_vec_slice;
use gossamer_runtime::c_abi::string::{
    alloc_cstring, gos_rt_str_free, gos_rt_str_lines, gos_rt_str_split, gos_rt_vec_clone,
};
use gossamer_runtime::c_abi::vec::{
    GosVec, VecSlotChild, gos_rt_result_disc, gos_rt_result_payload, gos_rt_vec_push,
    gos_rt_vec_retain, gos_rt_vec_with_capacity, gos_rt_vec_with_capacity_typed, vec_elem_kind,
    vec_is_region, vec_set_slot_children, vec_slot_children,
};

static LEDGER_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Takes the ledger lock and arms the recording hooks. A process that asked
/// for no instrumentation leaves the live counters off, and a counter only
/// reflects events recorded while it is armed, so every ledger test reads its
/// baseline inside this scope rather than before it.
fn ledger_scope() -> parking_lot::MutexGuard<'static, ()> {
    let guard = LEDGER_LOCK.lock();
    gossamer_runtime::c_abi::ledger::arm_instrumentation();
    guard
}

fn str_live() -> i64 {
    STR_LIVE.load(Ordering::SeqCst)
}

fn vec_live() -> i64 {
    VEC_LIVE.load(Ordering::SeqCst)
}

#[test]
fn with_capacity_uses_the_active_region() {
    let _guard = ledger_scope();
    gos_rt_arena_push();
    if !region_is_active() {
        // Targets without virtual-memory reservations intentionally fall back
        // to ordinary reference-counted allocation.
        return;
    }
    unsafe {
        let v = gos_rt_vec_with_capacity(8, 16);
        assert!(!v.is_null());
        assert!(
            vec_is_region(&*v),
            "with_capacity must not bypass an active arena"
        );
        assert!((*v).cap >= 16);
        let typed = gos_rt_vec_with_capacity_typed(8, 24, vec_elem_kind::STRING);
        assert!(!typed.is_null());
        assert!(
            vec_is_region(&*typed),
            "typed with_capacity must not bypass an active arena"
        );
        assert!((*typed).cap >= 24);
        // Region Vec frees are intentionally no-ops; the matching pop owns it.
        gos_rt_vec_free(v);
        gos_rt_vec_free(typed);
    }
    gos_rt_arena_pop();
}

#[test]
fn vec_valued_map_releases_replaced_and_removed_entries() {
    let _guard = ledger_scope();
    let base = vec_live();
    unsafe {
        let map = gos_rt_map_new(8, 8);
        gos_rt_map_set_vec_values(map);

        let first = gos_rt_vec_with_capacity(8, 1);
        // The insert mints the map's share itself, so the caller hands over a
        // value and keeps only its own.
        gos_rt_map_insert_i64_i64(map, 1, first as i64);
        gos_rt_vec_free(first); // source binding's normal scope cleanup
        assert_eq!(vec_live(), base + 1, "map must own the first Vec share");

        let second = gos_rt_vec_with_capacity(8, 1);
        gos_rt_map_insert_i64_i64(map, 1, second as i64);
        gos_rt_vec_free(second); // source cleanup
        assert_eq!(
            vec_live(),
            base + 1,
            "overwriting must release the old entry before retaining only the replacement"
        );

        assert!(gos_rt_map_remove_i64(map, 1));
        assert_eq!(vec_live(), base, "remove must release the map-owned Vec");

        let popped = gos_rt_vec_with_capacity(8, 1);
        gos_rt_map_insert_i64_i64(map, 2, popped as i64);
        gos_rt_vec_free(popped); // source cleanup
        let popped_result = gos_rt_map_pop_i64(map, 2);
        assert_eq!(gos_rt_result_disc(popped_result), 0, "pop must return Some");
        let popped_value = gos_rt_result_payload(popped_result) as *mut GosVec;
        assert_eq!(
            vec_live(),
            base + 1,
            "pop transfers the map share to its result"
        );
        gos_rt_vec_free(popped_value); // receiver consumes the transferred share
        assert_eq!(
            vec_live(),
            base,
            "popped Vec must be released by its receiver"
        );
        gos_rt_map_free(map);
    }
    assert_eq!(vec_live(), base, "map teardown must not leak Vec entries");
}

#[test]
fn compact_i64_byte_vec_map_leaves_the_caller_its_own_share() {
    let _guard = ledger_scope();
    let base = vec_live();
    unsafe {
        let map = gos_rt_map_new_with_capacity_typed(0, 2, 4);
        gos_rt_map_set_vec_values(map);

        for key in 0..1000 {
            let payload = gos_rt_vec_with_capacity(1, 32);
            for byte in 0u8..32 {
                gos_rt_vec_push(payload, (&raw const byte).cast());
            }
            gos_rt_map_insert_i64_i64(map, key, payload as i64);
            // The entry keeps the BYTES it copied, not the header, and takes
            // no share of the source - so the caller releases its own, the
            // way it does after any other keyed insert.
            gos_rt_vec_free(payload);
        }

        assert_eq!(
            vec_live(),
            base,
            "a byte entry owns its copied bytes, so no header outlives its caller"
        );
        assert_eq!(gos_rt_map_len(map), 1000);

        // A caller holding its own share keeps it: the insert consumes the
        // one it was handed and no more.
        let kept = gos_rt_vec_with_capacity(1, 8);
        for byte in 0u8..8 {
            gos_rt_vec_push(kept, (&raw const byte).cast());
        }
        gos_rt_map_insert_i64_i64(map, 1000, kept as i64);
        assert_eq!(vec_live(), base + 1, "the reader's own share survives");
        gos_rt_vec_free(kept);

        gos_rt_map_free(map);
    }
    assert_eq!(
        vec_live(),
        base,
        "compact byte map teardown must be balanced"
    );
}

#[test]
fn compact_str_byte_vec_map_preserves_loop_carried_source_share() {
    let _guard = ledger_scope();
    let base = vec_live();
    unsafe {
        let map = gos_rt_map_new_with_capacity_typed(1, 2, 4);
        gos_rt_map_set_vec_values(map);

        let payload = gos_rt_vec_with_capacity(1, 64);
        for byte in 0u8..64 {
            gos_rt_vec_push(payload, (&raw const byte).cast());
        }
        let key = alloc_cstring(b"key-00000000");
        gos_rt_map_insert_str_i64(map, key, payload as i64);

        assert_eq!(
            vec_live(),
            base + 1,
            "compact str-byte map insert must leave the source cleanup share"
        );
        gos_rt_vec_free(payload);
        assert_eq!(
            vec_live(),
            base,
            "source cleanup must release the final Vec share"
        );
        assert_eq!(gos_rt_map_len(map), 1);
        gos_rt_map_free(map);
    }
    assert_eq!(
        vec_live(),
        base,
        "compact str-byte map teardown must be balanced"
    );
}

#[test]
fn vec_valued_map_or_insert_transfers_only_the_map_share() {
    let _guard = ledger_scope();
    let base = vec_live();
    unsafe {
        let map = gos_rt_map_new(8, 8);
        gos_rt_map_set_vec_values(map);

        // This is the exact compiled-tier ownership protocol: the lowering
        // retains the consuming value argument for the map, while `or_insert`
        // returns only an interior borrow of the stored Vec.
        let first = gos_rt_vec_with_capacity(8, 1);
        gos_rt_vec_retain(first);
        let first_key = alloc_cstring(b"first");
        gos_rt_rc_retain(first_key.cast());
        assert_eq!(
            gos_rt_map_or_insert_str_i64(map, first_key, first as i64),
            first as i64
        );
        gos_rt_str_free(first_key); // caller's source share
        gos_rt_vec_free(first); // caller's source share
        assert_eq!(vec_live(), base + 1, "map must retain the inserted Vec");

        // A present-key call discards the prospective map share of the new
        // default and returns the existing interior borrow without retaining
        // it for a caller that must not free it.
        let unused = gos_rt_vec_with_capacity(8, 1);
        gos_rt_vec_retain(unused);
        let present_key = alloc_cstring(b"first");
        gos_rt_rc_retain(present_key.cast());
        assert_eq!(
            gos_rt_map_or_insert_str_i64(map, present_key, unused as i64),
            first as i64
        );
        gos_rt_str_free(present_key); // caller's source share
        gos_rt_vec_free(unused); // caller's source share
        assert_eq!(
            vec_live(),
            base + 1,
            "present default must not leak a Vec share"
        );

        gos_rt_map_free(map);
    }
    assert_eq!(vec_live(), base, "or_insert map teardown must be balanced");
}

#[test]
fn structural_copy_blob_retains_and_releases_string_and_vec_children() {
    let _guard = ledger_scope();
    let str_base = str_live();
    let vec_base = vec_live();
    let meta = [
        gossamer_abi::rc::RC_KIND_STRUCT,
        1,
        0,
        2,
        0,
        (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
    ];
    unsafe {
        let name = alloc_cstring(b"copied-name");
        let tags = gos_rt_vec_with_capacity(8, 1);
        // Aggregate ABI slots intentionally carry exposed pointer words.
        let source = [
            name.expose_provenance() as i64,
            tags.expose_provenance() as i64,
        ];
        let blob = gos_rt_rc_alloc_copy(16, meta.as_ptr(), source.as_ptr().cast());
        assert!(!blob.is_null());

        // The copy holds its own shares after the source aggregate dies.
        gos_rt_str_free(name);
        gos_rt_vec_free(tags);
        assert_eq!(str_live(), str_base + 1);
        assert_eq!(vec_live(), vec_base + 1);
        let copied_name = std::ptr::with_exposed_provenance(blob.cast::<usize>().read_unaligned());
        let copied_tags: *mut GosVec =
            std::ptr::with_exposed_provenance_mut(blob.add(8).cast::<usize>().read_unaligned());
        assert_eq!(CStr::from_ptr(copied_name).to_bytes(), b"copied-name");
        assert_eq!((*copied_tags).len, 0, "the copied Vec remains live");

        gos_rt_rc_release(blob);
    }
    assert_eq!(str_live(), str_base, "copy blob leaked its String child");
    assert_eq!(vec_live(), vec_base, "copy blob leaked its Vec child");
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// First buffer slot of `v` read as a c-string pointer. The element
/// buffer is exposed as `*mut u8` at the C ABI, so the slot word is
/// read unaligned (the allocation is 8-byte aligned in practice).
fn first_slot_cstr(v: *const GosVec) -> *const std::ffi::c_char {
    let vec = unsafe { &*v };
    let raw = unsafe { std::ptr::read_unaligned(vec.ptr.as_ptr().cast::<usize>()) };
    // Aggregate ABI slots intentionally carry pointer words. Recover the
    // provenance explicitly so Miri validates the same raw-slot contract
    // rather than treating the test's integer round trip as a dangling ptr.
    std::ptr::with_exposed_provenance(raw)
}

#[test]
fn regex_split_vec_is_string_typed_and_free_reclaims_pieces() {
    let _guard = ledger_scope();
    let pat = cstr(",");
    let re = unsafe { gos_rt_regex_compile(pat.as_ptr()) };
    let text = cstr("a,b,c,d");
    let str_base = str_live();
    let v = unsafe { gos_rt_regex_split(re, text.as_ptr()) };
    assert!(!v.is_null());
    let vec = unsafe { &*v };
    assert_eq!(vec.len, 4);
    assert_eq!(vec.elem_kind, vec_elem_kind::STRING);
    assert!(str_live() > str_base, "split allocated piece strings");
    // Free WITHOUT iterating - the early-break shape at the ABI level.
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "split pieces leaked on free");
}

#[test]
fn regex_find_all_free_without_iteration_reclaims_match_strings() {
    let _guard = ledger_scope();
    let pat = cstr("ab+");
    let re = unsafe { gos_rt_regex_compile(pat.as_ptr()) };
    let text = cstr("xabby_ab_abbb");
    let str_base = str_live();
    let v = unsafe { gos_rt_regex_find_all(re, text.as_ptr()) };
    assert!(!v.is_null());
    let vec = unsafe { &*v };
    assert_eq!(vec.len, 3);
    assert_eq!(vec.elem_bytes, 24);
    assert_eq!(vec.elem_kind, vec_elem_kind::AGGR_OWNED);
    let layout =
        vec_slot_children(unsafe { &*v }).expect("find_all registers a slot-children layout");
    assert_eq!(layout.len(), 1);
    assert_eq!(layout[0].gate, -1);
    assert_eq!(layout[0].word, 2);
    assert_eq!(layout[0].kind, vec_elem_kind::STRING);
    assert_eq!(str_live(), str_base + 3);
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "find_all match strings leaked");
}

#[test]
fn regex_captures_free_without_iteration_reclaims_some_groups() {
    let _guard = ledger_scope();
    let pat = cstr("(a)(b)?");
    let re = unsafe { gos_rt_regex_compile(pat.as_ptr()) };
    let text = cstr("a");
    let str_base = str_live();
    let r = unsafe { gos_rt_regex_captures(re, text.as_ptr()) };
    assert_eq!(gos_rt_result_disc(r), 0, "expected Some(captures)");
    let inner = gos_rt_result_payload(r) as *mut GosVec;
    assert!(!inner.is_null());
    let vec = unsafe { &*inner };
    // Group 0 (full match) + group 1 = two Some strings; group 2 = None.
    assert_eq!(vec.len, 3);
    assert_eq!(vec.elem_bytes, 16);
    assert_eq!(vec.elem_kind, vec_elem_kind::AGGR_OWNED);
    let layout =
        vec_slot_children(unsafe { &*inner }).expect("captures registers a slot-children layout");
    assert_eq!(layout.len(), 1);
    assert_eq!(layout[0].gate, 0);
    assert_eq!(layout[0].disc_word, 0);
    assert_eq!(layout[0].word, 1);
    assert_eq!(str_live(), str_base + 2);
    unsafe { gos_rt_vec_free(inner) };
    assert_eq!(str_live(), str_base, "Some-group strings leaked");
}

#[test]
fn regex_captures_all_outer_free_recursively_reclaims_rows_and_strings() {
    let _guard = ledger_scope();
    let pat = cstr("(a)(b)?");
    let re = unsafe { gos_rt_regex_compile(pat.as_ptr()) };
    let text = cstr("a ab a");
    let str_base = str_live();
    let vec_base = vec_live();
    let outer = unsafe { gos_rt_regex_captures_all(re, text.as_ptr()) };
    assert!(!outer.is_null());
    let o = unsafe { &*outer };
    assert_eq!(o.len, 3);
    assert_eq!(o.elem_kind, vec_elem_kind::VEC);
    // Free without touching any row: outer free must cascade through
    // the inner rows and their Some-group strings.
    unsafe { gos_rt_vec_free(outer) };
    assert_eq!(str_live(), str_base, "captures_all group strings leaked");
    assert_eq!(vec_live(), vec_base, "captures_all row vecs leaked");
}

#[test]
fn str_split_and_lines_vecs_are_string_typed_and_leak_free() {
    let _guard = ledger_scope();
    let s = cstr("a:b:c");
    let sep = cstr(":");
    let str_base = str_live();
    let v = unsafe { gos_rt_str_split(s.as_ptr(), sep.as_ptr()) };
    assert_eq!(unsafe { (*v).elem_kind }, vec_elem_kind::STRING);
    assert_eq!(unsafe { (*v).len }, 3);
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "split pieces leaked");

    let text = cstr("l1\nl2\nl3");
    let v = unsafe { gos_rt_str_lines(text.as_ptr()) };
    assert_eq!(unsafe { (*v).elem_kind }, vec_elem_kind::STRING);
    assert_eq!(unsafe { (*v).len }, 3);
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "lines leaked");
}

#[test]
fn map_keys_str_snapshot_is_string_typed_and_leak_free() {
    let _guard = ledger_scope();
    let m = unsafe { gos_rt_map_new(8, 8) };
    let k1 = cstr("alpha");
    let k2 = cstr("beta");
    unsafe {
        gos_rt_map_insert_str_i64(m, k1.as_ptr(), 1);
        gos_rt_map_insert_str_i64(m, k2.as_ptr(), 2);
    }
    let str_base = str_live();
    let keys = unsafe { gos_rt_map_keys_str(m) };
    assert_eq!(unsafe { (*keys).elem_kind }, vec_elem_kind::STRING);
    assert_eq!(unsafe { (*keys).len }, 2);
    assert_eq!(str_live(), str_base + 2);
    unsafe { gos_rt_vec_free(keys) };
    assert_eq!(str_live(), str_base, "keys_str snapshot strings leaked");
    unsafe { gos_rt_map_free(m) };
}

#[test]
fn pem_decode_all_free_reclaims_labels_and_body_vecs() {
    let _guard = ledger_scope();
    let pem = cstr(concat!(
        "-----BEGIN FIRST-----\nAAAA\n-----END FIRST-----\n",
        "-----BEGIN SECOND-----\nAAAA\n-----END SECOND-----\n",
    ));
    let str_base = str_live();
    let vec_base = vec_live();
    let r = unsafe { gos_rt_pem_decode_all_raw(pem.as_ptr()) };
    assert_eq!(gos_rt_result_disc(r), 0, "expected Ok(blocks)");
    let v = gos_rt_result_payload(r) as *mut GosVec;
    let vec = unsafe { &*v };
    assert_eq!(vec.len, 2);
    assert_eq!(vec.elem_kind, vec_elem_kind::AGGR_OWNED);
    let layout =
        vec_slot_children(unsafe { &*v }).expect("pem blocks register a slot-children layout");
    assert_eq!(layout.len(), 2);
    assert_eq!(layout[1].kind, vec_elem_kind::VEC);
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "pem labels leaked");
    assert_eq!(vec_live(), vec_base, "pem body vecs leaked");
}

/// Header-shaped layout: 16-byte `(String, String)` slots, both words
/// unconditional. Mirrors `gos_rt_http_response_headers`.
static PAIR_SLOTS: [VecSlotChild; 2] = [
    VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 0,
        kind: vec_elem_kind::STRING,
    },
    VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 1,
        kind: vec_elem_kind::STRING,
    },
];

fn build_pair_vec(pairs: &[(&str, &str)]) -> *mut GosVec {
    let v = unsafe { gos_rt_vec_with_capacity(16, pairs.len() as i64) };
    for (a, b) in pairs {
        let a = alloc_cstring(a.as_bytes());
        let b = alloc_cstring(b.as_bytes());
        let slot: [i64; 2] = [a.expose_provenance() as i64, b.expose_provenance() as i64];
        unsafe { gos_rt_vec_push(v, slot.as_ptr().cast::<u8>()) };
    }
    vec_set_slot_children(v, &PAIR_SLOTS);
    v
}

#[test]
fn two_string_slot_vec_partial_read_then_free_reclaims_all_slots() {
    let _guard = ledger_scope();
    let str_base = str_live();
    let v = build_pair_vec(&[("content-type", "text/plain"), ("x-id", "abc")]);
    assert_eq!(str_live(), str_base + 4);
    // Borrow slot 0 only (the early-break consumer shape) - reads never
    // transfer ownership.
    let name0 = first_slot_cstr(v);
    let got = unsafe { std::ffi::CStr::from_ptr(name0) }.to_str().unwrap();
    assert_eq!(got, "content-type");
    unsafe { gos_rt_vec_free(v) };
    assert_eq!(str_live(), str_base, "unvisited slot strings leaked");
}

#[test]
fn push_onto_tagged_vec_retains_children_for_balanced_frees() {
    let _guard = ledger_scope();
    let str_base = str_live();
    let v = build_pair_vec(&[("a", "b")]);
    // Push a slot whose strings the CALLER keeps holding - the tagged
    // push must retain so vec free + caller free are both balanced.
    let s1 = alloc_cstring(b"name");
    let s2 = alloc_cstring(b"value");
    let slot: [i64; 2] = [s1 as i64, s2 as i64];
    unsafe { gos_rt_vec_push(v, slot.as_ptr().cast::<u8>()) };
    unsafe { gos_rt_vec_free(v) };
    // The vec's shares are gone; the caller's shares are still live.
    assert_eq!(str_live(), str_base + 2);
    unsafe {
        gossamer_runtime::c_abi::string::gos_rt_str_free(s1);
        gossamer_runtime::c_abi::string::gos_rt_str_free(s2);
    }
    assert_eq!(str_live(), str_base, "push-retain shares unbalanced");
}

#[test]
fn clone_of_string_typed_vec_shares_then_frees_balanced() {
    let _guard = ledger_scope();
    let s = cstr("x,y,z");
    let sep = cstr(",");
    let str_base = str_live();
    let v = unsafe { gos_rt_str_split(s.as_ptr(), sep.as_ptr()) };
    let c = unsafe { gos_rt_vec_clone(v) };
    assert_eq!(unsafe { (*c).elem_kind }, vec_elem_kind::STRING);
    unsafe { gos_rt_vec_free(v) };
    // Clone still holds its shares - the pieces must be readable.
    let p0 = first_slot_cstr(c);
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(p0) }.to_str().unwrap(),
        "x"
    );
    unsafe { gos_rt_vec_free(c) };
    assert_eq!(str_live(), str_base, "clone shares unbalanced");
}

#[test]
fn clone_of_aggr_owned_vec_shares_then_frees_balanced() {
    let _guard = ledger_scope();
    let str_base = str_live();
    let v = build_pair_vec(&[("k1", "v1"), ("k2", "v2")]);
    let c = unsafe { gos_rt_vec_clone(v) };
    assert_eq!(unsafe { (*c).elem_kind }, vec_elem_kind::AGGR_OWNED);
    assert!(
        vec_slot_children(unsafe { &*c }).is_some(),
        "clone inherits the layout"
    );
    unsafe { gos_rt_vec_free(v) };
    let p0 = first_slot_cstr(c);
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(p0) }.to_str().unwrap(),
        "k1"
    );
    unsafe { gos_rt_vec_free(c) };
    assert_eq!(str_live(), str_base, "AGGR_OWNED clone shares unbalanced");
}

#[test]
fn slice_of_string_typed_vec_shares_then_frees_balanced() {
    let _guard = ledger_scope();
    let s = cstr("a,b,c,d");
    let sep = cstr(",");
    let str_base = str_live();
    let v = unsafe { gos_rt_str_split(s.as_ptr(), sep.as_ptr()) };
    let sl = unsafe { gos_rt_vec_slice(v, 1, 3) };
    assert_eq!(unsafe { (*sl).elem_kind }, vec_elem_kind::STRING);
    assert_eq!(unsafe { (*sl).len }, 2);
    unsafe { gos_rt_vec_free(v) };
    let p0 = first_slot_cstr(sl);
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(p0) }.to_str().unwrap(),
        "b"
    );
    unsafe { gos_rt_vec_free(sl) };
    assert_eq!(str_live(), str_base, "slice shares unbalanced");
}
