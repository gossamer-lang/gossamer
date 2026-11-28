//! Interpreter leaves under the `std::image` wrapper source, over the same
//! runtime codecs, limit, and resampler compiled code calls.

use std::borrow::Cow;
use std::sync::Arc;

use gossamer_runtime::image_ops;

use crate::builtins::{BuiltinFnPub, err_variant, ok_variant, value_to_int};
use crate::value::{RuntimeResult, Value};

pub(crate) fn install_image(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&'static str, BuiltinFnPub)] = &[
        ("__gos_image_decode_raw", image_decode_raw),
        ("__gos_image_filled_raw", image_filled_raw),
        ("__gos_image_check_raw", image_check_raw),
        ("__gos_image_resize_raw", image_resize_raw),
        ("__gos_image_encode_png_raw", image_encode_png_raw),
        ("__gos_image_encode_jpeg_raw", image_encode_jpeg_raw),
        ("__gos_image_set_max_pixels_raw", image_set_max_pixels_raw),
    ];
    for (name, call) in entries {
        globals.push((name, crate::builtins::builtin_pub(name, *call)));
    }
}

fn int(args: &[Value], n: usize) -> i64 {
    args.get(n).and_then(value_to_int).unwrap_or(0)
}

/// The bytes of argument `n`, lent where the value already holds them.
fn bytes(args: &[Value], n: usize) -> Cow<'_, [u8]> {
    args.get(n)
        .and_then(Value::byte_slice)
        .unwrap_or(Cow::Borrowed(&[]))
}

fn byte_vec(bytes: Vec<u8>) -> Value {
    Value::ByteVec(Arc::new(bytes))
}

fn bytes_answer(answer: Result<Vec<u8>, String>) -> Value {
    match answer {
        Ok(bytes) => ok_variant(byte_vec(bytes)),
        Err(message) => err_variant(message),
    }
}

/// The pixel bytes of a checked `width` by `height` image, or the error.
fn pixels<'a>(args: &'a [Value], context: &str) -> Result<(Cow<'a, [u8]>, i64, i64), String> {
    let (width, height) = (int(args, 1), int(args, 2));
    let len = image_ops::checked_len(context, width, height)?;
    let rgba = bytes(args, 0);
    if rgba.len() != len {
        return Err(format!(
            "{context}: a {width}x{height} image holds {len} bytes, not {}",
            rgba.len()
        ));
    }
    Ok((rgba, width, height))
}

fn image_decode_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match image_ops::decode(&bytes(args, 0)) {
        Ok(decoded) => ok_variant(Value::Tuple(Arc::new(vec![
            Value::Int(i64::from(decoded.width)),
            Value::Int(i64::from(decoded.height)),
            byte_vec(decoded.rgba),
        ]))),
        Err(message) => err_variant(message),
    })
}

fn image_filled_raw(args: &[Value]) -> RuntimeResult<Value> {
    let color = [2, 3, 4, 5].map(|n| int(args, n) as u8);
    Ok(bytes_answer(
        image_ops::checked_len("Image::filled", int(args, 0), int(args, 1))
            .map(|len| image_ops::fill(len, color)),
    ))
}

fn image_check_raw(args: &[Value]) -> RuntimeResult<Value> {
    let (width, height, len) = (int(args, 0), int(args, 1), int(args, 2));
    Ok(
        match image_ops::checked_len("Image::from_rgba", width, height) {
            Ok(expected) if usize::try_from(len) == Ok(expected) => ok_variant(Value::Unit),
            Ok(expected) => err_variant(format!(
                "Image::from_rgba: a {width}x{height} image holds {expected} bytes, not {len}"
            )),
            Err(message) => err_variant(message),
        },
    )
}

fn image_resize_raw(args: &[Value]) -> RuntimeResult<Value> {
    let (new_width, new_height) = (int(args, 3), int(args, 4));
    let answer = pixels(args, "Image::resize").and_then(|(rgba, width, height)| {
        image_ops::checked_len("Image::resize", new_width, new_height)?;
        Ok(image_ops::resize_rgba(
            &rgba,
            width as usize,
            height as usize,
            new_width as usize,
            new_height as usize,
        ))
    });
    Ok(bytes_answer(answer))
}

fn image_encode_png_raw(args: &[Value]) -> RuntimeResult<Value> {
    let answer = pixels(args, "Image::encode_png").and_then(|(rgba, width, height)| {
        image_ops::encode_png(&rgba, width as u32, height as u32)
    });
    Ok(bytes_answer(answer))
}

fn image_encode_jpeg_raw(args: &[Value]) -> RuntimeResult<Value> {
    let quality = int(args, 3);
    let Some(quality) = u8::try_from(quality).ok().filter(|q| (1..=100).contains(q)) else {
        return Ok(err_variant(format!(
            "Image::encode_jpeg: quality runs from 1 through 100, not {quality}"
        )));
    };
    let answer = pixels(args, "Image::encode_jpeg").and_then(|(rgba, width, height)| {
        image_ops::encode_jpeg(&rgba, width as u32, height as u32, quality)
    });
    Ok(bytes_answer(answer))
}

fn image_set_max_pixels_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(match image_ops::set_max_pixels(int(args, 0)) {
        Ok(()) => ok_variant(Value::Unit),
        Err(message) => err_variant(message),
    })
}
