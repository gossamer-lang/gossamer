#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_ptr_alignment)]

//! `std::archive::{tar,zip}` leaf intrinsics. `read` returns a
//! `[(String, [u8], bool)]` tuple-vec (the injected wrapper folds
//! each into a real `TarEntry` / `ZipEntry` struct); `write` takes a
//! `[(String, [u8])]` tuple-vec and returns `Result<[u8], Error>`.
//! Mirrors `gossamer_std::archive` so the compiled tier matches the
//! VM byte-for-byte.

use std::io::{Cursor, Read, Write};
use std::os::raw::c_char;

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
fn build_entry_vec(entries: &[(String, Vec<u8>, bool)]) -> *mut GosVec {
    let v = gos_rt_vec_with_capacity(24, entries.len() as i64);
    for (name, data, is_dir) in entries {
        let tup: [i64; 3] = [
            alloc_cstring(name.as_bytes()) as i64,
            byte_vec(data) as i64,
            i64::from(*is_dir),
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

// ----------------------------------------------------------------- tar

/// `archive::tar::read(data) -> Result<[(String,[u8],bool)], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tar_read_raw(data: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let iter = match archive.entries() {
            Ok(it) => it,
            Err(e) => return err(&format!("tar entries: {e}")),
        };
        let mut out: Vec<(String, Vec<u8>, bool)> = Vec::new();
        for entry in iter {
            let mut entry = match entry {
                Ok(en) => en,
                Err(e) => return err(&format!("tar entry: {e}")),
            };
            let name = match entry.path() {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => return err(&format!("tar entry path: {e}")),
            };
            let kind = entry.header().entry_type();
            let is_dir = kind.is_dir();
            let mut buf = Vec::new();
            if kind.is_file() && entry.read_to_end(&mut buf).is_err() {
                return err(&format!("tar read {name}"));
            }
            out.push((name, buf, is_dir));
        }
        ok_vec(build_entry_vec(&out))
    })
}

/// `archive::tar::write(files) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tar_write(files: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `files` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `read_name_data_pairs` accepts.
        let pairs = unsafe { read_name_data_pairs(files) };
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in &pairs {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            if builder
                .append_data(&mut header, name, data.as_slice())
                .is_err()
            {
                return err(&format!("tar append {name}"));
            }
        }
        match builder.into_inner() {
            Ok(out) => ok_bytes(&out),
            Err(e) => err(&format!("tar finish: {e}")),
        }
    })
}

// ----------------------------------------------------------------- zip

/// `archive::zip::read(data) -> Result<[(String,[u8],bool)], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_zip_read_raw(data: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `data` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
            Ok(a) => a,
            Err(e) => return err(&format!("zip read: {e}")),
        };
        let mut out: Vec<(String, Vec<u8>, bool)> = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut file = match archive.by_index(i) {
                Ok(f) => f,
                Err(e) => return err(&format!("zip entry {i}: {e}")),
            };
            let name = file.name().to_owned();
            let is_dir = file.is_dir();
            let mut buf = Vec::new();
            if !is_dir && file.read_to_end(&mut buf).is_err() {
                return err(&format!("zip read entry {name}"));
            }
            out.push((name, buf, is_dir));
        }
        ok_vec(build_entry_vec(&out))
    })
}

/// `archive::zip::write(files) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_zip_write(files: *const GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `files` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `read_name_data_pairs` accepts.
        let pairs = unsafe { read_name_data_pairs(files) };
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in &pairs {
            if zip.start_file(name.as_str(), opts).is_err() {
                return err(&format!("zip start_file {name}"));
            }
            if zip.write_all(data).is_err() {
                return err(&format!("zip write {name}"));
            }
        }
        match zip.finish() {
            Ok(c) => ok_bytes(&c.into_inner()),
            Err(e) => err(&format!("zip finish: {e}")),
        }
    })
}
