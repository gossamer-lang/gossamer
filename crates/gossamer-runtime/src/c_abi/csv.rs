#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

//! `std::encoding::csv` C-ABI shims over `crate::codec::csv`, the
//! implementation every tier shares. CSV records cross the ABI
//! as `Vec<Vec<String>>` - an outer `GosVec` of inner `GosVec`
//! pointers, each inner holding c-string pointers.

use std::os::raw::c_char;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;
use super::vec::{GosVec, gos_rt_vec_push};
use crate::codec::csv;

/// Reads a `GosVec<String>` (elements are c-string pointers) into
/// owned strings.
///
/// # Safety
/// `v` is null or a live `Vec<String>`.
unsafe fn read_str_vec(v: *const GosVec) -> Vec<String> {
    // SAFETY: this `unsafe fn`'s caller passes `v` null or a live `Vec<String>`.
    unsafe { crate::c_abi::vec::StrVecView::of(v) }
        .map_or_else(Vec::new, |row| row.texts().collect())
}

/// Builds a `GosVec<String>` from owned strings. STRING-typed: the
/// vec owns each element, so `gos_rt_vec_free` deep-frees them.
fn build_str_vec(parts: &[String]) -> *mut GosVec {
    let vec = {
        crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
            8,
            parts.len() as i64,
            crate::c_abi::vec::vec_elem_kind::STRING,
        )
    };
    for p in parts {
        let pv = alloc_cstring(p.as_bytes()) as i64;
        // SAFETY: `vec` is the fresh vec made above, or null, which `gos_rt_vec_push` accepts,
        // and `pv` is one 8-byte element.
        unsafe { gos_rt_vec_push(vec, std::ptr::addr_of!(pv).cast::<u8>()) };
    }
    vec
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `gos_str_arg_text`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

/// `encoding::csv::parse_line(line) -> [String]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_parse_line(line: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `line` is this shim's argument, null or a live string body for the call (C-ABI
        // contract), which `cstr` accepts.
        build_str_vec(&csv::parse_line(unsafe { cstr(line) }))
    })
}

/// `encoding::csv::read(input) -> Result<[[String]], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_read(input: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `input` is this shim's argument, null or a live string body for the call (C-ABI
        // contract), which `cstr` accepts.
        let input = unsafe { cstr(input) };
        // Build the outer GosVec of inner GosVec<String> pointers.
        // VEC-typed: the outer vec owns each row, so `gos_rt_vec_free`
        // cascades through unvisited rows (the early-`break` path)
        // instead of leaking them and their field strings.
        let outer =
            { crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC) };
        let mut scratch = String::new();
        let mut row =
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING);
        let walked = csv::for_each_record(input, &mut scratch, |event| match event {
            csv::Event::Field(field) => {
                let pv = alloc_cstring(field) as i64;
                // SAFETY: `row` is a fresh vec owned here, or null, which `gos_rt_vec_push`
                // accepts, and `pv` is one 8-byte element.
                unsafe { gos_rt_vec_push(row, std::ptr::addr_of!(pv).cast::<u8>()) };
            }
            csv::Event::EndRecord => {
                let inner = row as i64;
                // SAFETY: `outer` is the fresh vec made above, or null, which `gos_rt_vec_push`
                // accepts, and `inner` is one 8-byte element whose share moves into it.
                unsafe { gos_rt_vec_push(outer, std::ptr::addr_of!(inner).cast::<u8>()) };
                row = crate::c_abi::vec::gos_rt_vec_new_typed(
                    8,
                    crate::c_abi::vec::vec_elem_kind::STRING,
                );
            }
        });
        // SAFETY: `row` is the fresh, unpushed vec the walk left, owned here alone.
        unsafe { crate::c_abi::map::gos_rt_vec_free(row) };
        if let Err(msg) = walked {
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            // SAFETY: `outer` is the fresh vec made above, owned here alone and not read again.
            unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
            return gos_rt_result_new(1, err as i64);
        }
        gos_rt_result_new(0, outer as i64)
    })
}

/// `encoding::csv::write(records) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_write(records: *const GosVec) -> *mut c_char {
    ffi_entry!({
        let rows: Vec<Vec<String>> = if records.is_null() {
            Vec::new()
        } else {
            // SAFETY: `records` is non-null (checked above) and live for the call (C-ABI
            // contract).
            let vref = unsafe { &*records };
            if vref.ptr.is_null() || vref.len <= 0 {
                Vec::new()
            } else {
                let len = vref.len as usize;
                let words =
                    // SAFETY: the buffer is non-null (checked above) and holds `len` words, one
                    // per `Vec<Vec<String>>` element.
                    unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr().cast::<i64>(), len) };
                words
                    .iter()
                    // SAFETY: each element of a `Vec<Vec<String>>` is null or a live
                    // `Vec<String>` (C-ABI contract), which `read_str_vec` accepts.
                    .map(|&w| unsafe { read_str_vec(w as *const GosVec) })
                    .collect()
            }
        };
        let out = csv::write(&rows);
        alloc_cstring(out.as_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Refcount word of an `alloc_cstring` builder-layout string:
    /// `[rc:u32][cap:u32][len:u32][tag][content][NUL]`, body at +13.
    unsafe fn str_rc(s: *const c_char) -> u32 {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] })
    }

    #[test]
    fn csv_read_outer_vec_is_vec_typed_and_deep_frees_unvisited_rows() {
        let input = crate::c_abi::string::test_gos_str("a,b\nc,d\ne,f");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let r = unsafe { gos_rt_csv_read(input) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(r), 0);
        let outer = crate::c_abi::result::gos_rt_result_payload(r) as *mut GosVec;
        assert!(!outer.is_null());
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let o = unsafe { &*outer };
        assert_eq!(o.len, 3);
        assert_eq!(o.elem_kind, crate::c_abi::vec::vec_elem_kind::VEC);
        // Probe-share row 1's first field, then free the outer WITHOUT
        // iterating (the ABI shape of `for row in rows { break }`): the
        // cascade must release exactly one share - rc 2 -> 1, not 2
        // (leak) and not 0 (double free).
        // Slots hold child pointers exposed as i64 by the flat-slot ABI;
        // read the address and recover its provenance.
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let row1: *mut GosVec = std::ptr::with_exposed_provenance_mut(unsafe {
            crate::c_abi::vec::slot_read_word(o.ptr.add(8)).expose_provenance()
        });
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let field: *mut c_char = std::ptr::with_exposed_provenance_mut(unsafe {
            crate::c_abi::vec::slot_read_word((*row1).ptr.as_ptr()).expose_provenance()
        });
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_retain(field) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { str_rc(field) }, 2);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
        assert_eq!(
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe { str_rc(field) },
            1,
            "outer free must cascade exactly once"
        );
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { CStr::from_ptr(field) }.to_str().unwrap(), "c");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_free(field) };
    }

    #[test]
    fn csv_read_borrow_all_rows_then_free_is_balanced() {
        let input = crate::c_abi::string::test_gos_str("x,y\nz,w");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let r = unsafe { gos_rt_csv_read(input) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(r), 0);
        let outer = crate::c_abi::result::gos_rt_result_payload(r) as *mut GosVec;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let o = unsafe { &*outer };
        // Full-iteration consumer shape: every read is an interior
        // borrow (the drop pass never releases container loads), so a
        // single outer free afterwards is the only release.
        let mut fields = Vec::new();
        for i in 0..o.len as usize {
            // Slots hold child pointers exposed as i64 by the flat-slot
            // ABI; read the address and recover its provenance so the
            // borrow is sound under strict provenance.
            let raw =
                // SAFETY: every pointer argument is a value this test built above and still holds
                // live; a null one is accepted by the callee.
                unsafe { crate::c_abi::vec::slot_read_word(o.ptr.add(i * 8)).expose_provenance() };
            let row: *mut GosVec = std::ptr::with_exposed_provenance_mut(raw);
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            let rv = unsafe { &*row };
            for j in 0..rv.len as usize {
                // SAFETY: every pointer argument is a value this test built above and still holds
                // live; a null one is accepted by the callee.
                let raw = unsafe {
                    crate::c_abi::vec::slot_read_word(rv.ptr.add(j * 8)).expose_provenance()
                };
                let f: *mut c_char = std::ptr::with_exposed_provenance_mut(raw);
                // SAFETY: every pointer argument is a value this test built above and still holds
                // live; a null one is accepted by the callee.
                fields.push(unsafe { CStr::from_ptr(f) }.to_str().unwrap().to_string());
            }
        }
        assert_eq!(fields, ["x", "y", "z", "w"]);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
    }
}

#[cfg(test)]
mod parse_line_tests {
    use crate::codec::csv::parse_line;

    /// A field is the text between separators, whatever it holds.
    #[test]
    fn parse_line_splits_on_unquoted_separators() {
        assert_eq!(parse_line("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(parse_line(""), vec![""]);
        assert_eq!(parse_line("a,,c"), vec!["a", "", "c"]);
        assert_eq!(parse_line(",a,"), vec!["", "a", ""]);
    }

    /// A quoted field keeps its separators, and a doubled quote is one quote.
    #[test]
    fn parse_line_reads_quoted_fields() {
        assert_eq!(parse_line(r#""a,b",c"#), vec!["a,b", "c"]);
        assert_eq!(parse_line(r#""say ""hi""",x"#), vec![r#"say "hi""#, "x"]);
        assert_eq!(parse_line(r#"a,"",b"#), vec!["a", "", "b"]);
        assert_eq!(parse_line(r#"pre"mid"post"#), vec!["premidpost"]);
    }

    /// A run copied whole spans whole characters, so multi-byte text survives.
    #[test]
    fn parse_line_keeps_multibyte_text() {
        assert_eq!(parse_line("café,naïve"), vec!["café", "naïve"]);
        assert_eq!(parse_line(r#""café,x",é"#), vec!["café,x", "é"]);
        assert_eq!(parse_line("日本,語"), vec!["日本", "語"]);
    }
}
