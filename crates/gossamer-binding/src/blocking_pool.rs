//! The blocking region `#[gos_blocking]` runs a binding body in.

/// Runs `f` as a blocking native call: the host's scheduler keeps other
/// goroutines running while it does, and `f` runs on the calling thread.
pub fn run_blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    crate::host::blocking(f)
}
