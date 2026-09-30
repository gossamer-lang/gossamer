# `std::os::user`

Status: experimental

POSIX user / group lookup. Unix-backed by `nix`; Windows falls back to env vars.

## Items

| Item | Signature | Description |
|---|---|---|
| `current_name` | `fn current_name() -> String` | Login name of the current process user, or empty string. |
| `current_uid` | `fn current_uid() -> i64` | uid of the current process user, or -1 on non-unix. |
| `current_gid` | `fn current_gid() -> i64` | gid of the current process user, or -1 on non-unix. |
| `current_home` | `fn current_home() -> String` | Home directory of the current process user. |
| `lookup_uid` | `fn lookup_uid(uid: i64) -> String` | Login name for the given uid, or empty string if unknown. |
| `lookup_name` | `fn lookup_name(name: String) -> i64` | uid for the user with the given login name, or -1 if not found. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
