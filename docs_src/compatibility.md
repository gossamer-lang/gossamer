# Compatibility

What a Gossamer release may change, what it may not, and how the
toolchain carries your code across the difference.

This policy exists so an agent can answer "will this break under me?"
without reading a changelog. A promise nothing verifies is not worth
reading; every rule below is either enforced by a gate in CI or carried
by a `gos fix` rewriter.

**A change you have to make by hand is a bug in the release** - if a
version bump requires edits, the toolchain owes you the rewriter that
makes them.

## What a patch release (0.47.0 -> 0.47.1) may change

- Fix a defect, including one whose old behaviour a program depended on.
- Report an existing fault differently: a clearer diagnostic, a
  different exit status for a fault that already ended the program.
- Add a stdlib item, a lint, a subcommand, or a flag.

It may not change the meaning of a program that was already correct.

## What a minor release (0.47 -> 0.48) may change

Everything a patch release may, plus:

- Change the meaning of a construct, **only** with a `gos fix` rewriter
  that migrates existing code.
- Deprecate a spelling in favour of a canonical one. The old spelling
  keeps parsing for at least one further minor release.
- Change a default, where `project.toml` can restore the old one.

## Which toolchain a project is read by

A project states the toolchain it is written against, exactly, in its
manifest:

```toml
[project]
gossamer-version = "v0.55.0"
```

There are no editions: one toolchain version reads one language, and
the manifest names which version that is. The spelling decides how
strictly:

| Spelling | Meaning |
|---|---|
| `"v0.55.0"`, or `"=v0.55.0"` | That toolchain and no other |
| `"^v0.55.0"` | That toolchain or any later one |

A toolchain the requirement does not accept refuses the project rather
than failing later on a surface it does not have, so the mismatch is
reported where it can be acted on. `gos new` scaffolds a `^` floor,
because a project should not stop building at the next release unless
its author asked for that.
`gos new` stamps the toolchain that scaffolded the project; raise the
value when you adopt a newer one, after `gos fix` has carried the
source across.

Removing a stdlib item requires a deprecation period, whichever version
is stated.

## How the toolchain carries the delta

```text
gos fix --list          # what migrations exist
gos fix                 # apply them across the project
gos fix --check         # non-zero exit when any are pending (CI)
```

Every rewriter is:

- **deterministic** - the same input produces the same edits;
- **idempotent** - running it on its own output changes nothing, checked
  on every run and reported as an error if a rewriter lapses;
- **verified** - the file is re-checked after rewriting, and the result
  is discarded rather than written if it would introduce a diagnostic.

`gos fix` is separate from `gos lint --fix` on purpose. A lint is an
observation about the code you wrote, and you are entitled to disagree
with it. A migration is a mechanical upgrade the toolchain owns; there
is nothing to have an opinion about.

## What is not covered

- **Performance.** A release may make a program slower or faster. Timing
  is not part of the contract.
- **Diagnostic text.** Codes (`GT0017`) are stable and worth matching
  on; the prose next to them is not.
- **Output of a program with nondeterministic behaviour.** Goroutine
  interleaving is not ordered by this policy.
- **Anything marked `experimental` by `gos feature-status`**, which is
  the surface still being designed.
