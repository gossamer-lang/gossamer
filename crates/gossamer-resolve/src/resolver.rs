//! The resolver walks a parsed [`SourceFile`] and produces a
//! [`Resolutions`] side table plus a list of [`ResolveDiagnostic`]s.

#![forbid(unsafe_code)]

use gossamer_ast::{
    ArrayExpr, Block, ClosureParam, EnumDecl, Expr, ExprKind, FieldPattern, FnDecl, FnParam,
    GenericArg, GenericParam, Generics, Ident, ImplDecl, ImplItem, Item, ItemKind, Literal,
    MatchArm, ModulePath, NodeId, PathExpr, Pattern, PatternKind, SelectArm, SelectOp, SourceFile,
    Stmt, StmtKind, StructBody, StructDecl, StructExprField, StructField, TraitBound, TraitDecl,
    TraitItem, TupleField, Type, TypeAliasDecl, TypeKind, TypePath, UseDecl, UseListEntry,
    UseTarget, Visibility, WhereClause,
};
use gossamer_lex::Span;

use crate::def_id::{DefId, DefIdGenerator, DefKind};
use crate::diagnostic::{ResolveDiagnostic, ResolveError};
use crate::resolutions::{Resolution, Resolutions};
use crate::scope::{Binding, ScopeStack};

/// Runs name resolution on a parsed source file and returns the resolved
/// side-table plus any diagnostics surfaced along the way.
#[must_use]
pub fn resolve_source_file(source: &SourceFile) -> (Resolutions, Vec<ResolveDiagnostic>) {
    let mut resolver = Resolver::new();
    resolver.run(source);
    (resolver.resolutions, resolver.diagnostics)
}

/// Module name a `path = "..."` dependency's source is inlined
/// under by the entry bundler, derived deterministically from the
/// dependency's project id (`example.com/my-lib` -> `my_lib`). The
/// resolver binds `use "id" as alias` to a module of this name when
/// one exists, so both sides must agree on the transform.
#[must_use]
pub fn project_dep_module_name(id: &str) -> String {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let mut out: String = tail
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// A `use "project-id" as alias` whose binding is deferred until
/// items are collected, so it can resolve to the bundler-inlined
/// dependency module instead of an opaque import.
struct DeferredProjectUse {
    alias: String,
    /// Package id as the `use` spelled it, so the module the bundler
    /// actually emitted for it can be found by id.
    project_id: String,
    module_name: String,
    use_id: NodeId,
    span: Span,
}

/// Where a definition was declared and how far it may be named from.
#[derive(Debug, Clone)]
struct ItemHome {
    /// Chain of inline modules the item was declared inside, outermost
    /// first. Empty for a crate-root item.
    module: Vec<String>,
    /// `pub` or absent, as written on the declaration.
    visibility: Visibility,
    /// Item name, for the diagnostic.
    name: String,
    /// Item shape, for the diagnostic.
    kind: DefKind,
}

/// True for names the compiler synthesizes rather than the user writing
/// them. The leading double underscore is reserved for such items across
/// the whole front end (autoderive wrappers, format intrinsics), and
/// their bodies stand in for code the user never spelled - source-level
/// visibility is not theirs to satisfy.
/// True when `resolution` names a module. A module lives in the type
/// namespace, which a single-segment value path falls back to, so a module
/// and one of its items sharing a name would otherwise resolve to the
/// module - a name the backends have no function for.
fn is_module_resolution(resolution: Resolution) -> bool {
    matches!(
        resolution,
        Resolution::Def {
            kind: DefKind::Mod,
            ..
        }
    )
}

fn is_compiler_generated(name: &str) -> bool {
    name.starts_with("__")
}

/// True for an item the autoderive pass spliced in, marked
/// `#[gos_synthesized]`. Such an item belongs to the type it completes
/// rather than to the module the splice landed in, so it names that type
/// regardless of where the user declared it.
fn is_synthesized_item(attrs: &gossamer_ast::Attrs) -> bool {
    attrs.outer.iter().any(|attr| {
        attr.path.segments.len() == 1 && attr.path.segments[0].name.name == "gos_synthesized"
    })
}

struct Resolver {
    resolutions: Resolutions,
    diagnostics: Vec<ResolveDiagnostic>,
    scopes: ScopeStack,
    /// Loops enclosing the expression being resolved, innermost last,
    /// each holding its label when it carries one. A `break` or
    /// `continue` needs a non-empty stack, and a labelled one needs a
    /// matching entry. Reset across a closure body, which is a separate
    /// function: a loop outside it is not a target.
    loops: Vec<Option<String>>,
    defs: DefIdGenerator,
    deferred_project_uses: Vec<DeferredProjectUse>,
    /// Path each imported name is bound to, so a repeated import of the
    /// same path is distinguished from two paths claiming one name.
    imported_targets: std::collections::HashMap<String, String>,
    /// alias -> inlined dependency module name, for `use "id" as
    /// alias` bindings that resolved to a bundled module. Qualified
    /// item paths are registered under the module's real name, so
    /// alias-headed paths rewrite through this map.
    project_alias_modules: std::collections::HashMap<String, String>,
    /// Per-inline-module bare-name scopes, keyed by the `mod` item's
    /// `NodeId`. Built during item collection; pushed onto the scope
    /// stack while the module's body resolves so bare references bind
    /// to the module's own items first, and so two modules may define
    /// the same name without a flat-namespace collision.
    module_scopes: std::collections::HashMap<NodeId, crate::scope::Scope>,
    /// Chain of enclosing inline modules during item collection.
    collect_mod_stack: Vec<NodeId>,
    /// Every module path this compilation unit declares, joined with
    /// `::` (`options`, `config`, `config::example`). A `use` may name
    /// one of these directly - the crate root is the implicit base, the
    /// same default Rust gives `use`.
    local_module_paths: std::collections::HashSet<String>,
    /// Every non-module item path this compilation unit declares, joined
    /// with `::` (`Colorize`, `config::Options`). A `use` rooted at the
    /// crate may name an item as well as the module holding it.
    local_item_paths: std::collections::HashSet<String>,
    /// Every name a `use` in this compilation unit binds. A relative path
    /// may name one: the modules a file declares and the names it imports
    /// share one namespace, and an import's own source is a package or a
    /// Rust binding this unit cannot see the items of.
    unit_import_names: std::collections::HashSet<String>,
    /// Declaring module and declared visibility of every item this unit
    /// defines, keyed by its [`DefId`]. Drives the `pub` check.
    item_homes: std::collections::HashMap<DefId, ItemHome>,
    /// Visibility of each `mod` this unit declares, keyed by its
    /// `::`-joined path. A path segment absent from this map belongs to
    /// no local module, so it places no constraint on reachability.
    module_visibility: std::collections::HashMap<String, Visibility>,
    /// Module whose body is being resolved, outermost segment first.
    /// Empty while resolving the crate root.
    current_module: Vec<String>,
    /// Inlined dependency packages, keyed by the module name the bundler
    /// gave them, valued by the project id they are published under. A
    /// path headed by one of these names requires the matching import.
    dependency_modules: std::collections::HashMap<String, String>,
    /// Dependency module names some `use "id"` in this file bound.
    imported_dependencies: std::collections::HashSet<String>,
    /// Head segment of every module-form `use` target in this file. A
    /// dependency's inlined module carries the package's normalized name, so
    /// a `use` naming it head-on is the import that states the provenance,
    /// whether it is written bare, aliased, or with a list.
    imported_module_heads: std::collections::HashSet<String>,
    /// Depth of enclosing compiler-synthesized items. Visibility is a
    /// property of source the user wrote, so checks pause inside them.
    synthesized_depth: usize,
    /// Every module-scoped item, under its bare name, for the autoderive
    /// bodies the pass splices at the unit root. Those bodies name the
    /// user's types without a module path and have no source position for a
    /// `use`, so they resolve against this index; source the user wrote
    /// never consults it and reaches a module's items through a path or an
    /// import.
    synthesized_scope: std::collections::HashMap<String, Binding>,
    /// Enum-variant names this unit declares more than once, mapped to
    /// the enums that declare them. A bare reference to one of these is
    /// ambiguous and must be written with its enum.
    ambiguous_variants: std::collections::HashMap<String, Vec<String>>,
}

impl Resolver {
    fn new() -> Self {
        Self {
            resolutions: Resolutions::new(),
            diagnostics: Vec::new(),
            scopes: ScopeStack::with_prelude(),
            loops: Vec::new(),
            defs: DefIdGenerator::new(),
            deferred_project_uses: Vec::new(),
            imported_targets: std::collections::HashMap::new(),
            project_alias_modules: std::collections::HashMap::new(),
            module_scopes: std::collections::HashMap::new(),
            collect_mod_stack: Vec::new(),
            local_module_paths: std::collections::HashSet::new(),
            local_item_paths: std::collections::HashSet::new(),
            unit_import_names: std::collections::HashSet::new(),
            item_homes: std::collections::HashMap::new(),
            module_visibility: std::collections::HashMap::new(),
            current_module: Vec::new(),
            dependency_modules: std::collections::HashMap::new(),
            imported_dependencies: std::collections::HashSet::new(),
            imported_module_heads: std::collections::HashSet::new(),
            synthesized_depth: 0,
            synthesized_scope: std::collections::HashMap::new(),
            ambiguous_variants: std::collections::HashMap::new(),
        }
    }

    fn run(&mut self, source: &SourceFile) {
        self.local_module_paths = collect_module_paths(&source.items);
        self.local_item_paths = collect_item_paths(&source.items);
        self.unit_import_names = collect_import_names(&source.uses);
        self.precollect_project_aliases(source);
        self.collect_imports(&source.uses);
        self.collect_items(&source.items);
        self.bind_project_imports();
        for item in &source.items {
            if !crate::cfg::item_is_active(&item.attrs) {
                continue;
            }
            self.resolve_item(item);
        }
    }

    /// Binds each deferred `use "project-id" as alias` to the inlined
    /// dependency module when the bundler provided one; otherwise the
    /// alias stays an opaque import (registry/git dependencies the
    /// entry bundler does not inline).
    fn bind_project_imports(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_project_uses);
        // The bundler names each inlined dependency module, honouring a
        // `module = "..."` manifest override, and stamps the package id on
        // it. Reading the name back from that stamp is what lets a quoted
        // `use "id"` find a module the id alone would not derive.
        let by_id: std::collections::HashMap<&str, &str> = self
            .dependency_modules
            .iter()
            .map(|(module, id)| (id.as_str(), module.as_str()))
            .collect();
        let deferred: Vec<DeferredProjectUse> = deferred
            .into_iter()
            .map(|mut du| {
                if let Some(module) = by_id.get(du.project_id.as_str()) {
                    du.module_name = (*module).to_string();
                }
                du
            })
            .collect();
        for du in deferred {
            let module_binding = self
                .scopes
                .module_mut()
                .lookup_type(&du.module_name)
                .filter(|b| {
                    matches!(
                        b.resolution,
                        crate::resolutions::Resolution::Def {
                            kind: DefKind::Mod,
                            ..
                        }
                    )
                });
            match module_binding {
                Some(binding) => {
                    let module = self.scopes.module_mut();
                    module.insert_type(&du.alias, binding);
                    module.insert_value(&du.alias, binding);
                    self.resolutions
                        .insert_project_alias(du.alias.clone(), du.module_name.clone());
                    self.project_alias_modules.insert(du.alias, du.module_name);
                }
                None => self.define_import(&du.alias, du.use_id, du.span, &du.module_name),
            }
        }
    }

    fn emit(&mut self, error: ResolveError, span: Span) {
        if error.is_about_parse_placeholder() {
            return;
        }
        // Captured here because the scope stack is unwound by the time the
        // diagnostic is rendered, and locals live nowhere else.
        let candidate = match &error {
            ResolveError::UnresolvedName { name } => self.closest_visible_name(name),
            ResolveError::UnknownModulePath { path } => self.closest_local_module_path(path),
            _ => None,
        };
        self.diagnostics
            .push(ResolveDiagnostic::new(error, span).with_candidate(candidate));
    }

    /// Closest module of this unit to a `crate::` / `super::` / `self::` path
    /// that named none, spelled back under the root the import was written
    /// with. `None` for a path anchored outside the unit, whose neighbours are
    /// the standard library's rather than this unit's.
    fn closest_local_module_path(&self, path: &str) -> Option<String> {
        if !crate::diagnostic::is_relative_path(path) {
            return None;
        }
        let (root, written) = path.split_once("::")?;
        let known = gossamer_diagnostics::suggest(
            written,
            self.local_module_paths.iter().map(String::as_str),
            2,
        )?;
        Some(format!("{root}::{known}"))
    }

    /// Closest name currently in scope to `name`, for a "did you mean" hint.
    fn closest_visible_name(&self, name: &str) -> Option<String> {
        // A `module::member` path under a known stdlib module fails on its
        // leaf, so the neighbours worth offering are that module's own
        // exports - spelled back the way the call site writes them.
        if let Some((module, member)) = name.split_once("::")
            && !member.contains("::")
            && crate::stdlib_exports::is_stdlib_module_name(module)
        {
            let members = crate::stdlib_exports::stdlib_module_item_names(module);
            return gossamer_diagnostics::suggest(member, members.iter().copied(), 2)
                .map(|hit| format!("{module}::{hit}"));
        }
        // Any other qualified path fails on its head segment; comparing the
        // whole path against bare names would only ever find noise.
        let target = name.split("::").next().unwrap_or(name);
        gossamer_diagnostics::suggest(target, self.scopes.visible_names(), 2).map(str::to_string)
    }

    fn alloc_def(&mut self, node: NodeId, kind: DefKind) -> DefId {
        let def = self.defs.next();
        self.resolutions.insert_definition(node, def, kind);
        def
    }

    /// Records each `use "project-id" as alias` against the module the
    /// bundler inlined that package under, before any other `use` is
    /// validated. A path rooted at the alias - `use alias::submodule` - is
    /// the same path as one rooted at the module name, so the head is
    /// respelled and both spellings reach the same items.
    fn precollect_project_aliases(&mut self, source: &SourceFile) {
        let mut by_id: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        for item in &source.items {
            if let ItemKind::Mod(decl) = &item.kind
                && let Some(id) = item
                    .attrs
                    .outer
                    .iter()
                    .find_map(|attr| attr.string_argument("dependency"))
            {
                by_id.entry(id).or_insert_with(|| decl.name.name.clone());
            }
        }
        for use_decl in &source.uses {
            let gossamer_ast::UseTarget::Project { id, .. } = &use_decl.target else {
                continue;
            };
            let Some(alias) = use_decl.alias.as_ref().map(|a| a.name.clone()) else {
                continue;
            };
            if let Some(module) = by_id.get(id.as_str())
                && alias != *module
            {
                self.project_alias_modules.insert(alias, module.clone());
            }
        }
    }

    /// `path` with a leading project alias replaced by the module it names.
    fn respell_alias_head(&self, path: &str) -> String {
        let (head, rest) = match path.split_once("::") {
            Some((head, rest)) => (head, Some(rest)),
            None => (path, None),
        };
        let Some(module) = self.project_alias_modules.get(head) else {
            return path.to_string();
        };
        match rest {
            Some(rest) => format!("{module}::{rest}"),
            None => module.clone(),
        }
    }

    fn collect_imports(&mut self, uses: &[UseDecl]) {
        for use_decl in uses {
            self.record_imported_module_head(use_decl);
            match &use_decl.list {
                Some(list) => self.register_use_list(use_decl, list),
                None => self.register_use_simple(use_decl),
            }
        }
    }

    /// Records the head segment of a module-form `use` target, which is the
    /// name a dependency's inlined module is reached by.
    fn record_imported_module_head(&mut self, use_decl: &UseDecl) {
        let gossamer_ast::UseTarget::Module(path) = &use_decl.target else {
            return;
        };
        if let Some(head) = path.segments.first() {
            self.imported_module_heads.insert(head.name.clone());
        }
    }

    /// Records `name` as an alias of the stdlib module `target` names, when the
    /// import binds it under a name other than the module's own. Paths the
    /// name heads are respelled to the module before any name-keyed dispatch,
    /// which keys stdlib items by the module path without its `std` root.
    fn record_std_module_alias(&mut self, name: &str, target: &str) {
        let Some(module) = target.strip_prefix("std::") else {
            return;
        };
        if !crate::stdlib_exports::is_stdlib_module_path_or_namespace(module) {
            return;
        }
        if module.rsplit("::").next() != Some(name) {
            self.resolutions
                .insert_module_alias(name.to_string(), module.to_string());
        }
    }

    fn register_use_simple(&mut self, use_decl: &UseDecl) {
        self.reject_invalid_use_path(use_decl);
        let name = use_decl.alias.as_ref().map_or_else(
            || tail_name(&use_decl.target),
            |alias| Some(alias.name.clone()),
        );
        let Some(name) = name else {
            return;
        };
        // A project import can resolve to a bundler-inlined dependency
        // module, which is only registered during item collection -
        // defer the binding until then.
        if let gossamer_ast::UseTarget::Project { id, .. } = &use_decl.target {
            self.imported_dependencies
                .insert(project_dep_module_name(id));
            self.deferred_project_uses.push(DeferredProjectUse {
                alias: name,
                project_id: id.clone(),
                module_name: project_dep_module_name(id),
                use_id: use_decl.id,
                span: use_decl.span,
            });
            return;
        }
        let target = self.respell_alias_head(&target_path_text(&use_decl.target));
        // A module of this unit reached through an import: its items are
        // registered under the module's own path, so the name the import
        // introduced has to be respelled before any name-keyed dispatch
        // sees it. Without the record a constant or a variant reached
        // through the import type-checks and is unbound at run time.
        // A relative path names a module by its path from the unit root, which
        // is the key the module's items registered under, so it is anchored
        // before the lookup: `use self::filter as f` names `engine::filter`.
        let named = self
            .anchored_local_module(&use_decl.module, &target)
            .unwrap_or_else(|| target.clone());
        if self.local_module_paths.contains(&named) && name != named {
            self.resolutions
                .insert_module_alias(name.clone(), named.clone());
        }
        self.record_std_module_alias(&name, &target);
        self.define_import(&name, use_decl.id, use_decl.span, &target);
    }

    /// Validates `use` module paths against the canonical module table. Stdlib
    /// imports must be rooted at `std::`; `use iter` is not accepted as an
    /// alias for `use std::iter`. Non-`std` multi-segment imports must name a
    /// registered external item, otherwise a typo such as `use stp::iter`
    /// would silently bind `iter`.
    fn reject_invalid_use_path(&mut self, use_decl: &UseDecl) {
        let gossamer_ast::UseTarget::Module(p) = &use_decl.target else {
            return;
        };
        if p.segments.len() == 1 {
            let name = p.segments[0].name.as_str();
            // A stdlib module has to be spelled from its root, and any
            // other bare name has to name something this file can reach:
            // a sibling module, a stdlib item, or an external item.
            // Binding a name nothing declares would hide the typo.
            if crate::stdlib_exports::is_stdlib_module_path_or_namespace(name)
                || !(self.local_module_paths.contains(name)
                    || crate::stdlib_exports::is_stdlib_item_name(name)
                    || crate::scope::PRELUDE_TYPES.contains(&name)
                    || crate::external::all_external_module_paths()
                        .iter()
                        .any(|path| path == name)
                    || crate::external::lookup_external_item(name).is_some())
            {
                self.emit(
                    ResolveError::UnknownModulePath {
                        path: name.to_string(),
                    },
                    use_decl.span,
                );
            }
            return;
        }
        if p.segments.len() < 2 {
            return;
        }
        if matches!(
            p.segments[0].name.as_str(),
            "self" | "super" | "crate" | "root"
        ) {
            self.reject_invalid_relative_use_path(use_decl, p);
            return;
        }
        if p.segments[0].name != "std" {
            let segments: Vec<&str> = p.segments.iter().map(|s| s.name.as_str()).collect();
            let respelled = self.respell_alias_head(&segments.join("::"));
            let respelled_segments: Vec<&str> = respelled.split("::").collect();
            if self.names_local_module(&respelled_segments) {
                return;
            }
            let joined = segments.join("::");
            if crate::external::lookup_external_item(&joined).is_none() {
                self.emit(
                    ResolveError::UnknownModulePath { path: joined },
                    use_decl.span,
                );
            }
            return;
        }
        let rest: Vec<&str> = p.segments[1..].iter().map(|s| s.name.as_str()).collect();
        let joined = rest.join("::");
        if let Some(last) = rest.last()
            && let Some(replacement) = crate::stdlib_exports::canonical_collection_name(last)
        {
            self.emit(
                ResolveError::RemovedStdItem {
                    path: format!("std::{joined}"),
                    replacement: replacement.to_string(),
                },
                use_decl.span,
            );
            return;
        }
        if crate::stdlib_exports::is_stdlib_module_path_or_namespace(&joined) {
            return;
        }
        if let Some((item, parent)) = rest.split_last().filter(|(_, parent)| !parent.is_empty()) {
            let parent = parent.join("::");
            if crate::stdlib_exports::is_stdlib_module_path_or_namespace(&parent) {
                // The module resolves, so the leaf names an item: the module's
                // own export set decides whether the import binds anything.
                if !crate::stdlib_exports::is_stdlib_item_path(&joined) {
                    self.emit(
                        ResolveError::UnknownStdItem {
                            name: (*item).to_string(),
                            module: parent,
                        },
                        use_decl.span,
                    );
                }
                return;
            }
        }
        self.emit(
            ResolveError::UnknownModulePath {
                path: format!("std::{joined}"),
            },
            use_decl.span,
        );
    }

    fn register_use_list(&mut self, use_decl: &UseDecl, list: &[UseListEntry]) {
        self.reject_invalid_use_list_path(use_decl, list);
        for entry in list {
            let imported = entry
                .alias
                .as_ref()
                .map_or_else(|| entry.name.name.clone(), |alias| alias.name.clone());
            let base = self.respell_alias_head(&target_path_text(&use_decl.target));
            // The full path is `target :: prefix :: name`, so a nested entry
            // (`use std::{encoding::json}`) carries its own segments between
            // the list's root and the bound name.
            let mut target = base;
            for segment in &entry.prefix {
                target.push_str("::");
                target.push_str(&segment.name);
            }
            target.push_str("::");
            target.push_str(&entry.name.name);
            // An entry naming a module of this unit is reached under the
            // module's own path, which is the key its items registered under,
            // so the name the entry introduced is respelled to it before any
            // name-keyed dispatch sees it.
            let named = self
                .anchored_local_module(&use_decl.module, &target)
                .unwrap_or_else(|| target.clone());
            if self.local_module_paths.contains(&named) && imported != named {
                self.resolutions
                    .insert_module_alias(imported.clone(), named.clone());
            }
            self.record_std_module_alias(&imported, &target);
            self.define_import(&imported, use_decl.id, use_decl.span, &target);
        }
    }

    fn reject_invalid_use_list_path(&mut self, use_decl: &UseDecl, list: &[UseListEntry]) {
        let gossamer_ast::UseTarget::Module(path) = &use_decl.target else {
            return;
        };
        let Some(head) = path.segments.first() else {
            return;
        };
        if crate::diagnostic::is_relative_path(&head.name) {
            self.reject_invalid_relative_use_list(use_decl, path, list);
            return;
        }
        if head.name != "std" {
            return;
        }
        let base_rest: Vec<&str> = path.segments[1..]
            .iter()
            .map(|segment| segment.name.as_str())
            .collect();
        for entry in list {
            if let Some(replacement) =
                crate::stdlib_exports::canonical_collection_name(entry.name.name.as_str())
            {
                let mut rest = base_rest.clone();
                rest.extend(entry.prefix.iter().map(|segment| segment.name.as_str()));
                rest.push(entry.name.name.as_str());
                self.emit(
                    ResolveError::RemovedStdItem {
                        path: format!("std::{}", rest.join("::")),
                        replacement: replacement.to_string(),
                    },
                    use_decl.span,
                );
            }
        }
        for entry in list {
            // A `self` entry names the module the list is rooted at, not an
            // item that module exports.
            if entry.name.name == "self" {
                continue;
            }
            let mut rest = base_rest.clone();
            rest.extend(entry.prefix.iter().map(|segment| segment.name.as_str()));
            rest.push(entry.name.name.as_str());
            let joined = rest.join("::");
            if crate::stdlib_exports::is_stdlib_module_path_or_namespace(&joined)
                || crate::stdlib_exports::is_stdlib_qualified(&joined)
            {
                continue;
            }
            let split = rest.split_last().filter(|(_, parent)| !parent.is_empty());
            if let Some((item, parent)) = split {
                let parent = parent.join("::");
                if crate::stdlib_exports::is_stdlib_module_path_or_namespace(&parent) {
                    // The module resolves, so the leaf names an item: the
                    // module's own export set decides whether the import
                    // binds anything.
                    if !crate::stdlib_exports::is_stdlib_item_path(&joined) {
                        self.emit(
                            ResolveError::UnknownStdItem {
                                name: (*item).to_string(),
                                module: parent,
                            },
                            use_decl.span,
                        );
                    }
                    continue;
                }
            }
            self.emit(
                ResolveError::UnknownModulePath {
                    path: format!("std::{joined}"),
                },
                use_decl.span,
            );
        }
    }

    /// `true` when `segments` walks into a module this unit declares.
    /// The last segment is the imported item, so any prefix that names a
    /// declared module makes the path local: `options::Colorize` and
    /// `config::example::options::Colorize` both start at the crate root.
    /// True when a `use` path names a module this unit declares, or an item
    /// of one. Only the whole path and its parent are accepted: matching any
    /// prefix would take `pkg::nowhere::Missing` for a real import on the
    /// strength of `pkg` alone, binding a name nothing declares.
    fn names_local_module(&self, segments: &[&str]) -> bool {
        if self.local_module_paths.contains(&segments.join("::")) {
            return true;
        }
        segments.len() > 1
            && self
                .local_module_paths
                .contains(&segments[..segments.len() - 1].join("::"))
    }

    /// Validates a `use` path rooted at `self`, `super`, `crate` or `root`.
    ///
    /// Such a path names something this unit declares or it names nothing, and
    /// binding a name nothing declares puts a module in scope that no module
    /// defines. That binding then stands in front of every path headed by it,
    /// so the use site resolves against the import, reports nothing, and leaves
    /// the failure to run time.
    fn reject_invalid_relative_use_path(
        &mut self,
        use_decl: &UseDecl,
        path: &gossamer_ast::ModulePath,
    ) {
        let segments: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
        let respelled = self.respell_alias_head(&segments.join("::"));
        let respelled: Vec<&str> = respelled.split("::").collect();
        let candidates = self.relative_candidates(&use_decl.module, &respelled);
        // A path that is nothing but its anchor names the module it was
        // written in, which is always there.
        if candidates.is_empty() {
            return;
        }
        if candidates
            .iter()
            .any(|path| self.names_local_declaration(path))
        {
            return;
        }
        self.emit(
            ResolveError::UnknownModulePath {
                path: segments.join("::"),
            },
            use_decl.span,
        );
    }

    /// Validates each entry of a `use` list rooted at `self`, `super`,
    /// `crate` or `root`. Every entry binds a name of its own, so each is the
    /// whole path the list's root and that entry's own segments spell.
    fn reject_invalid_relative_use_list(
        &mut self,
        use_decl: &UseDecl,
        path: &gossamer_ast::ModulePath,
        list: &[UseListEntry],
    ) {
        let base: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
        for entry in list {
            let mut segments = base.clone();
            segments.extend(entry.prefix.iter().map(|s| s.name.as_str()));
            // A `self` entry names the module the list is rooted at, which the
            // root itself has already been taken to name.
            if entry.name.name != "self" {
                segments.push(entry.name.name.as_str());
            }
            let respelled = self.respell_alias_head(&segments.join("::"));
            let respelled: Vec<&str> = respelled.split("::").collect();
            let candidates = self.relative_candidates(&use_decl.module, &respelled);
            if candidates.is_empty()
                || candidates
                    .iter()
                    .any(|path| self.names_local_declaration(path))
            {
                continue;
            }
            self.emit(
                ResolveError::UnknownModulePath {
                    path: segments.join("::"),
                },
                use_decl.span,
            );
        }
    }

    /// Paths a `self` / `super` / `crate` / `root` target could name, spelled
    /// from the unit root, most specific first.
    ///
    /// `module` is the chain of inline modules the import was written in, so a
    /// `self` / `super` chain lands on exactly one path. `crate` and `root`
    /// anchor at the root of the package the import belongs to, which is the
    /// unit root for the package being compiled and the dependency's own
    /// module for one the bundler embedded, so every enclosing prefix is a
    /// root it could mean.
    fn relative_candidates(&self, module: &[String], segments: &[&str]) -> Vec<String> {
        let anchor = segments
            .iter()
            .take_while(|s| matches!(**s, "self" | "super" | "crate" | "root"))
            .count();
        if anchor == segments.len() {
            return Vec::new();
        }
        let rest = segments[anchor..].join("::");
        let under = |prefix: &[String]| {
            if prefix.is_empty() {
                rest.clone()
            } else {
                format!("{}::{rest}", prefix.join("::"))
            }
        };
        if segments[..anchor]
            .iter()
            .any(|s| matches!(*s, "crate" | "root"))
        {
            return (0..=module.len())
                .rev()
                .map(|d| under(&module[..d]))
                .collect();
        }
        let supers = segments[..anchor].iter().filter(|s| **s == "super").count();
        vec![under(&module[..module.len().saturating_sub(supers)])]
    }

    /// The declared module a relative `use` target names, spelled from the unit
    /// root, which is the key the module's items register under.
    fn anchored_local_module(&self, module: &[String], target: &str) -> Option<String> {
        if !crate::diagnostic::is_relative_path(target) {
            return None;
        }
        let segments: Vec<&str> = target.split("::").collect();
        self.relative_candidates(module, &segments)
            .into_iter()
            .find(|path| self.local_module_paths.contains(path))
    }

    /// `true` when `path` names a module or an item this unit declares.
    ///
    /// Both sets are read off this unit's own syntax, so unlike a path into a
    /// package the leaf is checked too: `crate::engine::nowhere` names nothing
    /// even though `crate::engine` names a module.
    fn names_local_declaration(&self, path: &str) -> bool {
        if self.local_module_paths.contains(path) || self.local_item_paths.contains(path) {
            return true;
        }
        // The head may instead name something the unit imported - a package's
        // module, a Rust binding's - whose items live outside this unit. What
        // it exports is checked where that import is, so the path is taken on
        // the strength of its head, as a path into a package is.
        let head = path.split("::").next().unwrap_or(path);
        self.unit_import_names.contains(head)
            || crate::external::all_external_module_paths()
                .iter()
                .any(|known| known == head)
    }

    /// Reports an unresolved name, naming the rename when the name is a
    /// container spelling this release replaced. Only `use` declarations used
    /// to carry that hint, so a bare `HashSet<i64>` in a signature reported
    /// only that the name was missing.
    fn emit_unresolved_or_rename(&mut self, name: &str, span: Span) {
        // The reflection pass rewrites `typeInfo::<T>()` to a synthesized
        // function; when no such function exists the user wrote a type that
        // has nothing to reflect, and they should read about that rather
        // than about a name they never wrote.
        if let Some(reflected) = name.strip_prefix("__gos_typeinfo_") {
            let spelled = reflected.split("__").next().unwrap_or(reflected);
            self.emit(
                ResolveError::UnreflectableType {
                    name: spelled.to_string(),
                },
                span,
            );
            return;
        }
        if let Some(replacement) = crate::stdlib_exports::canonical_collection_name(name) {
            self.emit(
                ResolveError::RemovedStdItem {
                    path: name.to_string(),
                    replacement: replacement.to_string(),
                },
                span,
            );
            return;
        }
        // A name some module declares is reachable - it just is not in this
        // scope. Naming the module turns the failure into the one-line
        // import that fixes it.
        if let Some(module) = self.declaring_module_of(name) {
            self.emit(
                ResolveError::NotImported {
                    name: name.to_string(),
                    module,
                },
                span,
            );
            return;
        }
        self.emit(
            ResolveError::UnresolvedName {
                name: name.to_string(),
            },
            span,
        );
    }

    /// `::`-joined path of the module declaring `name`, when some module in
    /// this unit declares it as `pub`.
    fn declaring_module_of(&self, name: &str) -> Option<String> {
        let Resolution::Def { def, .. } = self.synthesized_scope.get(name)?.resolution else {
            return None;
        };
        let home = self.item_homes.get(&def)?;
        (!home.module.is_empty() && home.visibility.is_public()).then(|| home.module.join("::"))
    }

    fn define_import(&mut self, name: &str, use_id: NodeId, span: Span, target: &str) {
        let module = self.scopes.module_mut();
        // Allow imports to shadow prelude entries (Gossamer's
        // `use std::collections::{HashMap, ...}` style imports
        // re-introduce names already in the prelude). A
        // collision with a non-prelude binding stays an error.
        let existing_kind = module
            .lookup_type(name)
            .or_else(|| module.lookup_value(name))
            .map(|b| b.resolution);
        let is_prelude_only = match existing_kind {
            Some(crate::resolutions::Resolution::Import { use_id: existing })
                if existing == crate::scope::PRELUDE_SENTINEL =>
            {
                true
            }
            // Naming an item this unit already defines is an alias for it,
            // not a second definition: `use options::Colorize` spells out
            // where a bundled sibling module's name comes from.
            Some(crate::resolutions::Resolution::Def { .. }) => true,
            None => true,
            _ => false,
        };
        // A single-segment `use NAME` whose name is a module this unit
        // already declares - a bundled sibling, or an inlined dependency -
        // states which module the file's paths come from. The module's own
        // binding is the one those paths need, so the import records where
        // the name leads without standing in front of it.
        if name == target
            && matches!(
                existing_kind,
                Some(crate::resolutions::Resolution::Def {
                    kind: DefKind::Mod,
                    ..
                })
            )
        {
            self.imported_targets
                .insert(name.to_string(), target.to_string());
            return;
        }
        if !is_prelude_only {
            // Every `use` in a compilation unit lands in this one module
            // scope, including those written inside a `mod { }` body and
            // those injected alongside synthesized code. Binding one name
            // to one path twice leaves nothing ambiguous; only two paths
            // competing for the same name do. A `super::` / `crate::` /
            // `self::` path names an item of this same unit, so it spells
            // out where an existing binding comes from rather than
            // introducing a rival one.
            let names_this_unit =
                matches!(target.split("::").next(), Some("super" | "crate" | "self"));
            if !names_this_unit
                && self
                    .imported_targets
                    .get(name)
                    .is_none_or(|prev| prev != target)
            {
                self.emit(
                    ResolveError::DuplicateImport {
                        name: name.to_string(),
                    },
                    span,
                );
            }
            return;
        }
        self.imported_targets
            .insert(name.to_string(), target.to_string());
        let binding = Binding::import(use_id);
        module.insert_type(name, binding);
        self.scopes.module_mut().insert_value(name, binding);
    }

    fn collect_items(&mut self, items: &[Item]) {
        let mut module_path: Vec<String> = Vec::new();
        self.collect_items_in(items, &mut module_path);
    }

    fn collect_items_in(&mut self, items: &[Item], module_path: &mut Vec<String>) {
        for item in items {
            self.collect_item(item, module_path);
        }
    }

    fn collect_item(&mut self, item: &Item, module_path: &mut Vec<String>) {
        let vis = item.visibility;
        match &item.kind {
            ItemKind::Fn(decl) => {
                self.reject_reserved_call_name(&decl.name.name, "function", item.span);
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Fn,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::Struct(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Struct,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::Enum(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Enum,
                    item.span,
                    module_path,
                    vis,
                );
                self.register_enum_variants(decl, item.span, module_path, vis);
            }
            ItemKind::Trait(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Trait,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::TypeAlias(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::TypeAlias,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::Const(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Const,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::Static(decl) => {
                self.register_item_with_module(
                    item.id,
                    &decl.name,
                    DefKind::Static,
                    item.span,
                    module_path,
                    vis,
                );
            }
            ItemKind::Mod(decl) => self.collect_mod(item, decl, module_path, vis),
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => {}
        }
    }

    /// Registers a module declaration and collects the items of its
    /// inline body.
    fn collect_mod(
        &mut self,
        item: &Item,
        decl: &gossamer_ast::ModDecl,
        module_path: &mut Vec<String>,
        vis: Visibility,
    ) {
        let dependency_id = item
            .attrs
            .outer
            .iter()
            .find_map(|attr| attr.string_argument("dependency"));
        // Two packages whose names normalize to one module name both inline
        // under it, and every path headed by it would be ambiguous. The
        // first claim stands, and the second is reported as the collision it
        // is rather than as a duplicate declaration nobody wrote.
        if let Some(id) = dependency_id
            && let Some(first) = self.dependency_modules.get(&decl.name.name)
            && first != id
        {
            let first = first.clone();
            // The colliding declaration is the bundler's, not the user's, so
            // reporting at its span would point into generated text. What
            // the reader edits is the manifest; anchor the report at the
            // start of the file they opened.
            let anchor = Span::new(item.span.file, 0, 0);
            self.emit(
                ResolveError::DependencyModuleCollision {
                    module: decl.name.name.clone(),
                    first,
                    second: id.to_string(),
                },
                anchor,
            );
            return;
        }
        // A module declared inside another one belongs to its parent's
        // scope, so two sibling modules may each declare a `mod tests`
        // exactly as they may each declare a `fn helper`.
        self.register_item_with_module(
            item.id,
            &decl.name,
            DefKind::Mod,
            item.span,
            module_path,
            vis,
        );
        if let Some(id) = dependency_id {
            self.dependency_modules
                .entry(decl.name.name.clone())
                .or_insert_with(|| id.to_string());
        }
        module_path.push(decl.name.name.clone());
        self.module_visibility.insert(module_path.join("::"), vis);
        match &decl.body {
            gossamer_ast::ModBody::Inline(inner) => {
                self.module_scopes
                    .insert(item.id, crate::scope::Scope::default());
                self.collect_mod_stack.push(item.id);
                self.collect_items_in(inner, module_path);
                self.collect_mod_stack.pop();
            }
            // The bundler fills an out-of-line `mod name;` from the
            // project layout and blanks the declaration, so one that
            // survives to here names a module with no source behind it -
            // nothing would bind at run time.
            gossamer_ast::ModBody::External => self.emit(
                ResolveError::MissingModuleSource {
                    name: decl.name.name.clone(),
                },
                item.span,
            ),
        }
        module_path.pop();
    }

    /// Registers an enum's variants in the value namespace. Variants
    /// belong to their enum, so a sibling module declaring a variant of
    /// the same name is not a redeclaration: the bare name registers in
    /// the declaring module's own scope, and the crate-root slot is
    /// claimed by the first declaration so unqualified references keep
    /// working wherever exactly one enum offers the name.
    fn register_enum_variants(
        &mut self,
        decl: &EnumDecl,
        span: Span,
        module_path: &[String],
        visibility: Visibility,
    ) {
        for variant in &decl.variants {
            let def = self.defs.next();
            let binding = Binding::def(def, DefKind::Variant);
            self.item_homes.insert(
                def,
                ItemHome {
                    module: module_path.to_vec(),
                    // A variant is reached through its enum, so it is
                    // exactly as visible as the enum that declares it.
                    visibility,
                    name: variant.name.name.clone(),
                    kind: DefKind::Variant,
                },
            );
            match self.ambiguous_variants.entry(variant.name.name.clone()) {
                std::collections::hash_map::Entry::Occupied(mut owners) => {
                    owners.get_mut().push(decl.name.name.clone());
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(vec![decl.name.name.clone()]);
                }
            }
            let own_module_ok = match self.collect_mod_stack.last().copied() {
                Some(mod_id) => self
                    .module_scopes
                    .get_mut(&mod_id)
                    .is_none_or(|scope| scope.insert_value(&variant.name.name, binding)),
                None => true,
            };
            let root_ok = self
                .scopes
                .module_mut()
                .insert_value(&variant.name.name, binding);
            // Two enums in one module competing for a bare variant name
            // leave that name genuinely ambiguous, and every reference
            // to it would have to guess.
            if !own_module_ok || (module_path.is_empty() && !root_ok) {
                self.emit(
                    ResolveError::DuplicateItem {
                        name: variant.name.name.clone(),
                    },
                    span,
                );
            }
        }
    }

    fn register_item(
        &mut self,
        node: NodeId,
        name: &Ident,
        kind: DefKind,
        span: Span,
        module_path: &[String],
        visibility: Visibility,
    ) {
        let def = self.alloc_def(node, kind);
        self.record_home(def, name, kind, module_path, visibility);
        let binding = Binding::def(def, kind);
        let module = self.scopes.module_mut();
        let mut inserted_any = false;
        if kind.is_type_ns() {
            inserted_any |= module.insert_type(&name.name, binding);
        }
        if kind.is_value_ns() {
            inserted_any |= module.insert_value(&name.name, binding);
        }
        if !inserted_any && (kind.is_type_ns() || kind.is_value_ns()) {
            self.emit(
                ResolveError::DuplicateItem {
                    name: name.name.clone(),
                },
                span,
            );
        }
    }

    /// Like [`Self::register_item`] but also registers the item under
    /// its fully-qualified `mod1::mod2::name` path (when nested inside
    /// inline modules), so cross-module call-site lookups (`other::greet`)
    /// resolve directly to the function's `DefId` and the type checker
    /// can pick up the function's declared return type.
    fn register_item_with_module(
        &mut self,
        node: NodeId,
        name: &Ident,
        kind: DefKind,
        span: Span,
        module_path: &[String],
        visibility: Visibility,
    ) {
        if module_path.is_empty() {
            self.register_item(node, name, kind, span, module_path, visibility);
            return;
        }
        let def = self.alloc_def(node, kind);
        self.record_home(def, name, kind, module_path, visibility);
        let binding = Binding::def(def, kind);
        // Duplicate detection is per module: the bare name registers
        // in the module's OWN scope (pushed while its body resolves),
        // so two sibling modules may define the same name.
        let mut module_scope_ok = true;
        if let Some(mod_id) = self.collect_mod_stack.last().copied()
            && let Some(scope) = self.module_scopes.get_mut(&mod_id)
        {
            let mut inserted_any = false;
            if kind.is_type_ns() {
                inserted_any |= scope.insert_type(&name.name, binding);
            }
            if kind.is_value_ns() {
                inserted_any |= scope.insert_value(&name.name, binding);
            }
            if !inserted_any && (kind.is_type_ns() || kind.is_value_ns()) {
                module_scope_ok = false;
                self.emit(
                    ResolveError::DuplicateItem {
                        name: name.name.clone(),
                    },
                    span,
                );
            }
        }
        // The flat root registration keeps the historical bare-name
        // visibility (a top-level caller may reference a module item
        // unqualified when only one module defines it) only for items the
        // autoderive pass splices at the unit root: those synthesized bodies
        // name the user's types bare from outside the module that declares
        // them, and there is no source position for a `use`. Everything a
        // user writes reaches a module's items through a path or an import.
        let _ = module_scope_ok;
        if is_compiler_generated(&name.name) {
            let module = self.scopes.module_mut();
            if kind.is_type_ns() {
                let _ = module.insert_type(&name.name, binding);
            }
            if kind.is_value_ns() {
                let _ = module.insert_value(&name.name, binding);
            }
        } else {
            // An enum's runtime representation is chosen per enum (tagged
            // niche vs heap node) from tables still keyed by bare variant
            // name, so two modules declaring the same enum name can build a
            // value under one representation and match it under the other.
            // Structs carry no such choice and may share a name freely.
            self.synthesized_scope
                .entry(name.name.clone())
                .or_insert(binding);
        }
        // Also register `mod1::mod2::name` so cross-module callers
        // resolve directly to this def. Failure to insert here is a
        // benign duplicate - another sibling module declared the
        // same fully-qualified path.
        let qualified = format!("{}::{}", module_path.join("::"), name.name);
        let module = self.scopes.module_mut();
        if kind.is_type_ns() {
            let _ = module.insert_type(&qualified, binding);
        }
        if kind.is_value_ns() {
            let _ = module.insert_value(&qualified, binding);
        }
    }

    /// Records where `def` was declared so references to it can be
    /// checked against its declared visibility.
    fn record_home(
        &mut self,
        def: DefId,
        name: &Ident,
        kind: DefKind,
        module_path: &[String],
        visibility: Visibility,
    ) {
        self.item_homes.insert(
            def,
            ItemHome {
                module: module_path.to_vec(),
                visibility,
                name: name.name.clone(),
                kind,
            },
        );
    }

    /// Definition an imported `name` ultimately refers to, when the `use`
    /// target names an item of this same compilation unit.
    ///
    /// A `use` binds an opaque [`Resolution::Import`] because a target may
    /// live outside the unit; a local module's item is also registered under
    /// its `mod::name` path, so the import's target text finds the very
    /// definition it names and the reference can be checked like any other.
    fn imported_definition(&self, name: &str) -> Option<DefId> {
        let target = self.imported_targets.get(name)?;
        let qualified: Vec<&str> = target
            .split("::")
            .skip_while(|segment| matches!(*segment, "crate" | "root" | "self"))
            .collect();
        if qualified.len() < 2 {
            return None;
        }
        let joined = qualified.join("::");
        let binding = self
            .scopes
            .module_ref()
            .lookup_value(&joined)
            .or_else(|| self.scopes.module_ref().lookup_type(&joined))?;
        match binding.resolution {
            Resolution::Def { def, .. } => Some(def),
            _ => None,
        }
    }

    /// Reports `resolution` when it names an item the module currently
    /// being resolved is not allowed to reach. `name` is the name written
    /// at the reference, which is what follows a `use` to its target.
    /// Records the definition a `use`-imported name at `anchor` targets,
    /// so the type checker can read its signature through the import.
    fn record_import_def(&mut self, resolution: Resolution, name: &str, anchor: NodeId) {
        if !matches!(resolution, Resolution::Import { .. }) {
            return;
        }
        if let Some(def) = self.imported_definition(name) {
            self.resolutions.insert_import_def(anchor, def);
        }
    }

    fn check_visibility(&mut self, resolution: Resolution, name: Option<&str>, span: Span) {
        if self.synthesized_depth > 0 {
            return;
        }
        let def = match resolution {
            Resolution::Def { def, .. } => def,
            Resolution::Import { .. } => {
                let Some(def) = name.and_then(|name| self.imported_definition(name)) else {
                    return;
                };
                def
            }
            _ => return,
        };
        let Some(home) = self.item_homes.get(&def) else {
            return;
        };
        if self.is_reachable(home) {
            return;
        }
        // A `pub` item behind a private module is blocked by the module,
        // and that module is the one place a `pub` can unblock it.
        let error = match self.first_unnameable_module(&home.module) {
            Some(depth) => ResolveError::PrivateItem {
                name: home.module[depth - 1].clone(),
                module: home.module[..depth - 1].join("::"),
                kind: "module",
            },
            None => ResolveError::PrivateItem {
                name: home.name.clone(),
                module: home.module.join("::"),
                kind: home.kind.as_str(),
            },
        };
        self.emit(error, span);
    }

    /// Depth (1-based) of the outermost module along `path` that cannot
    /// be named from the module being resolved, or `None` when the whole
    /// path is nameable and the item's own visibility is what blocks it.
    fn first_unnameable_module(&self, path: &[String]) -> Option<usize> {
        (1..=path.len()).find(|&depth| !self.module_depth_is_nameable(path, depth))
    }

    /// An item is reachable when the module resolving it is the
    /// declaring module or one of its descendants, when the item is
    /// `pub`, or when it is `pub(package)` and both modules belong to the
    /// same package - and, in every case, when each module on the way in
    /// can be named from here.
    fn is_reachable(&self, home: &ItemHome) -> bool {
        let visible_here = self.current_module.starts_with(&home.module)
            || match home.visibility {
                Visibility::Public => true,
                Visibility::Package => self.same_package(&home.module),
                Visibility::Inherited => false,
            };
        visible_here && self.module_is_nameable(&home.module)
    }

    /// True when `home_module` and the module being resolved belong to the
    /// same package.
    fn same_package(&self, home_module: &[String]) -> bool {
        self.resolutions
            .same_package(home_module, &self.current_module)
    }

    /// True when every module along `path` is either declared in a
    /// module enclosing the current one - a private `mod` is nameable
    /// throughout the module that declares it - or is itself `pub`.
    fn module_is_nameable(&self, path: &[String]) -> bool {
        (1..=path.len()).all(|depth| self.module_depth_is_nameable(path, depth))
    }

    /// True when the module `path[..depth]` can be named from the module
    /// currently being resolved.
    fn module_depth_is_nameable(&self, path: &[String], depth: usize) -> bool {
        if self.current_module.starts_with(&path[..depth - 1]) {
            return true;
        }
        match self
            .module_visibility
            .get(&path[..depth].join("::"))
            .copied()
        {
            None | Some(Visibility::Public) => true,
            Some(Visibility::Package) => self.same_package(path),
            Some(Visibility::Inherited) => false,
        }
    }

    fn resolve_item(&mut self, item: &Item) {
        let synthesized = is_synthesized_item(&item.attrs)
            || match &item.kind {
                ItemKind::Fn(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Struct(decl) => is_compiler_generated(&decl.name.name),
                ItemKind::Enum(decl) => is_compiler_generated(&decl.name.name),
                _ => false,
            };
        if synthesized {
            self.synthesized_depth += 1;
        }
        self.resolve_item_inner(item);
        if synthesized {
            self.synthesized_depth -= 1;
        }
    }

    fn resolve_item_inner(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => self.resolve_fn(decl),
            ItemKind::Struct(decl) => self.resolve_struct(decl),
            ItemKind::Enum(decl) => self.resolve_enum(decl),
            ItemKind::Trait(decl) => self.resolve_trait(decl),
            ItemKind::Impl(decl) => self.resolve_impl(decl, item.span),
            ItemKind::TypeAlias(decl) => self.resolve_type_alias(decl),
            ItemKind::Const(decl) => {
                self.resolve_type(&decl.ty);
                self.resolve_expr(&decl.value);
            }
            ItemKind::Static(decl) => {
                self.resolve_type(&decl.ty);
                self.resolve_expr(&decl.value);
            }
            ItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    // Bare references inside the module bind to the
                    // module's own items first, then fall through to
                    // the flat root scope (prelude, top-level items).
                    let own_scope = self
                        .module_scopes
                        .get(&item.id)
                        .cloned()
                        .unwrap_or_default();
                    self.scopes.push_scope(own_scope);
                    self.current_module.push(decl.name.name.clone());
                    for nested in inner {
                        if !crate::cfg::item_is_active(&nested.attrs) {
                            continue;
                        }
                        self.resolve_item(nested);
                    }
                    self.current_module.pop();
                    self.scopes.pop();
                }
            }
            ItemKind::AttrItem(_) => {}
        }
    }

    fn resolve_fn(&mut self, decl: &FnDecl) {
        self.scopes.push();
        self.bind_generics(&decl.generics);
        for param in &decl.params {
            match param {
                FnParam::Typed { pattern, ty, .. } => {
                    self.resolve_type(ty);
                    self.bind_pattern(pattern);
                }
                FnParam::Receiver(_) => {
                    self.scopes
                        .top_mut()
                        .shadow_value("self", Binding::local(NodeId::DUMMY));
                }
            }
        }
        if let Some(ret) = &decl.ret {
            self.resolve_type(ret);
        }
        self.resolve_where_clause(&decl.where_clause);
        if let Some(body) = &decl.body {
            self.resolve_expr(body);
        }
        self.scopes.pop();
    }

    fn resolve_struct(&mut self, decl: &StructDecl) {
        self.scopes.push();
        self.bind_generics(&decl.generics);
        self.resolve_where_clause(&decl.where_clause);
        self.resolve_struct_body(&decl.body);
        self.scopes.pop();
    }

    fn resolve_enum(&mut self, decl: &EnumDecl) {
        self.scopes.push();
        self.bind_generics(&decl.generics);
        self.resolve_where_clause(&decl.where_clause);
        for variant in &decl.variants {
            self.resolve_struct_body(&variant.body);
            if let Some(disc) = &variant.discriminant {
                self.resolve_expr(disc);
            }
        }
        self.scopes.pop();
    }

    fn resolve_struct_body(&mut self, body: &StructBody) {
        match body {
            StructBody::Named(fields) => {
                for field in fields {
                    self.resolve_struct_field(field);
                }
            }
            StructBody::Tuple(fields) => {
                for field in fields {
                    self.resolve_tuple_field(field);
                }
            }
            StructBody::Unit => {}
        }
    }

    fn resolve_trait(&mut self, decl: &TraitDecl) {
        self.scopes.push();
        // Inside a trait body `Self` names the implementing type: a parameter
        // the trait is written against, reached as `Self::item` the way a
        // generic parameter's items are.
        let self_def = self.defs.next();
        self.scopes
            .top_mut()
            .insert_type("Self", Binding::def(self_def, DefKind::TypeParam));
        self.bind_generics(&decl.generics);
        for bound in &decl.supertraits {
            self.resolve_trait_bound(bound);
        }
        self.resolve_where_clause(&decl.where_clause);
        for item in &decl.items {
            self.resolve_trait_item(item);
        }
        self.scopes.pop();
    }

    fn resolve_trait_item(&mut self, item: &TraitItem) {
        match item {
            TraitItem::Fn(decl) => self.resolve_fn(decl),
            TraitItem::Type {
                bounds, default, ..
            } => {
                for bound in bounds {
                    self.resolve_trait_bound(bound);
                }
                if let Some(default) = default {
                    self.resolve_type(default);
                }
            }
            TraitItem::Const { ty, default, .. } => {
                self.resolve_type(ty);
                if let Some(default) = default {
                    self.resolve_expr(default);
                }
            }
        }
    }

    fn resolve_impl(&mut self, decl: &ImplDecl, span: Span) {
        self.scopes.push();
        self.bind_generics(&decl.generics);
        if let Some(bound) = &decl.trait_ref {
            // A trait a module keeps to itself cannot be implemented from
            // outside it, so this reference is checked like any other. A
            // trait path carries no span of its own; the `impl` header is
            // what the reader needs pointed at anyway.
            self.resolve_trait_bound_at(bound, Some(span));
        }
        self.resolve_type(&decl.self_ty);
        self.resolve_where_clause(&decl.where_clause);
        for item in &decl.items {
            self.resolve_impl_item(item);
        }
        self.scopes.pop();
    }

    fn resolve_impl_item(&mut self, item: &ImplItem) {
        match item {
            ImplItem::Fn(decl) => self.resolve_fn(decl),
            ImplItem::Type { ty, .. } => self.resolve_type(ty),
            ImplItem::Const { ty, value, .. } => {
                self.resolve_type(ty);
                self.resolve_expr(value);
            }
        }
    }

    fn resolve_type_alias(&mut self, decl: &TypeAliasDecl) {
        self.scopes.push();
        self.bind_generics(&decl.generics);
        self.resolve_type(&decl.ty);
        self.scopes.pop();
    }

    fn resolve_struct_field(&mut self, field: &StructField) {
        self.resolve_type(&field.ty);
    }

    fn resolve_tuple_field(&mut self, field: &TupleField) {
        self.resolve_type(&field.ty);
    }

    fn resolve_trait_bound(&mut self, bound: &TraitBound) {
        self.resolve_trait_bound_at(bound, None);
    }

    /// Resolves a trait bound, and when a span is supplied also checks that
    /// the trait is one this module may name.
    ///
    /// The visibility check is separate from path resolution here because a
    /// bound may legitimately name a trait the resolver has no entry for -
    /// the operator traits (`Add`, `Index`, `Neg`, `From`) are recognised
    /// later by the checker - and reporting those as unresolved names would
    /// reject `impl Add for Vec3`.
    fn resolve_trait_bound_at(&mut self, bound: &TraitBound, span: Option<Span>) {
        self.resolve_type_path_in(&bound.path, None, None);
        let (Some(span), Some(head)) = (span, bound.path.segments.first()) else {
            return;
        };
        let name = &head.name.name;
        let Some(binding) = self.scopes.lookup_type(name) else {
            return;
        };
        self.check_visibility(binding.resolution, Some(name), span);
    }

    fn resolve_where_clause(&mut self, clause: &WhereClause) {
        for predicate in &clause.predicates {
            self.resolve_type(&predicate.bounded);
            for bound in &predicate.bounds {
                self.resolve_trait_bound(bound);
            }
        }
    }

    fn bind_generics(&mut self, generics: &Generics) {
        for param in &generics.params {
            match param {
                GenericParam::Type { name, bounds, .. } => {
                    let def = self.defs.next();
                    let binding = Binding::def(def, DefKind::TypeParam);
                    self.scopes.top_mut().insert_type(&name.name, binding);
                    for bound in bounds {
                        self.resolve_trait_bound(bound);
                    }
                }
                GenericParam::Const { name, ty, default } => {
                    self.resolve_type(ty);
                    let def = self.defs.next();
                    let binding = Binding::def(def, DefKind::Const);
                    self.scopes.top_mut().insert_value(&name.name, binding);
                    if let Some(default) = default {
                        self.resolve_expr(default);
                    }
                }
                GenericParam::Lifetime { .. } => {}
            }
        }
    }

    fn resolve_type(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Unit | TypeKind::Never | TypeKind::Infer => {}
            TypeKind::Path(path) => self.resolve_type_path_in(path, Some(ty.id), Some(ty.span)),
            TypeKind::Tuple(elems) => {
                for elem in elems {
                    self.resolve_type(elem);
                }
            }
            TypeKind::Array { elem, len } => {
                self.resolve_type(elem);
                self.resolve_expr(len);
            }
            TypeKind::Slice(inner) | TypeKind::Ref { inner, .. } => self.resolve_type(inner),
            TypeKind::Fn { params, ret, .. } => {
                for param in params {
                    self.resolve_type(param);
                }
                if let Some(ret) = ret {
                    self.resolve_type(ret);
                }
            }
        }
    }

    fn resolve_pattern_path(&mut self, path: &TypePath, anchor: NodeId, span: Span) {
        let Some(head) = path.segments.first() else {
            return;
        };
        let name = &head.name.name;
        let resolution = self
            .scopes
            .lookup_value(name)
            .or_else(|| self.scopes.lookup_type(name))
            .map_or(Resolution::Err, |b| b.resolution);
        if matches!(resolution, Resolution::Err) {
            self.emit_unresolved_or_rename(name, span);
        }
        self.check_visibility(resolution, Some(name), span);
        if path.segments.len() == 1 {
            self.reject_ambiguous_variant(resolution, name, span);
        }
        self.resolutions.insert(anchor, resolution);
        for segment in &path.segments {
            self.resolve_generic_args(&segment.generics);
        }
    }

    fn resolve_type_path_in(
        &mut self,
        path: &TypePath,
        anchor: Option<NodeId>,
        span: Option<Span>,
    ) {
        let Some(head) = path.segments.first() else {
            return;
        };
        // A module-qualified user type (`util::Rec`) registers under its
        // joined `mod::Type` key in the type namespace. Re-anchor the
        // written path against the module being resolved so
        // `super::util::Rec`, `self::util::Rec`, and a bare
        // `util::Rec` all reach the same key, mirroring
        // `resolve_struct_literal`.
        let written: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        let effective: Vec<&str> = written
            .iter()
            .copied()
            .skip_while(|s| matches!(*s, "super" | "crate" | "self" | "root"))
            .collect();
        if effective.len() > 1 {
            if let Some(span) = span {
                self.check_dependency_import(&effective, span);
            }
            if let Some(resolution) = self.lookup_qualified_type(&written) {
                if let Some(span) = span {
                    self.check_visibility(resolution, None, span);
                }
                if let Some(anchor) = anchor {
                    self.resolutions.insert(anchor, resolution);
                }
                for segment in &path.segments {
                    self.resolve_generic_args(&segment.generics);
                }
                return;
            }
            // A `use "id" as alias` head names the dependency by the alias;
            // its items register under the module's real name, so the type
            // side rewrites the head the same way the value side does.
            if let Some(real) = self.project_alias_modules.get(effective[0]).cloned() {
                let mut rejoined: Vec<&str> = vec![real.as_str()];
                rejoined.extend_from_slice(&effective[1..]);
                if let Some(resolution) = self.lookup_qualified_type(&rejoined) {
                    if let Some(span) = span {
                        self.check_visibility(resolution, None, span);
                    }
                    if let Some(anchor) = anchor {
                        self.resolutions.insert(anchor, resolution);
                    }
                    for segment in &path.segments {
                        self.resolve_generic_args(&segment.generics);
                    }
                    return;
                }
            }
            // A `use pkg::child` head names a module by the last segment of
            // the path it was imported through, and its types register under
            // that whole path. The value side already respells such a head;
            // a type named in a signature has to reach the same declaration
            // or it becomes a second, unrelated one.
            if let Some(target) = self.imported_targets.get(effective[0]).cloned() {
                let mut rejoined: Vec<&str> = target.split("::").collect();
                rejoined.extend_from_slice(&effective[1..]);
                if let Some(resolution) = self.lookup_qualified_type(&rejoined) {
                    if let Some(span) = span {
                        self.check_visibility(resolution, None, span);
                    }
                    if let Some(anchor) = anchor {
                        self.resolutions.insert(anchor, resolution);
                    }
                    for segment in &path.segments {
                        self.resolve_generic_args(&segment.generics);
                    }
                    return;
                }
            }
        }
        // A qualifier that leaves one segment behind (`super::Point`,
        // `crate::Point`) names that type through the scope chain, the same
        // way the value side reaches `super::origin`. Looking the written
        // head up instead would search for a type called `super`.
        let name = match effective.first() {
            Some(first) if effective.len() == 1 => *first,
            _ => head.name.name.as_str(),
        };
        let resolution = if is_self_type(name) {
            Resolution::Err
        } else {
            self.scopes
                .lookup_type(name)
                .map(|binding| binding.resolution)
                .or_else(|| self.synthesized_lookup(name))
                .or_else(|| self.const_generic_in_type_position(name, path))
                .unwrap_or(Resolution::Err)
        };
        if let Some(span) = span {
            if matches!(resolution, Resolution::Err) && !is_self_type(name) {
                self.emit_unresolved_or_rename(name, span);
            }
            self.check_visibility(resolution, Some(name), span);
        }
        if let Some(anchor) = anchor {
            self.resolutions.insert(anchor, resolution);
        }
        for segment in &path.segments {
            self.resolve_generic_args(&segment.generics);
        }
    }

    /// A single-segment path written where a type goes that names a const
    /// binding in scope. A generic argument list cannot tell a type from a
    /// const by its spelling, so `Ring<N>` inside `impl<const N: usize>`
    /// reaches the const generic parameter here, and the checker decides
    /// whether a const is allowed where it stands.
    fn const_generic_in_type_position(&self, name: &str, path: &TypePath) -> Option<Resolution> {
        if path.segments.len() != 1 {
            return None;
        }
        self.scopes
            .lookup_value(name)
            .map(|binding| binding.resolution)
            .filter(|resolution| {
                matches!(
                    resolution,
                    Resolution::Def {
                        kind: DefKind::Const,
                        ..
                    }
                )
            })
    }

    /// Resolves the generic arguments on every segment of `path`.
    fn resolve_path_generic_args(&mut self, path: &PathExpr) {
        for segment in &path.segments {
            self.resolve_generic_args(&segment.generics);
        }
    }

    fn resolve_generic_args(&mut self, args: &[GenericArg]) {
        for arg in args {
            match arg {
                GenericArg::Type(ty) => self.resolve_type(ty),
                GenericArg::Const(expr) => self.resolve_expr(expr),
            }
        }
    }

    /// Resolves an assignment's left-hand side. A tuple there is a
    /// destructuring target, so each element resolves as its own place and a
    /// `_` element discards its value rather than naming a binding.
    fn resolve_assign_place(&mut self, place: &Expr) {
        match &place.kind {
            ExprKind::Tuple(elems) => {
                for elem in elems {
                    self.resolve_assign_place(elem);
                }
            }
            _ if place.is_wildcard() => {}
            _ => self.resolve_expr(place),
        }
    }

    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Literal(lit) => self.resolve_literal(lit),
            ExprKind::Path(path) => self.resolve_value_path(path, expr.id, expr.span),
            ExprKind::Call { callee, args } => self.resolve_call(callee, args),
            ExprKind::MethodCall {
                receiver,
                generics,
                args,
                ..
            } => self.resolve_method_call(receiver, generics, args),
            ExprKind::FieldAccess { receiver, .. }
            | ExprKind::Unary {
                operand: receiver, ..
            } => {
                self.resolve_expr(receiver);
            }
            ExprKind::Index { base, index } => {
                self.resolve_expr(base);
                self.resolve_expr(index);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs);
                self.resolve_expr(rhs);
            }
            ExprKind::Assign { place, value, .. } => {
                self.resolve_assign_place(place);
                self.resolve_expr(value);
            }
            ExprKind::Cast { value, ty } => {
                self.resolve_expr(value);
                self.resolve_type(ty);
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.resolve_if(condition, then_branch, else_branch.as_deref()),
            ExprKind::Match { scrutinee, arms } => self.resolve_match(scrutinee, arms),
            ExprKind::Loop { label, body } => self.resolve_loop_body(label.as_ref(), body),
            ExprKind::While {
                label,
                condition,
                body,
            } => {
                self.resolve_expr(condition);
                self.resolve_loop_body(label.as_ref(), body);
            }
            ExprKind::For {
                label,
                pattern,
                iter,
                body,
            } => self.resolve_for(label.as_ref(), pattern, iter, body),
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.resolve_block(block),
            ExprKind::Closure { params, ret, body } => {
                self.resolve_closure(params, ret.as_ref(), body);
            }
            ExprKind::Return(value) => self.resolve_optional_expr(value.as_deref()),
            ExprKind::Break { label, value } => {
                self.resolve_optional_expr(value.as_deref());
                self.check_loop_target("break", label.as_ref(), expr.span);
            }
            ExprKind::Continue { label } => {
                self.check_loop_target("continue", label.as_ref(), expr.span);
            }
            ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => {
                self.resolve_exprs(elems);
            }
            ExprKind::Struct {
                path, fields, base, ..
            } => {
                self.resolve_struct_expr(path, fields, base.as_deref(), expr.id, expr.span);
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => self.resolve_array_expr(arr),
            ExprKind::Range { start, end, .. } => {
                self.resolve_optional_expr(start.as_deref());
                self.resolve_optional_expr(end.as_deref());
            }
            ExprKind::Try(inner) => self.resolve_expr(inner),
            ExprKind::Select(arms) => {
                for arm in arms {
                    self.resolve_select_arm(arm);
                }
            }
            ExprKind::Error => {}
        }
    }

    fn resolve_call(&mut self, callee: &Expr, args: &[Expr]) {
        self.resolve_expr(callee);
        self.resolve_exprs(args);
    }

    fn resolve_method_call(&mut self, receiver: &Expr, generics: &[GenericArg], args: &[Expr]) {
        self.resolve_expr(receiver);
        self.resolve_generic_args(generics);
        self.resolve_exprs(args);
    }

    fn resolve_if(&mut self, condition: &Expr, then_branch: &Expr, else_branch: Option<&Expr>) {
        self.resolve_expr(condition);
        self.resolve_expr(then_branch);
        self.resolve_optional_expr(else_branch);
    }

    fn resolve_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) {
        self.resolve_expr(scrutinee);
        for arm in arms {
            self.resolve_match_arm(arm);
        }
    }

    fn resolve_for(
        &mut self,
        label: Option<&gossamer_ast::Label>,
        pattern: &Pattern,
        iter: &Expr,
        body: &Expr,
    ) {
        self.resolve_expr(iter);
        self.scopes.push();
        self.bind_pattern(pattern);
        self.resolve_loop_body(label, body);
        self.scopes.pop();
    }

    /// Resolves a loop body with that loop pushed as a `break` target.
    fn resolve_loop_body(&mut self, label: Option<&gossamer_ast::Label>, body: &Expr) {
        self.loops.push(label.map(|label| label.name.clone()));
        self.resolve_expr(body);
        self.loops.pop();
    }

    /// Reports a `break` / `continue` with no loop to leave, or one
    /// naming a label no enclosing loop carries.
    fn check_loop_target(
        &mut self,
        keyword: &str,
        label: Option<&gossamer_ast::Label>,
        span: Span,
    ) {
        match label {
            None if self.loops.is_empty() => self.emit(
                ResolveError::LoopControlOutsideLoop {
                    keyword: keyword.to_string(),
                },
                span,
            ),
            None => {}
            Some(label) => {
                if self
                    .loops
                    .iter()
                    .any(|enclosing| enclosing.as_deref() == Some(label.name.as_str()))
                {
                    return;
                }
                let in_scope = self.loops.iter().flatten().cloned().collect();
                self.emit(
                    ResolveError::UnknownLoopLabel {
                        keyword: keyword.to_string(),
                        label: label.name.clone(),
                        in_scope,
                    },
                    span,
                );
            }
        }
    }

    fn resolve_struct_expr(
        &mut self,
        path: &PathExpr,
        fields: &[StructExprField],
        base: Option<&Expr>,
        anchor: NodeId,
        span: Span,
    ) {
        self.resolve_struct_literal(path, anchor, span);
        for field in fields {
            self.resolve_struct_expr_field(field);
        }
        self.resolve_optional_expr(base);
    }

    fn resolve_exprs(&mut self, exprs: &[Expr]) {
        for expr in exprs {
            self.resolve_expr(expr);
        }
    }

    fn resolve_optional_expr(&mut self, expr: Option<&Expr>) {
        if let Some(expr) = expr {
            self.resolve_expr(expr);
        }
    }

    fn resolve_array_expr(&mut self, arr: &ArrayExpr) {
        match arr {
            ArrayExpr::List(elems) => {
                for elem in elems {
                    self.resolve_expr(elem);
                }
            }
            ArrayExpr::Repeat { value, count } => {
                self.resolve_expr(value);
                self.resolve_expr(count);
            }
        }
    }

    fn resolve_literal(&self, _lit: &Literal) {}

    /// Resolves a path whose head is a `use`-imported name by respelling
    /// that head as the import's target.
    ///
    /// An item registers under its module-qualified path, so
    /// `use money::Amount` followed by `Amount::new(..)` has to be looked up
    /// as `money::Amount::new`. Without this the call type-checks and is
    /// unbound at run time.
    fn resolve_through_import_head(&self, effective: &[&str]) -> Option<Resolution> {
        let head = effective.first()?;
        let mut rejoined = self.imported_targets.get(*head)?.clone();
        for seg in &effective[1..] {
            rejoined.push_str("::");
            rejoined.push_str(seg);
        }
        self.lookup_value_or_type(&rejoined)
    }

    /// Reports a path whose head names an inlined dependency package that
    /// this file never imported. The import is what states which package a
    /// `dep::item` path comes from, so the bare path is rejected outside the
    /// dependency's own body.
    fn check_dependency_import(&mut self, effective: &[&str], span: Span) {
        // The import states which package a path comes from, which only user
        // code has to say: a synthesized item's paths were written by the
        // compiler, which already knows where each type lives.
        if self.synthesized_depth > 0 {
            return;
        }
        let Some(head) = effective.first() else {
            return;
        };
        if self.imported_dependencies.contains(*head)
            || self.imported_module_heads.contains(*head)
            || self.project_alias_modules.contains_key(*head)
            || self.current_module.first().is_some_and(|m| m == *head)
        {
            return;
        }
        let Some(id) = self.dependency_modules.get(*head).cloned() else {
            return;
        };
        self.emit(
            ResolveError::DependencyNotImported {
                module: (*head).to_string(),
                id,
            },
            span,
        );
    }

    /// Reports `joined` when it names a std macro. A macro is spelled
    /// `println!(..)` and expands at parse time; nothing binds the path
    /// itself, so naming it in value position has nothing to call.
    /// Returns whether the path was reported.
    fn report_std_macro_as_value(&mut self, joined: &str, anchor: NodeId, span: Span) -> bool {
        let Some(name) = crate::stdlib_exports::stdlib_macro_named(joined) else {
            return false;
        };
        self.emit(
            ResolveError::StdMacroAsValue {
                path: joined.to_string(),
                name: name.to_string(),
            },
            span,
        );
        self.resolutions.insert(anchor, Resolution::Err);
        true
    }

    /// Whether `segments` names a member of a local module that the module
    /// does not declare.
    ///
    /// A local module's surface is fully known here - every item it declares
    /// was collected before any body resolved - so a lowercase member of one
    /// that still has no binding names nothing, and would fail at run time
    /// (GX0002) rather than at check time. A path through a TYPE the module
    /// declares (`util::Widget::new`) stays opaque: a module binding does not
    /// carry its types' associated surfaces.
    fn names_no_local_module_member(&self, segments: &[&str]) -> bool {
        let Some((last, prefix)) = segments.split_last() else {
            return false;
        };
        if prefix.is_empty() || !starts_lowercase(last) {
            return false;
        }
        self.local_module_path_for(prefix).is_some()
    }

    /// The local module `prefix` names, as written from where it is
    /// written.
    ///
    /// Three spellings reach one module: the path as written, an alias
    /// the file imported it under (`use sqlite_gos as sqlite`), and - for
    /// a path written INSIDE a module - a path relative to it. The last
    /// is what a bundled dependency's own source uses: `helper::f` inside
    /// `mod deppkg` names `deppkg::helper::f`, and a check that only
    /// looked for `helper` found nothing and left the member unresolved
    /// until run time.
    fn local_module_path_for(&self, prefix: &[&str]) -> Option<String> {
        let written = prefix.join("::");
        if self.local_module_paths.contains(&written) {
            return Some(written);
        }
        if let Some(target) = self.imported_targets.get(prefix[0]) {
            let mut rejoined = target.clone();
            for seg in &prefix[1..] {
                rejoined.push_str("::");
                rejoined.push_str(seg);
            }
            if self.local_module_paths.contains(&rejoined) {
                return Some(rejoined);
            }
        }
        // Innermost enclosing module first, since a nearer one shadows.
        for depth in (1..=self.current_module.len()).rev() {
            let mut anchored = self.current_module[..depth].join("::");
            anchored.push_str("::");
            anchored.push_str(&written);
            if self.local_module_paths.contains(&anchored) {
                return Some(anchored);
            }
        }
        None
    }

    /// True when the path's head names a local module and its next segment
    /// names nothing that module declares - no item, no child module. The
    /// rest of the path hangs off that segment, so a mis-spelled or absent
    /// member is a name error here rather than a `GX0002` at run time.
    fn names_no_member_of_local_module(&self, segments: &[&str]) -> bool {
        let [head, next, ..] = segments else {
            return false;
        };
        let Some(head_path) = self.local_module_path_for(&[*head]) else {
            return false;
        };
        let member = format!("{head_path}::{next}");
        if self.local_module_paths.contains(&member) {
            return false;
        }
        self.lookup_qualified_value_or_type(&[head_path.as_str(), next])
            .is_none()
    }

    fn resolve_value_path(&mut self, path: &PathExpr, anchor: NodeId, span: Span) {
        let Some(head) = path.segments.first() else {
            return;
        };
        // `super::name` / `crate::name` / `self::name` inside an inline
        // child module (`mod tests {}`, an auto-bundled sibling, etc.)
        // navigate from the module being resolved. Items register under
        // their path from the unit root, so the written path is
        // re-anchored by `qualified_candidates` before lookup;
        // `effective` is the prefix-free spelling used for reporting and
        // for the stdlib / dependency-alias checks below.
        let written: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        let effective: Vec<&str> = written
            .iter()
            .copied()
            .skip_while(|s| matches!(*s, "super" | "crate" | "self" | "root"))
            .collect();
        // Try the fully-qualified `mod1::mod2::name` form first so
        // sibling-module call sites (`other::greet`) resolve directly
        // to the function's [`DefId`] when the resolver registered
        // it via [`Self::register_item_with_module`]. The single
        // segment lookup is the fallback for plain paths and for
        // multi-segment paths whose head is something other than an
        // inline module (`fmt::println`, `http::Response::text` -
        // these stay opaque-by-head, matching the historical
        // tree-walker behaviour).
        if effective.len() > 1 {
            self.check_dependency_import(&effective, span);
            let joined = effective.join("::");
            // `HashSet::new()` names the pre-rename container: report the
            // rename rather than an opaque missing path.
            if let Some(head_name) = effective.first()
                && self.lookup_qualified_value_or_type(&written).is_none()
                && self.scopes.lookup_type(head_name).is_none()
                && crate::stdlib_exports::canonical_collection_name(head_name).is_some()
            {
                self.emit_unresolved_or_rename(head_name, span);
                self.resolutions.insert(anchor, Resolution::Err);
                self.resolve_path_generic_args(path);
                return;
            }
            if let Some(resolution) = self.lookup_qualified_value_or_type(&written) {
                self.check_visibility(resolution, None, span);
                self.resolutions.insert(anchor, resolution);
                self.resolve_path_generic_args(path);
                return;
            }
            if let Some(resolution) = self.resolve_through_import_head(&effective) {
                self.check_visibility(resolution, None, span);
                self.resolutions.insert(anchor, resolution);
                self.resolve_path_generic_args(path);
                return;
            }
            // A path headed by a `use "id" as alias` dependency
            // binding: items are registered under the module's real
            // name, so rewrite the head and retry. The inlined
            // module's surface is fully known, so a member that still
            // fails to resolve is a phantom - reject it here instead
            // of a runtime GX0002 / native undefined symbol.
            if let Some(real) = self.project_alias_modules.get(effective[0]).cloned() {
                let mut rejoined = real.clone();
                for seg in &effective[1..] {
                    rejoined.push_str("::");
                    rejoined.push_str(seg);
                }
                if let Some(resolution) = self.lookup_value_or_type(&rejoined) {
                    self.resolutions.insert(anchor, resolution);
                    self.resolve_path_generic_args(path);
                    return;
                }
                // An item reached through a type (`alias::Point::new`)
                // stays opaque-by-head, exactly as the unaliased
                // spelling does: a module binding does not carry its
                // types' associated surfaces, so absence there says
                // nothing about whether the item exists. Bind the head
                // to the real module and leave the rest to the
                // type-directed passes.
                if effective[1..].iter().any(|seg| !starts_lowercase(seg))
                    && let Some(binding) = self.scopes.lookup_type(&real)
                {
                    let resolution = binding.resolution;
                    self.resolutions.insert(anchor, resolution);
                    self.resolve_path_generic_args(path);
                    return;
                }
                self.emit(ResolveError::UnresolvedName { name: joined }, span);
                self.resolutions.insert(anchor, Resolution::Err);
                self.resolve_path_generic_args(path);
                return;
            }
            if self.names_no_local_module_member(&effective)
                || self.names_no_member_of_local_module(&effective)
            {
                self.emit(ResolveError::UnresolvedName { name: joined }, span);
                self.resolutions.insert(anchor, Resolution::Err);
                self.resolve_path_generic_args(path);
                return;
            }
            // Root-cause stdlib-member validation. The resolver is
            // opaque-by-head for stdlib paths - it has no per-module
            // export model, so `module::nonexistent` slipped through
            // `check` and only failed at runtime (GX0002). The
            // generated table in `stdlib_exports` lists every
            // `module::member` the runtime actually binds, so a value
            // path whose head is a known stdlib module but whose full
            // name resolves to no binding is definitively an unresolved
            // name. User-defined module members never reach here: they
            // resolve via the joined lookup above (a real binding) and
            // return, and autoderive call rewrites (`csrf::check`,
            // `errors::newf`, ...) run at parse time, so the resolver
            // never sees those names either. `path_shape_is_validated`
            // decides which path shapes the tables can answer for.
            if self.report_std_macro_as_value(&joined, anchor, span) {
                self.resolve_path_generic_args(path);
                return;
            }
            let stdlib_phantom = path_shape_is_validated(&effective)
                && !self.stdlib_member_resolves(&joined, &effective);
            if stdlib_phantom {
                self.emit(ResolveError::UnresolvedName { name: joined }, span);
                self.resolutions.insert(anchor, Resolution::Err);
                self.resolve_path_generic_args(path);
                return;
            }
        }
        let head_name = head.name.name.clone();
        let lookup_name = effective.first().copied().unwrap_or(head_name.as_str());
        // For multi-segment paths (`a::b::c`), the head can only be a
        // module / type / trait - never a local value binding. A
        // local variable that happens to share a module's name (a
        // common shadow, e.g. `let mut provider = "";` plus
        // `mod provider`) must not capture `provider::xxx` paths.
        // The flat lookup-value-then-type fallback below would
        // otherwise resolve the head to the local binding, and the
        // VM tier would then dispatch the call as a method-on-string,
        // silently returning Unit.
        let resolution = if effective.len() > 1 {
            self.scopes.lookup_type(lookup_name).map(|b| b.resolution)
        } else {
            self.lookup_value_or_type(lookup_name)
                .filter(|resolution| !is_module_resolution(*resolution))
        };
        let resolution = resolution
            .or_else(|| self.synthesized_lookup(lookup_name))
            .unwrap_or_else(|| {
                self.emit_unresolved_or_rename(lookup_name, span);
                Resolution::Err
            });
        self.check_visibility(resolution, Some(lookup_name), span);
        self.record_import_def(resolution, lookup_name, anchor);
        if effective.len() == 1 {
            self.reject_ambiguous_variant(resolution, lookup_name, span);
        }
        self.resolutions.insert(anchor, resolution);
        self.resolve_path_generic_args(path);
    }

    /// Rejects a bare reference to an enum-variant name that more than
    /// one enum in this unit declares. Downstream dispatch identifies a
    /// variant by name, so the enum has to be written out.
    fn reject_ambiguous_variant(&mut self, resolution: Resolution, name: &str, span: Span) {
        if !matches!(
            resolution,
            Resolution::Def {
                kind: DefKind::Variant,
                ..
            }
        ) {
            return;
        }
        let Some(owners) = self.ambiguous_variants.get(name) else {
            return;
        };
        if owners.len() < 2 {
            return;
        }
        let error = ResolveError::AmbiguousVariant {
            name: name.to_string(),
            enums: owners.clone(),
        };
        self.emit(error, span);
    }

    /// True when a `module::member` (or `module::submodule::member`)
    /// stdlib value path names something the runtime actually binds.
    /// The export table is keyed inconsistently: some nested modules
    /// list the full path (`math::rand::seed`), others the binding
    /// spelling (`url::parse` for `net::url::parse`), and the
    /// whole-module `json` lowering lists `json::get` for
    /// `encoding::json::get`. A three-segment path is therefore known
    /// when its full spelling is bound, or when `head::sub` is a real
    /// module path and the `sub::member` binding spelling is bound.
    fn stdlib_member_resolves(&self, joined: &str, effective: &[&str]) -> bool {
        if crate::stdlib_exports::is_stdlib_qualified(joined) {
            return true;
        }
        if let [head, sub, member] = effective {
            let parent = format!("{head}::{sub}");
            let binding_member = format!("{sub}::{member}");
            if crate::stdlib_exports::is_stdlib_module_path_or_namespace(&parent)
                && crate::stdlib_exports::is_stdlib_qualified(&binding_member)
            {
                return true;
            }
        }
        false
    }

    fn resolve_struct_literal(&mut self, path: &PathExpr, anchor: NodeId, span: Span) {
        let Some(head) = path.segments.first() else {
            return;
        };
        // A struct literal written inside an inline child module
        // (`super::P { .. }` in a `#[cfg(test)] mod tests {}`) names a
        // type through the module tree, the same way
        // `resolve_value_path` resolves a `super::foo()` call. The head
        // of the prefix-free path is the type name (`super::P` -> `P`,
        // `Shape::Rect` -> `Shape`).
        let written: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        let effective: Vec<&str> = written
            .iter()
            .copied()
            .skip_while(|s| matches!(*s, "super" | "crate" | "self" | "root"))
            .collect();
        let lookup_name = effective
            .first()
            .copied()
            .unwrap_or(head.name.name.as_str());
        // A module-qualified type (`other::Widget { .. }`) registers
        // under its joined name; prefer that, then fall back to the bare
        // head (covers enum struct-variant literals like
        // `Shape::Rect { .. }`, whose head is the enum type).
        let joined = (effective.len() > 1)
            .then(|| self.lookup_qualified_type(&written))
            .flatten();
        let resolution = joined
            .or_else(|| self.scopes.lookup_type(lookup_name).map(|b| b.resolution))
            .or_else(|| self.synthesized_lookup(lookup_name))
            .unwrap_or_else(|| {
                self.emit_unresolved_or_rename(lookup_name, span);
                Resolution::Err
            });
        self.check_visibility(resolution, Some(lookup_name), span);
        self.record_import_def(resolution, lookup_name, anchor);
        self.resolutions.insert(anchor, resolution);
        for segment in &path.segments {
            self.resolve_generic_args(&segment.generics);
        }
    }

    /// Fully-qualified keys to try for `segments`, most specific first.
    /// Items register under their path from the unit root, so a path
    /// written inside a module has to be re-anchored before lookup: a
    /// leading `self` / `super` chain navigates from the module being
    /// resolved, `crate` / `root` anchors at the unit root, and an
    /// unprefixed path is tried relative to the current module before
    /// the root-level key.
    fn qualified_candidates(&self, segments: &[&str]) -> Vec<String> {
        let mut supers = 0usize;
        let mut rooted = false;
        let mut rest = segments;
        while let Some((head, tail)) = rest.split_first() {
            match *head {
                "self" => {}
                "super" => supers += 1,
                "crate" | "root" => rooted = true,
                _ => break,
            }
            rest = tail;
        }
        if rest.is_empty() {
            return Vec::new();
        }
        let absolute = rest.join("::");
        if rooted {
            return vec![absolute];
        }
        let depth = self.current_module.len().saturating_sub(supers);
        let mut out = Vec::new();
        // Walk outward from the enclosing module, so a path written in one
        // module reaches a sibling module's item (`model::Cell` from inside
        // `engine`, both under the same package) as well as its own child's.
        // Innermost first, matching how a name resolves.
        for level in (1..=depth).rev() {
            out.push(format!(
                "{}::{absolute}",
                self.current_module[..level].join("::")
            ));
        }
        out.push(absolute);
        out
    }

    /// Bare-name lookup for a body the autoderive pass spliced at the unit
    /// root: those bodies name module-scoped types without a path. Returns
    /// `None` in source the user wrote.
    fn synthesized_lookup(&self, name: &str) -> Option<Resolution> {
        (self.synthesized_depth > 0)
            .then(|| self.synthesized_scope.get(name).map(|b| b.resolution))
            .flatten()
    }

    /// First of [`Self::qualified_candidates`] that names a binding.
    fn lookup_qualified_value_or_type(&self, segments: &[&str]) -> Option<Resolution> {
        self.qualified_candidates(segments)
            .iter()
            .find_map(|key| self.lookup_value_or_type(key))
    }

    /// First of [`Self::qualified_candidates`] that names a type.
    fn lookup_qualified_type(&self, segments: &[&str]) -> Option<Resolution> {
        self.qualified_candidates(segments)
            .iter()
            .find_map(|key| self.scopes.lookup_type(key).map(|b| b.resolution))
    }

    fn lookup_value_or_type(&self, name: &str) -> Option<Resolution> {
        if let Some(binding) = self.scopes.lookup_value(name) {
            return Some(binding.resolution);
        }
        self.scopes.lookup_type(name).map(|b| b.resolution)
    }

    fn resolve_struct_expr_field(&mut self, field: &StructExprField) {
        if let Some(value) = &field.value {
            self.resolve_expr(value);
        }
    }

    fn resolve_block(&mut self, block: &Block) {
        self.scopes.push();
        for stmt in &block.stmts {
            self.resolve_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.resolve_expr(tail);
        }
        self.scopes.pop();
    }

    fn resolve_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                if let Some(ty) = ty {
                    self.resolve_type(ty);
                }
                if let Some(init) = init {
                    self.resolve_expr(init);
                }
                self.bind_pattern(pattern);
            }
            StmtKind::Expr { expr, .. } => self.resolve_expr(expr),
            StmtKind::Item(item) => {
                self.collect_item_nested(item);
                self.resolve_item(item);
            }
            StmtKind::Defer(inner) => self.resolve_expr(inner),
        }
    }

    fn collect_item_nested(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                let def = self.alloc_def(item.id, DefKind::Fn);
                self.scopes
                    .top_mut()
                    .insert_value(&decl.name.name, Binding::def(def, DefKind::Fn));
            }
            ItemKind::Const(decl) => {
                let def = self.alloc_def(item.id, DefKind::Const);
                self.scopes
                    .top_mut()
                    .insert_value(&decl.name.name, Binding::def(def, DefKind::Const));
            }
            ItemKind::Static(decl) => {
                let def = self.alloc_def(item.id, DefKind::Static);
                self.scopes
                    .top_mut()
                    .insert_value(&decl.name.name, Binding::def(def, DefKind::Static));
            }
            ItemKind::Struct(decl) => {
                let def = self.alloc_def(item.id, DefKind::Struct);
                self.scopes
                    .top_mut()
                    .insert_type(&decl.name.name, Binding::def(def, DefKind::Struct));
            }
            ItemKind::Enum(decl) => {
                let def = self.alloc_def(item.id, DefKind::Enum);
                self.scopes
                    .top_mut()
                    .insert_type(&decl.name.name, Binding::def(def, DefKind::Enum));
            }
            ItemKind::TypeAlias(decl) => {
                let def = self.alloc_def(item.id, DefKind::TypeAlias);
                self.scopes
                    .top_mut()
                    .insert_type(&decl.name.name, Binding::def(def, DefKind::TypeAlias));
            }
            ItemKind::Trait(decl) => {
                let def = self.alloc_def(item.id, DefKind::Trait);
                self.scopes
                    .top_mut()
                    .insert_type(&decl.name.name, Binding::def(def, DefKind::Trait));
            }
            ItemKind::Mod(decl) => {
                let def = self.alloc_def(item.id, DefKind::Mod);
                self.scopes
                    .top_mut()
                    .insert_type(&decl.name.name, Binding::def(def, DefKind::Mod));
            }
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => {}
        }
    }

    fn resolve_closure(&mut self, params: &[ClosureParam], ret: Option<&Type>, body: &Expr) {
        let outer_loops = std::mem::take(&mut self.loops);
        self.scopes.push();
        for param in params {
            if let Some(ty) = &param.ty {
                self.resolve_type(ty);
            }
            self.bind_pattern(&param.pattern);
        }
        if let Some(ret) = ret {
            self.resolve_type(ret);
        }
        self.resolve_expr(body);
        self.scopes.pop();
        self.loops = outer_loops;
    }

    fn resolve_match_arm(&mut self, arm: &MatchArm) {
        self.scopes.push();
        self.bind_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.resolve_expr(guard);
        }
        self.resolve_expr(&arm.body);
        self.scopes.pop();
    }

    fn resolve_select_arm(&mut self, arm: &SelectArm) {
        self.scopes.push();
        match &arm.op {
            SelectOp::Recv { pattern, channel } => {
                self.resolve_expr(channel);
                self.bind_pattern(pattern);
            }
            SelectOp::Send { channel, value } => {
                self.resolve_expr(channel);
                self.resolve_expr(value);
            }
            SelectOp::Default => {}
        }
        self.resolve_expr(&arm.body);
        self.scopes.pop();
    }

    /// Rejects a declaration under a compiler-known call name: the parser
    /// expands the call where it is written, so the declaration could
    /// never be reached.
    fn reject_reserved_call_name(&mut self, name: &str, kind: &'static str, span: Span) {
        if gossamer_parse::builtin_macros::is_reserved_call_name(name) {
            self.emit(
                ResolveError::ReservedCallName {
                    name: name.to_string(),
                    kind,
                },
                span,
            );
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Rest
            | PatternKind::Range { .. }
            | PatternKind::Error => {}
            PatternKind::Ident {
                name, subpattern, ..
            } => {
                self.reject_reserved_call_name(&name.name, "binding", pattern.span);
                self.scopes
                    .top_mut()
                    .shadow_value(&name.name, Binding::local(pattern.id));
                if let Some(subpattern) = subpattern {
                    self.bind_pattern(subpattern);
                }
            }
            PatternKind::Path(path) => {
                self.resolve_pattern_path(path, pattern.id, pattern.span);
            }
            PatternKind::Tuple(parts) => {
                for part in parts {
                    self.bind_pattern(part);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix {
                    self.bind_pattern(part);
                }
                if let Some(rest) = rest {
                    self.bind_pattern(rest);
                }
                for part in suffix {
                    self.bind_pattern(part);
                }
            }
            PatternKind::Struct { path, fields, .. } => {
                self.resolve_pattern_path(path, pattern.id, pattern.span);
                for field in fields {
                    self.bind_field_pattern(field);
                }
            }
            PatternKind::TupleStruct { path, elems } => {
                self.resolve_pattern_path(path, pattern.id, pattern.span);
                for elem in elems {
                    self.bind_pattern(elem);
                }
            }
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.bind_pattern(alt);
                }
            }
            PatternKind::Ref { inner, .. } => self.bind_pattern(inner),
        }
    }

    fn bind_field_pattern(&mut self, field: &FieldPattern) {
        match &field.pattern {
            Some(pattern) => self.bind_pattern(pattern),
            None => {
                self.scopes
                    .top_mut()
                    .shadow_value(&field.name.name, Binding::local(NodeId::DUMMY));
            }
        }
    }
}

/// Every `mod` path declared under `items`, joined with `::`. Nested
/// modules contribute their full path, so a `use` can name any level.
fn collect_module_paths(items: &[gossamer_ast::Item]) -> std::collections::HashSet<String> {
    fn walk(
        items: &[gossamer_ast::Item],
        prefix: &str,
        out: &mut std::collections::HashSet<String>,
    ) {
        for item in items {
            let ItemKind::Mod(decl) = &item.kind else {
                continue;
            };
            let path = if prefix.is_empty() {
                decl.name.name.clone()
            } else {
                format!("{prefix}::{}", decl.name.name)
            };
            if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                walk(inner, &path, out);
            }
            out.insert(path);
        }
    }

    let mut out = std::collections::HashSet::new();
    walk(items, "", &mut out);
    out
}

/// Every non-module item this unit declares, keyed by its `::`-joined path
/// from the unit root. An enum's variants are included: a `use` may name one.
fn collect_item_paths(items: &[gossamer_ast::Item]) -> std::collections::HashSet<String> {
    fn qualify(prefix: &str, name: &str) -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}::{name}")
        }
    }

    fn walk(
        items: &[gossamer_ast::Item],
        prefix: &str,
        out: &mut std::collections::HashSet<String>,
    ) {
        for item in items {
            let name = match &item.kind {
                ItemKind::Fn(decl) => &decl.name.name,
                ItemKind::Struct(decl) => &decl.name.name,
                ItemKind::Trait(decl) => &decl.name.name,
                ItemKind::TypeAlias(decl) => &decl.name.name,
                ItemKind::Const(decl) => &decl.name.name,
                ItemKind::Static(decl) => &decl.name.name,
                ItemKind::Enum(decl) => {
                    let path = qualify(prefix, &decl.name.name);
                    for variant in &decl.variants {
                        out.insert(qualify(&path, &variant.name.name));
                    }
                    out.insert(path);
                    continue;
                }
                ItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        walk(inner, &qualify(prefix, &decl.name.name), out);
                    }
                    continue;
                }
                ItemKind::Impl(decl) => {
                    // An associated item is named through its type, which is
                    // the only path a `use` could reach it by.
                    let gossamer_ast::TypeKind::Path(ty) = &decl.self_ty.kind else {
                        continue;
                    };
                    let Some(last) = ty.segments.last() else {
                        continue;
                    };
                    let path = qualify(prefix, &last.name.name);
                    for assoc in &decl.items {
                        match assoc {
                            gossamer_ast::ImplItem::Fn(f) => {
                                out.insert(qualify(&path, &f.name.name));
                            }
                            gossamer_ast::ImplItem::Type { name, .. }
                            | gossamer_ast::ImplItem::Const { name, .. } => {
                                out.insert(qualify(&path, &name.name));
                            }
                        }
                    }
                    continue;
                }
                ItemKind::AttrItem(_) => continue,
            };
            out.insert(qualify(prefix, name));
        }
    }

    let mut out = std::collections::HashSet::new();
    walk(items, "", &mut out);
    out
}

/// Every name the unit's `use` declarations bind from outside it, renames
/// included. A `mod { }` body's imports are hoisted here, so this is the whole
/// of what any module of the unit reaches by import.
///
/// A relative import is left out: it names something inside the unit, so it is
/// not a source of names the unit cannot see, and counting it would let a
/// `use crate::missing` stand as its own justification.
fn collect_import_names(uses: &[UseDecl]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for use_decl in uses {
        if let gossamer_ast::UseTarget::Module(path) = &use_decl.target
            && path
                .segments
                .first()
                .is_some_and(|head| crate::diagnostic::is_relative_path(&head.name))
        {
            continue;
        }
        match &use_decl.list {
            Some(list) => out.extend(list.iter().map(|entry| {
                entry
                    .alias
                    .as_ref()
                    .map_or_else(|| entry.name.name.clone(), |alias| alias.name.clone())
            })),
            None => out.extend(use_decl.alias.as_ref().map_or_else(
                || tail_name(&use_decl.target),
                |alias| Some(alias.name.clone()),
            )),
        }
    }
    out
}

fn tail_name(target: &UseTarget) -> Option<String> {
    match target {
        UseTarget::Module(path)
        | UseTarget::Project {
            module: Some(path), ..
        } => path_tail(path),
        UseTarget::Project { id, module: None } => Some(id.clone()),
    }
}

fn path_tail(path: &ModulePath) -> Option<String> {
    path.segments.last().map(|ident| ident.name.clone())
}

fn is_self_type(name: &str) -> bool {
    name == "Self" || name == "self"
}

/// True when a path segment names a value-position item (module or free
/// function) rather than a type / enum variant - its first character is
/// lowercase or `_`. Used to keep `module::Type::method` paths
/// opaque-by-head in stdlib phantom validation.
fn starts_lowercase(seg: &str) -> bool {
    seg.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
}

/// Whether a stdlib path of this shape is validated against the export
/// tables, so absence from them proves the member does not exist.
/// Two-segment `module::member` and all-lowercase three-segment
/// `module::submodule::member` are validated. `module::Type::member`
/// stays opaque-by-head in general - some type surfaces resolve through
/// compiler rewrites rather than runtime bindings - except for two
/// fully-bound surfaces: the `json::Value` / `flag::Value` constructor
/// sets, and the process/exec namespaces, which bind no type-associated
/// path at all.
fn path_shape_is_validated(effective: &[&str]) -> bool {
    match effective {
        [head, _member] => {
            crate::stdlib_exports::is_stdlib_module_name(head) || is_scalar_primitive_name(head)
        }
        [head, sub, member] if starts_lowercase(sub) && starts_lowercase(member) => {
            crate::stdlib_exports::is_stdlib_module_name(head)
        }
        [head, sub, _member]
            if (matches!(*head, "json" | "flag") && *sub == "Value")
                || (matches!(*head, "process" | "exec") && !starts_lowercase(sub)) =>
        {
            crate::stdlib_exports::is_stdlib_module_name(head)
        }
        _ => false,
    }
}

/// Whether `name` is a scalar primitive type. Its associated surface is
/// closed and entirely the standard library's, so a member absent from
/// the export table is definitively unresolved rather than an item the
/// resolver simply cannot see.
fn is_scalar_primitive_name(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "char"
            | "f32"
            | "f64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
    )
}

/// Canonical `::`-joined spelling of a `use` target, used to tell a repeated
/// import of one path from two paths competing for the same name.
fn target_path_text(target: &UseTarget) -> String {
    fn join(path: &ModulePath) -> String {
        path.segments
            .iter()
            .map(|segment| segment.name.as_str())
            .collect::<Vec<_>>()
            .join("::")
    }
    match target {
        UseTarget::Module(path) => join(path),
        UseTarget::Project { id, module } => match module {
            Some(path) => format!("{id}::{}", join(path)),
            None => id.clone(),
        },
    }
}
