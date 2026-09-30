# `std::html`

Status: experimental

HTML text escaping and unescaping.

## Items

| Item | Signature | Description |
|---|---|---|
| `escape` | `fn escape(text: String) -> String` | Escapes HTML metacharacters (, <, >, ", '). |
| `unescape` | `fn unescape(text: String) -> String` | Resolves HTML entities back to their characters. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
