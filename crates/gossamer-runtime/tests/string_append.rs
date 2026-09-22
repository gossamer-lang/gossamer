//! String append runtime tests.

use std::ffi::{CStr, CString};

use gossamer_runtime::c_abi::{
    alloc_cstring, gos_rt_arena_pop, gos_rt_arena_push, gos_rt_str_append_bytes,
    gos_rt_str_append_i64, gos_rt_str_char_at, gos_rt_str_clear, gos_rt_str_concat_drop_a,
    gos_rt_str_free, gos_rt_str_push_byte, gos_rt_str_push_char, gos_rt_str_retain_typed,
    gos_rt_str_truncate, gos_rt_str_with_capacity,
};

#[test]
fn append_i64_formats_into_existing_builder() {
    let prefix = CString::new("id=").expect("literal has no nul");
    unsafe {
        let first = gos_rt_str_append_i64(prefix.as_ptr(), -42);
        let second = gos_rt_str_append_i64(first, 17);
        let out = CStr::from_ptr(second).to_str().unwrap().to_owned();
        gos_rt_str_free(second);
        assert_eq!(out, "id=-4217");
    }
}

#[test]
fn growable_string_survives_the_arena_that_created_it() {
    unsafe {
        gos_rt_arena_push();
        let arena_string = gos_rt_str_with_capacity(64);
        // Growable strings still use the arena fast path when they have no
        // arena-backed source. The ABI tag lives immediately before content.
        assert_eq!(*arena_string.cast::<u8>().sub(1), 0xAA);
        let promoted = gos_rt_str_append_bytes(arena_string, b"escaped".as_ptr(), 7);
        // Copying arena storage promotes the result before the slab can be
        // recycled over its source bytes.
        assert_eq!(*promoted.cast::<u8>().sub(1), 0xAB);
        gos_rt_arena_pop();

        assert_eq!(CStr::from_ptr(promoted).to_bytes(), b"escaped");
        gos_rt_str_free(promoted);
    }
}

#[test]
fn concat_drop_a_appends_header_backed_fragment() {
    let prefix = CString::new("name=").expect("literal has no nul");
    unsafe {
        let fragment = alloc_cstring(b"user-000042");
        let out_ptr = gos_rt_str_concat_drop_a(prefix.as_ptr(), fragment);
        let out = CStr::from_ptr(out_ptr).to_str().unwrap().to_owned();
        gos_rt_str_free(fragment);
        gos_rt_str_free(out_ptr);
        assert_eq!(out, "name=user-000042");
    }
}

#[test]
fn character_pushes_reuse_unique_reserved_storage() {
    unsafe {
        let string = gos_rt_str_with_capacity(16);
        let after_char = gos_rt_str_push_char(string, 'a' as i32);
        assert_eq!(after_char, string, "reserved push should stay in place");
        let after_unicode = gos_rt_str_push_char(after_char, 'ç' as i32);
        assert_eq!(
            after_unicode, string,
            "multibyte push should reuse remaining capacity"
        );
        let after_byte = gos_rt_str_push_byte(after_unicode, i32::from(b'!'));
        assert_eq!(after_byte, string, "byte push should stay in place");
        assert_eq!(CStr::from_ptr(after_byte).to_bytes(), "aç!".as_bytes());
        gos_rt_str_free(after_byte);
    }
}

#[test]
fn character_push_grows_an_exhausted_buffer() {
    unsafe {
        let string = alloc_cstring(b"full");
        let grown = gos_rt_str_push_char(string, '!' as i32);
        assert_ne!(grown, string, "an exhausted buffer must be replaced");
        assert_eq!(CStr::from_ptr(grown).to_bytes(), b"full!");
        gos_rt_str_free(grown);
    }
}

#[test]
fn incremental_append_index_preserves_unicode_lookup() {
    unsafe {
        let mut string = gos_rt_str_with_capacity(512);
        for _ in 0..100 {
            string = gos_rt_str_append_bytes(string, "aç".as_ptr(), 3);
        }
        assert_eq!(gos_rt_str_char_at(string, 199), 'ç' as i64);
        gos_rt_str_free(string);
    }
}

#[test]
fn clearing_a_unique_builder_keeps_its_storage_for_the_next_append() {
    unsafe {
        let string = gos_rt_str_with_capacity(32);
        let filled = gos_rt_str_append_bytes(string, b"first row".as_ptr(), 9);
        assert_eq!(filled, string);
        let cleared = gos_rt_str_clear(filled);
        assert_eq!(cleared, string, "a unique builder is cleared in place");
        assert_eq!(CStr::from_ptr(cleared).to_bytes(), b"");
        let refilled = gos_rt_str_append_bytes(cleared, b"second".as_ptr(), 6);
        assert_eq!(refilled, string, "the cleared capacity is reused");
        assert_eq!(CStr::from_ptr(refilled).to_bytes(), b"second");
        gos_rt_str_free(refilled);
    }
}

#[test]
fn clearing_a_shared_builder_leaves_the_other_holder_its_text() {
    unsafe {
        let string = gos_rt_str_with_capacity(32);
        let filled = gos_rt_str_append_bytes(string, b"kept".as_ptr(), 4);
        gos_rt_str_retain_typed(filled);
        let cleared = gos_rt_str_clear(filled);
        assert_ne!(cleared, filled, "a shared builder is not rewritten");
        assert_eq!(CStr::from_ptr(cleared).to_bytes(), b"");
        assert_eq!(CStr::from_ptr(filled).to_bytes(), b"kept");
        gos_rt_str_free(cleared);
        gos_rt_str_free(filled);
    }
}

#[test]
fn truncating_a_unique_builder_shortens_it_at_a_character_boundary() {
    unsafe {
        let string = gos_rt_str_with_capacity(32);
        let text = "a\u{e7}\u{e7}b";
        let filled = gos_rt_str_append_bytes(string, text.as_ptr(), text.len() as i64);
        // Byte 4 falls inside the second `ç`, so the cut lands before it.
        let cut = gos_rt_str_truncate(filled, 4);
        assert_eq!(cut, string, "a unique builder is truncated in place");
        assert_eq!(CStr::from_ptr(cut).to_bytes(), "a\u{e7}".as_bytes());
        assert_eq!(gos_rt_str_char_at(cut, 1), i64::from(u32::from('ç')));
        gos_rt_str_free(cut);
    }
}
