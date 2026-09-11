#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

//! `std::encoding::csv` C-ABI shims. Mirrors
//! `gossamer_std::encoding::csv` exactly. CSV records cross the ABI
//! as `Vec<Vec<String>>` - an outer `GosVec` of inner `GosVec`
//! pointers, each inner holding c-string pointers.

use std::os::raw::c_char;

use super::string::alloc_cstring;
use super::vec::{GosVec, gos_rt_result_new, gos_rt_vec_push};

/// Reads a `GosVec<String>` (elements are c-string pointers) into
/// owned strings.
unsafe fn read_str_vec(v: *const GosVec) -> Vec<String> {
    if v.is_null() {
        return Vec::new();
    }
    let vref = unsafe { &*v };
    if vref.ptr.is_null() || vref.len <= 0 {
        return Vec::new();
    }
    let len = vref.len as usize;
    let words = unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr().cast::<i64>(), len) };
    words
        .iter()
        .map(|&w| {
            let p = w as *const c_char;
            if p.is_null() {
                String::new()
            } else {
                unsafe { crate::c_abi::gos_str_arg_string(p) }
            }
        })
        .collect()
}

/// Builds a `GosVec<String>` from owned strings. STRING-typed: the
/// vec owns each element, so `gos_rt_vec_free` deep-frees them.
fn build_str_vec(parts: &[String]) -> *mut GosVec {
    let vec = unsafe {
        crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
            8,
            parts.len() as i64,
            crate::c_abi::vec::vec_elem_kind::STRING,
        )
    };
    for p in parts {
        let pv = alloc_cstring(p.as_bytes()) as i64;
        unsafe { gos_rt_vec_push(vec, std::ptr::addr_of!(pv).cast::<u8>()) };
    }
    vec
}

/// Hands each of `line`'s fields to `emit` as the bytes it holds.
///
/// A field carrying no quote is a run of the line itself and reaches `emit`
/// as that run, so the common record costs no copy at all. Only a quoted
/// field is assembled, into `scratch`, which the caller reuses across the
/// whole document.
///
/// The separator and the quote are ASCII, so a byte can only be one of them
/// where a character is, and the run between two of them is a whole number
/// of characters.
fn for_each_field(line: &str, scratch: &mut String, mut emit: impl FnMut(&[u8])) {
    let bytes = line.as_bytes();
    let mut assembling = false;
    let mut in_quotes = false;
    let mut run_start = 0usize;
    let mut i = 0usize;
    scratch.clear();
    while i < bytes.len() {
        match bytes[i] {
            b'"' if in_quotes => {
                scratch.push_str(&line[run_start..i]);
                assembling = true;
                if bytes.get(i + 1) == Some(&b'"') {
                    scratch.push('"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
                run_start = i;
            }
            b'"' => {
                scratch.push_str(&line[run_start..i]);
                assembling = true;
                in_quotes = true;
                i += 1;
                run_start = i;
            }
            b',' if !in_quotes => {
                if assembling {
                    scratch.push_str(&line[run_start..i]);
                    emit(scratch.as_bytes());
                    scratch.clear();
                    assembling = false;
                } else {
                    emit(&bytes[run_start..i]);
                }
                i += 1;
                run_start = i;
            }
            _ => i += 1,
        }
    }
    if assembling {
        scratch.push_str(&line[run_start..]);
        emit(scratch.as_bytes());
    } else {
        emit(&bytes[run_start..]);
    }
}

fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut scratch = String::new();
    for_each_field(line, &mut scratch, |f| {
        fields.push(String::from_utf8_lossy(f).into_owned());
    });
    fields
}

/// Builds the `GosVec<String>` one record's fields fill, each string
/// allocated once at its final length. STRING-typed: the vec owns each
/// element, so `gos_rt_vec_free` deep-frees them.
fn build_record(line: &str, scratch: &mut String) -> *mut GosVec {
    let vec = unsafe {
        crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
    };
    for_each_field(line, scratch, |field| {
        let pv = alloc_cstring(field) as i64;
        unsafe { gos_rt_vec_push(vec, std::ptr::addr_of!(pv).cast::<u8>()) };
    });
    vec
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

/// `encoding::csv::parse_line(line) -> [String]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_parse_line(line: *const c_char) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        build_str_vec(&parse_line(unsafe { cstr(line) }))
    })
}

/// `encoding::csv::read(input) -> Result<[[String]], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_read(input: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let input = unsafe { cstr(input) };
        // Build the outer GosVec of inner GosVec<String> pointers.
        // VEC-typed: the outer vec owns each row, so `gos_rt_vec_free`
        // cascades through unvisited rows (the early-`break` path)
        // instead of leaking them and their field strings.
        let outer = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC)
        };
        let mut scratch = String::new();
        for line in input.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // A quote is ASCII and cannot occur inside a multi-byte
            // character, so the bytes answer the same count without decoding
            // the line.
            let quote_count = line.bytes().filter(|&b| b == b'"').count();
            if quote_count % 2 != 0 {
                let msg = format!("csv: unterminated quoted field in: {line}");
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
                return unsafe { gos_rt_result_new(1, err as i64) };
            }
            let inner = build_record(line, &mut scratch) as i64;
            unsafe { gos_rt_vec_push(outer, std::ptr::addr_of!(inner).cast::<u8>()) };
        }
        unsafe { gos_rt_result_new(0, outer as i64) }
    })
}

/// `encoding::csv::write(records) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_write(records: *const GosVec) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let rows: Vec<Vec<String>> = if records.is_null() {
            Vec::new()
        } else {
            let vref = unsafe { &*records };
            if vref.ptr.is_null() || vref.len <= 0 {
                Vec::new()
            } else {
                let len = vref.len as usize;
                let words =
                    unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr().cast::<i64>(), len) };
                words
                    .iter()
                    .map(|&w| unsafe { read_str_vec(w as *const GosVec) })
                    .collect()
            }
        };
        let mut out = String::new();
        for (i, record) in rows.iter().enumerate() {
            for (j, field) in record.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                if field.contains(',') || field.contains('"') || field.contains('\n') {
                    out.push('"');
                    out.push_str(&field.replace('"', "\"\""));
                    out.push('"');
                } else {
                    out.push_str(field);
                }
            }
            if i + 1 < rows.len() {
                out.push('\n');
            }
        }
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
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] })
    }

    #[test]
    fn csv_read_outer_vec_is_vec_typed_and_deep_frees_unvisited_rows() {
        let input = std::ffi::CString::new("a,b\nc,d\ne,f").unwrap();
        let r = unsafe { gos_rt_csv_read(crate::c_abi::string::test_gos_ptr(&input)) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(r), 0);
        let outer = crate::c_abi::vec::gos_rt_result_payload(r) as *mut GosVec;
        assert!(!outer.is_null());
        let o = unsafe { &*outer };
        assert_eq!(o.len, 3);
        assert_eq!(o.elem_kind, crate::c_abi::vec::vec_elem_kind::VEC);
        // Probe-share row 1's first field, then free the outer WITHOUT
        // iterating (the ABI shape of `for row in rows { break }`): the
        // cascade must release exactly one share - rc 2 -> 1, not 2
        // (leak) and not 0 (double free).
        // Slots hold child pointers exposed as i64 by the flat-slot ABI;
        // read the address and recover its provenance.
        let row1: *mut GosVec = std::ptr::with_exposed_provenance_mut(unsafe {
            (o.ptr.add(8) as *const usize).read_unaligned()
        });
        let field: *mut c_char = std::ptr::with_exposed_provenance_mut(unsafe {
            ((*row1).ptr.as_ptr() as *const usize).read_unaligned()
        });
        unsafe { crate::c_abi::string::gos_rt_str_retain(field) };
        assert_eq!(unsafe { str_rc(field) }, 2);
        unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
        assert_eq!(
            unsafe { str_rc(field) },
            1,
            "outer free must cascade exactly once"
        );
        assert_eq!(unsafe { CStr::from_ptr(field) }.to_str().unwrap(), "c");
        unsafe { crate::c_abi::string::gos_rt_str_free(field) };
    }

    #[test]
    fn csv_read_borrow_all_rows_then_free_is_balanced() {
        let input = std::ffi::CString::new("x,y\nz,w").unwrap();
        let r = unsafe { gos_rt_csv_read(crate::c_abi::string::test_gos_ptr(&input)) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(r), 0);
        let outer = crate::c_abi::vec::gos_rt_result_payload(r) as *mut GosVec;
        let o = unsafe { &*outer };
        // Full-iteration consumer shape: every read is an interior
        // borrow (the drop pass never releases container loads), so a
        // single outer free afterwards is the only release.
        let mut fields = Vec::new();
        for i in 0..o.len as usize {
            // Slots hold child pointers exposed as i64 by the flat-slot
            // ABI; read the address and recover its provenance so the
            // borrow is sound under strict provenance.
            let raw = unsafe { (o.ptr.add(i * 8) as *const usize).read_unaligned() };
            let row: *mut GosVec = std::ptr::with_exposed_provenance_mut(raw);
            let rv = unsafe { &*row };
            for j in 0..rv.len as usize {
                let raw = unsafe { (rv.ptr.add(j * 8) as *const usize).read_unaligned() };
                let f: *mut c_char = std::ptr::with_exposed_provenance_mut(raw);
                fields.push(unsafe { CStr::from_ptr(f) }.to_str().unwrap().to_string());
            }
        }
        assert_eq!(fields, ["x", "y", "z", "w"]);
        unsafe { crate::c_abi::map::gos_rt_vec_free(outer) };
    }
}

#[cfg(test)]
mod parse_line_tests {
    use super::parse_line;

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
