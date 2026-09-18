#![forbid(unsafe_code)]
#![allow(dead_code, unused_imports, clippy::unnecessary_wraps)]

pub mod coverage;

include!("builtins/setup.rs");
include!("builtins/install.rs");
include!("builtins/runtime_state.rs");
include!("builtins/core_io_http.rs");
include!("builtins/os_exec_signal.rs");
include!("builtins/json.rs");
include!("builtins/strings_collections_a.rs");
include!("builtins/strings_collections_b.rs");
include!("builtins/strings_collections_c.rs");
