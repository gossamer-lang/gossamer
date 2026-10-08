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
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Wires up Gossamer-callable builtins for stdlib modules whose
//! Rust-side implementation already exists but had no user-facing
//! exposure. Each `install_*` helper is invoked from
//! `builtins::install` so user code that writes
//! `strings::join`, `strconv::parse_i64`, `net::TcpStream::connect`,
//! `time::Instant::now`, etc. resolves to a real callable.
//!
//! All builtins return a `Result`-shaped variant (`Ok` / `Err`) on
//! fallible operations so callers can chain `?` without wrapping.

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::io::Read as IoRead;
use std::sync::Arc;

use gossamer_ast::Ident;

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_std::net as net_std;
use gossamer_std::os as os_std;
use gossamer_std::path as path_std;
use gossamer_std::strconv as strconv_std;
use gossamer_std::strings as strings_std;
use gossamer_std::unicode as unicode_std;
use gossamer_std::utf8 as utf8_std;

use gossamer_std::iter as iter_std;
use gossamer_std::utf16 as utf16_std;

use crate::builtins::{
    BuiltinFnPub, as_str, err_variant, install_module_pub, none_variant, ok_variant, some_variant,
    value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

pub(crate) fn install(globals: &mut Vec<(&'static str, Value)>) {
    install_strings(globals);
    install_strconv(globals);
    install_path(globals);
    install_utf8(globals);
    install_os_extras(globals);
    install_fs_extras(globals);
    install_bufio_extras(globals);
    install_time_extras(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_net(globals);
    install_set(globals);
    dyn_value::install(globals);
    install_sort(globals);
    install_io_streams(globals);
    install_sync_extras(globals);
    install_math(globals);
    install_math_bits(globals);
    install_math_rand(globals);
    install_bytes_builder(globals);
    install_unicode(globals);
    install_encoding_binary(globals);
    install_encoding_csv(globals);
    install_encoding_pem(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_database_sql(globals);
    install_utf16(globals);
    install_iter(globals);
    install_option(globals);
    install_result(globals);
    // Pure hashes (sha256/sha512/blake3/hmac/subtle) register on every target;
    // the OS-RNG `crypto::rand` registration inside is itself native-gated.
    install_crypto(globals);
    install_encoding_yaml(globals);
    install_compress(globals);
    install_hash_fnv(globals);
    install_archive_zip(globals);
    install_archive_tar(globals);
    install_sync_atomic_u64(globals);
    install_sync_barrier(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_crypto_breadth(globals);
    install_hash_crc32_adler32(globals);
    install_json_builtins(globals);
    install_time_completeness(globals);
    install_net_ip(globals);
    install_thread(globals);
    install_html(globals);
    install_encoding_base64_hex(globals);
    install_encoding_base32(globals);
    install_encoding_ascii85(globals);
    install_encoding_xml(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_crypto_insecure(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_compress_bzip2(globals);
    install_math_big(globals);
    install_http_chunked(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_sse(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_native_client(globals);
    install_http_static_files(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_proxy(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_websocket(globals);
    // Route registration + pattern lookup are pure (no sockets), so the
    // wasm playground gets the router surface; only serving stays gated.
    install_http_router(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_middleware(globals);
    install_http_request_values(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_middleware_bearer(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_security(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_ws_accept(globals);
    install_http_ws(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_http_static_server(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_uuid(globals);
    install_os_user(globals);
    install_netip(globals);
    install_mime(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_image(globals);
    install_encoding_toml(globals);
    install_container_heap(globals);
    install_deque(globals);
    install_url_escape(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_jwt(globals);
    install_validate(globals);
    install_rwlock(globals);
    install_shared(globals);
    install_context(globals);
    install_cohort(globals);
    lifecycle::install_lifecycle(globals);
    #[cfg(not(target_arch = "wasm32"))]
    http_server::install_http_server(globals);
    #[cfg(not(target_arch = "wasm32"))]
    http_response_stream::install_http_response_stream(globals);
    #[cfg(not(target_arch = "wasm32"))]
    net::install_smtp(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_metrics(globals);
    #[cfg(not(target_arch = "wasm32"))]
    install_trace(globals);
}

// ----------------------------------------------------------------------
// Helpers

pub(crate) fn arg_str_at(
    args: &[Value],
    idx: usize,
    fn_name: &str,
    label: &str,
) -> Result<String, Value> {
    match args.get(idx) {
        Some(Value::String(s)) => Ok(s.as_str().to_string()),
        _ => Err(err_variant(format!("{fn_name}: expected string {label}"))),
    }
}

pub(crate) fn string_array(values: Vec<String>) -> Value {
    Value::Array(Arc::new(
        values
            .into_iter()
            .map(|s| Value::String(s.into()))
            .collect(),
    ))
}

// ----------------------------------------------------------------------
// strings

pub mod archive_tar;
pub mod archive_zip;
pub mod bufio;
pub mod bytes_builder;
pub mod cohort;
pub mod compress;
pub mod container_heap;
pub mod context;
pub mod crypto;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_breadth;
#[cfg(not(target_arch = "wasm32"))]
pub mod crypto_insecure;
#[cfg(not(target_arch = "wasm32"))]
pub mod database_sql;
#[cfg(not(target_arch = "wasm32"))]
pub mod database_sql_native;
pub(crate) mod deadline_queue;
pub mod deque;
pub mod dyn_value;
pub mod encoding_binary;
pub mod encoding_csv;
pub mod encoding_pem;
pub mod encoding_toml;
pub mod encoding_xml;
pub mod encoding_yaml;
pub mod fs;
pub mod hash_fnv;
pub mod html;
pub mod http_chunked;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_middleware;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_middleware_bearer;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_native_client;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_proxy;
pub mod http_request_values;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod http_response_stream;
pub mod http_router;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_security;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod http_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_sse;
pub mod http_static_files;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_static_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_websocket;
pub mod http_ws;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_ws_accept;
#[cfg(not(target_arch = "wasm32"))]
pub mod image;
pub mod io_streams;
pub mod iter;
pub mod json_builtins;
#[cfg(not(target_arch = "wasm32"))]
pub mod jwt;
pub(crate) mod lifecycle;
pub mod math;
pub mod math_big;
pub mod math_bits;
pub mod math_rand;
#[cfg(not(target_arch = "wasm32"))]
pub mod metrics;
pub mod mime;
#[cfg(not(target_arch = "wasm32"))]
pub mod net;
pub mod net_ip;
pub mod netip;
pub mod option;
pub mod os;
pub mod os_user;
pub mod path;
pub mod result;
pub mod rwlock;
pub mod set;
pub mod shared;
pub mod sort;
pub mod strconv;
pub mod strings;
pub mod sync;
pub mod sync_barrier;
pub mod thread;
pub mod time;
pub mod time_completeness;
#[cfg(not(target_arch = "wasm32"))]
pub mod trace;
pub mod unicode;
pub mod url_escape;
#[cfg(not(target_arch = "wasm32"))]
pub mod uuid;
pub mod validate;
pub(crate) use archive_tar::install_archive_tar;
pub use archive_tar::*;
pub(crate) use archive_zip::install_archive_zip;
pub use archive_zip::*;
pub(crate) use bufio::install_bufio_extras;
pub use bufio::*;
pub(crate) use bytes_builder::install_bytes_builder;
pub use bytes_builder::*;
pub(crate) use cohort::install_cohort;
pub(crate) use compress::install_compress;
pub use compress::*;
pub(crate) use container_heap::install_container_heap;
pub use container_heap::*;
pub(crate) use context::install_context;
pub use context::*;
pub(crate) use crypto::install_crypto;
pub use crypto::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crypto_breadth::install_crypto_breadth;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_breadth::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crypto_insecure::install_crypto_insecure;
#[cfg(not(target_arch = "wasm32"))]
pub use crypto_insecure::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use database_sql::install_database_sql;
pub(crate) use deque::install_deque;
pub use deque::*;
pub(crate) use encoding_binary::install_encoding_binary;
pub use encoding_binary::*;
pub(crate) use encoding_csv::install_encoding_csv;
pub use encoding_csv::*;
pub(crate) use encoding_pem::install_encoding_pem;
pub use encoding_pem::*;
pub(crate) use encoding_toml::install_encoding_toml;
pub use encoding_toml::*;
pub(crate) use encoding_xml::install_encoding_xml;
pub use encoding_xml::*;
pub(crate) use encoding_yaml::install_encoding_yaml;
pub use encoding_yaml::*;
pub(crate) use fs::install_fs_extras;
pub use fs::*;
pub(crate) use hash_fnv::install_hash_fnv;
pub use hash_fnv::*;
pub(crate) use html::install_html;
pub use html::*;
pub(crate) use http_chunked::install_http_chunked;
pub use http_chunked::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_middleware::install_http_middleware;
#[cfg(not(target_arch = "wasm32"))]
pub use http_middleware::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_middleware_bearer::install_http_middleware_bearer;
#[cfg(not(target_arch = "wasm32"))]
pub use http_middleware_bearer::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_native_client::install_http_native_client;
#[cfg(not(target_arch = "wasm32"))]
pub use http_native_client::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_proxy::install_http_proxy;
#[cfg(not(target_arch = "wasm32"))]
pub use http_proxy::*;
pub(crate) use http_request_values::install_http_request_values;
pub use http_request_values::*;
pub(crate) use http_router::install_http_router;
pub use http_router::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_security::install_http_security;
#[cfg(not(target_arch = "wasm32"))]
pub use http_security::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_sse::install_http_sse;
#[cfg(not(target_arch = "wasm32"))]
pub use http_sse::*;
pub(crate) use http_static_files::install_http_static_files;
pub use http_static_files::*;
#[cfg(not(target_arch = "wasm32"))]
pub use http_static_server::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_static_server::{install_http_static_server, native_file_server_serve};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_websocket::install_http_websocket;
#[cfg(not(target_arch = "wasm32"))]
pub use http_websocket::*;
pub(crate) use http_ws::install_http_ws;
pub use http_ws::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use http_ws_accept::install_http_ws_accept;
#[cfg(not(target_arch = "wasm32"))]
pub use http_ws_accept::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use image::install_image;
#[cfg(not(target_arch = "wasm32"))]
pub use image::*;
pub(crate) use io_streams::install_io_streams;
pub(crate) use iter::install_iter;
pub use iter::*;
pub(crate) use json_builtins::install_json_builtins;
pub use json_builtins::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use jwt::install_jwt;
#[cfg(not(target_arch = "wasm32"))]
pub use jwt::*;
pub(crate) use math::install_math;
pub use math::*;
pub(crate) use math_big::install_math_big;
pub use math_big::*;
pub(crate) use math_bits::install_math_bits;
pub use math_bits::*;
pub(crate) use math_rand::install_math_rand;
pub use math_rand::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use metrics::install_metrics;
#[cfg(not(target_arch = "wasm32"))]
pub use metrics::*;
pub(crate) use mime::install_mime;
pub use mime::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use net::install_net;
#[cfg(not(target_arch = "wasm32"))]
pub use net::*;
pub(crate) use net_ip::install_net_ip;
pub use net_ip::*;
pub(crate) use netip::install_netip;
pub use netip::*;
pub(crate) use option::install_option;
pub use option::*;
pub(crate) use os::install_os_extras;
pub use os::*;
pub(crate) use os_user::install_os_user;
pub use os_user::*;
pub(crate) use path::install_path;
pub use path::*;
pub(crate) use result::install_result;
pub use result::*;
pub(crate) use rwlock::install_rwlock;
pub use rwlock::*;
pub(crate) use set::install_set;
pub use set::*;
pub(crate) use shared::install_shared;
pub(crate) use sort::install_sort;
pub(crate) use strconv::install_strconv;
pub use strconv::*;
pub(crate) use strings::install_strings;
pub use strings::*;
pub(crate) use sync::install_sync_extras;
pub use sync::*;
pub(crate) use sync_barrier::install_sync_barrier;
pub use sync_barrier::*;
pub(crate) use thread::install_thread;
pub use thread::*;
pub(crate) use time::install_time_extras;
pub use time::*;
pub(crate) use time_completeness::install_time_completeness;
pub use time_completeness::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use trace::install_trace;
#[cfg(not(target_arch = "wasm32"))]
pub use trace::*;
pub(crate) use unicode::install_unicode;
pub use unicode::*;
pub(crate) use url_escape::install_url_escape;
pub use url_escape::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use uuid::install_uuid;
#[cfg(not(target_arch = "wasm32"))]
pub use uuid::*;
pub(crate) use validate::install_validate;
pub use validate::*;
