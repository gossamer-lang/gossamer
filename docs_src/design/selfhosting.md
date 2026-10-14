# Self-hosting feasibility study (Phase 30)

> This is the original Phase 30 feasibility write-up, kept for its gap
> analysis. Most prerequisites it lists below have
> since shipped on all three tiers - growable `Vec`, `Map` /
> `Set` / `BTreeMap`, generic functions and types with full
> monomorphisation, traits with static dispatch, byte literals, and
> the `matches` / `format` desugar calls - and the `examples/selfhost/`
> ports it references are no longer in the tree. Read it as a record of
> where self-hosting stood then, not as the current surface.
>
> In 0.72.0 the parser was ported in full, byte-identical with the Rust
> parser's output on every file of the repository and the build corpus, and
> measured inside `gos check`. Even rewritten onto a flat, index-addressed
> tree it ran at 1.07x to 1.41x the Rust frontend, so no part of the
> frontend is self-hosted yet. Both ports now live in
> `benchmarks/frontend_parser/`, where the cost of reference-counted trees
> is measured.

## Scope

Phase 30 of the implementation plan asks whether the Gossamer compiler
can, in its current shape, begin compiling itself. This document
captures the state of that question at the end of Phase 30 and lists
every gap standing between today's surface language and a full
self-hosted front-end.

Companion ports under `examples/selfhost/`:

- `lexer.gos` - minimal tokeniser over a subset of Gossamer source.
- `parser.gos` - minimal recursive-descent parser over a synthetic
  token stream.

Both files parse cleanly through `gos parse`, which is asserted by
`crates/gossamer-cli/tests/cli.rs::selfhost_ports_parse_cleanly`.

## What works today

- **Enums with data**, pattern-matched exhaustively - viable for
  `TokenKind`, `Expr`, `Stmt`, `Ty`.
- **Structs with named fields**, `&` and `&mut` references - viable
  for `Lexer`, `Parser`, `TyCtxt`-shaped contexts.
- **`for`, `while`, `loop`, `match`, `if`/`else`** - enough control
  flow for a recursive-descent parser.
- **Function calls, method calls, tuples, slices, mutable locals**.
- **`println` / `print` / `eprintln` / `format`** builtins.
- **Package manager** (Phase 27-28): project layout, lockfile,
  fetcher, vendor directory. Ready to host a `gossamer-selfhost`
  crate once the front-end ports land.
- **Build graph** (Phase 29): incremental, parallel, content-
  addressable cache. Ready to compile the ports once the back-end
  gaps below are closed.

## Gaps blocking a real front-end port

The following are required by the Rust implementation of the front-end
and are not yet first-class in Gossamer source:

1. **Dynamic arrays / `Vec`-shaped growable collections.** The ports
   lean on `[T]` slice-push; the runtime supports it through stubs
   but the language surface needs a stable story for `push`, `pop`,
   `extend`, `with_capacity`.
2. **Hash maps.** Every non-trivial compiler phase has a symbol
   table. `std::collections::Map`-equivalent needs a public
   surface (`BTreeMap` would do; neither is wired).
3. **String / byte indexing.** The lexer port calls `byte_at(i)` and
   `slice(start, end)`. These are convenient shorthands over
   `str::as_bytes()` and `&str[start..end]`; the language needs the
   equivalent range-slice notation or methods exposed stably.
4. **`?` error propagation on `Result<T, E>`.** Already parses; the
   typechecker still needs the generic arithmetic pass that
   distinguishes `Ok` from `Err`.
5. **Generic functions and types.** The parser needs `Vec<T>`,
   `Option<T>`, `Result<T, E>`. Monomorphisation lands in Phase 10
   of the plan but is not yet wired through to codegen.
6. **Traits / interfaces.** `Display`, `Debug`, `Iterator` are all
   used pervasively in the Rust original; Phase 08 delivered the
   trait surface but trait-objects-through-the-VM is still stub.
7. **Stdlib `io::Read`-style traits.** The lexer needs a source of
   bytes that is not necessarily a fully-loaded `String`. Today we
   load the whole file.
8. **Byte literals, `b"..."`, `b'x'`.** Lexer proper uses byte
   literals; the Gossamer port had to synthesise integer constants
   instead.
9. **Macros.** The Rust parser uses `matches!`, `format!`,
   `vec![]`, `thiserror::Error`-derived enums. Macros are not
   planned for the 1.0.0 release; the port must inline these.

## Benchmarks

A throughput benchmark (tokens/sec, LoC/s) is tracked as work-item
under Phase 31 once `gos test` and `gos bench` are wired. For now
the only measurement is the no-op incremental build budget from
Phase 29 (`tests/build_graph.rs::no_op_rebuild_completes_within_the_phase_29_budget`).

## Conclusion

Self-hosting the front-end is reachable but not yet practical. The
single biggest prerequisite is the stdlib surface for collections
(items 1, 2 above); the second is closing the generics/trait-object
loop in the VM. With those two, porting the Rust lexer and parser
verbatim is an ~6 KLoC translation exercise, not a research project.

No work is proposed for Phase 30 beyond the ports and this write-up.
Subsequent phases will revisit self-hosting once the 1.0.0 release ships.
