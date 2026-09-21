//! A `BTreeMap` whose key type writes its own `cmp` orders by that
//! comparator, reached through the address the compiled program hands the
//! runtime.

use std::ffi::CString;

use gossamer_runtime::c_abi::{
    GosVec, gos_rt_map_insert_skey, gos_rt_map_keys_skey, gos_rt_map_new,
    gos_rt_map_set_ordered_by, gos_rt_vec_free, gos_rt_vec_len,
};

/// Descending by the one `i64` field, the order `impl Ord` would write.
unsafe extern "C" fn descending(a: *const u8, b: *const u8) -> i64 {
    // SAFETY: each key is the one-slot aggregate the test stores.
    let read = |p: *const u8| -> i64 {
        let mut bytes = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping(p, bytes.as_mut_ptr(), 8) };
        i64::from_le_bytes(bytes)
    };
    read(b) - read(a)
}

#[test]
fn a_user_comparator_decides_the_key_order() {
    let desc = CString::new("s").expect("descriptor has no NUL");
    // SAFETY: every pointer below is a live map, key, or descriptor.
    unsafe {
        let map = gos_rt_map_new(8, 8);
        gos_rt_map_set_ordered_by(map, descending as *const () as usize as i64, 1, 0);
        for key in [1i64, 3, 2] {
            let slots = key.to_le_bytes();
            gos_rt_map_insert_skey(map, slots.as_ptr(), desc.as_ptr(), key * 10);
        }
        let keys: *mut GosVec = gos_rt_map_keys_skey(map);
        assert_eq!(gos_rt_vec_len(keys), 3);
        let read = |index: usize| -> i64 {
            let base = (*keys).ptr.as_ptr().add(index * 8);
            let mut bytes = [0u8; 8];
            std::ptr::copy_nonoverlapping(base, bytes.as_mut_ptr(), 8);
            i64::from_le_bytes(bytes)
        };
        assert_eq!([read(0), read(1), read(2)], [3, 2, 1]);
        gos_rt_vec_free(keys);
        gossamer_runtime::c_abi::gos_rt_map_free(map);
    }
}
