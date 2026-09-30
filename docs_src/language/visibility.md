# `lang::visibility`

Three visibilities: private by default (the declaring module and its descendants), `pub(package)` (every module of the declaring package), and `pub` (the package's public API). Declared per item, per method, and per struct field; `pub(crate)` / `pub(super)` / `pub(in path)` are rejected (`GP0038`).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Packages, modules, and module descendants

Three levels of code organization, and visibility is defined against
them. They are distinct, and the words are not interchangeable.

A **package** is the unit of distribution: one `project.toml`, one
project id, one thing `gos add` pulls in. The library or application
you are developing is a package. Its dependencies are other packages.

A **module** is a directory of source under `src/`. `src/util/mod.gos`
declares module `util`. A single file directly under `src/` -
`src/util.gos` - declares the same module.

A **module descendant** is a module nested inside another. `src/deep/`
declares `deep`; `src/deep/nest/` declares `deep::nest`, a descendant
of `deep`. Descendancy is what the default visibility is written
against, and it runs one way: `deep::nest` is a descendant of `deep`,
and `deep` is not a descendant of `deep::nest`.

```text
my-app/                    the package
  project.toml             its manifest - one per package
  src/
    main.gos               the entry file, at the package root
    util/mod.gos           module `util`
    deep/mod.gos           module `deep`
    deep/nest/mod.gos      module `deep::nest`, a descendant of `deep`
```

An inline `mod name { ... }` block declares a module too, with the same
rules. Directories are the usual form; inline modules keep a small
grouping in one file.

## The three visibilities

An item with no annotation is **private to the module that declares it
and to that module's descendants**. This is Rust's rule. A module's
private helpers are reachable from the module itself and from anything
nested inside it, and from nowhere else.

`pub(package)` widens that to **every module of the declaring package**,
and no further. A dependency cannot reach it. This is the equivalent of
Rust's `pub(crate)`, and it is what internal machinery shared across a
package should use.

`pub` makes the item **part of the package's public API**: reachable by
anything that depends on the package. `pub` is a commitment, so annotate
it deliberately.

```gossamer
// src/util/mod.gos - module `util`
fn helper() -> i64 { 41 }                       // util and its descendants
pub(package) fn shared() -> i64 { helper() + 1 } // anywhere in this package
pub fn public() -> i64 { shared() + 1 }          // this package's API
```

```gossamer
// src/main.gos - the package root
use util::{shared, public}

fn main() {
    println("{} {}", shared(), public())
}
```

Naming `util::helper` from `main.gos` is `GR0008`: `helper` is private
to module `util`, and the package root is not one of `util`'s
descendants.

Rust's other restriction forms do not exist. `pub(crate)`, `pub(super)`,
and `pub(in path)` are rejected with `GP0038` naming `pub(package)` -
one restricted spelling, not four.

## Direction matters

Visibility flows inward, never outward. A descendant sees its ancestors'
private items; an ancestor does not see its descendants'.

```gossamer
mod outer {
    fn secret() -> i64 { 1 }

    mod inner {
        // `inner` is a descendant of `outer`, so `outer`'s private
        // items are in reach.
        pub fn read() -> i64 { super::secret() }
    }

    // `outer` is NOT a descendant of `inner`. A non-`pub` item of
    // `inner` cannot be named here.
    pub fn total() -> i64 { inner::read() }
}
```

## What carries a visibility

Every named item: `fn`, `struct`, `enum`, `trait`, `const`, `static`,
`type` alias, and `mod`.

**Methods** carry their own, declared inside the `impl` block. A method
without `pub` is private to the module the `impl` was written in, even
when the type is `pub` (`GT0063`). A public type with private helpers is
the normal shape.

**Struct fields** carry their own too. A `pub` struct may keep private
fields (`GT0065`): the type is API while its representation is not. A
private field cannot be read, written, destructured, or named in a
struct literal from outside - which means a struct with any private
field cannot be constructed from outside the module that declares it,
and the declaring module's constructor becomes the only way in.

```gossamer
mod money {
    pub struct Amount {
        pub currency: String,
        cents: i64,             // private: the representation
    }

    impl Amount {
        pub fn new(currency: String, cents: i64) -> Amount {
            Amount { currency: currency, cents: cents }
        }

        pub fn cents(&self) -> i64 { self.cents }

        fn normalize(&self) -> i64 { self.cents }   // private helper
    }
}
```

From another module, `a.currency` and `a.cents()` are reachable;
`a.cents`, `a.normalize()`, and `Amount { currency: .., cents: .. }` are
not.

## A private module blocks what it contains

A `pub` item inside a private module is still unreachable from outside,
and the module is what the diagnostic names - it is the one place a
`pub` would unblock the path.

<!-- compile_fail GR0008 -->
```gossamer
mod deep {
    mod nest { pub fn nested() -> i64 { 1 } }   // `nest` is private
}

fn main() { println("{}", deep::nest::nested()) }
// error[GR0008]: module `nest` is private to module `deep`
```

## Importing does not widen anything

A `use` is a spelling convenience, not an access grant. `use
util::helper` on a private `helper` is reported at the reference, the
same as writing `util::helper()` in full. Visibility is decided by where
the name is *used*, not by where the `use` was written.
