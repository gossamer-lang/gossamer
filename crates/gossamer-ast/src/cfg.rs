//! `#[cfg(...)]` attribute evaluation.
//! Understands the subset of Rust's `cfg` expression grammar the
//! standard library and examples use:
//! - `#[cfg(flag)]` - `true` when the flag is active.
//! - `#[cfg(key = "value")]` - `true` when `key` maps to `value`.
//! - `#[cfg(not(expr))]` - logical negation.
//! - `#[cfg(all(a, b, …))]` - logical and.
//! - `#[cfg(any(a, b, …))]` - logical or.
//! - `#[cfg(feature = "name")]` - `true` when the package declaring the
//!   item builds with that feature ([`set_package_features`]).
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

/// The platform `#[cfg(...)]` items are resolved for: its OS, family,
/// architecture, C library, pointer width, and byte order, spelled as Rust's
/// `target_os`, `target_family`, `target_arch`, `target_env`,
/// `target_pointer_width`, and `target_endian` values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CfgTarget {
    os: String,
    family: String,
    arch: String,
    env: String,
    pointer_width: String,
    endian: String,
}

impl CfgTarget {
    /// The platform running the toolchain.
    fn host() -> Self {
        // The C library the toolchain itself runs on is the one the bytecode
        // VM's foreign calls reach.
        let env = if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else if cfg!(target_env = "msvc") {
            "msvc"
        } else {
            ""
        };
        Self {
            os: std::env::consts::OS.to_string(),
            family: std::env::consts::FAMILY.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            env: env.to_string(),
            pointer_width: (usize::BITS).to_string(),
            endian: if cfg!(target_endian = "big") {
                "big"
            } else {
                "little"
            }
            .to_string(),
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
        let env = rest
            .last()
            .map(|last| {
                if last.starts_with("musl") {
                    "musl"
                } else if last.starts_with("gnu") {
                    "gnu"
                } else if *last == "msvc" {
                    "msvc"
                } else {
                    ""
                }
            })
            .unwrap_or_default()
            .to_string();
        let pointer_width = match arch.as_str() {
            "x86" | "arm" | "riscv32" | "wasm32" => "32",
            _ => "64",
        }
        .to_string();
        let endian = match arch.as_str() {
            "powerpc" | "powerpc64" | "s390x" | "sparc64" | "mips" | "mips64" => "big",
            _ => "little",
        }
        .to_string();
        Self {
            os,
            family,
            arch,
            env,
            pointer_width,
            endian,
        }
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

/// The platform and features the current build resolves `#[cfg]`
/// against, as one string for keys that must change when either does.
#[must_use]
pub fn cfg_target_key() -> String {
    let target = cfg_target();
    let mut key = format!(
        "{}/{}/{}/{}/{}/{}",
        target.os, target.family, target.arch, target.env, target.pointer_width, target.endian
    );
    if let Some(features) = PACKAGE_FEATURES.read().ok().and_then(|map| map.clone()) {
        for (package, enabled) in features {
            let names: Vec<&str> = enabled.iter().map(String::as_str).collect();
            key.push_str(&format!("|{package}:{}", names.join(",")));
        }
    }
    key
}

/// The features each package of the program builds with: the program's
/// own project under `""`, each dependency under its project id.
type FeatureMap = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

static PACKAGE_FEATURES: std::sync::RwLock<Option<FeatureMap>> = std::sync::RwLock::new(None);

/// Sets the features `#[cfg(feature = "..")]` answers for, per package:
/// the program's own project under `""`, a dependency under its id.
pub fn set_package_features(features: FeatureMap) {
    if let Ok(mut slot) = PACKAGE_FEATURES.write() {
        *slot = Some(features);
    }
}

/// Whether the package `package` (`""` for the program's own project)
/// builds with the feature `name`.
#[must_use]
pub fn feature_enabled(package: &str, name: &str) -> bool {
    PACKAGE_FEATURES.read().ok().is_some_and(|map| {
        map.as_ref()
            .and_then(|map| map.get(package))
            .is_some_and(|enabled| enabled.contains(name))
    })
}

/// Settles every `feature = "name"` in the `#[cfg]` attributes of `sf` for
/// the package declaring the item: a dependency's items sit in the
/// `#[dependency("id")]` module the bundler wraps them in, and every other
/// item is the program's own. The settled attribute reads `all()` (true) or
/// `any()` (false) in place of the test.
pub fn apply_package_features(sf: &mut crate::SourceFile) {
    if sf.uses.iter().any(|decl| decl.cfg.is_some()) {
        let dependencies: std::collections::BTreeMap<String, String> = sf
            .items
            .iter()
            .filter_map(|item| match &item.kind {
                crate::ItemKind::Mod(decl) => item
                    .attrs
                    .outer
                    .iter()
                    .find_map(|attr| attr.string_argument("dependency"))
                    .map(|id| (decl.name.name.clone(), id.to_string())),
                _ => None,
            })
            .collect();
        sf.uses.retain(|decl| {
            let Some(cfg) = decl.cfg.as_deref() else {
                return true;
            };
            let package = decl
                .module
                .first()
                .and_then(|module| dependencies.get(module))
                .map_or("", String::as_str);
            parse_cfg_expr(cfg).is_none_or(|expr| evaluate(&settle(expr, package)))
        });
    }
    if mentions_feature(&sf.items) {
        settle_items(&mut sf.items, "");
    }
}

fn mentions_feature(items: &[crate::Item]) -> bool {
    items.iter().any(|item| {
        item.attrs
            .outer
            .iter()
            .chain(&item.attrs.inner)
            .any(|attr| attr.is_named("cfg") && attr.tokens.as_deref().is_some_and(|t| t.contains("feature")))
            || matches!(
                &item.kind,
                crate::ItemKind::Mod(decl)
                    if matches!(&decl.body, crate::ModBody::Inline(inner) if mentions_feature(inner))
            )
    })
}

fn settle_items(items: &mut [crate::Item], package: &str) {
    for item in items {
        settle_attrs(&mut item.attrs, package);
        if let crate::ItemKind::Mod(decl) = &mut item.kind
            && let crate::ModBody::Inline(inner) = &mut decl.body
        {
            let dependency = item
                .attrs
                .outer
                .iter()
                .find_map(|attr| attr.string_argument("dependency").map(str::to_string));
            settle_items(inner, dependency.as_deref().unwrap_or(package));
        }
    }
}

fn settle_attrs(attrs: &mut crate::Attrs, package: &str) {
    for attr in attrs.outer.iter_mut().chain(attrs.inner.iter_mut()) {
        if !attr.is_named("cfg") {
            continue;
        }
        let Some(expr) = attr.tokens.as_deref().and_then(parse_cfg_expr) else {
            continue;
        };
        let settled = settle(expr, package);
        attr.tokens = Some(settled.to_string());
    }
}

fn settle(expr: CfgExpr, package: &str) -> CfgExpr {
    match expr {
        CfgExpr::KeyValue(key, value) if key == "feature" => {
            if feature_enabled(package, &value) {
                CfgExpr::All(Vec::new())
            } else {
                CfgExpr::Any(Vec::new())
            }
        }
        CfgExpr::Not(inner) => CfgExpr::Not(Box::new(settle(*inner, package))),
        CfgExpr::All(parts) => {
            CfgExpr::All(parts.into_iter().map(|p| settle(p, package)).collect())
        }
        CfgExpr::Any(parts) => {
            CfgExpr::Any(parts.into_iter().map(|p| settle(p, package)).collect())
        }
        other => other,
    }
}

impl std::fmt::Display for CfgExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = |f: &mut std::fmt::Formatter<'_>, name: &str, parts: &[CfgExpr]| {
            write!(f, "{name}(")?;
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{part}")?;
            }
            write!(f, ")")
        };
        match self {
            Self::Flag(name) => write!(f, "{name}"),
            Self::KeyValue(key, value) => write!(f, "{key} = {value:?}"),
            Self::Not(inner) => write!(f, "not({inner})"),
            Self::All(parts) => list(f, "all", parts),
            Self::Any(parts) => list(f, "any", parts),
        }
    }
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
        "target_env" => Some(target.env.as_str()),
        "target_pointer_width" => Some(target.pointer_width.as_str()),
        "target_endian" => Some(target.endian.as_str()),
        _ => None,
    }
}

fn evaluate(expr: &CfgExpr) -> bool {
    match expr {
        CfgExpr::Flag(name) => flag_is_active(name),
        // A test [`apply_package_features`] has not settled is the
        // program's own.
        CfgExpr::KeyValue(key, value) if key == "feature" => feature_enabled("", value),
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

    fn target(os: &str, family: &str, arch: &str, env: &str) -> CfgTarget {
        CfgTarget {
            os: os.to_string(),
            family: family.to_string(),
            arch: arch.to_string(),
            env: env.to_string(),
            pointer_width: "64".to_string(),
            endian: "little".to_string(),
        }
    }

    #[test]
    fn a_cross_triple_names_its_own_platform() {
        assert_eq!(
            CfgTarget::from_triple("aarch64-unknown-linux-musl"),
            target("linux", "unix", "aarch64", "musl")
        );
        assert_eq!(
            CfgTarget::from_triple("riscv64gc-unknown-linux-gnu"),
            target("linux", "unix", "riscv64", "gnu")
        );
        assert_eq!(
            CfgTarget::from_triple("x86_64-pc-windows-msvc"),
            target("windows", "windows", "x86_64", "msvc")
        );
        assert_eq!(
            CfgTarget::from_triple("arm64-apple-darwin"),
            target("macos", "unix", "aarch64", "")
        );
    }

    #[test]
    fn a_32_bit_triple_reports_its_pointer_width() {
        let wasm = CfgTarget::from_triple("wasm32-unknown-unknown");
        assert_eq!(wasm.pointer_width, "32");
        assert_eq!(wasm.endian, "little");
        assert_eq!(wasm.env, "");
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
