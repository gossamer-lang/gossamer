# Incremental front end

`gos`, `gos check`, `gos run`, `gos test`, and `gos build` all reach the
compiler through one gate: parse, resolve, typecheck, exhaustiveness, and
arena-escape analysis, run back to back under a single fatal-error policy.
Repeating that work on a project that has not changed is pure latency, so the
gate is backed by a content-addressed cache.

The cache stores the gate's complete accepted output. A hit skips every
front-end stage and hands the caller a deserialized result; a miss runs the
gate and publishes a new blob.

## What is stored

One postcard blob per accepted compile, holding the four values that make up
a checked program:

| Value | Produced by | Why it must be stored |
|---|---|---|
| `SourceFile` | parser (post-autoderive) | the AST every later stage walks |
| `Resolutions` | resolver | `NodeId` to definition side table |
| `TypeTable` | type checker | `NodeId` to `Ty` side table |
| `TyCtxt` | type checker | interner the `Ty` handles index into |

`TypeTable` entries are meaningless without the `TyCtxt` that produced them,
so the two are always written and read as one unit. HIR and MIR are not
cached: they are derived from these four values and only `gos build` and
`gos run` need them.

Only a pass that produced **zero** diagnostics publishes a blob. A cache hit
is therefore also proof that the program was accepted, which is what lets the
gate return early without re-running the analyses.

## Cache key

The key is a SHA-256 over every input that can change the result:

- the source bytes handed to the gate - already the bundled program, so a
  change in any sibling module or path dependency changes it;
- the toolchain version, the frontend build stamp, and the running `gos`
  executable's own size and mtime, so any rebuilt compiler starts cold;
- the `FileId` that spans in the cached AST are anchored to;
- the compile target triple, including an explicit `--target`;
- whether `#[cfg(test)]` items are visible, since that decides which items
  the resolver admits;
- a digest of the registered Rust-binding signatures, which participate in
  name resolution and type checking.

A blob also carries a magic prefix that encodes the payload schema. Bumping
it retires every blob written by an older layout, so a schema change can
never be mis-decoded as the current one.

## Location

In precedence order:

1. `GOSSAMER_CACHE_DIR`, when set;
2. `<project>/.gos-cache/frontend`, where `<project>` is the nearest ancestor
   directory holding a `project.toml` - the same anchor `gos build` uses for
   its object and link-stamp caches;
3. the per-user cache root: `$XDG_CACHE_HOME/gossamer/frontend`,
   `$HOME/.cache/gossamer/frontend`, or `%LOCALAPPDATA%\gossamer\frontend`.

`GOS_NO_CACHE` disables reads and writes entirely, matching the LLVM object
cache's opt-out. `gos cache status`, `gos cache prune`, and `gos clean
--frontend` report and reclaim these directories under the `frontend` class.

## Concurrency and corruption

A blob is written to a temporary file named for the writing process and a
per-write counter, flushed, and then renamed over the final path. Rename is
atomic on every supported platform, so a concurrent reader observes either
the previous blob or the complete new one, never a partial write. A failed
write removes its temporary file.

Reads are defensive by construction: the file is read through a length cap
one byte past the maximum accepted blob, the magic prefix must match, and the
postcard decode must succeed. Any failure is a miss, not an error. Cache
contents are disposable, so a corrupt entry costs one recompile.

## What is not cached

Comptime folding (`gos check` and `gos build` evaluate `comptime` regions on
the bytecode VM before the gate) runs the gate itself and so benefits from
the cache, but the fold's own output is not separately cached. Lints,
HIR/MIR lowering, and native code generation have their own caches or none.

## The LLVM object cache

The LLVM backend splits a program into one LLVM module per source module
(modules joined by a recursive call cycle share one) and keeps one object per
module under `ir-cache`. A module's cache key is its rendered IR text, plus the
target triple, the profile, the compiler build, and the `opt` / `llc` / `clang`
identities and code-generation settings. The text is what LLVM compiles, so an
edit that leaves a module's IR unchanged - a comment, a function moved within
its file, a body edited in another module that this one does not inline - is
served from the cache, and an edit that reaches the IR is recompiled.

A release module carries an `available_externally` copy of each small callee it
reaches in another module, so `opt` inlines across the module boundary as it
would inside one whole-program module. The copies are part of the module's
text, and therefore of its key: editing a callee recompiles every module that
inlined it. A module declares only the functions and RC type metadata its own
text names, so adding a function elsewhere does not change it.

Cache misses compile in parallel, one `opt` + `llc` pair per module, on up to
eight workers (`GOS_LLVM_JOBS` overrides the count). A finished object is
copied to a temporary name and renamed into the cache. `--reproducible` builds
the program as one module. `GOS_PIPELINE_TRACE=1` names each module, its body
and import counts, and whether it was cached or compiled.
