//! Runtime support for `std::path` - OS-neutral path manipulation.
//! All helpers operate on `/`-delimited posix-style paths. The
//! [`native`] submodule wraps the posix forms for native separators
//! (backslash on Windows, forward-slash elsewhere); prefer the
//! semantic layer here for paths that live inside the program and
//! [`native`] only at the boundary where a path crosses into an OS
//! call.

#![forbid(unsafe_code)]
#![allow(clippy::manual_let_else)]
#![allow(missing_docs)]

/// Immutable UTF-8 lexical path value.
///
/// This type never touches the filesystem. Operations return new values and
/// use the same normalized `/` grammar as the free functions in this module.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path(String);

impl Path {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn join(&self, segment: &str) -> Self {
        Self(join(&self.0, segment))
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        parent(&self.0).map(Self)
    }

    #[must_use]
    pub fn file_name(&self) -> Option<String> {
        file_name(&self.0)
    }

    #[must_use]
    pub fn stem(&self) -> Option<String> {
        stem(&self.0)
    }

    #[must_use]
    pub fn extension(&self) -> Option<String> {
        let value = ext(&self.0);
        (!value.is_empty()).then_some(value)
    }

    #[must_use]
    pub fn normalize(&self) -> Self {
        Self(clean(&self.0))
    }

    #[must_use]
    pub fn is_absolute(&self) -> bool {
        is_absolute(&self.0)
    }

    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        has_prefix(&self.0, &prefix.0)
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Path {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for Path {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

use gossamer_runtime::path_grammar as grammar;

/// Joins `base` with `segment` by one `/`; an absolute `segment` (a
/// separator, drive or UNC root) replaces `base`.
#[must_use]
pub fn join(base: &str, segment: &str) -> String {
    grammar::join(base, segment)
}

/// Splits `path` into a `(directory, file)` pair. The directory keeps a
/// root whole and carries no other trailing separator.
#[must_use]
pub fn split(path: &str) -> (String, String) {
    let (dir, file) = grammar::split(path);
    (dir.to_string(), file.to_string())
}

/// Returns Rust-like lexical path components.
///
/// The root (`/`, `C:/`, `//host/share/`) is its own component, repeated
/// separators and non-leading `.` components are skipped, and `..` is
/// retained instead of normalizing across parent directories.
#[must_use]
pub fn components(path: &str) -> Vec<String> {
    grammar::components(path)
}

/// Returns every cumulative Rust-like lexical path prefix, by the rules of
/// [`components`]: `["a", "a/b", "a/b/c"]` for `a/b/c`.
#[must_use]
pub fn prefixes(path: &str) -> Vec<String> {
    grammar::prefixes(path)
}

/// Returns sorted unique cumulative prefixes for newline-delimited paths.
#[must_use]
pub fn unique_prefixes(text: &str) -> Vec<String> {
    grammar::unique_prefixes(text)
}

/// Returns the final component of `path` (the file name).
#[must_use]
pub fn base(path: &str) -> String {
    grammar::base(path).to_string()
}

/// Returns the directory portion of `path`.
#[must_use]
pub fn dir(path: &str) -> String {
    grammar::dir(path).to_string()
}

/// Returns the extension (including the leading `.`) of `path`, or
/// an empty string when none is present.
#[must_use]
pub fn ext(path: &str) -> String {
    grammar::ext(path).to_string()
}

/// Cleans `path` in the same sense as Go's `filepath.Clean`:
/// collapses `..` and `.`, strips duplicate separators, preserves the
/// root, and writes `/`.
#[must_use]
pub fn clean(path: &str) -> String {
    grammar::clean(path)
}

/// Returns `true` when `path` begins with a root: a separator, a drive
/// (`C:\`), or a UNC share (`\\host\share`).
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    grammar::is_absolute(path)
}

/// Returns the parent directory of `path`, or `None` when `path`
/// has no parent directory (a root, the empty string, or a bare single
/// component such as `"file.txt"`). A trailing separator is ignored, so
/// `parent("dir/")` is the same as `parent("dir")`.
#[must_use]
pub fn parent(path: &str) -> Option<String> {
    grammar::parent(path).map(str::to_string)
}

/// Returns the final component of `path`, or `None` when there is
/// none (the empty string, a root, or a trailing-separator directory
/// such as `"dir/"`).
#[must_use]
pub fn file_name(path: &str) -> Option<String> {
    let name = grammar::base(path);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Returns the final component of `path` without its extension, or
/// `None` when there is no final component. A leading dot (a hidden
/// file such as `".config"`) is not treated as an extension.
#[must_use]
pub fn stem(path: &str) -> Option<String> {
    grammar::stem(path).map(str::to_string)
}

/// Returns `true` when `path` references a file inside `prefix`.
#[must_use]
pub fn has_prefix(path: &str, prefix: &str) -> bool {
    grammar::has_prefix(path, prefix)
}

pub mod native {
    //! Native-separator wrappers around the posix helpers.
    //!
    //! Convert paths at the OS boundary: read the path back out of
    //! the program in posix form, hand a posix form to the helpers
    //! here, pass the returned native form to system calls. Within
    //! the program, stick to posix - it avoids a combinatorial
    //! explosion of separator conversions.
    //!
    //! On Windows the native separator is `\`; everywhere else it
    //! is `/`, and the helpers are near-identity.
    use super::{clean as posix_clean, join as posix_join};

    /// The platform's preferred path separator character.
    #[cfg(windows)]
    pub const SEPARATOR: char = '\\';
    /// The platform's preferred path separator character.
    #[cfg(not(windows))]
    pub const SEPARATOR: char = '/';

    /// Joins two path components using the platform-native
    /// separator. Input components may use either `/` or `\`; output
    /// uses exclusively the native separator.
    #[must_use]
    pub fn join(base: &str, segment: &str) -> String {
        let posix_base = to_posix(base);
        let posix_segment = to_posix(segment);
        to_native(&posix_join(&posix_base, &posix_segment))
    }

    /// Canonicalises `path` into native-separator form with `..` /
    /// `.` collapsed as by [`super::clean`].
    #[must_use]
    pub fn clean(path: &str) -> String {
        to_native(&posix_clean(&to_posix(path)))
    }

    /// Rewrites a path that may use `\` into posix form, so the
    /// semantic-layer helpers can be used on Windows input.
    #[must_use]
    pub fn to_posix(path: &str) -> String {
        if SEPARATOR == '/' {
            return path.to_string();
        }
        path.replace('\\', "/")
    }

    /// Rewrites a posix-form path into native-separator form.
    #[must_use]
    pub fn to_native(path: &str) -> String {
        if SEPARATOR == '/' {
            return path.to_string();
        }
        path.replace('/', "\\")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[cfg(not(windows))]
        fn to_posix_round_trips_identity_on_non_windows() {
            // On Unix `to_posix` and `to_native` are both no-ops, so
            // a posix-form input survives the round-trip unchanged.
            // On Windows `to_native` replaces `/` with `\`, breaking
            // identity for an already-posix input - the assertion is
            // genuinely unix-only, hence the `#[cfg(not(windows))]`.
            let original = "a/b/c";
            assert_eq!(to_native(&to_posix(original)), original);
        }

        #[test]
        #[cfg(windows)]
        fn to_posix_strips_backslashes_on_windows() {
            // Windows half of the round-trip contract: a native-form
            // input survives a full conversion through both helpers.
            let native = "a\\b\\c";
            assert_eq!(to_native(&to_posix(native)), native);
        }

        #[test]
        fn join_uses_native_separator() {
            let joined = join("a", "b");
            assert!(joined.contains(SEPARATOR));
            assert_eq!(joined.matches(SEPARATOR).count(), 1);
        }

        #[test]
        fn clean_collapses_through_native_layer() {
            let cleaned = clean("a/b/../c");
            let expected = if SEPARATOR == '/' { "a/c" } else { "a\\c" };
            assert_eq!(cleaned, expected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_basic_cases() {
        assert_eq!(join("a", "b"), "a/b");
        assert_eq!(join("a/", "b"), "a/b");
        assert_eq!(join("a", "/b"), "/b");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("a", ""), "a/");
    }

    #[test]
    fn components_follow_rust_like_lexical_rules() {
        assert_eq!(components(""), Vec::<String>::new());
        assert_eq!(components("/"), vec!["/"]);
        assert_eq!(components("/a//b/."), vec!["/", "a", "b"]);
        assert_eq!(components("a/./b//"), vec!["a", "b"]);
        assert_eq!(components("./a/../b"), vec![".", "a", "..", "b"]);
        assert_eq!(components("../a"), vec!["..", "a"]);
    }

    #[test]
    fn prefixes_follow_component_semantics() {
        assert_eq!(prefixes(""), Vec::<String>::new());
        assert_eq!(prefixes("/"), vec!["/"]);
        assert_eq!(prefixes("/a//b/."), vec!["/", "/a", "/a/b"]);
        assert_eq!(prefixes("a/./b//"), vec!["a", "a/b"]);
        assert_eq!(prefixes("./a/../b"), vec![".", "./a", "./a/..", "./a/../b"]);
        assert_eq!(prefixes("../a"), vec!["..", "../a"]);
    }

    #[test]
    fn unique_prefixes_sorts_and_dedups_many_paths() {
        assert_eq!(
            unique_prefixes("a/b\na/c\n/a//b/.\n\n"),
            vec![
                "/".to_string(),
                "/a".to_string(),
                "/a/b".to_string(),
                "a".to_string(),
                "a/b".to_string(),
                "a/c".to_string(),
            ]
        );
    }

    #[test]
    fn split_separates_dir_and_file() {
        assert_eq!(split("a/b/c"), ("a/b".to_string(), "c".to_string()));
        assert_eq!(split("/a"), ("/".to_string(), "a".to_string()));
        assert_eq!(split("a"), (String::new(), "a".to_string()));
    }

    #[test]
    fn dir_returns_dot_when_no_separator() {
        assert_eq!(dir("file"), ".");
        assert_eq!(dir("a/file"), "a");
        assert_eq!(dir("/root/x"), "/root");
    }

    #[test]
    fn ext_returns_final_dot_segment() {
        assert_eq!(ext("a/b.gos"), ".gos");
        assert_eq!(ext("a/b.tar.gz"), ".gz");
        assert_eq!(ext("a/file"), "");
        assert_eq!(ext(".hidden"), "");
    }

    #[test]
    fn clean_collapses_double_slash_and_dots() {
        assert_eq!(clean("a//b/./c"), "a/b/c");
        assert_eq!(clean("a/b/../c"), "a/c");
        assert_eq!(clean("/a/b/../../c"), "/c");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("."), ".");
        assert_eq!(clean("../x"), "../x");
    }

    #[test]
    fn has_prefix_is_path_aware() {
        assert!(has_prefix("a/b/c", "a/b"));
        assert!(has_prefix("a/b", "a/b"));
        assert!(!has_prefix("a/bc", "a/b"));
    }

    #[test]
    fn parent_returns_none_at_root_and_for_bare_names() {
        assert_eq!(parent("a/b/c"), Some("a/b".to_string()));
        assert_eq!(parent("/foo"), Some("/".to_string()));
        assert_eq!(parent("dir/"), None);
        assert_eq!(parent("file.txt"), None);
        assert_eq!(parent("/"), None);
        assert_eq!(parent(""), None);
    }

    #[test]
    fn file_name_returns_final_component() {
        assert_eq!(file_name("a/b/c"), Some("c".to_string()));
        assert_eq!(file_name("file.txt"), Some("file.txt".to_string()));
        assert_eq!(file_name("dir/"), None);
        assert_eq!(file_name("/"), None);
        assert_eq!(file_name(""), None);
    }

    #[test]
    fn stem_drops_extension_but_not_leading_dot() {
        assert_eq!(stem("a/b.tar.gz"), Some("b.tar".to_string()));
        assert_eq!(stem("file.txt"), Some("file".to_string()));
        assert_eq!(stem(".hidden"), Some(".hidden".to_string()));
        assert_eq!(stem("noext"), Some("noext".to_string()));
        assert_eq!(stem("dir/"), None);
    }
}

// --- Pattern matching + walk (Go's path/filepath) ---------------------

/// Sentinel returned by a [`walk`] visitor to skip the current
/// directory subtree.
pub const SKIP_DIR: &str = "__SKIP_DIR__";
/// Sentinel returned by a [`walk`] visitor to skip every
/// remaining entry.
pub const SKIP_ALL: &str = "__SKIP_ALL__";

/// `filepath.Match` semantics: tests whether `name` matches the
/// shell-glob `pattern`. Single-segment matching only - `/`
/// inside `name` does NOT match `*`.
///
/// Pattern operators:
///
/// - `*` - matches any run of characters except `/`.
/// - `?` - matches any single character except `/`.
/// - `[abc]` - character class (no negation, no ranges).
/// - any other byte - literal.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    matches_inner(pattern.as_bytes(), name.as_bytes())
}

fn matches_inner(pat: &[u8], name: &[u8]) -> bool {
    let mut pi = 0;
    let mut ni = 0;
    let mut star_pat: Option<usize> = None;
    let mut star_name: usize = 0;
    while ni < name.len() {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star_pat = Some(pi);
                    star_name = ni;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    if name[ni] == b'/' {
                        return false;
                    }
                    pi += 1;
                    ni += 1;
                    continue;
                }
                b'[' => {
                    let close = match pat[pi + 1..].iter().position(|&b| b == b']') {
                        Some(p) => pi + 1 + p,
                        None => return false,
                    };
                    let class = &pat[pi + 1..close];
                    if class.contains(&name[ni]) {
                        pi = close + 1;
                        ni += 1;
                        continue;
                    }
                    // Fall through to backtrack.
                }
                lit if lit == name[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pat
            && name[star_name] != b'/'
        {
            pi = sp + 1;
            star_name += 1;
            ni = star_name;
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Walks `root` recursively. The visitor is invoked once per
/// entry (directories first, then their contents). Return
/// [`SKIP_DIR`] to skip the current directory's contents,
/// [`SKIP_ALL`] to stop the walk entirely. Any other error is
/// propagated.
///
/// Symlinks are NOT followed by default (matches Go's
/// `filepath.Walk` behaviour).
pub fn walk<F>(root: impl AsRef<std::path::Path>, mut visit: F) -> std::io::Result<()>
where
    F: FnMut(&std::path::Path, &std::fs::Metadata) -> std::io::Result<()>,
{
    let root = root.as_ref();
    let meta = std::fs::symlink_metadata(root)?;
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    match visit(root, &meta) {
        Ok(()) => {}
        Err(e) if e.to_string().contains(SKIP_ALL) => return Ok(()),
        Err(e) if e.to_string().contains(SKIP_DIR) => return Ok(()),
        Err(e) => return Err(e),
    }
    if meta.is_dir() {
        stack.push(root.to_path_buf());
    }
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut children: Vec<std::path::PathBuf> = Vec::new();
        for entry in entries.flatten() {
            children.push(entry.path());
        }
        children.sort();
        let mut skip_remaining = false;
        for child in children {
            if skip_remaining {
                break;
            }
            let meta = match std::fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(_) => continue,
            };
            match visit(&child, &meta) {
                Ok(()) => {
                    if meta.is_dir() {
                        stack.push(child);
                    }
                }
                Err(e) if e.to_string().contains(SKIP_ALL) => return Ok(()),
                Err(e) if e.to_string().contains(SKIP_DIR) => {
                    skip_remaining = false; // SKIP_DIR only skips THIS entry's contents
                    // We achieve that by not pushing the dir
                    // onto the stack - which is the same effect.
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Returns paths matching the glob `pattern`. Glob handles `*`,
/// `?`, `[abc]`, and `**` (recursive directory match). The
/// pattern is rooted at the current working directory unless
/// it begins with `/`.
///
/// On Windows, `\` is accepted as a path separator in the
/// pattern interchangeably with `/`, and drive-letter
/// (`C:\...`) or UNC (`\\server\share\...`) prefixes mark the
/// pattern as absolute. The function is the boundary at which
/// native-form paths cross into the otherwise-posix path
/// helpers - callers do not need to pre-convert.
pub fn glob(pattern: &str) -> std::io::Result<Vec<String>> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let normalised = normalise_glob_separators(pattern);
    let segments: Vec<&str> = normalised.split('/').collect();

    // Everything up to the first segment containing a glob
    // metacharacter is a literal prefix. Constructing the
    // starting `PathBuf` from that prefix in one step lets the
    // std::path machinery handle drive letters, UNC shares, and
    // POSIX roots natively - far more reliable than walking from
    // a synthetic root one `read_dir` at a time.
    let split_idx = segments
        .iter()
        .position(|s| s.contains('*') || s.contains('?') || s.contains('['))
        .unwrap_or(segments.len());

    let base = build_glob_base(&segments[..split_idx]);
    let glob_segments: Vec<&str> = segments[split_idx..]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect();

    // Wholly literal pattern - match if the path exists.
    if glob_segments.is_empty() {
        return Ok(match base.to_str() {
            Some(s) if base.exists() => vec![s.to_string()],
            _ => Vec::new(),
        });
    }

    let mut frontier: Vec<std::path::PathBuf> = vec![base];
    for seg in &glob_segments {
        let mut next: Vec<std::path::PathBuf> = Vec::new();
        for current in &frontier {
            if *seg == "**" {
                // Recursive descent: include `current` itself plus
                // every subdirectory.
                let mut bfs: Vec<std::path::PathBuf> = vec![current.clone()];
                while let Some(p) = bfs.pop() {
                    next.push(p.clone());
                    let entries = std::fs::read_dir(&p)?;
                    for entry in entries {
                        let entry = entry?;
                        let path = entry.path();
                        let metadata = std::fs::symlink_metadata(&path)?;
                        if metadata.is_dir() && !metadata.file_type().is_symlink() {
                            bfs.push(path);
                        }
                    }
                }
                continue;
            }
            let entries = std::fs::read_dir(current)?;
            for entry in entries {
                let path = entry?.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if matches(seg, name) {
                    next.push(path);
                }
            }
        }
        frontier = next;
    }
    let mut out: Vec<String> = frontier
        .into_iter()
        .filter_map(|p| p.to_str().map(str::to_string))
        .collect();
    out.sort();
    Ok(out)
}

/// Replace `\` with `/` on Windows so the glob splitter sees a
/// single canonical separator. No-op elsewhere - `\` is a valid
/// filename character on Unix and must stay literal.
fn normalise_glob_separators(pattern: &str) -> String {
    #[cfg(windows)]
    {
        pattern.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        pattern.to_string()
    }
}

/// Build the starting `PathBuf` from the literal-prefix segments
/// (everything before the first glob metacharacter).
fn build_glob_base(prefix_segments: &[&str]) -> std::path::PathBuf {
    if prefix_segments.is_empty() {
        return std::path::PathBuf::from(".");
    }
    let joined = prefix_segments.join("/");
    if joined.is_empty() {
        // All segments empty - pattern was `/` or similar.
        return std::path::PathBuf::from("/");
    }
    // Lone drive letter "C:" is drive-relative on Windows; append
    // `/` so the path resolves to the actual drive root rather
    // than the drive's current working directory.
    #[cfg(windows)]
    {
        let bytes = joined.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return std::path::PathBuf::from(format!("{joined}/"));
        }
    }
    std::path::PathBuf::from(joined)
}

/// Resolves all symlinks along `path` and returns the canonical
/// absolute path.
pub fn eval_symlinks(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let canonical = std::fs::canonicalize(path)?;
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| std::io::Error::other("non-UTF-8 path"))
}

#[cfg(test)]
mod glob_walk_tests {
    use super::*;

    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let pid = gossamer_runtime::platform::process_id();
            let n = gossamer_runtime::platform::system_time_now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let p =
                gossamer_runtime::platform::temp_dir().join(format!("gos-path-{tag}-{pid}-{n}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn match_literal() {
        assert!(matches("hello", "hello"));
        assert!(!matches("hello", "world"));
    }

    #[test]
    fn match_star_in_single_segment() {
        assert!(matches("*.gos", "hello.gos"));
        assert!(matches("hello*", "hello.gos"));
        assert!(!matches("*.gos", "hello.rs"));
    }

    #[test]
    fn match_question_mark_is_single_char() {
        assert!(matches("?ello", "hello"));
        assert!(!matches("?ello", "yello world"));
    }

    #[test]
    fn match_character_class() {
        assert!(matches("h[aeiou]llo", "hello"));
        assert!(matches("h[aeiou]llo", "hallo"));
        assert!(!matches("h[aeiou]llo", "hxllo"));
    }

    #[test]
    fn star_does_not_cross_slash() {
        assert!(!matches("a*c", "a/c"));
    }

    #[test]
    fn glob_returns_matching_files() {
        let dir = TmpDir::new("glob");
        std::fs::write(dir.path().join("a.gos"), "").unwrap();
        std::fs::write(dir.path().join("b.gos"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let pattern = format!("{}/*.gos", dir.path().display());
        let mut out = glob(&pattern).unwrap();
        out.sort();
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with("a.gos"));
        assert!(out[1].ends_with("b.gos"));
    }

    #[test]
    fn glob_recursive_double_star() {
        let dir = TmpDir::new("glob_rec");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::create_dir(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("a.gos"), "").unwrap();
        std::fs::write(dir.path().join("sub/b.gos"), "").unwrap();
        std::fs::write(dir.path().join("sub/deep/c.gos"), "").unwrap();
        let pattern = format!("{}/**/*.gos", dir.path().display());
        let out = glob(&pattern).unwrap();
        assert_eq!(out.len(), 3, "got {out:?}");
    }

    #[test]
    fn walk_visits_every_entry() {
        let dir = TmpDir::new("walk");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "").unwrap();
        let mut count = 0;
        walk(dir.path(), |_path, _meta| {
            count += 1;
            Ok(())
        })
        .unwrap();
        // root + sub + a.txt + sub/b.txt = 4
        assert_eq!(count, 4);
    }

    #[test]
    fn walk_skip_dir_skips_subtree() {
        let dir = TmpDir::new("walkskip");
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let mut visited: Vec<String> = Vec::new();
        walk(dir.path(), |path, _meta| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            visited.push(name.clone());
            if name == ".git" {
                return Err(std::io::Error::other(SKIP_DIR));
            }
            Ok(())
        })
        .unwrap();
        // .git/HEAD must NOT have been visited
        assert!(!visited.iter().any(|n| n == "HEAD"));
        assert!(visited.iter().any(|n| n == "a.txt"));
    }

    #[test]
    fn eval_symlinks_returns_canonical() {
        let dir = TmpDir::new("evalsym");
        let file = dir.path().join("real.txt");
        std::fs::write(&file, "x").unwrap();
        let resolved = eval_symlinks(&file).unwrap();
        assert!(resolved.ends_with("real.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn glob_accepts_backslash_separators() {
        // A pattern in native Windows form (backslashes, drive
        // prefix) is what callers produce by formatting a
        // `std::path::Path::display()` into a string, so glob must
        // accept it verbatim.
        let dir = TmpDir::new("glob_bs");
        std::fs::write(dir.path().join("a.gos"), "").unwrap();
        std::fs::write(dir.path().join("b.gos"), "").unwrap();
        let pattern = format!("{}\\*.gos", dir.path().display());
        let mut out = glob(&pattern).unwrap();
        out.sort();
        assert_eq!(out.len(), 2, "got {out:?}");
    }

    #[test]
    #[cfg(windows)]
    fn glob_accepts_mixed_separators() {
        // The exact shape produced by `format!("{}/*.gos",
        // dir.display())`: backslashes in the prefix from
        // `display()`, a forward slash inserted by the literal.
        let dir = TmpDir::new("glob_mix");
        std::fs::create_dir(dir.path().join("nest")).unwrap();
        std::fs::write(dir.path().join("nest\\a.gos"), "").unwrap();
        let prefix = dir.path().display().to_string();
        let pattern = format!("{prefix}/nest/*.gos");
        let out = glob(&pattern).unwrap();
        assert_eq!(out.len(), 1, "got {out:?}");
    }

    #[test]
    fn build_glob_base_relative() {
        let base = super::build_glob_base(&["a", "b"]);
        assert_eq!(base, std::path::PathBuf::from("a/b"));
    }

    #[test]
    fn build_glob_base_empty_is_cwd() {
        let base = super::build_glob_base(&[]);
        assert_eq!(base, std::path::PathBuf::from("."));
    }

    #[test]
    fn build_glob_base_posix_absolute() {
        // Leading "" comes from splitting "/etc/foo" on "/".
        let base = super::build_glob_base(&["", "etc", "foo"]);
        assert_eq!(base, std::path::PathBuf::from("/etc/foo"));
    }

    #[test]
    #[cfg(windows)]
    fn build_glob_base_drive_letter_alone_gets_root_slash() {
        let base = super::build_glob_base(&["C:"]);
        assert_eq!(base, std::path::PathBuf::from("C:/"));
    }

    #[test]
    #[cfg(windows)]
    fn build_glob_base_drive_letter_with_subpath() {
        let base = super::build_glob_base(&["C:", "Users", "foo"]);
        assert_eq!(base, std::path::PathBuf::from("C:/Users/foo"));
    }
}

#[cfg(test)]
mod separator_grammar_tests {
    use super::{is_absolute, join, split};

    #[test]
    fn paths_split_on_either_separator_on_every_platform() {
        assert_eq!(
            split("C:\\tmp\\gos-glob-1\\alpha.gos"),
            ("C:\\tmp\\gos-glob-1".to_string(), "alpha.gos".to_string())
        );
        // A path that mixes both forms - what an OS call joined with `/`
        // hands back - splits at the last component either way.
        assert_eq!(
            split("C:\\tmp/gos-glob-1\\alpha.gos"),
            ("C:\\tmp/gos-glob-1".to_string(), "alpha.gos".to_string())
        );
        assert_eq!(
            split("/tmp/odd\\name.gos"),
            ("/tmp/odd".to_string(), "name.gos".to_string())
        );
    }

    #[test]
    fn a_drive_path_is_absolute_and_replaces_a_base() {
        assert!(is_absolute("D:\\a\\gui-gos"));
        assert_eq!(join("examples", "D:\\a"), "D:\\a");
        assert_eq!(
            join("D:\\a\\examples\\hello-world", "packaging.toml"),
            "D:\\a\\examples\\hello-world/packaging.toml"
        );
    }
}
