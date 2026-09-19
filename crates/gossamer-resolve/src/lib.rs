//! Name resolution for the Gossamer compiler.
//! The resolver turns a parsed [`gossamer_ast::SourceFile`] into a
//! [`Resolutions`] side table keyed by `NodeId`, plus a vector of
//! [`ResolveDiagnostic`]s for any names that could not be resolved.
//! Name lookup runs in two passes over the top-level items of the crate
//! root. The first pass allocates [`DefId`]s and registers every item in
//! the module namespace so that forward references work. The second pass
//! walks each item body, pushing block/function/pattern scopes as it
//! goes, and records a [`Resolution`] for every path occurrence.
//! Imports brought in by `use` declarations are represented as
//! [`Resolution::Import`] and the consumer (HIR lowering) is responsible
//! for following the full module path externally. The resolver validates
//! canonical stdlib paths and registered external item imports so typos do not
//! silently introduce arbitrary tail aliases.

#![forbid(unsafe_code)]

mod cfg;
mod def_id;
mod diagnostic;
mod external;
mod named_args;
mod numeric_limits;
mod resolutions;
mod resolver;
mod scope;
mod stdlib_exports;

pub use cfg::{item_is_active, set_test_cfg, test_cfg_enabled, without_inactive_items};

pub use def_id::{CrateId, DefId, DefIdGenerator, DefKind, ModId};
pub use diagnostic::{ResolveDiagnostic, ResolveError};
pub use named_args::resolve_named_arguments;
pub use numeric_limits::{LimitConstant, LimitLiteral, limit_constant};
pub use resolutions::{FloatWidth, IntWidth, PrimitiveTy, Resolution, Resolutions};
pub use resolver::{project_dep_module_name, resolve_source_file};
pub use scope::is_prelude_value;
pub use stdlib_exports::{
    STDLIB_MACRO_ITEMS, STDLIB_MANIFEST_ITEMS, STDLIB_MODULE_PATHS, STDLIB_MODULES,
    STDLIB_QUALIFIED, canonical_stdlib_path, is_stdlib_item_path, is_stdlib_qualified,
    sole_stdlib_module_exporting, stdlib_macro_named, stdlib_module_item_names,
};

pub use external::{
    BindingType, BindingVariantArm, ExternalItem, ExternalModule, all_external_module_paths,
    all_external_modules, clear_for_test, lookup_external_item, lookup_external_module,
    set_external_modules,
};
