# `std::http::multipart`

Status: experimental

RFC 7578 multipart/form-data streaming parser.

## Items

| Item | Signature | Description |
|---|---|---|
| `Config` | `type Config` | Per-form size, part-count, and disk-spill limits. |
| `Part` | `type Part` | One field or file entry from a multipart body. |
| `PartData` | `type PartData` | In-memory bytes or spilled-to-disk path for a part. |
| `Form` | `type Form` | Parsed multipart envelope: fields + file parts. |
| `parse` | `fn parse(request: http::Request) -> Result<http::multipart::Form, errors::Error>` | Stream-parse from any Read source into a Form. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
