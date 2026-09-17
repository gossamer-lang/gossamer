//! Runtime tests for bulk operations over vecs of flat elements.

use gossamer_runtime::c_abi::vec::{GosVec, vec_elem_kind};
use gossamer_runtime::c_abi::{
    gos_rt_vec_capacity, gos_rt_vec_clear, gos_rt_vec_copy_from_slice, gos_rt_vec_copy_within,
    gos_rt_vec_extend, gos_rt_vec_extend_str_bytes, gos_rt_vec_free, gos_rt_vec_len,
    gos_rt_vec_push, gos_rt_vec_truncate, gos_rt_vec_with_capacity_typed,
};

/// A vec of `elem_bytes`-wide elements of `kind` holding `elems` pushed in order.
///
/// # Safety
///
/// Every element of `elems` is exactly `elem_bytes` long.
unsafe fn pushed(elem_bytes: u32, kind: u8, elems: &[Vec<u8>]) -> *mut GosVec {
    // SAFETY: the vec is fresh and each element is `elem_bytes` long.
    unsafe {
        let v = gos_rt_vec_with_capacity_typed(elem_bytes, 0, kind);
        for elem in elems {
            gos_rt_vec_push(v, elem.as_ptr());
        }
        v
    }
}

/// The live element bytes of `v`.
///
/// # Safety
///
/// `v` is a live vec.
unsafe fn contents(v: *const GosVec) -> Vec<u8> {
    // SAFETY: `len * elem_bytes` bytes from `ptr` are the vec's initialised slots.
    unsafe {
        let vec = &*v;
        let bytes = vec.len as usize * vec.elem_bytes as usize;
        if bytes == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(vec.ptr.as_ptr(), bytes).to_vec()
    }
}

fn elems(elem_bytes: u32, count: usize, seed: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            (0..elem_bytes as usize)
                .map(|b| seed.wrapping_add((i * 31 + b * 7) as u8))
                .collect()
        })
        .collect()
}

/// Extending with `src` leaves `dst` as pushing each of `src`'s elements does:
/// the same bytes, length, and capacity, with `src` unchanged.
fn assert_extend_matches_pushes(elem_bytes: u32, kind: u8) {
    for dst_len in 0..20 {
        for src_len in 0..40 {
            let head = elems(elem_bytes, dst_len, 3);
            let tail = elems(elem_bytes, src_len, 101);
            // SAFETY: every vec is built, read, and released in this scope.
            unsafe {
                let extended = pushed(elem_bytes, kind, &head);
                let by_push = pushed(elem_bytes, kind, &head);
                let src = pushed(elem_bytes, kind, &tail);
                let src_before = contents(src);

                gos_rt_vec_extend(extended, src);
                for elem in &tail {
                    gos_rt_vec_push(by_push, elem.as_ptr());
                }

                let context = format!("elem_bytes={elem_bytes} dst={dst_len} src={src_len}");
                assert_eq!(contents(extended), contents(by_push), "{context}");
                assert_eq!(
                    gos_rt_vec_len(extended),
                    gos_rt_vec_len(by_push),
                    "{context}"
                );
                assert_eq!(
                    gos_rt_vec_capacity(extended),
                    gos_rt_vec_capacity(by_push),
                    "{context}"
                );
                assert_eq!(contents(src), src_before, "{context}");

                gos_rt_vec_free(extended);
                gos_rt_vec_free(by_push);
                gos_rt_vec_free(src);
            }
        }
    }
}

#[test]
fn extending_bytes_matches_pushing_each_byte() {
    assert_extend_matches_pushes(1, vec_elem_kind::PRIMITIVE);
}

#[test]
fn extending_words_matches_pushing_each_word() {
    assert_extend_matches_pushes(8, vec_elem_kind::PRIMITIVE);
}

#[test]
fn extending_flat_aggregates_matches_pushing_each_aggregate() {
    assert_extend_matches_pushes(24, vec_elem_kind::AGGR_FLAT);
}

#[test]
fn extending_a_vec_with_itself_doubles_its_elements() {
    let head = elems(8, 13, 9);
    // SAFETY: the vec is built, read, and released in this scope.
    unsafe {
        let v = pushed(8, vec_elem_kind::PRIMITIVE, &head);
        let before = contents(v);
        gos_rt_vec_extend(v, v);
        assert_eq!(contents(v), [before.clone(), before].concat());
        gos_rt_vec_free(v);
    }
}

#[test]
fn extending_bytes_with_a_string_matches_pushing_each_byte() {
    let text = "h\u{e9}llo, w\u{f6}rld";
    let mut c_text = text.as_bytes().to_vec();
    c_text.push(0);
    for dst_len in 0..20 {
        let head = elems(1, dst_len, 5);
        // SAFETY: the vecs are built, read, and released in this scope, and
        // `c_text` is a NUL-terminated string body.
        unsafe {
            let extended = pushed(1, vec_elem_kind::PRIMITIVE, &head);
            let by_push = pushed(1, vec_elem_kind::PRIMITIVE, &head);
            gos_rt_vec_extend_str_bytes(extended, c_text.as_ptr().cast());
            for byte in text.as_bytes() {
                gos_rt_vec_push(by_push, byte);
            }
            assert_eq!(contents(extended), contents(by_push), "dst={dst_len}");
            assert_eq!(
                gos_rt_vec_capacity(extended),
                gos_rt_vec_capacity(by_push),
                "dst={dst_len}"
            );
            gos_rt_vec_free(extended);
            gos_rt_vec_free(by_push);
        }
    }
}

#[test]
fn truncating_and_clearing_flat_elements_keep_the_prefix_and_capacity() {
    for (elem_bytes, kind) in [
        (1, vec_elem_kind::PRIMITIVE),
        (8, vec_elem_kind::PRIMITIVE),
        (24, vec_elem_kind::AGGR_FLAT),
    ] {
        let all = elems(elem_bytes, 37, 11);
        for keep in [0, 1, 20, 37, 50] {
            // SAFETY: the vec is built, read, and released in this scope.
            unsafe {
                let v = pushed(elem_bytes, kind, &all);
                let cap = gos_rt_vec_capacity(v);
                gos_rt_vec_truncate(v, keep);
                let kept = keep.min(37) as usize;
                assert_eq!(
                    contents(v),
                    all[..kept].concat(),
                    "bytes={elem_bytes} keep={keep}"
                );
                assert_eq!(gos_rt_vec_capacity(v), cap);
                gos_rt_vec_clear(v);
                assert_eq!(gos_rt_vec_len(v), 0);
                assert_eq!(gos_rt_vec_capacity(v), cap);
                gos_rt_vec_free(v);
            }
        }
    }
}

#[test]
fn copying_flat_elements_within_and_between_vecs_moves_whole_elements() {
    for (elem_bytes, kind) in [
        (1, vec_elem_kind::PRIMITIVE),
        (8, vec_elem_kind::PRIMITIVE),
        (24, vec_elem_kind::AGGR_FLAT),
    ] {
        let all = elems(elem_bytes, 10, 42);
        // SAFETY: the vecs are built, read, and released in this scope.
        unsafe {
            let v = pushed(elem_bytes, kind, &all);
            gos_rt_vec_copy_within(v, 1, 4, 5);
            let mut expected = all.clone();
            expected.splice(4..9, all[1..6].iter().cloned());
            assert_eq!(contents(v), expected.concat(), "within bytes={elem_bytes}");

            let other = elems(elem_bytes, 10, 7);
            let dst = pushed(elem_bytes, kind, &all);
            let src = pushed(elem_bytes, kind, &other);
            gos_rt_vec_copy_from_slice(dst, src);
            assert_eq!(
                contents(dst),
                other.concat(),
                "from_slice bytes={elem_bytes}"
            );
            assert_eq!(contents(src), other.concat());

            gos_rt_vec_free(v);
            gos_rt_vec_free(dst);
            gos_rt_vec_free(src);
        }
    }
}
