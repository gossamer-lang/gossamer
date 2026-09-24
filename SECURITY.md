# Security Policy

## Supported versions

Gossamer ships from `main`. Tagged releases older
than the most recent tag are unsupported.

## Reporting a vulnerability

Please report suspected vulnerabilities privately rather than in
public issues or pull requests.

- Open a private security advisory via GitHub:
  `https://github.com/gossamer-lang/gossamer/security/advisories/new`.
- If that channel is unavailable, email the maintainers listed in
  `Cargo.toml` under `authors`.

Please include:

- A description of the issue and its impact.
- A minimal reproducer (input file, command line, or curl request).
- The affected commit hash or tag.
- Any suggested mitigation or patch, if you have one.

Do not file public issues, pull requests, or discussion posts for
unfixed vulnerabilities.

## What we consider in scope

- Memory safety or panic-from-untrusted-input in the compiler front
  end (lexer, parser, resolver, type checker, HIR lowering).
- Memory safety or panic-from-untrusted-input in the HTTP server
  (`std::http::server`) and HTTP client.
- Dependency-resolution or manifest-parser issues that let a
  malicious package compromise `gos build` or `gos tidy`.
- Code-execution issues in `gos` / `gos build` on attacker-
  controlled source files.
- Launcher-script injection via crafted paths or file names.

## Compile-time evaluation is capability-controlled

A `comptime { ... }` region and every `comptime fn` call are evaluated
on the bytecode VM while the program is compiled, so they run with the
privileges of whoever started the compile - including on source
somebody else wrote, and on the `gos check` an editor, the language
server, the MCP server, and CI run unattended.

`--comptime-io` bounds what that evaluation may reach:

| Level | Compile-time capabilities |
|---|---|
| `none` | No I/O at all. A comptime region is pure computation over its inputs. |
| `confined` (default) | Reads under the source tree. Writes, process spawn, network, environment mutation, and reads that leave the tree are denied. |
| `full` | Everything the compiling user can do. Never a default. |

A read is compared against the canonical path, so a symlink pointing
out of the source tree is denied at its target. `project.comptime-io`
pins a posture for a project; the toolchain takes the more restrictive
of the manifest and the command line, because the manifest is written
by the party the policy defends against. `codegen!` is pure computation
and is unaffected at every level.

A denied call reports `GX0010` naming the builtin, the capability
class, and the option that would permit it. Run
`gos explain GX0010` for the long form.

## Out of scope

- Self-DoS: slow programs, large files, or runaway recursion in the
  interpreter. These are bugs, not vulnerabilities.
- Outputs that depend on intentionally-disabled lints (for example,
  suppressing `unused_variable` with `_` prefixes).
- Vulnerabilities requiring a pre-existing local root compromise.
