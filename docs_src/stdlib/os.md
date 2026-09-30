# `std::os`

Status: experimental

Operating-system identity.

## Items

| Item | Signature | Description |
|---|---|---|
| `family` | `fn family() -> String` | Returns "unix" or "windows" for the running OS family. |
| `arch` | `fn arch() -> String` | Returns the target CPU architecture (e.g. "x86_64"). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
