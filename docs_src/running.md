# Running Gossamer

Once `gos` is on your `PATH`, run a program with `gos run FILE [ARGS...]`.
`FILE` may be a source file with any filename extension or a project directory containing
`project.toml`; `gos` with no arguments drops into the REPL. Every token after
`FILE` is passed to the program verbatim, including option-shaped values and a
literal `--`. Put `gos run` options before `FILE`.

## Cheat-sheet

| Command | What it does |
|---------|--------------|
| `gos new example.com/app --path ./app` | Scaffold a project |
| `gos new example.com/app --template lib` | Scaffold a library (`bin`, `lib`, `service`, `workspace`, `binding`) |
| `gos init example.com/app` | Scaffold just `project.toml` in the CWD |
| `gos run src/main.gos` | Register-based bytecode VM with in-process Cranelift JIT |
| `gos run --no-jit src/main.gos` | Same VM, pure bytecode dispatch (JIT off) |
| `gos check src/main.gos` | Type-check + exhaustiveness |
| `gos build src/main.gos` | Native build via LLVM AOT with checked arithmetic (`opt -O1 | llc -O0`), then links the user's object against the `gossamer-runtime` staticlib into an ELF/Mach-O/PE. |
| `gos build --release src/main.gos` | Optimised native build - full LLVM `opt -O3 | llc -O3` pipeline, static musl on Linux. |
| `gos build --target aarch64-unknown-linux-musl src/main.gos` | Cross-compile to a Tier 2 Linux-musl target. CI executes the resulting AOT output under QEMU and compares it with the pure bytecode VM. Other registered triples are not necessarily supported deployment targets. |
| `gos fmt src/main.gos` | Rewrite canonically; `--check` refuses to edit |
| `gos doc src/main.gos` | List items + docstrings |
| `gos test src/main.gos` | Discover and run `#[test]` functions |
| `gos bench src/main.gos` | Time `#[bench]` functions |
| `gos lint .` | Run the lint suite over a directory |
| `gos lint --deny-warnings .` | Fail CI on any warning |
| `gos lint --explain unused_variable` | Long-form rationale for a lint |
| `gos explain GT0001` | Long-form rationale for a diagnostic code |
| `gos watch [PATH] [--] [ARGS...]` | Restart a development service after validating local source changes |
| `gos serve [DIR] [--port 8000] [--bind 127.0.0.1]` | Serve a directory's files over HTTP, as `python -m http.server` does |
| `gos add example.org/lib@1.2.3` | Add a dependency to `project.toml` |
| `gos remove example.org/lib` | Drop a dependency |
| `gos update` | Update locked dependencies within declared ranges |
| `gos tidy` | Remove unused project dependencies and canonicalise the manifest |
| `gos fetch` / `gos vendor` | Prepare git / registry / tarball dependency sources |
| `gos bindgen native/lib.rs` | Scaffold a Rust binding crate from an existing source file |
| `gos` (no args) / `gos repl` | Start the interactive REPL |
| `gos repl` then `%help` | List REPL commands |
| `gos repl` then `%info strings::trim` | Show help and the relevant module or type listing |
| `gos repl` then `%info *trim*` | Search public catalog entries by wildcard |
| `gos repl` then `%explain value --details` | Inspect a binding's available methods with descriptions and examples |

## Entry file

`gos run file.gos [ARGS...]` and `gos build file.gos` accept a file with no
`fn main`: bare statements at the top of the entry file become the
body of an implicit `fn main()`. So a one-line `println("Hello
World")` file runs as-is. See
[Top-level statements](language/top_level_statements.md). A project's
entry file is `src/main.gos` by convention, or whatever
`[project] entry` names in `project.toml`.

`gos watch` is a restart-based development supervisor for HTTP services. It
watches the project, its transitive local path dependencies, and manifests;
after an edit it validates the revision in-process, gracefully terminates the
old `gos` child, waits for the port to be released, and starts a
replacement. An invalid edit leaves the last known-good service running. It
intentionally does not preserve in-memory state, WebSocket connections, or
streaming responses, and is not a production zero-downtime deployment
mechanism. Use `--debounce MS`, `--grace MS`, `--no-check`, `--clear`, and
`--locked` to tune the loop. `gos dev` remains accepted as a compatibility
alias.

## Environment variables

- `GOSSAMER_HTTP_MAX_REQUESTS=N` - ask the HTTP server to exit
  after `N` requests. Used by CI tests; leave unset for normal
  operation. A visible warning prints when the env var is
  honoured.
- `NO_COLOR` / `CLICOLOR_FORCE` - standard colour toggles.
- `EDITOR` - used by the REPL's `%edit` meta-command (Stream K
  follow-up).

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Subcommand-reported failure |
| 2 | Clap argument parsing failure |
| 101 | Panic from the compiler (file a bug) |
