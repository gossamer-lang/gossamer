//! Top-level source file, `use` declarations, and project/module targets.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_lex::{FileId, Span};

use crate::common::Ident;
use crate::items::{Attrs, Item};
use crate::node_id::NodeId;
use crate::printer::Printer;
use crate::stmt::Stmt;

/// A parsed `.gos` source file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceFile {
    /// File this source file was parsed from.
    pub file: FileId,
    /// File-level inner attributes (`#![...]`).
    pub attrs: Attrs,
    /// `use` declarations in source order.
    pub uses: Vec<UseDecl>,
    /// Items in source order.
    pub items: Vec<Item>,
    /// Bare statements parsed at file scope, in source order. Non-empty
    /// only for an entry file; `synthesize_entry_main` consumes them into
    /// the body of an implicit `fn main`. The sibling bundler keeps every
    /// non-entry file's contents inside a `mod { }` body, where statements
    /// are not accepted, so this list belongs solely to the entry file.
    #[serde(default)]
    pub top_level_stmts: Vec<Stmt>,
    /// First node id the parser did not use, so a post-parse pass (entry-main
    /// synthesis) can mint fresh ids for the wrapper nodes without colliding.
    #[serde(default)]
    pub next_node_id: u32,
    /// Argument labels written at a call site, keyed by the call
    /// expression's id.
    ///
    /// Kept beside the tree rather than inside `ExprKind::Call` so the
    /// call shape stays one thing everywhere it is built and matched.
    /// The named-argument pass reads this, rewrites each call into
    /// declared parameter order, and empties the map; nothing
    /// downstream of that pass sees a labelled argument.
    #[serde(default)]
    pub named_args: std::collections::HashMap<NodeId, Vec<NamedArg>>,
}

/// One `name: value` argument label at a call site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NamedArg {
    /// Position of the labelled argument in the call's `args` list, as written.
    pub index: usize,
    /// The parameter name the caller wrote.
    pub name: Ident,
    /// Span of the label, for diagnostics that name a parameter.
    pub span: Span,
}

impl SourceFile {
    /// Constructs a new source file with the given contents.
    #[must_use]
    pub fn new(file: FileId, uses: Vec<UseDecl>, items: Vec<Item>) -> Self {
        Self {
            file,
            attrs: Attrs::default(),
            uses,
            items,
            top_level_stmts: Vec::new(),
            next_node_id: 0,
            named_args: std::collections::HashMap::new(),
        }
    }
}

impl PartialEq for SourceFile {
    fn eq(&self, other: &Self) -> bool {
        self.attrs == other.attrs && self.uses == other.uses && self.items == other.items
    }
}

impl fmt::Display for SourceFile {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut printer = Printer::new();
        printer.print_source_file(self);
        out.write_str(&printer.finish())
    }
}

/// A single `use` declaration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UseDecl {
    /// Unique id within the enclosing source file.
    pub id: NodeId,
    /// Source range covered by this declaration.
    pub span: Span,
    /// What is being imported.
    pub target: UseTarget,
    /// Optional `as name` renaming of the imported target.
    pub alias: Option<Ident>,
    /// Optional `{ item1, item2 as x, ... }` brace-list after the target.
    pub list: Option<Vec<UseListEntry>>,
    /// Inline modules enclosing the declaration, outermost first, empty at
    /// the file's own level. A `use` written inside a `mod { }` body is
    /// hoisted to the file's imports, and a `self` / `super` / `crate` path
    /// is anchored at the module it was written in, so the anchor travels
    /// with it.
    pub module: Vec<String>,
    /// The `#[cfg(..)]` expression written before the declaration, which
    /// keeps it only where it holds (an optional dependency's import, a
    /// platform's).
    #[serde(default)]
    pub cfg: Option<String>,
}

impl UseDecl {
    /// Constructs a simple `use target` declaration with no alias and no brace list.
    #[must_use]
    pub fn simple(id: NodeId, span: Span, target: UseTarget) -> Self {
        Self {
            id,
            span,
            target,
            alias: None,
            list: None,
            module: Vec::new(),
            cfg: None,
        }
    }
}

impl PartialEq for UseDecl {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.alias == other.alias && self.list == other.list
    }
}

/// What a `use` declaration points at.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UseTarget {
    /// A bare module path within the current project.
    Module(ModulePath),
    /// A string-quoted project identifier, optionally followed by `::module_path`.
    Project {
        /// Project identifier as written in the string literal.
        id: String,
        /// Optional module path inside that project.
        module: Option<ModulePath>,
    },
}

/// A `::`-separated module path `a::b::c`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModulePath {
    /// Segments in order.
    pub segments: Vec<Ident>,
}

impl ModulePath {
    /// Constructs a module path from an iterator of segment names.
    #[must_use]
    pub fn from_names<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            segments: segments.into_iter().map(Ident::new).collect(),
        }
    }
}

/// One entry in a `use target::{ ... }` brace list. An entry may be a
/// multi-segment path (`use std::{encoding::json}`): `prefix` holds the
/// segments before the bound `name` (the final segment), so the full
/// import path is `target :: prefix :: name`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UseListEntry {
    /// Path segments before the bound name (empty for a single-segment
    /// entry like `env`).
    pub prefix: Vec<Ident>,
    /// Final segment of the entry, the name bound into scope.
    pub name: Ident,
    /// Optional `as rename`.
    pub alias: Option<Ident>,
}

impl UseListEntry {
    /// Constructs a single-segment entry with no rename.
    #[must_use]
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            prefix: Vec::new(),
            name: Ident::new(name),
            alias: None,
        }
    }

    /// Constructs a single-segment entry with `as rename`.
    #[must_use]
    pub fn aliased(name: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            prefix: Vec::new(),
            name: Ident::new(name),
            alias: Some(Ident::new(alias)),
        }
    }
}

#[cfg(test)]
mod source_file_tests {
    use super::*;
    use gossamer_lex::SourceMap;

    #[test]
    fn new_source_file_has_empty_top_level_stmts() {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos", String::new());
        let sf = SourceFile::new(file, Vec::new(), Vec::new());
        assert!(sf.top_level_stmts.is_empty());
        assert_eq!(sf.next_node_id, 0);
    }
}
