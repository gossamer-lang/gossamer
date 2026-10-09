#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// bufio::Scanner - wraps a reader with a buffered line iterator.
// `Scanner::new(reader)` returns an opaque handle; `.scan()`
// advances to the next line and returns `true` when one was
// available; `.text()` returns the most recently scanned line.
// ---------------------------------------------------------------

pub struct GosScanner {
    lines: std::vec::IntoIter<String>,
    current: Option<String>,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_scanner_new(
    stream: *mut std::ffi::c_void,
) -> *mut GosScanner {
    ffi_entry!({
        // Read the entire stream up front: cheap for the typical
        // CLI/file usage and avoids weaving a real Read trait
        // through the runtime.
        let text = if stream.is_null() {
            String::new()
        } else {
            // Re-use the stream-read-to-string helper: every stream
            // the runtime exposes is one of the io handles.
            // SAFETY: `stream` is this shim's non-null stream handle (checked above), live for the
            // call (C-ABI contract).
            let cstr = unsafe { gos_rt_stream_read_to_string(stream.cast::<GosStream>()) };
            if cstr.is_null() {
                String::new()
            } else {
                // SAFETY: `cstr` is the fresh non-null string the read answered, owned here.
                let text = unsafe { crate::c_abi::gos_str_arg_string(cstr) };
                // SAFETY: as above; its text is copied and nothing reads it again.
                unsafe { crate::c_abi::string::gos_rt_str_free(cstr) };
                text
            }
        };
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        Box::into_raw(Box::new(GosScanner {
            lines: lines.into_iter(),
            current: None,
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_scanner_scan(s: *mut GosScanner) -> bool {
    ffi_entry!({
        if s.is_null() {
            return false;
        }
        // SAFETY: `s` is a handle from compiled code, checked non-null above and live for the whole call.
        let scanner = unsafe { &mut *s };
        if let Some(line) = scanner.lines.next() {
            scanner.current = Some(line);
            true
        } else {
            scanner.current = None;
            false
        }
    })
}

/// `scanner.next() -> Option<String>`: advances to the next line and answers
/// it, or `None` at the end of input. The line is also the scanner's current
/// text, as a `scan` would leave it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_scanner_next(s: *mut GosScanner) -> i128 {
    ffi_entry!({
        if s.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `s` is a handle from compiled code, checked non-null above and live for the whole call.
        let scanner = unsafe { &mut *s };
        if let Some(line) = scanner.lines.next() {
            let text = alloc_cstring(line.as_bytes()) as i64;
            scanner.current = Some(line);
            gos_rt_result_new(0, text)
        } else {
            scanner.current = None;
            gos_rt_result_new(1, 0)
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_scanner_text(s: *const GosScanner) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a handle from compiled code, checked non-null above and live for the whole call.
        let scanner = unsafe { &*s };
        match &scanner.current {
            Some(text) => alloc_cstring(text.as_bytes()),
            None => alloc_cstring(b""),
        }
    })
}
