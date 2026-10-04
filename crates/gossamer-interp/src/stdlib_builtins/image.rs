//! Interpreter bridge for the opaque `std::image::Image` handle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use gossamer_std::{encoding::base64, image as image_std};

use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{RuntimeError, RuntimeResult, Value};

use super::as_str;

static IMAGES: LazyLock<parking_lot::ReentrantMutex<RefCell<HashMap<i64, image_std::Image>>>> =
    LazyLock::new(|| parking_lot::ReentrantMutex::new(RefCell::new(HashMap::new())));
static NEXT_IMAGE_ID: AtomicI64 = AtomicI64::new(1);

fn with_images<R>(f: impl FnOnce(&RefCell<HashMap<i64, image_std::Image>>) -> R) -> R {
    let guard = IMAGES.lock();
    f(&guard)
}

fn id(value: &Value) -> Option<i64> {
    value_to_int(value).filter(|id| *id > 0)
}

fn int(args: &[Value], n: usize) -> i64 {
    args.get(n).and_then(value_to_int).unwrap_or(0)
}
fn non_negative_u32(args: &[Value], n: usize, label: &str) -> RuntimeResult<u32> {
    let value = int(args, n);
    if value < 0 {
        return Err(RuntimeError::Type(format!("{label} must be non-negative")));
    }
    u32::try_from(value).map_err(|_| RuntimeError::Type(format!("{label} is too large")))
}
fn bounded_u8(args: &[Value], n: usize, min: i64, max: i64, label: &str) -> RuntimeResult<u8> {
    let value = int(args, n);
    if value < min || value > max {
        return Err(RuntimeError::Type(format!(
            "{label} must be between {min} and {max}"
        )));
    }
    u8::try_from(value).map_err(|_| RuntimeError::Type(format!("{label} is too large")))
}
fn channel(value: i64, shift: u32) -> u8 {
    ((value >> shift) & 0xff) as u8
}
fn rgba(value: i64) -> image_std::Rgba {
    image_std::Rgba::new(
        channel(value, 24),
        channel(value, 16),
        channel(value, 8),
        channel(value, 0),
    )
}
fn pack(value: image_std::Rgba) -> i64 {
    (i64::from(value.r) << 24)
        | (i64::from(value.g) << 16)
        | (i64::from(value.b) << 8)
        | i64::from(value.a)
}

pub(crate) fn install_image(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("new", image_new),
        ("filled", image_filled),
        ("decode_base64", image_decode_base64),
        ("width", image_width),
        ("height", image_height),
        ("pixel", image_pixel),
        ("set_pixel", image_set_pixel),
        ("from_rgba_bytes", image_from_rgba_bytes),
        ("to_rgba_bytes", image_to_rgba_bytes),
        ("encode_png_base64", image_encode_png),
        ("encode_jpeg_base64", image_encode_jpeg),
    ];
    for (name, call) in entries {
        let qualified: &'static str = Box::leak(format!("image::{name}").into_boxed_str());
        globals.push((qualified, crate::builtins::builtin_pub(qualified, *call)));
    }
}

fn insert(image: image_std::Image) -> Value {
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    with_images(|images| {
        images.borrow_mut().insert(id, image);
    });
    Value::Int(id)
}
fn image_new(args: &[Value]) -> RuntimeResult<Value> {
    let width = non_negative_u32(args, 0, "image::new: width")?;
    let height = non_negative_u32(args, 1, "image::new: height")?;
    Ok(image_std::Image::new(width, height).map_or(Value::Int(0), insert))
}
fn image_filled(args: &[Value]) -> RuntimeResult<Value> {
    let width = non_negative_u32(args, 0, "image::filled: width")?;
    let height = non_negative_u32(args, 1, "image::filled: height")?;
    Ok(image_std::Image::filled(width, height, rgba(int(args, 2))).map_or(Value::Int(0), insert))
}
fn image_decode_base64(args: &[Value]) -> RuntimeResult<Value> {
    let decoded = args
        .first()
        .and_then(as_str)
        .and_then(|text| base64::decode(text).ok());
    Ok(decoded
        .and_then(|bytes| image_std::decode(&bytes).ok())
        .map_or(Value::Int(0), insert))
}
fn image_width(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        id(args.first().unwrap_or(&Value::Unit))
            .and_then(|id| {
                with_images(|images| {
                    images
                        .borrow()
                        .get(&id)
                        .map(|image| i64::from(image.width()))
                })
            })
            .unwrap_or(0),
    ))
}
fn image_height(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        id(args.first().unwrap_or(&Value::Unit))
            .and_then(|id| {
                with_images(|images| {
                    images
                        .borrow()
                        .get(&id)
                        .map(|image| i64::from(image.height()))
                })
            })
            .unwrap_or(0),
    ))
}
fn image_pixel(args: &[Value]) -> RuntimeResult<Value> {
    let x = non_negative_u32(args, 1, "image::pixel: x")?;
    let y = non_negative_u32(args, 2, "image::pixel: y")?;
    Ok(Value::Int(
        id(args.first().unwrap_or(&Value::Unit))
            .and_then(|id| {
                with_images(|images| {
                    images
                        .borrow()
                        .get(&id)
                        .and_then(|image| image.pixel(x, y))
                        .map(pack)
                })
            })
            .unwrap_or(-1),
    ))
}
fn image_from_rgba_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let width = non_negative_u32(args, 0, "image::from_rgba_bytes: width")?;
    let height = non_negative_u32(args, 1, "image::from_rgba_bytes: height")?;
    let bytes = args.get(2).map(Value::bytes_or_empty).unwrap_or_default();
    Ok(image_std::Image::from_rgba8(width, height, bytes).map_or(Value::Int(0), insert))
}
fn image_to_rgba_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = id(args.first().unwrap_or(&Value::Unit))
        .and_then(|id| {
            with_images(|images| images.borrow().get(&id).map(image_std::Image::to_rgba8))
        })
        .unwrap_or_default();
    Ok(Value::ByteVec(std::sync::Arc::new(bytes)))
}
fn image_set_pixel(args: &[Value]) -> RuntimeResult<Value> {
    let x = non_negative_u32(args, 1, "image::set_pixel: x")?;
    let y = non_negative_u32(args, 2, "image::set_pixel: y")?;
    let changed = id(args.first().unwrap_or(&Value::Unit)).is_some_and(|id| {
        with_images(|images| {
            images
                .borrow_mut()
                .get_mut(&id)
                .is_some_and(|image| image.set_pixel(x, y, rgba(int(args, 3))))
        })
    });
    Ok(Value::Bool(changed))
}
fn image_encode_png(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        id(args.first().unwrap_or(&Value::Unit))
            .and_then(|id| {
                with_images(|images| {
                    images
                        .borrow()
                        .get(&id)
                        .and_then(|image| image_std::encode_png(image).ok())
                        .map(|bytes| base64::encode(&bytes))
                })
            })
            .unwrap_or_default()
            .into(),
    ))
}
fn image_encode_jpeg(args: &[Value]) -> RuntimeResult<Value> {
    let quality = bounded_u8(args, 1, 1, 100, "image::encode_jpeg: quality")?;
    Ok(Value::String(
        id(args.first().unwrap_or(&Value::Unit))
            .and_then(|id| {
                with_images(|images| {
                    images
                        .borrow()
                        .get(&id)
                        .and_then(|image| image_std::encode_jpeg(image, quality).ok())
                        .map(|bytes| base64::encode(&bytes))
                })
            })
            .unwrap_or_default()
            .into(),
    ))
}
