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

pub const COMPRESS_GZIP: StdModule = StdModule {
    path: "std::compress::gzip",
    summary: "gzip encoder / decoder (RFC 1952; flate2-backed).",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Compresses bytes at `level`, `0` (store only) to `9` (best); gzip(1) uses `6`.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Decompresses a gzip-formatted payload.",
        },
    ],
};

pub const COMPRESS_FLATE: StdModule = StdModule {
    path: "std::compress::flate",
    summary: "Raw DEFLATE (RFC 1951) encoder / decoder.",
    items: &[
        StdItem {
            name: "compress",
            kind: StdItemKind::Function,
            doc: "One-shot DEFLATE compress.",
        },
        StdItem {
            name: "decompress",
            kind: StdItemKind::Function,
            doc: "One-shot DEFLATE decompress.",
        },
    ],
};

pub const COMPRESS_ZLIB: StdModule = StdModule {
    path: "std::compress::zlib",
    summary: "zlib (RFC 1950) encoder / decoder.",
    items: &[
        StdItem {
            name: "compress",
            kind: StdItemKind::Function,
            doc: "One-shot zlib compress.",
        },
        StdItem {
            name: "decompress",
            kind: StdItemKind::Function,
            doc: "One-shot zlib decompress.",
        },
    ],
};

pub const COMPRESS_BZIP2: StdModule = StdModule {
    path: "std::compress::bzip2",
    summary: "bzip2 encoder / decoder (BZh format).",
    items: &[
        StdItem {
            name: "compress",
            kind: StdItemKind::Function,
            doc: "One-shot bzip2 compress.",
        },
        StdItem {
            name: "decompress",
            kind: StdItemKind::Function,
            doc: "One-shot bzip2 decompress.",
        },
    ],
};

pub const COMPRESS_ZSTD: StdModule = StdModule {
    path: "std::compress::zstd",
    summary: "Zstandard encoder / decoder (RFC 8478; libzstd-vendored).",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "One-shot Zstandard compress at the default level (3).",
        },
        StdItem {
            name: "encode_level",
            kind: StdItemKind::Function,
            doc: "One-shot Zstandard compress at the supplied level (1 fastest -- 22 best).",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "One-shot Zstandard decompress.",
        },
    ],
};
