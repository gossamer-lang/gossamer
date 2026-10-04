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
    /// The project allows foreign functions.
    Allowed,
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
            Some(parsed) if parsed.project.ffi => Self::Allowed,
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
            Self::Allowed => "allowed".to_string(),
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
