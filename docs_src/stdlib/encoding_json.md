# `std::encoding::json`

Status: experimental

JSON parser, emitter, and derive support.

## Items

| Item | Signature | Description |
|---|---|---|
| `Serialize` | `trait Serialize` | Trait for converting a value to JSON. |
| `Deserialize` | `trait Deserialize` | Trait for parsing a value from JSON. |
| `encode` | `fn encode(value: json::Value) -> String` | Encodes a `Serialize` value as a JSON `String`. |
| `decode` | `fn decode(source: String) -> Result<json::Value, errors::Error>` | Decodes a JSON `String` into a `Deserialize` value. |
| `Value` | `type Value` | Dynamically typed JSON value. |
| `Error` | `type Error` | Error raised by encoding/decoding operations. |
| `parse` | `fn parse(source: String) -> Result<json::Value, errors::Error>` | Parses JSON text into a dynamic Value. |
| `render` | `fn render(value: json::Value) -> String` | Renders a dynamic Value as compact JSON text. |
| `encode_pretty` | `fn encode_pretty(value: json::Value) -> String` | Renders a value as indented JSON text. |
| `valid` | `fn valid(source: String) -> bool` | Reports whether the text is well-formed JSON. |
| `get` | `fn get(value: json::Value, key: String) -> Option<json::Value>` | Looks up an object field on a dynamic Value. |
| `set` | `fn set(value: json::Value, key: String, next: json::Value) -> json::Value` | Sets an object field on a dynamic Value. |
| `at` | `fn at(value: json::Value, index: i64) -> json::Value` | Indexes an array element on a dynamic Value. An index the array does not have reads as JSON null. |
| `keys` | `fn keys(value: json::Value) -> Option<Vec<String>>` | Object field names of a dynamic Value. |
| `len` | `fn len(value: json::Value) -> i64` | Element / field count of a dynamic Value. |
| `is_null` | `fn is_null(value: json::Value) -> bool` | Reports whether a dynamic Value is null. |
| `as_str` | `fn as_str(value: json::Value) -> Option<String>` | Reads a dynamic Value as Option<String>. |
| `as_i64` | `fn as_i64(value: json::Value) -> Option<i64>` | Reads a dynamic Value as Option<i64>. |
| `as_u64` | `fn as_u64(value: json::Value) -> Option<u64>` | Reads a dynamic Value as Option<u64>, for a non-negative integer up to u64::MAX. |
| `as_f64` | `fn as_f64(value: json::Value) -> Option<f64>` | Reads a dynamic Value as Option<f64>. |
| `as_bool` | `fn as_bool(value: json::Value) -> Option<bool>` | Reads a dynamic Value as Option<bool>. |
| `as_array` | `fn as_array(value: json::Value) -> Option<Vec<json::Value>>` | Reads a dynamic Value as an array of Values. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
