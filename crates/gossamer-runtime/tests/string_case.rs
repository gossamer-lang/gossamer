//! String case-conversion runtime tests.

use std::ffi::CStr;

use gossamer_runtime::c_abi::{
    gos_rt_str_append_bytes, gos_rt_str_free, gos_rt_str_len, gos_rt_str_to_upper,
    gos_rt_str_with_capacity,
};

fn upper(input: &str) -> String {
    let input = gossamer_runtime::c_abi::string::alloc_cstring(input.as_bytes());
    // SAFETY: `input` is a live runtime string for the call, and both it and the answer are
    // freed once with `gos_rt_str_free`.
    unsafe {
        let raw = gos_rt_str_to_upper(input);
        let out = CStr::from_ptr(raw).to_str().unwrap().to_owned();
        gos_rt_str_free(raw);
        gos_rt_str_free(input);
        out
    }
}

#[test]
fn string_with_capacity_reuses_its_unique_buffer() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    unsafe {
        let string = gos_rt_str_with_capacity(128);
        let original = string;
        let appended = gos_rt_str_append_bytes(string, b"hello".as_ptr(), 5);
        assert_eq!(appended, original, "reserved buffer should grow in place");
        assert_eq!(gos_rt_str_len(appended), 5);
        gos_rt_str_free(appended);
    }
}

#[test]
fn str_to_upper_ascii_fast_path_matches_unicode_contract() {
    assert_eq!(upper("json-tag_42"), "JSON-TAG_42");
}

#[test]
fn str_to_upper_unicode_fallback_preserves_expansion() {
    assert_eq!(upper("straße"), "STRASSE");
}

#[test]
fn foreign_cstring_is_never_prefix_probed_by_public_string_helpers() {
    // Miri rejects the old `ptr[-1]` provenance probe because `foreign` owns
    // exactly its C-string bytes. The public ABI must treat it as borrowed:
    // length can use `strlen`, while free is a no-op.
    // A host allocation is at least 8-byte aligned, which is the shape of the
    // foreign strings the runtime receives; word-sized storage gives this one
    // the same alignment.
    let storage = [u64::from_ne_bytes(*b"borrowed"), 0];
    let foreign = storage.as_ptr().cast::<std::ffi::c_char>();
    // SAFETY: `foreign` is a live NUL-terminated host string for both calls.
    unsafe {
        assert_eq!(gos_rt_str_len(foreign), 8);
        gos_rt_str_free(foreign.cast_mut());
    }
    // SAFETY: `foreign` still addresses the NUL-terminated bytes written above.
    let text = unsafe { CStr::from_ptr(foreign) };
    assert_eq!(text.to_str().unwrap(), "borrowed");
}

#[test]
fn foreign_heap_bytes_shaped_like_a_body_are_never_treated_as_a_runtime_string() {
    // A pointer five bytes into a heap allocation has the low-bit shape of a
    // runtime string body, and the bytes before it are readable heap memory
    // rather than an owner. The untyped free must leave it alone: no count is
    // touched and nothing is freed. (The `_typed` entry points and the typed
    // readers admit only runtime strings and 8-byte-aligned host buffers, so
    // they are not part of this.)
    let mut block: Box<[u8]> = Box::new([0u8; 64]);
    block[5..14].copy_from_slice(b"borrowed\0");
    // SAFETY: offset 5 lies inside the 64-byte block, and the pointer keeps the whole block's
    // provenance.
    let body = unsafe { block.as_mut_ptr().add(5) }.cast::<std::ffi::c_char>();
    // SAFETY: `body` addresses a live NUL-terminated buffer inside `block`.
    unsafe { gos_rt_str_free(body) };
    assert_eq!(&block[5..13], b"borrowed");
    assert!(block[..5].iter().all(|&b| b == 0));
}
