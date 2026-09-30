# `std::math::bits`

Status: experimental

Integer bit-manipulation operations (Go's math/bits shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `count_ones` | `fn count_ones(x: u64) -> i64` | Number of set bits (popcount). |
| `count_zeros` | `fn count_zeros(x: u64) -> i64` | Number of clear bits. |
| `leading_zeros` | `fn leading_zeros(x: u64) -> i64` | Leading zero bit count. |
| `trailing_zeros` | `fn trailing_zeros(x: u64) -> i64` | Trailing zero bit count. |
| `rotate_left` | `fn rotate_left(x: u64, n: i64) -> u64` | Rotates x left by n bits. |
| `rotate_right` | `fn rotate_right(x: u64, n: i64) -> u64` | Rotates x right by n bits. |
| `reverse_bits` | `fn reverse_bits(x: u64) -> i64` | Reverses bit order of x. |
| `reverse_bytes` | `fn reverse_bytes(x: u64) -> i64` | Reverses byte order of x. |
| `len` | `fn len(x: u64) -> i64` | Minimum bits required to represent x. |
| `add` | `fn add(x: u64, y: u64, carry: u64) -> (u64, u64)` | x + y + carry; returns (sum, carry_out). |
| `sub` | `fn sub(x: u64, y: u64, borrow: u64) -> (u64, u64)` | x - y - borrow; returns (diff, borrow_out). |
| `mul` | `fn mul(x: u64, y: u64) -> (u64, u64)` | Full 128-bit product; returns (hi, lo). |
| `div` | `fn div(hi: u64, lo: u64, y: u64) -> (u64, u64)` | 128-bit dividend / 64-bit divisor; returns (quotient, remainder). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
