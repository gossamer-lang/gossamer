//! The text a program reads for a canonicalized path.

/// `path` as the program's path functions read it. On Windows,
/// `std::fs::canonicalize` answers a verbatim path (`\\?\C:\dir`), after
/// which `/` is no separator; the drive form (`C:\dir`) and the UNC form
/// (`\\host\share`) name the same file and join with `/` like any other
/// path. A path longer than the drive form can address keeps its prefix.
#[must_use]
pub fn display(path: &std::path::Path) -> String {
    plain(&path.to_string_lossy(), cfg!(windows))
}

// MAX_PATH, the longest path the drive and UNC forms address.
const PLAIN_LIMIT: usize = 260;

fn plain(text: &str, windows: bool) -> String {
    if !windows {
        return text.to_string();
    }
    let candidate = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\")
        && let [drive, b':', b'\\', ..] = rest.as_bytes()
        && drive.is_ascii_alphabetic()
    {
        rest.to_string()
    } else {
        return text.to_string();
    };
    if candidate.len() < PLAIN_LIMIT {
        candidate
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn a_verbatim_drive_path_reads_as_its_drive_form() {
        assert_eq!(plain(r"\\?\D:\a\gui-gos", true), r"D:\a\gui-gos");
    }

    #[test]
    fn a_verbatim_unc_path_reads_as_its_share_form() {
        assert_eq!(plain(r"\\?\UNC\host\share\dir", true), r"\\host\share\dir");
    }

    #[test]
    fn a_path_the_drive_form_cannot_address_keeps_its_prefix() {
        let long = format!(r"\\?\C:\{}", "d".repeat(300));
        assert_eq!(plain(&long, true), long);
        let volume = r"\\?\Volume{0b1c}\dir";
        assert_eq!(plain(volume, true), volume);
    }

    #[test]
    fn posix_paths_are_unchanged() {
        assert_eq!(plain("/home/user/../x", false), "/home/user/../x");
        assert_eq!(plain(r"\\?\C:\a", false), r"\\?\C:\a");
    }
}
