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

pub const NET_MAIL: StdModule = StdModule {
    path: "std::net::mail",
    summary: "Email addresses (RFC 5322): parse a mailbox or an address list, and write one in the form a header carries.",
    items: &[
        StdItem {
            name: "parse_address",
            kind: StdItemKind::Function,
            doc: "`parse_address(text) -> Result<Address, errors::Error>`: the one mailbox `text` names, `Name <local@domain>` or a bare `local@domain` (a trailing comment is read as the name), or the one member of a group. An encoded word (RFC 2047) in the name is decoded. More than one address, or text that is not one, is an `Err`.",
        },
        StdItem {
            name: "parse_address_list",
            kind: StdItemKind::Function,
            doc: "`parse_address_list(text) -> Result<Vec<Address>, errors::Error>`: the mailboxes of an address list (`Ann <ann@example.com>, team: bob@example.com;`), groups flattened and empty entries skipped. A malformed entry, or two entries with no comma between them, is an `Err`.",
        },
        StdItem {
            name: "Address",
            kind: StdItemKind::Type,
            doc: "A mailbox: `name` (the display name, empty when there is none) and `email`. `{}` writes the form a header field takes: `Name <email>`, the name quoted when it holds a character an atom cannot and written as RFC 2047 encoded words when it is not printable ASCII, and the bare address when there is no name.",
        },
    ],
};

pub const NET_URL: StdModule = StdModule {
    path: "std::net::url",
    summary: "Network URL parsing and component escaping; never use filesystem-path rules.",
    items: &[
        StdItem {
            name: "Url",
            kind: StdItemKind::Type,
            doc: "Parsed URL.",
        },
        StdItem {
            name: "query_escape",
            kind: StdItemKind::Function,
            doc: "Percent-encodes a query parameter.",
        },
        StdItem {
            name: "query_unescape",
            kind: StdItemKind::Function,
            doc: "Inverse of `query_escape`.",
        },
        StdItem {
            name: "path_escape",
            kind: StdItemKind::Function,
            doc: "Percent-encodes a URL path segment.",
        },
        StdItem {
            name: "path_unescape",
            kind: StdItemKind::Function,
            doc: "Inverse of `path_escape`.",
        },
    ],
};

pub const NET: StdModule = StdModule {
    path: "std::net",
    summary: "TCP/UDP networking primitives.",
    items: &[
        StdItem {
            name: "UnixListener",
            kind: StdItemKind::Type,
            doc: "Unix-domain socket listener.",
        },
        StdItem {
            name: "UnixStream",
            kind: StdItemKind::Type,
            doc: "Connected Unix-domain byte stream.",
        },
        StdItem {
            name: "TcpListener",
            kind: StdItemKind::Type,
            doc: "Accepts incoming TCP connections.",
        },
        StdItem {
            name: "TcpStream",
            kind: StdItemKind::Type,
            doc: "Bidirectional TCP byte stream; supports read/write, TLS upgrade, close, and read/write timeout setters.",
        },
        StdItem {
            name: "UdpSocket",
            kind: StdItemKind::Type,
            doc: "Bound UDP socket for datagram I/O.",
        },
        StdItem {
            name: "lookup",
            kind: StdItemKind::Function,
            doc: "Resolves a hostname to its IP addresses.",
        },
        StdItem {
            name: "lookup_mx",
            kind: StdItemKind::Function,
            doc: "Mail exchangers for a domain, by preference. `server:` names a nameserver (`host` or `host:port`) in place of the system's.",
        },
        StdItem {
            name: "lookup_txt",
            kind: StdItemKind::Function,
            doc: "Text records for a name, each record's strings joined.",
        },
        StdItem {
            name: "lookup_srv",
            kind: StdItemKind::Function,
            doc: "Service records for a name such as `_sip._tcp.example.com`, by priority.",
        },
        StdItem {
            name: "lookup_cname",
            kind: StdItemKind::Function,
            doc: "The canonical name an alias stands for.",
        },
        StdItem {
            name: "lookup_addr",
            kind: StdItemKind::Function,
            doc: "The names an IP address maps back to (PTR records).",
        },
        StdItem {
            name: "Mx",
            kind: StdItemKind::Type,
            doc: "A mail exchanger: `host` and `preference` (lower first).",
        },
        StdItem {
            name: "Srv",
            kind: StdItemKind::Type,
            doc: "A service location: `target`, `port`, `priority`, and `weight`.",
        },
    ],
};

pub const NET_IP: StdModule = StdModule {
    path: "std::net::ip",
    summary: "String-level IPv4 / IPv6 parsing and classification helpers.",
    items: &[
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Parses an IP string, returning its canonical form or None.",
        },
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Reports whether the string is a valid v4 or v6 IP.",
        },
        StdItem {
            name: "is_v4",
            kind: StdItemKind::Function,
            doc: "Reports whether the string is a valid v4 IP.",
        },
        StdItem {
            name: "is_v6",
            kind: StdItemKind::Function,
            doc: "Reports whether the string is a valid v6 IP.",
        },
        StdItem {
            name: "to_string",
            kind: StdItemKind::Function,
            doc: "Canonical lowercase string form of the IP.",
        },
        StdItem {
            name: "is_loopback",
            kind: StdItemKind::Function,
            doc: "Reports whether the IP is a loopback address.",
        },
        StdItem {
            name: "is_private",
            kind: StdItemKind::Function,
            doc: "Reports whether the IP is in a private range.",
        },
        StdItem {
            name: "is_multicast",
            kind: StdItemKind::Function,
            doc: "Reports whether the IP is a multicast address.",
        },
        StdItem {
            name: "is_unspecified",
            kind: StdItemKind::Function,
            doc: "Reports whether the IP is the unspecified address.",
        },
        StdItem {
            name: "octets",
            kind: StdItemKind::Function,
            doc: "Byte octets of the IP as a Vec.",
        },
    ],
};

pub const NETIP: StdModule = StdModule {
    path: "std::net::netip",
    summary: "Typed IP-address parsing, classification, and addr:port helpers (Go's net/netip shape).",
    items: &[
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a v4 or v6 IP.",
        },
        StdItem {
            name: "is_v4",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a v4 IP.",
        },
        StdItem {
            name: "is_v6",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a v6 IP.",
        },
        StdItem {
            name: "is_loopback",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a loopback IP (127.0.0.1 / ::1).",
        },
        StdItem {
            name: "is_unspecified",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as the unspecified IP (0.0.0.0 / ::).",
        },
        StdItem {
            name: "is_multicast",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a multicast IP.",
        },
        StdItem {
            name: "is_private",
            kind: StdItemKind::Function,
            doc: "Return true iff the IP is RFC1918 (v4) or ULA fc00::/7 (v6).",
        },
        StdItem {
            name: "normalize",
            kind: StdItemKind::Function,
            doc: "Canonical lowercase form of the IP, or empty string on parse failure.",
        },
        StdItem {
            name: "host_of",
            kind: StdItemKind::Function,
            doc: "Host portion of an addr:port string, or empty on parse failure.",
        },
        StdItem {
            name: "port_of",
            kind: StdItemKind::Function,
            doc: "Port portion of an addr:port string, or -1 on parse failure.",
        },
        StdItem {
            name: "join_addr_port",
            kind: StdItemKind::Function,
            doc: "Compose an addr:port string from host and port, or empty on failure.",
        },
        StdItem {
            name: "Prefix",
            kind: StdItemKind::Type,
            doc: "An IP prefix after Go's `netip.Prefix`: `Prefix::parse(\"10.0.0.1/8\")?` keeps the address as written (`addr()`, `bits()`); `masked()` answers the canonical prefix; `first()` and `last()` its first and last addresses for both families; `contains(addr)` and `overlaps(other)`, where an IPv4 prefix never contains an IPv6 address, the mapped `::ffff:10.0.0.1` included; `addresses()` iterates lazily, so an IPv6 /64 works. A length outside 0..=32 (IPv4) or 0..=128 (IPv6) is an `Err`.",
        },
    ],
};

pub const MIME: StdModule = StdModule {
    path: "std::mime",
    summary: "RFC 2045 media type parsing, parameter extraction, and extension lookup.",
    items: &[
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Canonical `type/subtype` form of the input, or empty on parse failure.",
        },
        StdItem {
            name: "top",
            kind: StdItemKind::Function,
            doc: "Top-level type (e.g. `text`) of a media type, or empty.",
        },
        StdItem {
            name: "sub",
            kind: StdItemKind::Function,
            doc: "Subtype (e.g. `html`) of a media type, or empty.",
        },
        StdItem {
            name: "charset",
            kind: StdItemKind::Function,
            doc: "Return the `charset` parameter, or empty.",
        },
        StdItem {
            name: "boundary",
            kind: StdItemKind::Function,
            doc: "Return the multipart `boundary` parameter, or empty.",
        },
        StdItem {
            name: "param",
            kind: StdItemKind::Function,
            doc: "Return an arbitrary parameter by key, or empty.",
        },
        StdItem {
            name: "type_by_extension",
            kind: StdItemKind::Function,
            doc: "Canonical media type for a filename extension (dot optional), or empty.",
        },
        StdItem {
            name: "extension_by_type",
            kind: StdItemKind::Function,
            doc: "Canonical extension (no leading dot) for a media type, or empty.",
        },
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a valid media type.",
        },
    ],
};
