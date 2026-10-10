# `std::archive`

Status: experimental

Helpers shared by the archive formats.

## Items

| Item | Signature | Description |
|---|---|---|
| `enclosed_path` | `fn enclosed_path(name: String) -> Option<String>` | `enclosed_path(name) -> Option<String>`: an entry name as a relative path that stays inside the directory it is extracted to, `.` and inner `..` steps resolved, or `None` for an absolute path, a drive prefix, or a path that leaves the directory. Check every name before writing an entry by it. |
