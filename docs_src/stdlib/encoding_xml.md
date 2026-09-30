# `std::encoding::xml`

Status: experimental

XML parse and encode (quick-xml) over a `json::Value` tree.

## Items

| Item | Signature | Description |
|---|---|---|
| `parse` | `fn parse(source: String) -> Result<json::Value, errors::Error>` | Parses an XML document into a `json::Value` tree: an element is `{"__xml_type": "element", "name", "attrs", "children"}`, a text node `{"__xml_type": "text", "value"}`. |
| `encode` | `fn encode(value: json::Value) -> String` | Serialises a tree of the shape `parse` answers to XML text; any other value encodes as the empty string. |
| `escape` | `fn escape(text: String) -> String` | Escapes XML metacharacters in text. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
