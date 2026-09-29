# `std::encoding::xml`

Status: experimental

XML parse and encode (quick-xml) over a `json::Value` tree.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`Event`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `type Event` | Start / End / Text / CData / Comment / Eof. |
| [`Reader`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `type Reader` | Pull-style XML reader. |
| [`Writer`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `type Writer` | Streaming XML writer. |
| [`encode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `fn encode(value: json::Value) -> String` | Serialises a sequence of events to XML text. |
| [`escape`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `fn escape(text: String) -> String` | Escapes XML metacharacters in text. |
| [`parse`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/encoding/xml.rs) | `fn parse(source: String) -> Result<json::Value, errors::Error>` | Parses an XML document into a Vec of events. |
