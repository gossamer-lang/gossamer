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

use super::*;

// ---------------------------------------------------------------
// Heap [i64] primitive
// ---------------------------------------------------------------
//
// Small-but-essential heap-backed array shared by reference
// across goroutines. Same memory model as Go's `make([]int64,
// n)`; the user holds the pointer as an i64 and passes it
// through `go expr` / channels. Indexing goes through the
// runtime so the language doesn't have to grow `&mut [T]`
// semantics for fan-out workloads.
//
// **Concurrency contract.** `GosI64Vec` and `GosU8Vec` are
// **single-owner**: the backing buffer is allocated and freed by
// one goroutine, and concurrent mutation across goroutines is
// undefined behaviour. `gos_rt_arr_push`-style operations resize
// by reallocating `data`, so two goroutines that both observe
// `len == cap` and both reallocate corrupt the heap. For
// cross-goroutine sharing use the `GosSyncI64Vec` / `GosSyncU8Vec`
// types defined below - same conceptual shape, every operation
// guarded by an internal `parking_lot` mutex.

#[repr(C)]
pub struct GosI64Vec {
    /// Length in elements.
    pub len: i64,
    /// Heap-allocated backing storage. `len * 8` bytes,
    /// 8-byte-aligned.
    pub data: *mut i64,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_heap_i64_new(len: i64) -> *mut GosI64Vec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            return std::ptr::null_mut();
        }
        let n = len as usize;
        let mut v: Vec<i64> = vec![0i64; n];
        let data = v.as_mut_ptr();
        std::mem::forget(v);
        Box::into_raw(Box::new(GosI64Vec { len, data }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_free(v: *mut GosI64Vec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above), the vector `gos_rt_heap_i64_new` boxed, which
        // this call consumes (C-ABI contract).
        let v = unsafe { Box::from_raw(v) };
        if !v.data.is_null() {
            let n = v.len as usize;
            // SAFETY: `data` is the `Vec<i64>` allocation of `len` words and capacity
            // `gos_rt_heap_i64_new` made.
            unsafe {
                let _ = Vec::from_raw_parts(v.data, n, n);
            }
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_get(v: *const GosI64Vec, idx: i64) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() || idx < 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        if idx >= v.len || v.data.is_null() {
            return 0;
        }
        // SAFETY: `v` is this shim's `GosI64Vec` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        unsafe { *v.data.add(idx as usize) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_set(v: *mut GosI64Vec, idx: i64, val: i64) {
    ffi_entry!((), {
        if v.is_null() || idx < 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if idx >= v_ref.len || v_ref.data.is_null() {
            return;
        }
        // SAFETY: `idx` is in `[0, len)` and `data` is non-null (checked above).
        unsafe { *v_ref.data.add(idx as usize) = val };
    });
}

/// Length accessor for the heap vec - separate from
/// `gos_rt_arr_len` so the codegen can route by symbol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_len(v: *const GosI64Vec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { (*v).len }
    })
}

/// Bulk write `v[start..start+count]` to stdout, emitting a
/// newline after every `line_width` bytes. Used by fasta-style
/// programs that fill a worker buffer then need to flush it
/// with line breaks. Single FFI call instead of one per
/// line.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_write_lines_to_stdout(
    v: *const GosI64Vec,
    start: i64,
    count: i64,
    line_width: i64,
) {
    ffi_entry!((), {
        if v.is_null() || start < 0 || count <= 0 || line_width <= 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if v_ref.data.is_null() {
            return;
        }
        let end = start.saturating_add(count);
        if end > v_ref.len {
            return;
        }
        let _guard = StdoutGuard::acquire();
        let bytes_ptr = GOS_RT_STDOUT_BYTES.0.get();
        let len_ptr = GOS_RT_STDOUT_LEN.0.get();
        // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its length
        // alone.
        let mut cur = unsafe { *len_ptr };
        let mut col: i64 = 0;
        let mut idx = start as usize;
        let end = (start + count) as usize;
        while idx < end {
            // Need at least 1 byte; if buffer full, flush.
            if cur >= STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and its first `cur` bytes are written.
                unsafe {
                    raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                }
                cur = 0;
            }
            let avail = STDOUT_BUF_SIZE - cur;
            // Plan a packed run that fits in the remaining
            // buffer space and doesn't cross the next newline.
            let chars_to_eol = (line_width - col) as usize;
            let chars_left = end - idx;
            let take = std::cmp::min(chars_to_eol, std::cmp::min(chars_left, avail));
            // SAFETY: `idx + take` is at most `end`, within the vector's `len` (checked above),
            // and `cur + take` is at most the buffer's size; the held `StdoutGuard` gives this
            // thread the stdout buffer and its length alone.
            unsafe {
                for i in 0..take {
                    *(*bytes_ptr).as_mut_ptr().add(cur + i) = *v_ref.data.add(idx + i) as u8;
                }
            }
            cur += take;
            idx += take;
            col += take as i64;
            if col >= line_width {
                // Append newline if room (otherwise flush first).
                if cur >= STDOUT_BUF_SIZE {
                    // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                    // length alone, and its first `cur` bytes are written.
                    unsafe {
                        raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                    }
                    cur = 0;
                }
                // SAFETY: `cur` is below the buffer's size (flushed above when full); the held
                // `StdoutGuard` gives this thread the stdout buffer and its length alone.
                unsafe {
                    *(*bytes_ptr).as_mut_ptr().add(cur) = b'\n';
                }
                cur += 1;
                col = 0;
            }
        }
        // Trailing newline if we ended mid-line (matches the
        // bench-game fasta convention: the last line is short
        // but still terminated with '\n').
        if col > 0 {
            if cur >= STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and its first `cur` bytes are written.
                unsafe {
                    raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                }
                cur = 0;
            }
            // SAFETY: `cur` is below the buffer's size (flushed above when full); the held
            // `StdoutGuard` gives this thread the stdout buffer and its length alone.
            unsafe {
                *(*bytes_ptr).as_mut_ptr().add(cur) = b'\n';
            }
            cur += 1;
        }
        // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its length
        // alone.
        unsafe { *len_ptr = cur };
    });
}

/// Bulk-write the low byte of every i64 slot in
/// `v[start..start+count]` to stdout. Used by the
/// multi-threaded fasta variant: each worker fills a slice
/// of a shared heap vec; main writes ranges out in order
/// without per-byte FFI cost.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_i64_write_bytes_to_stdout(
    v: *const GosI64Vec,
    start: i64,
    count: i64,
) {
    ffi_entry!((), {
        if v.is_null() || start < 0 || count <= 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if v_ref.data.is_null() {
            return;
        }
        let end = start.saturating_add(count);
        if end > v_ref.len {
            return;
        }
        let _guard = StdoutGuard::acquire();
        let bytes_ptr = GOS_RT_STDOUT_BYTES.0.get();
        let len_ptr = GOS_RT_STDOUT_LEN.0.get();
        // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its length
        // alone.
        let mut cur = unsafe { *len_ptr };
        let n = count as usize;
        let mut idx = start as usize;
        let mut written = 0usize;
        while written < n {
            let avail = STDOUT_BUF_SIZE - cur;
            let take = std::cmp::min(avail, n - written);
            // SAFETY: `idx + take` is at most the checked `end`, within the vector's `len`, and
            // `cur + take` is at most the buffer's size; the held `StdoutGuard` gives this thread
            // the stdout buffer and its length alone.
            unsafe {
                for i in 0..take {
                    *(*bytes_ptr).as_mut_ptr().add(cur + i) = *v_ref.data.add(idx + i) as u8;
                }
            }
            cur += take;
            idx += take;
            written += take;
            if cur == STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and it holds `cur` written bytes.
                unsafe {
                    raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                }
                cur = 0;
            }
        }
        // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its length
        // alone.
        unsafe { *len_ptr = cur };
    });
}
