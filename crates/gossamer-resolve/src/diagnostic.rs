//! Diagnostics emitted by the name resolver.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_lex::Span;
use thiserror::Error;

/// Prefix of the per-type serde functions the autoderive stage synthesizes.
/// A call to one that does not exist means that stage declined the type and
/// reported why, so the missing name is not the user's to act on.
const SYNTHESIZED_SERDE_PREFIX: &str = "__gos_serde_";

/// Note and help for a call that leaves a parameter unfilled: which
/// parameters must be given, and which may be left out.
fn missing_argument_help(
    out: gossamer_diagnostics::Diagnostic,
    missing: &str,
    plural: bool,
    optional: &str,
) -> gossamer_diagnostics::Diagnostic {
    let subject = if plural {
        format!("{missing} have no defaults")
    } else {
        format!("{missing} has no default")
    };
    let out = out.with_note(format!(
        "{subject}, so every call supplies a value - positionally or by name"
    ));
    if optional.is_empty() {
        out
    } else {
        out.with_help(format!("only {optional} may be omitted"))
    }
}

/// Note and help for a std macro named as a value path: what a macro is,
/// and the spelling that works.
fn std_macro_help(
    out: gossamer_diagnostics::Diagnostic,
    name: &str,
) -> gossamer_diagnostics::Diagnostic {
    out.with_note(
        "the set is fixed and expands where it is written, so it has no \
         function to call or pass as a value"
            .to_string(),
    )
    .with_help(format!(
        "write `{name}(..)`; it is in scope without an import"
    ))
}

/// Suggestion or help for a `use` path that names no module: the closest
/// real module path when one is near, otherwise how a path is spelled.
/// Built-in type names no module exports and no `use` reaches: the
/// language declares them, so writing an import for one names a module
/// that was never going to exist.
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "String", "Tuple", "bool", "char", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
    "isize", "usize", "f32", "f64",
];

fn unknown_module_help(
    out: gossamer_diagnostics::Diagnostic,
    location: gossamer_diagnostics::Location,
    path: &str,
    candidate: Option<&str>,
) -> gossamer_diagnostics::Diagnostic {
    if BUILTIN_TYPE_NAMES.contains(&path) {
        return out.with_help(format!(
            "`{path}` is a built-in type, in scope everywhere; drop the `use`"
        ));
    }
    // A path rooted at the unit itself names one of its own modules, so
    // neither the standard library nor a spelling drawn from it is what the
    // reader needs.
    if is_relative_path(path) {
        return match candidate {
            Some(known) => out.with_suggestion(gossamer_diagnostics::Suggestion::replacement(
                location,
                format!("did you mean `{known}`?"),
                known.to_string(),
            )),
            None => out.with_help(
                "a `crate::` / `super::` / `self::` path names a module of this package \
                 or an item one declares; a file is a module, so add the file that \
                 declares it, or correct the path"
                    .to_string(),
            ),
        };
    }
    match closest_module_path(path) {
        Some(known) => out.with_suggestion(gossamer_diagnostics::Suggestion::replacement(
            location,
            format!("did you mean `std::{known}`?"),
            format!("std::{known}"),
        )),
        None => out.with_help(
            "a standard library path is spelled in full, as in \
             `use std::encoding::json`; `gos doc std` lists the modules"
                .to_string(),
        ),
    }
}

/// A single resolver diagnostic with its source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveDiagnostic {
    /// The specific error that occurred.
    pub error: ResolveError,
    /// Where in the source the error was detected.
    pub span: Span,
    /// Closest name visible where the error was raised.
    ///
    /// Locals, parameters, and closure bindings exist only in the resolver's
    /// scope stack, so the candidate is captured at the point of failure;
    /// nothing downstream can reconstruct that scope.
    pub in_scope_candidate: Option<String>,
}

impl ResolveDiagnostic {
    /// Constructs a diagnostic pairing an error with its source span.
    #[must_use]
    pub const fn new(error: ResolveError, span: Span) -> Self {
        Self {
            error,
            span,
            in_scope_candidate: None,
        }
    }

    /// Attaches the closest name that was visible where the error was raised.
    #[must_use]
    pub fn with_candidate(mut self, candidate: Option<String>) -> Self {
        self.in_scope_candidate = candidate;
        self
    }
}

impl fmt::Display for ResolveDiagnostic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.error)
    }
}

/// Every failure mode the resolver can report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// The first segment of a path could not be resolved to any name in
    /// the current scope.
    #[error("cannot find `{name}` in this scope")]
    UnresolvedName {
        /// Name that could not be resolved.
        name: String,
    },
    /// A resolved name exists, but in the wrong namespace for this usage.
    #[error("expected {expected} but `{name}` is a {found}")]
    WrongNamespace {
        /// Name that was looked up.
        name: String,
        /// Namespace the caller was searching.
        expected: &'static str,
        /// Namespace where the name actually lives.
        found: &'static str,
    },
    /// A `use` path that names no stdlib module or registered external item.
    #[error("module `{path}` does not exist")]
    UnknownModulePath {
        /// The path as written.
        path: String,
    },
    /// Two items in the same module share a name.
    #[error("the name `{name}` is defined multiple times in this module")]
    DuplicateItem {
        /// Conflicting name.
        name: String,
    },
    /// Two `use` declarations import the same final name.
    #[error("the name `{name}` is imported multiple times in this scope")]
    DuplicateImport {
        /// Conflicting name.
        name: String,
    },
    /// A `use` path whose module exists but which names an item that
    /// module does not export.
    #[error("no `{name}` in `std::{module}`")]
    UnknownStdItem {
        /// Item name as written.
        name: String,
        /// `std::`-relative path of the module that was searched.
        module: String,
    },
    /// A `use` path naming a spelling that a canonical type replaced.
    #[error("`{path}` does not exist - use `{replacement}` instead")]
    RemovedStdItem {
        /// The path as written.
        path: String,
        /// The canonical name to import instead.
        replacement: String,
    },
    /// A compiler-known call named as a value path (`fmt::println(..)`).
    /// It expands at parse time and the runtime binds no callable for
    /// it, so the path has nothing to call.
    #[error("`{path}` is a compiler-known call; it is written `{name}(..)`")]
    StdMacroAsValue {
        /// The path as written, `std::`-relative.
        path: String,
        /// The macro's own name, for the `name!(..)` spelling.
        name: String,
    },
    /// An item declared without `pub` named from outside the module
    /// that declares it.
    #[error("{kind} `{name}` is private to module `{module}`")]
    PrivateItem {
        /// Item name as declared.
        name: String,
        /// `::`-joined path of the module the item is private to.
        module: String,
        /// Item shape, for the message.
        kind: &'static str,
    },
    /// A std free function called with its data argument in the slot it
    /// occupied before every module took its data first.
    #[error("`{callee}` takes its data first")]
    DataLastCallOrder {
        /// The call as written, module-qualified.
        callee: String,
        /// The parameter list in the order the function declares it.
        params: String,
    },
    /// A declaration under one of the compiler-known call names. The
    /// parser expands `println(..)` where it is written, so a `fn` or a
    /// binding under that name could never be reached.
    #[error("`{name}` is a compiler-known call and cannot be declared")]
    ReservedCallName {
        /// The name as written.
        name: String,
        /// What was being declared, for the message.
        kind: &'static str,
    },
    /// A single-name pattern starting with an uppercase letter that names no
    /// unit variant or constant in scope. Such a name is read as a path, not
    /// a binding, so it cannot introduce one.
    #[error("`{name}` is not a unit variant or constant in scope")]
    UppercaseBindingPattern {
        /// The name as written.
        name: String,
    },
    /// A call to a function that reflects its type parameter
    /// (`for f in typeInfo::<T>()`) written without the turbofish that names
    /// the type, so no specialisation of it exists.
    #[error("`{name}` reflects its type parameter, so a call to it names the type")]
    ReflectingCallWithoutType {
        /// The function as written.
        name: String,
    },
    /// A `typeInfo::<T>()` naming a type this unit does not declare, or
    /// one whose shape carries nothing to reflect.
    #[error("`typeInfo::<{name}>()` has nothing to reflect")]
    UnreflectableType {
        /// Type as the user spelled it in the turbofish.
        name: String,
    },
    /// A `break` or `continue` written where no loop encloses it.
    #[error("`{keyword}` outside of a loop")]
    LoopControlOutsideLoop {
        /// The keyword as written, `break` or `continue`.
        keyword: String,
    },
    /// A `break 'label` / `continue 'label` naming no enclosing loop.
    #[error("no enclosing loop is labelled `\'{label}`")]
    UnknownLoopLabel {
        /// The keyword as written, `break` or `continue`.
        keyword: String,
        /// Label as written, without its leading apostrophe.
        label: String,
        /// Labels that are in scope here, for the help line.
        in_scope: Vec<String>,
    },
    /// A `name:` argument label naming no parameter of the callee.
    #[error("`{name}` is not a parameter of this function")]
    UnknownNamedArgument {
        /// Label as the caller wrote it.
        name: String,
        /// Declared parameter names, for the help line.
        expected: String,
    },
    /// The same parameter named twice in one call.
    #[error("`{name}` is given twice in this call")]
    DuplicateNamedArgument {
        /// Label as the caller wrote it.
        name: String,
    },
    /// A positional argument written after a labelled one.
    #[error("positional arguments must come before named ones")]
    PositionalAfterNamed,
    /// A labelled call to a method more than one type declares
    /// differently.
    #[error("`{method}` is declared with different parameters by more than one type")]
    AmbiguousNamedArgument {
        /// Method name as written.
        method: String,
        /// One declaring type.
        first: String,
        /// Another declaring type, whose parameters differ.
        second: String,
    },
    /// A `name:` label on a call this pass cannot match to a declaration.
    #[error("`{name}:` cannot be matched to a parameter of {target}")]
    NamedArgumentTarget {
        /// Label as the caller wrote it.
        name: String,
        /// Description of the call target.
        target: String,
    },
    /// A call that leaves a parameter with neither an argument nor a
    /// default. Reported here rather than as an arity mismatch because
    /// names and defaults make the count a poor description of the
    /// problem: a call can supply the declared number of arguments and
    /// still leave a parameter unfilled.
    #[error("this call gives no value for {missing}")]
    MissingRequiredArgument {
        /// Backticked, comma-joined names of the unfilled parameters.
        missing: String,
        /// Whether more than one parameter is unfilled.
        plural: bool,
        /// Backticked, comma-joined parameters that do have defaults.
        optional: String,
    },
    /// A parameter default that is not a constant.
    #[error("a parameter default must be a constant")]
    NonConstantDefault {
        /// Parameter the default was written on.
        name: String,
    },
    /// A bare enum-variant name that more than one enum declares.
    #[error("`{name}` is a variant of more than one enum")]
    AmbiguousVariant {
        /// Variant name as written.
        name: String,
        /// Enums that declare a variant of this name, in source order.
        enums: Vec<String>,
    },
    /// A bare name that resolves nowhere in scope but names an item some
    /// module in this unit declares.
    #[error("`{name}` is not in scope; module `{module}` declares it")]
    NotImported {
        /// Name as written.
        name: String,
        /// `::`-joined path of the module that declares it.
        module: String,
    },
    /// A path headed by a dependency package's module that the file
    /// never imported.
    #[error("`{module}` is a dependency of this project and is not imported here")]
    DependencyNotImported {
        /// Module name as written at the head of the path.
        module: String,
        /// Project id the dependency is published under.
        id: String,
    },
    /// Two dependency packages whose names normalize to one module name.
    /// A `-` is not part of an identifier, so a package name carrying one is
    /// reached through the same name with `_` in its place - which two
    /// packages can share.
    #[error("dependencies `{first}` and `{second}` are both reached as `{module}`")]
    DependencyModuleCollision {
        /// Module name both packages normalize to.
        module: String,
        /// Project id of the package that claimed the name.
        first: String,
        /// Project id of the package that collides with it.
        second: String,
    },
    /// A `mod name;` declaration with no source behind it.
    #[error("no source file for module `{name}`")]
    MissingModuleSource {
        /// Module name as declared.
        name: String,
    },
}

impl ResolveError {
    /// Returns a short stable tag useful for snapshot tests.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::UnknownModulePath { .. } => "unknown-module-path",
            Self::UnresolvedName { .. } => "unresolved-name",
            Self::WrongNamespace { .. } => "wrong-namespace",
            Self::DuplicateItem { .. } => "duplicate-item",
            Self::DataLastCallOrder { .. } => "data-last-call-order",
            Self::DuplicateImport { .. } => "duplicate-import",
            Self::UnknownStdItem { .. } => "unknown-std-item",
            Self::RemovedStdItem { .. } => "removed-std-item",
            Self::StdMacroAsValue { .. } => "std-macro-as-value",
            Self::PrivateItem { .. } => "private-item",
            Self::UnreflectableType { .. } => "unreflectable-type",
            Self::UppercaseBindingPattern { .. } => "uppercase-binding-pattern",
            Self::ReflectingCallWithoutType { .. } => "reflecting-call-without-type",
            Self::ReservedCallName { .. } => "reserved-call-name",
            Self::UnknownNamedArgument { .. } => "unknown-named-argument",
            Self::DuplicateNamedArgument { .. } => "duplicate-named-argument",
            Self::PositionalAfterNamed => "positional-after-named",
            Self::AmbiguousNamedArgument { .. } => "ambiguous-named-argument",
            Self::NamedArgumentTarget { .. } => "named-argument-target",
            Self::NonConstantDefault { .. } => "non-constant-default",
            Self::MissingRequiredArgument { .. } => "missing-required-argument",
            Self::AmbiguousVariant { .. } => "ambiguous-variant",
            Self::NotImported { .. } => "not-imported",
            Self::DependencyNotImported { .. } => "dependency-not-imported",
            Self::DependencyModuleCollision { .. } => "dependency-module-collision",
            Self::MissingModuleSource { .. } => "missing-module-source",
            Self::LoopControlOutsideLoop { .. } => "loop-control-outside-loop",
            Self::UnknownLoopLabel { .. } => "unknown-loop-label",
        }
    }

    /// Whether this error is about a name an earlier stage produced rather
    /// than one the user wrote: a parser recovery placeholder, or a
    /// synthesized function the autoderive stage declined to emit.
    ///
    /// In both cases that stage has already reported the actionable error,
    /// and repeating it here would point the user at a name absent from
    /// their source.
    #[must_use]
    pub fn is_about_parse_placeholder(&self) -> bool {
        if let Self::UnresolvedName { name } = self
            && name.starts_with(SYNTHESIZED_SERDE_PREFIX)
        {
            return true;
        }
        let reported = match self {
            Self::UnresolvedName { name }
            | Self::WrongNamespace { name, .. }
            | Self::DuplicateItem { name }
            | Self::AmbiguousVariant { name, .. }
            | Self::MissingModuleSource { name }
            | Self::UnreflectableType { name }
            | Self::UppercaseBindingPattern { name }
            | Self::ReflectingCallWithoutType { name }
            | Self::ReservedCallName { name, .. }
            | Self::UnknownNamedArgument { name, .. }
            | Self::DuplicateNamedArgument { name }
            | Self::NamedArgumentTarget { name, .. }
            | Self::NonConstantDefault { name }
            | Self::DuplicateImport { name } => name,
            Self::PositionalAfterNamed
            | Self::MissingRequiredArgument { .. }
            | Self::LoopControlOutsideLoop { .. }
            | Self::DataLastCallOrder { .. }
            | Self::UnknownLoopLabel { .. } => return false,
            Self::DependencyNotImported { module, .. } => module,
            Self::DependencyModuleCollision { module, .. } => module,
            Self::AmbiguousNamedArgument { method, .. } => method,
            Self::UnknownModulePath { path }
            | Self::RemovedStdItem { path, .. }
            | Self::StdMacroAsValue { path, .. } => path,
            Self::PrivateItem { name, module, .. } => {
                return name
                    .split("::")
                    .chain(module.split("::"))
                    .any(gossamer_ast::common::is_error_name);
            }
            Self::UnknownStdItem { name, module } | Self::NotImported { name, module } => {
                return name
                    .split("::")
                    .chain(module.split("::"))
                    .any(gossamer_ast::common::is_error_name);
            }
        };
        reported
            .split("::")
            .any(gossamer_ast::common::is_error_name)
    }

    /// Stable error code used by the diagnostics framework.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnresolvedName { .. } => "GR0001",
            Self::WrongNamespace { .. } => "GR0002",
            Self::DuplicateItem { .. } => "GR0003",
            Self::DuplicateImport { .. } => "GR0004",
            Self::UnknownModulePath { .. } => "GR0005",
            Self::RemovedStdItem { .. } => "GR0006",
            Self::UnknownStdItem { .. } => "GR0007",
            Self::PrivateItem { .. } => "GR0008",
            Self::AmbiguousVariant { .. } => "GR0009",
            Self::MissingModuleSource { .. } => "GR0010",
            Self::NotImported { .. } => "GR0011",
            Self::UnreflectableType { .. } => "GR0012",
            Self::UnknownNamedArgument { .. }
            | Self::DuplicateNamedArgument { .. }
            | Self::PositionalAfterNamed
            | Self::AmbiguousNamedArgument { .. }
            | Self::NamedArgumentTarget { .. } => "GR0013",
            Self::NonConstantDefault { .. } => "GR0014",
            Self::MissingRequiredArgument { .. } => "GR0015",
            Self::DependencyNotImported { .. } => "GR0016",
            Self::LoopControlOutsideLoop { .. } | Self::UnknownLoopLabel { .. } => "GR0017",
            Self::StdMacroAsValue { .. } => "GR0018",
            Self::DependencyModuleCollision { .. } => "GR0019",
            Self::ReservedCallName { .. } => "GR0020",
            Self::DataLastCallOrder { .. } => "GR0021",
            Self::UppercaseBindingPattern { .. } => "GR0022",
            Self::ReflectingCallWithoutType { .. } => "GR0023",
        }
    }
}

impl ResolveDiagnostic {
    /// Renders this diagnostic as a structured
    /// [`gossamer_diagnostics::Diagnostic`]. When `in_scope` is
    /// non-empty, an `UnresolvedName` diagnostic also carries a
    /// did-you-mean suggestion drawn from the provided names.
    #[must_use]
    pub fn to_diagnostic(&self, in_scope: &[&str]) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location, Suggestion, suggest};
        let location = Location::new(self.span.file, self.span);
        let title = format!("{}", self.error);
        let mut out =
            Diagnostic::error(Code(self.error.code()), title.clone()).with_primary(location, title);
        if let ResolveError::UnresolvedName { name } = &self.error {
            if let Some(replacement) = crate::stdlib_exports::canonical_collection_name(name) {
                return out.with_help(format!(
                    "`{replacement}` is the one spelling for this type; write `{replacement}` \
                     and import it with `use std::collections::{replacement}`"
                ));
            }
            let mut module_paths = crate::STDLIB_MODULE_PATHS.iter().filter(|path| {
                **path == name.as_str()
                    || path
                        .strip_suffix(name)
                        .is_some_and(|prefix| prefix.ends_with("::"))
            });
            if let (Some(path), None) = (module_paths.next(), module_paths.next()) {
                // The name is a real stdlib module, so the import is the fix.
                // A spelling candidate found below would be a different name
                // entirely, and applying it would rewrite a correct call onto
                // an unrelated type.
                return out.with_help(format!(
                    "standard library module `{name}` is not in scope; add `use std::{path}`"
                ));
            }
            if let Some(module) = crate::stdlib_exports::sole_stdlib_module_exporting(name) {
                // Exactly one standard library module exports this name, so
                // the import is unambiguous. A spelling candidate found below
                // would name a different item entirely.
                return out.with_help(format!(
                    "`{name}` is exported by `std::{module}`; add `use std::{module}::{name}`"
                ));
            }
            // The scope-derived candidate is checked first: it is the only
            // one that can name a local, a parameter, or a closure binding.
            if let Some(suggestion) = self
                .in_scope_candidate
                .as_deref()
                .or_else(|| suggest(name, in_scope.iter().copied(), 2))
                .or_else(|| suggest(name, crate::scope::prelude_suggestion_names(), 2))
            {
                out = out.with_suggestion(Suggestion::replacement(
                    location,
                    format!("did you mean `{suggestion}`?"),
                    suggestion.to_string(),
                ));
            }
        } else {
            out = self.with_error_specific_help(out);
        }
        out
    }

    /// Attaches the help line for every resolve error that is not an
    /// unresolved name, which carries its own did-you-mean search.
    /// Help lines for the named-argument and parameter-default family.
    fn named_argument_help(
        &self,
        out: gossamer_diagnostics::Diagnostic,
    ) -> gossamer_diagnostics::Diagnostic {
        match &self.error {
            ResolveError::UnknownNamedArgument { expected, .. } => {
                if expected.is_empty() {
                    out.with_help("this function declares no parameters".to_string())
                } else {
                    out.with_help(format!("its parameters are `{expected}`"))
                }
            }
            ResolveError::DuplicateNamedArgument { .. } => {
                out.with_help("give each parameter at most once".to_string())
            }
            ResolveError::PositionalAfterNamed => out.with_help(
                "once a name is used, every later argument needs one too, because the \
                 positions after it are no longer in written order"
                    .to_string(),
            ),
            ResolveError::AmbiguousNamedArgument {
                method,
                first,
                second,
            } => out.with_help(format!(
                "`{first}` and `{second}` both declare `{method}`, with different parameters; \
                 the receiver's type is not known here, so pass the arguments by position"
            )),
            ResolveError::NamedArgumentTarget { .. } => out.with_help(
                "a name may only be given for a call to a function, method, or associated \
                 function declared in this package"
                    .to_string(),
            ),
            ResolveError::NonConstantDefault { .. } => out.with_help(
                "a default is spliced into every call that omits it, so it must be a \
                 literal - `10`, `-1`, `true`, `\"\"`"
                    .to_string(),
            ),
            _ => out,
        }
    }

    /// Help for the diagnostics about which package a path comes from: the
    /// import that names it, and the name two packages cannot share.
    fn with_dependency_help(
        &self,
        out: gossamer_diagnostics::Diagnostic,
    ) -> gossamer_diagnostics::Diagnostic {
        match &self.error {
            ResolveError::DependencyNotImported { module, id } => out.with_help(format!(
                "a dependency's items are reached through the import that names the \
                 package; add `use {module}` to this file, or \
                 `use \"{id}\" as {module}` to choose the name"
            )),
            ResolveError::DependencyModuleCollision {
                module,
                first,
                second,
            } => out
                .with_help(format!(
                    "name one of them in the manifest: \
                     `\"{second}\" = {{ .., module = \"..\" }}`, then reach it under \
                     that name (or through `use \"{second}\" as ..`)"
                ))
                .with_note(format!("`{first}` claimed `{module}` first"))
                .with_note(format!(
                    "a package name's `-` is not part of an identifier, so both reach \
                     source as `{module}`; the pairing is declared in `project.toml`, \
                     not in this file"
                )),
            _ => out,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per diagnostic that carries a help line"
    )]
    fn with_error_specific_help(
        &self,
        out: gossamer_diagnostics::Diagnostic,
    ) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Location, Suggestion, suggest};
        let location = Location::new(self.span.file, self.span);
        match &self.error {
            ResolveError::WrongNamespace {
                name,
                expected,
                found,
            } => out.with_help(format!(
                "use a {expected} in this position; `{name}` resolves to a {found}"
            )),
            error @ (ResolveError::LoopControlOutsideLoop { .. }
            | ResolveError::UnknownLoopLabel { .. }) => loop_control_help(out, location, error),
            ResolveError::StdMacroAsValue { name, .. } => std_macro_help(out, name),
            ResolveError::DataLastCallOrder { callee, params } => out
                .with_note(format!(
                    "every std free function takes its data first, so `{callee}` reads \
                     as the method it stands for: `{params}`"
                ))
                .with_help("move the data argument to the front".to_string()),
            ResolveError::ReservedCallName { name, kind } => out
                .with_note(
                    if gossamer_parse::builtin_macros::is_spawn_primitive(name) {
                        format!(
                            "`spawn(f)` is the one way to start a goroutine, so this {kind} \
                         would take concurrency away from every call in the file"
                        )
                    } else {
                        format!(
                            "`{name}(..)` expands where it is written, so this {kind} \
                         would have no way to be called"
                        )
                    },
                )
                .with_help(format!("give the {kind} a name of its own")),
            ResolveError::UnknownNamedArgument { .. }
            | ResolveError::DuplicateNamedArgument { .. }
            | ResolveError::PositionalAfterNamed
            | ResolveError::AmbiguousNamedArgument { .. }
            | ResolveError::NamedArgumentTarget { .. }
            | ResolveError::NonConstantDefault { .. } => self.named_argument_help(out),
            ResolveError::MissingRequiredArgument {
                missing,
                plural,
                optional,
            } => missing_argument_help(out, missing, *plural, optional),
            ResolveError::UnknownModulePath { path } => {
                unknown_module_help(out, location, path, self.in_scope_candidate.as_deref())
            }
            ResolveError::DuplicateItem { name } => out.with_help(format!(
                "rename or remove one `{name}` declaration in this module"
            )),
            ResolveError::DuplicateImport { name } => out.with_help(format!(
                "remove one import of `{name}`, or alias one with `as`"
            )),
            ResolveError::RemovedStdItem { replacement, .. } => out.with_help(format!(
                "`{replacement}` is the one spelling for this type; import it as \
                 `use std::collections::{replacement}`"
            )),
            ResolveError::UnknownStdItem { name, module } => {
                let exports = crate::stdlib_exports::stdlib_module_item_names(module);
                match suggest(name, exports.iter().copied(), 2) {
                    Some(known) => out.with_suggestion(Suggestion::replacement(
                        location,
                        format!("did you mean `std::{module}::{known}`?"),
                        format!("std::{module}::{known}"),
                    )),
                    None => out.with_help(format!(
                        "`gos doc std::{module}` lists what this module exports"
                    )),
                }
            }
            ResolveError::PrivateItem { name, module, kind } => out.with_help(format!(
                "`{name}` is declared without `pub`, so only `{module}` and its child \
                 modules can name it; write `pub(package)` on the {kind} to reach it \
                 from anywhere in this package, or `pub` to make it part of the \
                 package's public API"
            )),
            ResolveError::UppercaseBindingPattern { name } => {
                let mut chars = name.chars();
                let lower: String = chars
                    .next()
                    .map(|first| first.to_lowercase().chain(chars).collect())
                    .unwrap_or_default();
                out.with_help(format!(
                    "a pattern name that starts with an uppercase letter names a unit \
                     variant or a constant; a binding starts with a lowercase letter, \
                     as in `{lower}`"
                ))
            }
            ResolveError::ReflectingCallWithoutType { name } => out.with_help(format!(
                "name the type in a turbofish, as in `{name}::<Point>(value)`: each call \
                 is specialised for the type it names, and the type is not inferred \
                 from the argument"
            )),
            ResolveError::UnreflectableType { name } => out.with_help(format!(
                "`typeInfo` reflects a struct's fields or an enum's variants; \
                 check that `{name}` is declared in this program and is not a \
                 unit struct, which has no fields"
            )),
            ResolveError::NotImported { name, module } => out
                .with_suggestion(Suggestion::replacement(
                    location,
                    format!("did you mean `{module}::{name}`?"),
                    format!("{module}::{name}"),
                ))
                .with_help(format!(
                    "a module's items are reached through a path or an import; add \
                     `use {module}::{name}` to name it directly"
                )),
            ResolveError::DependencyNotImported { .. }
            | ResolveError::DependencyModuleCollision { .. } => self.with_dependency_help(out),
            ResolveError::MissingModuleSource { name } => out.with_help(format!(
                "`mod {name};` names a module whose source the build never supplied; \
                 add `{name}.gos` (or `{name}/mod.gos`) beside the entry inside a \
                 project, or write the body inline as `mod {name} {{ ... }}`"
            )),
            ResolveError::AmbiguousVariant { name, enums } => out.with_help(format!(
                "{} declare a variant named `{name}`; write the enum, as in `{}::{name}`",
                enums
                    .iter()
                    .map(|e| format!("`{e}`"))
                    .collect::<Vec<_>>()
                    .join(" and "),
                enums.first().map_or("Enum", String::as_str)
            )),
            ResolveError::UnresolvedName { .. } => out,
        }
    }
}

/// `true` when `path` is rooted at the compilation unit rather than at a
/// package: `crate`, `root`, `self` and `super` all anchor inside it.
pub(crate) fn is_relative_path(path: &str) -> bool {
    let head = path.split("::").next().unwrap_or(path);
    matches!(head, "crate" | "root" | "self" | "super")
}

/// Closest real standard library module path to `path`.
///
/// Compares the leaf segment as well as the whole path, so the common
/// mistake of omitting an intermediate namespace (`std::json` for
/// `std::encoding::json`) resolves to the module the user meant.
fn closest_module_path(path: &str) -> Option<&'static str> {
    let written = path.strip_prefix("std::").unwrap_or(path);
    if let Some(exact) = crate::STDLIB_MODULE_PATHS
        .iter()
        .find(|known| known.rsplit("::").next() == Some(written))
    {
        return Some(exact);
    }
    gossamer_diagnostics::suggest(written, crate::STDLIB_MODULE_PATHS.iter().copied(), 2)
}

/// Help and did-you-mean for a `break` / `continue` with no loop to act
/// on, or one naming a label no enclosing loop carries.
fn loop_control_help(
    out: gossamer_diagnostics::Diagnostic,
    location: gossamer_diagnostics::Location,
    error: &ResolveError,
) -> gossamer_diagnostics::Diagnostic {
    use gossamer_diagnostics::{Suggestion, suggest};

    let (keyword, label, in_scope) = match error {
        ResolveError::LoopControlOutsideLoop { keyword } => {
            return out.with_help(format!(
                "`{keyword}` acts on the innermost enclosing loop, so it needs one; move it \
                 inside a `loop`, `while`, or `for`"
            ));
        }
        ResolveError::UnknownLoopLabel {
            keyword,
            label,
            in_scope,
        } => (keyword, label, in_scope),
        // Only the two loop-control errors reach here.
        _ => return out,
    };
    let out = if in_scope.is_empty() {
        out.with_help(format!(
            "no enclosing loop carries a label; write `'name: loop {{ .. }}` on the \
             loop `{keyword}` should act on, or drop the label"
        ))
    } else {
        out.with_help(format!(
            "labels in scope here: {}",
            in_scope
                .iter()
                .map(|name| format!("`'{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    match suggest(label, in_scope.iter().map(String::as_str), 2) {
        Some(best) => out.with_suggestion(Suggestion::replacement(
            location,
            format!("did you mean `'{best}`?"),
            format!("{keyword} '{best}"),
        )),
        None => out,
    }
}
