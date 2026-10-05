# `lang::simd`

Fixed-width lane vectors: `Simd<T, N>` over `i8` through `u64`, `f32`, or `f64` and its `bool` form `Mask<N>`. Lane-wise arithmetic and bitwise operators, saturating and fused operations, lane comparisons, bitmasks, conversions, shuffles, `select`, reductions, gathers, and windows over a sequence, with the same bits on every tier (GT0089).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Building a vector

`Simd<T, N>` holds `N` lanes of one scalar type. The lane type is `i8`, `u8`,
`i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `f32`, or `f64`, and `N` is 2, 4, or 8,
or also 16 for lanes of 32 bits or fewer. `Mask<N>` is the `bool` form, and it
also takes 16 lanes. Any other lane type or count reports `GT0089`.

```gossamer
fn main() {
    let a = Simd::from_array([1.0, 2.0, 3.0, 4.0])
    let b: Simd<f64, 4> = Simd::splat(0.5)
    let xs = #[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]
    let window: Simd<f64, 4> = Simd::load(xs, 4)
    println("{:?} {:?}", (a + b).to_array(), window.to_array())  // [1.5, 2.5, 3.5, 4.5] [5.0, 6.0, 7.0, 8.0]
}
```

`Simd::from_array` takes the lane count from the array's length.
`Simd::splat` and `Simd::load` take it from the type the context expects - an
annotated `let`, a parameter, or a return type.

## Lane-wise operators

| Operator | Lane types |
|---|---|
| `+`, `-`, `*` | float and integer |
| `/` | float |
| `+%`, `-%`, `*%`, `<<`, `>>` | integer |
| `&`, `\|`, `^` | integer, and `Mask` |

Both operands share one vector type, and the result has it too. An operator
the lane type does not define reports `GT0089`. The integer lanes wrap under
`+%`, `-%`, and `*%` exactly as a scalar of the lane type does.

## Methods

| Method | Answers |
|---|---|
| `to_array()` | `[T; N]`, the lanes in order |
| `min(other)`, `max(other)` | the smaller or larger of each pair of lanes |
| `abs()` | each lane's absolute value |
| `sqrt()` | each lane's square root; float lanes |
| `lanes_eq`, `lanes_ne`, `lanes_lt`, `lanes_le`, `lanes_gt`, `lanes_ge` | a `Mask<N>`, true where the comparison holds |
| `reduce_sum()`, `reduce_min()`, `reduce_max()` | one lane value |
| `reduce_and()`, `reduce_or()` | the bitwise fold of integer lanes, or `bool` on a `Mask` |
| `saturating_add(other)`, `saturating_sub(other)` | integer lanes clamped to the lane type's range |
| `abs_diff(other)` | each pair's distance, in the unsigned lane type of the same width |
| `mul_add(a, b)` | `v * a + b` per lane with one rounding; float lanes |
| `cast::<U>()` | each lane converted as `as` converts it |
| `to_bits()`, `Simd::from_bits(v)` | the lanes' bits as unsigned integers, and back |
| `widen_low()`, `widen_high()` | half the lanes in the type twice as wide |
| `a.narrow(b)` | the lanes of `a` then `b` in the type half as wide, integers clamped |
| `swizzle([i, ..])`, `a.concat_swizzle(b, [i, ..])` | the lanes the literal indices pick, from `v` or from `a` then `b` |
| `a.interleave(b)` | the lanes of `a` and `b` alternating, as two vectors |
| `swizzle_dyn(idx)` | `u8` lanes: lane `idx[i]` of `v`, or zero past the end (a table lookup) |
| `v.store(&mut xs, offset)` | writes the lanes into `xs` from `offset` |
| `v.store_prefix(&mut xs, offset, n)` | writes the first `n` lanes |

A `Mask<N>` answers `select(if_true, if_false)` (each lane from `if_true` where
the mask is true, else from `if_false`), `any()`, `all()`, `to_bitmask()` (a
`u64` with lane `i` at bit `i`), and `first_set()` (the lowest true lane, as an
`Option<i64>`); `Mask::from_bitmask(bits)` builds one.

`reduce_sum` folds its lanes in one fixed pairing order, so a floating-point
sum answers the same bits on the bytecode VM, the JIT, and a native build.

## Windows over a sequence

`Simd::load(xs, offset)` reads `N` lanes of a `Vec`, a slice, or a fixed array,
and `v.store(&mut xs, offset)` writes them back through `&mut`. The window is
checked once: an `offset` whose window reaches past either end panics with the
same message on every tier, before any lane is read or written.
`Simd::load_or(xs, offset, fill)` never panics: lanes outside `xs` read `fill`,
so the tail of a sequence takes the same path as its body, and
`Simd::gather(xs, indices)` reads the lanes the index vector names.

```gossamer
fn halve_in_windows(data: Vec<f64>) -> Vec<f64> {
    let mut out = data
    let halves: Simd<f64, 4> = Simd::splat(0.5)
    let mut offset = 0
    while offset + 4 <= out.len() {
        let window: Simd<f64, 4> = Simd::load(out, offset)
        (window * halves).store(&mut out, offset)
        offset += 4
    }
    out
}

fn main() {
    println("{:?}", halve_in_windows(#[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]))
}
```

## Scanning bytes

A lane comparison read through a mask terminal compiles to vector code in a
native build: the scan below tests sixteen bytes per step.

```gossamer
fn find_byte(hay: [u8], needle: u8) -> Option<i64> {
    let target: Simd<u8, 16> = Simd::splat(needle)
    let mut i = 0
    while i < hay.len() {
        let block: Simd<u8, 16> = Simd::load_or(hay, i, needle +% 1)
        if let Some(lane) = block.lanes_eq(target).first_set() {
            return Some(i + lane)
        }
        i += 16
    }
    None
}

fn main() {
    let text = "find the comma, past sixteen bytes"
    println(find_byte(text.bytes(), 44))  // Some(14)
}
```

## Generic over the lane count

A function may take and answer vectors over a const generic lane count, and each
count it is called with is its own specialisation:

```gossamer
fn dot<const N: usize>(a: Simd<f64, N>, b: Simd<f64, N>) -> f64 {
    (a * b).reduce_sum()
}

fn main() {
    let small = Simd::from_array([1.0, 2.0])
    let wide = Simd::from_array([1.0, 2.0, 3.0, 4.0])
    println("{} {}", dot(small, small), dot(wide, wide))  // 5 30
}
```

In the REPL, `%info Simd` and `%info Mask` list every method with its signature,
and `%explain` on a binding shows the methods with its own lane type.
See `examples/simd_lanes.gos` for a complete program.
