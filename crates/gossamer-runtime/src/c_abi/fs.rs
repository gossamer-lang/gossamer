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

use std::collections::HashMap;
use std::io::Read;
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::*;
use crate::file_lock::Span;
use crate::open_file::OpenFile;
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

// Lookups take the read side, so goroutines working on open files never
// serialise on the table; only open and close write it.
static FILE_HANDLES: parking_lot::RwLock<Option<HashMap<i64, Arc<OpenFile>>>> =
    parking_lot::RwLock::new(None);

static OPEN_OPTIONS_HANDLES: Mutex<Option<HashMap<i64, Arc<Mutex<GosOpenOptions>>>>> =
    Mutex::new(None);
static NEXT_FS_HANDLE: AtomicI64 = AtomicI64::new(1);

fn next_fs_handle() -> i64 {
    NEXT_FS_HANDLE.fetch_add(1, Ordering::Relaxed)
}

/// Registers `file` and answers a handle owning the entry: the file closes
/// with the handle's last share, or at an explicit `close`.
fn insert_file(file: std::fs::File) -> i64 {
    let id = next_fs_handle();
    FILE_HANDLES
        .write()
        .get_or_insert_with(HashMap::new)
        .insert(id, OpenFile::new(file));
    super::registry_key::key_handle(id, retire_file)
}

/// Removes the file registered under `id`; its locks go with its last
/// reference. A call still working on the file keeps it open until that call
/// returns.
fn retire_file(id: i64) {
    if let Some(files) = FILE_HANDLES.write().as_mut() {
        files.remove(&id);
    }
}

fn file_clone(h: i64) -> Option<Arc<OpenFile>> {
    let id = super::registry_key::key_id(h);
    FILE_HANDLES
        .read()
        .as_ref()
        .and_then(|m| m.get(&id).cloned())
}

fn insert_open_options(opts: GosOpenOptions) -> i64 {
    let id = next_fs_handle();
    OPEN_OPTIONS_HANDLES
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(id, Arc::new(Mutex::new(opts)));
    super::registry_key::key_handle(id, |id| {
        if let Some(options) = OPEN_OPTIONS_HANDLES.lock().as_mut() {
            options.remove(&id);
        }
    })
}

fn open_options_clone(h: i64) -> Option<Arc<Mutex<GosOpenOptions>>> {
    let id = super::registry_key::key_id(h);
    OPEN_OPTIONS_HANDLES
        .lock()
        .as_ref()
        .and_then(|m| m.get(&id).cloned())
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
    gos_rt_result_new(1, err as i64)
}

fn fs_io_err(err: &std::io::Error, context: &str) -> i128 {
    let msg = classify_io_error(err, context);
    fs_err(&msg)
}

/// # Safety
///
/// `prefix` is null or a live string body.
unsafe fn temp_prefix(prefix: *const c_char, operation: &str) -> Result<String, i128> {
    if prefix.is_null() {
        return Err(fs_err(&format!("{operation}: null prefix")));
    }
    // SAFETY: this `unsafe fn`'s caller passes `prefix` live; non-null, checked above.
    let prefix = unsafe { crate::c_abi::gos_str_arg_string(prefix) };
    if !is_temp_prefix(&prefix) {
        return Err(fs_err(TEMP_PREFIX_ERROR));
    }
    Ok(prefix)
}

const TEMP_PREFIX_ERROR: &str = "temporary-resource prefix must be a single path component";

/// Whether `prefix` stays one path component once the name is built from it,
/// on every host: a `:` makes `C:x` a drive-relative path on Windows, which
/// a join puts in place of the parent.
fn is_temp_prefix(prefix: &str) -> bool {
    !prefix.contains(['/', '\\', ':', '\0']) && !matches!(prefix, "." | "..")
}

/// Names tried before giving up, as Go's `os.CreateTemp` and Python's
/// `tempfile` bound their retries (10 000 each).
const TEMP_NAME_ATTEMPTS: usize = 10_000;

/// `prefix` followed by 64 bits from the OS random source. Nobody else on the
/// host can predict the name, so nobody can create it first to block the call
/// or plant a file under it.
#[cfg(not(target_arch = "wasm32"))]
fn temp_resource_name(prefix: &str) -> std::io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("OS random source: {e}")))?;
    Ok(format!("{prefix}-{:016x}", u64::from_le_bytes(bytes)))
}

/// wasm32 has neither a file system to make one in nor an OS random source.
#[cfg(target_arch = "wasm32")]
fn temp_resource_name(_prefix: &str) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "temporary files need a file system, which wasm32 has none of",
    ))
}

/// Creates a fresh temporary resource with `create`, trying new random names
/// while one is taken. Under `parent` the name is `prefix-...`, as Go's
/// `os.MkdirTemp` names it; with no parent it lands in the system temporary
/// root as `gossamer-prefix-...`.
fn create_temp_resource<T>(
    parent: Option<&std::path::Path>,
    prefix: &str,
    create: impl Fn(&std::path::Path) -> std::io::Result<T>,
) -> std::io::Result<(T, std::path::PathBuf)> {
    if !is_temp_prefix(prefix) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            TEMP_PREFIX_ERROR,
        ));
    }
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let name = temp_resource_name(prefix)?;
        let path = match parent {
            Some(parent) => parent.join(name),
            None => crate::platform::temp_dir().join(format!("gossamer-{name}")),
        };
        match create(&path) {
            Ok(made) => return Ok((made, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free temporary name after {TEMP_NAME_ATTEMPTS} attempts"),
    ))
}

/// Creates a fresh directory for `fs::temp_dir` and `fs::TempDir` and answers
/// its path. On Unix only the owner may list or enter it (mode 0700), as with
/// Go's `os.MkdirTemp` and Python's `mkdtemp`; on Windows the user's own
/// temporary folder already limits access to that user.
///
/// # Errors
///
/// `prefix` is not a single path component, or the directory could not be
/// created.
pub fn create_temp_dir(
    parent: Option<&std::path::Path>,
    prefix: &str,
) -> std::io::Result<std::path::PathBuf> {
    create_temp_resource(parent, prefix, |path| {
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder.create(path)
    })
    .map(|((), path)| path)
}

/// Creates and opens a fresh file for `fs::temp_file` and `fs::TempFile`,
/// readable and writable. On Unix only the owner may read or write it (mode
/// 0600), as with Go's `os.CreateTemp` and Python's `mkstemp`; on Windows the
/// user's own temporary folder already limits access to that user.
///
/// # Errors
///
/// `prefix` is not a single path component, or the file could not be created.
pub fn create_temp_file(
    parent: Option<&std::path::Path>,
    prefix: &str,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    create_temp_resource(parent, prefix, |path| {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path)
    })
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
    ffi_entry!({
        if path.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        match crate::sched_global::run_blocking("fs-read-string", move || {
            crate::file_read::read_to_string(std::path::Path::new(&p))
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
/// The UTF-8 text of the file at `path`, read into a buffer laid out as a
/// string body and sized from the file's length, so the string takes the
/// buffer over without copying the text.
fn read_text_body(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = crate::file_read::open_for_read(path)?;
    let size = file
        .metadata()
        .ok()
        .and_then(|meta| usize::try_from(meta.len()).ok())
        .unwrap_or(0);
    let prefix = crate::c_abi::string::BODY_PREFIX_BYTES;
    let mut body = Vec::with_capacity(crate::c_abi::string::prefixed_block_len(size));
    body.resize(prefix, 0);
    file.read_to_end(&mut body)?;
    if std::str::from_utf8(&body[prefix..]).is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ));
    }
    Ok(body)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_read_to_string_result(path: *const c_char) -> i128 {
    ffi_entry!({
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("read_to_string: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-read-string", move || {
            read_text_body(std::path::Path::new(&p))
        }) {
            Ok(Ok(body)) => {
                let s = crate::c_abi::string::string_from_prefixed(body);
                gos_rt_result_new(0, s as i64)
            }
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_write(path: *const c_char, contents: *const c_char) -> i64 {
    ffi_entry!({
        if path.is_null() || contents.is_null() {
            return 0;
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        // SAFETY: `contents` is a String argument from compiled code, null or a live string body for the whole call.
        let c = unsafe { crate::c_abi::gos_str_arg_bytes(contents) }.to_vec();
        i64::from(
            crate::sched_global::run_blocking("fs-write", move || std::fs::write(p, c))
                .is_ok_and(|result| result.is_ok()),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_create_dir_all(path: *const c_char) -> i64 {
    ffi_entry!({
        if path.is_null() {
            return 0;
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        if path.is_null() {
            return 0;
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("File::open: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-open", move || {
            crate::file_read::open_for_read(std::path::Path::new(&p))
        }) {
            Ok(Ok(file)) => gos_rt_result_new(0, insert_file(file)),
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::create(path) -> Result<File, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_create(path: *const c_char) -> i128 {
    ffi_entry!({
        if path.is_null() {
            return fs_err("File::create: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-create", move || std::fs::File::create(p)) {
            Ok(Ok(file)) => gos_rt_result_new(0, insert_file(file)),
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `__gos_fs_temp_dir_raw(prefix, parent)`, the leaf under `fs::temp_dir`:
/// an empty `parent` names the system temporary root, as Go's `os.MkdirTemp`
/// reads an empty directory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_temp_dir_raw(
    prefix: *const c_char,
    parent: *const c_char,
) -> i128 {
    ffi_entry!({
        if prefix.is_null() || parent.is_null() {
            return fs_err("fs::temp_dir: null argument");
        }
        // SAFETY: both are String arguments from compiled code, non-null (checked above) and live for the whole call.
        let (prefix, parent) = unsafe {
            (
                crate::c_abi::gos_str_arg_string(prefix),
                crate::c_abi::gos_str_arg_string(parent),
            )
        };
        let context = if parent.is_empty() {
            prefix.clone()
        } else {
            parent.clone()
        };
        match crate::sched_global::run_blocking("fs-temp-dir", move || {
            let parent = (!parent.is_empty()).then(|| std::path::PathBuf::from(parent));
            create_temp_dir(parent.as_deref(), &prefix)
        }) {
            Ok(Ok(path)) => {
                let path = alloc_cstring(path.to_string_lossy().as_bytes());
                gos_rt_result_new(0, path as i64)
            }
            Ok(Err(error)) => fs_io_err(&error, &context),
            Err(error) => fs_err(&error),
        }
    })
}

/// Layout of the `fs::temp_file` `(File, String)` pair: the blob owns a
/// share of the file handle and the path string.
static TEMP_FILE_PAIR_META: [i64; 6] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    2,
    gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT,
    (gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
];

/// `__gos_fs_temp_file_raw(prefix, parent) -> Result<(File, String), Error>`,
/// the leaf under `fs::temp_file`: an empty `parent` names the system
/// temporary root, as for `fs::temp_dir`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_temp_file_raw(
    prefix: *const c_char,
    parent: *const c_char,
) -> i128 {
    ffi_entry!({
        if parent.is_null() {
            return fs_err("fs::temp_file: null argument");
        }
        // SAFETY: `prefix` is this shim's argument, null or a live string body (C-ABI contract).
        let prefix = match unsafe { temp_prefix(prefix, "fs::temp_file") } {
            Ok(prefix) => prefix,
            Err(error) => return error,
        };
        // SAFETY: `parent` is a String argument from compiled code, non-null (checked above) and live for the whole call.
        let parent = unsafe { crate::c_abi::gos_str_arg_string(parent) };
        let context = if parent.is_empty() {
            "fs::temp_file".to_string()
        } else {
            parent.clone()
        };
        match crate::sched_global::run_blocking("fs-temp-file", move || {
            let parent = (!parent.is_empty()).then(|| std::path::PathBuf::from(parent));
            create_temp_file(parent.as_deref(), &prefix)
        }) {
            Ok(Ok((file, path))) => {
                let path = path.to_string_lossy().into_owned();
                let words = [insert_file(file), alloc_cstring(path.as_bytes()) as i64];
                let pair = crate::c_abi::rc::counted_words(&words, &TEMP_FILE_PAIR_META);
                if pair.is_null() {
                    return fs_err("fs::temp_file: allocation failed");
                }
                gos_rt_result_new(0, pair as i64)
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
            ffi_entry!({
                if let Some(opts) = open_options_clone(h) {
                    opts.lock().$field = enabled != 0;
                }
                // The answer is the receiver, handed back as a share of its
                // own so the next call in a chain holds the builder.
                if h != 0 {
                    // SAFETY: `h` is the live builder handle the caller holds.
                    unsafe {
                        super::rc::gos_rt_rc_retain(std::ptr::with_exposed_provenance_mut(
                            h as usize,
                        ))
                    };
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("OpenOptions::open: null path");
        }
        let Some(opts) = open_options_clone(h) else {
            return fs_err("OpenOptions::open: stale handle");
        };
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let open = apply_open_options(&opts.lock());
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-open-options", move || open.open(p)) {
            Ok(Ok(file)) => gos_rt_result_new(0, insert_file(file)),
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::read(max) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_read(h: i64, max: i64) -> i128 {
    ffi_entry!({
        let cap = max.clamp(1, 1 << 24) as usize;
        match with_file_cursor(h, "File::read", move |file| {
            let mut buf = vec![0u8; cap];
            file.read(&mut buf).map(|n| {
                buf.truncate(n);
                buf
            })
        }) {
            Ok(Ok(buf)) => gos_rt_result_new(0, super::encoding::bytes_to_gosvec(&buf) as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::read"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::read_to_string() -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_read_to_string(h: i64) -> i128 {
    ffi_entry!({
        match with_file_cursor(h, "File::read_to_string", |file| {
            let mut text = String::new();
            file.read_to_string(&mut text).map(|_| text)
        }) {
            Ok(Ok(text)) => gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::read_to_string"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::write(data: String) -> Result<i64, Error>`: the whole text
/// is written, so the answer is its byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_write(h: i64, data: *const c_char) -> i128 {
    ffi_entry!({
        if data.is_null() {
            return fs_err("File::write: null data");
        }
        // SAFETY: `data` is a String argument from compiled code, null or a live string body for the whole call.
        let bytes = unsafe { crate::c_abi::gos_str_arg_string(data) }.into_bytes();
        let len = bytes.len();
        match with_open_file(h, "File::write", |file| file.write_all(&bytes)) {
            Ok(Ok(())) => gos_rt_result_new(0, len as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::write"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::flush() -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_flush(h: i64) -> i128 {
    ffi_entry!({
        match with_open_file(h, "File::flush", OpenFile::flush) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => fs_io_err(&e, "File::flush"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::close()`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_close(h: i64) {
    ffi_entry!({
        retire_file(super::registry_key::key_id(h));
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
    ffi_entry!({
        if path.is_null() || contents.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("write_file: null arg".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        // SAFETY: `contents` is a String argument from compiled code, null or a live string body for the whole call.
        let c = unsafe { crate::c_abi::gos_str_arg_bytes(contents) }.to_vec();
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-write-file", move || std::fs::write(p, c)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
    ffi_entry!({
        if path.is_null() || contents.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("write_file: null arg".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        // SAFETY: `contents` is non-null (checked above) and live for the call (C-ABI contract).
        let bytes = unsafe { crate::c_abi::vec::vec_bytes_cow(contents) }.into_owned();
        match crate::sched_global::run_blocking("fs-write-bytes", move || std::fs::write(p, bytes))
        {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
    ffi_entry!({
        if path.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("read_file: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match read_file_into_vec(p) {
            Ok(Ok(v)) => gos_rt_result_new(0, v as i64),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
        let file = crate::file_read::open_for_read(std::path::Path::new(&p))?;
        let len = file.metadata().map_or(0, |m| m.len());
        Ok::<_, std::io::Error>((file, len))
    })?;
    let (mut file, len) = match opened {
        Ok(opened) => opened,
        Err(e) => return Ok(Err(e)),
    };
    let cap = i64::try_from(len).unwrap_or(i64::MAX);
    let v = crate::c_abi::vec::gos_rt_vec_with_capacity(1, cap);
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
            // SAFETY: `v` is the fresh vec made above, owned here alone and not read again.
            unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
            return Ok(Err(e));
        }
        Err(e) => {
            // SAFETY: `v` is the fresh vec made above, owned here alone and not read again.
            unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
            return Err(e);
        }
    };
    let filled_len = i64::try_from(filled).unwrap_or(0);
    // SAFETY: `v` is the live header allocated above, owned here alone.
    unsafe { (*v).len = filled_len };
    if !tail.is_empty() {
        let total = filled_len + i64::try_from(tail.len()).unwrap_or(0);
        // SAFETY: `v` is the fresh vec made above, owned here alone.
        unsafe { crate::c_abi::vec::gos_rt_vec_reserve_exact(v, total) };
        // SAFETY: the reserve left room for `tail` past `len`.
        let vref = unsafe { &mut *v };
        // SAFETY: the reserve left room for `tail` past the `filled` bytes, and `tail` is this
        // function's own buffer.
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
    ffi_entry!({
        if path.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("mkdir_all: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-mkdir-all", move || std::fs::create_dir_all(p))
        {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::remove_all(path) -> Result<(), IoError>` - removes a directory tree or file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_remove_dir_all_result(path: *const c_char) -> i128 {
    ffi_entry!({
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_all: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-dir-all", move || {
            std::fs::remove_dir_all(p)
        }) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
    ffi_entry!({
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("create_dir: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-create-dir", move || std::fs::create_dir(p)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
    ffi_entry!({
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_dir: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-dir", move || std::fs::remove_dir(p)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::remove_file(path) -> Result<(), IoError>` - Result shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_remove_file_result(path: *const c_char) -> i128 {
    ffi_entry!({
        if path.is_null() {
            let err =
                crate::c_abi::errors::error_new_from_bytes("remove_file: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-remove-file", move || std::fs::remove_file(p)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::rename(from, to)` / `fs::rename(from, to)` -> Result<(), Error>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_rename(from: *const c_char, to: *const c_char) -> i128 {
    ffi_entry!({
        if from.is_null() || to.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes("rename: null path".as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `from` is a String argument from compiled code, null or a live string body for the whole call.
        let f = unsafe { crate::c_abi::gos_str_arg_string(from) };
        // SAFETY: `to` is a String argument from compiled code, null or a live string body for the whole call.
        let t = unsafe { crate::c_abi::gos_str_arg_string(to) };
        let context = f.clone();
        match crate::sched_global::run_blocking("fs-rename", move || std::fs::rename(f, t)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// A String argument from compiled code as owned text, or the empty string
/// for null.
///
/// # Safety
///
/// `ptr` is null or a live string body for the call.
unsafe fn path_arg(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { crate::c_abi::gos_str_arg_string(ptr) }
    }
}

/// Runs the filesystem operation `op` on the blocking pool, answering its
/// value through `ok` or its error named for `context`.
fn fs_op<T: Send + 'static>(
    label: &'static str,
    context: String,
    op: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ok: impl FnOnce(T) -> i128,
) -> i128 {
    match crate::sched_global::run_blocking(label, op) {
        Ok(Ok(value)) => ok(value),
        Ok(Err(e)) => fs_io_err(&e, &context),
        Err(e) => fs_err(&e),
    }
}

/// `fs::symlink(target, link) -> Result<(), io::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_symlink(target: *const c_char, link: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments (C-ABI contract).
        let (target, link) = unsafe { (path_arg(target), path_arg(link)) };
        fs_op(
            "fs-symlink",
            link.clone(),
            move || crate::fs_ops::symlink(&target, &link),
            |()| gos_rt_result_new(0, 0),
        )
    })
}

/// `fs::hard_link(original, link) -> Result<(), io::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_hard_link(original: *const c_char, link: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments (C-ABI contract).
        let (original, link) = unsafe { (path_arg(original), path_arg(link)) };
        fs_op(
            "fs-hard-link",
            link.clone(),
            move || crate::fs_ops::hard_link(&original, &link),
            |()| gos_rt_result_new(0, 0),
        )
    })
}

/// `fs::read_link(path) -> Result<String, io::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_read_link(path: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `path` is this shim's String argument (C-ABI contract).
        let path = unsafe { path_arg(path) };
        fs_op(
            "fs-read-link",
            path.clone(),
            move || crate::fs_ops::read_link(&path),
            |target| gos_rt_result_new(0, alloc_cstring(target.as_bytes()) as i64),
        )
    })
}

/// `fs::copy_dir(src, dst) -> Result<i64, io::Error>`: the bytes copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_copy_dir(src: *const c_char, dst: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments (C-ABI contract).
        let (src, dst) = unsafe { (path_arg(src), path_arg(dst)) };
        fs_op(
            "fs-copy-dir",
            src.clone(),
            move || crate::fs_ops::copy_dir(&src, &dst),
            |n| gos_rt_result_new(0, i64::try_from(n).unwrap_or(i64::MAX)),
        )
    })
}

/// `fs::move(src, dst) -> Result<(), io::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_move(src: *const c_char, dst: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments (C-ABI contract).
        let (src, dst) = unsafe { (path_arg(src), path_arg(dst)) };
        fs_op(
            "fs-move",
            src.clone(),
            move || crate::fs_ops::move_path(&src, &dst),
            |()| gos_rt_result_new(0, 0),
        )
    })
}

/// `__gos_fs_chown_raw(path, set_uid, uid, set_gid, gid) -> Result<(), Error>`:
/// an id whose flag is 0 is left as it is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_chown_raw(
    path: *const c_char,
    set_uid: i64,
    uid: i64,
    set_gid: i64,
    gid: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `path` is this shim's String argument (C-ABI contract).
        let path = unsafe { path_arg(path) };
        let id = |set: i64, id: i64, what: &str| match (set, u32::try_from(id)) {
            (0, _) => Ok(None),
            (_, Ok(id)) => Ok(Some(id)),
            (_, Err(_)) => Err(format!("chown: {what} {id} is out of range")),
        };
        let (uid, gid) = match (id(set_uid, uid, "uid"), id(set_gid, gid, "gid")) {
            (Ok(uid), Ok(gid)) => (uid, gid),
            (Err(message), _) | (_, Err(message)) => return fs_err(&message),
        };
        fs_op(
            "fs-chown",
            path.clone(),
            move || crate::fs_ops::chown(&path, uid, gid),
            |()| gos_rt_result_new(0, 0),
        )
    })
}

/// `__gos_fs_disk_space_raw(path) -> Result<(i64, i64, i64), Error>`: total,
/// free, and available bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_disk_space_raw(path: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `path` is this shim's String argument (C-ABI contract).
        let path = unsafe { path_arg(path) };
        fs_op(
            "fs-disk-space",
            path.clone(),
            move || crate::fs_ops::disk_space(&path),
            |space| {
                let words = space.map(|n| i64::try_from(n).unwrap_or(i64::MAX));
                let blob =
                    crate::c_abi::rc::counted_words(&words, &crate::c_abi::rc::LEAF_BLOB_META);
                gos_rt_result_new(0, blob as i64)
            },
        )
    })
}

/// `env::set_current_dir(path) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_env_set_current_dir(path: *const c_char) -> i128 {
    ffi_entry!({
        let p = if path.is_null() {
            String::new()
        } else {
            // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        let context = p.clone();
        match crate::sched_global::run_blocking("env-set-current-dir", move || {
            std::env::set_current_dir(p)
        }) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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

/// `os::hostname() -> Result<String, errors::Error>`: the machine's host name.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_os_hostname() -> i128 {
    ffi_entry!({
        match crate::platform::host_name() {
            Ok(name) => gos_rt_result_new(0, alloc_cstring(name.as_bytes()) as i64),
            Err(message) => {
                let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_join(a: *const c_char, b: *const c_char) -> *mut c_char {
    ffi_entry!({
        let a = if a.is_null() {
            ""
        } else {
            // SAFETY: `a` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(a) }
        };
        let b = if b.is_null() {
            ""
        } else {
            // SAFETY: `b` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(b) }
        };
        alloc_cstring(crate::path_grammar::join(a, b).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_base(p: *const c_char) -> *mut c_char {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_cstring(crate::path_grammar::base(s).as_bytes())
    })
}

/// Parent directory helper for `path::parent`.
/// Returns `"."` when no separator is present.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_dir(p: *const c_char) -> *mut c_char {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_cstring(crate::path_grammar::dir(s).as_bytes())
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
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let (dir, file) = crate::path_grammar::split(s);
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
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&crate::path_grammar::components(s))
    })
}

/// `path::prefixes(p) -> Vec<String>` - cumulative Rust-like lexical prefixes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_prefixes(p: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&crate::path_grammar::prefixes(s))
    })
}

/// `path::unique_prefixes(text) -> Vec<String>` - sorted unique cumulative
/// prefixes for newline-delimited paths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_unique_prefixes(p: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_plain_str_vec(&crate::path_grammar::unique_prefixes(s))
    })
}

/// `path::extension(p) -> Option<String>` - extension with the leading
/// dot wrapped in `Some`, or `None` if absent / the dot is at the
/// very start of the file name. Mirrors the interp / stdlib
/// `path::extension` Option-returning shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_ext(p: *const c_char) -> i128 {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let ext_str = crate::path_grammar::ext(s);
        if ext_str.is_empty() {
            gos_rt_result_new(1, 0)
        } else {
            let cstr = alloc_cstring(ext_str.as_bytes()) as i64;
            gos_rt_result_new(0, cstr)
        }
    })
}

/// `path::parent(p) -> Option<String>` - drops the trailing
/// component. Returns None when `p` has no parent (root or
/// single-component path). Mirrors `gossamer_std::path::parent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_parent(p: *const c_char) -> i128 {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        match crate::path_grammar::parent(s) {
            None => gos_rt_result_new(1, 0),
            Some(parent) => {
                let cstr = alloc_cstring(parent.as_bytes()) as i64;
                gos_rt_result_new(0, cstr)
            }
        }
    })
}

/// `path::file_stem(p) -> Option<String>` - basename minus the
/// extension. Returns None when the basename is empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_stem(p: *const c_char) -> i128 {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        match crate::path_grammar::stem(s) {
            None => gos_rt_result_new(1, 0),
            Some(stem) => {
                let cstr = alloc_cstring(stem.as_bytes()) as i64;
                gos_rt_result_new(0, cstr)
            }
        }
    })
}

/// `path::file_name(p) -> Option<String>` - last component.
/// Returns None for empty paths or trailing-slash directories.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_file_name(p: *const c_char) -> i128 {
    ffi_entry!({
        let s = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let basename = crate::path_grammar::base(s);
        if basename.is_empty() {
            gos_rt_result_new(1, 0)
        } else {
            let cstr = alloc_cstring(basename.as_bytes()) as i64;
            gos_rt_result_new(0, cstr)
        }
    })
}

/// `path::normalize(p) -> String`. Lexical
/// cleanup mirroring `gossamer_std::path::normalize` (no I/O); the
/// runtime crate stays free of a dep on `gossamer-std`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_clean(p: *const c_char) -> *mut c_char {
    ffi_entry!({
        let path = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        alloc_cstring(crate::path_grammar::clean(path).as_bytes())
    })
}

/// `path::is_absolute(p) -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_is_absolute(p: *const c_char) -> i32 {
    ffi_entry!({
        let path = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        i32::from(crate::path_grammar::is_absolute(path))
    })
}

/// `path::starts_with(p, prefix) -> bool` - path-aware prefix test.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_has_prefix(p: *const c_char, prefix: *const c_char) -> i32 {
    ffi_entry!({
        let path = if p.is_null() {
            ""
        } else {
            // SAFETY: `p` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(p) }
        };
        let prefix = if prefix.is_null() {
            ""
        } else {
            // SAFETY: `prefix` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(prefix) }
        };
        i32::from(crate::path_grammar::has_prefix(path, prefix))
    })
}

/// `os::copy(src, dst) / fs::copy(src, dst) -> Result<i64, Error>`
/// - copies the file contents and returns the byte count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_copy(src: *const c_char, dst: *const c_char) -> i128 {
    ffi_entry!({
        let src = if src.is_null() {
            String::new()
        } else {
            // SAFETY: `src` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(src) }
        };
        let dst = if dst.is_null() {
            String::new()
        } else {
            // SAFETY: `dst` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(dst) }
        };
        let context = format!("{src} -> {dst}");
        match crate::sched_global::run_blocking("fs-copy", move || std::fs::copy(src, dst)) {
            Ok(Ok(n)) => gos_rt_result_new(0, i64::try_from(n).unwrap_or(i64::MAX)),
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
            Err(e) => fs_err(&e),
        }
    })
}

/// `os::canonicalize(p) / fs::canonicalize(p) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_canonicalize(path: *const c_char) -> i128 {
    ffi_entry!({
        let p = if path.is_null() {
            String::new()
        } else {
            // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-canonicalize", move || std::fs::canonicalize(p))
        {
            Ok(Ok(abs)) => {
                let s = crate::canonical_path::display(&abs);
                let ptr = alloc_cstring(s.as_bytes()) as i64;
                gos_rt_result_new(0, ptr)
            }
            Ok(Err(e)) => {
                let msg = classify_io_error(&e, &context);
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
    gos_rt_result_new(0, vec as i64)
}

fn alloc_plain_str_vec(parts: &[String]) -> *mut GosVec {
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

fn err_io(e: &std::io::Error) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(format!("{e}").as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// `bufio::read_to_string(path) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_read_to_string(path: *const c_char) -> i128 {
    ffi_entry!({
        let p = if path.is_null() {
            String::new()
        } else {
            // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        match crate::sched_global::run_blocking("bufio-read-string", move || {
            std::fs::read_to_string(p)
        }) {
            Ok(Ok(text)) => gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
            Ok(Err(e)) => err_io(&e),
            Err(e) => fs_err(&e),
        }
    })
}

/// `bufio::read_lines_of(path) -> Result<[String], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bufio_read_lines_of(path: *const c_char) -> i128 {
    ffi_entry!({
        let p = if path.is_null() {
            String::new()
        } else {
            // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        use std::net::ToSocketAddrs;
        let h = if host.is_null() {
            String::new()
        } else {
            // SAFETY: `host` is a String argument from compiled code, null or a live string body for the whole call.
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

/// `__gos_net_dns_raw(name, rtype, server) -> Result<Vec<String>, Error>`:
/// the records of DNS type `rtype` for `name`, each record's text fields in
/// order, flattened (`dns::lookup` names the fields per type). An empty
/// `server` asks the system's nameservers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_net_dns_raw(
    name: *const c_char,
    rtype: i64,
    server: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's String arguments (C-ABI contract).
        let (name, server) = unsafe { (path_arg(name), path_arg(server)) };
        dns_records(name, rtype, server)
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn dns_records(name: String, rtype: i64, server: String) -> i128 {
    let Some(kind) = crate::dns::RecordType::from_code(rtype) else {
        return fs_err(&format!(
            "dns: record type {rtype} is not one this resolver reads"
        ));
    };
    match crate::sched_global::run_blocking("net-dns", move || {
        crate::dns::lookup(&name, kind, Some(&server))
    }) {
        Ok(Ok(records)) => ok_str_vec(&records.concat()),
        Ok(Err(e)) | Err(e) => fs_err(&e),
    }
}

/// wasm32 has no sockets to reach a nameserver through.
#[cfg(target_arch = "wasm32")]
fn dns_records(name: String, _rtype: i64, _server: String) -> i128 {
    fs_err(&format!(
        "dns: looking up {name} needs a network, which wasm32 has none of"
    ))
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

/// The panic every tier raises for a `clamp` whose bounds are out of order.
fn clamp_bounds_panic(lo: impl std::fmt::Display, hi: impl std::fmt::Display) -> ! {
    super::panic::panic_text(&format!(
        "clamp: the lower bound {lo} is above the upper bound {hi}"
    ))
}

/// The panic for a 128-bit `clamp` whose bounds are out of order, each bound
/// passed as its low and high words; `unsigned` is 1 for a `u128`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_clamp_bounds_wide(
    lo_low: i64,
    lo_high: i64,
    hi_low: i64,
    hi_high: i64,
    unsigned: i64,
) -> ! {
    let lo = super::concat::wide_from_words(lo_low.cast_unsigned(), lo_high.cast_unsigned());
    let hi = super::concat::wide_from_words(hi_low.cast_unsigned(), hi_high.cast_unsigned());
    if unsigned != 0 {
        clamp_bounds_panic(lo, hi)
    } else {
        clamp_bounds_panic(lo.cast_signed(), hi.cast_signed())
    }
}

/// Scalar `clamp(x, lo, hi)` for `i64`; bounds out of order panic.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_clamp_i64(x: i64, lo: i64, hi: i64) -> i64 {
    if lo > hi {
        clamp_bounds_panic(lo, hi);
    }
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
pub extern "C-unwind" fn gos_rt_clamp_u64(x: u64, lo: u64, hi: u64) -> u64 {
    if lo > hi {
        clamp_bounds_panic(lo, hi);
    }
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

/// Scalar `clamp(x, lo, hi)` for `f64`; bounds out of order or NaN panic.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_clamp_f64(x: f64, lo: f64, hi: f64) -> f64 {
    // Bounds out of order, or a NaN bound, which orders with nothing.
    if !matches!(
        lo.partial_cmp(&hi),
        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
    ) {
        clamp_bounds_panic(lo, hi);
    }
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
    ffi_entry!({
        let pat = if pattern.is_null() {
            ""
        } else {
            // SAFETY: `pattern` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(pattern) }
        };
        let n = if name.is_null() {
            ""
        } else {
            // SAFETY: `name` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_text(name) }
        };
        i64::from(path_glob_matches(pat.as_bytes(), n.as_bytes()))
    })
}

/// `path::glob(pattern) -> Result<Vec<String>, errors::Error>` - sorted
/// matching paths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_path_glob(pattern: *const c_char) -> i128 {
    ffi_entry!({
        let pat = if pattern.is_null() {
            ""
        } else {
            // SAFETY: `pattern` is a String argument from compiled code, null or a live string body for the whole call.
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
    let _syscall = crate::sched_global::syscall_enter();
    entry.with_cursor(op).map_err(|e| fs_io_err(&e, context))
}

/// Runs `op` on the open file behind `h`, marked as inside a system call.
fn with_open_file<T>(h: i64, context: &str, op: impl FnOnce(&OpenFile) -> T) -> Result<T, i128> {
    let Some(entry) = file_clone(h) else {
        return Err(fs_err(&format!("{context}: stale handle")));
    };
    let _syscall = crate::sched_global::syscall_enter();
    Ok(op(&entry))
}

/// Turns on the write buffer of the file behind `h`: the leaf under
/// `bufio::Writer::with_capacity`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bufio_buffer_writes_raw(h: i64, capacity: i64) -> i128 {
    ffi_entry!({
        let capacity = usize::try_from(capacity).unwrap_or(0);
        let Some(entry) = file_clone(h) else {
            return fs_err("bufio::Writer: stale handle");
        };
        match entry.buffer_writes(capacity) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(e) => fs_io_err(&e, "bufio::Writer"),
        }
    })
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
    let _syscall = crate::sched_global::syscall_enter();
    entry.positional(op).map_err(|e| fs_io_err(&e, context))
}

/// `fs::File::read_at(len, offset) -> Result<Vec<u8>, Error>`. The answer
/// is what the one positional read transferred, which may be shorter than
/// `len` at end of file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_read_at(h: i64, len: i64, offset: i64) -> i128 {
    ffi_entry!({
        if len < 0 || offset < 0 {
            return fs_err("File::read_at: length and offset must be non-negative");
        }
        let cap = len.min(1 << 24) as usize;
        let offset = offset as u64;
        let out = super::vec::gos_rt_vec_with_capacity(1, cap as i64);
        // SAFETY: `out` is the fresh byte vec made above, which nothing else reads.
        let read = with_file_positional(h, "File::read_at", |file| unsafe {
            read_at_into_vec(file, out, cap, offset)
        });
        match read {
            Ok(Ok(_)) => gos_rt_result_new(0, out as i64),
            Ok(Err(e)) => {
                // SAFETY: `out` is the fresh vec made above, owned here alone and not read again.
                unsafe { super::map::gos_rt_vec_free(out) };
                fs_io_err(&e, "File::read_at")
            }
            Err(packed) => {
                // SAFETY: `out` is the fresh vec made above, owned here alone and not read again.
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
    ffi_entry!({
        if len < 0 || offset < 0 {
            return fs_err("File::read_at_into: length and offset must be non-negative");
        }
        // SAFETY: `buf` is non-null on this side of the `||` and live for the call (C-ABI
        // contract).
        if buf.is_null() || unsafe { (*buf).elem_bytes } != 1 {
            return fs_err("File::read_at_into: the buffer must be a Vec<u8>");
        }
        let cap = len.min(1 << 24) as usize;
        let offset = offset as u64;
        // SAFETY: `buf` is this shim's non-null byte vec (checked above), live for the call and
        // read by nothing else (C-ABI contract).
        let read = with_file_positional(h, "File::read_at_into", |file| unsafe {
            read_at_into_vec(file, buf, cap, offset)
        });
        match read {
            Ok(Ok(n)) => gos_rt_result_new(0, n as i64),
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
    // SAFETY: this `unsafe fn`'s caller passes `v` live or null, which `gos_rt_vec_reserve_exact`
    // accepts.
    unsafe { super::vec::gos_rt_vec_reserve_exact(v, len as i64) };
    // SAFETY: this `unsafe fn`'s caller passes `v` as a live `GosVec` nothing else reads.
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
        // SAFETY: `dst` addresses `len` writable bytes of `v`'s capacity, zeroed just above.
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
    ffi_entry!({
        if offset < 0 {
            return fs_err("File::write_at: offset must be non-negative");
        }
        // SAFETY: `data` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let at = offset as u64;
        let written = with_file_positional(h, "File::write_at", move |file| {
            write_at_offset(file, &bytes, at)
        });
        match written {
            Ok(Ok(n)) => gos_rt_result_new(0, n as i64),
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
    ffi_entry!({
        // SAFETY: `data` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
        let written = with_open_file(h, "File::write_bytes", |file| file.write_some(&bytes));
        match written {
            Ok(Ok(n)) => gos_rt_result_new(0, n as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::write_bytes"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::seek(offset, whence) -> Result<i64, Error>`, answering the
/// new absolute position. `whence` is one of `fs::SEEK_SET`,
/// `fs::SEEK_CUR`, `fs::SEEK_END`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_seek(h: i64, offset: i64, whence: i64) -> i128 {
    ffi_entry!({
        let from = match whence {
            0 => std::io::SeekFrom::Start(offset.max(0) as u64),
            1 => std::io::SeekFrom::Current(offset),
            2 => std::io::SeekFrom::End(offset),
            _ => return fs_err("File::seek: whence must be SEEK_SET, SEEK_CUR, or SEEK_END"),
        };
        match with_file_cursor(h, "File::seek", move |file| std::io::Seek::seek(file, from)) {
            Ok(Ok(pos)) => gos_rt_result_new(0, pos as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::seek"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::set_len(len) -> Result<(), Error>`: truncate or extend.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_set_len(h: i64, len: i64) -> i128 {
    ffi_entry!({
        if len < 0 {
            return fs_err("File::set_len: length must be non-negative");
        }
        let len = len as u64;
        match with_file_cursor(h, "File::set_len", move |file| file.set_len(len)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => fs_io_err(&e, "File::set_len"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::len() -> Result<i64, Error>`: the open file's current size.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_len(h: i64) -> i128 {
    ffi_entry!({
        match with_file_cursor(h, "File::len", |file| file.metadata().map(|m| m.len())) {
            Ok(Ok(len)) => gos_rt_result_new(0, len as i64),
            Ok(Err(e)) => fs_io_err(&e, "File::len"),
            Err(packed) => packed,
        }
    })
}

/// Takes ownership of the open descriptor `raw` (a handle on Windows), as
/// Go's `os.NewFile` does: the file answered closes it.
///
/// # Errors
///
/// `raw` does not name an open descriptor, or the target has none.
pub fn file_from_raw(raw: i64) -> Result<std::fs::File, String> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd as _;
        let fd = i32::try_from(raw)
            .ok()
            .filter(|fd| *fd >= 0)
            .ok_or_else(|| format!("File::from_raw: {raw} is not a descriptor"))?;
        // SAFETY: F_GETFD reads the descriptor's flags and touches no memory.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return Err(format!(
                "File::from_raw: descriptor {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `fd` is open (checked above) and the caller hands over its
        // ownership, so the file is its one owner and closes it once.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};
        // A kernel handle is positive; the negative values are pseudo-handles
        // (-1 names the current process, -2 its thread) that no file owns.
        if raw <= 0 {
            return Err(format!("File::from_raw: {raw} is not a handle"));
        }
        let handle = raw as isize as HANDLE;
        let mut flags = 0u32;
        // SAFETY: the call writes one u32 through `flags` and fails for a
        // value that is not an open handle.
        if unsafe { GetHandleInformation(handle, &raw mut flags) } == 0 {
            return Err(format!(
                "File::from_raw: handle {raw}: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `handle` is open (checked above) and the caller hands over
        // its ownership, so the file is its one owner and closes it once.
        Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(format!(
            "File::from_raw({raw}): descriptors are not available on this target"
        ))
    }
}

/// `fs::File::from_raw(raw) -> Result<File, Error>`: a file owning `raw`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_from_raw(raw: i64) -> i128 {
    ffi_entry!({
        match file_from_raw(raw) {
            Ok(file) => gos_rt_result_new(0, insert_file(file)),
            Err(message) => fs_err(&message),
        }
    })
}

/// The OS descriptor (a handle on Windows) of an open file, as Go's
/// `os.File.Fd` answers it: still owned by the file. `None` on a target
/// without descriptors.
pub fn raw_descriptor(file: &std::fs::File) -> Option<i64> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        Some(i64::from(file.as_raw_fd()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        Some(file.as_raw_handle() as isize as i64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        None
    }
}

/// `fs::File::fd() -> Result<i64, Error>`: the file's OS descriptor (a
/// handle on Windows), for the calls that take one.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_fd(h: i64) -> i128 {
    ffi_entry!({
        let Some(entry) = file_clone(h) else {
            return fs_err("File::fd: stale handle");
        };
        match raw_descriptor(entry.file()) {
            Some(fd) => gos_rt_result_new(0, fd),
            None => fs_err("File::fd: descriptors are not available on this target"),
        }
    })
}

/// `fs::File::sync_all() -> Result<(), Error>`: flush the file's data and
/// metadata to the storage device.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_sync_all(h: i64) -> i128 {
    ffi_entry!({
        match with_file_cursor(h, "File::sync_all", |file| file.sync_all()) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => fs_io_err(&e, "File::sync_all"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::sync_data() -> Result<(), Error>`: flush the file's data,
/// leaving metadata the platform considers inessential unwritten.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_sync_data(h: i64) -> i128 {
    ffi_entry!({
        match with_file_cursor(h, "File::sync_data", |file| file.sync_data()) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("fs::sync_dir: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-sync-dir", move || sync_directory(&p)) {
            Ok(Ok(())) => gos_rt_result_new(0, 0),
            Ok(Err(e)) => fs_io_err(&e, &context),
            Err(e) => fs_err(&e),
        }
    })
}

/// `fs::File::try_lock_range(start, len, exclusive) -> Result<bool, Error>`.
/// `len` of 0 covers the range from `start` to end of file, however the
/// file later grows.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_try_lock_range(
    h: i64,
    start: i64,
    len: i64,
    exclusive: i32,
) -> i128 {
    ffi_entry!({
        let (Ok(start), Ok(len)) = (u64::try_from(start), u64::try_from(len)) else {
            return fs_err("File::try_lock_range: start and len must be non-negative");
        };
        try_lock_span(
            h,
            Span::Range { start, len },
            exclusive != 0,
            "File::try_lock_range",
        )
    })
}

/// `fs::File::unlock_range(start, len) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_unlock_range(h: i64, start: i64, len: i64) -> i128 {
    ffi_entry!({
        let (Ok(start), Ok(len)) = (u64::try_from(start), u64::try_from(len)) else {
            return fs_err("File::unlock_range: start and len must be non-negative");
        };
        match with_open_file(h, "File::unlock_range", |file| {
            file.unlock(Span::Range { start, len })
        }) {
            Ok(outcome) => fs_unit_result(outcome, "File::unlock_range"),
            Err(packed) => packed,
        }
    })
}

/// `fs::File::try_lock_shared() -> Result<bool, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_try_lock_shared(h: i64) -> i128 {
    ffi_entry!({ try_lock_span(h, Span::Whole, false, "File::try_lock_shared") })
}

/// `fs::File::try_lock_exclusive() -> Result<bool, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_file_try_lock_exclusive(h: i64) -> i128 {
    ffi_entry!({ try_lock_span(h, Span::Whole, true, "File::try_lock_exclusive") })
}

/// `fs::File::unlock() -> Result<(), Error>`: release every lock this file
/// holds, whole-file or range.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fs_file_unlock(h: i64) -> i128 {
    ffi_entry!({
        match with_open_file(h, "File::unlock", OpenFile::unlock_all) {
            Ok(outcome) => fs_unit_result(outcome, "File::unlock"),
            Err(packed) => packed,
        }
    })
}

fn try_lock_span(h: i64, span: Span, exclusive: bool, context: &str) -> i128 {
    match with_open_file(h, context, |file| file.try_lock(span, exclusive)) {
        Ok(Ok(acquired)) => gos_rt_result_new(0, i64::from(acquired)),
        Ok(Err(e)) => fs_io_err(&e, context),
        Err(packed) => packed,
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

/// Answers the `Result<(), errors::Error>` carrier for a filesystem
/// operation over `context`.
fn fs_unit_result(outcome: std::io::Result<()>, context: &str) -> i128 {
    match outcome {
        Ok(()) => gos_rt_result_new(0, 0),
        Err(e) => fs_io_err(&e, context),
    }
}

/// `fs::permissions(path) -> Result<i64, errors::Error>` - the
/// permission bits of `path`, in the chmod(2) encoding.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fs_permissions(path: *const c_char) -> i128 {
    ffi_entry!({
        if path.is_null() {
            return fs_err("permissions: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let context = p.clone();
        match crate::sched_global::run_blocking("fs-permissions", move || {
            crate::fs_mode::read(std::path::Path::new(&p))
        }) {
            Ok(Ok(mode)) => gos_rt_result_new(0, i64::from(mode)),
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("set_permissions: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("create_dir_mode: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        if path.is_null() {
            return fs_err("create_dir_all_mode: null path");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
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
    contents: *const super::vec::GosVec,
    mode: i64,
) -> i128 {
    ffi_entry!({
        if path.is_null() {
            return fs_err("write_mode: null argument");
        }
        // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        // SAFETY: `contents` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(contents) };
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
mod tests {
    use super::create_temp_dir;

    #[test]
    #[cfg_attr(miri, ignore)] // creates folders
    fn a_temp_folder_lands_inside_its_parent_and_a_prefix_cannot_leave_it() {
        let parent = std::env::temp_dir().join(format!("gos-temp-parent-{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        let made = create_temp_dir(Some(&parent), "work").unwrap();
        assert_eq!(made.parent(), Some(parent.as_path()));
        assert!(made.is_dir());
        for prefix in ["C:x", "a/b", "a\\b", "..", ".", "nul\0"] {
            let err = create_temp_dir(Some(&parent), prefix).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{prefix:?}");
        }
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // creates files
    fn temp_resources_take_unpredictable_names_only_their_owner_can_open() {
        let (file, file_path) = super::create_temp_file(None, "mode").unwrap();
        let (_, other_path) = super::create_temp_file(None, "mode").unwrap();
        let dir = create_temp_dir(None, "mode").unwrap();
        let name = file_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let suffix = name.strip_prefix("gossamer-mode-").unwrap();
        assert_eq!(suffix.len(), 16, "{name}");
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        assert_ne!(file_path, other_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        drop(file);
        for path in [file_path, other_path] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }
}
