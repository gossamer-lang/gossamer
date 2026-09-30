# `std::path`

Lexical filesystem-path operations; platform path grammar, no URL parsing.

## Items

| Item | Signature | Description |
|---|---|---|
| `join` | `fn join(base: String, segment: String) -> String` | Joins two path fragments. |
| `walk` | `fn walk(path: String, visit: Fn(fs::DirInfo) -> Result<(), io::Error>) -> Result<(), io::Error>` | Recursively visits every descendant entry under a directory, the path-module spelling of fs::walk_dir. |
| `components` | `fn components(path: String) -> Vec<String>` | Returns Rust-like lexical path components. |
| `prefixes` | `fn prefixes(path: String) -> Vec<String>` | Returns cumulative Rust-like lexical path prefixes. |
| `unique_prefixes` | `fn unique_prefixes(text: String) -> Vec<String>` | Returns sorted unique prefixes for newline-delimited paths. |
| `split` | `fn split(path: String) -> (String, String)` | Returns (dir, file) for the supplied path. |
| `parent` | `fn parent(path: String) -> Option<String>` | Parent directory, or None at the root. |
| `file_name` | `fn file_name(path: String) -> Option<String>` | Final path component, or None. |
| `file_stem` | `fn file_stem(path: String) -> Option<String>` | File name without its extension. |
| `extension` | `fn extension(path: String) -> Option<String>` | Dotted extension as an Option. |
| `is_absolute` | `fn is_absolute(path: String) -> bool` | Reports whether the path is absolute. |
| `normalize` | `fn normalize(path: String) -> String` | Lexically normalizes the path. |
| `starts_with` | `fn starts_with(path: String, prefix: String) -> bool` | Reports whether the path begins with a prefix component-wise. |
| `matches` | `fn matches(pattern: String, name: String) -> bool` | `matches(pattern, name) -> bool` - Go `filepath.Match` shell-glob test over a single path segment: `*` and `?` never cross a `/`, `[abc]` is a character class. Spelled `matches` because `match` is a keyword. Example: `path::matches("*.gos", "main.gos")` is true, `path::matches("a*c", "a/c")` is false. |
| `glob` | `fn glob(pattern: String) -> Result<Vec<String>, errors::Error>` | `glob(pattern) -> Result<Vec<String>, errors::Error>` - filesystem paths matching a shell glob, sorted so every tier reports the same order. Supports `*`, `?`, `[abc]`, and `**` (this directory and every descendant). Relative patterns resolve against the working directory. Example: `let found = path::glob("src/**/*.gos")?`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
