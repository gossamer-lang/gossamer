//! `std::compress` builtins over `gossamer_runtime::codec::compress`, the
//! implementation compiled code shares, so every tier answers the same bytes
//! and the same error text.

use gossamer_runtime::codec::compress::{self, Format};

use super::{bytes_from_value, bytes_to_array};
use crate::builtins::{BuiltinFnPub, err_variant, ok_variant, value_to_int};
use crate::value::{RuntimeResult, Value};

pub(crate) fn install_compress(globals: &mut Vec<(&'static str, Value)>) {
    for (short, call) in [
        ("gzip::encode", builtin_compress_gzip_encode as BuiltinFnPub),
        ("gzip::decode", builtin_compress_gzip_decode),
        ("gzip::decode_limited", builtin_compress_gzip_decode_limited),
        ("flate::compress", builtin_compress_flate_compress),
        ("flate::decompress", builtin_compress_flate_decompress),
        (
            "flate::decompress_limited",
            builtin_compress_flate_decompress_limited,
        ),
        ("zlib::compress", builtin_compress_zlib_compress),
        ("zlib::decompress", builtin_compress_zlib_decompress),
        (
            "zlib::decompress_limited",
            builtin_compress_zlib_decompress_limited,
        ),
    ] {
        let q: &'static str = Box::leak(format!("compress::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
    // zstd and bzip2 are backed by C libraries and unavailable in the wasm
    // sandbox; the gzip / flate / zlib codecs above use the pure-Rust flate2
    // backend and stay available on every target.
    #[cfg(not(target_arch = "wasm32"))]
    for (short, call) in [
        ("zstd::encode", builtin_compress_zstd_encode as BuiltinFnPub),
        ("zstd::encode_level", builtin_compress_zstd_encode_level),
        ("zstd::decode", builtin_compress_zstd_decode),
        ("zstd::decode_limited", builtin_compress_zstd_decode_limited),
        ("bzip2::compress", builtin_bzip2_compress),
        ("bzip2::decompress", builtin_bzip2_decompress),
        (
            "bzip2::decompress_limited",
            builtin_bzip2_decompress_limited,
        ),
    ] {
        let q: &'static str = Box::leak(format!("compress::{short}").into_boxed_str());
        globals.push((q, crate::builtins::builtin_pub(q, call)));
    }
}

/// The carrier a codec answer becomes.
fn carrier(answer: Result<Vec<u8>, String>) -> RuntimeResult<Value> {
    Ok(match answer {
        Ok(out) => ok_variant(bytes_to_array(out)),
        Err(e) => err_variant(e),
    })
}

fn input(args: &[Value]) -> Vec<u8> {
    bytes_from_value(args.first().unwrap_or(&Value::Unit))
}

fn int_arg(args: &[Value], index: usize, default: i64) -> i64 {
    args.get(index).and_then(value_to_int).unwrap_or(default)
}

fn compress_with(format: Format, args: &[Value], default_level: i64) -> RuntimeResult<Value> {
    carrier(compress::compress(
        format,
        &input(args),
        int_arg(args, 1, default_level),
    ))
}

fn decompress_with(format: Format, args: &[Value]) -> RuntimeResult<Value> {
    carrier(compress::decompress(format, &input(args), None))
}

fn decompress_limited_with(format: Format, args: &[Value]) -> RuntimeResult<Value> {
    carrier(
        compress::limit_of(int_arg(args, 1, 0))
            .and_then(|limit| compress::decompress(format, &input(args), Some(limit))),
    )
}

pub(crate) fn builtin_compress_gzip_encode(args: &[Value]) -> RuntimeResult<Value> {
    compress_with(Format::Gzip, args, 6)
}

pub(crate) fn builtin_compress_gzip_decode(args: &[Value]) -> RuntimeResult<Value> {
    decompress_with(Format::Gzip, args)
}

pub(crate) fn builtin_compress_gzip_decode_limited(args: &[Value]) -> RuntimeResult<Value> {
    decompress_limited_with(Format::Gzip, args)
}

pub(crate) fn builtin_compress_flate_compress(args: &[Value]) -> RuntimeResult<Value> {
    compress_with(Format::Deflate, args, 6)
}

pub(crate) fn builtin_compress_flate_decompress(args: &[Value]) -> RuntimeResult<Value> {
    decompress_with(Format::Deflate, args)
}

pub(crate) fn builtin_compress_flate_decompress_limited(args: &[Value]) -> RuntimeResult<Value> {
    decompress_limited_with(Format::Deflate, args)
}

pub(crate) fn builtin_compress_zlib_compress(args: &[Value]) -> RuntimeResult<Value> {
    compress_with(Format::Zlib, args, 6)
}

pub(crate) fn builtin_compress_zlib_decompress(args: &[Value]) -> RuntimeResult<Value> {
    decompress_with(Format::Zlib, args)
}

pub(crate) fn builtin_compress_zlib_decompress_limited(args: &[Value]) -> RuntimeResult<Value> {
    decompress_limited_with(Format::Zlib, args)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_compress_zstd_encode(args: &[Value]) -> RuntimeResult<Value> {
    carrier(compress::compress(
        Format::Zstd,
        &input(args),
        compress::ZSTD_DEFAULT_LEVEL,
    ))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_compress_zstd_encode_level(args: &[Value]) -> RuntimeResult<Value> {
    compress_with(Format::Zstd, args, compress::ZSTD_DEFAULT_LEVEL)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_compress_zstd_decode(args: &[Value]) -> RuntimeResult<Value> {
    decompress_with(Format::Zstd, args)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_compress_zstd_decode_limited(args: &[Value]) -> RuntimeResult<Value> {
    decompress_limited_with(Format::Zstd, args)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_bzip2_compress(args: &[Value]) -> RuntimeResult<Value> {
    compress_with(Format::Bzip2, args, 6)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_bzip2_decompress(args: &[Value]) -> RuntimeResult<Value> {
    decompress_with(Format::Bzip2, args)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn builtin_bzip2_decompress_limited(args: &[Value]) -> RuntimeResult<Value> {
    decompress_limited_with(Format::Bzip2, args)
}
