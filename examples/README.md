# Gossamer examples

Canonical starter programs - each one covers a single topic a new
user will reach for in their first hour with the language. Every
file passes `gos check`, and the set is exercised by the CLI
integration tests across all three tiers (`gos`, the in-process
JIT, and the `gos build` native binary).

| File | Topic | Status |
|------|-------|--------|
| `hello_world.gos` | First program; `println` | runs |
| `input.gos` | Basic interactive input with `std::io` | runs |
| `web_server.gos` | HTTP/1.1 routed server over `std::http` | runs |
| `cli_args.gos` | Command-line argument parsing (`std::flag`) | runs |
| `file_io.gos` | File read / write - text + JSON | runs |
| `json_structs.gos` | Typed JSON: `json::decode::<T>` / `json::encode`, serde attributes, `sort: true` | runs |
| `json_dynamic.gos` | Untyped JSON: `json::parse`, `get` / `at` / `pointer` / `entries`, `to_value` | runs |
| `toml_demo.gos` | TOML: typed `toml::decode::<T>` / `toml::encode`, `toml::parse`, text converters | runs |
| `yaml_demo.gos` | YAML: typed `yaml::decode::<T>` / `yaml::encode`, multi-document streams | runs |
| `http_client.gos` | HTTP client / REST call | runs (pair with `web_server.gos`) |
| `data_structures.gos` | Lists, maps, sets from `std::collections` | runs |
| `control_flow.gos` | Loops + conditionals + match (pure syntax) | runs |
| `errors.gos` | `Result<T, E>` + `?` + `std::errors::wrap` | runs |
| `concurrency.gos` | Goroutines + channels - producer / consumer | runs (`gos` and `gos build`) |
| `go_spawn.gos` | Goroutines without channels - fan-out sketch | runs and builds natively |
| `function_piping.gos` | `|>` forward-pipe: composing free functions | runs |
| `semicolon_separators.gos` | Optional same-line statement separators | runs |
| `grep.gos` | Simple Unix-style CLI tool | runs (reads stdin) |
| `testing.gos` | `#[test]` harness + `std::testing` | runs via `gos test` |
| `projects/web_service/` | Full project layout (`project.toml` + `src/`) - multi-endpoint HTTP service with unit tests | `cd` in and run `gos test` |

## Running

```sh
gos run examples/hello_world.gos
gos run examples/input.gos
gos run examples/web_server.gos &
curl 'http://localhost:8080/echo?msg=hi'
gos test examples/testing.gos

cd examples/projects/web_service
gos test           # no args - walks up to project.toml, scans src/
gos run src/main.gos
```

## Conventions

- File-level docstrings use `/* ... */` block comments. Block
  comments do not nest - the first `*/` closes the comment.
- Inline comments use `//`.
- Formatted output goes through the six compiler-known calls
  `format`, `println`, `print`, `eprintln`, `eprint`, and `panic` -
  one allocation per render, no `+` chains.
- No user-defined macros. Every `name!(...)` is a parse error, and a
  `!` after a compiler-known name is `GP0049`.
- Ordinary double-quoted string literals span multi-line
  without extra syntax.
