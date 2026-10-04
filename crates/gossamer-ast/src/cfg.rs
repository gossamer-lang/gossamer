//! `#[cfg(...)]` attribute evaluation.
//! Understands the subset of Rust's `cfg` expression grammar the
//! standard library and examples use:
//! - `#[cfg(flag)]` - `true` when the flag is active.
//! - `#[cfg(key = "value")]` - `true` when `key` maps to `value`.
//! - `#[cfg(not(expr))]` - logical negation.
//! - `#[cfg(all(a, b, …))]` - logical and.
//! - `#[cfg(any(a, b, …))]` - logical or.
//!
//! The active flags and key/value pairs describe the build's target: the
//! host running the toolchain, or the `--target` a cross build names
//! ([`set_cfg_target_triple`]); `test` is never considered active from `gos check` /
//! `gos` / `gos build` (there is no separate test build path in
//! the toolchain today, so `#[cfg(test)]` items are dropped).
//!
//! Unknown or malformed cfg expressions default to `true` so a
//! mistyped attribute does not silently hide code.

#![forbid(unsafe_code)]

use crate::Attrs;

/// Returns `true` when every `#[cfg(…)]` on `attrs` evaluates to
/// `true` under the current compilation target. Items that evaluate
/// to `false` are skipped by the resolver.
#[must_use]
pub fn item_is_active(attrs: &Attrs) -> bool {
    for attr in attrs.outer.iter().chain(attrs.inner.iter()) {
        let Some(last) = attr.path.segments.last() else {
            continue;
        };
        if last.name.name != "cfg" {
            continue;
        }
        let Some(tokens) = attr.tokens.as_deref() else {
            continue;
        };
        let Some(expr) = parse_cfg_expr(tokens) else {
            // Malformed cfg - leave the item visible rather than
            // silently drop it.
            continue;
        };
        if !evaluate(&expr) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CfgExpr {
    Flag(String),
    KeyValue(String, String),
    Not(Box<CfgExpr>),
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
}

/// The platform `#[cfg(...)]` items are resolved for: its OS, family, and
/// architecture, spelled as Rust's `target_os`, `target_family`, and
/// `target_arch` values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CfgTarget {
    os: String,
    family: String,
    arch: String,
}

impl CfgTarget {
    /// The platform running the toolchain.
    fn host() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            family: std::env::consts::FAMILY.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// The platform a target triple names (`aarch64-unknown-linux-musl`,
    /// `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`).
    fn from_triple(triple: &str) -> Self {
        let mut parts = triple.split('-');
        let arch = match parts.next().unwrap_or_default() {
            "arm64" => "aarch64",
            a if a.starts_with("riscv64") => "riscv64",
            a if a.starts_with("riscv32") => "riscv32",
            a if a.starts_with("armv7") || a.starts_with("thumbv7") => "arm",
            a if a == "i686" || a == "i586" => "x86",
            a => a,
        }
        .to_string();
        let rest: Vec<&str> = parts.collect();
        let os = if rest.contains(&"windows") {
            "windows"
        } else if rest.contains(&"darwin") || rest.contains(&"apple") || rest.contains(&"macos") {
            "macos"
        } else if rest.contains(&"linux") {
            "linux"
        } else if rest.contains(&"freebsd") {
            "freebsd"
        } else if rest.contains(&"wasi") || triple.starts_with("wasm") {
            "unknown"
        } else {
            rest.get(1).copied().unwrap_or("unknown")
        }
        .to_string();
        let family = match os.as_str() {
            "windows" => "windows",
            "unknown" if triple.starts_with("wasm") => "wasm",
            _ => "unix",
        }
        .to_string();
        Self { os, family, arch }
    }
}

static CFG_TARGET: std::sync::OnceLock<CfgTarget> = std::sync::OnceLock::new();

/// Resolves `#[cfg(...)]` for the target a build produces rather than the
/// host running the toolchain, so a cross build keeps the items written for
/// its target. Set once, before the first source is resolved; a later call
/// has no effect.
pub fn set_cfg_target_triple(triple: &str) {
    let _ = CFG_TARGET.set(CfgTarget::from_triple(triple));
}

fn cfg_target() -> &'static CfgTarget {
    CFG_TARGET.get_or_init(CfgTarget::host)
}

/// The platform the current build resolves `#[cfg]` against, as one
/// `os/family/arch` string for keys that must change when it does.
#[must_use]
pub fn cfg_target_key() -> String {
    let target = cfg_target();
    format!("{}/{}/{}", target.os, target.family, target.arch)
}

/// The `target_family` the current build resolves `#[cfg]` against:
/// `unix`, `windows`, or `wasm`.
#[must_use]
pub fn cfg_target_family() -> &'static str {
    &cfg_target().family
}

/// `source` with every item inactive under the current cfg removed, at any
/// depth, or `None` when every item is active.
///
/// The resolver never resolves an inactive item, so a pass that walks the
/// items after it reads this view: an item it would otherwise see has no
/// resolutions, and checking it reports names and bindings as missing.
#[must_use]
pub fn without_inactive_items(source: &crate::SourceFile) -> Option<crate::SourceFile> {
    if !any_inactive(&source.items) {
        return None;
    }
    let mut stripped = source.clone();
    retain_active(&mut stripped.items);
    Some(stripped)
}

fn any_inactive(items: &[crate::Item]) -> bool {
    items.iter().any(|item| {
        !item_is_active(&item.attrs)
            || matches!(
                &item.kind,
                crate::ItemKind::Mod(decl)
                    if matches!(&decl.body, crate::ModBody::Inline(inner) if any_inactive(inner))
            )
    })
}

fn retain_active(items: &mut Vec<crate::Item>) {
    items.retain(|item| item_is_active(&item.attrs));
    for item in items {
        if let crate::ItemKind::Mod(decl) = &mut item.kind
            && let crate::ModBody::Inline(inner) = &mut decl.body
        {
            retain_active(inner);
        }
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

static TEST_CFG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Toggles whether `#[cfg(test)]`-gated items are visible to the
/// resolver. `gos test` sets this to `true` so `mod tests { ... }`
/// blocks are lowered into HIR; all other driver commands keep the
/// default (`false`) so non-test builds stay lean.
pub fn set_test_cfg(enabled: bool) {
    TEST_CFG_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Reports whether `#[cfg(test)]` items are currently visible. The flag
/// selects which items the resolver admits, so it is part of the identity
/// of any cached front-end result.
#[must_use]
pub fn test_cfg_enabled() -> bool {
    TEST_CFG_ENABLED.load(Ordering::Relaxed)
}

fn flag_is_active(name: &str) -> bool {
    if matches!(name, "unix" | "windows") && cfg_target().family == name {
        return true;
    }
    if name == "test" && TEST_CFG_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    false
}

fn active_key_value(key: &str) -> Option<&'static str> {
    let target = cfg_target();
    match key {
        "target_os" => Some(target.os.as_str()),
        "target_family" => Some(target.family.as_str()),
        "target_arch" => Some(target.arch.as_str()),
        _ => None,
    }
}

fn evaluate(expr: &CfgExpr) -> bool {
    match expr {
        CfgExpr::Flag(name) => flag_is_active(name),
        CfgExpr::KeyValue(key, value) => active_key_value(key) == Some(value.as_str()),
        CfgExpr::Not(inner) => !evaluate(inner),
        CfgExpr::All(parts) => parts.iter().all(evaluate),
        CfgExpr::Any(parts) => parts.iter().any(evaluate),
    }
}

/// Parses the token body of a `#[cfg(...)]` attribute into an
/// expression tree. Very forgiving: returns `None` only for clearly
/// malformed inputs.
fn parse_cfg_expr(tokens: &str) -> Option<CfgExpr> {
    let mut parser = CfgParser::new(tokens);
    let expr = parser.parse_expr()?;
    parser.skip_whitespace();
    if parser.cursor < parser.bytes.len() {
        return None;
    }
    Some(expr)
}

struct CfgParser<'src> {
    bytes: &'src [u8],
    cursor: usize,
}

impl<'src> CfgParser<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            bytes: source.as_bytes(),
            cursor: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b) if b.is_ascii_whitespace()) {
            self.cursor += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(b) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn parse_expr(&mut self) -> Option<CfgExpr> {
        self.skip_whitespace();
        let ident = self.parse_ident()?;
        self.skip_whitespace();
        if self.eat(b'(') {
            let parts = self.parse_comma_list()?;
            if !self.eat(b')') {
                return None;
            }
            return match ident.as_str() {
                "not" => {
                    if parts.len() != 1 {
                        return None;
                    }
                    Some(CfgExpr::Not(Box::new(parts.into_iter().next()?)))
                }
                "all" => Some(CfgExpr::All(parts)),
                "any" => Some(CfgExpr::Any(parts)),
                _ => None,
            };
        }
        if self.eat(b'=') {
            self.skip_whitespace();
            let value = self.parse_string()?;
            return Some(CfgExpr::KeyValue(ident, value));
        }
        Some(CfgExpr::Flag(ident))
    }

    fn parse_comma_list(&mut self) -> Option<Vec<CfgExpr>> {
        let mut out = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b')') {
                break;
            }
            out.push(self.parse_expr()?);
            self.skip_whitespace();
            if !self.eat(b',') {
                break;
            }
        }
        Some(out)
    }

    fn parse_ident(&mut self) -> Option<String> {
        self.skip_whitespace();
        let start = self.cursor;
        while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_') {
            self.cursor += 1;
        }
        if start == self.cursor {
            return None;
        }
        Some(String::from_utf8_lossy(&self.bytes[start..self.cursor]).into_owned())
    }

    fn parse_string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let start = self.cursor;
        while let Some(b) = self.peek() {
            if b == b'"' {
                let end = self.cursor;
                self.cursor += 1;
                return Some(String::from_utf8_lossy(&self.bytes[start..end]).into_owned());
            }
            self.cursor += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(os: &str, family: &str, arch: &str) -> CfgTarget {
        CfgTarget {
            os: os.to_string(),
            family: family.to_string(),
            arch: arch.to_string(),
        }
    }

    #[test]
    fn a_cross_triple_names_its_own_platform() {
        assert_eq!(
            CfgTarget::from_triple("aarch64-unknown-linux-musl"),
            target("linux", "unix", "aarch64")
        );
        assert_eq!(
            CfgTarget::from_triple("riscv64gc-unknown-linux-gnu"),
            target("linux", "unix", "riscv64")
        );
        assert_eq!(
            CfgTarget::from_triple("x86_64-pc-windows-msvc"),
            target("windows", "windows", "x86_64")
        );
        assert_eq!(
            CfgTarget::from_triple("arm64-apple-darwin"),
            target("macos", "unix", "aarch64")
        );
    }

    #[test]
    fn the_host_target_matches_the_toolchain_platform() {
        let host = CfgTarget::host();
        assert_eq!(host.os, std::env::consts::OS);
        assert_eq!(host.arch, std::env::consts::ARCH);
    }

    #[test]
    fn flag_matches_active_flag() {
        #[cfg(unix)]
        assert!(evaluate(&CfgExpr::Flag("unix".to_string())));
        assert!(!evaluate(&CfgExpr::Flag("nonexistent".to_string())));
    }

    #[test]
    fn not_flips_result() {
        assert!(evaluate(&CfgExpr::Not(Box::new(CfgExpr::Flag(
            "nonexistent".to_string(),
        )))));
    }

    #[test]
    fn parse_roundtrips_common_shapes() {
        assert_eq!(
            parse_cfg_expr("test"),
            Some(CfgExpr::Flag("test".to_string()))
        );
        assert_eq!(
            parse_cfg_expr("target_os = \"linux\""),
            Some(CfgExpr::KeyValue(
                "target_os".to_string(),
                "linux".to_string()
            ))
        );
        assert_eq!(
            parse_cfg_expr("not ( windows )"),
            Some(CfgExpr::Not(Box::new(CfgExpr::Flag("windows".to_string()))))
        );
    }

    #[test]
    fn all_and_any_compose() {
        // The parser must succeed on every platform; the
        // truth-table half is unix-only because `evaluate` would
        // otherwise return false for the `not(windows)` arm.
        let parsed = parse_cfg_expr("all ( unix , not ( windows ) )");
        assert!(parsed.is_some(), "all/any composition parses");
        #[cfg(unix)]
        assert!(evaluate(&parsed.expect("checked Some above")));
    }
}
