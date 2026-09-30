# Where semantics live

Gossamer runs a program three ways: the bytecode VM compiles the checked
program's HIR to bytecode, and the Cranelift JIT and LLVM backend compile it
through MIR. A behaviour the language defines has to come out the same on all
three. This note records how that is kept true, and the decision about the
VM's input.

## The decision: the VM keeps compiling from HIR

The VM does not move onto MIR for 1.0.

- `gos run` and the REPL start by compiling to bytecode. HIR-to-bytecode is
  one pass; MIR construction, its ownership passes, and its optimisations are
  several, and their cost lands on every interactive run.
- MIR carries decisions that exist for native code - drop placement, carrier
  layout, share transfer - that the VM, with its own reference-counted values,
  does not need and would only have to interpret.
- A divergence is a bug in one of three places, each of which has a single
  home now: a desugaring, a shared algorithm, or a tier's lowering of a
  primitive. The first two no longer have two implementations to drift apart,
  and the third is what the parity harness exists to catch.

## One home for each kind of meaning

**Desugarings run before either tier sees the program.** A construct that
means another construct is rewritten on the syntax tree in the parse
pipeline (`gossamer-parse`'s autoderive passes), so the VM and MIR both
receive the rewritten form: `f"..."` strings, trait-qualified calls,
`(lo..hi).contains(x)`, typed serde, iterator adapters on user types, and
operator impls per right-hand type all work this way.

**Shared algorithms live in `gossamer-core`.** An algorithm both the VM's
standard library and the native runtime need is written once in
`crates/gossamer-core`, beneath both. JSON is the first: its value, grammar,
limits, and diagnostics are one parser, which the VM parses with and native
builds validate with before materializing a document, so every tier accepts,
rejects, and describes the same documents alike.

**Each tier lowers primitives itself.** A runtime function has a checker
signature, a VM builtin, and a C-ABI shim with its Cranelift and LLVM
dispatch rows. The JIT's dispatch table is generated from the ABI registry;
the tier-parity harness runs every registered fixture on all three tiers and
compares their output byte for byte.

## Evidence

- `crates/gossamer-cli/tests/tier_parity` runs each fixture on the VM, the
  JIT, and a native build, and CI fails on any difference.
- `gos feature-status --check` refuses a feature reported as shipped without
  a fixture that passes on every tier.
- The debug-build MIR reference-count verifier checks every function the
  native tiers compile.
