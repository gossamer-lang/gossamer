//! Stdlib symbol index used by the LSP completion / import-assist
//! paths.
//!
//! The bulk of the data already lives in `gossamer_std::registry` -
//! this module wraps it with two extra lookup tables tuned for the
//! shapes the LSP asks for:
//!
//! * `members_of(qualifier)` - list every top-level item exported from
//!   the module identified by a `::`-segment slice. Tolerant of three
//!   spellings: the canonical `std::fmt`, the user-facing `fmt`, and
//!   any segment-aliased `use` rebinding the calling site supplies.
//! * `fuzzy_paths_for(name)` - given a bare identifier, return every
//!   fully-qualified path that exports it. Powers the auto-import
//!   suggestion in Phase 4.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use gossamer_std::registry::{StdItemKind, modules};

/// Single member exported from a stdlib module (function, type,
/// constant, …).
#[derive(Debug, Clone)]
pub(crate) struct MemberSpec {
    /// Bare identifier - what the editor inserts after the user picks
    /// the completion.
    pub name: String,
    /// LSP `CompletionItemKind` (matches the protocol's wire numbers).
    pub kind: u32,
    /// Optional one-line signature / module summary used as `detail`.
    pub detail: Option<String>,
    /// Optional doc string used as `documentation.value`.
    pub doc: Option<String>,
}

/// Stdlib index. Built once at startup from the static registry.
#[derive(Debug, Default)]
pub(crate) struct StdlibIndex {
    /// `std::os` → list of items + sub-module pseudo-members.
    by_path: HashMap<String, Vec<MemberSpec>>,
    /// Canonical paths keyed by their bare leaf name. Lets the LSP
    /// resolve `os::p` (qualifier `["os"]`) without forcing the user
    /// to write the full `std::os` prefix.
    by_leaf: HashMap<String, Vec<String>>,
    /// `name → [canonical path]` for every item in every module.
    /// Powers fuzzy auto-import suggestions.
    by_member_name: HashMap<String, Vec<String>>,
    /// All canonical module paths (`std::fmt`, `std::os::signal`, …).
    all_paths: Vec<String>,
}

impl StdlibIndex {
    /// Builds the index from the compile-time registry.
    pub(crate) fn build() -> Self {
        let mut idx = Self::default();
        for module in modules() {
            idx.all_paths.push(module.path.to_string());
            // Collect items as members of this module.
            let mut entries: Vec<MemberSpec> = module
                .items
                .iter()
                .map(|item| MemberSpec {
                    name: item.name.to_string(),
                    kind: completion_kind_for(item.kind),
                    detail: Some(format!(
                        "{kind} {path}::{name}",
                        kind = std_kind_label(item.kind),
                        path = module.path,
                        name = item.name,
                    )),
                    doc: Some(item.doc.to_string()),
                })
                .collect();
            // Promote sub-module relationships: any module path of the
            // form "<this>::<child>" becomes a Module entry on `this`.
            for other in modules() {
                if let Some(rest) = other.path.strip_prefix(&format!("{}::", module.path)) {
                    if !rest.contains("::") {
                        entries.push(MemberSpec {
                            name: rest.to_string(),
                            kind: COMPLETION_KIND_MODULE,
                            detail: Some(format!("module {}", other.path)),
                            doc: Some(other.summary.to_string()),
                        });
                    }
                }
            }
            // Dedupe by name so registry-declared sub-modules don't
            // collide with the auto-discovered relationship pass above.
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            entries.dedup_by(|a, b| a.name == b.name);
            idx.by_path.insert(module.path.to_string(), entries);

            // Leaf alias: a module ending in `…::leaf` is reachable as
            // the bare `leaf` qualifier. If two stdlib modules share a
            // leaf name (none today, but possible later), all paths
            // accumulate so we can disambiguate later.
            if let Some(leaf) = module.path.rsplit("::").next() {
                idx.by_leaf
                    .entry(leaf.to_string())
                    .or_default()
                    .push(module.path.to_string());
            }
            for item in module.items {
                idx.by_member_name
                    .entry(item.name.to_string())
                    .or_default()
                    .push(format!("{}::{}", module.path, item.name));
            }
        }
        idx
    }

    /// Returns every direct member of the module identified by
    /// `qualifier`, or `None` when the qualifier is unknown.
    /// Recognises three spellings:
    /// * the full canonical path (`["std", "fmt"]`),
    /// * the same path with the leading `std` stripped (`["fmt"]`),
    /// * a leaf-name alias (`["fmt"]`, `["json"]`, `["signal"]`, …).
    pub(crate) fn members_of(&self, qualifier: &[&str]) -> Option<Vec<MemberSpec>> {
        if qualifier.is_empty() {
            return None;
        }
        // Synthetic root: "std" by itself surfaces every top-level
        // submodule (the typical `use std::|` case).
        if qualifier == ["std"] {
            return Some(self.std_top_level_submodules());
        }
        // Direct hit.
        let key = qualifier.join("::");
        if let Some(entries) = self.by_path.get(&key) {
            return Some(entries.clone());
        }
        // Implicit `std::` prefix.
        let with_std = format!("std::{key}");
        if let Some(entries) = self.by_path.get(&with_std) {
            return Some(entries.clone());
        }
        // Single-segment leaf alias.
        if qualifier.len() == 1 {
            if let Some(paths) = self.by_leaf.get(qualifier[0]) {
                if let Some(first) = paths.first() {
                    return self.by_path.get(first).cloned();
                }
            }
        }
        // Multi-segment alias: try resolving the head as a leaf alias
        // and the tail as a continuation.
        if qualifier.len() > 1 {
            if let Some(paths) = self.by_leaf.get(qualifier[0]) {
                for prefix in paths {
                    let combined = format!("{}::{}", prefix, qualifier[1..].join("::"));
                    if let Some(entries) = self.by_path.get(&combined) {
                        return Some(entries.clone());
                    }
                }
            }
        }
        None
    }

    fn std_top_level_submodules(&self) -> Vec<MemberSpec> {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut out: Vec<MemberSpec> = Vec::new();
        for path in &self.all_paths {
            let Some(rest) = path.strip_prefix("std::") else {
                continue;
            };
            let head = rest.split("::").next().unwrap_or(rest);
            if !seen.insert(head.to_string()) {
                continue;
            }
            let summary = self.by_path.get(&format!("std::{head}")).and_then(|_| {
                gossamer_std::registry::module(&format!("std::{head}"))
                    .map(|m| m.summary.to_string())
            });
            out.push(MemberSpec {
                name: head.to_string(),
                kind: COMPLETION_KIND_MODULE,
                detail: Some(format!("module std::{head}")),
                doc: summary,
            });
        }
        out
    }

    /// Returns every top-level stdlib module name. Used by use-context
    /// completion when the qualifier is empty (e.g. `use std::|`).
    pub(crate) fn root_modules(&self) -> Vec<MemberSpec> {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut out: Vec<MemberSpec> = Vec::new();
        for path in &self.all_paths {
            let mut iter = path.split("::");
            let first = iter.next().unwrap_or(path);
            if !seen.insert(first.to_string()) {
                continue;
            }
            out.push(MemberSpec {
                name: first.to_string(),
                kind: COMPLETION_KIND_MODULE,
                detail: Some(format!("module {first}")),
                doc: None,
            });
        }
        out
    }

    /// Returns every fully-qualified path matching the bare item name
    /// `name`. Used by import-assist to suggest auto-`use` edits.
    #[allow(
        dead_code,
        reason = "tested API; consumed by import-assist once the code-action lands"
    )]
    pub(crate) fn fuzzy_paths_for(&self, name: &str) -> Vec<String> {
        if name.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        if let Some(paths) = self.by_member_name.get(name) {
            out.extend(paths.iter().cloned());
        }
        // Modules whose leaf matches.
        if let Some(paths) = self.by_leaf.get(name) {
            out.extend(paths.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    /// Returns every fully-qualified path whose leaf identifier starts
    /// with `prefix`. Used by import-assist completion to surface a
    /// `use` suggestion as soon as the user types the first few
    /// characters of an out-of-scope name.
    pub(crate) fn fuzzy_paths_for_prefix(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        for (name, paths) in &self.by_member_name {
            if name.starts_with(prefix) {
                out.extend(paths.iter().cloned());
            }
        }
        for (name, paths) in &self.by_leaf {
            if name.starts_with(prefix) {
                out.extend(paths.iter().cloned());
            }
        }
        out.sort();
        out.dedup();
        // Cap to keep editor popups manageable.
        out.truncate(50);
        out
    }

    /// Returns the unique canonical module path for a bare qualifier.
    pub(crate) fn canonical_module_for_leaf(&self, leaf: &str) -> Option<&str> {
        let paths = self.by_leaf.get(leaf)?;
        (paths.len() == 1).then(|| paths[0].as_str())
    }
}

const COMPLETION_KIND_FUNCTION: u32 = 3;
const COMPLETION_KIND_MODULE: u32 = 9;
const COMPLETION_KIND_STRUCT: u32 = 22;
const COMPLETION_KIND_TRAIT: u32 = 8;
const COMPLETION_KIND_CONSTANT: u32 = 21;

fn completion_kind_for(kind: StdItemKind) -> u32 {
    match kind {
        StdItemKind::Function | StdItemKind::Builtin => COMPLETION_KIND_FUNCTION,
        StdItemKind::Type => COMPLETION_KIND_STRUCT,
        StdItemKind::Trait => COMPLETION_KIND_TRAIT,
        StdItemKind::Const => COMPLETION_KIND_CONSTANT,
    }
}

fn std_kind_label(kind: StdItemKind) -> &'static str {
    match kind {
        StdItemKind::Function => "fn",
        StdItemKind::Builtin => "builtin",
        StdItemKind::Type => "type",
        StdItemKind::Trait => "trait",
        StdItemKind::Const => "const",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> StdlibIndex {
        StdlibIndex::build()
    }

    #[test]
    fn members_of_resolves_canonical_path() {
        let idx = idx();
        let members = idx
            .members_of(&["std", "fmt"])
            .expect("std::fmt should be known");
        assert!(members.iter().any(|m| m.name == "format"));
    }

    #[test]
    fn members_of_resolves_leaf_alias() {
        let idx = idx();
        let members = idx.members_of(&["fmt"]).expect("fmt leaf alias");
        assert!(members.iter().any(|m| m.name == "format"));
    }

    #[test]
    fn members_of_lists_submodules() {
        let idx = idx();
        let members = idx.members_of(&["os"]).expect("os module");
        assert!(
            members.iter().any(|m| m.name == "signal"),
            "expected `signal` submodule in os, got {:?}",
            members.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn members_of_std_returns_top_level_submodules() {
        let idx = idx();
        let members = idx.members_of(&["std"]).expect("std root");
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"fmt"), "expected fmt in {names:?}");
        assert!(names.contains(&"os"), "expected os in {names:?}");
    }

    #[test]
    fn fuzzy_paths_for_returns_canonical_paths() {
        let idx = idx();
        let paths = idx.fuzzy_paths_for("format");
        assert!(
            paths.iter().any(|p| p == "std::fmt::format"),
            "expected std::fmt::format in {paths:?}"
        );
    }

    #[test]
    fn root_modules_includes_std() {
        let idx = idx();
        let roots = idx.root_modules();
        assert!(roots.iter().any(|m| m.name == "std"));
    }
}
