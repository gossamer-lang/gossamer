//! The lexical path grammar every tier and every platform shares.
//!
//! `/` and `\` both separate components, and a path is absolute when it
//! begins with a root: a separator (`/`), a drive (`C:/` or `C:\`), or a UNC
//! share (`//host/share` or `\\host\share`). Results are written with `/`,
//! which every supported platform accepts.

/// `true` when `c` separates path components.
#[must_use]
pub const fn is_separator(c: char) -> bool {
    c == '/' || c == '\\'
}

fn is_separator_byte(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// Byte length of the root `path` begins with, including the separator
/// that ends it; 0 for a relative path.
#[must_use]
pub fn root_len(path: &str) -> usize {
    let b = path.as_bytes();
    let sep = |i: usize| b.get(i).copied().is_some_and(is_separator_byte);
    if sep(0) && sep(1) && b.get(2).is_some_and(|&c| !is_separator_byte(c)) {
        let next = |from: usize| {
            b[from..]
                .iter()
                .position(|&c| is_separator_byte(c))
                .map_or(b.len(), |i| from + i)
        };
        let host_end = next(2);
        if host_end == b.len() {
            return b.len();
        }
        let share_end = next(host_end + 1);
        return if share_end == b.len() {
            b.len()
        } else {
            share_end + 1
        };
    }
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(2) {
        return 3;
    }
    usize::from(sep(0))
}

// The root of `path` written with `/`.
fn root_text(path: &str) -> String {
    path[..root_len(path)].replace('\\', "/")
}

/// `true` when `path` begins with a root.
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    root_len(path) > 0
}

/// Joins `base` with `segment` by one `/`; an absolute `segment` replaces
/// `base`.
#[must_use]
pub fn join(base: &str, segment: &str) -> String {
    if base.is_empty() || is_absolute(segment) {
        return segment.to_string();
    }
    let root = root_len(base);
    let mut out = base[..root].to_string();
    out.push_str(base[root..].trim_end_matches(is_separator));
    if !out.ends_with(is_separator) {
        out.push('/');
    }
    out.push_str(segment.trim_start_matches(is_separator));
    out
}

/// Splits `path` into `(directory, file)` at its last separator. The
/// directory keeps the root whole and carries no other trailing separator.
#[must_use]
pub fn split(path: &str) -> (&str, &str) {
    let root = root_len(path);
    match path[root..].rfind(is_separator) {
        None => (&path[..root], &path[root..]),
        Some(i) => (&path[..root + i], &path[root + i + 1..]),
    }
}

/// The final component of `path`; empty for a root or a trailing separator.
#[must_use]
pub fn base(path: &str) -> &str {
    split(path).1
}

/// The directory of `path`, or `"."` when it names no directory.
#[must_use]
pub fn dir(path: &str) -> &str {
    let (d, _) = split(path);
    if d.is_empty() { "." } else { d }
}

/// The extension of the final component, dot included, or empty when it
/// has none; a leading dot names a hidden file, not an extension.
#[must_use]
pub fn ext(path: &str) -> &str {
    let name = base(path);
    match name.rfind('.') {
        None | Some(0) => "",
        Some(i) => &name[i..],
    }
}

/// The final component without its extension, or `None` without a final
/// component.
#[must_use]
pub fn stem(path: &str) -> Option<&str> {
    let name = base(path);
    if name.is_empty() {
        return None;
    }
    Some(match name.rfind('.') {
        None | Some(0) => name,
        Some(i) => &name[..i],
    })
}

/// The directory `path` is in, or `None` for a root, an empty path, or a
/// single relative component. A trailing separator is ignored.
#[must_use]
pub fn parent(path: &str) -> Option<&str> {
    let root = root_len(path);
    let rest = path[root..].trim_end_matches(is_separator);
    if rest.is_empty() {
        return None;
    }
    match rest.rfind(is_separator) {
        None if root > 0 => Some(&path[..root]),
        None => None,
        Some(i) => Some(&path[..root + i]),
    }
}

/// Rust-like lexical components: the root is its own component, repeated
/// separators and non-leading `.` are skipped, and `..` is kept.
#[must_use]
pub fn components(path: &str) -> Vec<String> {
    let root = root_len(path);
    let mut out = Vec::new();
    if root > 0 {
        out.push(root_text(path));
    }
    let mut saw_normal = root > 0;
    for segment in path[root..].split(is_separator) {
        match segment {
            "" => {}
            "." if !saw_normal => {
                out.push(".".to_string());
                saw_normal = true;
            }
            "." => {}
            other => {
                out.push(other.to_string());
                saw_normal = true;
            }
        }
    }
    out
}

/// Every cumulative prefix of `path`, by the rules of [`components`].
#[must_use]
pub fn prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    extend_prefixes(path, &mut out);
    out
}

/// Sorted unique cumulative prefixes of newline-delimited paths.
#[must_use]
pub fn unique_prefixes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        extend_prefixes(line, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn extend_prefixes(path: &str, out: &mut Vec<String>) {
    let mut prefix = String::with_capacity(path.len());
    let mut saw_normal = false;
    for component in components(path) {
        if component == "." && !saw_normal {
            prefix.push('.');
        } else if prefix.is_empty() || prefix.ends_with('/') {
            prefix.push_str(&component);
        } else {
            prefix.push('/');
            prefix.push_str(&component);
        }
        saw_normal = true;
        out.push(prefix.clone());
    }
}

/// Collapses `.`, `..` and repeated separators, as Go's `filepath.Clean`
/// does; `..` never climbs above a root.
#[must_use]
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let root = root_len(path);
    let mut parts: Vec<&str> = Vec::new();
    for segment in path[root..].split(is_separator) {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|s| *s != "..") {
                    parts.pop();
                } else if root == 0 {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = root_text(path);
    out.push_str(&parts.join("/"));
    if out.is_empty() { ".".to_string() } else { out }
}

/// `true` when `path` is `prefix` or lies inside it, compared cleaned.
#[must_use]
pub fn has_prefix(path: &str, prefix: &str) -> bool {
    let path = clean(path);
    let prefix = clean(prefix);
    if path == prefix {
        return true;
    }
    if prefix.ends_with('/') {
        path.starts_with(&prefix)
    } else {
        path.strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_are_separators_drives_and_shares_on_every_platform() {
        assert_eq!(root_len("/a"), 1);
        assert_eq!(root_len(r"\a"), 1);
        assert_eq!(root_len(r"C:\a"), 3);
        assert_eq!(root_len("c:/a"), 3);
        assert_eq!(root_len(r"\\host\share\a"), r"\\host\share\".len());
        assert_eq!(root_len("//host/share"), "//host/share".len());
        assert_eq!(root_len("a/b"), 0);
        assert_eq!(root_len("C:a"), 0);
        assert!(is_absolute(r"D:\a\gui-gos"));
        assert!(!is_absolute("examples/hello-world"));
    }

    #[test]
    fn joining_a_drive_path_gives_a_path_the_platform_opens() {
        assert_eq!(
            join(r"D:\a\gui-gos\examples\hello-world", "packaging.toml"),
            r"D:\a\gui-gos\examples\hello-world/packaging.toml"
        );
        assert_eq!(join(r"C:\", "x"), r"C:\x");
        assert_eq!(join("/", "x"), "/x");
        assert_eq!(join("a/", "/abs"), "/abs");
        assert_eq!(join("a", r"C:\abs"), r"C:\abs");
        assert_eq!(join("", "x"), "x");
    }

    #[test]
    fn parent_split_and_names_keep_the_root_whole() {
        assert_eq!(parent(r"C:\a"), Some(r"C:\"));
        assert_eq!(parent(r"C:\"), None);
        assert_eq!(parent("/a/b/"), Some("/a"));
        assert_eq!(parent("/a"), Some("/"));
        assert_eq!(parent("a"), None);
        assert_eq!(parent(r"\\host\share\x"), Some(r"\\host\share\"));
        assert_eq!(split(r"a\b\c.txt"), (r"a\b", "c.txt"));
        assert_eq!(split("/x"), ("/", "x"));
        assert_eq!(base(r"C:\dir\file.tar.gz"), "file.tar.gz");
        assert_eq!(ext(r"C:\dir\file.tar.gz"), ".gz");
        assert_eq!(stem(".config"), Some(".config"));
        assert_eq!(dir("file"), ".");
    }

    #[test]
    fn clean_components_and_prefixes_write_one_separator() {
        assert_eq!(clean(r"C:\a\..\b\.\c"), "C:/b/c");
        assert_eq!(clean(r"\\host\share\a\..\b"), "//host/share/b");
        assert_eq!(clean("/../a//b/"), "/a/b");
        assert_eq!(clean("../a/../.."), "../..");
        assert_eq!(clean(""), ".");
        assert_eq!(components(r"C:\a\b"), vec!["C:/", "a", "b"]);
        assert_eq!(components("./a/./b"), vec![".", "a", "b"]);
        assert_eq!(prefixes(r"C:\a\b"), vec!["C:/", "C:/a", "C:/a/b"]);
        assert_eq!(prefixes("a/b"), vec!["a", "a/b"]);
        assert_eq!(prefixes("./a"), vec![".", "./a"]);
        assert_eq!(prefixes("/a"), vec!["/", "/a"]);
        assert!(has_prefix(r"C:\a\b", "C:/a"));
        assert!(!has_prefix("C:/ab", "C:/a"));
    }
}
