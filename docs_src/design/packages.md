# Package format and separate compilation

The decision for 1.0: a package is distributed as source, and a build
compiles every package it depends on together with the program. There is no
per-package interface file and no precompiled object in a package.

## What a package is

A package is a signed source archive: its `project.toml`, its `src/` tree, and
the publisher's Ed25519 signature over the archive's SHA-256 digest (see
[Libraries](../libraries.md)). The registry serves archives and an index; a
fetch verifies the signature and unpacks the source into the package cache.

## How a build compiles one

The front end reads the entry file and every module and dependency it reaches
as one unit. Each module's source is registered as its own file in the source
map, and the unit records which of its regions came from which file, so a
diagnostic or a runtime trace names the module's real path and line whichever
package it came from.

Two caches keep that from costing a full rebuild each time:

- The front-end cache stores a checked program - its syntax tree, resolutions,
  type table, and type interner - under a key covering every input, so an
  unchanged program skips parsing and checking outright
  ([Incremental front end](incremental.md)).
- The native object cache keys each LLVM chunk by the IR text it compiles and
  the settings it compiles under, so an unchanged function is never handed to
  LLVM twice, whichever package it came from.

## Why not interface plus object

- Generic functions are monomorphised per call site on every tier. A package
  that exposes `fn f<T: Trait>(x: T)` has no object code until a dependent
  names `T`, so an object file could hold only the non-generic part of a
  package's surface.
- `comptime` code runs while the dependent is compiled, and `typeInfo::<T>()`
  reflects the dependent's types, so what a package contributes can depend on
  who uses it.
- The bytecode VM compiles from the same source the native tiers do. A
  precompiled native object would give a package a native build the VM could
  not reproduce, which the tier-parity guarantee rules out.
- Cross-package inlining of small functions is where much of the performance
  of iterator and collection code comes from; an object boundary would stop
  it.

Native code that must ship prebuilt reaches a program through a Rust binding
crate, built from its Rust source ([Rust bindings](../rust_bindings.md)).

## What stays open

The registry protocol is versioned, so a later release can add an interface
section to an archive - the checked signatures of its public items, for
faster checking of large dependency graphs - without changing what a source
package is. Such a section would be a cache of what the source already says,
never a replacement for it.
