#![forbid(unsafe_code)]
// Every builtin answers `RuntimeResult<Value>`, the one signature the
// dispatch table stores, including those that cannot fail.
#![allow(clippy::unnecessary_wraps)]

pub mod coverage;

include!("builtins/setup.rs");
include!("builtins/install.rs");
include!("builtins/runtime_state.rs");
include!("builtins/core_io_http.rs");
include!("builtins/os_exec_signal.rs");
include!("builtins/par.rs");
include!("builtins/json.rs");
include!("builtins/typed_serde.rs");
include!("builtins/strings_collections_a.rs");
include!("builtins/strings_collections_b.rs");
include!("builtins/strings_collections_c.rs");
