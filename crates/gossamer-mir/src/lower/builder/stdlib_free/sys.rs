//! Lowering `fs`, `os`, `path`, `io`/`net`, `env`/`thread`, `exec`, and `signal`/`flag` free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_fs_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "fs::sync_dir" => {
                let unit = self.tcx.unit();
                ("gos_rt_fs_sync_dir", self.result_of(unit))
            }
            // `fs::read_to_string(path) -> Result<String, errors::Error>`.
            // Routes to the Result-shaped shim (not the bare-string
            // `gos_rt_fs_read_to_string`, which returns "" on failure) so a
            // missing / unreadable path propagates `Err` like `fs::read` and
            // the VM, not a silent `Ok("")`.
            "fs::read_to_string" => {
                let s = self.tcx.string_ty();
                let e = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([s, e]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_read_to_string_result", result_ty)
            }
            "fs::write" => {
                // Pick the bytes-shaped variant when the contents
                // argument is a Vec<u8> / &[u8] - the c-string-shaped
                // helper would truncate at the first NUL and corrupt
                // binary payloads (image writes, gzip bodies, etc.).
                // The typechecker often leaves `&local_vec`-shaped
                // args as `Ref<Var(_)>`, so we walk through the `&`
                // operator and consult `peek_collection_type`, which
                // recovers the actual MIR-pinned local type.
                let bytes_shaped = args.get(1).is_some_and(|a| {
                    use gossamer_types::{IntTy, TyKind};
                    if is_vec_u8_arg(self.tcx, a) {
                        return true;
                    }
                    let inner_expr = if let HirExprKind::Unary { op, operand } = &a.kind {
                        if matches!(op, HirUnaryOp::RefShared | HirUnaryOp::RefMut) {
                            operand.as_ref()
                        } else {
                            a
                        }
                    } else {
                        a
                    };
                    let probe = self
                        .peek_collection_type(inner_expr)
                        .or(Some(inner_expr.ty));
                    probe.is_some_and(|t| {
                        let mut walk = t;
                        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(walk) {
                            walk = *inner;
                        }
                        let elem = match self.tcx.kind_of(walk) {
                            TyKind::Vec(e) | TyKind::Slice(e) => *e,
                            _ => return false,
                        };
                        matches!(self.tcx.kind_of(elem), TyKind::Int(IntTy::U8))
                    })
                });
                let sym = if bytes_shaped {
                    "gos_rt_os_write_file_bytes_result"
                } else {
                    "gos_rt_os_write_file_result"
                };
                (sym, self.result_unit_error_adt_ty())
            }
            "fs::create_dir" => ("gos_rt_fs_create_dir", self.result_unit_error_adt_ty()),
            "fs::create_dir_mode" => ("gos_rt_fs_create_dir_mode", self.result_unit_error_adt_ty()),
            "fs::create_dir_all_mode" => (
                "gos_rt_fs_create_dir_all_mode",
                self.result_unit_error_adt_ty(),
            ),
            "fs::write_mode" => ("gos_rt_fs_write_mode", self.result_unit_error_adt_ty()),
            "fs::set_permissions" => ("gos_rt_fs_set_permissions", self.result_unit_error_adt_ty()),
            "fs::permissions" => {
                let mode = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let error = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([mode, error]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_permissions", result_ty)
            }
            "fs::create_dir_all" => (
                "gos_rt_os_mkdir_all_result",
                self.result_unit_error_adt_ty(),
            ),
            "fs::remove_file" => (
                "gos_rt_os_remove_file_result",
                self.result_unit_error_adt_ty(),
            ),
            // Non-recursive empty-directory removal, matching the interp.
            "fs::remove_dir" => ("gos_rt_fs_remove_dir", self.result_unit_error_adt_ty()),
            "fs::remove_dir_all" => (
                "gos_rt_os_remove_dir_all_result",
                self.result_unit_error_adt_ty(),
            ),
            "fs::temp_dir" => ("gos_rt_fs_temp_dir", self.result_string_error_adt_ty()),
            "fs::temp_file" => {
                let file = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let path = self.tcx.string_ty();
                let pair = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![file, path]));
                ("gos_rt_fs_temp_file", self.result_of(pair))
            }
            _ => return None,
        })
    }

    pub(super) fn lower_os_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "fs::read" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                let e = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([v, e]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_read_bytes_result", result_ty)
            }
            // 0.10.0 - os/fs copy + canonicalize, crypto::subtle.
            "fs::copy" => ("gos_rt_fs_copy", self.result_i64_error_adt_ty()),
            "fs::canonicalize" => ("gos_rt_fs_canonicalize", self.result_string_error_adt_ty()),
            // `os::arch()` / `os::family()` - target introspection.
            "os::arch" => ("gos_rt_os_arch", self.tcx.string_ty()),
            "os::family" => ("gos_rt_os_family", self.tcx.string_ty()),
            // `fs::rename(from, to)` -> Result<(), Error>.
            "fs::rename" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_fs_rename", result_ty)
            }
            "os::program_name" | "env::program_name" => {
                ("gos_rt_os_program_name", self.tcx.string_ty())
            }
            "env::set_current_dir" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_env_set_current_dir", result_ty)
            }
            "env::var" => ("gos_rt_os_env", self.option_string_adt_ty()),
            "fs::exists" => ("gos_rt_os_exists", self.tcx.bool_ty()),
            "fs::is_file" => ("gos_rt_os_is_file", self.tcx.bool_ty()),
            "fs::is_dir" => ("gos_rt_os_is_dir", self.tcx.bool_ty()),
            "fs::is_symlink" => ("gos_rt_os_is_symlink", self.tcx.bool_ty()),
            "fs::file_size" => (
                "gos_rt_os_file_size",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "env::current_dir" => ("gos_rt_os_cwd", self.result_string_error_adt_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_os_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `env::args() -> Vec<String>`. Pinning the dest type
            // here is what teaches `args[i].len()` to dispatch
            // through `gos_rt_str_len` instead of the generic
            // `gos_rt_arr_len`. Single-file builds got
            // `Vec<String>` for free from typeck, but cross-module
            // compilation (e.g. askq, where `cli.gos` references
            // `args` and sibling modules also exist) leaves the
            // call's HIR type as a `Var(_)` and the cranelift
            // dispatch then crashes inside `gos_rt_arr_len`
            // reading a Vec header out of a `*const c_char`
            // string pointer. The runtime now hands back a real
            // `*mut GosVec` whose data pointer is `argv + 1`, so
            // index access through the standard `header.ptr + i *
            // elem_bytes` shape Just Works.
            "env::args" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_os_args", v)
            }
            // `env::set_var(name, value) -> Result<(), errors::Error>`.
            // Pin the Ok payload to unit and the Err to
            // `errors::Error` so callers' `?` shapes find the
            // right field layout. Without this binding the
            // compiled tier silently no-op'd `set_env` because
            // the generic free-call dispatch couldn't resolve
            // the symbol, and downstream `env::var` reads
            // returned the old value.
            "env::set_var" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_os_set_env", result_ty)
            }
            "env::unset_var" => ("gos_rt_os_unset_env", self.tcx.unit()),
            _ => return None,
        })
    }

    pub(super) fn lower_path_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "path::join" => ("gos_rt_path_join", self.tcx.string_ty()),
            "path::split" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                ("gos_rt_path_split", tup)
            }
            "path::components" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_components", v)
            }
            "path::prefixes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_prefixes", v)
            }
            "path::unique_prefixes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_path_unique_prefixes", v)
            }
            "path::normalize" => ("gos_rt_path_clean", self.tcx.string_ty()),
            "path::is_absolute" => ("gos_rt_path_is_absolute", self.tcx.bool_ty()),
            "path::starts_with" => ("gos_rt_path_has_prefix", self.tcx.bool_ty()),
            "path::extension" => ("gos_rt_path_ext", self.option_string_adt_ty()),
            // 0.10.0 - path Option-returning free fns. Each wraps
            // the matching `gos_rt_path_*_opt` helper which packs a
            // `*mut GosResult` (disc=0 Some(String), disc=1 None).
            "path::glob" => ("gos_rt_path_glob", self.result_vec_string_error_ty()),
            "path::matches" => ("gos_rt_path_matches", self.tcx.bool_ty()),
            "path::parent" => ("gos_rt_path_parent", self.option_string_adt_ty()),
            "path::file_stem" => ("gos_rt_path_stem", self.option_string_adt_ty()),
            "path::file_name" => ("gos_rt_path_file_name", self.option_string_adt_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_io_net_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "bufio::read_to_string" => (
                "gos_rt_bufio_read_to_string",
                self.result_string_error_adt_ty(),
            ),
            "bufio::read_lines_of" | "bufio::read_lines" => (
                "gos_rt_bufio_read_lines_of",
                self.result_vec_string_error_ty(),
            ),
            "bufio::split_whitespace" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_split_whitespace",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            // io::Copy(dst, src) / io::ReadAll(reader) - Go-shaped
            // stream helpers over the fd-tagged `*GosStream` handles.
            "io::Copy" => (
                "gos_rt_io_copy",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "io::ReadAll" => ("gos_rt_io_read_all", self.result_string_error_adt_ty()),
            // Handle-based stream adapters: a Reader / Writer is an i64
            // registry id, so the whole family composes as plain scalars
            // across the C-ABI.
            "io::string_reader" | "io::buffer_writer" | "io::limit_reader" | "io::tee_reader"
            | "io::multi_reader" | "io::write" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let sym = match joined {
                    "io::string_reader" => "gos_rt_io_string_reader",
                    "io::buffer_writer" => "gos_rt_io_buffer_writer",
                    "io::limit_reader" => "gos_rt_io_limit_reader",
                    "io::tee_reader" => "gos_rt_io_tee_reader",
                    "io::multi_reader" => "gos_rt_io_multi_reader",
                    _ => "gos_rt_io_write_str",
                };
                (sym, i64_ty)
            }
            "io::pipe" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![i64_ty, i64_ty]));
                ("gos_rt_io_pipe", tup)
            }
            "io::copy_n" => ("gos_rt_io_copy_n", self.result_i64_error_adt_ty()),
            "io::drain" => ("gos_rt_io_drain", self.tcx.string_ty()),
            "io::contents" => ("gos_rt_io_contents", self.tcx.string_ty()),
            "io::close_writer" => ("gos_rt_io_close_writer", self.tcx.unit()),
            "net::lookup" => ("gos_rt_net_resolve", self.result_vec_string_error_ty()),
            "fs::open" | "fs::File::open" => {
                ("gos_rt_fs_file_open", self.result_i64_error_adt_ty())
            }
            "fs::create" | "fs::File::create" => {
                ("gos_rt_fs_file_create", self.result_i64_error_adt_ty())
            }
            "fs::OpenOptions::new" | "OpenOptions::new" => (
                "gos_rt_fs_open_options_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "net::ip::is_valid" => ("gos_rt_netip_is_valid", self.tcx.bool_ty()),
            "net::ip::is_v4" => ("gos_rt_netip_is_v4", self.tcx.bool_ty()),
            "net::ip::is_v6" => ("gos_rt_netip_is_v6", self.tcx.bool_ty()),
            "net::ip::is_loopback" => ("gos_rt_netip_is_loopback", self.tcx.bool_ty()),
            "net::ip::is_private" => ("gos_rt_netip_is_private", self.tcx.bool_ty()),
            "net::ip::is_multicast" => ("gos_rt_netip_is_multicast", self.tcx.bool_ty()),
            "net::ip::is_unspecified" => ("gos_rt_netip_is_unspecified", self.tcx.bool_ty()),
            "net::ip::to_string" => ("gos_rt_netip_normalize", self.tcx.string_ty()),
            "net::ip::parse" => ("gos_rt_net_ip_parse", self.result_string_error_adt_ty()),
            "net::ip::octets" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_net_ip_octets",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "net::TcpListener::bind" => {
                ("gos_rt_tcp_listener_bind", self.result_i64_error_adt_ty())
            }
            "net::TcpStream::connect" => {
                ("gos_rt_tcp_stream_connect", self.result_i64_error_adt_ty())
            }
            "net::UnixListener::bind" => {
                ("gos_rt_unix_listener_bind", self.result_i64_error_adt_ty())
            }
            "net::UnixStream::connect" => {
                ("gos_rt_unix_stream_connect", self.result_i64_error_adt_ty())
            }
            "net::UdpSocket::bind" => ("gos_rt_udp_bind", self.result_i64_error_adt_ty()),
            "bufio::Scanner::new" | "Scanner::new" => (
                "gos_rt_bufio_scanner_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "bufio::Scanner::next" | "Scanner::next" => {
                ("gos_rt_bufio_scanner_text", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    pub(super) fn lower_env_thread_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - env aliases (the os:: spelling is already wired
            // above; the env:: spelling matches `use std::env`).
            "env::set_var" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_os_set_env", result_ty)
            }
            "env::unset_var" => ("gos_rt_os_unset_env", self.tcx.unit()),
            "env::set_current_dir" => {
                let unit_ty = self.tcx.unit();
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([unit_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_env_set_current_dir", result_ty)
            }
            // `thread::yield_now()` - goroutine-aware yield (Gosched).
            "thread::yield_now" => ("gos_rt_go_yield", self.tcx.unit()),
            "thread::num_cpus" => (
                "gos_rt_thread_num_cpus",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "env::temp_dir" => ("gos_rt_env_temp_dir", self.tcx.string_ty()),
            "env::home_dir" => ("gos_rt_env_home_dir", self.option_string_adt_ty()),
            "env::vars" => {
                let string_ty = self.tcx.string_ty();
                let map_ty = self.tcx.intern(gossamer_types::TyKind::HashMap {
                    key: string_ty,
                    value: string_ty,
                    ordered: false,
                });
                ("gos_rt_env_vars", map_ty)
            }
            _ => return None,
        })
    }

    /// `Result<i64, errors::Error>`, the sentinel-def shape the runtime's
    /// Result aggregate takes.
    fn result_i64_error_ty(&mut self) -> gossamer_types::Ty {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let err_ty = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([i64_ty, err_ty]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(super) fn lower_exec_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `exec::spawn(prog, args) -> Result<i64, errors::Error>`.
            // Non-blocking process launch - returns the child PID
            // so callers (daemon launchers, long-running tools)
            // don't block the calling goroutine. Pin the Ok
            // payload to `i64` and the Err to `errors::Error` so
            // downstream `?` / `match` shapes find the right field
            // layout.
            "exec::spawn" | "os::exec::spawn" | "process::spawn" => {
                ("gos_rt_exec_spawn", self.result_i64_error_ty())
            }
            // `process::run_inherit(prog, args) -> Result<i64, errors::Error>`:
            // the child's exit code, run on this process's own stdio.
            "process::run_inherit" => ("gos_rt_exec_run_inherit", self.result_i64_error_ty()),
            // `exec::kill(pid) -> bool` - best-effort SIGTERM.
            "exec::kill" | "os::exec::kill" | "process::kill" => {
                ("gos_rt_exec_kill", self.tcx.bool_ty())
            }
            // `exec::signal(pid, signum) -> bool`.
            "exec::signal" | "os::exec::signal" | "process::signal" => {
                ("gos_rt_exec_signal", self.tcx.bool_ty())
            }
            // `exec::kill_group(pid) -> bool` - kills the entire
            // process group on Unix; best-effort on Windows.
            "exec::kill_group" | "os::exec::kill_group" | "process::kill_group" => {
                ("gos_rt_exec_kill_group", self.tcx.bool_ty())
            }
            // `exec::wait_timeout(pid, ms) -> i64`. Returns the
            // child's exit code on success, -1 on timeout, -2 on
            // error (unknown pid, permission denied).
            "exec::wait_timeout" | "os::exec::wait_timeout" | "process::wait_timeout" => (
                "gos_rt_exec_wait_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_signal_flag_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `signal::on(sig_raw) -> i64` - registers a notifier.
            "signal::on" | "os::signal::on" => (
                "gos_rt_signal_on",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `Notifier::wait(handle)` - blocks until signal fires.
            "signal_wait" | "Notifier::wait" | "signal::wait" | "os::signal::wait" => {
                ("gos_rt_signal_wait", self.tcx.bool_ty())
            }
            // `Notifier::try_wait(handle) -> bool`.
            "signal_try_wait"
            | "Notifier::try_wait"
            | "signal::try_wait"
            | "os::signal::try_wait" => ("gos_rt_signal_try_wait", self.tcx.bool_ty()),
            "signal_stop" | "Notifier::stop" | "signal::stop" | "os::signal::stop" => {
                ("gos_rt_signal_stop", self.tcx.unit())
            }
            "flag::Set::new" => ("gos_rt_flag_set_new", self.flag_set_ty()),
            _ => return None,
        })
    }
}
