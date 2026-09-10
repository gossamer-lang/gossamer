fn install_io_builtins(globals: &mut Vec<(&'static str, Value)>) {
    globals.push(("println", builtin("println", builtin_println)));
    globals.push(("print", builtin("print", builtin_print)));
    install_math_builtins(globals);
    // Stream constructors - each returns an `io::Stream` value
    // the program's subsequent method calls dispatch against.
    globals.push(("io::stdout", builtin("io::stdout", builtin_io_stdout)));
    globals.push(("io::stderr", builtin("io::stderr", builtin_io_stderr)));
    globals.push(("io::stdin", builtin("io::stdin", builtin_io_stdin)));
    // Method-style shortcuts: `stream.write_byte(b)` dispatches
    // through the walker's generic method routing, which falls
    // back to a global named `write_byte`. Register one each
    // under the bare name + the `Stream::…` qualified key so
    // both lookup paths succeed.
    globals.push((
        "write_byte",
        builtin("write_byte", builtin_stream_write_byte),
    ));
    globals.push((
        "Stream::write_byte",
        builtin("Stream::write_byte", builtin_stream_write_byte),
    ));
    globals.push(("write", builtin("write", builtin_stream_write_str)));
    globals.push((
        "Stream::write",
        builtin("Stream::write", builtin_stream_write_str),
    ));
    globals.push(("write_str", builtin("write_str", builtin_stream_write_str)));
    globals.push((
        "Stream::write_str",
        builtin("Stream::write_str", builtin_stream_write_str),
    ));
    globals.push(("flush", builtin("flush", builtin_stream_flush)));
    globals.push((
        "Stream::flush",
        builtin("Stream::flush", builtin_stream_flush),
    ));
    globals.push(("read_line", builtin("read_line", builtin_stream_read_line)));
    globals.push((
        "Stream::read_line",
        builtin("Stream::read_line", builtin_stream_read_line),
    ));
    globals.push((
        "gos_rt_stream_next_line",
        builtin("gos_rt_stream_next_line", builtin_stream_read_line),
    ));
    globals.push((
        "read_to_string",
        builtin("read_to_string", builtin_stream_read_to_string),
    ));
    globals.push((
        "Stream::read_to_string",
        builtin("Stream::read_to_string", builtin_stream_read_to_string),
    ));
    // io::ReadAll(reader) / io::Copy(dst, src) helpers for moving
    // bytes around the fd-shaped stream values.
    globals.push(("io::ReadAll", builtin("io::ReadAll", builtin_io_read_all)));
    globals.push(("io::Copy", builtin("io::Copy", builtin_io_copy)));
    globals.push(("eprintln", builtin("eprintln", builtin_eprintln)));
    globals.push(("eprint", builtin("eprint", builtin_eprint)));
    globals.push(("format", builtin("format", builtin_format)));
    globals.push(("panic", builtin("panic", builtin_panic)));
    globals.push(("assert", builtin("assert", builtin_assert)));
    globals.push(("assert_eq", builtin("assert_eq", builtin_assert_eq)));
    globals.push(("__concat", builtin("__concat", builtin_concat)));
    globals.push(("__debug", builtin("__debug", builtin_debug)));
    globals.push(("__fmt_prec", builtin("__fmt_prec", builtin_fmt_prec)));
    globals.push(("__fmt_radix", builtin("__fmt_radix", builtin_fmt_radix)));
    globals.push(("__fmt_upper", builtin("__fmt_upper", builtin_fmt_upper)));
    globals.push(("__fmt_pad", builtin("__fmt_pad", builtin_fmt_pad)));
    globals.push((
        "__repl_discard",
        builtin("__repl_discard", builtin_repl_discard),
    ));
    globals.push(("__struct", builtin("__struct", builtin_struct_new)));
}

fn install_math_builtins(globals: &mut Vec<(&'static str, Value)>) {
    // Math library - mirrors the native runtime's
    // `gos_rt_math_*` surface. Registered under both the bare
    // name and the qualified `math::*` key the VM's
    // `compile_path` joins.
    globals.push(("sqrt", builtin("sqrt", builtin_math_sqrt)));
    globals.push(("math::sqrt", builtin("math::sqrt", builtin_math_sqrt)));
    globals.push(("sin", builtin("sin", builtin_math_sin)));
    globals.push(("math::sin", builtin("math::sin", builtin_math_sin)));
    globals.push(("cos", builtin("cos", builtin_math_cos)));
    globals.push(("math::cos", builtin("math::cos", builtin_math_cos)));
    globals.push(("exp", builtin("exp", builtin_math_exp)));
    globals.push(("math::exp", builtin("math::exp", builtin_math_exp)));
    globals.push(("ln", builtin("ln", builtin_math_ln)));
    globals.push(("log", builtin("log", builtin_math_ln)));
    globals.push(("math::ln", builtin("math::ln", builtin_math_ln)));
    globals.push(("math::log", builtin("math::log", builtin_math_ln)));
    globals.push(("abs", builtin("abs", builtin_math_abs)));
    globals.push(("math::abs", builtin("math::abs", builtin_math_abs)));
    globals.push(("floor", builtin("floor", builtin_math_floor)));
    globals.push(("math::floor", builtin("math::floor", builtin_math_floor)));
    globals.push(("ceil", builtin("ceil", builtin_math_ceil)));
    globals.push(("math::ceil", builtin("math::ceil", builtin_math_ceil)));
    globals.push(("pow", builtin("pow", builtin_math_pow)));
    globals.push(("math::pow", builtin("math::pow", builtin_math_pow)));
    for (name, function) in [
        ("f64::to_bits", builtin_f64_to_bits as BuiltinFn),
        ("f64::from_bits", builtin_f64_from_bits as BuiltinFn),
        ("f32::to_bits", builtin_f32_to_bits as BuiltinFn),
        ("f32::from_bits", builtin_f32_from_bits as BuiltinFn),
        ("wrapping_add", builtin_i64_wrapping_add as BuiltinFn),
        ("wrapping_mul", builtin_i64_wrapping_mul as BuiltinFn),
        ("i8::wrapping_add", builtin_i8_wrapping_add as BuiltinFn),
        ("i8::wrapping_mul", builtin_i8_wrapping_mul as BuiltinFn),
        ("i16::wrapping_add", builtin_i16_wrapping_add as BuiltinFn),
        ("i16::wrapping_mul", builtin_i16_wrapping_mul as BuiltinFn),
        ("i32::wrapping_add", builtin_i32_wrapping_add as BuiltinFn),
        ("i32::wrapping_mul", builtin_i32_wrapping_mul as BuiltinFn),
        ("i64::wrapping_add", builtin_i64_wrapping_add as BuiltinFn),
        ("i64::wrapping_mul", builtin_i64_wrapping_mul as BuiltinFn),
        ("isize::wrapping_add", builtin_i64_wrapping_add as BuiltinFn),
        ("isize::wrapping_mul", builtin_i64_wrapping_mul as BuiltinFn),
        ("u8::wrapping_add", builtin_u8_wrapping_add as BuiltinFn),
        ("u8::wrapping_mul", builtin_u8_wrapping_mul as BuiltinFn),
        ("u16::wrapping_add", builtin_u16_wrapping_add as BuiltinFn),
        ("u16::wrapping_mul", builtin_u16_wrapping_mul as BuiltinFn),
        ("u32::wrapping_add", builtin_u32_wrapping_add as BuiltinFn),
        ("u32::wrapping_mul", builtin_u32_wrapping_mul as BuiltinFn),
        ("u64::wrapping_add", builtin_u64_wrapping_add as BuiltinFn),
        ("u64::wrapping_mul", builtin_u64_wrapping_mul as BuiltinFn),
        ("usize::wrapping_add", builtin_u64_wrapping_add as BuiltinFn),
        ("usize::wrapping_mul", builtin_u64_wrapping_mul as BuiltinFn),
    ] {
        globals.push((name, builtin(name, function)));
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "flat registration list - splitting hides the per-arm intent"
)]
fn install_http_builtins(globals: &mut Vec<(&'static str, Value)>) {
    // HTTP server / client surface depends on host sockets, TLS, and
    // C-library codecs that the wasm browser sandbox cannot provide;
    // only the pure response-building builtins below stay available.
    #[cfg(not(target_arch = "wasm32"))]
    {
        globals.push(("http::serve", native("http::serve", native_http_serve)));
        globals.push((
            "httptest::server",
            native("httptest::server", native_httptest_server),
        ));
        globals.push((
            "httptest::record",
            native("httptest::record", native_httptest_record),
        ));
        globals.push((
            "http::serve_tls",
            native("http::serve_tls", native_http_serve_tls),
        ));
    }
    globals.push((
        "websocket::serve",
        native(
            "websocket::serve",
            crate::stdlib_builtins::http_ws::native_websocket_serve,
        ),
    ));
    globals.push((
        "http::websocket::serve",
        native(
            "http::websocket::serve",
            crate::stdlib_builtins::http_ws::native_websocket_serve,
        ),
    ));
    #[cfg(not(target_arch = "wasm32"))]
    {
        globals.push((
            "Router::serve",
            native("Router::serve", crate::stdlib_builtins::native_router_serve),
        ));
        globals.push((
            "FileServer::serve",
            native(
                "FileServer::serve",
                crate::stdlib_builtins::native_file_server_serve,
            ),
        ));
        globals.push((
            "Middleware::serve",
            native(
                "Middleware::serve",
                crate::stdlib_builtins::native_middleware_serve,
            ),
        ));
        // HTTP/2 folded into std::http per the Go model. Canonical
        // names live under http::*; nothing exposes the old http2::
        // module path any more (the interp dispatch did, briefly,
        // during 0.4.0 dev - it's gone now).
        globals.push((
            "http::serve_h2c",
            native("http::serve_h2c", native_http2_bind_and_run_h2c),
        ));
        globals.push((
            "http_h3::serve",
            native("http_h3::serve", native_http3_serve),
        ));
        globals.push((
            "http::Http2Config::default",
            builtin("http::Http2Config::default", builtin_http2_config_default),
        ));
    }
    globals.push((
        "http::Response::text",
        builtin("http::Response::text", builtin_http_response_text),
    ));
    globals.push((
        "http::Response::json",
        builtin("http::Response::json", builtin_http_response_json),
    ));
    globals.push((
        "Response::text",
        builtin("Response::text", builtin_http_response_text),
    ));
    globals.push((
        "Response::json",
        builtin("Response::json", builtin_http_response_json),
    ));
    #[cfg(not(target_arch = "wasm32"))]
    {
        globals.push((
            "http::Response::stream",
            builtin("http::Response::stream", builtin_http_response_stream),
        ));
        globals.push((
            "Response::stream",
            builtin("Response::stream", builtin_http_response_stream),
        ));
    }
    globals.push((
        "http::Response::with_header",
        builtin(
            "http::Response::with_header",
            builtin_http_response_with_header,
        ),
    ));
    globals.push((
        "Response::with_header",
        builtin("Response::with_header", builtin_http_response_with_header),
    ));
    #[cfg(not(target_arch = "wasm32"))]
    {
        globals.push((
            "http::Client::new",
            builtin(
                "http::Client::new",
                crate::http_client_builtins::builtin_http_client_new,
            ),
        ));
        globals.push((
            "http::Client::builder",
            builtin(
                "http::Client::builder",
                crate::http_client_builtins::builtin_http_client_builder,
            ),
        ));
        globals.push((
            "ClientBuilder::max_redirects",
            builtin(
                "ClientBuilder::max_redirects",
                crate::http_client_builtins::builtin_http_client_builder_max_redirects,
            ),
        ));
        globals.push((
            "ClientBuilder::timeout_ms",
            builtin(
                "ClientBuilder::timeout_ms",
                crate::http_client_builtins::builtin_http_client_builder_timeout_ms,
            ),
        ));
        globals.push((
            "ClientBuilder::cookie_jar",
            builtin(
                "ClientBuilder::cookie_jar",
                crate::http_client_builtins::builtin_http_client_builder_cookie_jar,
            ),
        ));
        globals.push((
            "ClientBuilder::proxy",
            builtin(
                "ClientBuilder::proxy",
                crate::http_client_builtins::builtin_http_client_builder_proxy,
            ),
        ));
        globals.push((
            "ClientBuilder::build",
            builtin(
                "ClientBuilder::build",
                crate::http_client_builtins::builtin_http_client_builder_build,
            ),
        ));
        globals.push((
            "Client::request",
            builtin(
                "Client::request",
                crate::http_client_builtins::builtin_http_client_request,
            ),
        ));
        globals.push((
            "Client::request_bytes",
            builtin(
                "Client::request_bytes",
                crate::http_client_builtins::builtin_http_client_request_bytes,
            ),
        ));
        globals.push((
            "Client::get",
            builtin(
                "Client::get",
                crate::http_client_builtins::builtin_http_client_get,
            ),
        ));
        globals.push((
            "Client::post",
            builtin(
                "Client::post",
                crate::http_client_builtins::builtin_http_client_post,
            ),
        ));
        globals.push((
            "Client::put",
            builtin(
                "Client::put",
                crate::http_client_builtins::builtin_http_client_put,
            ),
        ));
        globals.push((
            "Client::options",
            builtin(
                "Client::options",
                crate::http_client_builtins::builtin_http_client_options,
            ),
        ));
        globals.push((
            "Client::delete",
            builtin(
                "Client::delete",
                crate::http_client_builtins::builtin_http_client_delete,
            ),
        ));
        globals.push((
            "Client::head",
            builtin(
                "Client::head",
                crate::http_client_builtins::builtin_http_client_head,
            ),
        ));
        globals.push((
            "Request::header",
            builtin(
                "Request::header",
                crate::http_client_builtins::builtin_http_request_header,
            ),
        ));
        globals.push((
            "Request::body",
            builtin(
                "Request::body",
                crate::http_client_builtins::builtin_http_request_body,
            ),
        ));
        globals.push((
            "Request::send",
            builtin(
                "Request::send",
                crate::http_client_builtins::builtin_http_request_send,
            ),
        ));
        globals.push((
            "Response::bytes",
            builtin(
                "Response::bytes",
                crate::http_client_builtins::builtin_http_response_bytes,
            ),
        ));
        // Free-function client surface: http::request, http::stream, plus
        // method-specific convenience wrappers.
        globals.push((
            "http::request",
            builtin(
                "http::request",
                crate::http_client_builtins::builtin_http_request,
            ),
        ));
        globals.push((
            "http::request_bytes",
            builtin(
                "http::request_bytes",
                crate::http_client_builtins::builtin_http_request_bytes,
            ),
        ));
        globals.push((
            "http::get",
            builtin("http::get", crate::http_client_builtins::builtin_http_get),
        ));
        globals.push((
            "http::post",
            builtin("http::post", crate::http_client_builtins::builtin_http_post),
        ));
        globals.push((
            "http::put",
            builtin("http::put", crate::http_client_builtins::builtin_http_put),
        ));
        globals.push((
            "http::options",
            builtin(
                "http::options",
                crate::http_client_builtins::builtin_http_options,
            ),
        ));
        globals.push((
            "http::delete",
            builtin(
                "http::delete",
                crate::http_client_builtins::builtin_http_delete,
            ),
        ));
        globals.push((
            "http::head",
            builtin("http::head", crate::http_client_builtins::builtin_http_head),
        ));
        globals.push((
            "http::stream",
            builtin(
                "http::stream",
                crate::http_client_builtins::builtin_http_stream,
            ),
        ));
        globals.push((
            "ResponseStream::next_line",
            builtin(
                "ResponseStream::next_line",
                crate::http_client_builtins::builtin_response_stream_next_line,
            ),
        ));
        globals.push((
            "ResponseStream::next_chunk",
            builtin(
                "ResponseStream::next_chunk",
                crate::http_client_builtins::builtin_response_stream_next_chunk,
            ),
        ));
    }
    globals.push(("path", builtin("path", builtin_field::<'p'>)));
    globals.push(("method", builtin("method", builtin_field::<'m'>)));
}

fn install_variant_builtins(globals: &mut Vec<(&'static str, Value)>) {
    globals.push(("Ok", builtin("Ok", builtin_variant_one::<'O'>)));
    globals.push(("Err", builtin("Err", builtin_variant_one::<'E'>)));
    globals.push(("Some", builtin("Some", builtin_variant_one::<'S'>)));
    globals.push(("None", Value::variant("None", Vec::new())));
}

// Pure registration list - splitting it would just split the
// install across files without making any function shorter.
#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn install_module_builtins(globals: &mut Vec<(&'static str, Value)>) {
    install_module(
        "os",
        &[
            // os identity (canonical - stays on os::).
            ("family", builtin_os_family),
            ("arch", builtin_os_arch),
        ],
        globals,
    );
    install_module(
        "env",
        &[
            ("args", builtin_os_args),
            ("program_name", builtin_os_program_name),
            ("var", builtin_os_env),
            ("set_var", builtin_env_set_var),
            ("unset_var", builtin_env_unset_var),
            ("current_dir", builtin_os_cwd),
            ("set_current_dir", builtin_env_set_current_dir),
            ("home_dir", builtin_env_home_dir),
            ("temp_dir", builtin_env_temp_dir),
            ("vars", builtin_env_vars),
        ],
        globals,
    );
    install_module(
        "time",
        &[
            ("now", builtin_time_now),
            ("now_ms", builtin_time_now_ms),
            ("freeze", builtin_time_freeze),
            ("advance", builtin_time_advance),
            ("unfreeze", builtin_time_unfreeze),
            ("is_frozen", builtin_time_is_frozen),
            ("sleep", crate::stdlib_builtins::time_completeness::builtin_time_sleep),
            ("sleep_ctx", builtin_time_sleep_ctx),
            ("format_rfc3339", builtin_time_format_rfc3339),
            ("parse_rfc3339", builtin_time_parse_rfc3339),
        ],
        globals,
    );
    install_module(
        "pprof",
        &[
            ("cpu_profile", builtin_pprof_cpu_profile),
            ("heap_profile", builtin_pprof_heap_profile),
            ("goroutine_profile", builtin_pprof_goroutine_profile),
            ("mutex_profile", builtin_pprof_mutex_profile),
            ("block_profile", builtin_pprof_block_profile),
            ("execution_trace", builtin_pprof_execution_trace),
            ("route", builtin_pprof_route),
        ],
        globals,
    );
    install_module(
        "runtime",
        &[
            ("collect_cycles", builtin_runtime_collect_cycles),
            (
                "cycle_collection_supported",
                builtin_runtime_cycle_collection_supported,
            ),
            ("arena_push", builtin_runtime_region_noop),
            ("arena_pop", builtin_runtime_region_noop),
            ("scheduler_stats_json", builtin_runtime_scheduler_stats_json),
            ("set_panic_hook", builtin_runtime_set_panic_hook),
        ],
        globals,
    );
    // Bare `exec::*` is a back-compat alias for `process::*` /
    // `os::exec::*`. New code should prefer `process::*`.
    install_module(
        "exec",
        &[
            ("run", builtin_exec_run),
            ("spawn", builtin_exec_spawn),
            ("spawn_piped", builtin_exec_spawn_piped),
            ("kill", builtin_exec_kill),
            ("signal", builtin_exec_signal),
            ("kill_group", builtin_exec_kill_group),
            ("wait_timeout", builtin_exec_wait_timeout),
            ("pipeline_run", builtin_exec_pipeline_run),
        ],
        globals,
    );
    install_module(
        "os::exec",
        &[
            ("run", builtin_exec_run),
            ("spawn", builtin_exec_spawn),
            ("spawn_piped", builtin_exec_spawn_piped),
            ("kill", builtin_exec_kill),
            ("signal", builtin_exec_signal),
            ("kill_group", builtin_exec_kill_group),
            ("wait_timeout", builtin_exec_wait_timeout),
            ("pipeline_run", builtin_exec_pipeline_run),
        ],
        globals,
    );
    install_module(
        "process",
        &[
            ("run", builtin_exec_run),
            ("run_in", builtin_exec_run_in),
            ("spawn", builtin_exec_spawn),
            ("spawn_piped", builtin_exec_spawn_piped),
            ("kill", builtin_exec_kill),
            ("signal", builtin_exec_signal),
            ("kill_group", builtin_exec_kill_group),
            ("wait_timeout", builtin_exec_wait_timeout),
            ("pipeline_run", builtin_exec_pipeline_run),
            ("exit", builtin_os_exit),
            ("id", builtin_process_id),
            ("abort", builtin_process_abort),
        ],
        globals,
    );
    // `process::Child` piped-handle methods, dispatched by the
    // receiver struct's qualified name like `WaitGroup::*`.
    for (name, call) in [
        ("Child::write_stdin", builtin_child_write_stdin as BuiltinFn),
        ("Child::close_stdin", builtin_child_close_stdin),
        ("Child::read_line", builtin_child_read_line),
        ("Child::read_stdout", builtin_child_read_stdout),
        ("Child::wait", builtin_child_wait),
        ("Child::kill", builtin_child_kill),
    ] {
        let leaked: &'static str = name;
        globals.push((leaked, builtin(leaked, call)));
    }
    install_module(
        "signal",
        &[
            ("on", builtin_signal_on),
            ("wait", builtin_signal_wait),
            ("try_wait", builtin_signal_try_wait),
        ],
        globals,
    );
    install_module(
        "os::signal",
        &[
            ("on", builtin_signal_on),
            ("wait", builtin_signal_wait),
            ("try_wait", builtin_signal_try_wait),
        ],
        globals,
    );
    globals.push(("signal_wait", builtin("signal_wait", builtin_signal_wait)));
    globals.push((
        "signal_try_wait",
        builtin("signal_try_wait", builtin_signal_try_wait),
    ));
    globals.push(("walk_dir", native("walk_dir", native_fs_walk_dir)));
    globals.push(("fs::walk_dir", native("fs::walk_dir", native_fs_walk_dir)));
    install_module(
        "fs",
        &[
            ("read", builtin_os_read_file),
            ("read_to_string", builtin_os_read_file_to_string),
            ("write", builtin_os_write_file),
            ("create_dir_all", builtin_os_mkdir_all),
            ("create_dir", builtin_os_mkdir),
            ("remove_file", builtin_os_remove_file),
            ("remove_dir", builtin_fs_remove_dir),
            ("remove_dir_all", builtin_fs_remove_dir_all),
            ("rename", builtin_os_rename),
            ("read_dir", builtin_fs_list_dir),
            ("exists", builtin_os_exists),
            ("is_file", builtin_fs_is_file),
            ("is_dir", builtin_fs_is_dir),
            ("is_symlink", builtin_fs_is_symlink),
            ("file_size", builtin_fs_file_size),
            ("canonicalize", builtin_fs_canonicalize),
        ],
        globals,
    );
    // `path::walk` was deprecated in favour of `fs::walk_dir`; the
    // dispatch entry stays for one release so existing user code keeps
    // resolving while we migrate examples.
    globals.push(("walk", native("walk", native_fs_walk_dir)));
    globals.push(("path::walk", native("path::walk", native_fs_walk_dir)));
    install_module("path", &[("join", builtin_path_join_v)], globals);
    install_module(
        "BTreeMap",
        &[("new", builtin_btmap_new), ("from", builtin_map_from)],
        globals,
    );
    install_module(
        "collections::BTreeMap",
        &[("new", builtin_btmap_new), ("from", builtin_map_from)],
        globals,
    );
    install_module("Set", &[("new", builtin_set_new)], globals);
    install_module("collections::Set", &[("new", builtin_set_new)], globals);
    install_module(
        "BTreeSet",
        &[("new", crate::stdlib_builtins::builtin_btreeset_new)],
        globals,
    );
    install_module(
        "collections::BTreeSet",
        &[("new", crate::stdlib_builtins::builtin_btreeset_new)],
        globals,
    );
    install_module(
        "time::Duration",
        &[
            ("from_millis", builtin_duration_passthrough),
            ("from_secs", builtin_duration_secs_to_ms),
        ],
        globals,
    );
    globals.push(("to_vec", builtin("to_vec", builtin_to_vec_v)));
    install_module(
        "slog",
        &[
            ("info", builtin_slog_info),
            ("warn", builtin_slog_warn),
            ("error", builtin_slog_error),
            ("debug", builtin_slog_debug),
        ],
        globals,
    );
    install_module(
        "bufio",
        &[
            ("read_lines", builtin_bufio_read_lines),
            ("Scanner::new", builtin_bufio_scanner_new),
            ("Scanner::next", builtin_bufio_scanner_next),
            ("Scanner::scan", builtin_bufio_scanner_scan),
            ("Scanner::text", builtin_bufio_scanner_text),
        ],
        globals,
    );
    // Bare names so user code can write `Scanner::new(stream)` /
    // `s.scan()` without an explicit `bufio::` prefix.
    globals.push((
        "Scanner::new",
        builtin("Scanner::new", builtin_bufio_scanner_new),
    ));
    globals.push((
        "Scanner::next",
        builtin("Scanner::next", builtin_bufio_scanner_next),
    ));
    globals.push((
        "Scanner::scan",
        builtin("Scanner::scan", builtin_bufio_scanner_scan),
    ));
    globals.push((
        "Scanner::text",
        builtin("Scanner::text", builtin_bufio_scanner_text),
    ));
    // Map surface - exposed both qualified (`Map::*`) and bare
    // (`m.get(k)`, `m.insert(k, v)`) so user code can use the
    // method form. Mutating methods
    // (insert/remove/clear) ride the method-dispatch writeback path
    // same as Vec mutators.
    install_module(
        "Map",
        &[
            ("new", builtin_map_new),
            ("from", builtin_map_from),
            ("with_capacity", builtin_map_with_capacity),
            ("get", builtin_map_get),
            ("get_or", builtin_map_get_or),
            ("inc", builtin_map_inc),
            ("or_insert", builtin_map_or_insert),
            ("inc_at", builtin_map_inc_at),
            ("inc_batch", builtin_map_inc_batch),
            ("insert", builtin_map_insert),
            ("remove", builtin_map_remove),
            ("contains_key", builtin_map_contains_key),
            ("len", builtin_map_len),
            ("keys", builtin_map_keys),
            ("values", builtin_map_values),
            ("iter", builtin_map_iter),
            ("clear", builtin_map_clear),
            ("is_empty", builtin_map_is_empty),
        ],
        globals,
    );
    install_module(
        "collections::Map",
        &[
            ("new", builtin_map_new),
            ("from", builtin_map_from),
            ("with_capacity", builtin_map_with_capacity),
            ("get", builtin_map_get),
            ("get_or", builtin_map_get_or),
            ("inc", builtin_map_inc),
            ("or_insert", builtin_map_or_insert),
            ("inc_at", builtin_map_inc_at),
            ("inc_batch", builtin_map_inc_batch),
            ("insert", builtin_map_insert),
            ("remove", builtin_map_remove),
            ("contains_key", builtin_map_contains_key),
            ("len", builtin_map_len),
            ("keys", builtin_map_keys),
            ("values", builtin_map_values),
            ("iter", builtin_map_iter),
            ("clear", builtin_map_clear),
            ("is_empty", builtin_map_is_empty),
        ],
        globals,
    );
    // Bare-name surface for method-call dispatch on a Map receiver.
    // The `qualified_method_key(receiver, "get")` lookup misses for
    // Map values (no struct name to derive a key from), so the
    // bare-name fallback in `eval_method_call` does the dispatch.
    globals.push((
        "contains_key",
        builtin("contains_key", builtin_map_contains_key),
    ));
    globals.push(("keys", builtin("keys", builtin_map_keys)));
    globals.push(("values", builtin("values", builtin_map_values)));
    globals.push(("iter", builtin("iter", builtin_map_iter)));
    globals.push(("get_or", builtin("get_or", builtin_map_get_or)));
    globals.push(("inc", builtin("inc", builtin_map_inc)));
    globals.push(("or_insert", builtin("or_insert", builtin_map_or_insert)));
    // Re-register `get` as the receiver router after the qualified
    // module entries. Later stdlib modules also expose bare `get`, so
    // leaving the first registration in place lets JSON/http surfaces
    // shadow HashMap::get and makes map lookups silently return None.
    globals.push(("get", builtin("get", builtin_get_router)));
    // `insert` and `remove` and `len` and `clear` already exist as bare
    // names for other types; the builtin already routes by receiver so
    // we don't double-register.

    install_module(
        "json",
        &[
            ("parse", builtin_json_parse),
            ("render", builtin_json_render),
            ("encode", builtin_json_render),
            ("decode", builtin_json_decode),
            // Query surface - operates on the dynamic struct shape
            // produced by `json_value_to_gossamer`, so a JSON object
            // is a struct keyed by field name and a JSON array is a
            // `Value::Array`.
            ("get", builtin_json_get),
            ("set", builtin_json_set),
            ("at", builtin_json_at),
            ("keys", builtin_json_keys),
            ("len", builtin_json_len),
            ("is_null", builtin_json_is_null),
            ("as_str", builtin_json_as_str),
            ("as_i64", builtin_json_as_i64),
            ("as_f64", builtin_json_as_f64),
            ("as_bool", builtin_json_as_bool),
            ("as_array", builtin_json_as_array),
        ],
        globals,
    );
    install_module(
        "encoding::json",
        &[
            ("parse", builtin_json_parse),
            ("render", builtin_json_render),
            ("encode", builtin_json_render),
            ("decode", builtin_json_decode),
            ("get", builtin_json_get),
            ("set", builtin_json_set),
            ("at", builtin_json_at),
            ("keys", builtin_json_keys),
            ("len", builtin_json_len),
            ("is_null", builtin_json_is_null),
            ("as_str", builtin_json_as_str),
            ("as_i64", builtin_json_as_i64),
            ("as_f64", builtin_json_as_f64),
            ("as_bool", builtin_json_as_bool),
            ("as_array", builtin_json_as_array),
        ],
        globals,
    );
    // Re-register the bare `keys` / `values` entries as receiver-
    // dispatching routers so `install_module("json", …)`'s
    // unconditional bare-name push (above) doesn't shadow the
    // HashMap surface. `HashMap::keys` and `json::keys` qualified
    // entries stay pointed at their dedicated builtins; only the
    // bare-name resolver dispatches by receiver shape.
    globals.push(("keys", builtin("keys", builtin_keys_router)));
    globals.push(("values", builtin("values", builtin_values_router)));
    // Same shape collision for bare `get`: `install_module("json", …)`
    // registers `("get", builtin_json_get)` which would otherwise
    // shadow `HashMap`'s `("get", builtin_map_get)` in the bare-name
    // resolver. `builtin_json_get` returns `None` for a `Value::Map`
    // receiver, so `match m.get(&k) { Some(v) => … }` always took the
    // `None` arm once the scrutinee was evaluated natively. Route by
    // receiver shape so both surfaces resolve correctly.
    globals.push(("get", builtin("get", builtin_get_router)));
    // `json::Value::*` enum constructors used by user code that
    // builds a payload before serialising.
    globals.push((
        "json::Value::String",
        builtin("json::Value::String", builtin_json_value_passthrough),
    ));
    globals.push((
        "json::Value::Int",
        builtin("json::Value::Int", builtin_json_value_passthrough),
    ));
    globals.push((
        "json::Value::Float",
        builtin("json::Value::Float", builtin_json_value_passthrough),
    ));
    globals.push((
        "json::Value::Bool",
        builtin("json::Value::Bool", builtin_json_value_passthrough),
    ));
    globals.push((
        "json::Value::Array",
        builtin("json::Value::Array", builtin_json_value_passthrough),
    ));
    globals.push((
        "json::Value::Null",
        builtin("json::Value::Null", builtin_json_value_null),
    ));
    globals.push((
        "json::Value::object",
        builtin("json::Value::object", builtin_json_value_object),
    ));
    globals.push((
        "json::Value::Object",
        builtin("json::Value::Object", builtin_json_value_object),
    ));
    install_module(
        "testing",
        &[
            ("check", builtin_testing_check),
            ("check_eq", builtin_testing_check_eq),
            ("check_ok", builtin_testing_check_ok),
            (
                "wait_for_scheduler_idle",
                builtin_testing_wait_for_scheduler_idle,
            ),
        ],
        globals,
    );
}

fn install_flag_builtins(globals: &mut Vec<(&'static str, Value)>) {
    globals.push(("flag::Value::Int", Value::variant("Int", Vec::new())));
    globals.push(("flag::Value::Str", Value::variant("Str", Vec::new())));
    globals.push(("flag::Value::Bool", Value::variant("Bool", Vec::new())));
    globals.push(("flag::parse", builtin("flag::parse", builtin_flag_parse)));
    globals.push((
        "FlagMap::get",
        builtin("FlagMap::get", builtin_flag_map_get),
    ));
    globals.push((
        "flag::Set::new",
        builtin(
            "flag::Set::new",
            crate::flag_set_builtins::builtin_flag_set_new,
        ),
    ));
    globals.push((
        "Set::string",
        builtin(
            "Set::string",
            crate::flag_set_builtins::builtin_flag_set_string,
        ),
    ));
    globals.push((
        "Set::int",
        builtin("Set::int", crate::flag_set_builtins::builtin_flag_set_int),
    ));
    globals.push((
        "Set::uint",
        builtin("Set::uint", crate::flag_set_builtins::builtin_flag_set_uint),
    ));
    globals.push((
        "Set::bool",
        builtin("Set::bool", crate::flag_set_builtins::builtin_flag_set_bool),
    ));
    globals.push((
        "Set::float",
        builtin(
            "Set::float",
            crate::flag_set_builtins::builtin_flag_set_float,
        ),
    ));
    globals.push((
        "Set::duration",
        builtin(
            "Set::duration",
            crate::flag_set_builtins::builtin_flag_set_duration,
        ),
    ));
    globals.push((
        "Set::string_list",
        builtin(
            "Set::string_list",
            crate::flag_set_builtins::builtin_flag_set_string_list,
        ),
    ));
    globals.push((
        "Set::usage",
        builtin(
            "Set::usage",
            crate::flag_set_builtins::builtin_flag_set_usage,
        ),
    ));
    globals.push((
        "Set::short",
        builtin(
            "Set::short",
            crate::flag_set_builtins::builtin_flag_set_short,
        ),
    ));
    globals.push((
        "Set::parse",
        builtin(
            "Set::parse",
            crate::flag_set_builtins::builtin_flag_set_parse,
        ),
    ));
    // Declarative builder: one expression produces a ready-to-use
    // flags struct whose fields deref through `__Cell` to the current
    // value. Avoids the mutate-the-set chain the Set:: builders use.
    globals.push(("flag::int", builtin("flag::int", builtin_flag_spec_int)));
    globals.push((
        "flag::string",
        builtin("flag::string", builtin_flag_spec_string),
    ));
    globals.push(("flag::bool", builtin("flag::bool", builtin_flag_spec_bool)));
    globals.push(("flag::define", builtin("flag::define", builtin_flag_define)));
}

/// Shape of a single spec produced by Gossamer's `flag::int` /
/// `flag::string` / `flag::bool` and consumed by `flag::define`.
/// Fields: kind (`"int"` / `"string"` / `"bool"`), long, default,
/// help, short.
fn flag_spec(kind: &str, long: &str, default: Value, help: &str, short: Option<char>) -> Value {
    Value::struct_(
        "FlagSpec",
        vec![
            ("kind", Value::String(SmolStr::from(kind.to_string()))),
            ("long", Value::String(SmolStr::from(long.to_string()))),
            ("default", default),
            ("help", Value::String(SmolStr::from(help.to_string()))),
            (
                "short",
                match short {
                    Some(c) => Value::Char(c),
                    None => Value::Unit,
                },
            ),
        ],
    )
}

fn builtin_flag_spec_int(args: &[Value]) -> RuntimeResult<Value> {
    let long = args.first().and_then(as_str).unwrap_or("");
    let default = args.get(1).cloned().unwrap_or(Value::Int(0));
    let help = args.get(2).and_then(as_str).unwrap_or("");
    let short = match args.get(3) {
        Some(Value::Char(c)) => Some(*c),
        _ => None,
    };
    Ok(flag_spec("int", long, default, help, short))
}

fn builtin_flag_spec_string(args: &[Value]) -> RuntimeResult<Value> {
    let long = args.first().and_then(as_str).unwrap_or("");
    let default = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| Value::String(SmolStr::from(String::new())));
    let help = args.get(2).and_then(as_str).unwrap_or("");
    let short = match args.get(3) {
        Some(Value::Char(c)) => Some(*c),
        _ => None,
    };
    Ok(flag_spec("string", long, default, help, short))
}

fn builtin_flag_spec_bool(args: &[Value]) -> RuntimeResult<Value> {
    let long = args.first().and_then(as_str).unwrap_or("");
    let default = args.get(1).cloned().unwrap_or(Value::Bool(false));
    let help = args.get(2).and_then(as_str).unwrap_or("");
    let short = match args.get(3) {
        Some(Value::Char(c)) => Some(*c),
        _ => None,
    };
    Ok(flag_spec("bool", long, default, help, short))
}

/// Registers `spec` inside the set identified by `set_id` and
/// returns the `(long_name, cell_value)` pair for the generated
/// `Flags` struct. Pulled out of `builtin_flag_define` so the
/// entry-point stays short enough for clippy's body-length lint.
fn register_flag_spec(set_id: u64, spec: &Value) -> Option<(&'static str, Value)> {
    let Value::Struct(spec_inner) = spec else {
        return None;
    };
    let spec_name = spec_inner.name.clone();
    let spec_fields = &spec_inner.fields;
    if spec_name != "FlagSpec" {
        return None;
    }
    let kind = spec_fields
        .iter()
        .find(|(i, _)| (**i) == "kind")
        .and_then(|(_, v)| as_str(v))
        .unwrap_or("")
        .to_string();
    let long = spec_fields
        .iter()
        .find(|(i, _)| (**i) == "long")
        .and_then(|(_, v)| as_str(v))
        .unwrap_or("")
        .to_string();
    let default = spec_fields
        .iter()
        .find(|(i, _)| (**i) == "default")
        .map_or(Value::Unit, |(_, v)| v.clone());
    let help = spec_fields
        .iter()
        .find(|(i, _)| (**i) == "help")
        .and_then(|(_, v)| as_str(v))
        .unwrap_or("")
        .to_string();
    let short = spec_fields
        .iter()
        .find(|(i, _)| (**i) == "short")
        .and_then(|(_, v)| match v {
            Value::Char(c) => Some(*c),
            _ => None,
        });
    let flag_kind = match kind.as_str() {
        "int" => FlagKind::Int,
        "string" => FlagKind::String,
        "bool" => FlagKind::Bool,
        _ => return None,
    };
    SET_REGISTRY.with(|reg| {
        if let Some(state) = reg.borrow_mut().get_mut(&set_id) {
            state.flag_order.push(long.clone());
            state.flags.insert(
                long.clone(),
                FlagDef {
                    short,
                    kind: flag_kind,
                    help,
                    default: default.clone(),
                },
            );
        }
    });
    let cell = make_cell(set_id, &long, default);
    Some((crate::value::intern_type_name(&long), cell))
}

/// Batch constructor. Creates the internal `Set`, registers every
/// spec, parses `env::args()`, and returns a `Flags` struct with one
/// cell-typed field per spec (named after the spec's long name).
/// Callers access parsed values via `*flags.<long>` - no mutation
/// needed at the call site.
fn builtin_flag_define(args: &[Value]) -> RuntimeResult<Value> {
    let set_name = args.first().and_then(as_str).unwrap_or("").to_string();
    let specs: &[Value] = match args.get(1) {
        Some(Value::Array(arr)) => arr.as_ref().as_slice(),
        _ => &[],
    };
    let set_id = NEXT_SET_ID.with(|cell| {
        let mut v = cell.borrow_mut();
        let id = *v;
        *v += 1;
        id
    });
    SET_REGISTRY.with(|reg| {
        reg.borrow_mut().insert(
            set_id,
            SetState {
                name: set_name,
                flag_order: Vec::new(),
                last_flag: None,
                flags: std::collections::HashMap::new(),
            },
        );
    });
    let mut fields: Vec<(&'static str, Value)> = Vec::with_capacity(specs.len() + 1);
    fields.push(("__set_id", Value::Int(i64::try_from(set_id).unwrap_or(0))));
    for spec in specs {
        if let Some(entry) = register_flag_spec(set_id, spec) {
            fields.push(entry);
        }
    }
    let args_vec = program_args();
    let args_array = Value::Array(Arc::new(
        args_vec
            .into_iter()
            .map(|s| Value::String(s.into()))
            .collect(),
    ));
    let set_value = Value::struct_(
        "Set",
        vec![("__id", Value::Int(i64::try_from(set_id).unwrap_or(0)))],
    );
    let _ = crate::flag_set_builtins::builtin_flag_set_parse(&[set_value, args_array]);
    Ok(Value::struct_(
        "Flags",
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "flat dispatch-registration list; splitting hides per-arm intent"
)]
fn install_method_helpers(globals: &mut Vec<(&'static str, Value)>) {
    globals.push(("len", builtin("len", builtin_len)));
    globals.push(("is_empty", builtin("is_empty", builtin_is_empty)));
    globals.push(("to_string", builtin("to_string", builtin_to_string)));
    globals.push(("split", builtin("split", builtin_split)));
    globals.push(("trim", builtin("trim", builtin_trim)));
    globals.push(("as_bytes", builtin("as_bytes", builtin_as_bytes)));
    globals.push(("String::bytes", builtin("String::bytes", builtin_as_bytes)));
    globals.push(("push", builtin("push", builtin_push)));
    globals.push(("pop", builtin("pop", builtin_pop)));
    globals.push(("insert", builtin("insert", builtin_insert)));
    globals.push(("remove", builtin("remove", builtin_remove)));
    globals.push(("clear", builtin("clear", builtin_clear)));
    globals.push(("extend", builtin("extend", builtin_extend)));
    globals.push((
        "extend_from_slice",
        builtin("extend_from_slice", builtin_extend),
    ));
    globals.push(("truncate", builtin("truncate", builtin_truncate)));
    globals.push(("resize", builtin("resize", builtin_resize)));
    globals.push((
        "copy_within",
        builtin("copy_within", builtin_copy_within),
    ));
    globals.push((
        "copy_from_slice",
        builtin("copy_from_slice", builtin_copy_from_slice),
    ));
    globals.push((
        "binary_search",
        builtin("binary_search", builtin_binary_search),
    ));
    globals.push(("reserve", builtin("reserve", builtin_vec_reserve)));
    globals.push((
        "reserve_exact",
        builtin("reserve_exact", builtin_vec_reserve_exact),
    ));
    globals.push(("capacity", builtin("capacity", builtin_vec_capacity)));
    globals.push(("sort", builtin("sort", builtin_sort)));
    globals.push(("sort_by", native("sort_by", native_sort_by)));
    globals.push((
        "__join_rendered",
        native("__join_rendered", native_join_rendered),
    ));
    globals.push((
        "__render_display",
        native("__render_display", native_render_display),
    ));
    globals.push(("reverse", builtin("reverse", builtin_reverse)));
    globals.push(("swap", builtin("swap", builtin_swap)));
    globals.push(("fill", builtin("fill", builtin_fill)));
    globals.push(("clone", builtin("clone", builtin_clone)));
    // `iter().next()` outside a for-loop. Returns `Some(first)`
    // for non-empty collections, `None` otherwise. The for-loop
    // fast paths still drive real iteration state - this binding
    // only covers the bare-call shape (`let v = it.next()` and
    // `match it.next() { Some(_) => …, None => … }`).
    globals.push(("next", builtin("next", builtin_next)));
    // `Box<T>` / `Arc<T>` / `Rc<T>` are transparent in a fully GC'd
    // language: every value is heap-shared already, so the wrapper
    // type is purely a Rust-flavoured ergonomic spelling. The
    // constructors return their argument unchanged so user code
    // that writes `Box::new(rest)` for a recursive enum payload
    // (or pattern-matches on the unwrapped value) works without a
    // distinct runtime representation.
    globals.push(("Box::new", builtin("Box::new", builtin_clone)));
    globals.push(("Arc::new", builtin("Arc::new", builtin_clone)));
    globals.push(("Rc::new", builtin("Rc::new", builtin_clone)));
    // `x.downgrade()` / `w.upgrade()` - weak references. `downgrade`
    // records a `std::sync::Weak` to the receiver's `Arc` (mirroring the
    // compiled tier's `gos_rt_rc_downgrade`); `upgrade` yields
    // `Some(value)` while a strong reference survives and `None` once the
    // last one is dropped (mirroring `gos_rt_rc_weak_upgrade_opt`).
    globals.push(("downgrade", builtin("downgrade", builtin_downgrade)));
    globals.push(("upgrade", builtin("upgrade", builtin_upgrade)));
    // String surface that the MIR method-dispatch table already
    // wires for compiled mode. Keep the interpreter's coverage
    // in lockstep so `gos` and `gos build` agree. The canonical
    // casing spellings are `to_lower` / `to_upper` (registered under
    // the `String::` key below); the longer Rust-style aliases are
    // not exposed.
    globals.push(("contains", builtin("contains", builtin_contains)));
    globals.push(("starts_with", builtin("starts_with", builtin_starts_with)));
    globals.push(("ends_with", builtin("ends_with", builtin_ends_with)));
    globals.push(("replace", builtin("replace", builtin_str_replace)));
    globals.push(("find", builtin("find", builtin_str_find)));
    globals.push((
        "String::byte_len",
        builtin("String::byte_len", builtin_str_byte_len),
    ));
    globals.push(("byte_len", builtin("byte_len", builtin_str_byte_len)));
    // `String::byte_at(s, i) -> i64`. Qualified key dominates any
    // user free fn named `byte_at` during method dispatch; bare key
    // lets `byte_at(s, i)` resolve too.
    globals.push((
        "String::byte_at",
        builtin("String::byte_at", builtin_str_byte_at),
    ));
    globals.push(("byte_at", builtin("byte_at", builtin_str_byte_at)));
    // `String::substring(s, a, b) -> String` - clamping, infallible
    // character-range substring (out-of-range bounds clamp; inverted bounds
    // yield ""). Mirrors the compiled tier's `gos_rt_str_substring`.
    // Registered qualified + bare so `s.substring(a, b)` dispatches by
    // type and `substring(s, a, b)` resolves too.
    globals.push((
        "String::substring",
        builtin("String::substring", builtin_str_substring),
    ));
    globals.push(("substring", builtin("substring", builtin_str_substring)));
    // `String::slice(s, a, b) -> Result<String, errors::Error>` -
    // the non-panicking character-range slice. Inverted or out-of-range
    // bounds return Err, not a truncated string. Registered under
    // both the qualified and bare names so `String::slice(s, a, b)?`
    // and `s.slice(a, b)?` both dispatch here.
    globals.push(("String::slice", builtin("String::slice", builtin_str_slice)));
    globals.push(("Vec::slice", builtin("Vec::slice", builtin_vec_slice)));
    globals.push(("slice", builtin("slice", builtin_str_or_vec_slice)));
    // 0.7.0 Vec read helpers - the compiled tier exposes these as
    // methods on any Vec; keep the interpreter in lockstep. Each is
    // registered under both the bare name (free-fn form `first(xs)`) and
    // the `Vec::` qualified key so a method call on a Vec/slice receiver
    // resolves to the builtin even when a user free function of the same
    // name (`fn first(xs)`) is in scope - the qualified key dominates the
    // bare-name fallback in `Op::MethodCall` dispatch.
    globals.push(("first", builtin("first", builtin_first)));
    globals.push(("Vec::first", builtin("Vec::first", builtin_first)));
    globals.push(("last", builtin("last", builtin_last)));
    globals.push(("Vec::last", builtin("Vec::last", builtin_last)));
    globals.push(("get", builtin("get", builtin_get_router)));
    globals.push(("Vec::get", builtin("Vec::get", builtin_get)));
    globals.push(("rev", builtin("rev", builtin_reversed)));
    globals.push(("Vec::rev", builtin("Vec::rev", builtin_reversed)));
    globals.push(("index_of", builtin("index_of", builtin_index_of)));
    globals.push(("Vec::index_of", builtin("Vec::index_of", builtin_index_of)));
    globals.push(("count_of", builtin("count_of", builtin_count_of)));
    globals.push(("Vec::count_of", builtin("Vec::count_of", builtin_count_of)));
    globals.push(("Vec::contains", builtin("Vec::contains", builtin_contains)));
    globals.push(("Vec::len", builtin("Vec::len", builtin_len)));
    globals.push(("Vec::push", builtin("Vec::push", builtin_push)));
    globals.push((
        "collections::Vec::push",
        builtin("collections::Vec::push", builtin_push),
    ));
    globals.push(("Vec::sort", builtin("Vec::sort", builtin_sort)));
    globals.push(("Vec::sort_by", native("Vec::sort_by", native_sort_by)));
    globals.push(("Vec::reverse", builtin("Vec::reverse", builtin_reverse)));
    globals.push(("Vec::swap", builtin("Vec::swap", builtin_swap)));
    // Legacy Vec fallback symbols remain installed for ABI compatibility.
    // Type-checked calls compile to the in-place Vec operations.
    globals.push((
        "Vec::insert",
        builtin("Vec::insert", builtin_vec_insert_safe),
    ));
    globals.push((
        "Vec::remove",
        builtin("Vec::remove", builtin_vec_remove_safe),
    ));
    globals.push((
        "collections::Vec::insert",
        builtin("collections::Vec::insert", builtin_vec_insert_safe),
    ));
    globals.push((
        "collections::Vec::remove",
        builtin("collections::Vec::remove", builtin_vec_remove_safe),
    ));
    globals.push(("Map::pop", builtin("Map::pop", builtin_map_pop)));
    globals.push((
        "collections::Map::pop",
        builtin("collections::Map::pop", builtin_map_pop),
    ));
    // `String::to_lowercase` / `String::to_uppercase` - Rust spellings
    // for the existing Unicode lowercase / uppercase shims.
    // Registered as qualified keys so `s.to_lowercase()` on a `String`
    // receiver dispatches here rather than to the char-level
    // `unicode::to_lower` shim (which would silently return the
    // first scalar only).
    globals.push((
        "String::to_lowercase",
        builtin("String::to_lowercase", builtin_to_lowercase),
    ));
    globals.push((
        "String::to_uppercase",
        builtin("String::to_uppercase", builtin_to_uppercase),
    ));
    // Fundamental String-building surface. `String::new` /
    // `String::with_capacity` are Path-call globals (no receiver);
    // `push` / `push_str` / `chars` dispatch through the `String::`
    // qualified key so a String receiver reaches the string op rather
    // than the bare Vec `push` (which would clobber the receiver with
    // its Unit return under the mutating-method writeback). `push` and
    // `push_str` return the new String so that writeback is idempotent.
    globals.push(("String::new", builtin("String::new", builtin_str_new)));
    globals.push((
        "String::with_capacity",
        builtin("String::with_capacity", builtin_str_with_capacity),
    ));
    globals.push(("String::from", builtin("String::from", builtin_str_from)));
    globals.push((
        "String::from_utf8",
        builtin("String::from_utf8", builtin_str_from_utf8),
    ));
    globals.push(("String::push", builtin("String::push", builtin_str_push)));
    globals.push((
        "String::push_char",
        builtin("String::push_char", builtin_str_push_char),
    ));
    globals.push((
        "String::push_byte",
        builtin("String::push_byte", builtin_str_push_byte),
    ));
    globals.push((
        "String::push_str",
        builtin("String::push_str", builtin_str_push_str),
    ));
    globals.push((
        "String::push_utf8",
        builtin("String::push_utf8", builtin_str_push_utf8),
    ));
    globals.push(("String::chars", builtin("String::chars", builtin_str_chars)));
    globals.push(("unwrap", builtin("unwrap", builtin_variant_unwrap)));
    // `expect(msg)` reads only the receiver: the compiled tiers route
    // both `unwrap` and `expect` to `gos_rt_result_unwrap` (the message
    // arg is discarded), so the VM matches that for tier parity.
    globals.push(("expect", builtin("expect", builtin_variant_unwrap)));
    globals.push(("unwrap_or", builtin("unwrap_or", builtin_variant_unwrap_or)));
    globals.push((
        "unwrap_or_else",
        native("unwrap_or_else", native_variant_unwrap_or_else),
    ));
    globals.push((
        "unwrap_or_default",
        builtin("unwrap_or_default", builtin_variant_unwrap_or_default),
    ));
    globals.push(("is_some", builtin("is_some", builtin_variant_is::<'S'>)));
    globals.push(("is_none", builtin("is_none", builtin_variant_is::<'N'>)));
    globals.push(("is_ok", builtin("is_ok", builtin_variant_is::<'O'>)));
    globals.push(("is_err", builtin("is_err", builtin_variant_is::<'E'>)));
    globals.push(("ok", builtin("ok", builtin_variant_ok)));
    globals.push(("err", builtin("err", builtin_variant_err)));
    globals.push(("ok_or", builtin("ok_or", builtin_variant_ok_or)));
    globals.push((
        "ok_or_else",
        native("ok_or_else", native_variant_ok_or_else),
    ));
    globals.push(("and_then", native("and_then", native_variant_and_then)));
    globals.push(("or_else", native("or_else", native_variant_or_else)));
    globals.push(("filter", native("filter", native_variant_filter)));
    globals.push(("map", native("map", native_variant_map)));
    globals.push(("map_or", native("map_or", native_variant_map_or)));
    globals.push(("map_err", native("map_err", native_variant_map_err)));
    globals.push(("parse", builtin("parse", builtin_str_parse_result)));
    globals.push(("errors::new", builtin("errors::new", builtin_errors_new)));
    globals.push((
        "errors::Error::from",
        builtin("errors::Error::from", builtin_errors_from),
    ));
    globals.push(("errors::wrap", builtin("errors::wrap", builtin_errors_wrap)));
    globals.push(("errors::join", builtin("errors::join", builtin_errors_join)));
    globals.push((
        "errors::is",
        builtin("errors::is", builtin_errors_is_freefn),
    ));
    globals.push((
        "errors::Error::message",
        builtin("errors::Error::message", builtin_errors_message),
    ));
    globals.push((
        "errors::Error::cause",
        builtin("errors::Error::cause", builtin_errors_cause),
    ));
    globals.push((
        "errors::Error::is",
        builtin("errors::Error::is", builtin_errors_is_method),
    ));
    globals.push((
        "errors::Error::chain",
        builtin("errors::Error::chain", builtin_errors_chain),
    ));
    globals.push((
        "errors::Error::with_field",
        builtin("errors::Error::with_field", builtin_errors_with_field),
    ));
    globals.push((
        "errors::Error::field",
        builtin("errors::Error::field", builtin_errors_field),
    ));
    globals.push((
        "errors::Error::fields",
        builtin("errors::Error::fields", builtin_errors_fields),
    ));
    globals.push(("message", builtin("message", builtin_errors_message)));
    globals.push(("cause", builtin("cause", builtin_errors_cause)));
    globals.push(("chain", builtin("chain", builtin_errors_chain)));
    globals.push((
        "with_field",
        builtin("with_field", builtin_errors_with_field),
    ));
    globals.push(("field", builtin("field", builtin_errors_field)));
    globals.push(("fields", builtin("fields", builtin_errors_fields)));
    globals.push(("to_vec", builtin("to_vec", builtin_clone)));
    globals.push((
        "std::sync::channel",
        builtin("std::sync::channel", builtin_channel_new),
    ));
}

fn native_variant_map_err(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let transform = args.get(1).cloned().unwrap_or(Value::Unit);
    if let Value::Variant(inner) = &receiver
        && inner.name == "Err"
        && !inner.fields.is_empty()
    {
        let mapped = dispatch.call_value(&transform, vec![inner.fields[0].clone()])?;
        return Ok(Value::variant("Err", vec![mapped]));
    }
    Ok(receiver)
}

/// An `errors::Error` value with `message` and no cause - the shape a
/// runtime-side failure (a cohort's child report) hands back to source.
pub(crate) fn make_error_value(message: &str) -> Value {
    errors_struct(message.to_string(), Value::variant("None", vec![]))
}

fn errors_struct(message: String, cause: Value) -> Value {
    errors_struct_with(message, cause, Value::Array(Arc::new(Vec::new())))
}

/// Builds an `errors::Error` value carrying structured diagnostic
/// fields; `fields` is a sequence of `(key, value)` string tuples in
/// insertion order.
fn errors_struct_with(message: String, cause: Value, fields: Value) -> Value {
    let slots = vec![
        ("message", Value::String(SmolStr::from(message))),
        ("cause", cause),
        ("__fields", fields),
    ];
    Value::struct_("errors::Error", slots)
}

/// The structured-field pairs attached to an error value.
fn errors_fields_of(v: &Value) -> Vec<(String, String)> {
    let Value::Struct(inner) = v else {
        return Vec::new();
    };
    if inner.name != "errors::Error" {
        return Vec::new();
    }
    let Some((_, Value::Array(items))) = inner.fields.iter().find(|(n, _)| **n == "__fields")
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::Tuple(pair) if pair.len() == 2 => {
                Some((pair[0].to_string(), pair[1].to_string()))
            }
            _ => None,
        })
        .collect()
}

fn errors_fields_value(pairs: &[(String, String)]) -> Value {
    Value::Array(Arc::new(
        pairs
            .iter()
            .map(|(k, v)| {
                Value::Tuple(Arc::from(vec![
                    Value::String(SmolStr::from(k.as_str())),
                    Value::String(SmolStr::from(v.as_str())),
                ]))
            })
            .collect(),
    ))
}

/// `errors::Error::chain() -> [errors::Error]` - this error followed by
/// every ancestor cause, outermost first.
fn builtin_errors_chain(args: &[Value]) -> RuntimeResult<Value> {
    let mut out = Vec::new();
    let mut cursor = args.first().cloned();
    while let Some(cur) = cursor {
        if errors_message_of(&cur).is_none() {
            break;
        }
        cursor = match errors_cause_of(&cur) {
            Some(Value::Variant(inner)) if inner.name == "Some" && !inner.fields.is_empty() => {
                Some(inner.fields[0].clone())
            }
            _ => None,
        };
        out.push(cur);
    }
    Ok(Value::Array(Arc::new(out)))
}

/// `errors::Error::with_field(key, value) -> errors::Error` - a copy of
/// the receiver carrying one more structured diagnostic field.
fn builtin_errors_with_field(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let key = args.get(1).map(Value::to_string).unwrap_or_default();
    let value = args.get(2).map(Value::to_string).unwrap_or_default();
    let mut pairs = errors_fields_of(&receiver);
    match pairs.iter_mut().find(|(name, _)| *name == key) {
        Some((_, current)) => *current = value,
        None => pairs.push((key, value)),
    }
    Ok(errors_struct_with(
        errors_message_of(&receiver).unwrap_or_default(),
        errors_cause_of(&receiver).unwrap_or_else(|| Value::variant("None", vec![])),
        errors_fields_value(&pairs),
    ))
}

/// `errors::Error::field(key) -> Option<String>`.
fn builtin_errors_field(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    let key = args.get(1).map(Value::to_string).unwrap_or_default();
    match errors_fields_of(&receiver)
        .into_iter()
        .find(|(name, _)| *name == key)
    {
        Some((_, value)) => Ok(Value::variant(
            "Some",
            vec![Value::String(SmolStr::from(value))],
        )),
        None => Ok(Value::variant("None", vec![])),
    }
}

/// `errors::Error::fields() -> [(String, String)]` in insertion order.
fn builtin_errors_fields(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    Ok(errors_fields_value(&errors_fields_of(&receiver)))
}

/// Identity of an error value: the shared payload address, which every
/// clone of the same error preserves.
fn error_identity(v: &Value) -> Option<usize> {
    match v {
        Value::Struct(inner) if inner.name == "errors::Error" => Some(Arc::as_ptr(inner) as usize),
        _ => None,
    }
}

/// Whether `sentinel` is `err` itself or any link of its cause chain.
fn errors_chain_has_sentinel(err: &Value, sentinel: &Value) -> bool {
    let Some(target) = error_identity(sentinel) else {
        return false;
    };
    let mut cursor = Some(err.clone());
    while let Some(cur) = cursor {
        if error_identity(&cur) == Some(target) {
            return true;
        }
        cursor = match errors_cause_of(&cur) {
            Some(Value::Variant(inner)) if inner.name == "Some" && !inner.fields.is_empty() => {
                Some(inner.fields[0].clone())
            }
            _ => None,
        };
    }
    false
}

fn errors_message_of(v: &Value) -> Option<String> {
    if let Value::Struct(inner) = v
        && inner.name == "errors::Error"
    {
        for (name, value) in &inner.fields {
            if *name == "message"
                && let Value::String(s) = value
            {
                return Some(s.as_str().to_string());
            }
        }
    }
    None
}

fn errors_cause_of(v: &Value) -> Option<Value> {
    if let Value::Struct(inner) = v
        && inner.name == "errors::Error"
    {
        for (name, value) in &inner.fields {
            if *name == "cause" {
                return Some(value.clone());
            }
        }
    }
    None
}

fn builtin_errors_new(args: &[Value]) -> RuntimeResult<Value> {
    let msg = match args.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    };
    Ok(errors_struct(msg, Value::variant("None", vec![])))
}

/// Canonical `Into<errors::Error>` conversion. Routes through
/// the SPEC §4.5 `?`-propagation auto-conversion: when the
/// inner expression's `Err` type differs from the enclosing
/// function's, the HIR desugar calls this helper to coerce the
/// value into the enclosing fn's error type. Identity for
/// `errors::Error`; wraps a String into a fresh `errors::Error`
/// via `errors::new`; falls back to `format!("{:?}", v)` for
/// anything else.
fn builtin_errors_from(args: &[Value]) -> RuntimeResult<Value> {
    let Some(value) = args.first() else {
        return Ok(errors_struct(String::new(), Value::variant("None", vec![])));
    };
    match value {
        Value::Struct(inner) if inner.name == "errors::Error" => Ok(value.clone()),
        Value::String(s) => Ok(errors_struct(
            s.as_str().to_string(),
            Value::variant("None", vec![]),
        )),
        other => Ok(errors_struct(
            format!("{other:?}"),
            Value::variant("None", vec![]),
        )),
    }
}

fn builtin_errors_join(args: &[Value]) -> RuntimeResult<Value> {
    let errs = match args.first() {
        Some(Value::Array(arr)) => arr.as_ref().clone(),
        _ => vec![],
    };
    let messages: Vec<String> = errs.iter().filter_map(errors_message_of).collect();
    if messages.is_empty() {
        return Ok(Value::variant("None", vec![]));
    }
    let combined = messages.join("; ");
    let err = errors_struct(combined, Value::variant("None", vec![]));
    Ok(Value::variant("Some", vec![err]))
}

fn builtin_errors_wrap(args: &[Value]) -> RuntimeResult<Value> {
    let cause = args.first().cloned().unwrap_or(Value::Unit);
    let msg = match args.get(1) {
        Some(Value::String(s)) => s.as_str().to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    };
    let cause_some = Value::variant("Some", vec![cause]);
    Ok(errors_struct(msg, cause_some))
}

fn builtin_errors_message(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    Ok(Value::String(SmolStr::from(
        errors_message_of(&receiver).unwrap_or_default(),
    )))
}

fn builtin_errors_cause(args: &[Value]) -> RuntimeResult<Value> {
    let receiver = args.first().cloned().unwrap_or(Value::Unit);
    Ok(errors_cause_of(&receiver).unwrap_or_else(|| Value::variant("None", vec![])))
}

fn errors_chain_contains(err: &Value, needle: &str) -> bool {
    let mut cursor = Some(err.clone());
    while let Some(cur) = cursor {
        match errors_message_of(&cur) {
            Some(m) if m.contains(needle) => return true,
            Some(_) => {}
            None => return false,
        }
        cursor = match errors_cause_of(&cur) {
            Some(Value::Variant(inner)) if inner.name == "Some" && !inner.fields.is_empty() => {
                Some(inner.fields[0].clone())
            }
            _ => None,
        };
    }
    false
}

/// `errors::is(err, needle)` - `needle` is either a message (substring
/// match down the cause chain, the Go `errors.Is` string fallback) or a
/// sentinel error value (identity match).
fn errors_is(args: &[Value]) -> Value {
    let err = args.first().cloned().unwrap_or(Value::Unit);
    match args.get(1) {
        Some(Value::String(needle)) => Value::Bool(errors_chain_contains(&err, needle.as_str())),
        Some(sentinel) => Value::Bool(errors_chain_has_sentinel(&err, sentinel)),
        None => Value::Bool(false),
    }
}

fn builtin_errors_is_method(args: &[Value]) -> RuntimeResult<Value> {
    Ok(errors_is(args))
}

fn builtin_errors_is_freefn(args: &[Value]) -> RuntimeResult<Value> {
    Ok(errors_is(args))
}

fn builtin_str_parse_result(args: &[Value]) -> RuntimeResult<Value> {
    let s = match args.first() {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => {
            return Ok(Value::variant(
                "Err",
                vec![errors_struct(
                    "parse: not a string".to_string(),
                    Value::variant("None", vec![]),
                )],
            ));
        }
    };
    if let Ok(n) = s.trim().parse::<i64>() {
        return Ok(Value::variant("Ok", vec![Value::Int(n)]));
    }
    let msg = format!(
        "unexpected byte 0x{:x} at 1:1",
        s.as_bytes().first().copied().unwrap_or(0)
    );
    let err = errors_struct(msg, Value::variant("None", vec![]));
    Ok(Value::variant("Err", vec![err]))
}

// Pure registration list - splitting it would obscure the
// concurrency surface area without making any function shorter.
#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn install_concurrency_builtins(globals: &mut Vec<(&'static str, Value)>) {
    globals.push(("spawn", native("spawn", native_spawn)));
    globals.push(("channel", builtin("channel", builtin_channel_new)));
    globals.push(("channel::new", builtin("channel::new", builtin_channel_new)));
    globals.push((
        "channel::unbounded",
        builtin("channel::unbounded", builtin_channel_unbounded),
    ));
    globals.push((
        "sync::channel",
        builtin("sync::channel", builtin_channel_new),
    ));
    globals.push((
        "sync::channel_unbounded",
        builtin("sync::channel_unbounded", builtin_channel_unbounded),
    ));
    globals.push((
        "std::sync::channel_unbounded",
        builtin("std::sync::channel_unbounded", builtin_channel_unbounded),
    ));
    globals.push((
        "Channel::send",
        builtin("Channel::send", builtin_channel_send),
    ));
    globals.push((
        "Channel::recv",
        builtin("Channel::recv", builtin_channel_recv),
    ));
    // `spawn(f)` hands back a one-shot channel as its join handle;
    // `.join()` on that Channel blocks for the outcome variant.
    globals.push((
        "Channel::join",
        builtin("Channel::join", builtin_channel_join),
    ));
    globals.push((
        "Channel::try_recv",
        builtin("Channel::try_recv", builtin_channel_try_recv),
    ));
    // MIR-emitted runtime call names - VM intercepts these so the
    // interpreter's channel impl is used instead of the native GosChan.
    globals.push((
        "gos_rt_chan_recv_option",
        builtin("gos_rt_chan_recv_option", builtin_channel_recv),
    ));
    globals.push((
        "gos_rt_chan_try_recv_option",
        builtin("gos_rt_chan_try_recv_option", builtin_channel_try_recv),
    ));
    // `rx.recv_ctx(&ctx)` checks the VM Context registry between bounded
    // channel waits, so the interpreter observes cancellation without sharing
    // the native runtime's GosChan condvar.
    globals.push((
        "Channel::recv_ctx",
        builtin("Channel::recv_ctx", builtin_channel_recv_ctx),
    ));
    globals.push((
        "gos_rt_chan_recv_ctx_option",
        builtin("gos_rt_chan_recv_ctx_option", builtin_channel_recv_ctx),
    ));
    globals.push((
        "Channel::close",
        builtin("Channel::close", builtin_channel_close),
    ));
    globals.push(("close", builtin("close", builtin_channel_close)));
    globals.push((
        "sync::Channel::new",
        builtin("sync::Channel::new", builtin_channel_new),
    ));

    // Shared atomic-i64 buffer used by goroutine fan-out programs
    // (`fasta.gos`'s multi-threaded variant). Backed by a global
    // side table keyed on a u32 handle stuffed into the
    // `I64Vec.__handle` struct field.
    globals.push(("I64Vec::new", builtin("I64Vec::new", builtin_i64vec_new)));
    globals.push((
        "I64Vec::set_at",
        builtin("I64Vec::set_at", builtin_i64vec_set_at),
    ));
    globals.push((
        "I64Vec::get_at",
        builtin("I64Vec::get_at", builtin_i64vec_get_at),
    ));
    globals.push((
        "I64Vec::vec_len",
        builtin("I64Vec::vec_len", builtin_i64vec_vec_len),
    ));
    globals.push((
        "I64Vec::write_range_to_stdout",
        builtin(
            "I64Vec::write_range_to_stdout",
            builtin_i64vec_write_range_to_stdout,
        ),
    ));
    globals.push((
        "I64Vec::write_lines_to_stdout",
        builtin(
            "I64Vec::write_lines_to_stdout",
            builtin_i64vec_write_lines_to_stdout,
        ),
    ));

    // `Vec::new()` produces an empty growable array. Without
    // this entry the `Vec::new` path lookup misses, falls back
    // to the bare `new` global, and resolves to whichever
    // module's `new` was installed last - typically `HashMap`'s,
    // which means `let mut v: Vec<i64> = Vec::new(); v.push(1)`
    // silently builds an empty `HashMap` and the push is a no-op.
    globals.push(("Vec::new", builtin("Vec::new", builtin_vec_new)));
    globals.push(("Vec::from", builtin("Vec::from", builtin_vec_from)));
    globals.push((
        "collections::Vec::from",
        builtin("collections::Vec::from", builtin_vec_from),
    ));
    // `Vec::with_capacity(n)` produces the same empty growable array as
    // `Vec::new()`; the count is a preallocation hint the VM's dynamically
    // grown array needs no separate reservation for. Registered so the
    // surface resolves identically on the VM to the compiled tiers (which
    // reserve `n` up front via `gos_rt_vec_with_capacity`). Both the bare
    // and `collections::`-qualified paths are covered, matching `Vec::new`.
    globals.push((
        "Vec::with_capacity",
        builtin("Vec::with_capacity", builtin_vec_with_capacity),
    ));
    globals.push((
        "collections::Vec::with_capacity",
        builtin("collections::Vec::with_capacity", builtin_vec_with_capacity),
    ));

    // U8Vec: 1-byte-per-element heap vec. Same shape as I64Vec
    // but with byte-aligned storage - fasta-style scratch
    // buffers no longer pay the 8x storage tax.
    globals.push(("U8Vec::new", builtin("U8Vec::new", builtin_u8vec_new)));
    globals.push((
        "U8Vec::set_byte",
        builtin("U8Vec::set_byte", builtin_u8vec_set_byte),
    ));
    globals.push((
        "U8Vec::get_byte",
        builtin("U8Vec::get_byte", builtin_u8vec_get_byte),
    ));
    globals.push((
        "U8Vec::byte_len",
        builtin("U8Vec::byte_len", builtin_u8vec_byte_len),
    ));
    globals.push((
        "U8Vec::to_string",
        builtin("U8Vec::to_string", builtin_u8vec_to_string),
    ));
    globals.push((
        "U8Vec::write_byte_range_to_stdout",
        builtin(
            "U8Vec::write_byte_range_to_stdout",
            builtin_u8vec_write_byte_range_to_stdout,
        ),
    ));
    globals.push((
        "U8Vec::write_byte_lines_to_stdout",
        builtin(
            "U8Vec::write_byte_lines_to_stdout",
            builtin_u8vec_write_byte_lines_to_stdout,
        ),
    ));
    // Sliding-window pack: read `k` bytes from `i` and pack
    // them into a single i64 by `(key << 2) | byte`. Single
    // C-side loop replaces what was a k-iter bytecode loop in
    // user code; sliding-window scans ride this op directly.
    // Also exposed via the bare-name dispatch path
    // (`buf.window_key(i, k)`) and as a method receiver.
    globals.push((
        "U8Vec::window_key",
        builtin("U8Vec::window_key", builtin_u8vec_window_key),
    ));
    globals.push((
        "window_key",
        builtin("window_key", builtin_u8vec_window_key),
    ));
    // Whole-program k-mer count: scan the entire buffer and
    // emit a `Value::IntMap` of (packed_kmer_key -> count).
    // Replaces the user-side `while i < stop { … insert … }`
    // loop with a single C-side call for sliding-window
    // counter scans.
    globals.push((
        "U8Vec::count_kmers",
        builtin("U8Vec::count_kmers", builtin_u8vec_count_kmers),
    ));
    globals.push((
        "count_kmers",
        builtin("count_kmers", builtin_u8vec_count_kmers),
    ));
    // Whole-program 4-bucket / 16-bucket frequency scans for
    // small-alphabet single- and pair-base counts. Returns a flat
    // `Value::IntArray` so the caller can index it directly
    // (the existing print-freq helpers already accept a
    // `[i64; N]`-shaped receiver).
    globals.push((
        "U8Vec::count_singles",
        builtin("U8Vec::count_singles", builtin_u8vec_count_singles),
    ));
    globals.push((
        "count_singles",
        builtin("count_singles", builtin_u8vec_count_singles),
    ));
    globals.push((
        "U8Vec::count_pairs",
        builtin("U8Vec::count_pairs", builtin_u8vec_count_pairs),
    ));
    globals.push((
        "count_pairs",
        builtin("count_pairs", builtin_u8vec_count_pairs),
    ));

    // `sync::WaitGroup` mirroring Go's API. The constructor is bound
    // under both spellings like its `sync::` siblings (Mutex, Barrier);
    // the compiled tiers already accept the qualified form.
    globals.push((
        "WaitGroup::new",
        builtin("WaitGroup::new", builtin_waitgroup_new),
    ));
    globals.push((
        "sync::WaitGroup::new",
        builtin("sync::WaitGroup::new", builtin_waitgroup_new),
    ));
    globals.push((
        "WaitGroup::add",
        builtin("WaitGroup::add", builtin_waitgroup_add),
    ));
    globals.push((
        "WaitGroup::done",
        builtin("WaitGroup::done", builtin_waitgroup_done),
    ));
    globals.push((
        "WaitGroup::wait",
        builtin("WaitGroup::wait", builtin_waitgroup_wait),
    ));
    globals.push((
        "WaitGroup::wait_ctx",
        builtin("WaitGroup::wait_ctx", builtin_waitgroup_wait_ctx),
    ));

    // O(log n) Lehmer LCG affine-transform jump-ahead.
    globals.push(("lcg_jump", builtin("lcg_jump", builtin_lcg_jump)));
    globals.push((
        "gos_rt_lcg_jump",
        builtin("gos_rt_lcg_jump", builtin_lcg_jump),
    ));

    // Bulk byte-array writer used by `out.write_byte_array(&line, n)`
    // in the `fasta` block-write hot path.
    globals.push((
        "Stream::write_byte_array",
        builtin("Stream::write_byte_array", builtin_stream_write_byte_array),
    ));
    globals.push((
        "write_byte_array",
        builtin("write_byte_array", builtin_stream_write_byte_array),
    ));
}

fn install_regex_builtins(globals: &mut Vec<(&'static str, Value)>) {
    // Register regex helpers only under their qualified key plus
    // a `regex::Pattern::*` shape (used by qualified-method
    // dispatch on `Value::Struct` regex handles). The bare names
    // (`split`, `find`, `replace`, …) collide with the string
    // method-call dispatch - bare-registering would route a
    // `s.split(" ")` call to the regex helper, which would then
    // bail with "expected Pattern handle".
    let qualified_only = [
        "compile",
        "is_match",
        "find",
        "find_all",
        "count",
        "captures",
        "captures_all",
        "replace",
        "replace_all",
        "split",
    ];
    for (short, call) in crate::regex_builtins::ENTRIES {
        let joined: &'static str = Box::leak(format!("regex::{short}").into_boxed_str());
        globals.push((joined, builtin(joined, *call)));
        let pattern_key: &'static str =
            Box::leak(format!("regex::Pattern::{short}").into_boxed_str());
        globals.push((pattern_key, builtin(pattern_key, *call)));
        if !qualified_only.contains(short) {
            globals.push((*short, builtin(short, *call)));
        }
    }
}
