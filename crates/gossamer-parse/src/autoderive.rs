#![forbid(unsafe_code)]

include!("autoderive/core.rs");
include!("autoderive/typeinfo_inline.rs");
include!("autoderive/serde_derive.rs");
include!("autoderive/augment_entry.rs");
include!("autoderive/stdlib_wrappers.rs");
include!("autoderive/assoc_consts.rs");
include!("autoderive/trait_defaults.rs");
include!("autoderive/self_paths.rs");
include!("autoderive/static_init.rs");
include!("autoderive/parse_rewrites.rs");
include!("autoderive/stdlib_surface.rs");
include!("autoderive/tests.rs");

#[path = "autoderive/primitive_surface.rs"]
mod primitive_surface;
use primitive_surface::{synthesize_format_helpers, synthesize_primitive_surface};

#[path = "autoderive/iterator_adapters.rs"]
mod iterator_adapters;
use iterator_adapters::{
    rewrite_fallible_collect, splice_iterator_methods, synthesize_collect_helpers,
    synthesize_iterator_adapters,
};

#[path = "autoderive/operator_impls.rs"]
mod operator_impls;
use operator_impls::rewrite_operator_impls;
