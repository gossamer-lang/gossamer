# `std::flag`

Status: experimental

Batteries-included CLI argument parsing.

## Items

| Item | Signature | Description |
|---|---|---|
| `Value` | `type Value` | Parsed command-line flag value. |
| `Set` | `type Set` | Flag definition + parsing set. |
| `Error` | `type Error` | Error produced while parsing flags. |
| `parse` | `fn parse(args: Vec<String>) -> Result<Vec<String>, errors::Error>` | Parses the default flag set against the given args. |
| `string` | `fn string(name: String, default: String, usage: String, short: char) -> flag::Flag` | Defines a string flag on the default set. |
| `int` | `fn int(name: String, default: i64, usage: String, short: char) -> flag::Flag` | Defines an integer flag on the default set. |
| `bool` | `fn bool(name: String, default: bool, usage: String, short: char) -> flag::Flag` | Defines a boolean flag on the default set. |
| `define` | `fn define(name: String, flags: Vec<flag::Flag>) -> flag::FlagSet` | Registers a flag definition on the default set. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
