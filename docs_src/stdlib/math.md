# `std::math`

Status: experimental

Mathematical constants and f64 functions (Go's math package shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `PI` | `const PI` | Archimedes' constant π. |
| `E` | `const E` | Euler's number e. |
| `SQRT_2` | `const SQRT_2` | √2. |
| `LN_2` | `const LN_2` | Natural log of 2. |
| `LN_10` | `const LN_10` | Natural log of 10. |
| `PHI` | `const PHI` | Golden ratio φ. |
| `INF` | `const INF` | Positive infinity. |
| `NAN` | `const NAN` | Not-a-number value. |
| `abs` | `fn abs(x: f64) -> f64` | Absolute value of x. |
| `sqrt` | `fn sqrt(x: f64) -> f64` | Square root. |
| `cbrt` | `fn cbrt(x: f64) -> f64` | Cube root. |
| `floor` | `fn floor(x: f64) -> f64` | Largest integer ≤ x. |
| `ceil` | `fn ceil(x: f64) -> f64` | Smallest integer ≥ x. |
| `round` | `fn round(x: f64) -> f64` | Nearest integer, half away from zero. |
| `trunc` | `fn trunc(x: f64) -> f64` | Integer part of x. |
| `sin` | `fn sin(x: f64) -> f64` | Sine (radians). |
| `cos` | `fn cos(x: f64) -> f64` | Cosine (radians). |
| `tan` | `fn tan(x: f64) -> f64` | Tangent (radians). |
| `asin` | `fn asin(x: f64) -> f64` | Arcsine (radians). |
| `acos` | `fn acos(x: f64) -> f64` | Arccosine (radians). |
| `atan` | `fn atan(x: f64) -> f64` | Arctangent (radians). |
| `atan2` | `fn atan2(y: f64, x: f64) -> f64` | Four-quadrant arctangent of y/x. |
| `exp` | `fn exp(x: f64) -> f64` | e^x. |
| `exp2` | `fn exp2(x: f64) -> f64` | 2^x. |
| `ln` | `fn ln(x: f64) -> f64` | Natural logarithm. |
| `log2` | `fn log2(x: f64) -> f64` | Base-2 logarithm. |
| `log10` | `fn log10(x: f64) -> f64` | Base-10 logarithm. |
| `log` | `fn log(x: f64, y: f64) -> f64` | Logarithm with given base. |
| `pow` | `fn pow(x: f64, y: f64) -> f64` | x raised to the power y. |
| `hypot` | `fn hypot(x: f64, y: f64) -> f64` | Euclidean distance √(x²+y²). |
| `rem` | `fn rem(x: f64, y: f64) -> f64` | Floating-point remainder x%y. |
| `is_nan` | `fn is_nan(x: f64) -> bool` | Reports whether x is NaN. |
| `is_inf` | `fn is_inf(x: f64, sign: i64) -> bool` | Reports whether x is infinite. |
| `copysign` | `fn copysign(x: f64, y: f64) -> f64` | Magnitude of x with sign of y. |
| `positive_diff` | `fn positive_diff(x: f64, y: f64) -> f64` | max(x-y, 0). |
| `sinh` | `fn sinh(x: f64) -> f64` | Hyperbolic sine. |
| `cosh` | `fn cosh(x: f64) -> f64` | Hyperbolic cosine. |
| `tanh` | `fn tanh(x: f64) -> f64` | Hyperbolic tangent. |
| `min` | `fn min(x: f64, y: f64) -> f64` | Lesser of two values. |
| `max` | `fn max(x: f64, y: f64) -> f64` | Greater of two values. |
| `clamp` | `fn clamp(x: f64, min: f64, max: f64) -> f64` | Constrain x to the inclusive range [lo, hi]. |
| `LOG2_E` | `const LOG2_E` | Base-2 logarithm of e. |
| `LOG10_E` | `const LOG10_E` | Base-10 logarithm of e. |
| `MAX_F64` | `const MAX_F64` | Largest finite f64 value. |
| `MIN_POSITIVE_F64` | `const MIN_POSITIVE_F64` | Smallest positive normal f64 value. |
| `NEG_INF` | `const NEG_INF` | Negative infinity. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
