//! Vec scalar swap runtime tests.

use gossamer_runtime::c_abi::vec::{GosVec, vec_owner_generation};
use gossamer_runtime::c_abi::{
    gos_rt_vec_free, gos_rt_vec_get_i64, gos_rt_vec_new, gos_rt_vec_push, gos_rt_vec_push_i64,
    gos_rt_vec_swap_i64,
};

#[test]
fn vec_swap_i64_exchanges_in_range_elements_and_ignores_oob() {
    // SAFETY: The test uses a runtime-owned Vec and releases it with
    // `gos_rt_vec_free` after all element accesses.
    unsafe {
        let v = gos_rt_vec_new(8);
        gos_rt_vec_push_i64(v, 10);
        gos_rt_vec_push_i64(v, 20);
        gos_rt_vec_push_i64(v, 30);

        gos_rt_vec_swap_i64(v, 0, 2);
        assert_eq!(gos_rt_vec_get_i64(v, 0), 30);
        assert_eq!(gos_rt_vec_get_i64(v, 1), 20);
        assert_eq!(gos_rt_vec_get_i64(v, 2), 10);

        gos_rt_vec_swap_i64(v, -1, 1);
        gos_rt_vec_swap_i64(v, 1, 99);
        assert_eq!(gos_rt_vec_get_i64(v, 0), 30);
        assert_eq!(gos_rt_vec_get_i64(v, 1), 20);
        assert_eq!(gos_rt_vec_get_i64(v, 2), 10);

        gos_rt_vec_free(v);
    }
}

#[test]
fn vec_swap_exchanges_multiword_elements() {
    // SAFETY: The test writes two fixed-width elements into a runtime-owned
    // Vec, swaps them, inspects the backing buffer while the Vec is live, and
    // releases the Vec exactly once.
    unsafe {
        let v = gos_rt_vec_new(16);
        let first = [1_i64, 2_i64];
        let second = [3_i64, 4_i64];
        gos_rt_vec_push(v, first.as_ptr().cast());
        gos_rt_vec_push(v, second.as_ptr().cast());

        gos_rt_vec_swap_i64(v, 0, 1);
        let data = (*v).ptr.as_ptr();
        let actual = [
            std::ptr::read_unaligned(data.cast::<i64>()),
            std::ptr::read_unaligned(data.add(8).cast::<i64>()),
            std::ptr::read_unaligned(data.add(16).cast::<i64>()),
            std::ptr::read_unaligned(data.add(24).cast::<i64>()),
        ];
        assert_eq!(actual, [3, 4, 1, 2]);

        gos_rt_vec_free(v);
    }
}

#[test]
fn vec_owner_releases_once_and_allows_address_reuse() {
    // A raw ABI pointer does not contain a generation, so a duplicate free is
    // undefined even if an address-set happens to reject it today. Keep this
    // test Miri-valid: each owner is released exactly once while allocator
    // churn verifies that ordinary reuse remains correct.
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let v = gos_rt_vec_new(8);
        gos_rt_vec_push_i64(v, 1);
        gos_rt_vec_free(v);

        for i in 0..64 {
            let next = gos_rt_vec_new(8);
            gos_rt_vec_push_i64(next, i);
            assert_eq!(gos_rt_vec_get_i64(next, 0), i);
            gos_rt_vec_free(next);
        }
    }
}

#[test]
fn vec_owner_generation_is_distinct_from_the_header_address() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let first = gos_rt_vec_new(8);
        let first_generation = vec_owner_generation(&*first);
        assert_ne!(first_generation, 0);
        gos_rt_vec_free(first);

        let second = gos_rt_vec_new(8);
        let second_generation = vec_owner_generation(&*second);
        assert_ne!(second_generation, 0);
        assert_ne!(first_generation, second_generation);
        gos_rt_vec_free(second);
    }
}

#[test]
fn vec_prefix_is_pinned_and_primitive_vec_needs_no_owner_carrier() {
    assert_eq!(std::mem::offset_of!(GosVec, len), 0);
    assert_eq!(std::mem::offset_of!(GosVec, cap), 8);
    assert_eq!(std::mem::offset_of!(GosVec, elem_bytes), 16);
    assert_eq!(std::mem::offset_of!(GosVec, ptr), 24);
    assert_eq!(std::mem::offset_of!(GosVec, generation), 32);
    assert_eq!(std::mem::offset_of!(GosVec, elem_meta), 40);
    assert_eq!(std::mem::offset_of!(GosVec, owner), 48);
    assert_eq!(std::mem::offset_of!(GosVec, mutation_generation), 56);
    assert_eq!(std::mem::size_of::<GosVec>(), 64);

    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let v = gos_rt_vec_new(8);
        assert!(
            (*v).owner.is_null(),
            "primitive Vecs must not allocate optional ownership metadata"
        );
        gos_rt_vec_free(v);
    }
}

#[test]
fn concurrent_vec_lifetimes_release_each_owner_once() {
    // Each worker exercises independent allocation and final release without
    // address-keyed liveness locks or stale-pointer access.
    std::thread::scope(|scope| {
        for worker in 0..8_i64 {
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            scope.spawn(move || unsafe {
                for i in 0..2_000_i64 {
                    let v = gos_rt_vec_new(8);
                    gos_rt_vec_push_i64(v, worker * 2_000 + i);
                    assert_eq!(gos_rt_vec_get_i64(v, 0), worker * 2_000 + i);
                    gos_rt_vec_free(v);
                }
            });
        }
    });
}
