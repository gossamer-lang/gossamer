# `std::env`

Process environment, command-line arguments, working directory.

## Items

| Item | Signature | Description |
|---|---|---|
| `args` | `fn args() -> Vec<String>` | Returns the program's command-line arguments. |
| `program_name` | `fn program_name() -> String` | Returns the path used to invoke the program (argv[0]). |
| `var` | `fn var(name: String) -> Option<String>` | Returns the value of an environment variable. |
| `set_var` | `fn set_var(name: String, value: String) -> ()` | Sets an environment variable in the current process. |
| `unset_var` | `fn unset_var(name: String) -> ()` | Removes an environment variable from the current process. |
| `current_dir` | `fn current_dir() -> Result<String, io::Error>` | Returns the current working directory. |
| `set_current_dir` | `fn set_current_dir(path: String) -> Result<(), io::Error>` | Changes the current working directory. |
| `home_dir` | `fn home_dir() -> Option<String>` | Returns the calling user's home directory if known. |
| `temp_dir` | `fn temp_dir() -> String` | Returns the system's temporary directory. |
| `vars` | `fn vars() -> Map<String, String>` | vars() -> Map<String, String>. Every environment variable this process has, as a snapshot. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
