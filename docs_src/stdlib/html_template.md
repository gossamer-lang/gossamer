# `std::html::template`

Context-aware HTML templates with auto-escape (text/attr/URL/JS). The context classifier is heuristic - sound for typical server-rendered responses but NOT a content-security-policy substitute; sanitize untrusted HTML fragments with a dedicated sanitizer.

## Items

| Item | Signature | Description |
|---|---|---|
| `render_json` | `fn render_json(template: String, data: json::Value) -> Result<String, errors::Error>` | render_json(source, json_data) -> Result<String, Error>: renders a context-aware HTML template against a JSON data context. Stateless and wired bit-identically across every tier. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
