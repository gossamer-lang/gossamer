//! `std::archive` builtins over `gossamer_runtime::codec::archive`, the
//! implementation compiled code shares.

use std::sync::Arc;

use gossamer_runtime::codec::archive::{self, Entry, EntryKind, Limits};

use super::{bytes_from_value, bytes_to_array};
use crate::builtins::{BuiltinFnPub, as_str, err_variant, none_variant, ok_variant, some_variant};
use crate::value::{RuntimeResult, Value};

pub(crate) fn install_archive_zip(globals: &mut Vec<(&'static str, Value)>) {
    install_format(globals, "zip", &ZIP);
}

pub(crate) fn install_archive_tar(globals: &mut Vec<(&'static str, Value)>) {
    install_format(globals, "tar", &TAR);
    globals.push((
        "archive::enclosed_path",
        crate::builtins::builtin_pub("archive::enclosed_path", builtin_enclosed_path),
    ));
}

/// The builtins one archive format installs.
struct FormatBuiltins {
    read: BuiltinFnPub,
    write: BuiltinFnPub,
    extract: BuiltinFnPub,
    read_raw: BuiltinFnPub,
    raw_name: &'static str,
}

const ZIP: FormatBuiltins = FormatBuiltins {
    read: builtin_archive_zip_read,
    write: builtin_archive_zip_write,
    extract: builtin_archive_zip_extract,
    read_raw: builtin_zip_read_raw,
    raw_name: "__gos_zip_read_raw",
};

const TAR: FormatBuiltins = FormatBuiltins {
    read: builtin_archive_tar_read,
    write: builtin_archive_tar_write,
    extract: builtin_archive_tar_extract,
    read_raw: builtin_tar_read_raw,
    raw_name: "__gos_tar_read_raw",
};

fn install_format(
    globals: &mut Vec<(&'static str, Value)>,
    format: &str,
    builtins: &FormatBuiltins,
) {
    for (short, call) in [
        ("read", builtins.read),
        ("write", builtins.write),
        ("extract", builtins.extract),
    ] {
        let q: &'static str = Box::leak(format!("archive::{format}::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
    // Leaf for the injected real-struct entry wrapper: each entry as a
    // `(name, data, is_dir)` tuple, under the limits its three count
    // arguments name (negative is unbounded).
    globals.push((
        builtins.raw_name,
        crate::builtins::builtin_pub(builtins.raw_name, builtins.read_raw),
    ));
}

fn int_arg(args: &[Value], index: usize) -> i64 {
    args.get(index)
        .and_then(crate::builtins::value_to_int)
        .unwrap_or(-1)
}

fn limits_of(args: &[Value]) -> Limits {
    Limits::from_counts(int_arg(args, 1), int_arg(args, 2), int_arg(args, 3))
}

fn entry_tuples(entries: Vec<Entry>) -> Value {
    Value::Array(Arc::new(
        entries
            .into_iter()
            .map(|e| {
                Value::Tuple(Arc::from(vec![
                    Value::String(e.name.into()),
                    bytes_to_array(e.data),
                    Value::Bool(e.kind == EntryKind::Dir),
                ]))
            })
            .collect(),
    ))
}

fn entry_structs(type_name: &'static str, entries: Vec<Entry>) -> Value {
    Value::Array(Arc::new(
        entries
            .into_iter()
            .map(|e| {
                Value::struct_(
                    type_name,
                    vec![
                        ("name", Value::String(e.name.into())),
                        ("data", bytes_to_array(e.data)),
                        ("is_dir", Value::Bool(e.kind == EntryKind::Dir)),
                    ],
                )
            })
            .collect(),
    ))
}

fn carrier(answer: Result<Value, String>) -> RuntimeResult<Value> {
    Ok(match answer {
        Ok(v) => ok_variant(v),
        Err(e) => err_variant(e),
    })
}

/// The `(name, data)` pairs an archive write takes.
fn name_data_pairs(arg: Option<&Value>) -> Vec<(String, Vec<u8>)> {
    let Some(Value::Array(arr)) = arg else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| match v {
            Value::Tuple(t) => {
                let name = match t.first()? {
                    Value::String(s) => s.as_str().to_string(),
                    _ => return None,
                };
                Some((name, bytes_from_value(t.get(1)?)))
            }
            _ => None,
        })
        .collect()
}

type Reader = fn(&[u8], Limits) -> Result<Vec<Entry>, String>;

fn extract_with(read: Reader, args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    let dir = args.get(1).and_then(as_str).unwrap_or("");
    carrier(
        read(&data, Limits::default())
            .and_then(|entries| archive::extract(&entries, std::path::Path::new(dir)))
            .map(|written| Value::Int(i64::try_from(written).unwrap_or(i64::MAX))),
    )
}

pub(crate) fn builtin_zip_read_raw(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    carrier(archive::zip_read(&data, limits_of(args)).map(entry_tuples))
}

pub(crate) fn builtin_tar_read_raw(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    carrier(archive::tar_read(&data, limits_of(args)).map(entry_tuples))
}

pub(crate) fn builtin_archive_zip_read(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    carrier(
        archive::zip_read(&data, Limits::default())
            .map(|entries| entry_structs("archive::ZipEntry", entries)),
    )
}

pub(crate) fn builtin_archive_tar_read(args: &[Value]) -> RuntimeResult<Value> {
    let data = bytes_from_value(args.first().unwrap_or(&Value::Unit));
    carrier(
        archive::tar_read(&data, Limits::default())
            .map(|entries| entry_structs("archive::TarEntry", entries)),
    )
}

pub(crate) fn builtin_archive_zip_write(args: &[Value]) -> RuntimeResult<Value> {
    carrier(archive::zip_write(&name_data_pairs(args.first())).map(bytes_to_array))
}

pub(crate) fn builtin_archive_tar_write(args: &[Value]) -> RuntimeResult<Value> {
    carrier(archive::tar_write(&name_data_pairs(args.first())).map(bytes_to_array))
}

pub(crate) fn builtin_archive_zip_extract(args: &[Value]) -> RuntimeResult<Value> {
    extract_with(archive::zip_read, args)
}

pub(crate) fn builtin_archive_tar_extract(args: &[Value]) -> RuntimeResult<Value> {
    extract_with(archive::tar_read, args)
}

pub(crate) fn builtin_enclosed_path(args: &[Value]) -> RuntimeResult<Value> {
    let name = args.first().and_then(as_str).unwrap_or("");
    Ok(match archive::enclosed_path(name) {
        Some(path) => some_variant(Value::String(path.into())),
        None => none_variant(),
    })
}
