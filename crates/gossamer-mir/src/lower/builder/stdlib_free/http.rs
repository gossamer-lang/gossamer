//! Lowering `http` free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_http_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `http::get(url, headers) -> Result<Response, errors::Error>`.
            // Pin the Ok payload to the sentinel-DefId Response Adt
            // so `r.status` / `r.body` / `r.content_type` /
            // `r.location` projections find the right field index
            // via `stdlib_struct_shapes`.
            "http::get" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_http_get", result_ty)
            }
            // One-shot client verbs sharing `http::get`'s Ok-payload
            // pinning. `head`/`options` take `(url, headers)`;
            // `post`/`put` take `(url, body, content_type)`; `delete`
            // takes `(url, body, headers)`. Each lowers to its
            // per-verb shim so the method string is fixed at the
            // runtime boundary.
            "http::head" | "http::options" | "http::post" | "http::put" | "http::delete" => {
                let result_ty = self.result_response_error_adt_ty();
                let sym = match joined {
                    "http::head" => "gos_rt_http_head",
                    "http::options" => "gos_rt_http_options",
                    "http::post" => "gos_rt_http_post",
                    "http::put" => "gos_rt_http_put",
                    _ => "gos_rt_http_delete",
                };
                (sym, result_ty)
            }
            // Bare `NativeClient` one-shot helpers. `get`/`delete` take
            // just the URL; `post`/`put` take `(url, body, content_type)`
            // (empty content type defaults to application/octet-stream in
            // the shim). Each pins the Response Ok payload like `http::get`.
            "http::native_client::get" | "native_client::get" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_get", result_ty)
            }
            "http::native_client::delete" | "native_client::delete" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_delete", result_ty)
            }
            "http::native_client::post" | "native_client::post" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_post", result_ty)
            }
            "http::native_client::put" | "native_client::put" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_nc_put", result_ty)
            }
            // `proxy::forward(upstream_url, method, body)` one-shot
            // upstream request; `static_files::serve_file(path)` one-shot
            // file read. Both return Result<Response, errors::Error>.
            "http::proxy::forward" | "proxy::forward" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_proxy_forward_url", result_ty)
            }
            "http::static_files::serve_file" | "static_files::serve_file" => {
                let result_ty = self.result_response_error_adt_ty();
                ("gos_rt_static_serve_file", result_ty)
            }
            // `router::add(router, method, pattern)` registers a
            // handler-less pattern route; `router::lookup(router, method,
            // path) -> Option<i64>` returns the matched route index.
            "http::router::add" | "router::add" => ("gos_rt_router_add_pattern", self.tcx.unit()),
            "http::router::lookup" | "router::lookup" => {
                ("gos_rt_router_lookup", self.option_i64_adt_ty())
            }
            _ => return None,
        })
    }

    pub(super) fn lower_http_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // `http::request(method, url, body, headers)` and
            // `http::request_bytes(method, url, body: [u8], headers)`
            // -> Result<Response, errors::Error>. Same Ok-payload
            // pinning as `http::get`. The String-bodied form lowers
            // to `gos_rt_http_request` (body arrives as a c-string,
            // like `gos_rt_http_stream`); the byte-bodied form lowers
            // to `gos_rt_http_request_bytes` (body arrives as a byte
            // GosVec) so binary upload payloads survive intact.
            "http::request" | "http::request_bytes" => {
                let resp_def = gossamer_resolve::DefId::local(u32::MAX - 5);
                let resp_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: resp_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([resp_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                let sym = if joined == "http::request" {
                    "gos_rt_http_request"
                } else {
                    "gos_rt_http_request_bytes"
                };
                (sym, result_ty)
            }
            // `http::stream(method, url, body, headers) -> Result<ResponseStream, errors::Error>`.
            // Pin the Ok payload to the sentinel-DefId
            // ResponseStream Adt so `.__handle` / `.status` /
            // `.content_type` projections find the right field index
            // via `stdlib_struct_shapes`. Without this binding, the
            // call lowered to a non-existent symbol and the
            // destination held an undefined pointer the caller
            // dereferenced as a Result aggregate (askq SSE chat
            // round hung when next_line read garbage).
            "http::stream" => {
                let rs_def = gossamer_resolve::DefId::local(u32::MAX - 4);
                let rs_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: rs_def,
                    substs: gossamer_types::Substs::new(),
                });
                let err_ty = self.tcx.dyn_error_ty();
                let substs = gossamer_types::Substs::from_types([rs_ty, err_ty]);
                let result_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX),
                    substs,
                });
                ("gos_rt_http_stream", result_ty)
            }
            "http::Client::new" => (
                "gos_rt_http_client_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Client::builder" => (
                "gos_rt_http_client_builder_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Response::text" => (
                "gos_rt_http_response_text_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Response::json" => (
                "gos_rt_http_response_json_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `Response::stream(status, content_type, rs)` - the rs
            // argument is the 3-slot ResponseStream blob pointer
            // (same ptr shape `next_line` receives as receiver).
            "http::Response::stream" | "Response::stream" => (
                "gos_rt_http_response_stream_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::serve" => {
                let ty = self.result_unit_error_adt_ty();
                ("gos_rt_http_serve", ty)
            }
            "http::serve_h2c" => {
                let ty = self.result_unit_error_adt_ty();
                ("gos_rt_http2_bind_and_run_h2c", ty)
            }
            _ => return None,
        })
    }

    pub(super) fn lower_http_3_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.4.0 HTTP-module bridges (compiled tier free-fn surface).
            // Stateful types (router::new, etc.) are interp-only and not
            // listed here - calling them in compiled mode emits an
            // "unsupported call" diagnostic via the generic fallback.
            "http::chunked::encode" | "chunked::encode" => {
                ("gos_rt_chunked_encode", self.tcx.string_ty())
            }
            "http::chunked::decode" | "chunked::decode" => {
                ("gos_rt_chunked_decode", self.tcx.string_ty())
            }
            "http::sse::encode_event" | "sse::encode_event" => {
                ("gos_rt_sse_encode_event", self.tcx.string_ty())
            }
            "http::sse::encode_comment" | "sse::encode_comment" => {
                ("gos_rt_sse_encode_comment", self.tcx.string_ty())
            }
            "http::sse::encode_retry" | "sse::encode_retry" => {
                ("gos_rt_sse_encode_retry", self.tcx.string_ty())
            }
            "http::middleware::new_request_id" | "middleware::new_request_id" => {
                ("gos_rt_mw_new_request_id", self.tcx.string_ty())
            }
            "http::middleware::accepts_gzip" | "middleware::accepts_gzip" => {
                ("gos_rt_mw_accepts_gzip", self.tcx.bool_ty())
            }
            "http::middleware::decode_basic_auth" | "middleware::decode_basic_auth" => (
                "gos_rt_mw_decode_basic_auth",
                self.option_pair_string_adt_ty(),
            ),
            "http::websocket::accept_key" | "websocket::accept_key" => {
                ("gos_rt_ws_accept_key", self.tcx.string_ty())
            }
            "http::websocket::is_websocket_upgrade" | "websocket::is_websocket_upgrade" => {
                ("gos_rt_ws_is_upgrade", self.tcx.bool_ty())
            }
            "http::websocket::accept" | "websocket::accept" => {
                ("gos_rt_ws_accept", self.result_response_error_adt_ty())
            }
            "http::websocket::connect" | "websocket::connect" => {
                ("gos_rt_ws_serve_connect", self.result_i64_error_adt_ty())
            }
            "http::websocket::send_text" | "websocket::send_text" => {
                ("gos_rt_ws_send_text", self.result_unit_error_adt_ty())
            }
            "http::websocket::send_binary" | "websocket::send_binary" => {
                ("gos_rt_ws_send_binary", self.result_unit_error_adt_ty())
            }
            "http::websocket::recv" | "websocket::recv" => {
                ("gos_rt_ws_recv", self.result_string_error_adt_ty())
            }
            "http::websocket::close" | "websocket::close" => {
                ("gos_rt_ws_close", self.result_unit_error_adt_ty())
            }
            "http::cookie::parse_cookie_header" | "cookie::parse_cookie_header" => {
                ("gos_rt_http_cookie_parse_header", self.string_pair_vec_ty())
            }
            "http::cookie::serialize" | "cookie::serialize" => {
                ("gos_rt_http_cookie_serialize", self.tcx.string_ty())
            }
            "http::csrf::issue_token" | "csrf::issue_token" => (
                "gos_rt_http_csrf_issue_token",
                self.result_string_error_adt_ty(),
            ),
            "http::csrf::verify_token" | "csrf::verify_token" => (
                "gos_rt_http_csrf_verify_token",
                self.result_unit_error_adt_ty(),
            ),
            "http::session::sign" | "session::sign" => {
                ("gos_rt_http_session_sign", self.tcx.string_ty())
            }
            "http::session::verify" | "session::verify" => (
                "gos_rt_http_session_verify",
                self.result_string_error_adt_ty(),
            ),
            "http::static_files::mime_for_path" | "static_files::mime_for_path" => {
                ("gos_rt_static_mime_for_path", self.tcx.string_ty())
            }
            _ => return None,
        })
    }

    pub(super) fn lower_http_4_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // Stateful constructors. The MIR call-path emits the
            // bare runtime symbol; user code does `Router::new()`
            // → constructor handle. Returns `*mut T` (Ptr) which
            // the caller treats as the receiver of subsequent
            // method calls.
            "http::router::Router::new"
            | "router::Router::new"
            | "Router::new"
            | "http::router::new"
            | "router::new" => (
                "gos_rt_router_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::websocket::ws_frame_text" | "websocket::ws_frame_text" => {
                ("gos_rt_ws_frame_text", self.tcx.string_ty())
            }
            "http::native_client::Client::new"
            | "native_client::Client::new"
            | "NativeClient::new" => (
                "gos_rt_native_client_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::static_files::FileServer::new"
            | "static_files::FileServer::new"
            | "FileServer::new" => (
                "gos_rt_file_server_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::proxy::Proxy::new" | "proxy::Proxy::new" | "Proxy::new" => (
                "gos_rt_proxy_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::ResponseStream::new" | "ResponseStream::new" => (
                "gos_rt_http_response_stream_open",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "http::Server::new" | "Server::new" => (
                "gos_rt_http_server_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }
}
