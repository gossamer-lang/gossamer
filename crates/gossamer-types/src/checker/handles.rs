//! Opaque stdlib handles: which constructors produce one, and which methods each answers.

use super::{
    FILE_SERVER_OFFSET, LEGACY_HANDLE_CTORS, PURE_HANDLE_HI_OFFSET, PURE_HANDLE_LO_OFFSET,
    PURE_HANDLES, SYNC_HANDLE_LO_OFFSET, TRACE_ENDED_SPAN_OFFSET, TRACE_SPAN_OFFSET, Ty,
    U8_VEC_OFFSET,
};

/// `(sentinel offset, name)` of the runtime handle the stdlib constructor
/// `module::last` produces, if it produces one.
pub(super) fn stdlib_handle_ctor(module: &[&str], last: &str) -> Option<(u32, &'static str)> {
    let module = module.strip_prefix(&["std"]).unwrap_or(module);
    PURE_HANDLES
        .iter()
        .find(|(_, _, ctors)| {
            ctors
                .iter()
                .any(|(path, name)| *name == last && *path == module)
        })
        .map(|(offset, name, _)| (*offset, *name))
        .or_else(|| {
            LEGACY_HANDLE_CTORS
                .iter()
                .find(|(path, name, _, _)| *name == last && *path == module)
                .map(|(_, _, offset, name)| (*offset, *name))
        })
}

/// `(sentinel offset, name)` of the runtime handle a written type
/// annotation names. A parameter, a struct field, and a return type carry
/// no constructor to infer from, so the annotation itself has to land on
/// the same sentinel `Adt` the constructor answers - otherwise the slot
/// stays an inference variable and method dispatch falls back to the
/// name, reaching whatever runtime symbol shares it.
pub(super) fn stdlib_handle_by_path(segments: &[&str]) -> Option<(u32, &'static str)> {
    let last = *segments.last()?;
    let written = segments.strip_prefix(&["std"]).unwrap_or(segments);
    PURE_HANDLES.iter().find_map(|(offset, name, ctors)| {
        let (module, tail) = name.split_once("::")?;
        if tail != last {
            return None;
        }
        // The display name is not always the path the type is written
        // under - `http::Router` lives in `http::router` - so the module
        // its own constructors are written under counts too. Without that
        // a `router::Router` parameter stays an inference variable and its
        // dispatch falls back to the bare method name.
        let written_as_ctor = ctors.iter().any(|(path, _)| *path == written);
        (written_as_ctor
            || written.len() == 1
            || written[written.len() - 2] == module
            || module.split("::").last() == Some(written[written.len() - 2]))
        .then_some((*offset, *name))
    })
}

/// Sentinel offsets of the stdlib types that are runtime-owned handles:
/// a pointer the runtime hands back, with no text form. The pure-handle
/// band is covered by range; these are the older sentinels that predate
/// it, including the field-bearing blobs (`http::ResponseStream`,
/// `http::Response`) whose fields are read through accessors rather than
/// rendered.
pub(super) const OPAQUE_HANDLE_OFFSETS: &[u32] = &[
    4,
    5,
    9,
    10,
    11,
    12,
    13,
    14,
    15,
    16,
    17,
    U8_VEC_OFFSET,
    21,
    22,
    23,
    24,
    25,
    26,
    27,
    FILE_SERVER_OFFSET,
    super::WEBSOCKET_CONN_OFFSET,
    super::BYTES_BUFFER_OFFSET,
];

/// A parameter or return shape in [`HANDLE_METHODS`].
#[derive(Clone, Copy)]
pub(super) enum Shape {
    Unit,
    Bool,
    I64,
    U64,
    U32,
    F64,
    Str,
    StrVec,
    F64Vec,
    OptStr,
    /// `&mut String`, a buffer the method appends to.
    MutStr,
    /// `Result<i64, errors::Error>`.
    ResultI64,
    DoneChannel,
    /// A handle, by sentinel offset and display name.
    Handle(u32, &'static str),
    /// Any of the three `metrics` instruments a registry collects.
    Metric,
}

/// The typed surface of the table-described runtime handles: `(owner,
/// method, parameters, return)`. `new` rows are the constructors, written on
/// the owner's path; every other row is a method on the handle. A name an
/// owner does not list is reported where it is written.
pub(super) const HANDLE_METHODS: &[(&str, &str, &[Shape], Shape)] = &[
    (
        "context::Context",
        "background",
        &[],
        Shape::Handle(11, "context::Context"),
    ),
    (
        "context::Context",
        "with_cancel",
        &[Shape::Handle(11, "context::Context")],
        Shape::Handle(11, "context::Context"),
    ),
    (
        "context::Context",
        "with_timeout",
        &[Shape::Handle(11, "context::Context"), Shape::I64],
        Shape::Handle(11, "context::Context"),
    ),
    ("context::Context", "cancel", &[], Shape::Unit),
    ("context::Context", "done", &[], Shape::Bool),
    ("context::Context", "is_cancelled", &[], Shape::Bool),
    ("context::Context", "done_chan", &[], Shape::DoneChannel),
    (
        "metrics::Counter",
        "new",
        &[Shape::Str, Shape::Str],
        Shape::Handle(36, "metrics::Counter"),
    ),
    ("metrics::Counter", "inc", &[], Shape::Unit),
    ("metrics::Counter", "value", &[], Shape::I64),
    (
        "metrics::Gauge",
        "new",
        &[Shape::Str, Shape::Str],
        Shape::Handle(37, "metrics::Gauge"),
    ),
    ("metrics::Gauge", "set", &[Shape::F64], Shape::Unit),
    ("metrics::Gauge", "inc", &[], Shape::Unit),
    ("metrics::Gauge", "dec", &[], Shape::Unit),
    ("metrics::Gauge", "value", &[], Shape::F64),
    (
        "metrics::Histogram",
        "new",
        &[Shape::Str, Shape::Str, Shape::F64Vec],
        Shape::Handle(38, "metrics::Histogram"),
    ),
    ("metrics::Histogram", "observe", &[Shape::F64], Shape::Unit),
    ("metrics::Histogram", "count", &[], Shape::I64),
    ("metrics::Histogram", "sum", &[], Shape::F64),
    (
        "metrics::Registry",
        "new",
        &[],
        Shape::Handle(39, "metrics::Registry"),
    ),
    (
        "metrics::Registry",
        "register",
        &[Shape::Metric],
        Shape::Unit,
    ),
    ("metrics::Registry", "render", &[], Shape::Str),
    (
        "trace::Tracer",
        "new",
        &[],
        Shape::Handle(40, "trace::Tracer"),
    ),
    (
        "trace::Tracer",
        "start_span",
        &[Shape::Str],
        Shape::Handle(TRACE_SPAN_OFFSET, "trace::Span"),
    ),
    (
        "trace::Span",
        "set_attribute",
        &[Shape::Str, Shape::Str],
        Shape::Unit,
    ),
    (
        "trace::Span",
        "set_status",
        &[Shape::I64, Shape::Str],
        Shape::Unit,
    ),
    (
        "trace::Span",
        "end",
        &[],
        Shape::Handle(TRACE_ENDED_SPAN_OFFSET, "trace::EndedSpan"),
    ),
    ("trace::EndedSpan", "to_otlp_json", &[], Shape::Str),
    (
        "rand::Rng",
        "new",
        &[Shape::I64],
        Shape::Handle(42, "rand::Rng"),
    ),
    ("rand::Rng", "next_u64", &[], Shape::U64),
    ("rand::Rng", "next_u32", &[], Shape::U32),
    (
        "rand::Rng",
        "range_u64",
        &[Shape::U64, Shape::U64],
        Shape::U64,
    ),
    ("rand::Rng", "next_f64", &[], Shape::F64),
    (
        "bufio::Scanner",
        "new",
        &[Shape::Handle(25, "io::Stream")],
        Shape::Handle(43, "bufio::Scanner"),
    ),
    ("bufio::Scanner", "scan", &[], Shape::Bool),
    ("bufio::Scanner", "next", &[], Shape::OptStr),
    ("bufio::Scanner", "text", &[], Shape::Str),
    ("sync::Map", "new", &[], Shape::Handle(34, "sync::Map")),
    (
        "sync::Map",
        "insert",
        &[Shape::Str, Shape::Str],
        Shape::Unit,
    ),
    ("sync::Map", "get", &[Shape::Str], Shape::OptStr),
    ("sync::Map", "remove", &[Shape::Str], Shape::Unit),
    ("sync::Map", "len", &[], Shape::I64),
    ("sync::Map", "contains_key", &[Shape::Str], Shape::Bool),
    ("sync::Map", "keys", &[], Shape::StrVec),
    ("io::Stream", "write_byte", &[Shape::I64], Shape::Unit),
    ("io::Stream", "write", &[Shape::Str], Shape::Unit),
    ("io::Stream", "write_str", &[Shape::Str], Shape::Unit),
    ("io::Stream", "flush", &[], Shape::Unit),
    ("io::Stream", "read_line", &[], Shape::OptStr),
    (
        "io::Stream",
        "read_line",
        &[Shape::MutStr],
        Shape::ResultI64,
    ),
    ("io::Stream", "read_to_string", &[], Shape::Str),
];

/// The owner a type-qualified path names in [`HANDLE_METHODS`]: the display
/// name itself, or one reached with a `std::` / `math::` prefix or through
/// its bare type name.
pub(super) fn table_handle_owner(module: &[&str]) -> Option<&'static str> {
    let module = module.strip_prefix(&["std"]).unwrap_or(module);
    let module = module.strip_prefix(&["math"]).unwrap_or(module);
    HANDLE_METHODS
        .iter()
        .map(|(owner, ..)| *owner)
        .find(|owner| {
            let mut parts = owner.split("::");
            let (Some(head), Some(tail)) = (parts.next(), parts.next()) else {
                return false;
            };
            // A bare `Map` is the collections map, so the concurrent one is
            // reached only through `sync::Map`.
            matches!(module, [m, t] if *m == head && *t == tail)
                || (*owner != "sync::Map" && matches!(module, [t] if *t == tail))
        })
}

/// `(parameters, return)` of an atomic's method over its word type. A
/// `bool` word has no arithmetic, so only the integer atomics add and
/// subtract.
pub(super) fn atomic_method(
    method: &str,
    word: Ty,
    bool_ty: Ty,
    unit: Ty,
) -> Option<(Vec<Ty>, Ty)> {
    match method {
        "load" => Some((Vec::new(), word)),
        "store" => Some((vec![word], unit)),
        "compare_exchange" => Some((vec![word, word], bool_ty)),
        "fetch_add" | "fetch_sub" if word != bool_ty => Some((vec![word], word)),
        _ => None,
    }
}

/// True when `def` names a runtime handle rather than a value with a
/// representation of its own.
pub(super) fn is_opaque_handle_def(local: u32) -> bool {
    let offset = u32::MAX - local;
    (PURE_HANDLE_LO_OFFSET..=PURE_HANDLE_HI_OFFSET).contains(&offset)
        || (SYNC_HANDLE_LO_OFFSET..=TRACE_ENDED_SPAN_OFFSET).contains(&offset)
        || OPAQUE_HANDLE_OFFSETS.contains(&offset)
}

/// Sentinel-def offset for a stdlib handle named by its last path segment,
/// the same type whether it is written in source or read out of a signature
/// row.
pub(super) fn stdlib_handle_def_offset(tail: &str) -> Option<u32> {
    Some(match tail {
        "Pattern" => 26,
        "Policy" => 13,
        "ResponseStream" => 4,
        "Response" => 5,
        _ => {
            return stdlib_net_handle(tail)
                .map(|(offset, _)| offset)
                .or_else(|| stdlib_fs_handle(tail).map(|(offset, _)| offset));
        }
    })
}

/// `(sentinel offset, canonical name)` for the streaming filesystem
/// handles. One `DefId` per handle carries one registered spelling, so a
/// written `let f: fs::File` and a signature slot naming the same type
/// land on the same `Adt`.
pub(super) fn stdlib_fs_handle(tail: &str) -> Option<(u32, &'static str)> {
    Some(match tail {
        "File" => (44, "fs::File"),
        "OpenOptions" => (45, "fs::OpenOptions"),
        _ => return None,
    })
}

/// `(sentinel offset, name)` of the socket a `std::net` constructor
/// answers. The path may name the type alone (`TcpStream::connect`) or
/// carry its module (`net::TcpStream::connect`), so only the type segment
/// the method hangs off is matched.
/// Whether `module::last` is one of the constructors that answers an
/// `fs::File` through a `Result`: the two `File` associated functions and
/// the terminal `OpenOptions::open`.
pub(super) fn fs_file_ctor(module: &[&str], last: &str) -> bool {
    let module = module.strip_prefix(&["std"]).unwrap_or(module);
    matches!(
        (module, last),
        (["fs", "File"] | ["File"], "open" | "create")
            | (["fs", "OpenOptions"] | ["OpenOptions"], "open")
    )
}

pub(super) fn net_socket_ctor(module: &[&str], last: &str) -> Option<(u32, &'static str)> {
    let type_name = *module.last()?;
    let expected = match (type_name, last) {
        ("TcpStream" | "UnixStream", "connect")
        | ("TcpListener" | "UnixListener" | "UdpSocket", "bind") => type_name,
        _ => return None,
    };
    stdlib_net_handle(expected)
}

/// Sentinel-`Adt` offset and canonical name for the opaque `std::net`
/// socket handles. The written annotation (`let s: net::TcpStream`) and a
/// signature slot naming the same type must land on one `DefId` under one
/// registered name, so both sides read this table.
pub(super) fn stdlib_net_handle(tail: &str) -> Option<(u32, &'static str)> {
    Some(match tail {
        "TcpStream" => (12, "net::TcpStream"),
        "TcpListener" => (13, "net::TcpListener"),
        "UdpSocket" => (14, "net::UdpSocket"),
        "UnixStream" => (15, "net::UnixStream"),
        "UnixListener" => (16, "net::UnixListener"),
        _ => return None,
    })
}
