# `std::bufio`

Status: experimental

Buffered readers, writers, and line scanners.

## Items

| Item | Signature | Description |
|---|---|---|
| `Reader` | `type Reader` | Buffered reader. |
| `Writer` | `type Writer` | Buffered writer. |
| `Scanner` | `type Scanner` | Line / token scanner. |
| `read_lines` | `fn read_lines(path: String) -> Result<Vec<String>, io::Error>` | Reads every line from a file path; one-shot convenience over the streaming Scanner. |
| `read_lines_of` | `fn read_lines_of(path: String) -> Result<Vec<String>, io::Error>` | Reads every line of a file path into a Vec<String>. |
| `read_to_string` | `fn read_to_string(path: String) -> Result<String, io::Error>` | Reads an entire file path into a String. |
| `split_whitespace` | `fn split_whitespace(text: String) -> Vec<String>` | Splits a String on runs of whitespace. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
