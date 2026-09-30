#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

/// Renders one stdlib module as a Markdown page (Python-style
/// per-module reference). Used by `gos doc --emit-stdlib`. The
/// page carries a lifecycle marker only when the module is experimental.
/// Routine availability data belongs in `gos feature-status`, not in the
/// user-facing reference.
#[must_use]
pub fn render_module_markdown(module: &StdModule) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!("# `{}`\n\n", module.path));
    // The reported status, not the authored one: a surface no fixture
    // exercises reads `unproven` here, so a page cannot claim more than
    // the evidence behind it.
    if let Some(entry) = super::feature_status::lookup(module.path) {
        let derived = super::feature_status::derived_status(module.path, entry.status);
        if matches!(
            derived,
            super::feature_status::Status::Experimental | super::feature_status::Status::Unproven
        ) {
            out.push_str(&format!("Status: {}\n\n", derived.tag()));
        }
    }
    out.push_str(&format!("{}\n\n", module.summary));
    out
}

/// Renders the `docs_src/stdlib/index.md` landing page listing
/// every module with its one-line summary.
#[must_use]
pub fn render_index_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Gossamer standard library\n\n");
    out.push_str(
        "One page per module. Source is `crates/gossamer-std/src/`; \
this index is regenerated from `manifest::ALL_MODULES` by \
`gos doc --emit-stdlib`.\n\n",
    );
    out.push_str(
        "For receiver methods on built-in types, see \
[`Methods by type`](../method_support.md).\n\n",
    );
    out.push_str("| Module | Summary |\n");
    out.push_str("|---|---|\n");
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut sorted: Vec<&StdModule> = ALL_MODULES.iter().collect();
    sorted.sort_by_key(|m| m.path);
    for m in sorted {
        if !seen.insert(m.path) {
            continue;
        }
        let slug = module_slug(m.path);
        out.push_str(&format!(
            "| [`{}`]({}.md) | {} |\n",
            m.path,
            slug,
            escape_table_cell(m.summary)
        ));
    }
    out.push('\n');
    out
}

/// Canonical slug for a module path - `std::http::router`
/// becomes `http_router`.
#[must_use]
pub fn module_slug(path: &str) -> String {
    path.strip_prefix("std::")
        .unwrap_or(path)
        .replace("::", "_")
}

/// Canonical slug for a language-feature path - `lang::if_let`
/// becomes `if_let`.
#[must_use]
pub fn language_slug(path: &str) -> String {
    path.strip_prefix("lang::")
        .unwrap_or(path)
        .replace("::", "_")
}

/// Renders one language-feature entry as a Markdown stub. Only experimental
/// features receive a lifecycle marker in the user-facing reference.
#[must_use]
pub fn render_language_markdown(entry: &super::feature_status::FeatureStatus) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("# `{}`\n\n", entry.path));
    if entry.status == super::feature_status::Status::Experimental {
        out.push_str("Status: experimental\n\n");
    }
    out.push_str(&format!("{}\n", entry.doc));
    out
}

/// Renders the `docs_src/language/index.md` landing page listing
/// every documented language feature and its one-line summary. Mirrors
/// `render_index_markdown` for the language surface.
#[must_use]
pub fn render_language_index_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Gossamer language reference\n\n");
    out.push_str(
        "One page per language feature. Source is \
`crates/gossamer-std/src/manifest/feature_status.rs`; this index \
is regenerated from `manifest::FEATURE_STATUS` by \
`gos doc --emit-stdlib`.\n\n",
    );
    out.push_str("| Feature | Summary |\n");
    out.push_str("|---|---|\n");
    // The trait catalog is not a feature-status entry - it is rendered from
    // the checker's own table - so the index names it explicitly rather than
    // leaving the one page a reader cannot reach from here.
    out.push_str(
        "| [Built-in traits](builtin_traits.md) | Every trait an `impl` header or a bound may name without declaring it. |\n",
    );
    for entry in super::feature_status::FEATURE_STATUS {
        if !entry.path.starts_with("lang::") {
            continue;
        }
        out.push_str(&format!(
            "| [`{}`]({}.md) | {} |\n",
            entry.path,
            language_slug(entry.path),
            escape_table_cell(entry.doc),
        ));
    }
    out
}

/// Returns every `(slug, markdown)` pair for the language-feature
/// docs site under `docs_src/language/`. Mirrors `render_all_docs`
/// for the language surface: the `index` page plus one page per
/// documented feature.
#[must_use]
pub fn render_all_language_docs() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    out.push(("index".to_string(), render_language_index_markdown()));
    for entry in super::feature_status::FEATURE_STATUS {
        if !entry.path.starts_with("lang::") {
            continue;
        }
        out.push((language_slug(entry.path), render_language_markdown(entry)));
    }
    out
}

/// Returns every `(slug, markdown)` pair for the docs site.
/// Includes the `index` page plus one page per module.
///
/// Multiple manifest entries sharing the same module path are
/// merged into one page with the union of their item lists. The
/// historical reason was a split `ENCODING_BINARY` /
/// `ENCODING_BINARY_FULL` pair; that's gone but the merge logic
/// is cheap and stays as a safety net for future additions.
#[must_use]
pub fn render_all_docs() -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    // Group items by module path, preserving insertion order.
    let mut order: Vec<&'static str> = Vec::new();
    let mut merged: BTreeMap<&'static str, (String, Vec<&'static StdItem>)> = BTreeMap::new();
    for m in ALL_MODULES {
        let entry = merged.entry(m.path).or_insert_with(|| {
            order.push(m.path);
            (m.summary.to_string(), Vec::new())
        });
        for item in m.items {
            // Dedupe by item name within the merged set.
            if !entry.1.iter().any(|i| i.name == item.name) {
                entry.1.push(item);
            }
        }
    }
    let mut out: Vec<(String, String)> = Vec::with_capacity(order.len() + 1);
    out.push(("index".to_string(), render_index_markdown()));
    for path in order {
        let (summary, items) = &merged[path];
        let synthetic = StdModule {
            path,
            summary: Box::leak(summary.clone().into_boxed_str()),
            items: Box::leak(
                items
                    .iter()
                    .map(|i| **i)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        };
        out.push((module_slug(path), render_module_markdown(&synthetic)));
    }
    out
}

/// Escapes Markdown table delimiters in prose while preserving inline-code
/// content. A pipe inside backticks is literal text, and escaping it there
/// leaks a visible backslash into rendered signatures such as `String | char`.
#[must_use]
pub fn escape_table_cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for ch in text.chars() {
        if ch == '`' {
            in_code = !in_code;
        }
        if ch == '|' && !in_code {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape_table_cell;

    #[test]
    fn table_cell_escaping_preserves_pipes_in_inline_code() {
        assert_eq!(
            escape_table_cell("Accepts `String | char` in prose | tables."),
            "Accepts `String | char` in prose \\| tables."
        );
    }

    #[test]
    fn module_pages_leave_item_tables_to_the_canonical_api_section() {
        let module = crate::registry::StdModule {
            path: "std::sample",
            summary: "Sample module.",
            items: &[],
        };
        let page = super::render_module_markdown(&module);
        assert!(!page.contains("Public items"));
        assert!(!page.contains("| Name | Kind | Description |"));
    }

    #[test]
    fn module_pages_do_not_advertise_shipped_status() {
        let module = crate::registry::StdModule {
            path: "std::process",
            summary: "Process support.",
            items: &[],
        };
        let page = super::render_module_markdown(&module);
        assert!(!page.contains("Status: shipped"));
    }
}
