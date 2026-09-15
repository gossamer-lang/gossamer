# `lang::simd`

Fixed-width lane vectors: `Simd<T, N>` over `f32`, `f64`, `i32`, `i64`, `u8`, or `u32` and its `bool` form `Mask<N>`. Lane-wise arithmetic and bitwise operators, lane comparisons, `select`, reductions, and `Simd::load` / `store` over a window checked once, with the same bits on every tier (GT0089).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Building a vector

`Simd<T, N>` holds `N` lanes of one scalar type. The lane type is `f32`, `f64`,
`i32`, `i64`, `u8`, or `u32`, and `N` is 2, 4, or 8, or also 16 for `u8`,
`i32`, and `u32`. `Mask<N>` is the `bool` form, and it also takes 16 lanes. Any
other lane type or count reports `GT0089`.

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
| `lanes_eq(other)`, `lanes_lt(other)`, `lanes_le(other)` | a `Mask<N>`, true where the comparison holds |
| `reduce_sum()`, `reduce_min()`, `reduce_max()` | one lane value |
| `reduce_and()`, `reduce_or()` | the bitwise fold of integer lanes, or `bool` on a `Mask` |
| `mask.select(if_true, if_false)` | each lane from `if_true` where the mask is true, else from `if_false` |
| `v.store(&mut xs, offset)` | writes the lanes into `xs` from `offset` |

`reduce_sum` folds its lanes in one fixed pairing order, so a floating-point
sum answers the same bits on the bytecode VM, the JIT, and a native build.

## Windows over a sequence

`Simd::load(xs, offset)` reads `N` lanes of a `Vec`, a slice, or a fixed array,
and `v.store(&mut xs, offset)` writes them back through `&mut`. The window is
checked once: an `offset` whose window reaches past either end panics with the
same message on every tier, before any lane is read or written.

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
