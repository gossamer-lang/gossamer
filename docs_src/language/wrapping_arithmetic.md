# `lang::wrapping_arithmetic`

Wrapping arithmetic operators `+%`, `-%`, `*%` and their compound forms `+%=`, `-%=`, `*%=`: two's-complement wrapping at the operands' declared integer width, on every tier and in every build profile. Plain `+`, `-`, and `*` keep their overflow check, and `wrapping_add` / `wrapping_mul` are not methods (GT0087).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## The operators

`a +% b`, `a -% b`, and `a *% b` add, subtract, and multiply two values of one
integer type with two's-complement wrapping at that type's declared width. The
answer is the same on the bytecode VM, the JIT, and a native build, in a debug
and a release profile alike. `+%=`, `-%=`, and `*%=` are the compound forms.

```gossamer
fn djb2(text: String) -> u32 {
    let mut hash: u32 = 5381
    for b in text.bytes() {
        hash = (hash << 5) +% hash +% b as u32
    }
    hash
}

fn main() {
    let mut countdown: u8 = 2
    countdown -%= 5
    println("{} {}", djb2("hello"), countdown)  // 261238937 253
}
```

| Expression | Value |
|---|---|
| `250 as u8 +% 10` | `4` |
| `0 as u8 -% 1` | `255` |
| `200 as u8 *% 2` | `144` |
| `-128 as i8 -% 1` | `127` |

## Why a separate spelling

Plain `+`, `-`, and `*` keep their overflow check, so an arithmetic step that is
meant to wrap - a hash, a checksum, a pseudo-random step, a ring index - says so
where it is written, and means the same thing in every build profile.

## Operand types and precedence

Both operands share one integer type: `i8`-`i64`, `u8`-`u64`, `isize`, or
`usize`. A byte literal joins an integer operand as it does for `+`. A float or
`String` operand reports `GT0003`. The operators bind like the ones they wrap:
`*%` with `*` (level 5), and `+%` and `-%` with `+` and `-` (level 6). On the
integer lanes of a [lane vector](simd.md) they wrap lane by lane.

## No wrapping methods

`x.wrapping_add(y)` and `x.wrapping_mul(y)` are not integer methods. A call
reports `GT0087`, and `gos check --fix` rewrites it to `x +% y` or `x *% y`,
including a chain of calls.

In the REPL, `%info +%` (and `-%`, `*%`, `+%=`, `-%=`, `*%=`) describes each
operator with an example. See `examples/wrapping_hash.gos` for a complete
program.
