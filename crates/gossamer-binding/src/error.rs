//! `GosError` - boundary error type for bindings.
//!
//! Bindings return `Result<T, GosError>` and propagate Rust-side
//! errors with `?`. `GosError` carries a message plus an optional
//! cause chain; on the Gossamer side it lowers to the same
//! `errors::Error` shape native `errors::new(msg)` /
//! `errors::wrap(cause, msg)` produces - pattern-matchable on arm
//! name, walkable via `errors::chain(err)`.
//!
//! # Build-side examples
//!
//! ```text
//! use gossamer_binding::{GosError, gos_module};
//!
//! gos_module!(
//!     name: cfg,
//!     doc: "Configuration loader.",
//!
//!     fn load(path: String) -> Result<String, GosError> {
//!         let s = std::fs::read_to_string(&path)?;          // From<io::Error>
//!         Ok(s)
//!     }
//! );
//! ```
//!
//! # Wire shape
//!
//! Interp tier: `GosError` → `Value::Variant { name: "Error",
//! fields: [message: String, cause: Option<Box<Error>>] }`. The
//! Gossamer `errors::Error` constructor accepts this layout
//! directly so user code reads:
//!
//! ```text
//! cfg::load(path) |> |v| result::map(use_config, v)
//!                 |> |v| result::default_with(|e| log("load: {}", e), v)
//! ```
//!
//! Compiled tier: routes through the existing `GosVariant` shape
//! used by `Result<T, String>`; the `Err` payload is a
//! single-element variant carrying the rendered message string.
//! Cause-chain depth survives because each `wrap()` call in the
//! Rust code threads it into `display()` before crossing the
//! boundary.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use crate::value::{RuntimeError, RuntimeResult, Str, Value};

use crate::conv::{FromGos, ToGos};
use crate::sig::SigType;
use crate::types::Type;

/// Boundary error type for binding fns.
///
/// Owns a heap-allocated message and an optional cause chain.
/// Cheap to clone, `Send + Sync`, `'static`. Constructed via
/// `GosError::new(msg)` or - usually - via `?` on any `E:
/// Into<GosError>`.
#[derive(Debug, Clone)]
pub struct GosError {
    inner: Arc<GosErrorInner>,
}

#[derive(Debug)]
struct GosErrorInner {
    message: String,
    cause: Option<GosError>,
}

impl GosError {
    /// Build a new error from a message.
    #[must_use]
    pub fn new<S: Into<String>>(message: S) -> Self {
        Self {
            inner: Arc::new(GosErrorInner {
                message: message.into(),
                cause: None,
            }),
        }
    }

    /// Wrap an existing error with a higher-level message.
    #[must_use]
    pub fn wrap<S: Into<String>>(self, message: S) -> Self {
        Self {
            inner: Arc::new(GosErrorInner {
                message: message.into(),
                cause: Some(self),
            }),
        }
    }

    /// Top-level message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// Walk the cause chain.
    #[must_use]
    pub fn cause(&self) -> Option<&Self> {
        self.inner.cause.as_ref()
    }

    /// Render the full chain as a `: `-joined string.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = self.inner.message.clone();
        let mut cursor = self.cause();
        while let Some(c) = cursor {
            out.push_str(": ");
            out.push_str(&c.inner.message);
            cursor = c.cause();
        }
        out
    }
}

impl fmt::Display for GosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl StdError for GosError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        // Don't expose the `cause` chain through `std::error`'s
        // source walk - the chain is binding-side metadata and the
        // Display impl already renders it. Returning `None` keeps
        // downstream `anyhow`-style chains uncluttered.
        None
    }
}

// --- From<E> conversions ---------------------------------------------

impl From<&str> for GosError {
    fn from(s: &str) -> Self {
        Self::new(s.to_string())
    }
}

impl From<String> for GosError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<std::io::Error> for GosError {
    fn from(e: std::io::Error) -> Self {
        Self::new(format!("io: {e}"))
    }
}

impl From<std::num::ParseIntError> for GosError {
    fn from(e: std::num::ParseIntError) -> Self {
        Self::new(format!("parse int: {e}"))
    }
}

impl From<std::num::ParseFloatError> for GosError {
    fn from(e: std::num::ParseFloatError) -> Self {
        Self::new(format!("parse float: {e}"))
    }
}

impl From<std::str::Utf8Error> for GosError {
    fn from(e: std::str::Utf8Error) -> Self {
        Self::new(format!("utf-8: {e}"))
    }
}

impl From<std::string::FromUtf8Error> for GosError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Self::new(format!("utf-8: {e}"))
    }
}

impl From<std::fmt::Error> for GosError {
    fn from(e: std::fmt::Error) -> Self {
        Self::new(format!("fmt: {e}"))
    }
}

impl From<std::time::SystemTimeError> for GosError {
    fn from(e: std::time::SystemTimeError) -> Self {
        Self::new(format!("system time: {e}"))
    }
}

impl From<std::convert::Infallible> for GosError {
    fn from(_: std::convert::Infallible) -> Self {
        unreachable!("Infallible has no inhabitants")
    }
}

// --- ToGos / FromGos -------------------------------------------------

impl ToGos for GosError {
    fn to_gos(self) -> Value {
        Value::String(Str::from(self.render()))
    }
}

impl FromGos for GosError {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::String(s) => Ok(Self::new(s.as_str().to_string())),
            Value::Variant(inner) => {
                let msg = inner
                    .fields
                    .first()
                    .and_then(|v| {
                        if let Value::String(s) = v {
                            Some(s.as_str().to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                Ok(Self::new(msg))
            }
            other => Err(RuntimeError::Type(format!(
                "expected GosError, found {other:?}"
            ))),
        }
    }
}

impl SigType for GosError {
    const TYPE: Type = Type::String;
}

// --- Compiled-tier `BindingAbi` --------------------------------------
//
// On the compiled tier, `GosError` rides through the same wire
// shape as `String`. The rendered message string is the visible
// payload; the cause chain is folded into it by `render()`. This
// keeps the wire shape compatible with any caller that already
// pattern-matches on `Result<T, String>` - `Result<T, GosError>`
// is wire-equivalent.

#[allow(unsafe_code, reason = "compiled-tier C-ABI bridge")]
impl crate::native::BindingAbi for GosError {
    type Input = *const std::os::raw::c_char;
    type Output = *mut std::os::raw::c_char;
    const TYPE: Type = Type::String;

    unsafe fn from_input(input: Self::Input) -> Self {
        let s = unsafe { <String as crate::native::BindingAbi>::from_input(input) };
        Self::new(s)
    }

    fn to_output(self) -> Self::Output {
        let s = self.render();
        <String as crate::native::BindingAbi>::to_output(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_render_short_message() {
        let e = GosError::new("boom");
        assert_eq!(e.message(), "boom");
        assert_eq!(e.render(), "boom");
        assert!(e.cause().is_none());
    }

    #[test]
    fn wrap_builds_chain() {
        let root = GosError::new("nope");
        let mid = root.wrap("fetching x");
        let top = mid.wrap("loading config");
        assert_eq!(top.render(), "loading config: fetching x: nope");
    }

    #[test]
    fn from_io_error_lifts() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e: GosError = io.into();
        assert!(e.render().contains("io"));
    }

    #[test]
    fn from_str_via_question_mark() {
        fn fallible() -> Result<i64, GosError> {
            "1x".parse::<i64>()?;
            Ok(0)
        }
        let err = fallible().unwrap_err();
        assert!(err.render().contains("parse int"));
    }

    #[test]
    fn round_trip_through_to_gos_from_gos() {
        let e = GosError::new("hello").wrap("outer");
        let v = e.clone().to_gos();
        let back = GosError::from_gos(&v).unwrap();
        assert!(back.render().contains("outer: hello"));
    }
}
