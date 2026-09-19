#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Wires up Gossamer-callable builtins for stdlib modules whose
//! Rust-side implementation already exists but had no user-facing
//! exposure. Each `install_*` helper is invoked from
//! `builtins::install` so user code that writes
//! `strings::join`, `strconv::parse_i64`, `net::TcpStream::connect`,
//! `time::Instant::now`, etc. resolves to a real callable.
//!
//! All builtins return a `Result`-shaped variant (`Ok` / `Err`) on
//! fallible operations so callers can chain `?` without wrapping.

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::io::Read as IoRead;
use std::io::Write as IoWrite;
use std::sync::Arc;

use gossamer_ast::Ident;
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_std::net as net_std;
use gossamer_std::os as os_std;
use gossamer_std::path as path_std;
use gossamer_std::strconv as strconv_std;
use gossamer_std::strings as strings_std;
use gossamer_std::unicode as unicode_std;
use gossamer_std::utf8 as utf8_std;

use gossamer_std::iter as iter_std;
use gossamer_std::utf16 as utf16_std;

use crate::builtins::{
    BuiltinFnPub, as_str, err_variant, install_module_pub, none_variant, ok_variant, some_variant,
    value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeError, RuntimeResult, Value};

use gossamer_runtime::c_abi::fs::{
    classify_io_error, read_at_offset, sync_directory, try_lock_range_on, unlock_range_on,
    write_at_offset,
};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_fs_extras(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "fs",
        &[
            ("open", builtin_fs_file_open),
            ("create", builtin_fs_file_create),
            ("is_file", builtin_os_is_file),
            ("is_dir", builtin_os_is_dir),
            ("is_symlink", builtin_os_is_symlink),
            ("file_size", builtin_os_file_size),
            ("metadata", builtin_fs_metadata),
            ("copy", builtin_os_copy),
            ("canonicalize", builtin_os_canonicalize),
            ("temp_dir", builtin_fs_temp_dir),
            ("temp_file", builtin_fs_temp_file),
            ("sync_dir", builtin_fs_sync_dir),
            ("permissions", builtin_fs_permissions),
            ("set_permissions", builtin_fs_set_permissions),
            ("create_dir_mode", builtin_fs_create_dir_mode),
            ("create_dir_all_mode", builtin_fs_create_dir_all_mode),
            ("write_mode", builtin_fs_write_mode),
        ],
        globals,
    );
    // Leaf intrinsic for the injected real-struct `Metadata` wrapper
    // (gossamer-parse autoderive): returns the fields as a 6-tuple the
    // wrapper folds into a struct. Same field order as `builtin_fs_metadata`.
    {
        let q = "__gos_fs_metadata_raw";
        globals.push((q, crate::builtins::builtin_pub(q, builtin_fs_metadata_raw)));
    }
    // `whence` selectors for `File::seek`, bound as integer globals so
    // call sites name the position rather than a bare 0/1/2.
    for (name, value) in [
        ("fs::SEEK_SET", 0),
        ("fs::SEEK_CUR", 1),
        ("fs::SEEK_END", 2),
    ] {
        globals.push((name, Value::Int(value)));
    }
    let methods: &[(&str, BuiltinFnPub)] = &[
        ("File::open", builtin_fs_file_open),
        ("File::create", builtin_fs_file_create),
        ("File::read", builtin_fs_file_read),
        ("File::read_to_string", builtin_fs_file_read_to_string),
        ("File::write", builtin_fs_file_write),
        ("File::write_all", builtin_fs_file_write),
        ("File::write_bytes", builtin_fs_file_write_bytes),
        ("File::read_at", builtin_fs_file_read_at),
        ("File::read_at_into", builtin_fs_file_read_at_into),
        ("File::write_at", builtin_fs_file_write_at),
        ("File::seek", builtin_fs_file_seek),
        ("File::set_len", builtin_fs_file_set_len),
        ("File::len", builtin_fs_file_len),
        ("File::sync_all", builtin_fs_file_sync_all),
        ("File::sync_data", builtin_fs_file_sync_data),
        ("File::try_lock_range", builtin_fs_file_try_lock_range),
        ("File::unlock_range", builtin_fs_file_unlock_range),
        ("File::try_lock_shared", builtin_fs_file_try_lock_shared),
        (
            "File::try_lock_exclusive",
            builtin_fs_file_try_lock_exclusive,
        ),
        ("File::unlock", builtin_fs_file_unlock),
        ("File::flush", builtin_fs_file_flush),
        ("File::close", builtin_fs_file_close),
        ("OpenOptions::new", builtin_fs_open_options_new),
        ("OpenOptions::read", builtin_fs_open_options_read),
        ("OpenOptions::write", builtin_fs_open_options_write),
        ("OpenOptions::append", builtin_fs_open_options_append),
        ("OpenOptions::truncate", builtin_fs_open_options_truncate),
        ("OpenOptions::create", builtin_fs_open_options_create),
        (
            "OpenOptions::create_new",
            builtin_fs_open_options_create_new,
        ),
        ("OpenOptions::open", builtin_fs_open_options_open),
    ];
    for (short, call) in methods {
        let qualified: &'static str = Box::leak(format!("fs::{short}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
        globals.push((*short, crate::builtins::builtin_pub(short, *call)));
    }
}

#[derive(Default, Clone)]
struct FsOpenOptionsState {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

static NEXT_FS_HANDLE: GlobalReg<i64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
static FS_FILE_REGISTRY: GlobalReg<StdHashMap<i64, Arc<parking_lot::Mutex<std::fs::File>>>> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
static FS_OPEN_OPTIONS_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<FsOpenOptionsState>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

fn next_fs_handle() -> i64 {
    NEXT_FS_HANDLE.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

fn insert_file_handle(file: std::fs::File) -> Value {
    let id = next_fs_handle();
    FS_FILE_REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(id, Arc::new(parking_lot::Mutex::new(file)));
    });
    handle_struct("fs::File", id)
}

fn insert_open_options_handle(opts: FsOpenOptionsState) -> Value {
    let id = next_fs_handle();
    FS_OPEN_OPTIONS_REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(id, Arc::new(parking_lot::Mutex::new(opts)));
    });
    handle_struct("fs::OpenOptions", id)
}

fn fetch_file_handle(id: i64) -> Option<Arc<parking_lot::Mutex<std::fs::File>>> {
    FS_FILE_REGISTRY.with(|r| r.borrow().get(&id).cloned())
}

/// Run `op` on the handle's own descriptor in the blocking pool.
///
/// The descriptor is reached through the registry `Arc` rather than a
/// `try_clone`: POSIX record locks are released when any descriptor for
/// the file is closed by the process, so a handle owns exactly one for
/// its lifetime and a lock taken through it survives later reads and
/// writes.
/// Runs a file operation in place, as the compiled tiers do: a file call
/// never waits without bound, so it holds its thread for the syscall rather
/// than handing it to another.
fn with_file_inline<T, F>(id: i64, _label: &'static str, context: &str, op: F) -> Result<T, Value>
where
    F: FnOnce(&mut std::fs::File) -> T,
{
    let Some(file) = fetch_file_handle(id) else {
        return Err(err_variant(format!("{context}: stale handle")));
    };
    let mut guard = file.lock();
    let _syscall = gossamer_runtime::sched_global::syscall_enter();
    Ok(op(&mut guard))
}

/// Byte offset a `whence` selector names, or the diagnostic for an
/// unknown one.
fn seek_from(offset: i64, whence: i64) -> Result<std::io::SeekFrom, Value> {
    match whence {
        0 => Ok(std::io::SeekFrom::Start(offset.max(0) as u64)),
        1 => Ok(std::io::SeekFrom::Current(offset)),
        2 => Ok(std::io::SeekFrom::End(offset)),
        _ => Err(err_variant(
            "File::seek: whence must be SEEK_SET, SEEK_CUR, or SEEK_END",
        )),
    }
}

fn fetch_open_options_handle(id: i64) -> Option<Arc<parking_lot::Mutex<FsOpenOptionsState>>> {
    FS_OPEN_OPTIONS_REGISTRY.with(|r| r.borrow().get(&id).cloned())
}

fn std_open_options(opts: &FsOpenOptionsState) -> std::fs::OpenOptions {
    let mut out = std::fs::OpenOptions::new();
    out.read(opts.read)
        .write(opts.write)
        .append(opts.append)
        .truncate(opts.truncate)
        .create(opts.create)
        .create_new(opts.create_new);
    out
}

pub(crate) fn builtin_fs_file_open(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "File::open", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = path.clone();
    crate::comptime_gate::guard_read("fs::open", &path_context)?;
    match gossamer_runtime::sched_global::run_blocking("fs-file-open", move || {
        std::fs::File::open(path)
    }) {
        Ok(Ok(file)) => Ok(ok_variant(insert_file_handle(file))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_fs_file_create(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "File::create", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = path.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-file-create", move || {
        std::fs::File::create(path)
    }) {
        Ok(Ok(file)) => Ok(ok_variant(insert_file_handle(file))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// Creates a unique directory beneath the system temporary root. The caller
/// owns the returned directory and should remove it with `fs::remove_dir_all`.
pub(crate) fn builtin_fs_temp_dir(args: &[Value]) -> RuntimeResult<Value> {
    let prefix = match arg_str_at(args, 0, "fs::temp_dir", "prefix") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = prefix.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-temp-dir", move || {
        gossamer_std::fs::temp_dir(&prefix)
    }) {
        Ok(Ok(path)) => Ok(ok_variant(Value::String(
            path.to_string_lossy().into_owned().into(),
        ))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// Creates a unique temporary file and returns its streaming handle plus path.
/// Closing/removing the file remains the caller's responsibility.
pub(crate) fn builtin_fs_temp_file(args: &[Value]) -> RuntimeResult<Value> {
    let prefix = match arg_str_at(args, 0, "fs::temp_file", "prefix") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = prefix.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-temp-file", move || {
        gossamer_std::fs::temp_file(&prefix)
    }) {
        Ok(Ok((file, path))) => Ok(ok_variant(Value::Tuple(
            vec![
                insert_file_handle(file),
                Value::String(path.to_string_lossy().into_owned().into()),
            ]
            .into(),
        ))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_fs_open_options_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(insert_open_options_handle(FsOpenOptionsState::default()))
}

fn fs_open_options_set(args: &[Value], field: fn(&mut FsOpenOptionsState) -> &mut bool) -> Value {
    let Some(id) = args.first().and_then(handle_id) else {
        return err_variant("OpenOptions: missing handle");
    };
    let enabled = matches!(args.get(1), Some(Value::Bool(true)));
    let Some(opts) = fetch_open_options_handle(id) else {
        return err_variant("OpenOptions: stale handle");
    };
    *field(&mut opts.lock()) = enabled;
    handle_struct("fs::OpenOptions", id)
}

pub(crate) fn builtin_fs_open_options_read(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.read))
}

pub(crate) fn builtin_fs_open_options_write(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.write))
}

pub(crate) fn builtin_fs_open_options_append(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.append))
}

pub(crate) fn builtin_fs_open_options_truncate(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.truncate))
}

pub(crate) fn builtin_fs_open_options_create(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.create))
}

pub(crate) fn builtin_fs_open_options_create_new(args: &[Value]) -> RuntimeResult<Value> {
    Ok(fs_open_options_set(args, |opts| &mut opts.create_new))
}

pub(crate) fn builtin_fs_open_options_open(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("OpenOptions::open: missing handle"));
    };
    let path = match arg_str_at(args, 1, "OpenOptions::open", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = path.clone();
    let Some(opts) = fetch_open_options_handle(id) else {
        return Ok(err_variant("OpenOptions::open: stale handle"));
    };
    let open = std_open_options(&opts.lock());
    match gossamer_runtime::sched_global::run_blocking("fs-open-options", move || open.open(path)) {
        Ok(Ok(file)) => Ok(ok_variant(insert_file_handle(file))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

pub(crate) fn builtin_fs_file_read(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::read: missing handle"));
    };
    let max = match args.get(1).and_then(value_to_int) {
        Some(n) if n <= 0 => {
            return Err(RuntimeError::Type(
                "File::read: size must be positive".to_string(),
            ));
        }
        Some(n) => n.min(1 << 24),
        None => 4096,
    };
    match with_file_inline(id, "fs-file-read", "File::read", move |file| {
        let mut buf = vec![0u8; max as usize];
        file.read(&mut buf).map(|n| {
            buf.truncate(n);
            buf
        })
    }) {
        Ok(Ok(buf)) => Ok(ok_variant(bytes_value(&buf))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::read"))),
        Err(v) => Ok(v),
    }
}

pub(crate) fn builtin_fs_file_read_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::read_to_string: missing handle"));
    };
    match with_file_inline(
        id,
        "fs-file-read-string",
        "File::read_to_string",
        move |file| {
            let mut out = String::new();
            file.read_to_string(&mut out).map(|_| out)
        },
    ) {
        Ok(Ok(out)) => Ok(ok_variant(Value::String(out.into()))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::read_to_string"))),
        Err(v) => Ok(v),
    }
}

pub(crate) fn builtin_fs_file_write(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::write: missing handle"));
    };
    let bytes = crate::stdlib_builtins::crypto::value_to_bytes(args.get(1).unwrap_or(&Value::Unit));
    let written = bytes.len() as i64;
    match with_file_inline(id, "fs-file-write", "File::write", move |file| {
        file.write_all(&bytes)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Int(written))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::write"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::write_bytes(data) -> Result<i64, Error>`: one write against
/// the handle's cursor, answering the byte count it transferred.
pub(crate) fn builtin_fs_file_write_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::write_bytes: missing handle"));
    };
    let bytes = crate::stdlib_builtins::crypto::value_to_bytes(args.get(1).unwrap_or(&Value::Unit));
    match with_file_inline(
        id,
        "fs-file-write-bytes",
        "File::write_bytes",
        move |file| file.write(&bytes),
    ) {
        Ok(Ok(n)) => Ok(ok_variant(Value::Int(n as i64))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::write_bytes"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::read_at(len, offset) -> Result<Vec<u8>, Error>`.
pub(crate) fn builtin_fs_file_read_at(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::read_at: missing handle"));
    };
    let len = args.get(1).and_then(value_to_int).unwrap_or(0);
    let offset = args.get(2).and_then(value_to_int).unwrap_or(0);
    if len < 0 || offset < 0 {
        return Ok(err_variant(
            "File::read_at: length and offset must be non-negative",
        ));
    }
    let cap = len.min(1 << 24) as usize;
    let at = offset as u64;
    match with_file_inline(id, "fs-file-read-at", "File::read_at", move |file| {
        let mut buf = vec![0u8; cap];
        read_at_offset(file, &mut buf, at).map(|n| {
            buf.truncate(n);
            buf
        })
    }) {
        Ok(Ok(buf)) => Ok(ok_variant(bytes_value(&buf))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::read_at"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::read_at_into(buf, len, offset) -> Result<i64, Error>`: the
/// VM hands the `&mut Vec<u8>` over as a write-back cell, which holds
/// exactly the bytes read afterwards.
pub(crate) fn builtin_fs_file_read_at_into(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::read_at_into: missing handle"));
    };
    let Some(Value::MutCell(cell)) = args.get(1) else {
        return Ok(err_variant(
            "File::read_at_into: the buffer must be a `&mut Vec<u8>`",
        ));
    };
    let len = args.get(2).and_then(value_to_int).unwrap_or(0);
    let offset = args.get(3).and_then(value_to_int).unwrap_or(0);
    if len < 0 || offset < 0 {
        return Ok(err_variant(
            "File::read_at_into: length and offset must be non-negative",
        ));
    }
    let cap = len.min(1 << 24) as usize;
    let at = offset as u64;
    match with_file_inline(id, "fs-file-read-at", "File::read_at_into", move |file| {
        let mut buf = vec![0u8; cap];
        read_at_offset(file, &mut buf, at).map(|n| {
            buf.truncate(n);
            buf
        })
    }) {
        Ok(Ok(buf)) => {
            let n = buf.len() as i64;
            *cell.lock() = Value::ByteVec(Arc::new(buf));
            Ok(ok_variant(Value::Int(n)))
        }
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::read_at_into"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::write_at(data, offset) -> Result<i64, Error>`.
pub(crate) fn builtin_fs_file_write_at(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::write_at: missing handle"));
    };
    let bytes = crate::stdlib_builtins::crypto::value_to_bytes(args.get(1).unwrap_or(&Value::Unit));
    let offset = args.get(2).and_then(value_to_int).unwrap_or(0);
    if offset < 0 {
        return Ok(err_variant("File::write_at: offset must be non-negative"));
    }
    let at = offset as u64;
    match with_file_inline(id, "fs-file-write-at", "File::write_at", move |file| {
        write_at_offset(file, &bytes, at)
    }) {
        Ok(Ok(n)) => Ok(ok_variant(Value::Int(n as i64))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::write_at"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::seek(offset, whence) -> Result<i64, Error>`.
pub(crate) fn builtin_fs_file_seek(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::seek: missing handle"));
    };
    let offset = args.get(1).and_then(value_to_int).unwrap_or(0);
    let whence = args.get(2).and_then(value_to_int).unwrap_or(0);
    let from = match seek_from(offset, whence) {
        Ok(from) => from,
        Err(v) => return Ok(v),
    };
    match with_file_inline(id, "fs-file-seek", "File::seek", move |file| {
        std::io::Seek::seek(file, from)
    }) {
        Ok(Ok(pos)) => Ok(ok_variant(Value::Int(pos as i64))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::seek"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::set_len(len) -> Result<(), Error>`.
pub(crate) fn builtin_fs_file_set_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::set_len: missing handle"));
    };
    let len = args.get(1).and_then(value_to_int).unwrap_or(0);
    if len < 0 {
        return Ok(err_variant("File::set_len: length must be non-negative"));
    }
    let len = len as u64;
    match with_file_inline(id, "fs-file-set-len", "File::set_len", move |file| {
        file.set_len(len)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::set_len"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::len() -> Result<i64, Error>`.
pub(crate) fn builtin_fs_file_len(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::len: missing handle"));
    };
    match with_file_inline(id, "fs-file-len", "File::len", |file| {
        file.metadata().map(|m| m.len())
    }) {
        Ok(Ok(len)) => Ok(ok_variant(Value::Int(len as i64))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::len"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::sync_all() -> Result<(), Error>`.
pub(crate) fn builtin_fs_file_sync_all(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::sync_all: missing handle"));
    };
    match with_file_inline(id, "fs-file-sync-all", "File::sync_all", |file| {
        file.sync_all()
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::sync_all"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::sync_data() -> Result<(), Error>`.
pub(crate) fn builtin_fs_file_sync_data(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::sync_data: missing handle"));
    };
    match with_file_inline(id, "fs-file-sync-data", "File::sync_data", |file| {
        file.sync_data()
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::sync_data"))),
        Err(v) => Ok(v),
    }
}

/// `fs::sync_dir(path) -> Result<(), Error>`.
pub(crate) fn builtin_fs_sync_dir(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "fs::sync_dir", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path_context = path.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-sync-dir", move || sync_directory(&path))
    {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &path_context))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `fs::File::try_lock_range(start, len, exclusive) -> Result<bool, Error>`.
pub(crate) fn builtin_fs_file_try_lock_range(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::try_lock_range: missing handle"));
    };
    let start = args.get(1).and_then(value_to_int).unwrap_or(0);
    let len = args.get(2).and_then(value_to_int).unwrap_or(0);
    let exclusive = matches!(args.get(3), Some(Value::Bool(true)));
    if start < 0 || len < 0 {
        return Ok(err_variant(
            "File::try_lock_range: start and len must be non-negative",
        ));
    }
    let (start, len) = (start as u64, len as u64);
    match with_file_inline(id, "fs-file-lock", "File::try_lock_range", move |file| {
        try_lock_range_on(file, start, len, exclusive)
    }) {
        Ok(Ok(acquired)) => Ok(ok_variant(Value::Bool(acquired))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::try_lock_range"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::unlock_range(start, len) -> Result<(), Error>`.
pub(crate) fn builtin_fs_file_unlock_range(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::unlock_range: missing handle"));
    };
    let start = args.get(1).and_then(value_to_int).unwrap_or(0);
    let len = args.get(2).and_then(value_to_int).unwrap_or(0);
    if start < 0 || len < 0 {
        return Ok(err_variant(
            "File::unlock_range: start and len must be non-negative",
        ));
    }
    let (start, len) = (start as u64, len as u64);
    match with_file_inline(id, "fs-file-unlock", "File::unlock_range", move |file| {
        unlock_range_on(file, start, len)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::unlock_range"))),
        Err(v) => Ok(v),
    }
}

/// `fs::File::try_lock_shared() -> Result<bool, Error>`.
pub(crate) fn builtin_fs_file_try_lock_shared(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    builtin_fs_file_try_lock_range(&[handle, Value::Int(0), Value::Int(0), Value::Bool(false)])
}

/// `fs::File::try_lock_exclusive() -> Result<bool, Error>`.
pub(crate) fn builtin_fs_file_try_lock_exclusive(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    builtin_fs_file_try_lock_range(&[handle, Value::Int(0), Value::Int(0), Value::Bool(true)])
}

/// `fs::File::unlock() -> Result<(), Error>`.
pub(crate) fn builtin_fs_file_unlock(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args.first().cloned().unwrap_or(Value::Unit);
    builtin_fs_file_unlock_range(&[handle, Value::Int(0), Value::Int(0)])
}

/// Byte sequence in the packed `Vec<u8>` representation the rest of the
/// byte surface hands back.
fn bytes_value(bytes: &[u8]) -> Value {
    Value::ByteVec(Arc::new(bytes.to_vec()))
}

pub(crate) fn builtin_fs_file_flush(args: &[Value]) -> RuntimeResult<Value> {
    let Some(id) = args.first().and_then(handle_id) else {
        return Ok(err_variant("File::flush: missing handle"));
    };
    match with_file_inline(id, "fs-file-flush", "File::flush", |file| file.flush()) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, "File::flush"))),
        Err(v) => Ok(v),
    }
}

pub(crate) fn builtin_fs_file_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(id) = args.first().and_then(handle_id) {
        FS_FILE_REGISTRY.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_fs_metadata_raw(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::metadata", &path)?;
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
            Ok(ok_variant(Value::Tuple(Arc::from(vec![
                Value::Int(i64::try_from(meta.len()).unwrap_or(i64::MAX)),
                Value::Bool(meta.is_file()),
                Value::Bool(meta.is_dir()),
                Value::Bool(meta.file_type().is_symlink()),
                Value::Bool(meta.permissions().readonly()),
                Value::Int(modified),
            ]))))
        }
        Err(e) => Ok(err_variant(format!("metadata: {e}"))),
    }
}

pub(crate) fn builtin_fs_metadata(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::metadata", &path)?;
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let fields = vec![
                (
                    "size",
                    Value::Int(i64::try_from(meta.len()).unwrap_or(i64::MAX)),
                ),
                ("is_file", Value::Bool(meta.is_file())),
                ("is_dir", Value::Bool(meta.is_dir())),
                ("is_symlink", Value::Bool(meta.file_type().is_symlink())),
                ("readonly", Value::Bool(meta.permissions().readonly())),
                (
                    "modified_unix_ms",
                    Value::Int(
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                    ),
                ),
            ];
            Ok(ok_variant(Value::struct_(
                "fs::Metadata",
                Arc::unwrap_or_clone(Arc::new(fields)),
            )))
        }
        Err(e) => Ok(err_variant(format!("metadata: {e}"))),
    }
}

/// `fs::permissions(path) -> Result<i64, errors::Error>` - the
/// permission bits of `path`, in the chmod(2) encoding.
pub(crate) fn builtin_fs_permissions(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "permissions", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let path = gossamer_runtime::comptime_paths::resolve(&path);
    crate::comptime_gate::guard_read("fs::permissions", &path)?;
    let context = path.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-permissions", move || {
        gossamer_runtime::fs_mode::read(std::path::Path::new(&path))
    }) {
        Ok(Ok(mode)) => Ok(ok_variant(Value::Int(i64::from(mode)))),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &context))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `fs::set_permissions(path, mode) -> Result<(), errors::Error>`.
pub(crate) fn builtin_fs_set_permissions(args: &[Value]) -> RuntimeResult<Value> {
    mode_call(args, "set_permissions", |path, mode| {
        gossamer_runtime::fs_mode::apply(path, mode)
    })
}

/// `fs::create_dir_mode(path, mode) -> Result<(), errors::Error>`.
pub(crate) fn builtin_fs_create_dir_mode(args: &[Value]) -> RuntimeResult<Value> {
    mode_call(args, "create_dir_mode", |path, mode| {
        gossamer_runtime::fs_mode::create_dir(path, mode)
    })
}

/// `fs::create_dir_all_mode(path, mode) -> Result<(), errors::Error>`.
pub(crate) fn builtin_fs_create_dir_all_mode(args: &[Value]) -> RuntimeResult<Value> {
    mode_call(args, "create_dir_all_mode", |path, mode| {
        gossamer_runtime::fs_mode::create_dir_all(path, mode)
    })
}

/// The shared shape of the mode-taking calls: a path, a mode, and a
/// `Result<(), errors::Error>` carrying the same text the compiled
/// tiers produce.
fn mode_call(
    args: &[Value],
    name: &str,
    operation: impl FnOnce(&std::path::Path, u32) -> std::io::Result<()> + Send + 'static,
) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, name, "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let Some(mode) = args.get(1).and_then(value_to_int) else {
        return Ok(err_variant(format!("{name}: expected integer mode")));
    };
    let mode = gossamer_runtime::fs_mode::bits(mode);
    let context = path.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-mode", move || {
        operation(std::path::Path::new(&path), mode)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &context))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `fs::write_mode(path, contents, mode) -> Result<(), errors::Error>`.
pub(crate) fn builtin_fs_write_mode(args: &[Value]) -> RuntimeResult<Value> {
    let path = match arg_str_at(args, 0, "write_mode", "path") {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };
    let Some(contents) = args.get(1).and_then(as_str).map(str::as_bytes) else {
        return Ok(err_variant("write_mode: expected string contents"));
    };
    let contents = contents.to_vec();
    let Some(mode) = args.get(2).and_then(value_to_int) else {
        return Ok(err_variant("write_mode: expected integer mode"));
    };
    let mode = gossamer_runtime::fs_mode::bits(mode);
    let context = path.clone();
    match gossamer_runtime::sched_global::run_blocking("fs-write-mode", move || {
        gossamer_runtime::fs_mode::write(std::path::Path::new(&path), &contents, mode)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(classify_io_error(&e, &context))),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(test)]
mod file_handle_tests {
    use super::*;

    fn ok_payload(value: Value) -> Value {
        match value {
            Value::Variant(inner) if inner.name.as_str() == "Ok" => inner
                .fields
                .first()
                .cloned()
                .expect("Ok payload should exist"),
            other => panic!("expected Ok variant, got {other:?}"),
        }
    }

    fn temp_path(name: &str) -> String {
        let mut path = gossamer_runtime::platform::temp_dir();
        path.push(format!(
            "gossamer-interp-{name}-{}-{}.txt",
            gossamer_runtime::platform::process_id(),
            next_fs_handle()
        ));
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn file_handle_create_write_reopen_read_to_string() {
        let path = temp_path("file-handle");
        let created = ok_payload(
            builtin_fs_file_create(&[Value::String(path.clone().into())]).expect("create"),
        );
        ok_payload(
            builtin_fs_file_write(&[created.clone(), Value::String(SmolStr::from("hello file"))])
                .expect("write"),
        );
        ok_payload(builtin_fs_file_flush(std::slice::from_ref(&created)).expect("flush"));
        assert!(matches!(
            builtin_fs_file_close(std::slice::from_ref(&created)).expect("close"),
            Value::Unit
        ));

        let opened =
            ok_payload(builtin_fs_file_open(&[Value::String(path.clone().into())]).expect("open"));
        let read = ok_payload(
            builtin_fs_file_read_to_string(std::slice::from_ref(&opened)).expect("read"),
        );
        assert!(matches!(read, Value::String(s) if s.as_str() == "hello file"));
        let _ = builtin_fs_file_close(&[opened]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_options_handle_opens_with_configured_flags() {
        let path = temp_path("open-options");
        let opts = builtin_fs_open_options_new(&[]).expect("new");
        let opts = builtin_fs_open_options_write(&[opts, Value::Bool(true)]).expect("write");
        let opts = builtin_fs_open_options_create(&[opts, Value::Bool(true)]).expect("create");
        let opts = builtin_fs_open_options_truncate(&[opts, Value::Bool(true)]).expect("truncate");
        let file = ok_payload(
            builtin_fs_open_options_open(&[opts, Value::String(path.clone().into())])
                .expect("open"),
        );
        ok_payload(
            builtin_fs_file_write(&[file.clone(), Value::String(SmolStr::from("via opts"))])
                .expect("write"),
        );
        let _ = builtin_fs_file_close(&[file]);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "via opts"
        );
        let _ = std::fs::remove_file(path);
    }
}

// ----------------------------------------------------------------------
// bufio extras
