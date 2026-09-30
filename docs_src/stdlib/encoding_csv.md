# `std::encoding::csv`

Status: experimental

CSV record reader and writer.

## Items

| Item | Signature | Description |
|---|---|---|
| `read` | `fn read(text: String) -> Result<Vec<Vec<String>>, errors::Error>` | Parses all CSV records from a string. |
| `parse_line` | `fn parse_line(line: String) -> Vec<String>` | Parses a single CSV-formatted line. |
| `write` | `fn write(rows: Vec<Vec<String>>) -> String` | Serialises records as a CSV string. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
