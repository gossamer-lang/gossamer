//! How the bytecode VM reaches the behaviour of each compiled-tier runtime
//! helper.
//!
//! A `gos_rt_*` helper whose VM counterpart is a registered builtin of the
//! same name (`gos_rt_fs_read_to_string` and `fs::read_to_string`) needs no
//! entry here: [`vm_counterpart`] finds it. Every other helper is named in
//! [`VM_NATIVE_EXEMPT`], exactly or through its family, with the mechanism
//! the VM uses instead. A helper with neither is a compiled-only feature.

/// Runtime helpers the VM reaches through something other than a builtin
/// of the same name, and how.
///
/// An entry ending in `*` names every helper sharing that prefix; the
/// longest matching entry is the one that answers for a helper.
pub const VM_NATIVE_EXEMPT: &[(&str, &str)] = &[
    (
        "gos_rt_aggr_*",
        "the VM holds structs and tuples as reference-counted `Value::Struct` / \
         `Value::Tuple` payloads, so allocating, freeing, and sharing their \
         children is Rust ownership on those values",
    ),
    (
        "gos_rt_arena_*",
        "the VM evaluates an `arena { }` block over ordinary counted values, \
         which are released when the block's bindings leave scope",
    ),
    (
        "gos_rt_arr_*",
        "the VM keeps fixed arrays as `Value::Array` / `Value::IntArray` / \
         `Value::FloatArray` and implements their length, iteration, sorting, \
         and rendering as the array methods in `strings_collections_*.rs`",
    ),
    (
        "gos_rt_atomic_*",
        "the VM implements `sync::AtomicI64` / `AtomicBool` as builtin method \
         calls over a shared `std::sync::atomic` cell (`stdlib_builtins/sync.rs`)",
    ),
    (
        "gos_rt_bheap_*",
        "the VM implements `MinHeap` / `MaxHeap` as builtin methods over its own \
         heap value (`stdlib_builtins/container_heap.rs`); these shims are the \
         compiled tiers' per-element-layout specialisations of the same methods",
    ),
    (
        "gos_rt_bin_*",
        "the VM implements `encoding::binary` in `stdlib_builtins/encoding_binary.rs`, \
         reading and writing the same byte orders on its byte-vector value",
    ),
    (
        "gos_rt_binding_*",
        "the VM marshals `[rust-bindings]` arguments and results through \
         `external_natives.rs` on its own values; these shims are the compiled \
         tiers' conversions between the binding wire layout and native layout",
    ),
    (
        "gos_rt_bool_to_str",
        "the VM renders a bool through its `Display` implementation for `Value::Bool`",
    ),
    (
        "gos_rt_btree_*",
        "the VM implements `BTreeSet` as builtin methods over an ordered set value \
         (`stdlib_builtins/set.rs`)",
    ),
    (
        "gos_rt_bytearr_slice_result",
        "the VM slices a byte array with the range-index operation on `Value::ByteArray`",
    ),
    (
        "gos_rt_callback_*",
        "the VM calls a binding callback by invoking the closure value it holds \
         directly, with no registry handle in between",
    ),
    (
        "gos_rt_carrier_from_box",
        "the VM holds `Option` / `Result` as `Value::Variant`, so there is no \
         boxed-payload carrier to unpack",
    ),
    (
        "gos_rt_chan_*",
        "the VM implements channels as its `channel` builtins and their send and \
         receive methods, parking a blocked goroutine in its own scheduler",
    ),
    (
        "gos_rt_char_to_str",
        "the VM renders a char through its `Display` implementation for `Value::Char`",
    ),
    (
        "gos_rt_clamp_*",
        "the VM implements the prelude `clamp` as one builtin over `Value::Int` and \
         `Value::Float`; these shims are its per-type compiled forms",
    ),
    (
        "gos_rt_cohort_root",
        "the VM answers `runtime::root()` from its own root cohort \
         (`stdlib_builtins/cohort.rs`)",
    ),
    (
        "gos_rt_concat_*",
        "the VM builds formatted text with its `format` / `__concat` builtins, which \
         render each piece through `Display`; these shims are the compiled tiers' \
         incremental string builder for the same format strings",
    ),
    (
        "gos_rt_cov_*",
        "the VM records `gos test --coverage` hits from the source map its compiler \
         builds when coverage is active, where these shims hold the compiled \
         tiers' table",
    ),
    (
        "gos_rt_crypto_*",
        "the VM implements these digests, ciphers, and key-derivation functions \
         in `stdlib_builtins/crypto*.rs` over the same crates",
    ),
    (
        "gos_rt_ctx_*",
        "the VM implements `context` in `stdlib_builtins/context.rs` on the same \
         cancellation tree these shims reach",
    ),
    (
        "gos_rt_debug_*",
        "the VM renders `{:?}` for `Option` and `Result` through the `Debug` \
         rendering of `Value::Variant`",
    ),
    (
        "gos_rt_deque_*",
        "the VM implements `Deque` as builtin methods over its own deque value \
         (`stdlib_builtins/deque.rs`); these shims specialise them by element layout",
    ),
    (
        "gos_rt_desc_cmp",
        "the VM orders any two values structurally through `Value`'s own comparison",
    ),
    (
        "gos_rt_desc_eq",
        "the VM compares any two values for equality through `Value`'s own \
         equality, which already answers IEEE equality for float fields",
    ),
    (
        "gos_rt_dyn_*",
        "the VM implements `DynValue` in `stdlib_builtins/dyn_value.rs` as builtin \
         constructors and methods over its own tagged value",
    ),
    (
        "gos_rt_enum_*",
        "the VM holds enums as `Value::Variant`, so boxing, unit construction, and \
         structural equality are operations on that value",
    ),
    (
        "gos_rt_eprint_str",
        "the VM writes `eprint` output through its `eprint` / `eprintln` builtins",
    ),
    (
        "gos_rt_error_*",
        "the VM implements `errors::Error` construction, wrapping, and inspection \
         as the `errors::` builtins and `Error::` methods over its error value",
    ),
    (
        "gos_rt_errors_join_vec",
        "the VM implements `errors::join` as a builtin over a vector of error values",
    ),
    (
        "gos_rt_exec_*",
        "the VM runs processes through the `__gos_process_*_raw` builtins the \
         `process::run` wrappers fold, on the same process API",
    ),
    (
        "gos_rt_f64_*",
        "the VM renders a float through its `Display` implementation for \
         `Value::Float`, precision included",
    ),
    (
        "gos_rt_field_error_*",
        "the VM implements `validate::FieldError` in `stdlib_builtins/validate.rs`",
    ),
    (
        "gos_rt_file_server_*",
        "the VM serves static files through `stdlib_builtins/http_static_files.rs`",
    ),
    (
        "gos_rt_flag_*",
        "the VM implements `flag::Set` in `flag_set_builtins.rs`, and its flag cells \
         deref to the parsed value directly",
    ),
    (
        "gos_rt_floatarr_slice_result",
        "the VM slices a float array with the range-index operation on \
         `Value::FloatArray`",
    ),
    (
        "gos_rt_flush_stdout",
        "the VM writes stdout through its own line-flushed writer, flushed at exit",
    ),
    (
        "gos_rt_fmt_*",
        "the VM applies width, fill, and radix in its `__fmt_pad` / `__fmt_radix` \
         builtins",
    ),
    (
        "gos_rt_fs_*",
        "the VM implements `fs::OpenOptions` and the `Result` forms of the file \
         reads in `stdlib_builtins/fs.rs`",
    ),
    (
        "gos_rt_gc_*",
        "the VM's values are reference counted by Rust ownership, so there is no \
         allocator registry to maintain or reset",
    ),
    (
        "gos_rt_go_yield",
        "the VM yields at its own safepoints in the instruction loop",
    ),
    (
        "gos_rt_goroutine_panicked",
        "the VM reports a goroutine's panic through its cohort join, which reads \
         the goroutine's outcome directly",
    ),
    (
        "gos_rt_hash_*",
        "the VM implements the FNV hashes in `stdlib_builtins/hash_fnv.rs`",
    ),
    (
        "gos_rt_heap_*",
        "the VM holds `I64Vec` / `U8Vec` handles as its own vector values and \
         implements their methods directly",
    ),
    (
        "gos_rt_http_*",
        "the VM implements the HTTP client, request, response, and stream surface \
         in the `stdlib_builtins/http_*.rs` builtins over the same client and \
         server stack",
    ),
    (
        "gos_rt_http2_*",
        "the VM serves h2c through the same `http2_server` driver from its HTTP builtins",
    ),
    (
        "gos_rt_http3_*",
        "the VM serves HTTP/3 through the shared `gossamer-http3` engine from its \
         HTTP builtins",
    ),
    (
        "gos_rt_i64_*",
        "the VM renders an integer and walks its characters through `Value::Int`'s \
         `Display` implementation",
    ),
    (
        "gos_rt_int_wrapping_*",
        "the VM evaluates `+%` and `*%` as wrapping arithmetic in its instruction loop",
    ),
    (
        "gos_rt_intarr_slice_result",
        "the VM slices an integer array with the range-index operation on \
         `Value::IntArray`",
    ),
    (
        "gos_rt_io_*",
        "the VM implements `io::ReadAll` and stream writes as its `io::` and \
         `Stream::` builtins",
    ),
    (
        "gos_rt_iter_*",
        "the VM implements every sequence combinator once over its own values \
         (`stdlib_builtins/iter.rs`); these shims are the compiled tiers' \
         specialisations of each combinator by element class (word, f64, pointer)",
    ),
    (
        "gos_rt_json_*",
        "the VM implements JSON parsing, rendering, and typed encoding in \
         `stdlib_builtins/json_builtins.rs` over `Value::Json`; these shims are the \
         compiled tiers' encoder and `Option`-returning accessors",
    ),
    (
        "gos_rt_lazy_iter_*",
        "the VM implements lazy iterators as `Value::LazyIter` in \
         `stdlib_builtins/iter.rs`; these shims are the compiled tiers' cursor \
         specialised by element class",
    ),
    (
        "gos_rt_len_is_zero",
        "the VM answers `is_empty` through each collection value's length",
    ),
    (
        "gos_rt_main_*",
        "the VM maps `main`'s result to the process exit code in its own entry path",
    ),
    (
        "gos_rt_map_*",
        "the VM implements `Map` as builtin methods over its own map value \
         (`strings_collections_*.rs`); these shims are the compiled tiers' \
         specialisations of the same methods by key and value layout",
    ),
    (
        "gos_rt_math_*",
        "the VM implements these as `math::` builtins over `Value::Int` and \
         `Value::Float`, and `math::rand::Rng` in `stdlib_builtins/math_rand.rs`",
    ),
    (
        "gos_rt_max_*",
        "the VM implements the prelude `max` as one builtin over every ordered \
         value; these shims are its per-type compiled forms",
    ),
    (
        "gos_rt_metrics_serve",
        "the VM serves metrics through `stdlib_builtins/metrics.rs`",
    ),
    (
        "gos_rt_middleware_*",
        "the VM applies HTTP middleware as closures around the handler \
         (`stdlib_builtins/http_middleware.rs`)",
    ),
    (
        "gos_rt_min_*",
        "the VM implements the prelude `min` as one builtin over every ordered \
         value; these shims are its per-type compiled forms",
    ),
    (
        "gos_rt_mw_*",
        "the VM configures CORS, HSTS, caching, rate limits, and auth in \
         `stdlib_builtins/http_middleware*.rs` and `http_security.rs`",
    ),
    (
        "gos_rt_native_client_new",
        "the VM builds the native HTTP client in `stdlib_builtins/http_native_client.rs`",
    ),
    (
        "gos_rt_nc_*",
        "the VM issues native-client requests in `stdlib_builtins/http_native_client.rs`",
    ),
    (
        "gos_rt_nested_arr_to_vec",
        "the VM converts a nested array with `to_vec` over its own array values",
    ),
    (
        "gos_rt_net_resolve",
        "the VM resolves host names in `stdlib_builtins/net.rs`",
    ),
    (
        "gos_rt_now_ns",
        "the VM reads the monotonic clock in its `time::` builtins",
    ),
    (
        "gos_rt_option_*",
        "the VM holds `Option` as `Value::Variant`, so defaults, maps, and payload \
         ownership are operations on that value (`stdlib_builtins/option.rs`)",
    ),
    (
        "gos_rt_os_*",
        "the VM implements these in `stdlib_builtins/os.rs` and \
         `stdlib_builtins/os_user.rs` over the same OS calls",
    ),
    (
        "gos_rt_packed_bytearr_slice_result",
        "the VM slices a packed byte array with the range-index operation on \
         `Value::ByteArray`",
    ),
    (
        "gos_rt_panic_*",
        "the VM raises an out-of-bounds index as its own runtime error with the \
         same message",
    ),
    (
        "gos_rt_par_*",
        "the VM runs a parallel adapter through its `__gos_par_run` builtin, which \
         sizes the leaves from the VM's own worker count and needs no grain query \
         or submission counter from the compiled scheduler",
    ),
    (
        "gos_rt_parse_i64_result",
        "the VM parses integers with the `to_i64` / `strconv` builtins",
    ),
    (
        "gos_rt_path_*",
        "the VM implements `path::` in `stdlib_builtins/path.rs`",
    ),
    (
        "gos_rt_preempt_*",
        "the VM yields at its own instruction-loop safepoints, so it needs no \
         preemption check call",
    ),
    (
        "gos_rt_print_*",
        "the VM prints through its `print` / `println` builtins, which render each \
         value through `Display`",
    ),
    (
        "gos_rt_println_fn_*",
        "the VM passes `println` as a callback like any other builtin value",
    ),
    (
        "gos_rt_program_start",
        "the VM opens the root cohort and marks the program entered in its own \
         entry path (`open_root_cohort`)",
    ),
    (
        "gos_rt_proxy_*",
        "the VM forwards proxied requests in `stdlib_builtins/http_proxy.rs`",
    ),
    (
        "gos_rt_queue_*",
        "the VM implements `Queue` as builtin methods over its own queue value; \
         these shims are its compiled forms",
    ),
    (
        "gos_rt_race_access",
        "the VM runs goroutines under its own scheduler and has no instrumented \
         memory accesses to report",
    ),
    (
        "gos_rt_rc_*",
        "the VM's values are reference counted by `Arc`, and `downgrade` answers \
         a `Value::Weak` observing the same allocation",
    ),
    (
        "gos_rt_regex_*",
        "the VM compiles and matches patterns in `regex_builtins.rs`",
    ),
    (
        "gos_rt_result_*",
        "the VM holds `Result` as `Value::Variant`, so construction, unwrapping, \
         mapping, and payload ownership are operations on that value \
         (`stdlib_builtins/result.rs`)",
    ),
    (
        "gos_rt_router_*",
        "the VM registers routes in `stdlib_builtins/http_router.rs`",
    ),
    (
        "gos_rt_rwlock_*",
        "the VM implements `sync::RwLock` in `stdlib_builtins/rwlock.rs`",
    ),
    (
        "gos_rt_select_*",
        "the VM evaluates a `select` expression in its own scheduler, polling each \
         arm's channel in source order",
    ),
    (
        "gos_rt_set_args",
        "the VM receives the program arguments through `set_program_args`",
    ),
    (
        "gos_rt_set_*",
        "the VM implements `Set` as builtin methods over its own set value \
         (`stdlib_builtins/set.rs`); these shims specialise them by element layout",
    ),
    (
        "gos_rt_sleep_*",
        "the VM parks the goroutine for `time::sleep` in its own scheduler",
    ),
    (
        "gos_rt_sort_*",
        "the VM implements `sort::` in `stdlib_builtins/sort.rs`; these shims \
         specialise it by element layout",
    ),
    (
        "gos_rt_spawn_ex",
        "the VM starts a goroutine for `spawn` in its own scheduler and attaches it \
         to the enclosing cohort",
    ),
    (
        "gos_rt_sql_*",
        "the VM implements `database::sql` in `stdlib_builtins/database_sql*.rs` \
         over the same driver layer",
    ),
    (
        "gos_rt_stack_*",
        "the VM implements `Stack` as builtin methods over its own stack value; \
         these shims are its compiled forms",
    ),
    (
        "gos_rt_static_*",
        "the VM serves static files and answers MIME types in \
         `stdlib_builtins/http_static_*.rs` and `stdlib_builtins/mime.rs`",
    ),
    (
        "gos_rt_stdout_*",
        "the VM serialises stdout writes through its own writer lock",
    ),
    (
        "gos_rt_str_*",
        "the VM implements `String` as builtin methods over `Value::String` \
         (`stdlib_builtins/strings.rs`); these shims are the compiled tiers' forms \
         of the same methods",
    ),
    (
        "gos_rt_strconv_*",
        "the VM implements `strconv::` in `stdlib_builtins/strconv.rs`",
    ),
    (
        "gos_rt_stream_next_line",
        "the VM reads the next line through `Stream::read_line`",
    ),
    (
        "gos_rt_sync_*",
        "the VM implements `sync::Map` and the shared vector handles in \
         `stdlib_builtins/sync.rs` and `stdlib_builtins/shared.rs`",
    ),
    (
        "gos_rt_tcp_*",
        "the VM implements `net::TcpListener` / `TcpStream` in `stdlib_builtins/net.rs`",
    ),
    (
        "gos_rt_testing_check_eq_i64",
        "the VM records `testing::check_eq` through its testing builtins",
    ),
    (
        "gos_rt_trace_ended_to_otlp_json",
        "the VM exports finished spans in `stdlib_builtins/trace.rs`",
    ),
    (
        "gos_rt_tuple_*",
        "the VM holds tuples as `Value::Tuple` and compares and renders them \
         structurally",
    ),
    (
        "gos_rt_u64_to_str",
        "the VM renders an unsigned integer through its `Display` implementation",
    ),
    (
        "gos_rt_udp_*",
        "the VM implements `net::UdpSocket` in `stdlib_builtins/net.rs`",
    ),
    (
        "gos_rt_unix_*",
        "the VM implements `net::UnixListener` / `UnixStream` in \
         `stdlib_builtins/net.rs`",
    ),
    (
        "gos_rt_utf8_*",
        "the VM counts runes with its `rune_count` builtin (`stdlib_builtins/path.rs`)",
    ),
    (
        "gos_rt_vec_*",
        "the VM implements `Vec` as builtin methods over its own vector values \
         (`strings_collections_*.rs`); these shims are the compiled tiers' \
         specialisations of the same methods by element layout",
    ),
    (
        "gos_rt_wg_*",
        "the VM implements `sync::WaitGroup` in `stdlib_builtins/sync.rs`",
    ),
    (
        "gos_rt_ws_*",
        "the VM implements WebSockets in `stdlib_builtins/http_websocket.rs` and \
         `http_ws*.rs`",
    ),
];

/// The VM builtin that implements runtime helper `symbol` under the same
/// name, if there is one.
///
/// The names compare with `::` read as `_` and case folded, so
/// `gos_rt_fs_read_to_string` matches `fs::read_to_string` and
/// `gos_rt_string_trim` matches `String::trim`. A `__gos_*` leaf builtin
/// matches the helper with the same tail.
#[must_use]
pub fn vm_counterpart(symbol: &str) -> Option<&'static str> {
    static BY_FOLDED_NAME: std::sync::OnceLock<std::collections::HashMap<String, &'static str>> =
        std::sync::OnceLock::new();
    let tail = symbol.strip_prefix("gos_rt_")?;
    let by_folded = BY_FOLDED_NAME.get_or_init(|| {
        super::registered_names()
            .into_iter()
            .map(|name| {
                let folded = name.replace("::", "_").to_lowercase();
                let folded = folded
                    .strip_prefix("__gos_")
                    .map_or(folded.clone(), str::to_string);
                (folded, name)
            })
            .collect()
    });
    by_folded.get(tail).copied()
}

/// The [`VM_NATIVE_EXEMPT`] entry answering for `symbol`: an exact entry,
/// or the longest family prefix that covers it.
#[must_use]
pub fn exemption_for(symbol: &str) -> Option<&'static (&'static str, &'static str)> {
    VM_NATIVE_EXEMPT
        .iter()
        .filter(|(pattern, _)| match pattern.strip_suffix('*') {
            Some(prefix) => symbol.starts_with(prefix),
            None => *pattern == symbol,
        })
        .max_by_key(|(pattern, _)| pattern.len())
}
