# `std::encoding::yaml`

Status: experimental

YAML 1.2 parser/emitter (serde_norway-backed).

## Items

| Item | Signature | Description |
|---|---|---|
| `Value` | `type Value` | Dynamically typed YAML value. |
| `parse` | `fn parse(source: String) -> Result<json::Value, errors::Error>` | Parses a YAML document into a Value. |
| `encode` | `fn encode(value: json::Value) -> Result<String, errors::Error>` | Encodes a Value as a YAML document. |
| `parse_all` | `fn parse_all(source: String) -> Result<Vec<json::Value>, errors::Error>` | Parses a multi-document YAML stream into a Vec<Value>. |
| `to_json` | `fn to_json(source: String) -> Result<String, errors::Error>` | Converts a YAML document to JSON text. |
| `from_json` | `fn from_json(source: String) -> Result<String, errors::Error>` | Converts JSON text to a YAML document. |
| `is_valid` | `fn is_valid(source: String) -> bool` | Reports whether the text is well-formed YAML. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
