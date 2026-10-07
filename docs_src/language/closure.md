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
`move`. It reads each capture's current value when it runs, and a write
inside it reaches the binding, whatever the binding's type:

```gossamer
fn main() {
    let mut count = 0
    let mut seen = #[]
    #[3, 1, 2].for_each(|x| {
        count += x
        seen.push(x * 10)
    })
    println("{} {}", count, seen)
}
```

A closure that outlives the function that made it keeps its captures alive,
so a counter is a closure over a local:

```gossamer
fn counter() -> Fn() -> i64 {
    let mut n = 0
    || {
        n += 1
        n
    }
}

fn main() {
    let next = counter()
    next()
    println("{}", next())
}
```

A closure given to `spawn` is the exception: it runs on another goroutine,
so it takes a snapshot of each binding it captures at the spawn. A write to
one inside it would change only the goroutine's snapshot, so it is rejected
with `GT0114`:

<!-- compile_fail GT0114 -->
```gossamer
fn main() {
    let mut total = 0
    let h = spawn(|| total += 1)
    h.join()
}
```

Answer the value through `join()` instead, send it on a channel, or share it
through a `sync::Shared`.

Calling a method declared `&mut self` on a captured value is a write too,
whatever the method is called.

Only a closure written at the `spawn` takes snapshots. Any other callable
that reaches a goroutine - a closure bound earlier, a closure the spawned
one calls, or a callable parameter a function spawns - carries the bindings
it captured themselves, so it is accepted only when nothing writes them.
One that would share a written binding with the code around it is rejected
with `GT0118`, as is one whose captures cannot be seen where it crosses (a
call's result, a field, a reassigned binding):

<!-- compile_fail GT0118 -->
```gossamer
fn main() {
    let mut n = 0
    let next = || {
        n += 1
        n
    }
    let h = spawn(next)
    println("{:?} {}", h.join(), n)
}
```

A callable whose captures nothing writes crosses freely, and a named
function always does:

```gossamer
fn work() -> i64 { 41 }

fn main() {
    let base = 10
    let f = || base + 1
    let a = spawn(f)
    let b = spawn(|| f() * 2)
    let c = spawn(work)
    println("{:?} {:?} {:?}", a.join(), b.join(), c.join())
}
```

## A closure on its own line

A line that starts with `|` or `||` begins a new statement, so a function
can end with a closure, with parameters or without. A logical or that spans
lines ends the first line with `||`; a closure that nothing binds, passes, or
answers is GP0067, which is what a `|| b` line meant to continue `a` becomes.

```gossamer
fn counter_from(start: i64) -> Fn() -> i64 {
    let base = start * 10
    || base + 1
}

fn main() {
    let next = counter_from(4)
    println("{}", next())
}
```
