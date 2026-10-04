# `std::image`

Status: experimental

Opaque RGBA8 image handles with PNG and JPEG codecs.

## Items

| Item | Signature | Description |
|---|---|---|
| `new` | `fn new(width: i64, height: i64) -> i64` | Allocates a transparent image handle. |
| `filled` | `fn filled(width: i64, height: i64, rgba: i64) -> i64` | Allocates an image handle filled with a packed 0xRRGGBBAA colour. |
| `decode_base64` | `fn decode_base64(encoded: String) -> i64` | Decodes a base64 PNG or JPEG; returns zero for malformed input. |
| `width` | `fn width(image: i64) -> i64` | Returns an image width in pixels. |
| `height` | `fn height(image: i64) -> i64` | Returns an image height in pixels. |
| `pixel` | `fn pixel(image: i64, x: i64, y: i64) -> i64` | Returns packed 0xRRGGBBAA, or -1 outside the image. |
| `set_pixel` | `fn set_pixel(image: i64, x: i64, y: i64, rgba: i64) -> bool` | Sets a packed 0xRRGGBBAA pixel and reports whether it was in bounds. |
| `from_rgba_bytes` | `fn from_rgba_bytes(width: i64, height: i64, bytes: [u8]) -> i64` | Builds an image from width * height RGBA8 pixels, row by row; returns zero when the byte count does not match. |
| `to_rgba_bytes` | `fn to_rgba_bytes(image: i64) -> Vec<u8>` | Returns the pixels as RGBA8 bytes, row by row; empty for an invalid handle. |
| `encode_png_base64` | `fn encode_png_base64(image: i64) -> String` | Encodes an image as lossless base64 PNG. |
| `encode_jpeg_base64` | `fn encode_jpeg_base64(image: i64, quality: i64) -> String` | Encodes an image as base64 JPEG at quality 1 through 100. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Handle API and codec behavior

`new`, `filled`, and `decode_base64` return opaque nonzero `i64` image
handles. Pixels are packed `0xRRGGBBAA`; `pixel` returns `-1` out of bounds,
and `set_pixel` returns `false` out of bounds. Invalid dimensions and malformed
or unsupported base64 input return handle `0` without panicking.

`encode_png_base64` is lossless for RGBA8. `encode_jpeg_base64` accepts quality
`1..=100`; JPEG composites alpha against black and returns opaque decoded
pixels. The API has VM, forced-Cranelift-JIT, and LLVM-native output parity,
with malformed-input coverage in `image_png_jpeg_round_trip_and_invalid_input_match_native`.

See `examples/image_png_jpeg.gos` and `benchmarks/perf/image_png_jpeg.gos` for
consumer and benchmark workloads.
