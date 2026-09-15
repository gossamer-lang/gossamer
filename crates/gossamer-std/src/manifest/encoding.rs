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

pub const ENCODING_BASE64: StdModule = StdModule {
    path: "std::encoding::base64",
    summary: "RFC 4648 base64 encode/decode.",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Encodes bytes to a base64 string.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Decodes a base64 string.",
        },
    ],
};

pub const ENCODING_HEX: StdModule = StdModule {
    path: "std::encoding::hex",
    summary: "Lowercase hex encode/decode.",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Encodes bytes to hex.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Decodes a hex string.",
        },
    ],
};

pub const ENCODING_YAML: StdModule = StdModule {
    path: "std::encoding::yaml",
    summary: "YAML 1.2 parser/emitter (serde_norway-backed).",
    items: &[
        StdItem {
            name: "Value",
            kind: StdItemKind::Type,
            doc: "Dynamically typed YAML value.",
        },
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Parses a YAML document into a Value.",
        },
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Encodes a Value as a YAML document.",
        },
        StdItem {
            name: "parse_all",
            kind: StdItemKind::Function,
            doc: "Parses a multi-document YAML stream into a Vec<Value>.",
        },
        StdItem {
            name: "to_json",
            kind: StdItemKind::Function,
            doc: "Converts a YAML document to JSON text.",
        },
        StdItem {
            name: "from_json",
            kind: StdItemKind::Function,
            doc: "Converts JSON text to a YAML document.",
        },
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Reports whether the text is well-formed YAML.",
        },
    ],
};

pub const ENCODING_JSON: StdModule = StdModule {
    path: "std::encoding::json",
    summary: "JSON parser, emitter, and derive support.",
    items: &[
        StdItem {
            name: "Serialize",
            kind: StdItemKind::Trait,
            doc: "Trait for converting a value to JSON.",
        },
        StdItem {
            name: "Deserialize",
            kind: StdItemKind::Trait,
            doc: "Trait for parsing a value from JSON.",
        },
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Encodes a `Serialize` value as a JSON `String`.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Decodes a JSON `String` into a `Deserialize` value.",
        },
        StdItem {
            name: "Value",
            kind: StdItemKind::Type,
            doc: "Dynamically typed JSON value.",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Error raised by encoding/decoding operations.",
        },
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Parses JSON text into a dynamic Value.",
        },
        StdItem {
            name: "render",
            kind: StdItemKind::Function,
            doc: "Renders a dynamic Value as compact JSON text.",
        },
        StdItem {
            name: "encode_pretty",
            kind: StdItemKind::Function,
            doc: "Renders a value as indented JSON text.",
        },
        StdItem {
            name: "valid",
            kind: StdItemKind::Function,
            doc: "Reports whether the text is well-formed JSON.",
        },
        StdItem {
            name: "get",
            kind: StdItemKind::Function,
            doc: "Looks up an object field on a dynamic Value.",
        },
        StdItem {
            name: "set",
            kind: StdItemKind::Function,
            doc: "Sets an object field on a dynamic Value.",
        },
        StdItem {
            name: "at",
            kind: StdItemKind::Function,
            doc: "Indexes an array element on a dynamic Value. An index the \
                  array does not have reads as JSON null.",
        },
        StdItem {
            name: "keys",
            kind: StdItemKind::Function,
            doc: "Object field names of a dynamic Value.",
        },
        StdItem {
            name: "len",
            kind: StdItemKind::Function,
            doc: "Element / field count of a dynamic Value.",
        },
        StdItem {
            name: "is_null",
            kind: StdItemKind::Function,
            doc: "Reports whether a dynamic Value is null.",
        },
        StdItem {
            name: "as_str",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as Option<String>.",
        },
        StdItem {
            name: "as_i64",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as Option<i64>.",
        },
        StdItem {
            name: "as_u64",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as Option<u64>, for a non-negative integer up to u64::MAX.",
        },
        StdItem {
            name: "as_f64",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as Option<f64>.",
        },
        StdItem {
            name: "as_bool",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as Option<bool>.",
        },
        StdItem {
            name: "as_array",
            kind: StdItemKind::Function,
            doc: "Reads a dynamic Value as an array of Values.",
        },
    ],
};

pub const ENCODING_CSV: StdModule = StdModule {
    path: "std::encoding::csv",
    summary: "CSV record reader and writer.",
    items: &[
        StdItem {
            name: "read",
            kind: StdItemKind::Function,
            doc: "Parses all CSV records from a string.",
        },
        StdItem {
            name: "parse_line",
            kind: StdItemKind::Function,
            doc: "Parses a single CSV-formatted line.",
        },
        StdItem {
            name: "write",
            kind: StdItemKind::Function,
            doc: "Serialises records as a CSV string.",
        },
    ],
};

pub const ENCODING_PEM: StdModule = StdModule {
    path: "std::encoding::pem",
    summary: "PEM block encoder and decoder.",
    items: &[
        StdItem {
            name: "Block",
            kind: StdItemKind::Type,
            doc: "A decoded PEM block with type label and DER bytes.",
        },
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Encodes a Block as a PEM string.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Decodes the first PEM block from a string.",
        },
        StdItem {
            name: "decode_all",
            kind: StdItemKind::Function,
            doc: "Decodes all PEM blocks from a string.",
        },
    ],
};

pub const ENCODING_BINARY: StdModule = StdModule {
    path: "std::encoding::binary",
    summary: "Big/little-endian integer packing and varint codecs.",
    items: &[
        StdItem {
            name: "get_u8",
            kind: StdItemKind::Function,
            doc: "Reads a single byte.",
        },
        StdItem {
            name: "put_u8",
            kind: StdItemKind::Function,
            doc: "Writes a single byte.",
        },
        StdItem {
            name: "get_u16_be",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u16.",
        },
        StdItem {
            name: "put_u16_be",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u16.",
        },
        StdItem {
            name: "get_u16_le",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u16.",
        },
        StdItem {
            name: "put_u16_le",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u16.",
        },
        StdItem {
            name: "get_u32_be",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u32.",
        },
        StdItem {
            name: "put_u32_be",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u32.",
        },
        StdItem {
            name: "get_u32_le",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u32.",
        },
        StdItem {
            name: "put_u32_le",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u32.",
        },
        StdItem {
            name: "get_u64_be",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u64.",
        },
        StdItem {
            name: "put_u64_be",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u64.",
        },
        StdItem {
            name: "get_u64_le",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u64.",
        },
        StdItem {
            name: "put_u64_le",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u64.",
        },
        StdItem {
            name: "get_u16_be_at",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u16 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u16_be_at",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u16 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "get_u16_le_at",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u16 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u16_le_at",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u16 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "get_u32_be_at",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u32 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u32_be_at",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u32 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "get_u32_le_at",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u32 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u32_le_at",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u32 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "get_u64_be_at",
            kind: StdItemKind::Function,
            doc: "Reads a big-endian u64 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u64_be_at",
            kind: StdItemKind::Function,
            doc: "Writes a big-endian u64 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "get_u64_le_at",
            kind: StdItemKind::Function,
            doc: "Reads a little-endian u64 at a byte offset of an existing buffer. An offset plus width past the end is an Err, never a zero-fill.",
        },
        StdItem {
            name: "put_u64_le_at",
            kind: StdItemKind::Function,
            doc: "Writes a little-endian u64 at a byte offset, in place through the caller's buffer. An offset plus width past the end is an Err.",
        },
        StdItem {
            name: "uvarint",
            kind: StdItemKind::Function,
            doc: "Decodes an unsigned varint.",
        },
        StdItem {
            name: "varint",
            kind: StdItemKind::Function,
            doc: "Decodes a signed varint (zigzag).",
        },
        StdItem {
            name: "put_uvarint",
            kind: StdItemKind::Function,
            doc: "Encodes an unsigned varint.",
        },
        StdItem {
            name: "put_varint",
            kind: StdItemKind::Function,
            doc: "Encodes a signed varint (zigzag).",
        },
    ],
};

pub const ENCODING_XML: StdModule = StdModule {
    path: "std::encoding::xml",
    summary: "Streaming XML decoder + builder (quick-xml).",
    items: &[
        StdItem {
            name: "Reader",
            kind: StdItemKind::Type,
            doc: "Pull-style XML reader.",
        },
        StdItem {
            name: "Writer",
            kind: StdItemKind::Type,
            doc: "Streaming XML writer.",
        },
        StdItem {
            name: "Event",
            kind: StdItemKind::Type,
            doc: "Start / End / Text / CData / Comment / Eof.",
        },
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Parses an XML document into a Vec of events.",
        },
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Serialises a sequence of events to XML text.",
        },
        StdItem {
            name: "escape",
            kind: StdItemKind::Function,
            doc: "Escapes XML metacharacters in text.",
        },
    ],
};

pub const ENCODING_BASE32: StdModule = StdModule {
    path: "std::encoding::base32",
    summary: "RFC 4648 base32 (uppercase) encode / decode.",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Bytes -> base32 string.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "Base32 string -> bytes.",
        },
        StdItem {
            name: "encode_string",
            kind: StdItemKind::Function,
            doc: "Encodes a String as standard base32 text.",
        },
        StdItem {
            name: "decode_string",
            kind: StdItemKind::Function,
            doc: "Decodes standard base32 text into a String.",
        },
        StdItem {
            name: "encode_hex",
            kind: StdItemKind::Function,
            doc: "Encodes a String as extended-hex base32 text.",
        },
        StdItem {
            name: "decode_hex",
            kind: StdItemKind::Function,
            doc: "Decodes extended-hex base32 text into a String.",
        },
    ],
};

pub const ENCODING_ASCII85: StdModule = StdModule {
    path: "std::encoding::ascii85",
    summary: "ASCII85 / base85 encode / decode.",
    items: &[
        StdItem {
            name: "encode",
            kind: StdItemKind::Function,
            doc: "Bytes -> ASCII85 string.",
        },
        StdItem {
            name: "decode",
            kind: StdItemKind::Function,
            doc: "ASCII85 string -> bytes.",
        },
    ],
};

pub const ENCODING_TOML: StdModule = StdModule {
    path: "std::encoding::toml",
    summary: "TOML 1.0 parsing + emission. Pair with the turbofish `from_toml::<Type>` for typed decoding (struct auto-derive).",
    items: &[
        StdItem {
            name: "to_json",
            kind: StdItemKind::Function,
            doc: "Convert TOML text to JSON text; returns Result<String, errors::Error>.",
        },
        StdItem {
            name: "from_json",
            kind: StdItemKind::Function,
            doc: "Render JSON text as TOML text; returns Result<String, errors::Error>.",
        },
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as TOML.",
        },
        StdItem {
            name: "pretty",
            kind: StdItemKind::Function,
            doc: "Round-trip TOML through the pretty-printer.",
        },
    ],
};
