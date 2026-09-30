//! The method names each built-in receiver answers, as the checker admits them.

use super::{AUTOMATIC_METHODS, SEQUENCE_COMBINATOR_METHODS, SET_METHODS};
use crate::builtin_traits::ITERATOR_BOUND_METHODS as ITERATOR_METHODS;

/// The canonical `String` method surface. Mirrors the compiled-tier
/// dispatch in `gossamer-mir`'s `method_call.rs` (the `TyKind::String`
/// arms) plus the universal `push` / `push_str` building surface, so a
/// String receiver accepts exactly what every tier can lower. A method
/// outside this set on a `String` receiver is the name-global dispatch
/// leak (a `unicode::*` char predicate or a typo) and is rejected. The
/// commonly-typed methods (`find`, `len`, `contains`, ...) are handled
/// with precise return types before this fallback and need not recur
/// here, but listing them keeps the set self-describing.
pub(super) fn is_string_method(name: &str) -> bool {
    STRING_METHODS.contains(&name)
}

/// The `String` method surface, in the order diagnostics list it.
pub(super) const STRING_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "as_bytes",
    "bytes",
    "chars",
    "split",
    "splitn",
    "split_whitespace",
    "split_once",
    "rsplit_once",
    "lines",
    "find",
    "rfind",
    "find_any",
    "rfind_any",
    "index_rune",
    "contains",
    "contains_any",
    "contains_rune",
    "starts_with",
    "ends_with",
    "equal_fold",
    "count",
    "byte_at",
    "byte_len",
    "trim",
    "trim_start",
    "trim_end",
    "trim_matches",
    "trim_start_matches",
    "trim_end_matches",
    "replace",
    "replacen",
    "to_lowercase",
    "to_uppercase",
    "to_title",
    "to_i64",
    "to_f64",
    "to_bool",
    "repeat",
    "strip_prefix",
    "strip_suffix",
    "pad_left",
    "pad_right",
    "center",
    "slice",
    "substring",
    "clear",
    "truncate",
    "push",
    "push_str",
    "push_char",
    "push_byte",
    "push_utf8",
    "push_json_quoted",
    "parse",
];

/// Returns whether a `Vec` method changes capacity or length and therefore
/// cannot be called on a fixed-size array receiver.
#[must_use]
pub fn is_vec_only_sequence_method(name: &str) -> bool {
    VEC_ONLY_SEQUENCE_METHODS.contains(&name)
}

/// The length- and capacity-changing sequence methods, which only a
/// `Vec<T>` receiver carries.
pub(super) const VEC_ONLY_SEQUENCE_METHODS: &[&str] = &[
    "binary_search",
    "copy_from_slice",
    "copy_within",
    "push",
    "pop",
    "insert",
    "remove",
    "clear",
    "extend",
    "extend_from_slice",
    "truncate",
    "reserve",
    "reserve_exact",
    "capacity",
    "append",
    "resize",
    "resize_with",
    "split_off",
    "drain",
    "shrink_to_fit",
    "dedup",
];

/// Returns whether a method belongs to Gossamer's slice surface. This is the
/// canonical list used by method checking and REPL documentation. Eager
/// iterator combinators remain Vec operations; arrays and slices use `iter()`
/// before applying iterator methods, matching Rust's separation of collection
/// and iterator APIs.
#[must_use]
pub fn is_slice_sequence_method(name: &str) -> bool {
    SLICE_SEQUENCE_METHODS.contains(&name) || COLLECTION_TRAVERSAL_METHODS.contains(&name)
}

/// The slice method surface shared by slices, arrays, and `Vec`.
pub(super) const SLICE_SEQUENCE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "slice",
    "first",
    "last",
    "get",
    "contains",
    "index_of",
    "count_of",
    "sort",
    "sort_by",
    "sort_by_key",
    "reverse",
    "swap",
    "fill",
    "windows",
    "chunks",
    "join",
    "to_vec",
    "iter",
];

/// Returns whether a method is rejected for a tuple receiver. A tuple's
/// elements may differ in type, so nothing that walks it as a sequence of
/// one element type applies: iteration has no element type to yield, and
/// the combinators built on it inherit that. Positional access (`t.0`,
/// `t.get(i)`) and whole-value operations stay available.
#[must_use]
pub fn is_tuple_rejected_method(name: &str) -> bool {
    !is_tuple_method(name)
}

/// Returns whether a method is implemented for a tuple receiver. A tuple is
/// a fixed heterogeneous group, so its surface is whole-value operations
/// plus positional access - the sequence methods have no single element
/// type to act on and no buffer to reorder.
#[must_use]
pub fn is_tuple_method(name: &str) -> bool {
    TUPLE_METHODS.contains(&name)
}

/// The tuple method surface: whole-value operations plus positional access.
pub(super) const TUPLE_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "get",
    "clone",
    "to_string",
    "into",
    "try_into",
];

/// Returns whether a method is implemented for a `Map` receiver. Keeping
/// discovery, type checking, and the runtime on one list stops a sequence
/// method from reaching a map, where it has no ordered buffer to act on and
/// would read as a silent no-op.
#[must_use]
pub fn is_map_method(name: &str) -> bool {
    MAP_METHODS.contains(&name)
        || (COLLECTION_TRAVERSAL_METHODS.contains(&name)
            && !MAP_UNTRAVERSABLE_METHODS.contains(&name)
            && !is_free_call_only_traversal(name))
}

/// Traversals a map cannot answer: its element is a `(K, V)` pair, which
/// neither adds up, multiplies, nor flattens.
pub(super) const MAP_UNTRAVERSABLE_METHODS: &[&str] = &["flatten", "product", "sum"];

/// Whether the runtime binds `name` only as a data-last free call
/// (`iter::filter_map(f, xs)`), with no receiver form on any tier.
#[must_use]
pub fn is_free_call_only_traversal(name: &str) -> bool {
    FREE_CALL_ONLY_TRAVERSALS.contains(&name)
}

/// Traversals with a data-last free call and no receiver form.
///
/// `sort_by` and `sort_by_key` are the only two, and only for the receivers
/// this list gates - `Iterator`, `Range`, and `Set` - which hold no ordered
/// buffer a sort could reorder. `Vec`, an array, and a slice reach them
/// through the slice surface instead, where they sort the receiver in place.
pub(super) const FREE_CALL_ONLY_TRAVERSALS: &[&str] = &["sort_by", "sort_by_key"];

/// Whether a `Set` / `BTreeSet` receiver answers `name`.
#[must_use]
pub fn is_set_method(name: &str) -> bool {
    SET_METHODS.contains(&name) && !is_free_call_only_traversal(name)
}

/// Whether an `Iterator` / `Range` receiver answers `name` in method
/// position. The data-last free surface is wider - it takes an iterator
/// for every name [`is_iterator_method`] lists - so the two differ.
#[must_use]
pub fn iterator_receiver_accepts_method(name: &str) -> bool {
    is_iterator_method(name) && !is_free_call_only_traversal(name)
}

/// The `Map` method surface shared by discovery, type checking, and the
/// runtime.
pub(super) const MAP_METHODS: &[&str] = &[
    "insert",
    "get",
    "get_or",
    "or_insert",
    "remove",
    "pop",
    "contains_key",
    "contains",
    "inc",
    "inc_at",
    "inc_batch",
    "len",
    "is_empty",
    "keys",
    "values",
    "iter",
    "clear",
];

/// Methods a `BTreeMap` has beyond the `Map` surface: its key order makes a
/// first and last entry, and a range of keys, meaningful.
pub(super) const BTREE_MAP_ONLY_METHODS: &[&str] = &[
    "first_key_value",
    "last_key_value",
    "pop_first",
    "pop_last",
    "range",
];

/// Methods a `BTreeSet` has beyond the `Set` surface.
pub(super) const BTREE_SET_ONLY_METHODS: &[&str] =
    &["first", "last", "pop_first", "pop_last", "range"];

/// Whether a `BTreeSet` receiver answers `name`.
#[must_use]
pub fn is_btree_set_method(name: &str) -> bool {
    is_set_method(name) || BTREE_SET_ONLY_METHODS.contains(&name)
}

/// Whether a `BTreeMap` receiver answers `name`.
#[must_use]
pub fn is_btree_map_method(name: &str) -> bool {
    is_map_method(name) || BTREE_MAP_ONLY_METHODS.contains(&name)
}

/// Fixed arrays expose value-preserving `clone` in addition to methods made
/// available through Rust-like array-to-slice receiver coercion.
#[must_use]
pub fn is_array_sequence_method(name: &str) -> bool {
    matches!(name, "clone" | "into") || is_slice_sequence_method(name)
}

/// Returns whether a method is implemented for an `Iterator<T>` receiver on
/// every execution tier. Keep discovery and type checking on this single list
/// so `%info` and `%explain` never advertise eager Vec-only helpers as lazy
/// iterator operations.
#[must_use]
pub fn is_iterator_method(name: &str) -> bool {
    ITERATOR_METHODS.contains(&name)
}

/// Returns whether an iterator method answers with another iterator rather
/// than materialising a value. A name absent from this list is a terminal:
/// it ends the pipeline and produces a concrete result. Type checking and
/// `%info` share this list so a rendered signature cannot drift from the
/// type the call actually has.
#[must_use]
pub fn iterator_adapter_is_lazy(name: &str) -> bool {
    LAZY_ITERATOR_ADAPTERS.contains(&name)
}

/// Methods that traverse a sequence rather than describe or mutate it. A
/// collection does not answer these: `xs.iter()` starts the traversal and the
/// iterator answers them from there. Kept apart from the collection surface so
/// one operation has one spelling instead of an eager and a lazy one.
/// Whether `name` traverses a collection's elements.
#[must_use]
pub fn is_collection_traversal_method(name: &str) -> bool {
    COLLECTION_TRAVERSAL_METHODS.contains(&name)
}

pub(super) const COLLECTION_TRAVERSAL_METHODS: &[&str] = &[
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "scan",
    "take",
    "take_while",
    "skip",
    "skip_while",
    "step_by",
    "enumerate",
    "zip",
    "chain",
    "rev",
    "flatten",
    "pairwise",
    "fold",
    "reduce",
    "for_each",
    "sum",
    "sum_by",
    "product",
    "product_by",
    "min",
    "max",
    "min_by",
    "max_by",
    "min_by_key",
    "max_by_key",
    "any",
    "all",
    "find",
    "find_map",
    "position",
    "count",
    "partition",
    "unzip",
    "chunk_by",
    "count_by",
];

/// Iterator adapters that answer with another iterator on every tier.
pub(super) const LAZY_ITERATOR_ADAPTERS: &[&str] = &[
    "take",
    "skip",
    "step_by",
    "enumerate",
    "chain",
    "zip",
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "scan",
    "take_while",
    "skip_while",
    "rev",
];

/// Whether `owner` is a core type that already declares `name` itself.
///
/// The inverse-safe form of [`core_type_accepts_method`]: an owner with no
/// table here answers `false`, so only a name a core type genuinely carries is
/// reported. A type's own surface answers a call before any `impl` block a
/// program writes for it, so a block declaring one of these names declares a
/// method no call on that type can reach.
#[must_use]
pub fn core_type_declares_method(owner: &str, name: &str) -> bool {
    core_type_own_method_names(owner).is_some_and(|names| names.contains(&name))
}

/// The method names a core type carries itself, keyed by the name an `impl`
/// block's owner is written under. `None` is a type with no such surface.
///
/// One table answers both readers, which have to agree: the checker asks
/// whether a call is already answered before it consults a user block, and
/// the bytecode VM asks the same of an `impl` block as it loads one. A name
/// every type derives - equality, ordering, hashing, formatting, copying - is
/// absent here on purpose, because a written `impl` of one of those overrides
/// the derived behaviour rather than being answered before it.
/// The parallel twins of the eager walks, on every sequence and on an
/// integer range.
pub(super) const PARALLEL_ADAPTER_METHODS: &[&str] = &[
    "par_filter",
    "par_map",
    "par_max",
    "par_min",
    "par_reduce",
    "par_sum",
];

/// The associated functions a built-in type answers as `Type::name(..)`
/// besides its methods, or `None` for a type this table does not cover.
pub(super) fn builtin_type_constructors(owner: &str) -> Option<&'static [&'static str]> {
    Some(match owner {
        "String" => &["new", "from", "with_capacity", "from_utf8"],
        "Vec" => &["new", "from", "with_capacity"],
        "Map" | "BTreeMap" => &["new", "from", "with_capacity"],
        "Set" | "BTreeSet" => &["new", "from", "with_capacity"],
        _ => return None,
    })
}

pub(super) fn core_type_own_method_names(owner: &str) -> Option<Vec<&'static str>> {
    // A tuple `impl` registers under the arity its receiver carries; the
    // surface is the one every tuple shares.
    let owner = if owner.starts_with("tuple_") {
        "Tuple"
    } else {
        owner
    };
    let names: Vec<&'static str> = match owner {
        "String" => STRING_METHODS.to_vec(),
        // `to_vec` converts a borrowed or fixed sequence into an owned one, so
        // it is not among a Vec's own names.
        "Vec" => SLICE_SEQUENCE_METHODS
            .iter()
            .chain(VEC_ONLY_SEQUENCE_METHODS)
            .chain(SEQUENCE_COMBINATOR_METHODS)
            .chain(PARALLEL_ADAPTER_METHODS)
            .filter(|name| **name != "to_vec")
            .copied()
            .collect(),
        "Slice" | "Array" => SLICE_SEQUENCE_METHODS
            .iter()
            .chain(PARALLEL_ADAPTER_METHODS)
            .copied()
            .collect(),
        "Map" => MAP_METHODS.to_vec(),
        "BTreeMap" => MAP_METHODS
            .iter()
            .chain(BTREE_MAP_ONLY_METHODS)
            .copied()
            .collect(),
        "Set" => SET_METHODS.to_vec(),
        "BTreeSet" => SET_METHODS
            .iter()
            .chain(BTREE_SET_ONLY_METHODS)
            .copied()
            .collect(),
        "Iterator" | "Range" => ITERATOR_METHODS.to_vec(),
        "Tuple" => TUPLE_METHODS.to_vec(),
        _ => return None,
    };
    Some(names)
}

/// An owner this does not model answers `true`, so a surface it has no
/// table for keeps whatever the runtime registered.
#[must_use]
pub fn core_type_accepts_method(owner: &str, name: &str) -> bool {
    // Equality, ordering, hashing, formatting, and copying are derived for
    // every type, so no per-owner table lists them.
    if AUTOMATIC_METHODS.contains(&name) {
        return true;
    }
    match owner {
        "Iterator" | "Range" => iterator_receiver_accepts_method(name),
        "Map" => is_map_method(name),
        "BTreeMap" => is_btree_map_method(name),
        "Set" => is_set_method(name),
        "BTreeSet" => is_btree_set_method(name),
        "Vec" => {
            is_slice_sequence_method(name)
                || is_vec_only_sequence_method(name)
                || SEQUENCE_COMBINATOR_METHODS.contains(&name)
        }
        "Slice" => is_slice_sequence_method(name),
        "Array" => is_array_sequence_method(name),
        "String" => STRING_METHODS.contains(&name),
        "Tuple" => is_tuple_method(name),
        _ => true,
    }
}
