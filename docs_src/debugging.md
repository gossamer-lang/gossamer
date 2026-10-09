# Debugging

`gos build -g` embeds DWARF debug information: line tables, one subprogram per
function, and a location for every parameter and named local. `gdb` and `lldb`
read it as they would a C program's.

```sh
gos build -g src/main.gos
gdb ./target/debug/main
```

```text
(gdb) break area
(gdb) run
Breakpoint 1, area (width=7, height=2.5, label=0x555555572d25) at main.gos:4
(gdb) next
(gdb) info locals
scaled = 17.5
p = {7, 3}
letter = U'z'
```

What a debugger shows for each value:

| Gossamer type | Shown as |
|---|---|
| `i8`..`i64`, `u8`..`u64`, `isize`, `usize` | the integer |
| `f32`, `f64` | the float |
| `bool` | `true` / `false` |
| `char` | the code point, as `char32_t` |
| a struct, tuple, or array held inline | its words, in declaration order |
| `String`, `Vec`, `Map`, an enum, a handle | the address of the runtime value |

A `--release -g` build keeps the same information, but optimization moves or
removes values, so a local may read `<optimized out>` and stepping may jump
between lines. A debug build (`gos build -g`, no `--release`) keeps every
local in its stack slot.

`gos run` executes on the bytecode VM and the JIT, which a native debugger
does not step through; build the program to debug it. A panic in any tier
prints the call stack with source lines, which is often enough without a
debugger.

`lldb` works the same way: `lldb ./target/debug/main`, then
`breakpoint set --name area`, `run`, and `frame variable`.

## Optimization remarks

Two variables make `gos build --release` explain what it did with a loop,
one line per site, with the file, line, and column it was written at.
Setting either makes the build skip its artifact cache, so the remarks always
describe the compile that just ran.

`GOS_BOUNDS_REMARKS=1` reports every index access `xs[i]` on a vector of
scalars: whether its bounds check was removed, versioned (an unchecked copy of
the loop runs behind one check at loop entry, with the checked loop as the
fallback), or kept. A check inside a `for i in 0..xs.len()` loop that stays
because something in the loop could change the vector's length names that
construct:

```text
[bounds] main.gos:12:19: index check removed in `kernel`
[bounds] main.gos:24:17: index check kept in `rebuild`: `grow` may resize the vector
```

A call inside such a loop keeps the proof when the called function cannot
change the length of the vector it is handed, however deep the calls go, and a
function whose every caller indexes it with a loop counter below the vector's
length gets the same proof for its own `xs[i]`.

`GOS_ARENA_TRACE=1` reports which loop and closure bodies were given an
automatic arena region, and why an allocating one was not; see
[Automatic arenas](language/arena.md#automatic-arenas-no-annotation-needed).
