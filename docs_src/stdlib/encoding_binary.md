# `std::encoding::binary`

Status: experimental

Big/little-endian integer packing and varint codecs.

## Items

| Item | Signature | Description |
|---|---|---|
| `get_u8` | `fn get_u8(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a single byte. |
| `put_u8` | `fn put_u8(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a single byte. |
| `get_u16_be` | `fn get_u16_be(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a big-endian u16. |
| `put_u16_be` | `fn put_u16_be(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a big-endian u16. |
| `get_u16_le` | `fn get_u16_le(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a little-endian u16. |
| `put_u16_le` | `fn put_u16_le(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a little-endian u16. |
| `get_u32_be` | `fn get_u32_be(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a big-endian u32. |
| `put_u32_be` | `fn put_u32_be(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a big-endian u32. |
| `get_u32_le` | `fn get_u32_le(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a little-endian u32. |
| `put_u32_le` | `fn put_u32_le(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a little-endian u32. |
| `get_u64_be` | `fn get_u64_be(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a big-endian u64. |
| `put_u64_be` | `fn put_u64_be(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a big-endian u64. |
| `get_u64_le` | `fn get_u64_le(bytes: Vec<u8>) -> Result<i64, errors::Error>` | Reads a little-endian u64. |
| `put_u64_le` | `fn put_u64_le(buf: Vec<u8>, value: i64) -> Vec<u8>` | Writes a little-endian u64. |
| `get_u16_be_at` | `fn get_u16_be_at(bytes: Vec<u8>, offset: i64) -> Result<u16, errors::Error>` | Reads a big-endian u16 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u16_be_at` | `fn put_u16_be_at(buf: &mut Vec<u8>, offset: i64, value: u16) -> Result<(), errors::Error>` | Writes a big-endian u16 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `get_u16_le_at` | `fn get_u16_le_at(bytes: Vec<u8>, offset: i64) -> Result<u16, errors::Error>` | Reads a little-endian u16 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u16_le_at` | `fn put_u16_le_at(buf: &mut Vec<u8>, offset: i64, value: u16) -> Result<(), errors::Error>` | Writes a little-endian u16 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `get_u32_be_at` | `fn get_u32_be_at(bytes: Vec<u8>, offset: i64) -> Result<u32, errors::Error>` | Reads a big-endian u32 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u32_be_at` | `fn put_u32_be_at(buf: &mut Vec<u8>, offset: i64, value: u32) -> Result<(), errors::Error>` | Writes a big-endian u32 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `get_u32_le_at` | `fn get_u32_le_at(bytes: Vec<u8>, offset: i64) -> Result<u32, errors::Error>` | Reads a little-endian u32 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u32_le_at` | `fn put_u32_le_at(buf: &mut Vec<u8>, offset: i64, value: u32) -> Result<(), errors::Error>` | Writes a little-endian u32 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `get_u64_be_at` | `fn get_u64_be_at(bytes: Vec<u8>, offset: i64) -> Result<u64, errors::Error>` | Reads a big-endian u64 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u64_be_at` | `fn put_u64_be_at(buf: &mut Vec<u8>, offset: i64, value: u64) -> Result<(), errors::Error>` | Writes a big-endian u64 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `get_u64_le_at` | `fn get_u64_le_at(bytes: Vec<u8>, offset: i64) -> Result<u64, errors::Error>` | Reads a little-endian u64 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill. |
| `put_u64_le_at` | `fn put_u64_le_at(buf: &mut Vec<u8>, offset: i64, value: u64) -> Result<(), errors::Error>` | Writes a little-endian u64 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err. |
| `uvarint` | `fn uvarint(bytes: Vec<u8>) -> Result<(i64, i64), errors::Error>` | Decodes an unsigned varint. |
| `varint` | `fn varint(bytes: Vec<u8>) -> Result<(i64, i64), errors::Error>` | Decodes a signed varint (zigzag). |
| `put_uvarint` | `fn put_uvarint(buf: Vec<u8>, value: i64) -> Vec<u8>` | Encodes an unsigned varint. |
| `put_varint` | `fn put_varint(buf: Vec<u8>, value: i64) -> Vec<u8>` | Encodes a signed varint (zigzag). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
