#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
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
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use parking_lot::Mutex;

#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the flags mirror the OS open flags one for one"
)]
struct GosOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

/// The advisory ranges each open file handle currently holds.
type HeldLocks = HashMap<i64, Vec<(u64, u64)>>;

/// An open file: the handle positional and metadata calls share, and the lock
/// serialising calls that move the file cursor.
struct FileEntry {
    file: std::fs::File,
    cursor: Mutex<()>,
}

// Lookups take the read side, so goroutines working on open files never
// serialise on the table; only open and close write it.
static FILE_HANDLES: parking_lot::RwLock<Option<HashMap<i64, Arc<FileEntry>>>> =
    parking_lot::RwLock::new(None);

// Win32 releases a lock only through an UnlockFileEx naming the same span
// LockFileEx took, so a whole-file `unlock` has to name each range it is
// releasing. The ranges a handle holds are recorded here and replayed.
static HELD_LOCKS: Mutex<Option<HeldLocks>> = Mutex::new(None);
static OPEN_OPTIONS_HANDLES: Mutex<Option<HashMap<i64, Arc<Mutex<GosOpenOptions>>>>> =
    Mutex::new(None);
static NEXT_FS_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_TEMP_RESOURCE: AtomicI64 = AtomicI64::new(1);

fn next_fs_handle() -> i64 {
    NEXT_FS_HANDLE.fetch_add(1, Ordering::Relaxed)
}

fn insert_file(file: std::fs::File) -> i64 {
    let h = next_fs_handle();
    FILE_HANDLES
        .write()
        .get_or_insert_with(HashMap::new)
        .insert(
            h,
            Arc::new(FileEntry {
                file,
                cursor: Mutex::new(()),
            }),
        );
    h
}

fn file_clone(h: i64) -> Option<Arc<FileEntry>> {
    FILE_HANDLES
        .read()
        .as_ref()
        .and_then(|m| m.get(&h).cloned())
}

fn insert_open_options(opts: GosOpenOptions) -> i64 {
    let h = next_fs_handle();
    OPEN_OPTIONS_HANDLES
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(h, Arc::new(Mutex::new(opts)));
    h
}

fn open_options_clone(h: i64) -> Option<Arc<Mutex<GosOpenOptions>>> {
    OPEN_OPTIONS_HANDLES
        .lock()
        .as_ref()
        .and_then(|m| m.get(&h).cloned())
}

fn apply_open_options(opts: &GosOpenOptions) -> std::fs::OpenOptions {
    let mut out = std::fs::OpenOptions::new();
    out.read(opts.read)
        .write(opts.write)
        .append(opts.append)
        .truncate(opts.truncate)
        .create(opts.create)
        .create_new(opts.create_new);
    out
}

fn fs_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { gos_rt_result_new(1, err as i64) }
}

fn fs_io_err(err: &std::io::Error, context: &str) -> i128 {
    let msg = classify_io_error(err, context);
    fs_err(&msg)
}

fn temp_prefix(prefix: *const c_char, operation: &str) -> Result<String, i128> {
    if prefix.is_null() {
        return Err(fs_err(&format!("{operation}: null prefix")));
    }
    let prefix = unsafe { crate::c_abi::gos_str_arg_string(prefix) };
    if prefix.contains(['/', '\\', '\0']) || matches!(prefix.as_str(), "." | "..") {
        return Err(fs_err(
            "temporary-resource prefix must be a single path component",
        ));
    }
    Ok(prefix)
}

fn temp_resource_path(prefix: &str) -> std::path::PathBuf {
    let n = NEXT_TEMP_RESOURCE.fetch_add(1, Ordering::Relaxed);
    let nanos = crate::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    crate::platform::temp_dir().join(format!(
        "gossamer-{prefix}-{:x}-{nanos:x}-{n}",
        crate::platform::process_id()
    ))
}

// ---------------------------------------------------------------
// fs / path helpers - read_to_string, write, create_dir_all,
// path::join. Mirror Rust std::fs minus the typed Error.
// Filesystem syscalls are synchronous in every host kernel. Core
// reads, writes, and handle operations run through `run_blocking`,
// which parks a compiled goroutine while an OS thread performs the
// syscall. Pointer decoding and Gossamer heap allocation stay on the
// calling goroutine.
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_read_to_string(path: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if path.is_null() {
            return alloc_cstring(b"");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        match crate::sched_global::run_blocking("fs-read-string", move || {
            std::fs::read_to_string(p)
        }) {
            Ok(Ok(text)) => alloc_cstring(text.as_bytes()),
            Ok(Err(_)) | Err(_) => alloc_cstring(b""),
        }
    })
}

/// `fs::read_to_string(path) -> Result<String, errors::Error>`. Result-shaped
/// counterpart of the bare-string `gos_rt_fs_read_to_string` (which returns ""
/// on failure and cannot express an error); the compiled tiers route
/// `fs::read_to_string` here so a missing / unreadable path propagates `Err`
/// identically to the VM, mirroring how `fs::read` (`gos_rt_fs_read_bytes_result`)
/// already builds its error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_read_to_string_result(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("read_to_string: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-read-string", move || {
            std::fs::read_to_string(p)
        }) {
            Ok(Ok(text)) => {
                let s = alloc_cstring(text.as_bytes());
                unsafe { gos_rt_result_new(0, s as i64) }
            }
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_write(path: *const c_char, contents: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if path.is_null() || contents.is_null() {
            return 0;
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let c = unsafe { crate::c_abi::gos_str_arg_bytes(contents) }.to_vec();
        i64::from(
            crate::sched_global::run_blocking("fs-write", move || std::fs::write(p, c))
                .is_ok_and(|result| result.is_ok()),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_create_dir_all(path: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if path.is_null() {
            return 0;
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        i64::from(
            crate::sched_global::run_blocking("fs-mkdir-all", move || std::fs::create_dir_all(p))
                .is_ok_and(|result| result.is_ok()),
        )
    })
}

/// `os::remove_file(path) -> Result<(), IoError>`. Returns a bool
/// in the compiled tier (the same shape every other os/fs mutator
/// uses). Without an explicit binding the call falls through to a
/// non-existent symbol and corrupts the destination slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_remove_file(path: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if path.is_null() {
            return 0;
        }
        let p = unsafe { crate::c_abi::gos_str_arg_text(path) };
        i64::from(
            crate::sched_global::run_blocking("fs-remove-file", move || std::fs::remove_file(p))
                .is_ok_and(|result| result.is_ok()),
        )
    })
}

/// Mirrors `gossamer_std::io::IoError::from_std` classification so the
/// native fs error text matches the interp tier byte-for-byte
/// (`not found: {path}` / `permission denied: {path}` / `io: {path}: {err}`).
/// `gossamer-runtime` cannot depend on `gossamer-std` (the dependency
/// points the other way), so the three-arm mapping is replicated here;
/// the cross-tier fixture suite pins the parity.
/// Uniform text for an OS error reached through the filesystem surface:
/// the same classification `io::Error::from_std` applies, so a diagnostic
/// reads identically whichever tier produced it.
pub fn classify_io_error(err: &std::io::Error, context: &str) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => format!("not found: {context}"),
        ErrorKind::PermissionDenied => format!("permission denied: {context}"),
        _ => format!("io: {context}: {err}"),
    }
}

/// `fs::File::open(path) -> Result<File, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_open(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("File::open: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-open", move || std::fs::File::open(p)) {
            Ok(Ok(file)) => unsafe { gos_rt_result_new(0, insert_file(file)) },
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::create(path) -> Result<File, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_create(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("File::create: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-create", move || std::fs::File::create(p)) {
            Ok(Ok(file)) => unsafe { gos_rt_result_new(0, insert_file(file)) },
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::temp_dir(prefix) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_temp_dir(prefix: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let prefix = match temp_prefix(prefix, "fs::temp_dir") {
            Ok(prefix) => prefix,
            Err(error) => return error,
        };
        let path = temp_resource_path(&prefix);
        let context = path.to_string_lossy().into_owned();
        match crate::sched_global::run_blocking("fs-temp-dir", move || std::fs::create_dir(&path)) {
            Ok(Ok(())) => {
                let path = alloc_cstring(context.as_bytes());
                unsafe { gos_rt_result_new(0, path as i64) }
            }
            Ok(Err(error)) => fs_io_err(&error, &context),
            Err(error) => fs_err(&error),
        }
    })
}

/// Layout of the `fs::temp_file` `(File, String)` pair: the path string is
/// owned by the blob; the file handle is a registry id.
static TEMP_FILE_PAIR_META: [i64; 5] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    1,
    (gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
];

/// `fs::temp_file(prefix) -> Result<(File, String), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_temp_file(prefix: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let prefix = match temp_prefix(prefix, "fs::temp_file") {
            Ok(prefix) => prefix,
            Err(error) => return error,
        };
        let path = temp_resource_path(&prefix);
        let context = path.to_string_lossy().into_owned();
        match crate::sched_global::run_blocking("fs-temp-file", move || {
            std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)
        }) {
            Ok(Ok(file)) => {
                let words = [insert_file(file), alloc_cstring(context.as_bytes()) as i64];
                let pair = crate::c_abi::rc::counted_words(&words, &TEMP_FILE_PAIR_META);
                if pair.is_null() {
                    return fs_err("fs::temp_file: allocation failed");
                }
                unsafe { gos_rt_result_new(0, pair as i64) }
            }
            Ok(Err(error)) => fs_io_err(&error, &context),
            Err(error) => fs_err(&error),
        }
    })
}

/// `fs::OpenOptions::new() -> OpenOptions`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_open_options_new() -> i64 {
    insert_open_options(GosOpenOptions::default())
}

macro_rules! open_option_setter {
    ($name:ident, $field:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(h: i64, enabled: i32) -> i64 {
            ffi_entry!(h, {
                if let Some(opts) = open_options_clone(h) {
                    opts.lock().$field = enabled != 0;
                }
                h
            })
        }
    };
}

open_option_setter!(gos_rt_fs_open_options_read, read);
open_option_setter!(gos_rt_fs_open_options_write, write);
open_option_setter!(gos_rt_fs_open_options_append, append);
open_option_setter!(gos_rt_fs_open_options_truncate, truncate);
open_option_setter!(gos_rt_fs_open_options_create, create);
open_option_setter!(gos_rt_fs_open_options_create_new, create_new);

/// `fs::OpenOptions::open(path) -> Result<File, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_open_options_open(h: i64, path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("OpenOptions::open: null path");
        }
        let Some(opts) = open_options_clone(h) else {
            return fs_err("OpenOptions::open: stale handle");
        };
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let open = apply_open_options(&opts.lock());
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-open-options", move || open.open(p)) {
            Ok(Ok(file)) => unsafe { gos_rt_result_new(0, insert_file(file)) },
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::read(max) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_read(h: i64, max: i64) -> i128 {
    ffi_entry!(0i128, {
        let cap = max.clamp(1, 1 << 24) as usize;
        match with_file_cursor(h, "File::read", move |file| {
            let mut buf = vec![0u8; cap];
            file.read(&mut buf).map(|n| {
                buf.truncate(n);
                buf
            })
        }) {
            Ok(Ok(buf)) => unsafe {
                gos_rt_result_new(0, super::encoding::bytes_to_gosvec(&buf) as i64)
            },
            Ok(Err(e)) => fs_io_err(&e, "File::read"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::read_to_string() -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_read_to_string(h: i64) -> i128 {
    ffi_entry!(0i128, {
        match with_file_cursor(h, "File::read_to_string", |file| {
            let mut text = String::new();
            file.read_to_string(&mut text).map(|_| text)
        }) {
            Ok(Ok(text)) => unsafe { gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::read_to_string"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::write(data: String) -> Result<i64, Error>`: the whole text
/// is written, so the answer is its byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_write(h: i64, data: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if data.is_null() {
            return fs_err("File::write: null data");
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_string(data) }.into_bytes();
        let len = bytes.len();
        match with_file_cursor(h, "File::write", move |file| file.write_all(&bytes)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, len as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::write"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::flush() -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_flush(h: i64) -> i128 {
    ffi_entry!(0i128, {
        match with_file_cursor(h, "File::flush", |file| std::io::Write::flush(file)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, "File::flush"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::close()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_close(h: i64) {
    ffi_entry!((), {
        if let Some(files) = FILE_HANDLES.write().as_mut() {
            files.remove(&h);
        }
        if let Some(locks) = HELD_LOCKS.lock().as_mut() {
            locks.remove(&h);
        }
    });
}

/// `os::write_file(path, contents) -> Result<(), IoError>` - Result
/// shape. Used when the call site chains `.map_err(...)` (askq's
/// `save_history`); the bool-returning `gos_rt_fs_write` would
/// trip cranelift's verifier when `.map_err` then tried to feed
/// the i8 result into a `*mut GosResult` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_write_file_result(
    path: *const c_char,
    contents: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() || contents.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("write_file: null arg".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let c = unsafe { crate::c_abi::gos_str_arg_bytes(contents) }.to_vec();
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-write-file", move || std::fs::write(p, c)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::write_file(path, bytes: &[u8]) -> Result<(), IoError>` - `Vec<u8>`
/// payload variant. The MIR dispatcher picks this helper over the
/// c-string-shaped one when the contents argument types as `Vec<u8>`
/// or `&[u8]`, so binary writes (e.g. saving a downloaded image)
/// preserve every byte including embedded NULs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_write_file_bytes_result(
    path: *const c_char,
    contents: *const crate::c_abi::vec::GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() || contents.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("write_file: null arg".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let v = unsafe { &*contents };
        // Width-aware payload extraction: an i64-stride Vec carrying
        // u8 values needs the low byte of each slot, not a flat
        // memcpy. `elem_bytes == 1` is the proper Vec<u8> case.
        let bytes_buf: Vec<u8>;
        let bytes: &[u8] = if v.ptr.is_null() || v.len == 0 {
            &[]
        } else if v.elem_bytes == 1 {
            unsafe { std::slice::from_raw_parts(v.ptr.as_ptr(), v.len as usize) }
        } else if v.elem_bytes == 8 {
            let count = v.len as usize;
            bytes_buf = (0..count)
                .map(|i| {
                    let slot_ptr = unsafe { v.ptr.add(i * 8) };
                    unsafe { *slot_ptr }
                })
                .collect();
            &bytes_buf
        } else {
            unsafe {
                std::slice::from_raw_parts(v.ptr.as_ptr(), v.len as usize * v.elem_bytes as usize)
            }
        };
        let context = p.clone();
        let bytes = bytes.to_vec();
        match crate::sched_global::run_blocking("fs-write-bytes", move || std::fs::write(p, bytes))
        {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::read_file(path) -> Result<Vec<u8>, IoError>` - bytes-shaped
/// read counterpart to `gos_rt_fs_read_to_string`. The Ok payload
/// is a `*mut GosVec` with elem_bytes=1 holding the raw file
/// content (preserves embedded NULs / non-UTF-8 bytes). Callers
/// who want a String should use `os::read_file_to_string` /
/// `fs::read_to_string` instead.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_read_bytes_result(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("read_file: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match read_file_into_vec(p) {
            Ok(Ok(v)) => unsafe { gos_rt_result_new(0, v as i64) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// Reads the file at `p` into a fresh byte Vec sized from its metadata, so
/// the bytes are written once, into the buffer the program holds.
///
/// The syscalls run on the blocking pool and the Vec is allocated on the
/// calling goroutine. A file that grows between the size query and the read
/// has its tail appended; one that shrinks leaves the Vec at what was read.
fn read_file_into_vec(p: String) -> Result<std::io::Result<*mut GosVec>, String> {
    let opened = crate::sched_global::run_blocking("fs-read-bytes", move || {
        let file = std::fs::File::open(&p)?;
        let len = file.metadata().map_or(0, |m| m.len());
        Ok::<_, std::io::Error>((file, len))
    })?;
    let (mut file, len) = match opened {
        Ok(opened) => opened,
        Err(e) => return Ok(Err(e)),
    };
    let cap = i64::try_from(len).unwrap_or(i64::MAX);
    let v = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(1, cap) };
    // SAFETY: `gos_rt_vec_with_capacity` answers a live header.
    let buf = SyncRawPtr::new(unsafe { (*v).ptr.as_ptr() });
    let room = if buf.as_ptr().is_null() {
        0
    } else {
        usize::try_from(cap).unwrap_or(0)
    };
    let read = crate::sched_global::run_blocking("fs-read-bytes", move || {
        let window: &mut [u8] = if room == 0 {
            &mut []
        } else {
            // SAFETY: the Vec owns `room` bytes at `buf`, and nothing else
            // reaches them while its goroutine waits on this closure. They
            // are zeroed before a slice is formed over them.
            unsafe {
                std::ptr::write_bytes(buf.as_ptr(), 0, room);
                std::slice::from_raw_parts_mut(buf.as_ptr(), room)
            }
        };
        let mut filled = 0;
        while filled < window.len() {
            match file.read(&mut window[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut tail = Vec::new();
        if filled == window.len() {
            file.read_to_end(&mut tail)?;
        }
        Ok((filled, tail))
    });
    let (filled, tail) = match read {
        Ok(Ok(read)) => read,
        Ok(Err(e)) => {
            unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
            return Ok(Err(e));
        }
        Err(e) => {
            unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
            return Err(e);
        }
    };
    let filled_len = i64::try_from(filled).unwrap_or(0);
    // SAFETY: `v` is the live header allocated above, owned here alone.
    unsafe { (*v).len = filled_len };
    if !tail.is_empty() {
        let total = filled_len + i64::try_from(tail.len()).unwrap_or(0);
        unsafe { crate::c_abi::vec::gos_rt_vec_reserve_exact(v, total) };
        // SAFETY: the reserve left room for `tail` past `len`.
        let vref = unsafe { &mut *v };
        unsafe {
            std::ptr::copy_nonoverlapping(tail.as_ptr(), vref.ptr.as_ptr().add(filled), tail.len());
        }
        vref.len = total;
    }
    Ok(Ok(v))
}

/// `os::mkdir_all(path) -> Result<(), IoError>` - Result shape, for
/// `.map_err(...)` chains.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_mkdir_all_result(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("mkdir_all: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-mkdir-all", move || std::fs::create_dir_all(p))
        {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::remove_all(path) -> Result<(), IoError>` - removes a directory tree or file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_remove_dir_all_result(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_all: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-dir-all", move || {
            std::fs::remove_dir_all(p)
        }) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::create_dir(path) -> Result<(), Error>` - non-recursive
/// directory creation; fails when a parent component is missing.
/// Use `fs::create_dir_all` for the recursive form. Matches the
/// interp's `fs::create_dir` builtin (`std::fs::create_dir`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_create_dir(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("create_dir: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-create-dir", move || std::fs::create_dir(p)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::remove_dir(path) -> Result<(), Error>` - removes a single
/// empty directory; fails if it is non-empty. Use `fs::remove_dir_all`
/// / `fs::remove_all` for a recursive tree removal. Matches the
/// interp's `fs::remove_dir` / `os::remove_dir` builtin
/// (`std::fs::remove_dir`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_remove_dir(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_dir: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-dir", move || std::fs::remove_dir(p)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::remove_file(path) -> Result<(), IoError>` - Result shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_remove_file_result(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_file: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-file", move || std::fs::remove_file(p)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::rename(from, to)` / `fs::rename(from, to)` -> Result<(), Error>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_rename(from: *const c_char, to: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if from.is_null() || to.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("rename: null path".as_bytes());
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let f = unsafe { crate::c_abi::gos_str_arg_string(from) };
        let t = unsafe { crate::c_abi::gos_str_arg_string(to) };
        let context = f.clone();
        match crate::sched_global::run_blocking("fs-rename", move || std::fs::rename(f, t)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `env::set_current_dir(path) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_env_set_current_dir(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let p = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        let context = p.clone();
        match crate::sched_global::run_blocking("env-set-current-dir", move || {
            std::env::set_current_dir(p)
        }) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::arch() -> String` - target CPU architecture (e.g. `"x86_64"`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_arch() -> *const c_char {
    alloc_cstring(std::env::consts::ARCH.as_bytes()).cast_const()
}

/// `os::family() -> String` - target OS family (e.g. `"unix"`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_family() -> *const c_char {
    alloc_cstring(std::env::consts::FAMILY.as_bytes()).cast_const()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_join(a: *const c_char, b: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let a = if a.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(a) }
        };
        let b = if b.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(b) }
        };
        alloc_cstring(path_join(a, b).as_bytes())
    })
}

/// Final path component helper for `path::file_name`.
/// Inlined here so the runtime
/// crate stays free of a dep on `gossamer-std`.
/// `true` when `c` separates path components on `windows` hosts. Windows
/// accepts both forms and its APIs hand back `\\`, so parsing splits on
/// either there; elsewhere `\\` is an ordinary filename byte and stays
/// literal. Mirrors `gossamer_std::path::is_separator_on`.
pub(crate) const fn path_is_separator_on(c: char, windows: bool) -> bool {
    c == '/' || (windows && c == '\\')
}

/// [`path_is_separator_on`] for the host this build targets.
pub(crate) const fn path_is_separator(c: char) -> bool {
    path_is_separator_on(c, cfg!(windows))
}

/// Index of the last separator in `path` under `windows` rules. Exposed
/// separately so the Windows grammar is exercised from any host.
pub(crate) fn path_last_separator_on(path: &str, windows: bool) -> Option<usize> {
    path.rfind(|c| path_is_separator_on(c, windows))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_base(p: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let basename: &str = match path_last_separator_on(s, cfg!(windows)) {
            None => s,
            Some(idx) => &s[idx + 1..],
        };
        alloc_cstring(basename.as_bytes())
    })
}

/// Parent directory helper for `path::parent`.
/// Returns `"."` when no separator is present.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_dir(p: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let dirname: &str = match path_last_separator_on(s, cfg!(windows)) {
            None => ".",
            Some(0) => "/",
            Some(idx) => &s[..idx],
        };
        alloc_cstring(dirname.as_bytes())
    })
}

/// `path::split(p) -> (String, String)` - splits into a
/// `(directory, file)` pair. Returns a heap `*mut StrPair` (two
/// c-string slots); the MIR types the return as `(String, String)`
/// so a destructure reads slot 0 (dir) and slot 1 (file). The
/// directory carries no trailing separator unless the path is `/`,
/// matching `gossamer_std::path::split`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_split(p: *const c_char) -> *mut i64 {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let (dir, file): (&str, &str) = match path_last_separator_on(s, cfg!(windows)) {
            None => ("", s),
            Some(0) => ("/", &s[1..]),
            Some(idx) => (&s[..idx], &s[idx + 1..]),
        };
        #[repr(C)]
        struct StrPair {
            a: i64,
            b: i64,
        }
        Box::into_raw(Box::new(StrPair {
            a: alloc_cstring(dir.as_bytes()) as i64,
            b: alloc_cstring(file.as_bytes()) as i64,
        }))
        .cast()
    })
}

/// `path::components(p) -> Vec<String>` - Rust-like lexical components.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_components(p: *const c_char) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&path_components(s))
    })
}

/// `path::prefixes(p) -> Vec<String>` - cumulative Rust-like lexical prefixes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_prefixes(p: *const c_char) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&path_prefixes(s))
    })
}

/// `path::unique_prefixes(text) -> Vec<String>` - sorted unique cumulative
/// prefixes for newline-delimited paths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_unique_prefixes(p: *const c_char) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&path_unique_prefixes(s))
    })
}

/// `path::extension(p) -> Option<String>` - extension with the leading
/// dot wrapped in `Some`, or `None` if absent / the dot is at the
/// very start of the file name. Mirrors the interp / stdlib
/// `path::extension` Option-returning shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_ext(p: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let basename: &str = match path_last_separator_on(s, cfg!(windows)) {
            None => s,
            Some(idx) => &s[idx + 1..],
        };
        let ext_str: &str = match basename.rfind('.') {
            None | Some(0) => "",
            Some(idx) => &basename[idx..],
        };
        if ext_str.is_empty() {
            unsafe { gos_rt_result_new(1, 0) }
        } else {
            let cstr = alloc_cstring(ext_str.as_bytes()) as i64;
            unsafe { gos_rt_result_new(0, cstr) }
        }
    })
}

/// `path::parent(p) -> Option<String>` - drops the trailing
/// component. Returns None when `p` has no parent (root or
/// single-component path). Mirrors `gossamer_std::path::parent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_parent(p: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let trimmed = s.trim_end_matches(path_is_separator);
        match path_last_separator_on(trimmed, cfg!(windows)) {
            None => unsafe { gos_rt_result_new(1, 0) },
            Some(0) => {
                let cstr = alloc_cstring(b"/") as i64;
                unsafe { gos_rt_result_new(0, cstr) }
            }
            Some(idx) => {
                let cstr = alloc_cstring(&trimmed.as_bytes()[..idx]) as i64;
                unsafe { gos_rt_result_new(0, cstr) }
            }
        }
    })
}

/// `path::file_stem(p) -> Option<String>` - basename minus the
/// extension. Returns None when the basename is empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_stem(p: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let basename = match path_last_separator_on(s, cfg!(windows)) {
            None => s,
            Some(idx) => &s[idx + 1..],
        };
        if basename.is_empty() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let stem = match basename.rfind('.') {
            None | Some(0) => basename,
            Some(idx) => &basename[..idx],
        };
        let cstr = alloc_cstring(stem.as_bytes()) as i64;
        unsafe { gos_rt_result_new(0, cstr) }
    })
}

/// `path::file_name(p) -> Option<String>` - last component.
/// Returns None for empty paths or trailing-slash directories.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_file_name(p: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let s = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let basename = match path_last_separator_on(s, cfg!(windows)) {
            None => s,
            Some(idx) => &s[idx + 1..],
        };
        if basename.is_empty() {
            unsafe { gos_rt_result_new(1, 0) }
        } else {
            let cstr = alloc_cstring(basename.as_bytes()) as i64;
            unsafe { gos_rt_result_new(0, cstr) }
        }
    })
}

/// `path::normalize(p) -> String`. Lexical
/// cleanup mirroring `gossamer_std::path::normalize` (no I/O); the
/// runtime crate stays free of a dep on `gossamer-std`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_clean(p: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let path = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_cstring(path_clean(path).as_bytes())
    })
}

/// `path::is_absolute(p) -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_is_absolute(p: *const c_char) -> i32 {
    ffi_entry!(-1, {
        let path = if p.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        i32::from(path.starts_with(path_is_separator))
    })
}

/// `path::starts_with(p, prefix) -> bool` - path-aware prefix test.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_has_prefix(p: *const c_char, prefix: *const c_char) -> i32 {
    ffi_entry!(-1, {
        let path = if p.is_null() {
            String::new()
        } else {
            path_clean(unsafe { crate::c_abi::gos_str_arg_text(p) })
        };
        let prefix = if prefix.is_null() {
            String::new()
        } else {
            path_clean(unsafe { crate::c_abi::gos_str_arg_text(prefix) })
        };
        if path == prefix {
            return 1;
        }
        let matched = if prefix.ends_with('/') {
            path.starts_with(&prefix)
        } else {
            let mut candidate = prefix.clone();
            candidate.push('/');
            path.starts_with(&candidate)
        };
        i32::from(matched)
    })
}

/// `os::copy(src, dst) / fs::copy(src, dst) -> Result<i64, Error>`
/// - copies the file contents and returns the byte count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_copy(src: *const c_char, dst: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let src = if src.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(src) }
        };
        let dst = if dst.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(dst) }
        };
        let context = format!("{src} -> {dst}");
        match crate::sched_global::run_blocking("fs-copy", move || std::fs::copy(src, dst)) {
            Ok(Ok(n)) => unsafe { gos_rt_result_new(0, i64::try_from(n).unwrap_or(i64::MAX)) },
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::canonicalize(p) / fs::canonicalize(p) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_canonicalize(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let p = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-canonicalize", move || std::fs::canonicalize(p))
        {
            Ok(Ok(abs)) => {
                let s = abs.to_string_lossy().into_owned();
                let ptr = alloc_cstring(s.as_bytes()) as i64;
                unsafe { gos_rt_result_new(0, ptr) }
            }
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// Builds a `Result::Ok(*mut GosVec)` carrying owned strings.
/// STRING-typed: the vec owns each element, so `gos_rt_vec_free`
/// deep-frees them.
fn ok_str_vec(parts: &[String]) -> i128 {
    let vec = alloc_plain_str_vec(parts);
    unsafe { gos_rt_result_new(0, vec as i64) }
}

fn alloc_plain_str_vec(parts: &[String]) -> *mut GosVec {
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

fn err_io(e: &std::io::Error) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(format!("{e}").as_bytes());
    unsafe { gos_rt_result_new(1, err as i64) }
}

/// `bufio::read_to_string(path) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_read_to_string(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let p = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        match crate::sched_global::run_blocking("bufio-read-string", move || {
            std::fs::read_to_string(p)
        }) {
            Ok(Ok(text)) => unsafe { gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64) },
            Ok(Err(e)) => err_io(&e),
            Err(e) => fs_err(&e),
        }
    })
}

/// `bufio::read_lines_of(path) -> Result<[String], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_read_lines_of(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let p = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        match crate::sched_global::run_blocking("bufio-read-lines", move || {
            std::fs::read_to_string(p)
        }) {
            Ok(Ok(text)) => {
                let lines: Vec<String> = text.lines().map(str::to_string).collect();
                ok_str_vec(&lines)
            }
            Ok(Err(e)) => err_io(&e),
            Err(e) => fs_err(&e),
        }
    })
}

/// `net::resolve(host) / net::lookup(host) -> Result<[String], Error>`
/// - resolves a host (optionally `host:port`) to IP address strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_net_resolve(host: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        use std::net::ToSocketAddrs;
        let h = if host.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(host) }.to_string()
        };
        let needle = if h.contains(':') { h } else { format!("{h}:0") };
        match crate::sched_global::run_blocking("net-resolve", move || {
            needle
                .to_socket_addrs()
                .map(|addrs| addrs.map(|a| a.ip().to_string()).collect::<Vec<_>>())
        }) {
            Ok(Ok(ips)) => ok_str_vec(&ips),
            Ok(Err(e)) => err_io(&e),
            Err(e) => fs_err(&e),
        }
    })
}

/// Posix path join mirroring `gossamer_std::path::join`: collapses a
/// trailing separator on `base` and a leading separator on `segment`
/// to a single `/`, absorbs an absolute `segment`, and is independent
/// of the host separator so the compiled tier matches the VM on every
/// platform.
fn path_join(base: &str, segment: &str) -> String {
    if segment.starts_with(path_is_separator) {
        return segment.to_string();
    }
    if base.is_empty() {
        return segment.to_string();
    }
    let mut out = base.trim_end_matches(path_is_separator).to_string();
    out.push('/');
    out.push_str(segment.trim_start_matches(path_is_separator));
    out
}

fn path_components(path: &str) -> Vec<String> {
    let absolute = path.starts_with(path_is_separator);
    let mut out = Vec::new();
    if absolute {
        out.push("/".to_string());
    }
    let mut saw_normal = absolute;
    for segment in path.split(path_is_separator) {
        match segment {
            "" => {}
            "." if !saw_normal && !absolute => {
                out.push(".".to_string());
                saw_normal = true;
            }
            "." => {}
            other => {
                out.push(other.to_string());
                saw_normal = true;
            }
        }
    }
    out
}

fn path_prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    extend_path_prefixes(path, &mut out);
    out
}

fn path_unique_prefixes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        extend_path_prefixes(line, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn extend_path_prefixes(path: &str, out: &mut Vec<String>) {
    let absolute = path.starts_with(path_is_separator);
    let mut prefix = String::with_capacity(path.len());
    if absolute {
        prefix.push('/');
        out.push(prefix.clone());
    }
    let mut saw_normal = absolute;
    for segment in path.split(path_is_separator) {
        match segment {
            "" => {}
            "." if !saw_normal && !absolute => {
                prefix.push('.');
                out.push(prefix.clone());
                saw_normal = true;
            }
            "." => {}
            other => {
                if prefix.is_empty() || prefix == "/" {
                    prefix.push_str(other);
                } else {
                    prefix.push('/');
                    prefix.push_str(other);
                }
                out.push(prefix.clone());
                saw_normal = true;
            }
        }
    }
}

/// Lexical path normalization shared by `gos_rt_path_clean` /
/// `gos_rt_path_has_prefix`. Mirrors `gossamer_std::path::normalize`.
fn path_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with(path_is_separator);
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split(path_is_separator) {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|s: &&str| *s != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    out.push_str(&parts.join("/"));
    if out.is_empty() { ".".to_string() } else { out }
}

// --- 0.7.0 scalar cmp helpers ---

/// Scalar `min(a, b)` for i64. Pair-shape used by the prelude
/// `min(a, b)` dispatch in compiled mode.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_min_i64(a: i64, b: i64) -> i64 {
    a.min(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_max_i64(a: i64, b: i64) -> i64 {
    a.max(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_clamp_i64(x: i64, lo: i64, hi: i64) -> i64 {
    x.clamp(lo, hi)
}

/// Scalar `min(a, b)` for `u64` / `usize`, whose words order unsigned.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_min_u64(a: u64, b: u64) -> u64 {
    a.min(b)
}

/// Scalar `max(a, b)` for `u64` / `usize`, whose words order unsigned.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_max_u64(a: u64, b: u64) -> u64 {
    a.max(b)
}

/// Scalar `clamp(x, lo, hi)` for `u64` / `usize`, whose words order unsigned.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_clamp_u64(x: u64, lo: u64, hi: u64) -> u64 {
    x.clamp(lo, hi)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_min_f64(a: f64, b: f64) -> f64 {
    a.min(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_max_f64(a: f64, b: f64) -> f64 {
    a.max(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_clamp_f64(x: f64, lo: f64, hi: f64) -> f64 {
    x.clamp(lo, hi)
}

// ---------------------------------------------------------------
// path::matches / path::glob - Go `filepath.Match` / `filepath.Glob`
// semantics. The matcher is byte-oriented and single-segment: `*`
// and `?` never cross a `/`. `glob` walks the filesystem one pattern
// segment at a time, with `**` meaning "this directory and every
// descendant directory". Results are sorted so every tier reports
// the same order. Mirrors `gossamer_std::path::{matches, glob}`.
// ---------------------------------------------------------------

/// Single-segment shell-glob match over raw bytes.
fn path_glob_matches(pat: &[u8], name: &[u8]) -> bool {
    let mut pi = 0;
    let mut ni = 0;
    let mut star_pat: Option<usize> = None;
    let mut star_name: usize = 0;
    while ni < name.len() {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star_pat = Some(pi);
                    star_name = ni;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    if name[ni] == b'/' {
                        return false;
                    }
                    pi += 1;
                    ni += 1;
                    continue;
                }
                b'[' => {
                    let close = match pat[pi + 1..].iter().position(|&b| b == b']') {
                        Some(p) => pi + 1 + p,
                        None => return false,
                    };
                    let class = &pat[pi + 1..close];
                    if class.contains(&name[ni]) {
                        pi = close + 1;
                        ni += 1;
                        continue;
                    }
                }
                lit if lit == name[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pat
            && name[star_name] != b'/'
        {
            pi = sp + 1;
            star_name += 1;
            ni = star_name;
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// `\` is a path separator on Windows only; elsewhere it is an
/// ordinary filename byte and must stay literal.
fn path_glob_normalise(pattern: &str) -> String {
    #[cfg(windows)]
    {
        pattern.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        pattern.to_string()
    }
}

/// Starting directory built from the literal segments preceding the
/// first glob metacharacter, so drive letters and UNC shares resolve
/// through `std::path` rather than a synthetic root walk.
fn path_glob_base(prefix_segments: &[&str]) -> std::path::PathBuf {
    if prefix_segments.is_empty() {
        return std::path::PathBuf::from(".");
    }
    let joined = prefix_segments.join("/");
    if joined.is_empty() {
        return std::path::PathBuf::from("/");
    }
    #[cfg(windows)]
    {
        let bytes = joined.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return std::path::PathBuf::from(format!("{joined}/"));
        }
    }
    std::path::PathBuf::from(joined)
}

fn path_glob_expand(pattern: &str) -> std::io::Result<Vec<String>> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let normalised = path_glob_normalise(pattern);
    let segments: Vec<&str> = normalised.split('/').collect();
    let split_idx = segments
        .iter()
        .position(|s| s.contains('*') || s.contains('?') || s.contains('['))
        .unwrap_or(segments.len());
    let base = path_glob_base(&segments[..split_idx]);
    let glob_segments: Vec<&str> = segments[split_idx..]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect();
    if glob_segments.is_empty() {
        return Ok(match base.to_str() {
            Some(s) if base.exists() => vec![s.to_string()],
            _ => Vec::new(),
        });
    }
    let mut frontier: Vec<std::path::PathBuf> = vec![base];
    for seg in &glob_segments {
        let mut next: Vec<std::path::PathBuf> = Vec::new();
        for current in &frontier {
            if *seg == "**" {
                let mut bfs: Vec<std::path::PathBuf> = vec![current.clone()];
                while let Some(p) = bfs.pop() {
                    next.push(p.clone());
                    for entry in std::fs::read_dir(&p)? {
                        let path = entry?.path();
                        let metadata = std::fs::symlink_metadata(&path)?;
                        if metadata.is_dir() && !metadata.file_type().is_symlink() {
                            bfs.push(path);
                        }
                    }
                }
                continue;
            }
            for entry in std::fs::read_dir(current)? {
                let path = entry?.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if path_glob_matches(seg.as_bytes(), name.as_bytes()) {
                    next.push(path);
                }
            }
        }
        frontier = next;
    }
    let mut out: Vec<String> = frontier
        .into_iter()
        .filter_map(|p| p.to_str().map(str::to_string))
        .collect();
    out.sort();
    Ok(out)
}

/// `path::matches(pattern, name) -> bool` - Go `filepath.Match`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_matches(pattern: *const c_char, name: *const c_char) -> i64 {
    ffi_entry!(0, {
        let pat = if pattern.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(pattern) }
        };
        let n = if name.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(name) }
        };
        i64::from(path_glob_matches(pat.as_bytes(), n.as_bytes()))
    })
}

/// `path::glob(pattern) -> Result<Vec<String>, errors::Error>` - sorted
/// matching paths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_glob(pattern: *const c_char) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let pat = if pattern.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(pattern) }
        };
        match path_glob_expand(pat) {
            Ok(paths) => ok_str_vec(&paths),
            Err(e) => err_io(&e),
        }
    })
}

// ---------------------------------------------------------------
// Positional I/O, durability barriers, and advisory range locks
// ---------------------------------------------------------------
//
// Every operation below reaches the handle's own descriptor through
// its registry `Arc` rather than a `try_clone`. POSIX record locks are
// released when *any* descriptor for the file is closed by the
// process, so a handle owns exactly one descriptor for its lifetime
// and a lock taken through it stays held across later reads and
// writes.

/// Run `op` on the handle's file in the blocking pool, or answer the
/// stale-handle error when the handle has already been closed.
/// Runs `op` on the file behind `h` in place, holding its cursor lock: the
/// call moves or depends on the file cursor. The worker is marked as inside a
/// system call, so a call that runs long hands its other goroutines to a
/// fresh worker.
fn with_file_cursor<T, F>(h: i64, context: &str, op: F) -> Result<T, i128>
where
    F: FnOnce(&mut &std::fs::File) -> T,
{
    let Some(entry) = file_clone(h) else {
        return Err(fs_err(&format!("{context}: stale handle")));
    };
    let _cursor = entry.cursor.lock();
    let _syscall = crate::sched_global::syscall_enter();
    Ok(op(&mut &entry.file))
}

/// [`with_file_cursor`] for a positional call. On Unix it reads and writes
/// at an offset without touching the cursor, so it takes no lock and
/// positional calls on one file run in parallel.
fn with_file_positional<T, F>(h: i64, context: &str, op: F) -> Result<T, i128>
where
    F: FnOnce(&mut &std::fs::File) -> T,
{
    let Some(entry) = file_clone(h) else {
        return Err(fs_err(&format!("{context}: stale handle")));
    };
    // Win32 positional calls move the cursor, so they take its lock.
    #[cfg(windows)]
    let _cursor = entry.cursor.lock();
    let _syscall = crate::sched_global::syscall_enter();
    Ok(op(&mut &entry.file))
}

/// `fs::File::read_at(len, offset) -> Result<Vec<u8>, Error>`. The answer
/// is what the one positional read transferred, which may be shorter than
/// `len` at end of file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_read_at(h: i64, len: i64, offset: i64) -> i128 {
    ffi_entry!(0i128, {
        if len < 0 || offset < 0 {
            return fs_err("File::read_at: length and offset must be non-negative");
        }
        let cap = len.min(1 << 24) as usize;
        let offset = offset as u64;
        let out = unsafe { super::vec::gos_rt_vec_with_capacity(1, cap as i64) };
        let read = with_file_positional(h, "File::read_at", |file| unsafe {
            read_at_into_vec(file, out, cap, offset)
        });
        match read {
            Ok(Ok(_)) => unsafe { gos_rt_result_new(0, out as i64) },
            Ok(Err(e)) => {
                unsafe { super::map::gos_rt_vec_free(out) };
                fs_io_err(&e, "File::read_at")
            }
            Err(packed) => {
                unsafe { super::map::gos_rt_vec_free(out) };
                packed
            }
        }
    })
}

/// `fs::File::read_at_into(buf, len, offset) -> Result<i64, Error>`: one
/// positional read of up to `len` bytes into `buf`, reusing its storage.
/// `buf` holds exactly the bytes read afterwards, and the answer is their
/// count, short at end of file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_read_at_into(
    h: i64,
    buf: *mut super::vec::GosVec,
    len: i64,
    offset: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if len < 0 || offset < 0 {
            return fs_err("File::read_at_into: length and offset must be non-negative");
        }
        if buf.is_null() || unsafe { (*buf).elem_bytes } != 1 {
            return fs_err("File::read_at_into: the buffer must be a Vec<u8>");
        }
        let cap = len.min(1 << 24) as usize;
        let offset = offset as u64;
        let read = with_file_positional(h, "File::read_at_into", |file| unsafe {
            read_at_into_vec(file, buf, cap, offset)
        });
        match read {
            Ok(Ok(n)) => unsafe { gos_rt_result_new(0, n as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::read_at_into"),
            Err(packed) => packed,
        }
    })
}

/// One positional read of up to `len` bytes at `offset` straight into the
/// storage of the byte vector `v`, which is grown to hold them. `v` holds
/// exactly the bytes read afterwards.
///
/// # Safety
/// `v` is a live `GosVec` of one-byte elements that nothing else is reading.
unsafe fn read_at_into_vec(
    file: &std::fs::File,
    v: *mut super::vec::GosVec,
    len: usize,
    offset: u64,
) -> std::io::Result<usize> {
    unsafe { super::vec::gos_rt_vec_reserve_exact(v, len as i64) };
    let vec = unsafe { &mut *v };
    vec.len = 0;
    if len == 0 {
        return Ok(0);
    }
    let dst = vec.ptr.as_ptr();
    // `pread` writes through the raw pointer, so the fresh capacity needs no
    // initialising first.
    #[cfg(unix)]
    let read = loop {
        use std::os::fd::AsRawFd as _;
        // SAFETY: `dst` addresses `len` writable bytes of `v`'s capacity.
        let n = unsafe { libc::pread(file.as_raw_fd(), dst.cast(), len, offset as libc::off_t) };
        if n >= 0 {
            break n as usize;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    // A positional read takes a slice here, which the capacity must be
    // initialised to form.
    #[cfg(not(unix))]
    let read = {
        // SAFETY: `dst` addresses `len` writable bytes of `v`'s capacity.
        unsafe { std::ptr::write_bytes(dst, 0, len) };
        let slice = unsafe { std::slice::from_raw_parts_mut(dst, len) };
        read_at_offset(file, slice, offset)?
    };
    vec.len = read as i64;
    Ok(read)
}

/// `fs::File::write_at(data, offset) -> Result<i64, Error>`. Answers the
/// byte count the one positional write transferred; a short write is
/// reported rather than looped over.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_write_at(
    h: i64,
    data: *const crate::c_abi::vec::GosVec,
    offset: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if offset < 0 {
            return fs_err("File::write_at: offset must be non-negative");
        }
        let bytes = unsafe { super::encoding::gosvec_u8(data) };
        let at = offset as u64;
        let written = with_file_positional(h, "File::write_at", move |file| {
            write_at_offset(file, &bytes, at)
        });
        match written {
            Ok(Ok(n)) => unsafe { gos_rt_result_new(0, n as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::write_at"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::write_bytes(data) -> Result<i64, Error>`: the byte-oriented
/// write against the handle's own cursor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_write_bytes(
    h: i64,
    data: *const crate::c_abi::vec::GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        let bytes = unsafe { super::encoding::gosvec_u8(data) };
        let written = with_file_cursor(h, "File::write_bytes", move |f| f.write(&bytes));
        match written {
            Ok(Ok(n)) => unsafe { gos_rt_result_new(0, n as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::write_bytes"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::seek(offset, whence) -> Result<i64, Error>`, answering the
/// new absolute position. `whence` is one of `fs::SEEK_SET`,
/// `fs::SEEK_CUR`, `fs::SEEK_END`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_seek(h: i64, offset: i64, whence: i64) -> i128 {
    ffi_entry!(0i128, {
        let from = match whence {
            0 => std::io::SeekFrom::Start(offset.max(0) as u64),
            1 => std::io::SeekFrom::Current(offset),
            2 => std::io::SeekFrom::End(offset),
            _ => return fs_err("File::seek: whence must be SEEK_SET, SEEK_CUR, or SEEK_END"),
        };
        match with_file_cursor(h, "File::seek", move |file| std::io::Seek::seek(file, from)) {
            Ok(Ok(pos)) => unsafe { gos_rt_result_new(0, pos as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::seek"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::set_len(len) -> Result<(), Error>`: truncate or extend.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_set_len(h: i64, len: i64) -> i128 {
    ffi_entry!(0i128, {
        if len < 0 {
            return fs_err("File::set_len: length must be non-negative");
        }
        let len = len as u64;
        match with_file_cursor(h, "File::set_len", move |file| file.set_len(len)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, "File::set_len"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::len() -> Result<i64, Error>`: the open file's current size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_len(h: i64) -> i128 {
    ffi_entry!(0i128, {
        match with_file_cursor(h, "File::len", |file| file.metadata().map(|m| m.len())) {
            Ok(Ok(len)) => unsafe { gos_rt_result_new(0, len as i64) },
            Ok(Err(e)) => fs_io_err(&e, "File::len"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::sync_all() -> Result<(), Error>`: flush the file's data and
/// metadata to the storage device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_sync_all(h: i64) -> i128 {
    ffi_entry!(0i128, {
        match with_file_cursor(h, "File::sync_all", |file| file.sync_all()) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, "File::sync_all"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::sync_data() -> Result<(), Error>`: flush the file's data,
/// leaving metadata the platform considers inessential unwritten.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_sync_data(h: i64) -> i128 {
    ffi_entry!(0i128, {
        match with_file_cursor(h, "File::sync_data", |file| file.sync_data()) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, "File::sync_data"),
            Err(packed) => packed,
        }
    })
}

/// `fs::sync_dir(path) -> Result<(), Error>`: make a directory's own
/// entries durable, the barrier a rename or unlink needs after its own
/// file sync.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_sync_dir(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("fs::sync_dir: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-sync-dir", move || sync_directory(&p)) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::try_lock_range(start, len, exclusive) -> Result<bool, Error>`.
/// `len` of 0 covers the range from `start` to end of file, however the
/// file later grows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_try_lock_range(
    h: i64,
    start: i64,
    len: i64,
    exclusive: i32,
) -> i128 {
    ffi_entry!(0i128, {
        if start < 0 || len < 0 {
            return fs_err("File::try_lock_range: start and len must be non-negative");
        }
        let exclusive = exclusive != 0;
        match with_file_cursor(h, "File::try_lock_range", move |file| {
            try_lock_range_on(file, start as u64, len as u64, exclusive)
        }) {
            Ok(Ok(acquired)) => {
                if acquired {
                    record_held_range(h, start as u64, len as u64);
                }
                unsafe { gos_rt_result_new(0, i64::from(acquired)) }
            }
            Ok(Err(e)) => fs_io_err(&e, "File::try_lock_range"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::unlock_range(start, len) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_unlock_range(h: i64, start: i64, len: i64) -> i128 {
    ffi_entry!(0i128, {
        if start < 0 || len < 0 {
            return fs_err("File::unlock_range: start and len must be non-negative");
        }
        match with_file_cursor(h, "File::unlock_range", move |file| {
            unlock_range_on(file, start as u64, len as u64)
        }) {
            Ok(Ok(())) => {
                forget_held_range(h, start as u64, len as u64);
                unsafe { gos_rt_result_new(0, 0) }
            }
            Ok(Err(e)) => fs_io_err(&e, "File::unlock_range"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::try_lock_shared() -> Result<bool, Error>`: the whole file as
/// one shared range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_try_lock_shared(h: i64) -> i128 {
    unsafe { gos_rt_fs_file_try_lock_range(h, 0, 0, 0) }
}

/// `fs::File::try_lock_exclusive() -> Result<bool, Error>`: the whole file
/// as one exclusive range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_try_lock_exclusive(h: i64) -> i128 {
    unsafe { gos_rt_fs_file_try_lock_range(h, 0, 0, 1) }
}

/// `fs::File::unlock() -> Result<(), Error>`: release every range this
/// handle holds, whole-file or otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_unlock(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let held = HELD_LOCKS
            .lock()
            .as_mut()
            .and_then(|locks| locks.remove(&h))
            .unwrap_or_default();
        if held.is_empty() {
            return unsafe { gos_rt_result_new(0, 0) };
        }
        match with_file_cursor(h, "File::unlock", move |file| {
            held.into_iter()
                .try_for_each(|(start, len)| unlock_range_on(file, start, len))
        }) {
            Ok(Ok(())) => unsafe { gos_rt_result_new(0, 0) },
            Ok(Err(e)) => fs_io_err(&e, "File::unlock"),
            Err(packed) => packed,
        }
    })
}

/// Note a range this handle now holds, so `File::unlock` can name it.
fn record_held_range(h: i64, start: u64, len: u64) {
    let mut guard = HELD_LOCKS.lock();
    let held = guard.get_or_insert_with(HashMap::new).entry(h).or_default();
    if !held.contains(&(start, len)) {
        held.push((start, len));
    }
}

/// Drop one range from the handle's held set once it has been released.
fn forget_held_range(h: i64, start: u64, len: u64) {
    if let Some(held) = HELD_LOCKS
        .lock()
        .as_mut()
        .and_then(|locks| locks.get_mut(&h))
        && let Some(at) = held.iter().position(|range| *range == (start, len))
    {
        held.swap_remove(at);
    }
}

/// One positional read at `offset`, independent of the file cursor.
#[cfg(unix)]
pub fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

/// One positional read at `offset`, independent of the file cursor.
///
/// A synchronous Win32 handle carries the transfer offset in the OVERLAPPED
/// structure and advances the file pointer past the bytes moved, so the
/// cursor is restored here to keep the call positional on every platform.
#[cfg(windows)]
pub fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Seek, SeekFrom};
    let mut cursor = file;
    let resume = cursor.stream_position()?;
    let read = std::os::windows::fs::FileExt::seek_read(file, buf, offset);
    cursor.seek(SeekFrom::Start(resume))?;
    read
}

/// One positional write at `offset`, independent of the file cursor.
#[cfg(unix)]
pub fn write_at_offset(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

/// One positional write at `offset`, independent of the file cursor.
///
/// Cursor-restoring for the same reason as [`read_at_offset`].
#[cfg(windows)]
pub fn write_at_offset(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Seek, SeekFrom};
    let mut cursor = file;
    let resume = cursor.stream_position()?;
    let written = std::os::windows::fs::FileExt::seek_write(file, buf, offset);
    cursor.seek(SeekFrom::Start(resume))?;
    written
}

/// Make a directory's own entries durable.
#[cfg(not(windows))]
pub fn sync_directory(path: &str) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Windows keeps a directory's entries consistent through NTFS metadata
/// ordering and offers no handle a program can flush for one, so the
/// barrier is satisfied by the platform rather than by this call.
#[cfg(windows)]
pub fn sync_directory(path: &str) -> std::io::Result<()> {
    std::fs::metadata(path).map(|_| ())
}

/// One positional read at `offset` on a target with neither the POSIX nor
/// the Win32 positional call: the cursor is moved, the read taken, and the
/// cursor restored. Single-threaded by construction on those targets, so no
/// other holder observes the moved cursor.
#[cfg(not(any(unix, windows)))]
pub fn read_at_offset(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = file;
    let resume = file.stream_position()?;
    file.seek(SeekFrom::Start(offset))?;
    let read = file.read(buf);
    file.seek(SeekFrom::Start(resume))?;
    read
}

/// One positional write at `offset`, cursor-restoring like
/// [`read_at_offset`].
#[cfg(not(any(unix, windows)))]
pub fn write_at_offset(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file;
    let resume = file.stream_position()?;
    file.seek(SeekFrom::Start(offset))?;
    let written = file.write(buf);
    file.seek(SeekFrom::Start(resume))?;
    written
}

/// Advisory locks are the operating system's, and a target with neither the
/// POSIX nor the Win32 call has none to take. Answering `Ok(false)` would
/// claim another holder owns the range, so the absence is reported instead.
#[cfg(not(any(unix, windows)))]
pub fn try_lock_range_on(
    _file: &std::fs::File,
    _start: u64,
    _len: u64,
    _exclusive: bool,
) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "advisory file locks are unavailable on this target",
    ))
}

/// Companion to [`try_lock_range_on`] on a target with no advisory locks.
#[cfg(not(any(unix, windows)))]
pub fn unlock_range_on(_file: &std::fs::File, _start: u64, _len: u64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "advisory file locks are unavailable on this target",
    ))
}

/// POSIX record lock over `[start, start + len)`, or `[start, EOF)` when
/// `len` is zero. `Ok(false)` reports a conflicting holder; every other
/// failure is a real error.
#[cfg(unix)]
pub fn try_lock_range_on(
    file: &std::fs::File,
    start: u64,
    len: u64,
    exclusive: bool,
) -> std::io::Result<bool> {
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = if exclusive {
        libc::F_WRLCK as libc::c_short
    } else {
        libc::F_RDLCK as libc::c_short
    };
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start as libc::off_t;
    lock.l_len = len as libc::off_t;
    // SAFETY: `lock` is a fully initialised `flock` and `fd` is owned by
    // the borrowed `File` for the duration of the call.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &raw const lock) };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(unix)]
pub fn unlock_range_on(file: &std::fs::File, start: u64, len: u64) -> std::io::Result<()> {
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_UNLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start as libc::off_t;
    lock.l_len = len as libc::off_t;
    // SAFETY: as in `try_lock_range_on`.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &raw const lock) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `LockFileEx` over the same range, with `LOCKFILE_FAIL_IMMEDIATELY` so a
/// held lock answers `Ok(false)` instead of blocking the scheduler thread.
/// A zero `len` covers the whole 64-bit range, matching the POSIX
/// "to end of file" spelling.
#[cfg(windows)]
pub fn try_lock_range_on(
    file: &std::fs::File,
    start: u64,
    len: u64,
    exclusive: bool,
) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE;
    let span = if len == 0 { u64::MAX } else { len };
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = start as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (start >> 32) as u32;
    let mut flags = LOCKFILE_FAIL_IMMEDIATELY;
    if exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK;
    }
    // SAFETY: `handle` is owned by the borrowed `File` and `overlapped` is
    // a fully initialised structure that outlives the call.
    let ok = unsafe {
        LockFileEx(
            handle,
            flags,
            0,
            span as u32,
            (span >> 32) as u32,
            &raw mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
pub fn unlock_range_on(file: &std::fs::File, start: u64, len: u64) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE;
    let span = if len == 0 { u64::MAX } else { len };
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = start as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (start >> 32) as u32;
    // SAFETY: as in `try_lock_range_on`.
    let ok = unsafe {
        UnlockFileEx(
            handle,
            0,
            span as u32,
            (span >> 32) as u32,
            &raw mut overlapped,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Answers the `Result<(), errors::Error>` carrier for a filesystem
/// operation over `context`.
fn fs_unit_result(outcome: std::io::Result<()>, context: &str) -> i128 {
    match outcome {
        Ok(()) => unsafe { gos_rt_result_new(0, 0) },
        Err(e) => fs_io_err(&e, context),
    }
}

/// `fs::permissions(path) -> Result<i64, errors::Error>` - the
/// permission bits of `path`, in the chmod(2) encoding.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_permissions(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("permissions: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-permissions", move || {
            crate::fs_mode::read(std::path::Path::new(&p))
        }) {
            Ok(Ok(mode)) => unsafe { gos_rt_result_new(0, i64::from(mode)) },
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::set_permissions(path, mode) -> Result<(), errors::Error>` -
/// chmod(2). On Windows only the owner write bit is meaningful: it
/// sets or clears the read-only attribute.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_set_permissions(path: *const c_char, mode: i64) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("set_permissions: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        let bits = crate::fs_mode::bits(mode);
        let outcome = crate::sched_global::run_blocking("fs-set-permissions", move || {
            crate::fs_mode::apply(std::path::Path::new(&p), bits)
        });
        match outcome {
            Ok(result) => fs_unit_result(result, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::create_dir_mode(path, mode) -> Result<(), errors::Error>` -
/// creates one directory and gives it exactly `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_create_dir_mode(path: *const c_char, mode: i64) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("create_dir_mode: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        let bits = crate::fs_mode::bits(mode);
        let outcome = crate::sched_global::run_blocking("fs-create-dir-mode", move || {
            crate::fs_mode::create_dir(std::path::Path::new(&p), bits)
        });
        match outcome {
            Ok(result) => fs_unit_result(result, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::create_dir_all_mode(path, mode) -> Result<(), errors::Error>`
/// - creates `path` and every missing parent with exactly `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_create_dir_all_mode(path: *const c_char, mode: i64) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() {
            return fs_err("create_dir_all_mode: null path");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        let bits = crate::fs_mode::bits(mode);
        let outcome = crate::sched_global::run_blocking("fs-create-dir-all-mode", move || {
            crate::fs_mode::create_dir_all(std::path::Path::new(&p), bits)
        });
        match outcome {
            Ok(result) => fs_unit_result(result, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::write_mode(path, contents, mode) -> Result<(), errors::Error>`
/// - writes `contents` and gives the file exactly `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_write_mode(
    path: *const c_char,
    contents: *const c_char,
    mode: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if path.is_null() || contents.is_null() {
            return fs_err("write_mode: null argument");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(contents) }.to_vec();
        let context = p.clone();
        let bits = crate::fs_mode::bits(mode);
        let outcome = crate::sched_global::run_blocking("fs-write-mode", move || {
            crate::fs_mode::write(std::path::Path::new(&p), &bytes, bits)
        });
        match outcome {
            Ok(result) => fs_unit_result(result, &context),
            Err(e) => fs_err(&e),
        }
    })
}

#[cfg(test)]
mod path_separator_tests {
    use super::{path_is_separator_on, path_last_separator_on};

    #[test]
    fn windows_paths_end_at_either_separator() {
        let mixed = "C:\\tmp/gos-glob-1\\alpha.gos";
        let idx = path_last_separator_on(mixed, true).expect("separator present");
        assert_eq!(&mixed[idx + 1..], "alpha.gos");
        assert_eq!(&mixed[..idx], "C:\\tmp/gos-glob-1");
    }

    #[test]
    fn backslash_is_an_ordinary_byte_off_windows() {
        let unix = "/tmp/odd\\name.gos";
        let idx = path_last_separator_on(unix, false).expect("separator present");
        assert_eq!(&unix[idx + 1..], "odd\\name.gos");
        assert!(!path_is_separator_on('\\', false));
        assert!(path_is_separator_on('\\', true));
    }
}
