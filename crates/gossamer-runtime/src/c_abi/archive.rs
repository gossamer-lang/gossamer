#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_ptr_alignment)]

//! `std::archive::{tar,zip}` leaf intrinsics over `crate::codec::archive`,
//! the implementation the bytecode VM shares. `read` returns a
//! `[(String, [u8], bool)]` tuple-vec (the injected wrapper folds each into a
//! real `TarEntry` / `ZipEntry` struct); `write` takes a `[(String, [u8])]`
//! tuple-vec and returns `Result<[u8], Error>`.

use std::os::raw::c_char;

use crate::codec::archive::{self, Entry, EntryKind, Limits};

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;
use super::vec::{GosVec, gos_rt_vec_push, gos_rt_vec_with_capacity};

fn byte_vec(bytes: &[u8]) -> *mut GosVec {
    super::encoding::bytes_to_gosvec(bytes)
}

/// Reads a `[(name: String, data: [u8])]` tuple-vec (16-byte inline
/// 2-slot elements) into owned `(name, data)` pairs.
unsafe fn read_name_data_pairs(v: *const GosVec) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    if v.is_null() {
        return out;
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live `Vec`.
    let vref = unsafe { &*v };
    // Each element is a name word and a data word, so a narrower slot holds no pair.
    if vref.ptr.is_null() || vref.len <= 0 || vref.elem_bytes < 16 {
        return out;
    }
    let elem = vref.elem_bytes as usize;
    let base = vref.ptr.as_ptr();
    for i in 0..vref.len as usize {
        // SAFETY: `i` counts below the vec's length, so the element's two words lie inside its
        // slot of at least 16 bytes.
        let slot = unsafe { base.add(i * elem).cast::<i64>() };
        // SAFETY: as above; a slot may sit at any byte offset.
        let name_ptr = unsafe { slot.read_unaligned() } as *const c_char;
        // SAFETY: as above.
        let data_ptr = unsafe { slot.add(1).read_unaligned() } as *const GosVec;
        let name = if name_ptr.is_null() {
            String::new()
        } else {
            // SAFETY: a non-null name word is a live string body (C-ABI contract).
            unsafe { crate::c_abi::gos_str_arg_string(name_ptr) }
        };
        // SAFETY: a data word is null or a live `Vec<u8>` (C-ABI contract), which `vec_bytes`
        // accepts.
        out.push((name, unsafe { crate::c_abi::vec::vec_bytes(data_ptr) }));
    }
    out
}

/// Slot layout of [`build_entry_vec`] elements: the entry name string
/// at word 0 and the data byte-vec at word 1, both owned by the vec.
static ENTRY_SLOT_CHILDREN: [crate::c_abi::vec::VecSlotChild; 2] = [
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 0,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    },
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 1,
        kind: crate::c_abi::vec::vec_elem_kind::VEC,
    },
];

/// Builds the `[(String, [u8], bool)]` result Vec: 24-byte inline
/// 3-slot elements `[name_ptr, data_vec_ptr, is_dir]`. The vec owns
/// the name strings and data vecs (slot-children layout registered
/// after the pushes), so `gos_rt_vec_free` deep-frees them.
fn build_entry_vec(entries: &[Entry]) -> *mut GosVec {
    let v = gos_rt_vec_with_capacity(24, entries.len() as i64);
    for entry in entries {
        let tup: [i64; 3] = [
            alloc_cstring(entry.name.as_bytes()) as i64,
            byte_vec(&entry.data) as i64,
            i64::from(entry.kind == EntryKind::Dir),
        ];
        // SAFETY: `v` is the fresh vec made above, or null, which `gos_rt_vec_push` accepts, and
        // `tup` is one 24-byte element.
        unsafe { gos_rt_vec_push(v, tup.as_ptr().cast::<u8>()) };
    }
    // SAFETY: `v` is the live vec built above.
    unsafe { crate::c_abi::vec::vec_set_slot_children(v, &ENTRY_SLOT_CHILDREN) };
    v
}

fn ok_vec(v: *mut GosVec) -> i128 {
    gos_rt_result_new(0, v as i64)
}

fn ok_bytes(bytes: &[u8]) -> i128 {
    gos_rt_result_new(0, byte_vec(bytes) as i64)
}

fn err(msg: &str) -> i128 {
    let e = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, e as i64)
}

/// The carrier a codec answer becomes.
fn result_of<T>(answer: Result<T, String>, ok: impl FnOnce(T) -> i128) -> i128 {
    match answer {
        Ok(value) => ok(value),
        Err(msg) => err(&msg),
    }
}

type Reader = fn(&[u8], Limits) -> Result<Vec<Entry>, String>;

/// A raw read under the limits three count arguments name (negative is
/// unbounded).
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
unsafe fn read_raw(
    read: Reader,
    data: *const GosVec,
    entries: i64,
    entry_bytes: i64,
    total: i64,
) -> i128 {
    // SAFETY: the caller passes `data` null or live, which `vec_bytes_cow` accepts.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes_cow(data) };
    let limits = Limits::from_counts(entries, entry_bytes, total);
    result_of(read(&bytes, limits), |list| ok_vec(build_entry_vec(&list)))
}

/// Writes the archive in `data` under `dir`.
///
/// # Safety
/// `data` is null or a live `Vec`, and `dir` null or a live string, for the call.
unsafe fn extract_raw(read: Reader, data: *const GosVec, dir: *const c_char) -> i128 {
    // SAFETY: the caller passes `data` null or live, which `vec_bytes_cow` accepts.
    let bytes = unsafe { crate::c_abi::vec::vec_bytes_cow(data) };
    // SAFETY: the caller passes `dir` null or a live string body.
    let dir = unsafe { crate::c_abi::gos_str_arg_text(dir) };
    let written = read(&bytes, Limits::default())
        .and_then(|list| archive::extract(&list, std::path::Path::new(dir)));
    result_of(written, |n| {
        gos_rt_result_new(0, i64::try_from(n).unwrap_or(i64::MAX))
    })
}

/// `archive::tar::read` leaf: `(data, max_entries, max_entry_bytes,
/// max_total_bytes) -> Result<[(String, [u8], bool)], Error>`.
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tar_read_raw(
    data: *const GosVec,
    max_entries: i64,
    max_entry_bytes: i64,
    max_total_bytes: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or live for the call (C-ABI contract).
        unsafe {
            read_raw(
                archive::tar_read,
                data,
                max_entries,
                max_entry_bytes,
                max_total_bytes,
            )
        }
    })
}

/// `archive::zip::read` leaf, with the arguments of [`gos_rt_tar_read_raw`].
///
/// # Safety
/// `data` is null or a live `Vec` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_zip_read_raw(
    data: *const GosVec,
    max_entries: i64,
    max_entry_bytes: i64,
    max_total_bytes: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, null or live for the call (C-ABI contract).
        unsafe {
            read_raw(
                archive::zip_read,
                data,
                max_entries,
                max_entry_bytes,
                max_total_bytes,
            )
        }
    })
}

/// `archive::tar::write(files) -> Result<[u8], Error>`.
///
/// # Safety
/// `files` is null or a live `[(String, [u8])]` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tar_write(files: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `files` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `read_name_data_pairs` accepts.
        let pairs = unsafe { read_name_data_pairs(files) };
        result_of(archive::tar_write(&pairs), |bytes| ok_bytes(&bytes))
    })
}

/// `archive::zip::write(files) -> Result<[u8], Error>`.
///
/// # Safety
/// `files` is null or a live `[(String, [u8])]` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_zip_write(files: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `files` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `read_name_data_pairs` accepts.
        let pairs = unsafe { read_name_data_pairs(files) };
        result_of(archive::zip_write(&pairs), |bytes| ok_bytes(&bytes))
    })
}

/// `archive::tar::extract(data, dir) -> Result<i64, Error>`.
///
/// # Safety
/// `data` is null or a live `Vec`, and `dir` null or a live string, for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tar_extract(data: *const GosVec, dir: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both arguments are this shim's, null or live for the call (C-ABI contract).
        unsafe { extract_raw(archive::tar_read, data, dir) }
    })
}

/// `archive::zip::extract(data, dir) -> Result<i64, Error>`.
///
/// # Safety
/// `data` is null or a live `Vec`, and `dir` null or a live string, for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_zip_extract(data: *const GosVec, dir: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both arguments are this shim's, null or live for the call (C-ABI contract).
        unsafe { extract_raw(archive::zip_read, data, dir) }
    })
}

/// `archive::enclosed_path(name) -> Option<String>`.
///
/// # Safety
/// `name` is null or a live string body for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_archive_enclosed_path(name: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `name` is this shim's argument, null or a live string body (C-ABI contract).
        let name = unsafe { crate::c_abi::gos_str_arg_text(name) };
        match archive::enclosed_path(name) {
            Some(path) => gos_rt_result_new(0, alloc_cstring(path.as_bytes()) as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}
