//! Image operations shared by `std::image` on every tier, over row-major
//! RGBA8 buffers: decoding PNG, JPEG, and GIF under one pixel limit, encoding
//! PNG and JPEG, filling, and resampling.

use std::io::{Cursor, Write};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

/// Pixels an image may hold unless `image::set_max_pixels` says otherwise:
/// Pillow's refusal threshold, twice its `MAX_IMAGE_PIXELS` of 89 478 485.
pub const DEFAULT_MAX_PIXELS: u64 = 178_956_970;

static MAX_PIXELS: AtomicU64 = AtomicU64::new(DEFAULT_MAX_PIXELS);

/// The pixel limit every decode and constructor checks before allocating.
#[must_use]
pub fn max_pixels() -> u64 {
    MAX_PIXELS.load(Ordering::Relaxed)
}

/// Sets the pixel limit for the rest of the program.
///
/// # Errors
/// `limit` is not positive.
pub fn set_max_pixels(limit: i64) -> Result<(), String> {
    let limit = u64::try_from(limit)
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("image::set_max_pixels: the limit must be positive, not {limit}"))?;
    MAX_PIXELS.store(limit, Ordering::Relaxed);
    Ok(())
}

/// Checks that a `width` by `height` image is non-empty and within the pixel
/// limit, naming `context` in the error, and answers its byte length.
///
/// # Errors
/// A side is not positive, or the image holds more pixels than the limit.
pub fn checked_len(context: &str, width: i64, height: i64) -> Result<usize, String> {
    if width <= 0 || height <= 0 {
        return Err(format!(
            "{context}: an image needs a positive width and height, not {width}x{height}"
        ));
    }
    let pixels = u64::try_from(width)
        .ok()
        .zip(u64::try_from(height).ok())
        .and_then(|(w, h)| w.checked_mul(h));
    let limit = max_pixels();
    match pixels {
        Some(pixels) if pixels <= limit => usize::try_from(pixels)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| format!("{context}: {width}x{height} does not fit in memory")),
        _ => Err(format!(
            "{context}: {width}x{height} is past the limit of {limit} pixels \
             (image::set_max_pixels raises it)"
        )),
    }
}

/// The storage an operation writes its answer into, allocated once at the
/// answer's size: a `Vec<u8>` for the bytecode VM, the packed vector compiled
/// code holds for native code. Encoders write through [`Write`], which grows
/// it.
pub trait ByteStore: Write + Sized {
    /// `len` zero bytes.
    fn zeroed(len: usize) -> Self;
    /// The bytes it holds.
    fn bytes_mut(&mut self) -> &mut [u8];
}

impl ByteStore for Vec<u8> {
    fn zeroed(len: usize) -> Self {
        vec![0; len]
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }
}

/// An image read from encoded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded<B> {
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
    /// `width * height` RGBA8 pixels, row by row.
    pub rgba: B,
}

/// Decodes PNG, JPEG, or GIF `bytes` (a GIF's first frame on its logical
/// screen), refusing an image past the pixel limit before its pixels are
/// allocated.
///
/// # Errors
/// The bytes are none of the three formats, the codec refused them, or the
/// image is past the pixel limit.
pub fn decode<B: ByteStore>(bytes: &[u8]) -> Result<Decoded<B>, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes).map_err(|e| format!("image::decode: PNG: {e}"))
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(bytes).map_err(|e| format!("image::decode: JPEG: {e}"))
    } else if bytes.starts_with(b"GIF8") {
        decode_gif(bytes).map_err(|e| format!("image::decode: GIF: {e}"))
    } else {
        Err("image::decode: the bytes are not a PNG, JPEG, or GIF image".to_string())
    }
}

/// The byte budget a codec may allocate for an image at the limit: eight
/// bytes a pixel covers 16-bit RGBA before it is narrowed.
fn codec_budget() -> u64 {
    max_pixels().saturating_mul(8)
}

fn sized(width: u32, height: u32) -> Result<usize, String> {
    checked_len("image", i64::from(width), i64::from(height))
        .map_err(|e| e.trim_start_matches("image: ").to_string())
}

fn decode_png<B: ByteStore>(bytes: &[u8]) -> Result<Decoded<B>, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(codec_budget()).unwrap_or(usize::MAX),
    });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (width, height) = (reader.info().width, reader.info().height);
    let len = sized(width, height)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "the image does not fit in memory".to_string())?;
    if size > len {
        return Err("the decoded frame is larger than its image".to_string());
    }
    // The frame decodes into the front of the answer, which is then widened
    // to RGBA in place from the back, so no second buffer is needed.
    let mut store = B::zeroed(len);
    let out = store.bytes_mut();
    let info = reader
        .next_frame(&mut out[..size])
        .map_err(|e| e.to_string())?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return Err("an indexed image stayed unexpanded".to_string()),
    };
    widen_to_rgba(out, len / 4, channels);
    Ok(Decoded {
        width: info.width,
        height: info.height,
        rgba: store,
    })
}

/// Widens `pixels` pixels of `channels` bytes each, packed at the front of
/// `out`, to RGBA8 in place. Each pixel's widened bytes start at or after
/// its source bytes, so walking from the last pixel never overwrites one not
/// yet read.
fn widen_to_rgba(out: &mut [u8], pixels: usize, channels: usize) {
    if channels == 4 {
        return;
    }
    for i in (0..pixels).rev() {
        let at = i * channels;
        let rgba = match channels {
            3 => [out[at], out[at + 1], out[at + 2], 255],
            2 => [out[at], out[at], out[at], out[at + 1]],
            _ => [out[at], out[at], out[at], 255],
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&rgba);
    }
}

fn decode_jpeg<B: ByteStore>(bytes: &[u8]) -> Result<Decoded<B>, String> {
    let limit = usize::try_from(max_pixels()).unwrap_or(usize::MAX);
    let options = zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA)
        .set_max_width(limit)
        .set_max_height(limit);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder
        .info()
        .ok_or_else(|| "the header holds no frame".to_string())?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let len = sized(width, height)?;
    if decoder.output_buffer_size() != Some(len) {
        return Err("the decoder answers a frame of another size".to_string());
    }
    let mut store = B::zeroed(len);
    decoder
        .decode_into(store.bytes_mut())
        .map_err(|e| e.to_string())?;
    Ok(Decoded {
        width,
        height,
        rgba: store,
    })
}

/// The first frame of the GIF `bytes`, drawn on the image's logical screen
/// (transparent where the frame does not cover it).
fn decode_gif<B: ByteStore>(bytes: &[u8]) -> Result<Decoded<B>, String> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    if let Some(budget) = NonZeroU64::new(codec_budget()) {
        options.set_memory_limit(gif::MemoryLimit::Bytes(budget));
    }
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|e| e.to_string())?;
    let (width, height) = (decoder.width(), decoder.height());
    let len = sized(u32::from(width), u32::from(height))?;
    let width = usize::from(width);
    let frame = decoder
        .read_next_frame()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the image holds no frame".to_string())?;
    let mut store = B::zeroed(len);
    let canvas = store.bytes_mut();
    let (left, top) = (usize::from(frame.left), usize::from(frame.top));
    let frame_width = usize::from(frame.width);
    for (row, line) in frame.buffer.chunks_exact(frame_width * 4).enumerate() {
        let y = top + row;
        if y * width * 4 >= len {
            break;
        }
        for (column, pixel) in line.chunks_exact(4).enumerate() {
            let x = left + column;
            if x >= width {
                break;
            }
            let at = (y * width + x) * 4;
            canvas[at..at + 4].copy_from_slice(pixel);
        }
    }
    Ok(Decoded {
        width: u32::from(decoder.width()),
        height: u32::from(decoder.height()),
        rgba: store,
    })
}

/// `rgba` (`width` by `height`, already checked) as lossless PNG bytes.
///
/// # Errors
/// The encoder refused the image.
pub fn encode_png<B: ByteStore>(rgba: &[u8], width: u32, height: u32) -> Result<B, String> {
    let mut out = B::zeroed(0);
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("Image::encode_png: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("Image::encode_png: {e}"))?;
    drop(writer);
    Ok(out)
}

/// `rgba` (`width` by `height`, already checked) as JPEG bytes at `quality`
/// 1 through 100. JPEG has no alpha, so each pixel is drawn over black.
///
/// # Errors
/// A side is past JPEG's 65 535, or the encoder refused the image.
pub fn encode_jpeg<B: ByteStore>(
    rgba: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<B, String> {
    let (Ok(w), Ok(h)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err(format!(
            "Image::encode_jpeg: JPEG holds at most 65535x65535, not {width}x{height}"
        ));
    };
    let mut out = B::zeroed(0);
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode_image(OverBlack { rgba, w, h })
        .map_err(|e| format!("Image::encode_jpeg: {e}"))?;
    Ok(out)
}

/// RGBA8 pixels drawn over black, handed to the JPEG encoder a row at a time
/// so no RGB copy of the whole image is made.
struct OverBlack<'a> {
    rgba: &'a [u8],
    w: u16,
    h: u16,
}

impl jpeg_encoder::ImageBuffer for OverBlack<'_> {
    fn get_jpeg_color_type(&self) -> jpeg_encoder::JpegColorType {
        jpeg_encoder::JpegColorType::Ycbcr
    }
    fn width(&self) -> u16 {
        self.w
    }
    fn height(&self) -> u16 {
        self.h
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a channel times an alpha, both at most 255, divided by 255 is at most 255"
    )]
    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let row = usize::from(self.w) * 4;
        let start = usize::from(y) * row;
        for p in self.rgba[start..start + row].chunks_exact(4) {
            let alpha = u16::from(p[3]);
            let over = |c: u8| ((u16::from(c) * alpha + 127) / 255) as u8;
            let (luma, cb, cr) = jpeg_encoder::rgb_to_ycbcr(over(p[0]), over(p[1]), over(p[2]));
            buffers[0].push(luma);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    }
}

/// `len` bytes (a whole number of pixels) of the one pixel `rgba` repeated.
#[must_use]
pub fn fill<B: ByteStore>(len: usize, rgba: [u8; 4]) -> B {
    let mut store = B::zeroed(len);
    for pixel in store.bytes_mut().chunks_exact_mut(4) {
        pixel.copy_from_slice(&rgba);
    }
    store
}

/// `rgba` (`width` by `height`) resampled to `new_width` by `new_height` with a
/// Lanczos-3 filter, widened by the scale factor when shrinking so every
/// source pixel contributes. Colour is filtered premultiplied by alpha, so a
/// transparent pixel's colour never bleeds into its neighbours.
///
/// # Panics
/// `rgba` holds fewer than `width * height` pixels.
#[must_use]
pub fn resize_rgba<B: ByteStore>(
    rgba: &[u8],
    width: usize,
    height: usize,
    new_width: usize,
    new_height: usize,
) -> B {
    let mut store = B::zeroed(new_width * new_height * 4);
    if width == 0 || height == 0 || new_width == 0 || new_height == 0 {
        return store;
    }
    let columns = filter_taps(width, new_width);
    let mut across = vec![[0f32; 4]; new_width * height];
    // One source row is premultiplied at a time, so the filter's float copy
    // of the image is a row, not the whole image.
    let mut row = vec![[0f32; 4]; width];
    for y in 0..height {
        for (slot, p) in row
            .iter_mut()
            .zip(rgba[y * width * 4..(y + 1) * width * 4].chunks_exact(4))
        {
            let alpha = f32::from(p[3]) / 255.0;
            *slot = [
                f32::from(p[0]) * alpha,
                f32::from(p[1]) * alpha,
                f32::from(p[2]) * alpha,
                f32::from(p[3]),
            ];
        }
        for (x, (first, weights)) in columns.iter().enumerate() {
            across[y * new_width + x] = weigh(
                weights
                    .iter()
                    .enumerate()
                    .map(|(i, w)| (&row[first + i], *w)),
            );
        }
    }
    let rows = filter_taps(height, new_height);
    let mut out = store.bytes_mut().chunks_exact_mut(4);
    for (first, weights) in &rows {
        for x in 0..new_width {
            let [r, g, b, a] = weigh(
                weights
                    .iter()
                    .enumerate()
                    .map(|(i, w)| (&across[(first + i) * new_width + x], *w)),
            );
            let alpha = a.clamp(0.0, 255.0);
            let unpremultiply = |c: f32| {
                if alpha <= 0.0 {
                    0
                } else {
                    channel(c * 255.0 / alpha)
                }
            };
            if let Some(pixel) = out.next() {
                pixel.copy_from_slice(&[
                    unpremultiply(r),
                    unpremultiply(g),
                    unpremultiply(b),
                    channel(alpha),
                ]);
            }
        }
    }
    store
}

fn weigh<'a>(taps: impl Iterator<Item = (&'a [f32; 4], f32)>) -> [f32; 4] {
    let mut sum = [0f32; 4];
    for (pixel, weight) in taps {
        for (total, value) in sum.iter_mut().zip(pixel) {
            *total += value * weight;
        }
    }
    sum
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to 0..=255 and rounded first"
)]
fn channel(value: f32) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

/// For each destination index, the first source index the filter reads and
/// the normalized weights from there on.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "image sizes stay far below 2^24, where f32 holds them exactly; floor and ceil \
              results are clamped into the source range before use"
)]
fn filter_taps(source: usize, destination: usize) -> Vec<(usize, Vec<f32>)> {
    const RADIUS: f32 = 3.0;
    let scale = source as f32 / destination as f32;
    let widen = scale.max(1.0);
    let support = RADIUS * widen;
    (0..destination)
        .map(|i| {
            let center = (i as f32 + 0.5) * scale;
            let first = (center - support).floor().max(0.0) as usize;
            let last = ((center + support).ceil() as usize).min(source);
            let mut weights: Vec<f32> = (first..last)
                .map(|s| lanczos3((s as f32 + 0.5 - center) / widen))
                .collect();
            let total: f32 = weights.iter().sum();
            if total != 0.0 {
                for w in &mut weights {
                    *w /= total;
                }
            }
            (first, weights)
        })
        .collect()
}

fn lanczos3(x: f32) -> f32 {
    fn sinc(x: f32) -> f32 {
        if x == 0.0 {
            1.0
        } else {
            let px = std::f32::consts::PI * x;
            px.sin() / px
        }
    }
    if x.abs() >= 3.0 {
        0.0
    } else {
        sinc(x) * sinc(x / 3.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3 by 2 GIF whose 2 by 1 frame sits at (1, 1): a red pixel and a
    /// transparent one.
    fn offset_frame_gif() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut out, 3, 2, &[255, 0, 0, 0, 0, 0]).unwrap();
            let frame = gif::Frame {
                left: 1,
                top: 1,
                width: 2,
                height: 1,
                transparent: Some(1),
                buffer: std::borrow::Cow::Owned(vec![0, 1]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
        out
    }

    #[test]
    fn a_gif_decodes_its_first_frame_with_transparency() {
        let Decoded {
            width,
            height,
            rgba,
        } = decode::<Vec<u8>>(&offset_frame_gif()).unwrap();
        assert_eq!((width, height), (3, 2));
        assert_eq!(&rgba[16..20], &[255, 0, 0, 255]);
        assert_eq!(rgba[23], 0);
        assert!(rgba[..12].iter().all(|&b| b == 0));
        assert!(decode::<Vec<u8>>(b"GIF89a").is_err());
    }

    #[test]
    fn a_tiny_gif_claiming_a_huge_screen_is_refused_before_its_pixels_exist() {
        let mut bomb = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut bomb, 65535, 65535, &[0, 0, 0, 255, 255, 255]).unwrap();
            let frame = gif::Frame {
                width: 1,
                height: 1,
                buffer: std::borrow::Cow::Owned(vec![1]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
        assert!(bomb.len() < 64, "{}", bomb.len());
        let error = decode::<Vec<u8>>(&bomb).unwrap_err();
        assert!(error.contains("65535x65535 is past the limit"), "{error}");
    }

    #[test]
    fn sizes_must_be_positive_and_within_the_limit() {
        assert_eq!(checked_len("t", 2, 3), Ok(24));
        assert!(checked_len("t", 0, 3).unwrap_err().contains("positive"));
        assert!(checked_len("t", -1, 3).is_err());
        assert!(
            checked_len("t", 20_000, 20_000)
                .unwrap_err()
                .contains("past the limit")
        );
        assert!(checked_len("t", i64::MAX, i64::MAX).is_err());
    }

    #[test]
    fn a_flat_colour_resizes_to_the_same_colour_at_any_scale() {
        let rgba: Vec<u8> = std::iter::repeat_n([10u8, 200, 30, 255], 64)
            .flatten()
            .collect();
        for (w, h) in [(3, 5), (16, 16), (1, 1)] {
            let out: Vec<u8> = resize_rgba(&rgba, 8, 8, w, h);
            assert_eq!(out.len(), w * h * 4);
            assert!(out.chunks_exact(4).all(|p| p == [10, 200, 30, 255]));
        }
    }

    #[test]
    fn a_transparent_neighbour_lends_no_colour() {
        let rgba = [255, 0, 0, 255, 0, 255, 0, 0];
        let out: Vec<u8> = resize_rgba(&rgba, 2, 1, 1, 1);
        assert_eq!(&out[..3], &[255, 0, 0]);
        assert!(out[3] > 0 && out[3] < 255);
    }

    /// A 4 by 2 image with a different colour in each pixel, one of them
    /// half transparent.
    fn sample_rgba() -> Vec<u8> {
        (0u8..8)
            .flat_map(|i| [i * 30, 255 - i * 20, i * 11, if i == 5 { 128 } else { 255 }])
            .collect()
    }

    #[test]
    fn a_png_round_trips_every_colour_type_to_rgba() {
        let rgba = sample_rgba();
        let png: Vec<u8> = encode_png(&rgba, 4, 2).unwrap();
        let back = decode::<Vec<u8>>(&png).unwrap();
        assert_eq!((back.width, back.height), (4, 2));
        assert_eq!(back.rgba, rgba);
        for (color, channels) in [(png::ColorType::Rgb, 3), (png::ColorType::Grayscale, 1)] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
                encoder.set_color(color);
                encoder.set_depth(png::BitDepth::Eight);
                let data: Vec<u8> = (0..2 * channels).map(|i| (i * 40) as u8).collect();
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&data)
                    .unwrap();
            }
            let back = decode::<Vec<u8>>(&bytes).unwrap();
            let expected: Vec<u8> = if channels == 3 {
                vec![0, 40, 80, 255, 120, 160, 200, 255]
            } else {
                vec![0, 0, 0, 255, 40, 40, 40, 255]
            };
            assert_eq!(back.rgba, expected);
        }
    }

    #[test]
    fn jpeg_rows_drawn_over_black_encode_as_the_rgb_image_would() {
        let rgba = sample_rgba();
        let ours: Vec<u8> = encode_jpeg(&rgba, 4, 2, 90).unwrap();
        let rgb: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|p| {
                let a = u16::from(p[3]);
                [0, 1, 2].map(|c| ((u16::from(p[c]) * a + 127) / 255) as u8)
            })
            .collect();
        let mut theirs = Vec::new();
        jpeg_encoder::Encoder::new(&mut theirs, 90)
            .encode(&rgb, 4, 2, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn a_jpeg_decodes_to_rgba_with_opaque_alpha() {
        let flat: Vec<u8> = std::iter::repeat_n([200u8, 40, 90, 255], 64)
            .flatten()
            .collect();
        let jpeg: Vec<u8> = encode_jpeg(&flat, 8, 8, 95).unwrap();
        let back = decode::<Vec<u8>>(&jpeg).unwrap();
        assert_eq!((back.width, back.height), (8, 8));
        for p in back.rgba.chunks_exact(4) {
            assert_eq!(p[3], 255);
            for (got, want) in p[..3].iter().zip([200u8, 40, 90]) {
                assert!(got.abs_diff(want) <= 3, "{p:?}");
            }
        }
        let mut cmyk = Vec::new();
        jpeg_encoder::Encoder::new(&mut cmyk, 95)
            .encode(
                &[0, 255, 255, 0].repeat(64),
                8,
                8,
                jpeg_encoder::ColorType::Cmyk,
            )
            .unwrap();
        let red = decode::<Vec<u8>>(&cmyk).unwrap();
        let p = &red.rgba[..4];
        assert!(p[0] > 200 && p[1] < 60 && p[2] < 60 && p[3] == 255, "{p:?}");
    }
}
