//! ANSI styling for diagnostic output. Detects whether stderr is
//! a TTY at first use and disables colour for piped/redirected
//! output. Honours `NO_COLOR` (any value), `CLICOLOR=0`, and `CLICOLOR_FORCE`.

use std::io::IsTerminal;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) fn terminal_width(fallback: usize, minimum: usize) -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            terminal_size::terminal_size()
                .map(|(terminal_size::Width(width), _)| usize::from(width))
        })
        .unwrap_or(fallback)
        .max(minimum)
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if matches!(std::env::var("CLICOLOR").as_deref(), Ok("0")) {
            return false;
        }
        crate::paths::colour_forced() || std::io::stderr().is_terminal()
    })
}

/// Force-enable colour. The REPL uses this when its readline
/// backend owns the terminal.
pub(crate) fn force_enable() {
    let _ = ENABLED.set(true);
}

const RESET: &str = "\x1b[0m";
// Light blue/cyan metadata remains distinct from source syntax colours while
// staying legible on both dark terminals and low-contrast displays.
const REPL_META_HEADING: &str = "\x1b[38;5;111m";
const REPL_META_ACCENT: &str = "\x1b[38;5;117m";
const REPL_META_DETAIL: &str = "\x1b[38;5;252m";
const REPL_ERROR: &str = "\x1b[91m";

fn wrap(prefix: &'static str, s: &str) -> String {
    if enabled() && !s.is_empty() {
        format!("{prefix}{s}{RESET}")
    } else {
        s.to_string()
    }
}

#[must_use]
pub(crate) fn error(s: &str) -> String {
    wrap("\x1b[1;31m", s)
}

/// High-contrast REPL metadata palette. These 256-colour tones deliberately
/// avoid the green, yellow, and magenta used by source syntax highlighting.
#[must_use]
pub(crate) fn repl_meta_heading(s: &str) -> String {
    wrap(REPL_META_HEADING, s)
}

#[must_use]
pub(crate) fn repl_meta_accent(s: &str) -> String {
    wrap(REPL_META_ACCENT, s)
}

#[must_use]
pub(crate) fn repl_meta_detail(s: &str) -> String {
    wrap(REPL_META_DETAIL, s)
}

#[must_use]
pub(crate) fn repl_error(s: &str) -> String {
    wrap(REPL_ERROR, s)
}

#[cfg(test)]
mod tests {
    use super::{REPL_ERROR, REPL_META_ACCENT, REPL_META_DETAIL, REPL_META_HEADING};

    #[test]
    fn repl_metadata_palette_is_distinct_and_reserves_red_for_errors() {
        let metadata = [REPL_META_HEADING, REPL_META_ACCENT, REPL_META_DETAIL];
        assert_eq!(
            metadata.as_slice(),
            ["\x1b[38;5;111m", "\x1b[38;5;117m", "\x1b[38;5;252m"]
        );
        assert_eq!(REPL_ERROR, "\x1b[91m");
        assert!(!metadata.contains(&REPL_ERROR));
    }
}
