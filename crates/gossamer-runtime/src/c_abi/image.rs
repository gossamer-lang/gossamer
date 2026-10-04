//! Native bridge for the public `std::image` opaque-handle API.

use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::string::alloc_cstring;

#[derive(Clone, Copy)]
struct Rgba(u8, u8, u8, u8);

struct Image {
    width: u32,
    height: u32,
    pixels: Vec<Rgba>,
}

impl Image {
    fn new(width: u32, height: u32) -> Option<Self> {
        Self::filled(width, height, Rgba(0, 0, 0, 0))
    }

    fn filled(width: u32, height: u32, color: Rgba) -> Option<Self> {
        let count = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
        Some(Self {
            width,
            height,
            pixels: vec![color; count],
        })
    }

    fn pixel(&self, x: u32, y: u32) -> Option<Rgba> {
        let index = usize::try_from(u64::from(y) * u64::from(self.width) + u64::from(x)).ok()?;
        (x < self.width && y < self.height).then(|| self.pixels[index])
    }

    fn set_pixel(&mut self, x: u32, y: u32, color: Rgba) -> bool {
        let Some(index) = usize::try_from(u64::from(y) * u64::from(self.width) + u64::from(x)).ok()
        else {
            return false;
        };
        if x >= self.width || y >= self.height {
            return false;
        }
        self.pixels[index] = color;
        true
    }

    fn rgba(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .flat_map(|p| [p.0, p.1, p.2, p.3])
            .collect()
    }

    fn from_rgba(width: u32, height: u32, bytes: Vec<u8>) -> Option<Self> {
        let count = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
        (bytes.len() == count.checked_mul(4)?).then(|| Self {
            width,
            height,
            pixels: bytes
                .chunks_exact(4)
                .map(|p| Rgba(p[0], p[1], p[2], p[3]))
                .collect(),
        })
    }
}

fn decode(bytes: &[u8]) -> Option<Image> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(bytes)
    } else {
        None
    }
}

fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    let bytes = &buffer[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => bytes.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    Image::from_rgba(info.width, info.height, rgba)
}

fn decode_jpeg(bytes: &[u8]) -> Option<Image> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let data = decoder.decode().ok()?;
    let info = decoder.info()?;
    let rgba = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        jpeg_decoder::PixelFormat::RGB24 => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        jpeg_decoder::PixelFormat::CMYK32 => data
            .chunks_exact(4)
            .flat_map(|p| {
                let (c, m, y, k) = (
                    u16::from(p[0]),
                    u16::from(p[1]),
                    u16::from(p[2]),
                    u16::from(p[3]),
                );
                [
                    (255 - (c + k).min(255)) as u8,
                    (255 - (m + k).min(255)) as u8,
                    (255 - (y + k).min(255)) as u8,
                    255,
                ]
            })
            .collect(),
        jpeg_decoder::PixelFormat::L16 => return None,
    };
    Image::from_rgba(u32::from(info.width), u32::from(info.height), rgba)
}

fn encode_png(image: &Image) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&image.rgba()).ok()?;
    drop(writer);
    Some(out)
}

fn encode_jpeg(image: &Image, quality: u8) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity(image.pixels.len() * 3);
    for Rgba(r, g, b, a) in &image.pixels {
        let alpha = u16::from(*a);
        rgb.extend([
            ((u16::from(*r) * alpha + 127) / 255) as u8,
            ((u16::from(*g) * alpha + 127) / 255) as u8,
            ((u16::from(*b) * alpha + 127) / 255) as u8,
        ]);
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100))
        .encode(
            &rgb,
            u16::try_from(image.width).ok()?,
            u16::try_from(image.height).ok()?,
            jpeg_encoder::ColorType::Rgb,
        )
        .ok()?;
    Some(out)
}

static IMAGES: OnceLock<Mutex<HashMap<i64, Image>>> = OnceLock::new();
static NEXT_IMAGE_ID: AtomicI64 = AtomicI64::new(1);

fn images() -> &'static Mutex<HashMap<i64, Image>> {
    IMAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_images() -> std::sync::MutexGuard<'static, HashMap<i64, Image>> {
    images()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn insert(image: Image) -> i64 {
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    lock_images().insert(id, image);
    id
}

fn dimension(value: i64) -> u32 {
    if value < 0 {
        crate::c_abi::panic::panic_text("image dimension must be non-negative");
    }
    u32::try_from(value).unwrap_or_else(|_| {
        crate::c_abi::panic::panic_text("image dimension is too large");
        0
    })
}

fn rgba(value: i64) -> Rgba {
    Rgba(
        ((value >> 24) & 0xff) as u8,
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

fn pack(value: Rgba) -> i64 {
    (i64::from(value.0) << 24)
        | (i64::from(value.1) << 16)
        | (i64::from(value.2) << 8)
        | i64::from(value.3)
}

unsafe fn input<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        return None;
    }
    // SAFETY: this `unsafe fn`'s caller passes `value` live; non-null, checked above.
    std::str::from_utf8(unsafe { crate::c_abi::gos_str_arg_bytes(value) }).ok()
}

/// `image::new(width, height) -> i64`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_new(width: i64, height: i64) -> i64 {
    ffi_entry!(0, {
        Image::new(dimension(width), dimension(height)).map_or(0, insert)
    })
}

/// `image::filled(width, height, rgba) -> i64`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_filled(width: i64, height: i64, color: i64) -> i64 {
    ffi_entry!(0, {
        Image::filled(dimension(width), dimension(height), rgba(color)).map_or(0, insert)
    })
}

/// Decodes a base64 PNG or JPEG, returning zero for malformed data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_decode_base64(text: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `text` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `input` accepts.
        unsafe { input(text) }
            .and_then(|text| super::encoding::base64_decode(text).ok())
            .and_then(|bytes| decode(&bytes))
            .map_or(0, insert)
    })
}

/// `image::width(handle) -> i64`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_width(handle: i64) -> i64 {
    ffi_entry!(0, {
        lock_images()
            .get(&handle)
            .map_or(0, |image| i64::from(image.width))
    })
}

/// `image::height(handle) -> i64`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_height(handle: i64) -> i64 {
    ffi_entry!(0, {
        lock_images()
            .get(&handle)
            .map_or(0, |image| i64::from(image.height))
    })
}

/// `image::pixel(handle, x, y) -> i64`, returning -1 outside the image.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_pixel(handle: i64, x: i64, y: i64) -> i64 {
    ffi_entry!(-1, {
        lock_images()
            .get(&handle)
            .and_then(|image| image.pixel(dimension(x), dimension(y)))
            .map_or(-1, pack)
    })
}

/// `image::from_rgba_bytes(width, height, bytes) -> i64`: an image over
/// `width * height` RGBA8 pixels, or 0 when `bytes` has another length.
///
/// # Safety
///
/// `bytes` is null or a live `Vec<u8>` for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_image_from_rgba_bytes(
    width: i64,
    height: i64,
    bytes: *const super::vec::GosVec,
) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `bytes` is this shim's argument, null or a live `Vec` (contract).
        let bytes = unsafe { super::vec::vec_bytes(bytes) };
        Image::from_rgba(dimension(width), dimension(height), bytes).map_or(0, insert)
    })
}

/// `image::to_rgba_bytes(handle) -> Vec<u8>`: the pixels as RGBA8 bytes, row
/// by row, empty for an invalid handle.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_to_rgba_bytes(handle: i64) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = lock_images()
            .get(&handle)
            .map(Image::rgba)
            .unwrap_or_default();
        super::encoding::bytes_to_gosvec(&bytes)
    })
}

/// `image::set_pixel(handle, x, y, rgba) -> bool`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_set_pixel(handle: i64, x: i64, y: i64, color: i64) -> i64 {
    ffi_entry!(0, {
        i64::from(
            lock_images()
                .get_mut(&handle)
                .is_some_and(|image| image.set_pixel(dimension(x), dimension(y), rgba(color))),
        )
    })
}

/// Encodes a handle as base64 PNG, returning an empty string for an invalid handle.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_encode_png_base64(handle: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = lock_images()
            .get(&handle)
            .and_then(encode_png)
            .map_or_else(String::new, |bytes| super::encoding::base64_encode(&bytes));
        alloc_cstring(text.as_bytes())
    })
}

/// Encodes a handle as base64 JPEG, returning an empty string for an invalid handle.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_image_encode_jpeg_base64(handle: i64, quality: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if !(1..=100).contains(&quality) {
            crate::c_abi::panic::panic_text(
                "image::encode_jpeg: quality must be between 1 and 100",
            );
        }
        let text = lock_images()
            .get(&handle)
            .and_then(|image| encode_jpeg(image, quality as u8))
            .map_or_else(String::new, |bytes| super::encoding::base64_encode(&bytes));
        alloc_cstring(text.as_bytes())
    })
}
