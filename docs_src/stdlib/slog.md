# `std::slog`

Status: experimental

Structured, levelled logging.

## Items

| Item | Signature | Description |
|---|---|---|
| `Logger` | `type Logger` | Logger handle. |
| `Field` | `type Field` | Key/value pair threaded through a logger. |
| `TextHandler` | `type TextHandler` | Line-oriented handler. |
| `JsonHandler` | `type JsonHandler` | JSON-lines handler. |
| `info` | `fn info(message: String) -> ()` | Logs a JSON record at INFO level. Trailing args are key/value pairs. |
| `warn` | `fn warn(message: String) -> ()` | Logs a JSON record at WARN level. |
| `error` | `fn error(message: String) -> ()` | Logs a JSON record at ERROR level. |
| `debug` | `fn debug(message: String) -> ()` | Logs a JSON record at DEBUG level. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
