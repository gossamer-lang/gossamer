# `std::encoding::toml`

Status: experimental

TOML 1.0 parsing + emission. Pair with the turbofish `from_toml::<Type>` for typed decoding (struct auto-derive).

## Items

| Item | Signature | Description |
|---|---|---|
| `to_json` | `fn to_json(source: String) -> Result<String, errors::Error>` | Convert TOML text to JSON text; returns Result<String, errors::Error>. |
| `from_json` | `fn from_json(source: String) -> Result<String, errors::Error>` | Render JSON text as TOML text; returns Result<String, errors::Error>. |
| `is_valid` | `fn is_valid(source: String) -> bool` | Return true iff the string parses as TOML. |
| `pretty` | `fn pretty(source: String) -> Result<String, errors::Error>` | Round-trip TOML through the pretty-printer. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
