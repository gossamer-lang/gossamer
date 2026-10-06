# `lang::closure`

Lambda expression `|args| body`.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A closure is a value of type `Fn(args) -> ret`. It may be stored in a
binding, a struct field, or a collection, passed to a function, and
answered from one:

```gossamer
fn adder(k: i64) -> Fn(i64) -> i64 {
    |x| x + k
}

fn main() {
    let add2 = adder(2)
    println("{}", add2(5))
}
```

## What a closure captures

A closure names the bindings around it without any annotation; there is no
`move`. What it captures depends on the type:

- A `Vec`, `Map`, `Set`, deque, or heap is captured by managed reference.
  A write to it inside the closure is the enclosing binding's write:

  ```gossamer
  fn main() {
      let mut seen = #[]
      #[3, 1, 2].for_each(|x| seen.push(x * 10))
      println("{}", seen)
  }
  ```

- Every other value - a scalar, a `String`, a tuple, a fixed array, a
  struct or enum - is captured by copy. The closure reads the value the
  binding held when the closure was made, and a write to it would change
  only the closure's copy, which starts over on every call. Such a write is
  rejected with `GT0114`:

  <!-- compile_fail GT0114 -->
  ```gossamer
  fn main() {
      let mut count = 0
      #[1, 2, 3].for_each(|x| count += x)
  }
  ```

  Answer the new value instead (`let count = xs.fold(0, |acc, x| acc + x)`),
  or keep the state in a container the closure captures by reference.

A closure given to `spawn` captures the same way. A container it shares
with the code that spawned it is reached from two goroutines, so writes to
it are serialised with a `sync::Mutex`, or the goroutine answers its result
through `join()` or a channel.

## A closure on its own line

A line that starts with `|` and a parameter list begins a new statement,
so a function can end with a closure. A line that starts with `||`
continues the expression above it as a logical or; a no-argument closure on
a line of its own is written in parentheses:

```gossamer
fn counter_from(start: i64) -> Fn() -> i64 {
    let base = start * 10
    (|| base + 1)
}

fn main() {
    let next = counter_from(4)
    println("{}", next())
}
```
