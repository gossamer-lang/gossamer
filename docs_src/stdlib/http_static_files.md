# `std::http::static_files`

Status: experimental

Caching static-file handler: ETag, Last-Modified, byte ranges, MIME sniff.

## Items

| Item | Signature | Description |
|---|---|---|
| `FileServer` | `type FileServer` | Static-file handler rooted at a directory (Rust-side; streaming). |
| `serve_file` | `fn serve_file(path: String) -> Result<http::Response, errors::Error>` | Read a single file and return it as a Response struct. Interp tier. |
| `mime_for_path` | `fn mime_for_path(path: String) -> String` | Guess a MIME type from a file path's extension. Available in interp + compiled. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
