//! Native leaves under the `std::image` wrapper source. An `image::Image` is a
//! Gossamer value holding its size and RGBA8 bytes; these calls decode,
//! encode, fill, and resample those bytes, each checking the pixel limit
//! before it allocates.

use std::io::Write;

use super::result::gos_rt_result_new;
use super::vec::GosVec;
use crate::image_ops::ByteStore;

/// A packed `Vec<u8>` an image operation writes its answer into in place, so
/// compiled code receives the bytes without a copy. An answer dropped before
/// [`GosBytes::into_raw`] is freed.
struct GosBytes(*mut GosVec);

impl GosBytes {
    /// The vector, handed to its new owner.
    fn into_raw(self) -> *mut GosVec {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for GosBytes {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live vector this value alone holds.
        unsafe { crate::c_abi::map::gos_rt_vec_free(self.0) };
    }
}

impl ByteStore for GosBytes {
    fn zeroed(len: usize) -> Self {
        let raw = super::vec::gos_rt_vec_with_capacity(1, i64::try_from(len).unwrap_or(i64::MAX));
        // SAFETY: a non-negative capacity answers a live header this value
        // alone holds, with room for `len` bytes.
        unsafe {
            let header = &mut *raw;
            if len > 0 && !header.ptr.is_null() {
                std::ptr::write_bytes(header.ptr.as_ptr(), 0, len);
                header.len = len as i64;
            }
        }
        Self(raw)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the vector holds `len` initialised bytes at `ptr`, and this
        // value is its only holder for the borrow.
        unsafe {
            let header = &mut *self.0;
            if header.ptr.is_null() {
                return &mut [];
            }
            std::slice::from_raw_parts_mut(header.ptr.as_ptr(), header.len.max(0) as usize)
        }
    }
}

impl Write for GosBytes {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: the vector is live and held by this value alone; the reserve
        // leaves room for `buf` past its length.
        unsafe {
            let len = (*self.0).len;
            super::vec::gos_rt_vec_reserve_at_least(self.0, len + buf.len() as i64);
            let header = &mut *self.0;
            std::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                header.ptr.as_ptr().add(len as usize),
                buf.len(),
            );
            header.len = len + buf.len() as i64;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn err_result(message: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, err as i64)
}

fn bytes_result(answer: Result<GosBytes, String>) -> i128 {
    match answer {
        Ok(bytes) => gos_rt_result_new(0, bytes.into_raw() as i64),
        Err(message) => err_result(&message),
    }
}

/// Layout of [`gos_rt_image_decode_raw`]'s `(i64, i64, Vec<u8>)` answer: the
/// width, the height, then the pixel bytes the blob owns.
static DECODED_META: [i64; 5] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    1,
    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 2,
];

/// The pixel bytes of a checked `width` by `height` image, or the error.
///
/// # Safety
/// `rgba` is null or a live vector for the call.
unsafe fn pixels<'a>(
    rgba: *const GosVec,
    width: i64,
    height: i64,
    context: &str,
) -> Result<std::borrow::Cow<'a, [u8]>, String> {
    let len = crate::image_ops::checked_len(context, width, height)?;
    // SAFETY: this `unsafe fn`'s caller passes `rgba` live or null for the
    // call, which `vec_bytes_cow` accepts; the borrow ends with the call.
    let bytes = unsafe { super::vec::vec_bytes_cow(rgba) };
    if bytes.len() != len {
        return Err(format!(
            "{context}: a {width}x{height} image holds {len} bytes, not {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// `__gos_image_decode_raw(bytes) -> Result<(i64, i64, Vec<u8>), Error>`: a
/// PNG, JPEG, or GIF (its first frame) as its size and RGBA8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_decode_raw(bytes: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `bytes` is this shim's argument, live for the call or null
        // (C-ABI contract), which `vec_bytes` accepts.
        let bytes = unsafe { super::vec::vec_bytes(bytes) };
        match crate::image_ops::decode::<GosBytes>(&bytes) {
            Ok(decoded) => {
                let words = [
                    i64::from(decoded.width),
                    i64::from(decoded.height),
                    decoded.rgba.into_raw() as i64,
                ];
                let blob = crate::c_abi::rc::counted_words(&words, &DECODED_META);
                gos_rt_result_new(0, blob as i64)
            }
            Err(message) => err_result(&message),
        }
    })
}

/// `__gos_image_filled_raw(width, height, r, g, b, a) -> Result<Vec<u8>, Error>`:
/// the bytes of an image of one colour.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_filled_raw(
    width: i64,
    height: i64,
    r: i64,
    g: i64,
    b: i64,
    a: i64,
) -> i128 {
    ffi_entry!({
        bytes_result(
            crate::image_ops::checked_len("Image::filled", width, height)
                .map(|len| crate::image_ops::fill(len, [r as u8, g as u8, b as u8, a as u8])),
        )
    })
}

/// `__gos_image_check_raw(width, height, len) -> Result<(), Error>`: whether
/// `len` bytes make a `width` by `height` image within the pixel limit.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_check_raw(width: i64, height: i64, len: i64) -> i128 {
    ffi_entry!({
        match crate::image_ops::checked_len("Image::from_rgba", width, height) {
            Ok(expected) if usize::try_from(len) == Ok(expected) => gos_rt_result_new(0, 0),
            Ok(expected) => err_result(&format!(
                "Image::from_rgba: a {width}x{height} image holds {expected} bytes, not {len}"
            )),
            Err(message) => err_result(&message),
        }
    })
}

/// `__gos_image_resize_raw(rgba, width, height, new_width, new_height)
/// -> Result<Vec<u8>, Error>`: the bytes resampled with a Lanczos-3 filter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_resize_raw(
    rgba: *const GosVec,
    width: i64,
    height: i64,
    new_width: i64,
    new_height: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `rgba` is this shim's argument, live for the call or null.
        let answer = unsafe { pixels(rgba, width, height, "Image::resize") }.and_then(|bytes| {
            crate::image_ops::checked_len("Image::resize", new_width, new_height)?;
            Ok(crate::image_ops::resize_rgba(
                &bytes,
                width as usize,
                height as usize,
                new_width as usize,
                new_height as usize,
            ))
        });
        bytes_result(answer)
    })
}

/// `__gos_image_encode_png_raw(rgba, width, height) -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_encode_png_raw(
    rgba: *const GosVec,
    width: i64,
    height: i64,
) -> i128 {
    ffi_entry!({
        // SAFETY: `rgba` is this shim's argument, live for the call or null.
        let answer = unsafe { pixels(rgba, width, height, "Image::encode_png") }
            .and_then(|bytes| crate::image_ops::encode_png(&bytes, width as u32, height as u32));
        bytes_result(answer)
    })
}

/// `__gos_image_encode_jpeg_raw(rgba, width, height, quality)
/// -> Result<Vec<u8>, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_encode_jpeg_raw(
    rgba: *const GosVec,
    width: i64,
    height: i64,
    quality: i64,
) -> i128 {
    ffi_entry!({
        let Some(quality) = u8::try_from(quality).ok().filter(|q| (1..=100).contains(q)) else {
            return err_result(&format!(
                "Image::encode_jpeg: quality runs from 1 through 100, not {quality}"
            ));
        };
        // SAFETY: `rgba` is this shim's argument, live for the call or null.
        let answer =
            unsafe { pixels(rgba, width, height, "Image::encode_jpeg") }.and_then(|bytes| {
                crate::image_ops::encode_jpeg(&bytes, width as u32, height as u32, quality)
            });
        bytes_result(answer)
    })
}

/// `__gos_image_set_max_pixels_raw(limit) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_set_max_pixels_raw(limit: i64) -> i128 {
    ffi_entry!({
        match crate::image_ops::set_max_pixels(limit) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(message) => err_result(&message),
        }
    })
}
