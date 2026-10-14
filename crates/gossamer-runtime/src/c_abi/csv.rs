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
        let new_row = |fields: i64| {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                8,
                fields,
                crate::c_abi::vec::vec_elem_kind::STRING,
            )
        };
        // Records of one document mostly have one width, so each row is
        // sized by the one before it.
        let mut fields = 0i64;
        let mut row = new_row(fields);
        let walked = csv::for_each_record(input, &mut scratch, |event| match event {
            csv::Event::Field(field) => {
                let pv = alloc_cstring(field) as i64;
                // SAFETY: `row` is a fresh vec owned here, or null, which `gos_rt_vec_push`
                // accepts, and `pv` is one 8-byte element.
                unsafe { gos_rt_vec_push(row, std::ptr::addr_of!(pv).cast::<u8>()) };
            }
            csv::Event::EndRecord => {
                if !row.is_null() {
                    // SAFETY: `row` is the live, non-null vec this walk filled.
                    fields = unsafe { (*row).len };
                }
                let inner = row as i64;
                // SAFETY: `outer` is the fresh vec made above, or null, which `gos_rt_vec_push`
                // accepts, and `inner` is one 8-byte element whose share moves into it.
                unsafe { gos_rt_vec_push(outer, std::ptr::addr_of!(inner).cast::<u8>()) };
                row = new_row(fields);
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

/// The records of a CSV document as offsets into one copy of its text, for a
/// program that reads `csv::read`'s answer only by indexing it.
///
/// A field with no quote is a span of the text; a quoted one is unescaped
/// once into `unquoted`. A field becomes a `String` only when the program
/// reads one as text, and then only for the length of the call reading it.
pub struct CsvTable {
    text: Box<[u8]>,
    unquoted: Vec<u8>,
    /// Each field's span: in `text`, or in `unquoted` when the flag is set.
    fields: Vec<(u32, u32, bool)>,
    /// Index of each record's first field, then one past the last field.
    records: Vec<u32>,
    /// The field string the last text read answered, freed by the next.
    current: *mut c_char,
}

impl CsvTable {
    fn read(input: &str) -> Result<Self, String> {
        let base = input.as_ptr() as usize;
        let mut table = Self {
            text: input.as_bytes().into(),
            unquoted: Vec::new(),
            fields: Vec::new(),
            records: vec![0],
            current: std::ptr::null_mut(),
        };
        let mut scratch = String::new();
        csv::for_each_record(input, &mut scratch, |event| match event {
            csv::Event::Field(field) => {
                let at = (field.as_ptr() as usize).wrapping_sub(base);
                // An unquoted field is a run of the input itself; a quoted
                // one was assembled in the walk's scratch buffer.
                let span = if at <= input.len() && at + field.len() <= input.len() {
                    (at as u32, (at + field.len()) as u32, false)
                } else {
                    let start = table.unquoted.len();
                    table.unquoted.extend_from_slice(field);
                    (start as u32, table.unquoted.len() as u32, true)
                };
                table.fields.push(span);
            }
            csv::Event::EndRecord => table.records.push(table.fields.len() as u32),
        })?;
        Ok(table)
    }

    fn len(&self) -> usize {
        self.records.len() - 1
    }

    /// The field spans of record `record`, which the caller has checked.
    fn record(&self, record: usize) -> &[(u32, u32, bool)] {
        &self.fields[self.records[record] as usize..self.records[record + 1] as usize]
    }

    /// The text of field `field` of record `record`, panicking as indexing
    /// the rows `csv::read` answers would.
    fn field(&self, record: i64, field: i64) -> &str {
        let records = self.len() as i64;
        if record < 0 || record >= records {
            crate::c_abi::panic::panic_oob_text("vec index", record, records);
        }
        let spans = self.record(record as usize);
        let count = spans.len() as i64;
        if field < 0 || field >= count {
            crate::c_abi::panic::panic_oob_text("vec index", field, count);
        }
        let (start, end, unquoted) = spans[field as usize];
        let bytes = if unquoted {
            &self.unquoted[start as usize..end as usize]
        } else {
            &self.text[start as usize..end as usize]
        };
        // A field ends only at an ASCII separator or quote of UTF-8 text, so
        // its bytes are UTF-8.
        std::str::from_utf8(bytes).expect("a CSV field of UTF-8 text is UTF-8")
    }
}

impl Drop for CsvTable {
    fn drop(&mut self) {
        // SAFETY: `current` is null or the string this table made and still owns.
        unsafe { crate::c_abi::string::gos_rt_str_free(self.current) };
    }
}

/// The table a handle names.
///
/// # Safety
/// `t` is a live handle from [`gos_rt_csv_table`] or
/// [`gos_rt_csv_table_unwrap_or_empty`].
unsafe fn table<'a>(t: i64) -> &'a mut CsvTable {
    // SAFETY: this `unsafe fn`'s caller passes a live table handle.
    unsafe { &mut *(t as usize as *mut CsvTable) }
}

/// `csv::read(input)` for a program that only indexes the answer: the same
/// result, its `Ok` payload a [`CsvTable`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_table(input: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `input` is this shim's argument, null or a live string body for the call (C-ABI
        // contract), which `cstr` accepts.
        match CsvTable::read(unsafe { cstr(input) }) {
            Ok(table) => gos_rt_result_new(0, Box::into_raw(Box::new(table)) as i64),
            Err(msg) => {
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

/// `csv::read(input).unwrap_or(#[])` over [`gos_rt_csv_table`]'s answer: its
/// table, or a table of no records.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_csv_table_unwrap_or_empty(r: i128) -> i64 {
    ffi_entry!({
        if crate::c_abi::result::gos_rt_result_disc(r) == 0 {
            return crate::c_abi::result::gos_rt_result_payload(r);
        }
        let empty = CsvTable {
            text: Box::default(),
            unquoted: Vec::new(),
            fields: Vec::new(),
            records: vec![0],
            current: std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(empty)) as i64
    })
}

/// The number of records in a table.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_table_len(t: i64) -> i64 {
    // SAFETY: this shim's contract makes `t` a live table handle.
    ffi_entry!({ unsafe { table(t) }.len() as i64 })
}

/// Record `record` of a table, as the index the field reads take, panicking
/// as indexing `csv::read`'s rows would.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_csv_table_record(t: i64, record: i64) -> i64 {
    ffi_entry_passthrough!({
        // SAFETY: this shim's contract makes `t` a live table handle.
        let records = unsafe { table(t) }.len() as i64;
        if record < 0 || record >= records {
            crate::c_abi::panic::panic_oob_text("vec index", record, records);
        }
        record
    })
}

/// The number of fields in record `record`, which [`gos_rt_csv_table_record`]
/// answered.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_table_record_len(t: i64, record: i64) -> i64 {
    // SAFETY: this shim's contract makes `t` a live table handle, and `record` is in range.
    ffi_entry!({ unsafe { table(t) }.record(record as usize).len() as i64 })
}

/// Field `field` of record `record` as a `String` the table owns until its
/// next text read, for one call that reads it and keeps nothing.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_csv_table_field(
    t: i64,
    record: i64,
    field: i64,
) -> *mut c_char {
    ffi_entry_passthrough!({
        // SAFETY: this shim's contract makes `t` a live table handle.
        let table = unsafe { table(t) };
        let text = alloc_cstring(table.field(record, field).as_bytes());
        // SAFETY: `current` is null or the string the previous read made, which its one reader
        // has finished with.
        unsafe { crate::c_abi::string::gos_rt_str_free(table.current) };
        table.current = text;
        text
    })
}

/// `record[field].to_f64()` read in place.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_csv_table_field_to_f64_opt(
    t: i64,
    record: i64,
    field: i64,
) -> i128 {
    ffi_entry_passthrough!({
        // SAFETY: this shim's contract makes `t` a live table handle.
        match unsafe { table(t) }.field(record, field).parse::<f64>() {
            Ok(f) => crate::c_abi::gos_rt_result_new_f64(0, f),
            Err(_) => gos_rt_result_new(1, 0),
        }
    })
}

/// `record[field].to_i64()` read in place.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_csv_table_field_to_i64_opt(
    t: i64,
    record: i64,
    field: i64,
) -> i128 {
    ffi_entry_passthrough!({
        // SAFETY: this shim's contract makes `t` a live table handle.
        let text = unsafe { table(t) }.field(record, field);
        match crate::c_abi::string::parse_i64_bytes(text.as_bytes()) {
            Some(n) => gos_rt_result_new(0, n),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `record[field].to_bool()` read in place.
///
/// # Safety
/// `t` is a live table handle.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_csv_table_field_to_bool_opt(
    t: i64,
    record: i64,
    field: i64,
) -> i128 {
    ffi_entry_passthrough!({
        // SAFETY: this shim's contract makes `t` a live table handle.
        match unsafe { table(t) }.field(record, field) {
            "true" => gos_rt_result_new(0, 1),
            "false" => gos_rt_result_new(0, 0),
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// Frees a table. Zero is accepted and ignored.
///
/// # Safety
/// `t` is zero or a live table handle nothing reads afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_csv_table_free(t: i64) {
    ffi_entry!({
        if t != 0 {
            // SAFETY: a non-zero `t` is a table handle this call consumes.
            drop(unsafe { Box::from_raw(t as usize as *mut CsvTable) });
        }
    });
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
