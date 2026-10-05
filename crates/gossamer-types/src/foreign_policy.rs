//! Whether a project may declare foreign functions.
//!
//! A project may call native code unless its `project.toml` sets
//! `ffi = false`. With that set, every function declared in an
//! `unsafe extern "C"` block - in the project's own sources or in a
//! dependency's, which reach the front end bundled with them - is GT0102,
//! reported once per library with a label at each declaration. The standard library's own declarations are appended
//! after the program and are not the project's, and a file outside any
//! project is not governed.

use std::path::Path;

use gossamer_ast::{Item, ItemKind, ModBody, ModDecl, SourceFile};
use gossamer_diagnostics::{Diagnostic, Location};
use gossamer_lex::Span;

use crate::{ForeignError, ForeignLibrary, TypeDiagnostic, TypeError};

/// The rule a build follows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ForeignPolicy {
    /// No project governs the build: a file run on its own.
    #[default]
    Ungoverned,
    /// The project allows foreign functions. `legacy_by_value` names the
    /// manifest when its `gossamer-version` predates 0.67, where a plain
    /// `#[repr(C)]` struct parameter passed a pointer rather than the struct.
    Allowed {
        /// The manifest whose `gossamer-version` predates by-value structs.
        legacy_by_value: Option<String>,
    },
    /// The project does not allow them.
    Denied {
        /// The governing manifest's path, for the report.
        manifest: String,
        /// The project's id, when its manifest parses.
        project: Option<String>,
    },
}

impl ForeignPolicy {
    /// The rule of the project `subject` (a source file or a directory)
    /// belongs to: ungoverned outside a project, denied when its manifest
    /// sets `ffi = false` or cannot be read or parsed, and allowed otherwise.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn for_path(subject: &Path) -> Self {
        let Some(manifest) = gossamer_pkg::manifest::find_manifest(subject) else {
            return Self::Ungoverned;
        };
        let parsed = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|text| gossamer_pkg::manifest::Manifest::parse(&text).ok());
        match parsed {
            Some(parsed) if parsed.project.ffi => {
                let predates = parsed
                    .project
                    .gossamer_version
                    .as_ref()
                    .is_some_and(|req| (req.version.major, req.version.minor) < (0, 67));
                Self::Allowed {
                    legacy_by_value: predates.then(|| manifest.display().to_string()),
                }
            }
            parsed => Self::Denied {
                manifest: manifest.display().to_string(),
                project: parsed.map(|parsed| parsed.project.id.as_str().to_string()),
            },
        }
    }

    /// A wasm build has no project on disk to govern it.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn for_path(_subject: &Path) -> Self {
        Self::Ungoverned
    }

    /// The spelling a cache keys a result on; a denial names its manifest
    /// and project, which the report carries.
    #[must_use]
    pub fn cache_term(&self) -> String {
        match self {
            Self::Ungoverned => "ungoverned".to_string(),
            Self::Allowed {
                legacy_by_value: None,
            } => "allowed".to_string(),
            Self::Allowed {
                legacy_by_value: Some(manifest),
            } => format!("allowed-legacy:{manifest}"),
            Self::Denied { manifest, project } => {
                format!("denied:{manifest}:{}", project.as_deref().unwrap_or(""))
            }
        }
    }

    /// One GT0102 per library that declares an active foreign function in
    /// `sf` before `program_end`, the offset where the program's own text
    /// ends and the toolchain's appended code begins. Libraries are reported
    /// in source order, the project first when it declares any.
    #[must_use]
    pub fn diagnostics(&self, sf: &SourceFile, program_end: usize) -> Vec<Diagnostic> {
        if let Self::Allowed {
            legacy_by_value: Some(manifest),
        } = self
        {
            return legacy_by_value_diagnostics(sf, program_end, manifest);
        }
        let Self::Denied { manifest, .. } = self else {
            return Vec::new();
        };
        self.groups(sf, program_end)
            .iter()
            .map(|group| group.report(group.first.1, manifest))
            .collect()
    }

    /// As [`ForeignPolicy::diagnostics`], with the library's report once per
    /// declaration and anchored there, for an editor, which shows a
    /// diagnostic in the file of its primary label.
    #[must_use]
    pub fn diagnostics_per_declaration(
        &self,
        sf: &SourceFile,
        program_end: usize,
    ) -> Vec<Diagnostic> {
        if let Self::Allowed {
            legacy_by_value: Some(manifest),
        } = self
        {
            return legacy_by_value_diagnostics(sf, program_end, manifest);
        }
        let Self::Denied { manifest, .. } = self else {
            return Vec::new();
        };
        self.groups(sf, program_end)
            .iter()
            .flat_map(|group| {
                group
                    .declarations()
                    .map(|(_, span)| group.report(*span, manifest))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn groups(&self, sf: &SourceFile, program_end: usize) -> Vec<Group> {
        let Self::Denied { project, .. } = self else {
            return Vec::new();
        };
        let mut groups = Vec::new();
        let root = ForeignLibrary::Project(project.clone());
        collect(&sf.items, program_end, &root, &mut groups);
        groups
    }
}

/// Appends each active foreign declaration in `items` to the group of the
/// library it belongs to: the nearest enclosing `#[dependency]` module, or
/// `library` outside one.
fn collect(items: &[Item], program_end: usize, library: &ForeignLibrary, groups: &mut Vec<Group>) {
    for item in items {
        if !gossamer_ast::cfg::item_is_active(&item.attrs) {
            continue;
        }
        match &item.kind {
            ItemKind::Fn(decl)
                if decl.extern_abi.is_some() && (item.span.start as usize) < program_end =>
            {
                let entry = (decl.name.name.clone(), item.span);
                match groups.iter_mut().find(|group| group.library == *library) {
                    Some(group) => group.rest.push(entry),
                    None => groups.push(Group {
                        library: library.clone(),
                        first: entry,
                        rest: Vec::new(),
                    }),
                }
            }
            ItemKind::Mod(ModDecl {
                body: ModBody::Inline(inner),
                ..
            }) => {
                let dependency = item
                    .attrs
                    .outer
                    .iter()
                    .find_map(|attr| attr.string_argument("dependency"))
                    .map(|id| ForeignLibrary::Dependency(id.to_string()));
                collect(
                    inner,
                    program_end,
                    dependency.as_ref().unwrap_or(library),
                    groups,
                );
            }
            _ => {}
        }
    }
}

/// One library's foreign declarations, in source order.
struct Group {
    library: ForeignLibrary,
    first: (String, Span),
    rest: Vec<(String, Span)>,
}

impl Group {
    fn declarations(&self) -> impl Iterator<Item = &(String, Span)> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// The GT0102 for this library with its primary label at `anchor` and a
    /// secondary label at every other declaration, each resolving to its
    /// own file.
    fn report(&self, anchor: Span, manifest: &str) -> Diagnostic {
        let error = TypeError::Foreign(ForeignError::NotAllowed {
            library: self.library.clone(),
            names: self.declarations().map(|(name, _)| name.clone()).collect(),
            manifest: manifest.to_string(),
        });
        let mut out = TypeDiagnostic {
            error,
            span: anchor,
        }
        .to_diagnostic();
        for (name, span) in self.declarations().filter(|(_, span)| *span != anchor) {
            out = out.with_secondary(
                Location::new(span.file, *span),
                format!("`{name}` is declared here"),
            );
        }
        out
    }
}

/// Every `#[repr(C)]` struct name `items` declare, at any module depth.
fn repr_c_names(items: &[Item], out: &mut Vec<String>) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(decl) if item.attrs.lists_argument("repr", "C") => {
                out.push(decl.name.name.clone());
            }
            ItemKind::Mod(ModDecl {
                body: ModBody::Inline(inner),
                ..
            }) => repr_c_names(inner, out),
            _ => {}
        }
    }
}

/// One plain `#[repr(C)]` struct parameter: its position, name, type
/// spelling, and type span.
type PlainStructParam = (usize, String, String, Span);

/// The program's foreign functions with a plain `#[repr(C)]` struct
/// parameter: the function's name, and each such parameter.
fn plain_struct_params(
    items: &[Item],
    program_end: usize,
    structs: &[String],
    out: &mut Vec<(String, Vec<PlainStructParam>)>,
) {
    for item in items {
        if !gossamer_ast::cfg::item_is_active(&item.attrs) {
            continue;
        }
        match &item.kind {
            ItemKind::Fn(decl)
                if decl.extern_abi.is_some() && (item.span.start as usize) < program_end =>
            {
                let mut params = Vec::new();
                for (position, param) in decl.params.iter().enumerate() {
                    let gossamer_ast::FnParam::Typed { pattern, ty, .. } = param else {
                        continue;
                    };
                    let gossamer_ast::TypeKind::Path(path) = &ty.kind else {
                        continue;
                    };
                    let Some(last) = path.segments.last() else {
                        continue;
                    };
                    if structs.contains(&last.name.name) {
                        let name = match &pattern.kind {
                            gossamer_ast::PatternKind::Ident { name, .. } => name.name.clone(),
                            _ => format!("{}", position + 1),
                        };
                        params.push((position, name, last.name.name.clone(), ty.span));
                    }
                }
                if !params.is_empty() {
                    out.push((decl.name.name.clone(), params));
                }
            }
            ItemKind::Mod(ModDecl {
                body: ModBody::Inline(inner),
                ..
            }) => plain_struct_params(inner, program_end, structs, out),
            _ => {}
        }
    }
}

/// The arguments at `positions` of every call to a function named `name`.
struct CallArgs<'a> {
    name: &'a str,
    positions: Vec<usize>,
    found: Vec<(usize, Span)>,
}

impl gossamer_ast::visitor::Visitor for CallArgs<'_> {
    fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
        if let gossamer_ast::ExprKind::Call { callee, args } = &expr.kind
            && let gossamer_ast::ExprKind::Path(path) = &callee.kind
            && path
                .segments
                .last()
                .is_some_and(|segment| segment.name.name == self.name)
        {
            for position in &self.positions {
                if let Some(arg) = args.get(*position) {
                    self.found.push((*position, arg.span));
                }
            }
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }
}

/// GT0112 for each plain `#[repr(C)]` struct parameter of the program's
/// foreign functions, in a project whose `gossamer-version` predates 0.67,
/// with the `&mut` spelling at the declaration and at every call as the fix.
fn legacy_by_value_diagnostics(
    sf: &SourceFile,
    program_end: usize,
    manifest: &str,
) -> Vec<Diagnostic> {
    let mut structs = Vec::new();
    repr_c_names(&sf.items, &mut structs);
    let mut functions = Vec::new();
    plain_struct_params(&sf.items, program_end, &structs, &mut functions);
    let mut out = Vec::new();
    for (function, params) in functions {
        let mut calls = CallArgs {
            name: &function,
            positions: params.iter().map(|(position, ..)| *position).collect(),
            found: Vec::new(),
        };
        gossamer_ast::visitor::Visitor::visit_source_file(&mut calls, sf);
        for (position, param, ty, span) in &params {
            let error = TypeError::Foreign(ForeignError::LegacyByValue {
                name: function.clone(),
                param: param.clone(),
                ty: ty.clone(),
                manifest: manifest.to_string(),
            });
            let insert_at =
                |at: Span| Location::new(at.file, Span::new(at.file, at.start, at.start));
            let mut diagnostic = TypeDiagnostic { error, span: *span }
                .to_diagnostic()
                .with_suggestion(gossamer_diagnostics::Suggestion {
                    location: insert_at(*span),
                    message: format!("pass `{ty}` through a pointer, as before 0.67"),
                    replacement: "&mut ".to_string(),
                });
            for (call_position, arg_span) in &calls.found {
                if call_position == position && (arg_span.start as usize) < program_end {
                    diagnostic = diagnostic.with_suggestion(gossamer_diagnostics::Suggestion {
                        location: insert_at(*arg_span),
                        message: "pass the argument by `&mut`".to_string(),
                        replacement: "&mut ".to_string(),
                    });
                }
            }
            out.push(diagnostic);
        }
    }
    out
}
