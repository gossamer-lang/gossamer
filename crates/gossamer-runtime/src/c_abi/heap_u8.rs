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
// Heap [u8] primitive (`U8Vec`)
// ---------------------------------------------------------------
//
// Mirrors `GosI64Vec` but stores one byte per element. The
// motivating use case is fasta-style scratch buffers where each
// element is a single ASCII character - using `i64` storage
// blew memory up by 8x with no upside since the workers only
// ever write 0..=255.

#[repr(C)]
pub struct GosU8Vec {
    /// Length in elements (= bytes).
    pub len: i64,
    /// Heap-allocated backing storage. `len` bytes, 1-byte
    /// aligned.
    pub data: *mut u8,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_heap_u8_new(len: i64) -> *mut GosU8Vec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            return std::ptr::null_mut();
        }
        let n = len as usize;
        let mut v: Vec<u8> = vec![0u8; n];
        let data = v.as_mut_ptr();
        std::mem::forget(v);
        Box::into_raw(Box::new(GosU8Vec { len, data }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_free(v: *mut GosU8Vec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above), the vector `gos_rt_heap_u8_new` boxed, which
        // this call consumes (C-ABI contract).
        let v = unsafe { Box::from_raw(v) };
        if !v.data.is_null() {
            let n = v.len as usize;
            // SAFETY: `data` is the `Vec<u8>` allocation of `len` bytes and capacity
            // `gos_rt_heap_u8_new` made.
            unsafe {
                let _ = Vec::from_raw_parts(v.data, n, n);
            }
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_get(v: *const GosU8Vec, idx: i64) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() || idx < 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        if idx >= v.len || v.data.is_null() {
            return 0;
        }
        // SAFETY: `idx` is in `[0, len)` and `data` is non-null (checked above).
        unsafe { i64::from(*v.data.add(idx as usize)) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_set(v: *mut GosU8Vec, idx: i64, val: i64) {
    ffi_entry!((), {
        if v.is_null() || idx < 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if idx >= v_ref.len || v_ref.data.is_null() {
            return;
        }
        // Truncate to a byte; callers pass `i64`-typed source values
        // that always live in `0..=255` for this use case.
        // SAFETY: `idx` is in `[0, len)` and `data` is non-null (checked above).
        unsafe { *v_ref.data.add(idx as usize) = val as u8 };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_len(v: *const GosU8Vec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { (*v).len }
    })
}

/// Materialises the first `len` bytes of a `U8Vec` into a fresh
/// immutable `String` (NUL-terminated arena allocation). The
/// canonical "freeze the build buffer" step at the end of an
/// incremental construction loop - equivalent to F#'s
/// `StringBuilder.ToString()` or Rust's
/// `String::from_utf8(vec).unwrap()`.
///
/// `len` is a separate argument because callers typically
/// pre-allocate a capacity-sized `U8Vec` and write fewer bytes
/// than the buffer's nominal length. Returns the empty string
/// when `v` is null, `len` is non-positive, or `len` exceeds the
/// buffer's nominal length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_to_string(v: *const GosU8Vec, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() || len <= 0 {
            return alloc_cstring(b"");
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if v_ref.data.is_null() {
            return alloc_cstring(b"");
        }
        let cap = v_ref.len.max(0) as usize;
        let take = (len as usize).min(cap);
        // SAFETY: `data` holds `len` bytes and `take` is at most `len`.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(v_ref.data, take) };
        alloc_cstring(bytes)
    })
}

/// Bulk write `v[start..start+count]` to stdout, emitting a
/// newline after every `line_width` bytes. Single FFI call so
/// fasta-shape programs don't pay one `gos_rt_print_*` per byte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_write_lines_to_stdout(
    v: *const GosU8Vec,
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
            if cur >= STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and its first `cur` bytes are written.
                unsafe {
                    raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                }
                cur = 0;
            }
            let avail = STDOUT_BUF_SIZE - cur;
            let chars_to_eol = (line_width - col) as usize;
            let chars_left = end - idx;
            let take = std::cmp::min(chars_to_eol, std::cmp::min(chars_left, avail));
            // u8 → u8 plain memcpy.
            // SAFETY: `idx + take` is at most `end`, within the vector's `len` (checked above),
            // and `cur + take` is at most the buffer's size; the held `StdoutGuard` gives this
            // thread the stdout buffer and its length alone.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    v_ref.data.add(idx),
                    (*bytes_ptr).as_mut_ptr().add(cur),
                    take,
                );
            }
            cur += take;
            idx += take;
            col += take as i64;
            if col >= line_width {
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

/// Bulk-write the bytes of `v[start..start+count]` to stdout,
/// no line breaks. Used by the phased fasta variant where one
/// "phase" fills the buffer with whole 60-byte lines (newlines
/// already in the buffer) and then dumps it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_write_bytes_to_stdout(
    v: *const GosU8Vec,
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
            if cur >= STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and its first `cur` bytes are written.
                unsafe {
                    raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), cur));
                }
                cur = 0;
            }
            let avail = STDOUT_BUF_SIZE - cur;
            let take = std::cmp::min(avail, n - written);
            // SAFETY: `idx + take` is at most the checked `end`, within the vector's `len`, and
            // `cur + take` is at most the buffer's size; the held `StdoutGuard` gives this thread
            // the stdout buffer and its length alone.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    v_ref.data.add(idx),
                    (*bytes_ptr).as_mut_ptr().add(cur),
                    take,
                );
            }
            cur += take;
            idx += take;
            written += take;
            if cur == STDOUT_BUF_SIZE {
                // SAFETY: the held `StdoutGuard` gives this thread the stdout buffer and its
                // length alone, and its first `cur` bytes are written.
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

// ---------------------------------------------------------------
// Small-alphabet scans (`window_key`, `count_singles`,
// `count_pairs`, `count_kmers`)
// ---------------------------------------------------------------
//
// The compiled mirrors of the VM's `U8Vec` scan builtins. Each is a
// single C-side loop over the byte buffer, replacing the per-byte
// bytecode loop user code would otherwise write. The buffer holds
// 2-bit alphabet codes, so a window packs into an `i64` key by
// `(key << 2) | byte`.

/// Bytes read from a `U8Vec`, clamped to the buffer's own length.
unsafe fn u8_slice<'a>(v: *const GosU8Vec, limit: i64) -> &'a [u8] {
    if v.is_null() || limit <= 0 {
        return &[];
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live
    // `GosU8Vec`.
    let v = unsafe { &*v };
    if v.data.is_null() || v.len <= 0 {
        return &[];
    }
    let len = usize::try_from(v.len.min(limit)).unwrap_or(0);
    // SAFETY: `data` is non-null (checked above) and holds `len` bytes, and the slice takes at
    // most that many.
    unsafe { std::slice::from_raw_parts(v.data, len) }
}

/// Pack the `k` bytes starting at `index` into one 2-bit-per-byte key.
/// A window running past the end zero-extends, matching the by-byte
/// loop it replaces.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_window_key(v: *const GosU8Vec, index: i64, k: i64) -> i64 {
    ffi_entry!(0, {
        if v.is_null() || index < 0 || k <= 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v_ref = unsafe { &*v };
        if v_ref.data.is_null() {
            return 0;
        }
        let len = usize::try_from(v_ref.len.max(0)).unwrap_or(0);
        let start = usize::try_from(index).unwrap_or(usize::MAX).min(len);
        let want = usize::try_from(k).unwrap_or(0);
        let stop = start.saturating_add(want).min(len);
        let mut key: i64 = 0;
        for offset in start..stop {
            // SAFETY: `offset` is below `len`, the vector's length.
            let byte = unsafe { *v_ref.data.add(offset) };
            key = (key << 2) | i64::from(byte);
        }
        for _ in 0..(start.saturating_add(want).saturating_sub(stop)) {
            key <<= 2;
        }
        key
    })
}

/// Frequency of each of the four single-byte codes, as a `Vec<i64>`
/// of length 4 indexed by the code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_count_singles(
    v: *const GosU8Vec,
    length: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = gos_rt_vec_new(8);
        let mut counts = [0i64; 4];
        // SAFETY: `v` is this shim's argument, null or a live vector (C-ABI contract), which
        // `u8_slice` accepts.
        for &byte in unsafe { u8_slice(v, length) } {
            if (byte as usize) < counts.len() {
                counts[byte as usize] += 1;
            }
        }
        for count in counts {
            let bytes = count.to_ne_bytes();
            // SAFETY: `out` is the fresh vec made above, or null, which `gos_rt_vec_push`
            // accepts, and `bytes` is one 8-byte element.
            unsafe { gos_rt_vec_push(out, bytes.as_ptr()) };
        }
        out
    })
}

/// Frequency of each adjacent byte pair, as a `Vec<i64>` of length 16
/// indexed by `(first << 2) | second`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_count_pairs(
    v: *const GosU8Vec,
    length: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = gos_rt_vec_new(8);
        let mut counts = [0i64; 16];
        // SAFETY: `v` is this shim's argument, null or a live vector (C-ABI contract), which
        // `u8_slice` accepts.
        let bytes = unsafe { u8_slice(v, length) };
        for window in bytes.windows(2) {
            let key = ((window[0] as usize) << 2) | (window[1] as usize);
            if key < counts.len() {
                counts[key] += 1;
            }
        }
        for count in counts {
            let raw = count.to_ne_bytes();
            // SAFETY: `out` is the fresh vec made above, or null, which `gos_rt_vec_push`
            // accepts, and `raw` is one 8-byte element.
            unsafe { gos_rt_vec_push(out, raw.as_ptr()) };
        }
        out
    })
}

/// Sliding-window k-mer frequencies, as a `Map<i64, i64>` from the
/// packed window key to its count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_heap_u8_count_kmers(
    v: *const GosU8Vec,
    length: i64,
    k: i64,
) -> *mut GosMap {
    ffi_entry!(std::ptr::null_mut(), {
        let out = gos_rt_map_new(8, 8);
        // SAFETY: `v` is this shim's argument, null or a live vector (C-ABI contract), which
        // `u8_slice` accepts.
        let bytes = unsafe { u8_slice(v, length) };
        let Ok(width) = usize::try_from(k) else {
            return out;
        };
        if width == 0 || width > bytes.len() {
            return out;
        }
        for window in bytes.windows(width) {
            let mut key: i64 = 0;
            for &byte in window {
                key = (key << 2) | i64::from(byte);
            }
            // SAFETY: `out` is the fresh map made above, or null, which `gos_rt_map_inc_i64`
            // accepts.
            unsafe { gos_rt_map_inc_i64(out, key, 1) };
        }
        out
    })
}
