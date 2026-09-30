//! ASAN-friendly stress reproductions of the askq hot path.
//!
//! Each test exercises a specific call shape that the compiled
//! tier emits for askq's chat round (parse JSON, walk children,
//! accumulate strings into a Vec, push tuples, etc.) so a
//! sanitizer run on this crate (`cargo +nightly test -p
//! gossamer-runtime --release askq_stress -Z sanitizer=address`)
//! flags any cross-domain free / use-after-free / mismatched
//! allocator the runtime helpers commit when driven through that
//! shape.
//!
//! These are *runtime-level* tests - they call the C-ABI
//! `gos_rt_*` helpers directly without going through the
//! compiled Gossamer binary, so they reproduce ownership-domain
//! bugs without needing the codegen pipeline. If a test segfaults
//! under ASAN, the report points at the offending free.

#![allow(clippy::missing_safety_doc, clippy::doc_markdown)]

use std::ffi::CString;

use gossamer_runtime::c_abi::{
    GosVec, gos_rt_json_as_array_opt, gos_rt_json_as_i64, gos_rt_json_get, gos_rt_json_get_opt,
    gos_rt_json_parse, gos_rt_result_disc, gos_rt_result_payload, gos_rt_vec_clone,
    gos_rt_vec_from_arr, gos_rt_vec_get_i64, gos_rt_vec_new, gos_rt_vec_push, gos_rt_vec_push_i64,
};

/// Repeatedly clone a Vec<i64> and push past `cap` so the realloc
/// inside `gos_rt_vec_push` reconstructs through
/// `Vec::from_raw_parts`. Pre-Stage-1 the cloned buffer was
/// arena-backed; reconstruction handed an arena pointer to
/// `Global::dealloc` and ASAN reported a mismatched allocator.
#[test]
fn vec_clone_then_grow_no_cross_domain_free() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        // Build a source Vec<i64> with 5 elements through the
        // standard from_arr path.
        let elems: [i64; 5] = [10, 20, 30, 40, 50];
        let src = gos_rt_vec_from_arr(8, elems.as_ptr().cast::<u8>(), 5);
        assert!(!src.is_null());

        for _ in 0..1000 {
            // Clone, then push past cap (5 → 6 forces realloc).
            let cloned = gos_rt_vec_clone(src);
            assert!(!cloned.is_null());
            // Verify contents match.
            for (i, expected) in elems.iter().enumerate() {
                assert_eq!(gos_rt_vec_get_i64(cloned, i as i64), *expected);
            }
            // Push 10 more elements; first push triggers realloc.
            for j in 0..10 {
                gos_rt_vec_push_i64(cloned, 100 + j);
            }
            // Verify the original survived unchanged.
            for (i, expected) in elems.iter().enumerate() {
                assert_eq!(gos_rt_vec_get_i64(src, i as i64), *expected);
            }
        }
    }
}

/// Drive `gos_rt_vec_push` through the typed-i64 entry point on a
/// Vec built via `gos_rt_vec_new(8)`. Confirms the empty-vec
/// growth path in `vec_push` doesn't trip the
/// `Vec::from_raw_parts` reclamation when the original buffer was
/// null (cap = 0).
#[test]
fn empty_vec_first_push_does_not_segfault() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let v = gos_rt_vec_new(8);
        for i in 0..1000 {
            gos_rt_vec_push_i64(v, i);
        }
        assert_eq!((*v).len, 1000);
        for i in 0..1000 {
            assert_eq!(gos_rt_vec_get_i64(v, i), i);
        }
    }
}

/// Walks the askq-shape json tree (delta → tool_calls → 0 →
/// function → name → as_str) repeatedly. Pre-Stage-2 each
/// `gos_rt_json_get` call deep-cloned the matched child; this
/// drill leaked a multi-KB clone per iteration. The current
/// `Arc<Value>`-shared form should make the stress drill flat in
/// memory.
#[test]
fn json_get_chain_is_arc_shared() {
    let blob = r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"list_files","arguments":"{\"path\":\"/tmp\"}"}}]}}]}"#;
    let blob_c = CString::new(blob).unwrap();
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        for _ in 0..2000 {
            let parse_res = gos_rt_json_parse(blob_c.as_ptr());
            assert_eq!(gos_rt_result_disc(parse_res), 0);
            let parsed =
                gos_rt_result_payload(parse_res) as *const gossamer_runtime::c_abi::GosJson;

            let key_choices = CString::new("choices").unwrap();
            let choices_v = gos_rt_json_get(parsed, key_choices.as_ptr());
            assert!(!choices_v.is_null());

            let arr_res = gos_rt_json_as_array_opt(choices_v);
            assert_eq!(gos_rt_result_disc(arr_res), 0);
            let arr_vec = gos_rt_result_payload(arr_res) as *mut GosVec;
            assert!(!arr_vec.is_null());
            assert_eq!((*arr_vec).len, 1);

            let chunk = gos_rt_vec_get_i64(arr_vec, 0) as *const gossamer_runtime::c_abi::GosJson;
            assert!(!chunk.is_null());

            let key_delta = CString::new("delta").unwrap();
            let delta_opt = gos_rt_json_get_opt(chunk, key_delta.as_ptr());
            assert_eq!(gos_rt_result_disc(delta_opt), 0);
            let delta = gos_rt_result_payload(delta_opt) as *const gossamer_runtime::c_abi::GosJson;

            let key_tc = CString::new("tool_calls").unwrap();
            let tcs_v = gos_rt_json_get(delta, key_tc.as_ptr());
            assert!(!tcs_v.is_null());

            let tcs_arr = gos_rt_json_as_array_opt(tcs_v);
            assert_eq!(gos_rt_result_disc(tcs_arr), 0);
            let tcs_vec = gos_rt_result_payload(tcs_arr) as *mut GosVec;
            let tc = gos_rt_vec_get_i64(tcs_vec, 0) as *const gossamer_runtime::c_abi::GosJson;

            let key_fn = CString::new("function").unwrap();
            let f = gos_rt_json_get(tc, key_fn.as_ptr());
            let key_name = CString::new("name").unwrap();
            let name_v = gos_rt_json_get(f, key_name.as_ptr());
            // For non-numeric values this returns 0; we just want to
            // confirm we can walk the tree without segfaulting.
            let _ = gos_rt_json_as_i64(name_v);
        }
    }
}

/// Reproduces the askq tc_args accumulator: build a Vec<String>
/// of c-strings, then for many iterations read the i-th element,
/// concatenate it with a chunk via `gos_rt_concat_*`, and store
/// the result back at the same index. After 200 such iterations,
/// each slot's c-string should still be readable end-to-end.
/// If alloc_cstring's Box<[u8]> reclamation is being silently
/// triggered (e.g. by an inadvertent Box::from_raw somewhere),
/// the read after re-write would segfault.
#[test]
fn vec_string_slot_rewrite_keeps_pointer_live() {
    use gossamer_runtime::c_abi::{
        gos_rt_concat_finish, gos_rt_concat_init, gos_rt_concat_str, gos_rt_str_len,
        gos_rt_vec_set_i64,
    };
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let v = gos_rt_vec_new(8);
        // Initial fill: 4 empty c-strings.
        let empty = CString::new("").unwrap();
        let empty_ptr = empty.as_ptr() as i64;
        for _ in 0..4 {
            gos_rt_vec_push(v, std::ptr::addr_of!(empty_ptr).cast::<u8>());
        }
        // Now the askq pattern: tc_args[idx] = format!("{}{}",
        // tc_args[idx], chunk_i)
        let chunk_template = "AAAAAAAAAA";
        for iter in 0..200 {
            for idx in 0..4 {
                let cur_ptr = gos_rt_vec_get_i64(v, idx) as *const std::os::raw::c_char;
                gos_rt_concat_init();
                gos_rt_concat_str(cur_ptr);
                let chunk = CString::new(format!("{chunk_template}{iter}")).unwrap();
                gos_rt_concat_str(chunk.as_ptr());
                let new_ptr = gos_rt_concat_finish();
                gos_rt_vec_set_i64(v, idx, new_ptr as i64);
            }
        }
        // Verify each slot has a non-zero strlen and is readable.
        for idx in 0..4 {
            let p = gos_rt_vec_get_i64(v, idx) as *const std::os::raw::c_char;
            assert!(!p.is_null());
            let len = gos_rt_str_len(p);
            assert!(len > 0, "slot {idx} length was {len}");
        }
    }
}

/// Exercises a `Vec<String>` shape: push N c-string pointers
/// (each from a fresh `Box<[u8]>`-backed alloc_cstring), then
/// trigger growth past cap. Closer-to-askq because the string
/// payloads in the chat-round accumulator are c-strings the GosVec
/// data buffer points at.
#[test]
fn vec_of_cstring_push_grow_drill() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let v = gos_rt_vec_new(8);
        let mut leaked: Vec<*mut std::os::raw::c_char> = Vec::with_capacity(500);
        for i in 0..500 {
            let s = CString::new(format!("entry-{i}")).unwrap();
            let bytes = s.as_bytes_with_nul();
            // Caller-side: allocate a fresh Box<[u8]> for the
            // string payload, push the pointer into the GosVec.
            let mut buf: Vec<u8> = bytes.to_vec();
            let ptr = buf.as_mut_ptr();
            std::mem::forget(buf);
            leaked.push(ptr.cast());
            let ptr_val = ptr as i64;
            gos_rt_vec_push(v, std::ptr::addr_of!(ptr_val).cast::<u8>());
        }
        assert_eq!((*v).len, 500);
        // Spot-check the first and last.
        let first = gos_rt_vec_get_i64(v, 0) as *const std::os::raw::c_char;
        let last = gos_rt_vec_get_i64(v, 499) as *const std::os::raw::c_char;
        assert!(!first.is_null() && !last.is_null());
    }
}
