//! Typed runtime ABI registry for the Gossamer compiler.
//!
//! A single source of truth for every `gos_rt_*` symbol's name and
//! C-ABI signature. Consumers (LLVM lowerer, Cranelift backend,
//! dispatch-consistency verifier) all derive their declarations from
//! this registry instead of maintaining parallel string arrays.

/// Reference-counting type-meta ABI (kind tags + blob layout) shared by
/// the MIR lowerer and the runtime.
pub mod format_pad;
pub mod int_range;
pub mod jit_carrier;
pub mod rc;
/// ABI registry - the typed list of all `gos_rt_*` symbols.
pub mod registry;
pub mod string_layout;
/// Core ABI types: [`AbiType`], [`AbiSig`], [`RuntimeEntry`].
pub mod types;

/// Tag byte introducing a nested tuple in a `gos_rt_tuple_format` /
/// `gos_rt_tuple_cmp` tag stream. The next byte is the nested tuple's
/// element count, followed by that many tags, recursively. A nested
/// tuple's slots are flattened into the parent's buffer, so the stream
/// is walked with separate tag and slot cursors.
pub const TUPLE_TAG_NESTED: u8 = 8;

/// `payload_kind` tag selecting rendering of an `Option` / `Result` payload
/// through the payload type's derived `fmt`, whose address travels alongside
/// the tag in `gos_rt_debug_option_fmt` / `gos_rt_debug_result_fmt`.
pub const DEBUG_PAYLOAD_ADT: u8 = 9;
/// Payload kind for a tuple: the word is its slot buffer and the companion
/// pointer addresses a tag stream opening with the nested marker and arity.
pub const DEBUG_PAYLOAD_TUPLE: u8 = 11;

/// Descriptor tag for a `Vec` / slice: the slot holds its handle and the
/// element's own descriptor follows this byte.
pub const DESC_VEC: u8 = 12;
/// Descriptor tag for a map: the slot holds its handle and the key's
/// descriptor follows this byte, then the value's.
pub const DESC_MAP: u8 = 13;
/// Descriptor tag for an `i64`-element set; the next byte is `1` for the
/// ordered spelling.
pub const DESC_SET_I64: u8 = 14;
/// Descriptor tag for a `String`-element set; the next byte is `1` for the
/// ordered spelling.
pub const DESC_SET_STR: u8 = 15;
/// Payload kind rendering an `Option` / `Result` payload through the
/// descriptor stream the companion pointer addresses.
pub const DEBUG_PAYLOAD_DESC: u8 = 12;
/// Payload kind for a unit payload: the arm carries no value and renders
/// as `()`, which is what `Result<(), E>` shows on its `Ok` side.
pub const DEBUG_PAYLOAD_UNIT: u8 = 13;

/// Descriptor tag for a nested `Result`: the slot holds a pointer to the
/// two-word `[disc, payload]` pair, and the Ok arm's descriptor follows
/// this byte, then the Err arm's.
pub const DESC_RESULT: u8 = 16;
/// Descriptor tag for a nested `Option`, laid out as `DESC_RESULT` with
/// only the Some arm's descriptor following.
pub const DESC_OPTION: u8 = 17;
/// Descriptor tag for an `errors::Error`: the slot holds the error
/// pointer, rendered as the colon-joined cause chain `{}` shows.
pub const DESC_ERROR: u8 = 18;
/// Descriptor tag for a user struct or enum. Three bytes follow: the index
/// of the type's derived `fmt` in the formatter table passed alongside the
/// descriptor, whether that `fmt` takes the address of the value's slots (a
/// struct) rather than the slot word itself (an enum), and how many slots the
/// value occupies where it is stored inline, as a tuple field is.
pub const DESC_ADT: u8 = 19;

/// Ordering-descriptor tag for a user enum. Three bytes follow: whether the
/// value is stored inline as `[disc, payload]` rather than as a counted node
/// pointer, the variant count, and then, per variant, its field count
/// followed by that many field descriptors.
pub const DESC_ENUM: u8 = 21;
/// Ordering-descriptor tag for a field whose type is the enum the enclosing
/// [`DESC_ENUM`] describes, read through the same descriptor.
pub const DESC_SELF: u8 = 22;

/// Descriptor tag for a container whose elements live in the runtime - a
/// `Deque`, `Queue`, `Stack`, `MaxHeap`, or `MinHeap`. The slot holds the
/// handle; one byte follows naming which container (the `u32::MAX -`
/// offset of its sentinel `DefId`: 19, 28, 30, 31, 32), then the
/// element's own descriptor.
pub const DESC_CONTAINER: u8 = 23;

/// Descriptor tag for a fixed-size array stored inline. Four bytes follow -
/// the element count and how many slots one element spans, each a
/// little-endian `u16` - then the element's own descriptor once.
pub const DESC_ARRAY: u8 = 20;

/// Ordering-descriptor tag for a struct whose fields narrower than a word sit
/// at their own width. Two bytes follow - the words the value spans and its
/// leaf count - then, per leaf in field order with nested structs flattened,
/// the leaf's byte offset as a little-endian `u16` and its [`packed_leaf`]
/// kind.
pub const DESC_PACKED: u8 = 24;

/// How a [`DESC_PACKED`] leaf is stored and ordered.
pub mod packed_leaf {
    /// A signed integer in one byte.
    pub const I8: u8 = 0;
    /// An unsigned integer in one byte.
    pub const U8: u8 = 1;
    /// A signed integer in two bytes.
    pub const I16: u8 = 2;
    /// An unsigned integer in two bytes.
    pub const U16: u8 = 3;
    /// A signed integer in four bytes.
    pub const I32: u8 = 4;
    /// An unsigned integer in four bytes.
    pub const U32: u8 = 5;
    /// A signed integer in a word.
    pub const I64: u8 = 6;
    /// An unsigned integer in a word.
    pub const U64: u8 = 7;
    /// A float in a word, held as a double.
    pub const FLOAT: u8 = 8;
    /// A `bool` in one byte.
    pub const BOOL: u8 = 9;
    /// A `char` in four bytes.
    pub const CHAR: u8 = 10;
}

/// Runtime shims that invoke a gossamer callback through
/// `extern "C" fn(..) -> i128`, reading the callback's address from offset
/// zero of the closure env blob handed to the shim.
///
/// The two-word `[disc, payload]` carrier comes back in a vector register
/// under the Win64 ABI and in the integer-register pair under System V, so a
/// backend targeting Windows hands the runtime a vector-returning wrapper in
/// place of the callback's own address. Callbacks that answer `i64`, `bool`,
/// or `f64` agree on the register already and are not listed.
///
/// The sequence combinators are absent here and answered by
/// [`reads_carrier_from_callback`] instead: each has one shim per element
/// class, and which classes exist is the registry's to say.
pub const I128_CALLBACK_SHIMS: &[&str] = &[
    "gos_rt_fs_walk_dir_raw",
    "gos_rt_lazy_iter_filter_map_i64",
    "gos_rt_lazy_iter_filter_map_str",
    "gos_rt_option_and_then",
    "gos_rt_option_or_else",
    "gos_rt_result_and_then",
    "gos_rt_result_or_else",
];

/// Sequence combinators whose callback answers the two-word carrier, named
/// without an element class. Every registry shim implementing one of these
/// reads a carrier back, whichever class its symbol carries.
const I128_CALLBACK_COMBINATORS: &[&str] = &["filter_map", "find_map"];

/// Whether `name` invokes a gossamer callback that answers the two-word
/// `[disc, payload]` carrier, so a Win64 backend must hand the runtime a
/// vector-returning wrapper in place of the callback's own address.
///
/// A combinator shim is recognised through its registry entry rather than by
/// spelling, so a class the registry gains is covered the moment it is
/// declared. See [`I128_CALLBACK_SHIMS`].
#[must_use]
pub fn reads_carrier_from_callback(name: &str) -> bool {
    if I128_CALLBACK_SHIMS.contains(&name) {
        return true;
    }
    // Every combinator shim is spelled under this prefix, so a name outside
    // it needs no registry lookup.
    if !name.starts_with("gos_rt_iter_") {
        return false;
    }
    registry::lookup(name).is_some_and(|entry| {
        entry
            .combinator
            .is_some_and(|abi| I128_CALLBACK_COMBINATORS.contains(&abi.combinator))
    })
}

/// Content-keyed map and set entry points that take a key. Each reads the key's
/// slots through the layout descriptor that follows it, so every backend passes
/// the key by address: an aggregate's own storage, or a slot holding a one-word
/// value such as a `Vec` or `String` pointer, or a two-word `Option` carrier.
pub const SKEY_BY_ADDRESS: &[&str] = &[
    "gos_rt_map_contains_skey",
    "gos_rt_map_get_or_skey",
    "gos_rt_map_get_skey_opt",
    "gos_rt_map_inc_skey",
    "gos_rt_map_insert_skey",
    "gos_rt_map_insert_skey_opt",
    "gos_rt_map_or_insert_skey",
    "gos_rt_map_pop_skey",
    "gos_rt_set_contains_skey",
    "gos_rt_set_insert_skey",
    "gos_rt_set_remove_skey",
];

/// Whether `name` is a content-keyed entry point whose second argument, the
/// key, crosses by address. See [`SKEY_BY_ADDRESS`].
#[must_use]
pub fn takes_key_by_address(name: &str) -> bool {
    SKEY_BY_ADDRESS.contains(&name)
}

/// Ordered-container pushes whose second argument, the element, crosses as
/// the address of its slots: the store copies its own stride from it, and the
/// sift compares through the descriptor that follows.
pub const ELEM_BY_ADDRESS: &[&str] = &["gos_rt_bheap_max_push_desc", "gos_rt_bheap_min_push_desc"];

/// Whether `name` takes its element by address. See [`ELEM_BY_ADDRESS`].
#[must_use]
pub fn takes_elem_by_address(name: &str) -> bool {
    ELEM_BY_ADDRESS.contains(&name)
}

/// Runtime registration shims that store a gossamer handler's address and
/// later invoke it as `extern "C" fn(..) -> i128`, paired with the position
/// of the address argument in the shim's signature. Same carrier-register
/// rule as [`I128_CALLBACK_SHIMS`], reached through a stored handler rather
/// than an env blob.
pub const I128_HANDLER_REGISTRATIONS: &[(&str, usize)] = &[
    ("gos_rt_http2_bind_and_run_h2c", 2),
    ("gos_rt_http3_serve", 4),
    ("gos_rt_http_serve", 2),
    ("gos_rt_http_serve_tls", 4),
    ("gos_rt_http_server_serve", 2),
    ("gos_rt_httptest_record", 4),
    ("gos_rt_middleware_new", 1),
    ("gos_rt_middleware_new_kind", 1),
    ("gos_rt_router_add", 4),
    ("gos_rt_router_add_fn", 3),
    ("gos_rt_router_delete", 3),
    ("gos_rt_router_delete_fn", 2),
    ("gos_rt_router_get", 3),
    ("gos_rt_router_get_fn", 2),
    ("gos_rt_router_head", 3),
    ("gos_rt_router_head_fn", 2),
    ("gos_rt_router_options", 3),
    ("gos_rt_router_options_fn", 2),
    ("gos_rt_router_patch", 3),
    ("gos_rt_router_patch_fn", 2),
    ("gos_rt_router_post", 3),
    ("gos_rt_router_post_fn", 2),
    ("gos_rt_router_put", 3),
    ("gos_rt_router_put_fn", 2),
];

pub use registry::{
    REGISTRY, all_llvm_declarations, answers_counted_element, answers_counted_payload,
    combinator_abi_of, combinator_crossings, combinator_symbol, lookup, mints_owned_string,
    returns_fresh_aggregate,
};
pub use types::{AbiSig, AbiType, CombinatorAbi, ElemClass, RuntimeEntry, Tier};

/// The overflow-checked runtime entry for an integer shim, for the build
/// profiles in which `+` and `*` panic on overflow. `sum` and `product` over
/// integers, and an open `start..` range stepping past its type, follow the
/// same rule as the operators they are built from.
#[must_use]
pub fn checked_integer_entry(name: &str) -> Option<&'static str> {
    match name {
        "gos_rt_lazy_iter_range_from_i64" => Some("gos_rt_lazy_iter_range_from_i64_checked"),
        "gos_rt_iter_sum_i64" => Some("gos_rt_iter_sum_i64_checked"),
        "gos_rt_iter_product_i64" => Some("gos_rt_iter_product_i64_checked"),
        "gos_rt_lazy_iter_sum_i64" => Some("gos_rt_lazy_iter_sum_i64_checked"),
        "gos_rt_lazy_iter_product_i64" => Some("gos_rt_lazy_iter_product_i64_checked"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A combinator shim exists once per element class, so recognising the
    /// carrier crossing by spelling loses whichever class nobody wrote down.
    /// The registry is what knows the classes, and the answer follows it.
    #[test]
    fn every_element_class_of_a_carrier_combinator_reads_a_carrier() {
        for combinator in I128_CALLBACK_COMBINATORS {
            let shims: Vec<&str> = registry::REGISTRY
                .iter()
                .filter(|entry| {
                    entry
                        .combinator
                        .is_some_and(|abi| abi.combinator == *combinator)
                })
                .map(|entry| entry.name)
                .collect();
            assert_eq!(
                shims.len(),
                3,
                "{combinator} has one shim per element class: {shims:?}"
            );
            for name in shims {
                assert!(
                    reads_carrier_from_callback(name),
                    "{name} answers a carrier and needs the Win64 wrapper"
                );
            }
        }
    }

    /// A callback that answers a word agrees on the register already, so
    /// wrapping it would hand the runtime a vector register it never reads.
    #[test]
    fn a_word_answering_callback_reads_no_carrier() {
        assert!(!reads_carrier_from_callback("gos_rt_iter_map_ptr_i64"));
        assert!(!reads_carrier_from_callback("gos_rt_iter_filter_i64"));
    }

    #[test]
    fn win64_marshals_fat_i128_across_the_ffi_boundary() {
        // A scalar `i128` by value has no stable `extern "C"` ABI on Win64
        // (llc uses a GP register pair; rustc uses a by-pointer arg + a
        // `<16 x i8>` return). The Windows declaration must therefore render a
        // Fat `i128` argument as `ptr` and a Fat `i128` return as `<16 x i8>`,
        // matching the call-site marshalling. SysV keeps bare `i128`.
        let entry =
            lookup("gos_rt_result_default_with").expect("gos_rt_result_default_with is registered");
        assert_eq!(entry.sig.ret, types::AbiType::I64);
        assert_eq!(entry.sig.params[0], types::AbiType::I128);

        let win = entry.llvm_declare_for(true);
        assert!(
            win.contains("@gos_rt_result_default_with(ptr,"),
            "Win64 must pass a Fat i128 argument by pointer: {win}"
        );
        assert!(
            !win.contains("i128"),
            "Win64 declaration must not contain a bare i128: {win}"
        );

        let sysv = lookup("gos_rt_result_new")
            .expect("gos_rt_result_new is registered")
            .llvm_declare_for(false);
        assert!(
            sysv.starts_with("declare i128 @gos_rt_result_new("),
            "SysV must keep the bare i128 return: {sysv}"
        );
        let win_ret = lookup("gos_rt_result_new").unwrap().llvm_declare_for(true);
        assert!(
            win_ret.starts_with("declare <16 x i8> @gos_rt_result_new("),
            "Win64 must return a Fat i128 in a 16-byte vector register: {win_ret}"
        );
    }

    #[test]
    fn registry_is_sorted() {
        let names: Vec<&str> = REGISTRY.iter().map(|e| e.name).collect();

        let mut sorted = names.clone();
        sorted.sort_unstable();

        if let Some((idx, (actual, expected))) = names
            .iter()
            .zip(sorted.iter())
            .enumerate()
            .find(|(_, (a, b))| a != b)
        {
            panic!("REGISTRY out of order at index {idx}: found {actual:?}, expected {expected:?}");
        }
    }

    #[test]
    fn registry_no_duplicates() {
        let mut names: Vec<&str> = REGISTRY.iter().map(|e| e.name).collect();
        let orig_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(orig_len, names.len(), "REGISTRY contains duplicate entries");
    }

    #[test]
    fn all_names_have_gos_rt_prefix() {
        for entry in REGISTRY {
            assert!(
                entry.name.starts_with("gos_rt_"),
                "entry {:?} does not start with gos_rt_",
                entry.name
            );
        }
    }

    #[test]
    fn llvm_declare_round_trips() {
        for entry in REGISTRY {
            let decl = entry.llvm_declare();
            assert!(
                decl.starts_with("declare "),
                "malformed declare for {}: {}",
                entry.name,
                decl
            );
            assert!(
                decl.contains(&format!("@{}", entry.name)),
                "declare does not contain symbol name for {0}: {1}",
                entry.name,
                decl
            );
        }
    }

    #[test]
    fn registry_size_sanity() {
        assert!(
            REGISTRY.len() > 200,
            "only {} entries - registry likely truncated",
            REGISTRY.len()
        );
    }

    #[test]
    fn void_return_only_for_void_type() {
        use AbiType::Void;
        for entry in REGISTRY {
            if entry.sig.ret == Void {
                let decl = entry.llvm_declare();
                assert!(
                    decl.starts_with("declare void "),
                    "void return type must produce 'declare void' for {0}: {1}",
                    entry.name,
                    decl
                );
            }
        }
    }

    #[test]
    fn docs_field_is_non_empty() {
        for entry in REGISTRY {
            assert!(
                !entry.docs.is_empty(),
                "entry {0} has an empty docs field",
                entry.name
            );
        }
    }

    #[test]
    fn tier_field_coverage() {
        use types::Tier;
        let both = REGISTRY.iter().filter(|e| e.tier == Tier::Both).count();
        let cl = REGISTRY
            .iter()
            .filter(|e| e.tier == Tier::Cranelift)
            .count();
        let ll = REGISTRY.iter().filter(|e| e.tier == Tier::Llvm).count();
        assert!(both >= 30, "expected >=30 Both-tier entries, got {both}");
        assert!(cl >= 100, "expected >=100 Cranelift-tier entries, got {cl}");
        assert!(ll >= 1, "expected >=1 Llvm-tier entries, got {ll}");
        assert_eq!(
            both + cl + ll,
            REGISTRY.len(),
            "every entry must have a tier"
        );
    }
}
