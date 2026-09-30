# `lang::slicing`

A range in index position takes a subsequence: `xs[1..3]`, `xs[..k]`, `xs[k..]`, `xs[..]`, `xs[a..=b]`, over fixed arrays, slices, `Vec`, and `String`. Bounds clamp rather than panic, matching `substring`; a `String` slice takes byte offsets and snaps to codepoint boundaries.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

Indexing with a range yields the elements it covers:

```gossamer
let xs = #[1, 2, 3, 4, 5]

println("{:?}", xs[1..3])     // #[2, 3]
println("{:?}", xs[..2])      // #[1, 2]
println("{:?}", xs[3..])      // #[4, 5]
println("{:?}", xs[..])       // #[1, 2, 3, 4, 5]
println("{:?}", xs[1..=3])    // #[2, 3, 4]
```

Fixed arrays, slices, and `Vec` all accept it:

```gossamer
let a: [i64; 5] = [1, 2, 3, 4, 5]
println("{:?}", a[1..3])      // #[2, 3]
```

A range binds looser than arithmetic, so `xs[i * 2..n]` is `xs[(i * 2)..n]`
and needs no parentheses.

## Bounds clamp

An out-of-range range yields the part that exists:

```gossamer
let xs = #[1, 2, 3]
println("{:?}", xs[1..99])    // #[2, 3]
println("{:?}", xs[9..12])    // #[]
```

This differs from element indexing, which panics, and the difference is the
point. An out-of-range single index has no answer to give; an out-of-range
range has exactly one - the overlap. It is also the rule `substring` already
follows, so the two spellings of the same operation agree.

If you need out-of-range to be an error, check the bound yourself:

<!-- fragment -->
```gossamer
if end <= xs.len() {
    process(xs[start..end])
} else {
    return Err(errors::new("range past the end"))
}
```

## Slicing a `String`

A `String` has two index spaces (see [`String`](../syntax.md)): `s.len()` and
`s[i]` count Unicode scalars, while `s.byte_len()`, `s.byte_at(i)`,
`s.as_bytes()`, and `substring` count UTF-8 bytes.

**A `String` slice uses byte offsets**, the same space `substring` uses -
slicing a string *is* `substring`:

```gossamer
let s = "héllo"                // 5 characters, 6 bytes
println("{}", s.len())        // 5
println("{}", s.byte_len())   // 6

println("{}", s[1..3])        // é   - bytes 1..3, the two bytes of `é`
println("{}", s.substring(1, 3))  // é - the same operation
```

Offsets snap outward to codepoint boundaries, so a slice is always valid
text and never splits a character in half.

Do not mix the spaces on non-ASCII text: `s[i]` is a character, `s[a..b]` is
a byte range. For ASCII the two coincide, which is why the difference only
shows up once real text arrives.
