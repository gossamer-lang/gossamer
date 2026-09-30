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
