//! System clipboard binding: wraps the published `arboard` crate
//! (which knows nothing about Gossamer) behind two Gossamer-callable
//! functions.
//!
//! `register_module!` emits both the VM thunk `gos run` calls and the C-ABI
//! thunk (`gos_binding_clipboard__get_text`, ...) a native build links, so
//! the same crate is callable from `gos run`, `gos test`, and
//! `gos build --release`.
//!
//! On Linux the native arboard path (X11 protocol) is tried first;
//! when it fails (Wayland-only session, Termux, headless), the
//! binding falls back to the common clipboard utilities - wl-copy /
//! wl-paste, xclip, xsel, termux-clipboard-get/set. Only when none
//! of those exist does an operation fail, with a message asking for
//! a clipboard utility to be installed.

use arboard::Clipboard;
use gossamer_binding::register_module;

/// Sentinel env var marking the re-executed clipboard-holder process.
#[cfg(target_os = "linux")]
const HOLD_ENV: &str = "GOS_CLIPLIB_HOLD";

/// Guidance appended when no clipboard path exists on Linux.
#[cfg(target_os = "linux")]
const INSTALL_HINT: &str = "No clipboard support available. Please install a clipboard \
     utility appropriate for your machine.";

register_module!(
    name: clipboard,
    doc: "System clipboard access (arboard, with Linux utility fallbacks).",

    /// Current clipboard text; Ok("") when the clipboard is empty.
    /// Err when no clipboard is reachable at all.
    fn get_text() -> Result<String, String> {
        get_text_impl()
    }

    /// Replaces the clipboard contents with `text`.
    fn set_text(text: String) -> Result<(), String> {
        set_text_impl(text)
    }
);

#[cfg(not(target_os = "linux"))]
fn get_text_impl() -> Result<String, String> {
    let mut cb = Clipboard::new().map_err(|e| format!("Error reading clipboard: {e}"))?;
    match cb.get_text() {
        Ok(text) => Ok(text),
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        Err(e) => Err(format!("Error reading clipboard: {e}")),
    }
}

#[cfg(not(target_os = "linux"))]
fn set_text_impl(text: String) -> Result<(), String> {
    Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .map_err(|e| format!("Error writing output: {e}"))
}

#[cfg(target_os = "linux")]
fn get_text_impl() -> Result<String, String> {
    let native_err = match Clipboard::new() {
        Ok(mut cb) => match cb.get_text() {
            Ok(text) => return Ok(text),
            Err(arboard::Error::ContentNotAvailable) => return Ok(String::new()),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    match util_read() {
        Ok(text) => Ok(text),
        Err(UtilError::NoneFound) => Err(format!(
            "Error reading clipboard on Linux: {INSTALL_HINT} (native clipboard: {native_err})"
        )),
        Err(UtilError::Failed(msg)) => Err(format!(
            "Error reading clipboard on Linux: {msg} (native clipboard: {native_err})"
        )),
    }
}

#[cfg(target_os = "linux")]
fn set_text_impl(text: String) -> Result<(), String> {
    if std::env::var_os(HOLD_ENV).is_some() {
        return hold_clipboard(text).map_err(|e| format!("Error writing output on Linux: {e}"));
    }
    let native_err = match native_set_via_holder() {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    match util_write(&text) {
        Ok(()) => Ok(()),
        Err(UtilError::NoneFound) => Err(format!(
            "Error writing output on Linux: {INSTALL_HINT} (native clipboard: {native_err})"
        )),
        Err(UtilError::Failed(msg)) => Err(format!(
            "Error writing output on Linux: {msg} (native clipboard: {native_err})"
        )),
    }
}

/// Holder-process body: claim the selection, acknowledge with one
/// `+` byte on stderr (the spawning parent blocks on it), then serve
/// paste requests until another application replaces the contents.
#[cfg(target_os = "linux")]
fn hold_clipboard(text: String) -> Result<(), String> {
    use std::io::Write;

    use arboard::SetExtLinux;

    let mut cb = Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.clone()).map_err(|e| e.to_string())?;
    let _ = std::io::stderr().write_all(b"+");
    cb.set().wait().text(text).map_err(|e| e.to_string())
}

/// X11 clipboard contents die with the process that set them, so a
/// short-lived CLI cannot set-and-exit: without a clipboard manager
/// in the session, nothing remains to answer paste requests. The
/// standard fix - what xclip and arboard's `daemonize` example do -
/// is to detach a holder process that owns the selection until
/// another application replaces it. The holder here is this same
/// program re-executed with `HOLD_ENV` set: the re-run reaches
/// `set_text_impl` again and takes the `hold_clipboard` branch. The
/// parent blocks on the holder's `+` acknowledgement so the new
/// contents are readable the moment `set_text` returns.
#[cfg(target_os = "linux")]
fn native_set_via_holder() -> Result<(), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Read argv from /proc/self/cmdline: `std::env::args()` is empty
    // in a static-musl AOT binary (musl hands no argv to
    // initializers, and the program does not enter through Rust's
    // `lang_start`), while the cmdline file is always populated.
    let cmdline = std::fs::read("/proc/self/cmdline").map_err(|e| e.to_string())?;
    let args: Vec<std::ffi::OsString> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .skip(1)
        .map(|s| std::os::unix::ffi::OsStringExt::from_vec(s.to_vec()))
        .collect();
    let mut child = Command::new(exe)
        .args(args)
        .env(HOLD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut pipe = child.stderr.take().ok_or("holder stderr unavailable")?;
    let mut byte = [0u8; 1];
    match pipe.read(&mut byte) {
        Ok(1) if byte[0] == b'+' => Ok(()),
        _ => Err("clipboard holder failed to claim the selection".into()),
    }
}

#[cfg(target_os = "linux")]
enum UtilError {
    /// Every candidate utility was absent from PATH.
    NoneFound,
    /// At least one utility exists; the last one to run failed thus.
    Failed(String),
}

/// Read commands tried in order. None of these daemonize on read, so
/// capturing their output with `Command::output` cannot block.
#[cfg(target_os = "linux")]
const READ_UTILITIES: &[&[&str]] = &[
    &["wl-paste", "--no-newline"],
    &["xclip", "-selection", "clipboard", "-o"],
    &["xsel", "--clipboard", "--output"],
    &["termux-clipboard-get"],
];

/// Write commands tried in order; each reads the text from stdin.
/// wl-copy / xclip / xsel fork their own holder process, so plain
/// `wait` returns promptly while the contents stay alive.
#[cfg(target_os = "linux")]
const WRITE_UTILITIES: &[&[&str]] = &[
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
    &["termux-clipboard-set"],
];

#[cfg(target_os = "linux")]
fn util_read() -> Result<String, UtilError> {
    let mut last_failure: Option<String> = None;
    for cmd in READ_UTILITIES {
        match std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(out) => {
                let detail = String::from_utf8_lossy(&out.stderr);
                let detail = detail.trim();
                last_failure = Some(if detail.is_empty() {
                    format!("{} exited with {}", cmd[0], out.status)
                } else {
                    format!("{} failed: {detail}", cmd[0])
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => last_failure = Some(format!("{}: {e}", cmd[0])),
        }
    }
    Err(last_failure.map_or(UtilError::NoneFound, UtilError::Failed))
}

#[cfg(target_os = "linux")]
fn util_write(text: &str) -> Result<(), UtilError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut last_failure: Option<String> = None;
    for cmd in WRITE_UTILITIES {
        // stderr must NOT be piped: the holder these tools fork
        // inherits it and would keep the pipe open forever, hanging
        // any read-to-EOF. `wait` only reaps the direct child.
        let spawned = Command::new(cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                match child.wait() {
                    Ok(status) if status.success() => return Ok(()),
                    Ok(status) => {
                        last_failure = Some(format!("{} exited with {status}", cmd[0]));
                    }
                    Err(e) => last_failure = Some(format!("{}: {e}", cmd[0])),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => last_failure = Some(format!("{}: {e}", cmd[0])),
        }
    }
    Err(last_failure.map_or(UtilError::NoneFound, UtilError::Failed))
}

