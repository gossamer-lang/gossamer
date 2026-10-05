//! Choosing the runtime symbol a method call reaches, and heap element ordering.

use super::*;

impl<'a> Builder<'a> {
    /// Name-keyed runtime-symbol dispatch table (`Bail` = stop the lowering).
    #[allow(
        clippy::too_many_lines,
        reason = "flat method-name dispatch table; arm order encodes guarded/unguarded same-name shadowing"
    )]
    pub(super) fn runtime_symbol_by_name(
        &self,
        receiver: &HirExpr,
        method: &Ident,
        args: &[HirExpr],
        receiver_kind_flat: &TyKind,
        receiver_ty: Ty,
    ) -> SymbolLookup {
        let receiver_kind_flat = receiver_kind_flat.clone();
        // A user `impl` method wins over every builtin of the same name. The
        // table below keys on method name first and receiver kind second, and
        // several arms end in a catch-all that would take a user type with
        // it - `len` on an enum reaching `gos_rt_len`, which reads a Vec
        // header out of the enum's pointer.
        if self.user_impl_method_exists(receiver_ty, receiver, &method.name) {
            return SymbolLookup::Found(None);
        }
        // The open dynamic value answers its whole surface through the
        // `gos_rt_dyn_*` family, on every tier. Matched ahead of the shared
        // names below (`at`, `len`, `as_i64`, ...) so a `DynValue` receiver
        // never falls into another type's helper.
        if matches!(&receiver_kind_flat, TyKind::DynValue) {
            if let Some(symbol) = match method.name.as_str() {
                "kind" => Some("gos_rt_dyn_kind_name"),
                "name" => Some("gos_rt_dyn_name"),
                "len" => Some("gos_rt_dyn_len"),
                "at" => Some("gos_rt_dyn_at"),
                "key_at" => Some("gos_rt_dyn_key_at"),
                "as_i64" => Some("gos_rt_dyn_as_i64"),
                "as_f64" => Some("gos_rt_dyn_as_f64"),
                "as_bool" => Some("gos_rt_dyn_as_bool"),
                "as_char" => Some("gos_rt_dyn_as_char"),
                "as_str" => Some("gos_rt_dyn_as_str"),
                "as_bytes" => Some("gos_rt_dyn_as_bytes"),
                "clone" => Some("gos_rt_dyn_clone"),
                "to_string" => Some("gos_rt_dyn_display"),
                _ => None,
            } {
                return SymbolLookup::Found(Some(symbol));
            }
        }
        SymbolLookup::Found(match method.name.as_str() {
            // `.to_string()` routes to the runtime numeric
            // formatter for integer / float receivers. String
            // receivers fall through to the identity copy.
            // `to_string()` (no args) - scalar-to-string for
            // integer / float receivers; identity copy for the
            // others.
            //
            // `to_string(len)` (1 arg) - the canonical "freeze the
            // build buffer" step at the end of a `U8Vec`-backed
            // incremental construction loop. Mirrors F#'s
            // `StringBuilder.ToString()` and Rust's
            // `String::from_utf8(vec).unwrap()`. Routes to a
            // runtime helper that copies the first `len` bytes
            // into a fresh immutable `String`.
            "to_string" => {
                if args.len() == 1 {
                    Some("gos_rt_heap_u8_to_string")
                } else {
                    match &receiver_kind_flat {
                        TyKind::Int(int) => Some(super::int_to_str_symbol(*int)),
                        TyKind::Float(gossamer_types::FloatTy::F32) => Some("gos_rt_f32_to_str"),
                        TyKind::Float(_) => Some("gos_rt_f64_to_str"),
                        // A `char` is a scalar Unicode value, so its String
                        // form is built rather than reinterpreted.
                        TyKind::Char => Some("gos_rt_char_to_str"),
                        TyKind::Bool => Some("gos_rt_bool_to_str"),
                        _ => Some(""),
                    }
                }
            }
            "clone" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Slice(_) => Some("gos_rt_vec_clone"),
                // A `String` clone takes a share of its own. Lowering it
                // to nothing left the clone and its source as one value
                // with one share, so a consuming callee given the clone
                // took the caller's.
                TyKind::String => Some("gos_rt_str_clone"),
                _ => Some(""),
            },
            "extend" | "extend_from_slice" if args.len() == 1 => match &receiver_kind_flat {
                // The text's own bytes are appended in place, so no byte
                // vector is built only to be copied and released.
                TyKind::Vec(_) if self.string_as_bytes_text(&args[0]).is_some() => {
                    Some("gos_rt_vec_extend_str_bytes")
                }
                TyKind::Vec(_) => Some("gos_rt_vec_extend"),
                _ => None,
            },
            "truncate" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_truncate"),
                _ => None,
            },
            "reserve" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_reserve_at_least"),
                _ => None,
            },
            "reserve_exact" if args.len() == 1 => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_reserve_exact"),
                _ => None,
            },
            "capacity" if args.is_empty() => match &receiver_kind_flat {
                TyKind::Vec(_) => Some("gos_rt_vec_capacity"),
                _ => None,
            },
            // Option / Result methods. Result/Option now live as
            // `*mut GosResult { disc, payload }` heap aggregates
            // (see `gos_rt_result_new`), so `.unwrap()` /
            // `.unwrap_or()` / `.ok()` / `.err()` route through
            // runtime helpers that read the disc and return the
            // payload (or default) as a raw 64-bit slot. The
            // older identity-copy path was a leftover from the
            // pre-discriminator layout and silently returned the
            // aggregate pointer for callers expecting an i64 -
            // see e.g. fasta's `args[0].to_i64().unwrap_or(1000)`,
            // which yielded an arena address instead of 10. Fall
            // back to identity for non-Result receivers (e.g.
            // stdlib helpers that still return raw inner values
            // tagged with a Result-shaped HIR type).
            "unwrap" | "expect" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    // A payload that is itself a carrier was boxed at
                    // construction, so it is loaded back as two words.
                    let nested = self.carrier_payload_is_carrier(receiver_ty);
                    match (self.is_option_adt(receiver_ty), nested) {
                        (true, true) => Some("gos_rt_option_unwrap_carrier"),
                        (true, false) => Some("gos_rt_option_unwrap"),
                        (false, true) => Some("gos_rt_result_unwrap_carrier"),
                        (false, false) => Some("gos_rt_result_unwrap"),
                    }
                } else {
                    Some("")
                }
            }
            "unwrap_or" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    // A payload that is itself a carrier was boxed at
                    // construction, so it is loaded back rather than
                    // handed over as the address the word helper returns.
                    if self.carrier_payload_is_carrier(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_carrier")
                    } else if self.carrier_payload_is_map(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_map")
                    } else if self.carrier_payload_is_sequence(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_vec")
                    } else if self.carrier_payload_is_string(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_str")
                    } else if self.carrier_payload_is_counted_node(receiver_ty) {
                        Some("gos_rt_result_unwrap_or_node")
                    } else {
                        Some("gos_rt_result_unwrap_or")
                    }
                } else {
                    Some("")
                }
            }
            // `.ok()` / `.err()` answer an `Option`, so they take the carrier
            // helpers the `result::ok` / `result::err` free forms use. The
            // payload-reading helpers of the same name are the `unwrap` family
            // and hand back a bare word, which a `match` then reads as a
            // discriminant.
            "ok" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_to_opt_ok")
                } else {
                    Some("")
                }
            }
            "err" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_to_opt_err")
                } else {
                    Some("")
                }
            }
            "next" if args.is_empty() => match &receiver_kind_flat {
                TyKind::Iterator(elem) => self.lazy_iter_next_symbol(*elem),
                _ => None,
            },
            // `option.ok_or(new_err)` converts None into Err and
            // passes Some through.
            "ok_or" => {
                if matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_option_adt(receiver_ty)
                {
                    Some("gos_rt_result_ok_or")
                } else {
                    Some("")
                }
            }
            "len" => match &receiver_kind_flat {
                TyKind::String => Some("gos_rt_str_len"),
                TyKind::HashMap { .. } => Some("gos_rt_map_len"),
                TyKind::JsonValue => Some("gos_rt_json_len"),
                TyKind::Vec(_) | TyKind::Array { .. } | TyKind::Slice(_) => Some("gos_rt_len"),
                // The MIR type didn't resolve. Inspect the HIR
                // expression's static type as a fallback - common
                // shape is `let s = fs::read_to_string(...)?; s.len()`
                // where the typechecker leaves `s` as `Var(...)` but
                // the HIR `Path(s)` node still carries `String`.
                // Without this fallback the dispatch lands on the
                // generic `gos_rt_len`, which reads a Vec header
                // out of a `*const c_char` and returns garbage.
                _ => {
                    let mut peeled = receiver.ty;
                    while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
                        peeled = *inner;
                    }
                    match self.tcx.kind_of(peeled) {
                        TyKind::String => Some("gos_rt_str_len"),
                        TyKind::HashMap { .. } => Some("gos_rt_map_len"),
                        TyKind::JsonValue => Some("gos_rt_json_len"),
                        _ => Some("gos_rt_len"),
                    }
                }
            },
            "trim" => Some("gos_rt_str_trim"),
            "trim_start" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_start")
            }
            "trim_end" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_end")
            }
            "contains" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_contains")
            }
            "starts_with" => Some("gos_rt_str_starts_with"),
            "ends_with" => Some("gos_rt_str_ends_with"),
            "find" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_find_opt"),
            "rfind" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rfind_opt")
            }
            "to_i64" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_i64_opt")
            }
            "to_f64" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_f64_opt")
            }
            "to_bool" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_bool_opt")
            }
            "replace" => Some("gos_rt_str_replace"),
            "split" => Some("gos_rt_str_split"),
            // 0.7.0 string surface - split_once / rsplit_once return
            // `Option<(String, String)>` packed as a `*mut GosResult`
            // pair payload (see `gos_rt_str_split_once`).
            "split_once" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_split_once")
            }
            "rsplit_once" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rsplit_once")
            }
            "count" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_count"),
            "trim_start_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_lstrip_chars")
            }
            "trim_end_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_rstrip_chars")
            }
            "center" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_center"),
            "slice" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_slice"),
            "substring" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_substring")
            }
            // 0.14.0 - the remaining canonical String method surface.
            // Each already exists as a `strings::*` free fn (see
            // `stdlib_free.rs`); wiring the method form here lets
            // `s.method(...)` and the `_.method` pipe placeholder dispatch
            // on the compiled tiers the same way the VM does, instead of
            // emitting an undefined `@method` symbol.
            "split_whitespace" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_split_whitespace")
            }
            "splitn" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_splitn"),
            "to_title" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_to_title")
            }
            "trim_matches" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_trim_matches")
            }
            "replacen" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_replacen")
            }
            "pad_left" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_pad_left")
            }
            "pad_right" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_pad_right")
            }
            "contains_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_contains_any")
            }
            "equal_fold" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_equal_fold")
            }
            "find_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_index_any")
            }
            "rfind_any" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_last_index_any")
            }
            "strip_prefix" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_strip_prefix")
            }
            "strip_suffix" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_strip_suffix")
            }
            // 0.7.0 Vec method surface. `xs.slice(a, b)?` returns a
            // Result<Vec<T>, errors::Error>; `xs.first()` / `xs.last()`
            // return Option<T>; `xs.rev()` returns a fresh Vec;
            // `xs.contains` / `xs.index_of` / `xs.count_of` need
            // element-type dispatch (String vs i64).
            // `xs.slice(a, b)?` - receiver shape decides which
            // helper handles the buffer layout. Vec receivers
            // (`Vec<T>` and `&[T]` after the `to_vec` route) carry
            // a `GosVec` header; raw `[T; N]` array literals are
            // a len-prefixed flat `*const i64` buffer the
            // intarr / floatarr shims walk directly.
            "slice" if matches!(&receiver_kind_flat, TyKind::Vec(_) | TyKind::Slice(_)) => {
                Some("gos_rt_vec_slice_result")
            }
            "slice" if matches!(&receiver_kind_flat, TyKind::Array { .. }) => {
                let elem_kind = match &receiver_kind_flat {
                    TyKind::Array { elem, .. } => self.tcx.kind_of(*elem),
                    _ => unreachable!(),
                };
                if matches!(elem_kind, TyKind::Float(_)) {
                    Some("gos_rt_floatarr_slice_result")
                } else if matches!(elem_kind, TyKind::Int(gossamer_types::IntTy::U8)) {
                    // Byte-packed result (stride 1) - keeps a `[u8]` slice at
                    // one byte per element instead of 8x.
                    Some("gos_rt_bytearr_slice_result")
                } else {
                    Some("gos_rt_intarr_slice_result")
                }
            }
            "first"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_first")
            }
            "last"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_last")
            }
            "get"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_get_opt")
            }
            "rev"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                Some("gos_rt_vec_reversed")
            }
            "take"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_take")
            }
            "step_by"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                Some("gos_rt_vec_step_by")
            }
            // `xs.join(sep)` - the method form of `strings::join` for a
            // String element, and the Display-rendering join shims for
            // scalar elements. Keyed on the element TyKind so a numeric
            // vec never joins pointer words; the one-arg gate keeps
            // `JoinHandle::join` (zero args, handled above) unshadowed.
            "join"
                if args.len() == 1
                    && matches!(
                        &receiver_kind_flat,
                        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                    ) =>
            {
                self.vec_join_symbol(receiver_ty)
            }
            "contains"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_contains_str"
                } else {
                    "gos_rt_vec_contains_i64"
                })
            }
            "index_of"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_index_of_str"
                } else {
                    "gos_rt_vec_index_of_i64"
                })
            }
            "count_of"
                if matches!(
                    &receiver_kind_flat,
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. }
                ) =>
            {
                let elem = vec_element_kind(self.tcx, receiver_ty);
                Some(if elem == VecElemKind::Str {
                    "gos_rt_vec_count_of_str"
                } else {
                    "gos_rt_vec_count_of_i64"
                })
            }
            // 0.7.0 HashMap method surface - keys / values yield
            // Vec<K> / Vec<V>; pop returns Option<V>.
            "keys" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                Some(if self.map_keys_unsigned(receiver_ty) {
                    "gos_rt_map_keys_vec_u64"
                } else {
                    "gos_rt_map_keys_vec"
                })
            }
            // A boxed two-word value is read back out of each box.
            "values"
                if matches!(&receiver_kind_flat, TyKind::HashMap { .. })
                    && self.map_value_is_carrier(receiver_ty) =>
            {
                Some(if self.map_keys_unsigned(receiver_ty) {
                    "gos_rt_map_values_carrier_u64"
                } else {
                    "gos_rt_map_values_carrier"
                })
            }
            "values" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                Some(if self.map_keys_unsigned(receiver_ty) {
                    "gos_rt_map_values_vec_u64"
                } else {
                    "gos_rt_map_values_vec"
                })
            }
            "pop" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                let key = hashmap_key_kind(self.tcx, receiver_ty);
                Some(if key == VecElemKind::Str {
                    "gos_rt_map_pop_typed_str"
                } else {
                    "gos_rt_map_pop_i64"
                })
            }
            "lines" => Some("gos_rt_str_lines"),
            "repeat" => Some("gos_rt_str_repeat"),
            "byte_len" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_str_byte_len")
            }
            "byte_at" => Some("gos_rt_str_byte_at"),
            // `s.chars()` - materialise the Unicode scalars as a
            // `Vec<char>` (one i64 codepoint per slot) so the
            // for-loop reads each via `gos_rt_vec_get_i64` and binds
            // a `char`. Gated on a String receiver: a user struct
            // with its own `chars` method falls through to user
            // dispatch.
            // A String's `chars()` answers a cursor over the text.
            "chars" if matches!(&receiver_kind_flat, TyKind::String) => {
                Some("gos_rt_lazy_iter_str_chars")
            }
            // `is_empty` collapses to `len(self) == 0`. Route to
            // a small helper that delegates to the right `len`
            // backend for the receiver kind.
            "is_empty" => match &receiver_kind_flat {
                TyKind::String => Some("gos_rt_str_is_empty"),
                _ => Some("gos_rt_len_is_zero"),
            },
            // errors::Error methods. `is` here routes
            // unconditionally to the runtime helper because no
            // other type in the stdlib defines a `.is(...)`
            // method today; if a user struct defines one, the
            // user-impl dispatch below wins (it runs after this
            // table).
            "message" => Some("gos_rt_error_message"),
            "cause" => Some("gos_rt_error_cause"),
            "is" => Some("gos_rt_error_is"),
            "chain" => Some("gos_rt_error_chain"),
            "with_field" => Some("gos_rt_error_with_field"),
            "field" => Some("gos_rt_error_field"),
            "fields" => Some("gos_rt_error_fields"),
            // bufio::Scanner methods.
            "scan" => Some("gos_rt_bufio_scanner_scan"),
            "text" => Some("gos_rt_bufio_scanner_text"),
            // `ResponseStream::next_line() -> Option<String>`. The
            // receiver is the 3-slot blob `[__handle, status,
            // content_type]` returned by `gos_rt_http_stream`; the
            // helper reads the leading i64 (the handle) and pops
            // one line from the registered Vec.
            "next_line" => Some("gos_rt_http_stream_next_line"),
            // `ResponseStream::next_chunk(max_bytes) ->
            // Option<[u8]>` - same blob receiver as `next_line`;
            // the Some payload is a packed `elem_bytes = 1` byte
            // vec (the `raw_bytes` representation contract).
            "next_chunk" => Some("gos_rt_http_stream_next_chunk"),
            // http::Response getters.
            "status" => Some("gos_rt_http_response_status"),
            "body" => Some("gos_rt_http_response_body"),
            // http builder. The kind-dispatch above already routes
            // tagged `http::Request` receivers for `.header(k, v)`
            // builder calls; this name-only arm catches untagged
            // ones - `.send` falls below to the channel default
            // because channel sends are far more common in user
            // code than untagged-http requests.
            "header" => Some("gos_rt_http_request_header"),
            // Chainable server-response builder: replace-then-push
            // a header and return the same response pointer.
            "with_header" => Some("gos_rt_http_response_with_header"),
            "send" => Some("gos_rt_chan_send"),
            // string parsing - `text.parse()` for an i64 binding
            // routes to gos_rt_parse_i64 with a discarded ok flag.
            // Pin return to i64 for the common case; users with
            // f64 / float must annotate explicitly today.
            "parse" => Some("gos_rt_parse_i64_result"),
            // Result/Option chained helpers map to identity on
            // the happy path. The user passes in a closure; we
            // discard it (the compiled tier doesn't run the
            // error-mapping closure today).
            // map_err / map only dispatch when the receiver's
            // MIR-pinned type is a real Result Adt. Stdlib helpers
            // like `fs::write` return a bool today; routing
            // `.map_err(...)` on a bool through the result-helper
            // would feed an i8 to a `*mut GosResult` parameter and
            // trip the cranelift verifier.
            "map_err" => {
                if (matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty))
                    || self.expr_is_send_result(receiver)
                {
                    Some("gos_rt_result_map_err")
                } else {
                    Some("")
                }
            }
            "map" => {
                if (matches!(&receiver_kind_flat, TyKind::Adt { .. })
                    && self.is_result_or_option_adt(receiver_ty))
                    || self.expr_is_send_result(receiver)
                {
                    Some("gos_rt_result_map")
                } else {
                    Some("")
                }
            }
            "to_lowercase" => Some("gos_rt_str_to_lower"),
            "to_uppercase" => Some("gos_rt_str_to_upper"),
            "push" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Var(_) => Some("gos_rt_vec_push"),
                _ => None,
            },
            "pop" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Var(_) => Some("gos_rt_vec_pop_opt"),
                _ => None,
            },
            "sort" => Some(
                if vec_element_kind(self.tcx, receiver_ty) == VecElemKind::Str {
                    "gos_rt_vec_sort_str"
                } else if self.sequence_elem_is_float(receiver_ty) {
                    "gos_rt_vec_sort_f64"
                } else {
                    "gos_rt_vec_sort_i64"
                },
            ),
            "reverse" => match &receiver_kind_flat {
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Var(_) => Some("gos_rt_vec_reverse"),
                _ => None,
            },
            "iter" => match &receiver_kind_flat {
                // HashMap `.iter()` is handled before the helper-name
                // dispatch: the `for (k, v) in m.iter()` shape by
                // `try_lower_for_hashmap_iter` and the direct-binding
                // `let xs = m.iter()` shape by
                // `materialize_hashmap_entries` (both produce a real
                // `Vec<(K, V)>`). Reaching this arm would mean a map
                // receiver slipped past both; fall back to a MIR error
                // rather than the `gos_rt_arr_iter` path, which would
                // reinterpret the `*mut GosMap` as a `*mut GosVec`.
                TyKind::HashMap { .. } => return SymbolLookup::Bail,
                // A sequence whose element the lazy state cannot carry keeps
                // its own handle, so the eager sequence helpers - which read
                // the element at its real width - consume it directly.
                TyKind::Vec(elem) | TyKind::Slice(elem)
                    if !self.lazy_iter_borrowable_elem(*elem) =>
                {
                    Some("")
                }
                // A fixed array lives inline in its slots rather than behind a
                // `*mut GosVec` header, so the handle-taking iterator helper
                // would read those slots as one. Take the eager sequence path,
                // which reads the elements at their real width.
                TyKind::Array { .. } => Some(""),
                _ => Some("gos_rt_arr_iter"),
            },
            "collect" | "to_vec" => match &receiver_kind_flat {
                // Vec/Slice/Array `.to_vec()` and `.collect()` must produce an
                // independent copy - bubble_sort's `out.swap(...)`
                // was mutating the caller's slice through the
                // aliased pointer. Other types fall through to
                // the identity copy.
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_clone")
                }
                _ => Some(""),
            },
            // `s.bytes()` is the UTF-8 byte view of a String receiver, the
            // same buffer `as_bytes` materialises. Gated on a String
            // receiver so a user struct with its own `bytes` method falls
            // through to user dispatch.
            "bytes" if matches!(&receiver_kind_flat, TyKind::String) => Some("gos_rt_str_as_bytes"),
            "as_bytes" => match &receiver_kind_flat {
                // String-receiver `.as_bytes()` materialises a real
                // length-prefixed arena buffer. The previous identity
                // lowering returned the raw c_char ptr; passing that
                // to a callee with `&[u8]` parameter and calling
                // `.len()` on it inside the callee read the first
                // 8 string bytes as a length, crashing on dereference.
                TyKind::String => Some("gos_rt_str_as_bytes"),
                _ => Some(""),
            },
            "as_str" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_as_str_opt"),
                _ => Some(""),
            },
            // JSON value query/cast methods. The runtime helpers
            // accept a `*mut GosJson` (passed as a flat pointer)
            // and return either a fresh `*mut GosJson` (for
            // chained queries) or a primitive scalar.
            "as_i64" => Some("gos_rt_json_as_i64_opt"),
            "as_u64" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_as_u64_opt"),
                _ => None,
            },
            "as_f64" => Some("gos_rt_json_as_f64_opt"),
            "as_bool" => Some("gos_rt_json_as_bool_opt"),
            "is_null" => Some("gos_rt_json_is_null"),
            "at" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_at"),
                _ => None,
            },
            "recv" => Some("gos_rt_chan_recv_option"),
            // `rx.recv_ctx(&ctx)` - same shape as `recv`, but
            // takes a Context handle as the second arg. The
            // runtime helper polls cancellation on both the
            // goroutine park path and the OS-thread condvar
            // path, returning None when the context fires.
            "recv_ctx" => Some("gos_rt_chan_recv_ctx_option"),
            "try_send" => Some("gos_rt_chan_try_send"),
            "try_recv" => Some("gos_rt_chan_try_recv_option"),
            // `close` is also a user-facing method on structs (the
            // injected sql `Rows` / `Conn` wrappers). Route to the
            // channel helper only when the receiver is not a struct
            // carrying its own `close` impl - the same receiver gate
            // as `insert` / `get` below. Without it, `rows.close()`
            // closed a bogus channel handle instead of dispatching
            // to `__gos_sql_Rows::close`.
            "close" => {
                let user_close = self
                    .struct_name_of(receiver_ty)
                    .or_else(|| self.struct_name_from_expr(receiver))
                    .is_some_and(|s| self.impl_methods.contains_key(&format!("{s}::close")));
                if user_close {
                    None
                } else {
                    Some("gos_rt_chan_close")
                }
            }
            // Stream methods (on `io::stdout()` / `io::stderr()`
            // / `io::stdin()` handles). Mirrors Rust's `Write` /
            // `BufRead` trait surface.
            "write_byte" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_write_byte")
            }
            "write_byte_array" | "write_bytes"
                if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") =>
            {
                Some("gos_rt_stream_write_byte_array")
            }
            "write" | "write_str"
                if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") =>
            {
                Some("gos_rt_stream_write_str")
            }
            "flush" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_flush")
            }
            "read_line" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some(if args.is_empty() {
                    "gos_rt_stream_next_line"
                } else {
                    "gos_rt_stream_read_line"
                })
            }
            "read_to_string" if self.runtime_kind_from_ty(receiver_ty) == Some("io::Stream") => {
                Some("gos_rt_stream_read_to_string")
            }
            // HashMap method dispatch - gated on the receiver
            // actually being a `HashMap`, not just on having a
            // matching method name. Without the gate, a user
            // struct with an `impl Foo { fn get(...) }` would
            // route through the map helper at codegen time and
            // either segfault on the wrong ABI or read garbage.
            // `get` extends the gate to `JsonValue` because the
            // json runtime also exposes a single-arg `get(key)`.
            "insert" => match &receiver_kind_flat {
                // An aggregate value (Vec / struct) is stored as an 8-byte
                // handle word, so the helper routes by KEY kind there - a
                // String key must still use the str path, not the i64/i64
                // path that reinterprets the key pointer.
                TyKind::HashMap { .. } => Some(self.map_insert_helper(receiver_ty)),
                // Vec insertion mutates in place and returns a bounds error.
                // A struct / tuple / array element reaches the runtime as the
                // address of its slot block, the way `gos_rt_vec_push` takes
                // one; every other element is the word itself. The two shapes
                // need different entry points because a one-slot struct is
                // indistinguishable from a scalar once it is in the vec.
                TyKind::Vec(elem) | TyKind::Slice(elem) => {
                    if self.is_inline_slot_block(*elem) {
                        Some("gos_rt_vec_insert_slots_safe")
                    } else {
                        Some("gos_rt_vec_insert_safe")
                    }
                }
                TyKind::Array { elem, .. } => {
                    if self.is_inline_slot_block(*elem) {
                        Some("gos_rt_vec_insert_slots_safe")
                    } else {
                        Some("gos_rt_vec_insert_safe")
                    }
                }
                _ => None,
            },
            "get" => match &receiver_kind_flat {
                TyKind::JsonValue => Some("gos_rt_json_get"),
                // HashMap::get now uniformly returns Option<V> packed
                // in a *mut GosResult. The MIR pin restores V from the
                // call's Option<V> substs so `if let Some(p) = m.get(&k)`
                // binds `p` with the right element type - struct refs
                // included. Pre-0.8.0 the bare i64-returning helpers
                // collided None with stored-0 (HashMap<_, i64>) and
                // produced a silent miscompile on field access through
                // struct-valued maps. See feature-testing-examples/
                // hashmap_get_some_field.gos.
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_get_typed_str_opt"),
                    _ => Some("gos_rt_map_get_i64_opt"),
                },
                _ => None,
            },
            "get_or" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_value_kind(receiver_ty) {
                    Some(MapValueKind::String) => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_str_str"),
                        _ => Some("gos_rt_map_get_or_i64_str"),
                    },
                    Some(MapValueKind::Bytes) => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_typed_str_i64"),
                        _ => Some("gos_rt_map_get_or_i64"),
                    },
                    _ => match self.hash_map_key_kind(receiver_ty) {
                        Some(MapKeyKind::String) => Some("gos_rt_map_get_or_typed_str_i64"),
                        _ => Some("gos_rt_map_get_or_i64"),
                    },
                },
                _ => None,
            },
            // `or_insert` stores an 8-byte value word (i64 scalar or
            // an aggregate handle: Vec / struct), so route purely by
            // KEY kind - a String-keyed, Vec-valued map needs the str
            // path, not the absent value-kind branch that emitted an
            // undefined `@or_insert` call.
            "or_insert" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_or_insert_typed_str_i64"),
                    _ => Some("gos_rt_map_or_insert_i64_i64"),
                },
                _ => None,
            },
            "remove" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_pop_typed_str"),
                    _ => Some("gos_rt_map_pop_i64"),
                },
                // Vec removal mutates in place and returns a bounds error.
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_remove_safe")
                }
                _ => None,
            },
            // A `BTreeMap`'s ordered slices, which its `first_key_value`,
            // `pop_first`, `range`, and kin desugar to.
            "__window" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                Some("gos_rt_map_window")
            }
            "__range" if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) => {
                match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_range_typed_str"),
                    _ => Some("gos_rt_map_range_i64"),
                }
            }
            "contains_key" | "contains"
                if matches!(&receiver_kind_flat, TyKind::HashMap { .. }) =>
            {
                match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_contains_key_typed_str"),
                    _ => Some("gos_rt_map_contains_key_i64"),
                }
            }
            "clear" if args.is_empty() => match &receiver_kind_flat {
                TyKind::HashMap { .. } => Some("gos_rt_map_clear"),
                TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
                    Some("gos_rt_vec_clear")
                }
                _ => None,
            },
            // `m.inc_at(seq, start, len, by)` - zero-copy slice
            // hash for `HashMap<String, i64>`. Single hash lookup
            // per call, no per-iteration scratch allocation -
            // mirrors `*m.entry(&seq[i..i+k]).or_insert(0) += by`.
            "inc_at" => match self.hash_map_value_kind(receiver_ty) {
                Some(MapValueKind::I64) => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_inc_at_str_i64"),
                    _ => None,
                },
                _ => None,
            },
            // HashMap iteration. Each helper snapshots the
            // requested column into a fresh `GosVec` so the
            // for-loop lowerer can drive iteration with the
            // regular `gos_rt_vec_*` helpers. String-keyed /
            // string-valued shapes go through `*_str`; everything
            // else through `*_i64`.
            "keys" => match &receiver_kind_flat {
                TyKind::HashMap { .. } => match self.hash_map_key_kind(receiver_ty) {
                    Some(MapKeyKind::String) => Some("gos_rt_map_keys_str"),
                    _ if self.map_keys_unsigned(receiver_ty) => Some("gos_rt_map_keys_u64"),
                    _ => Some("gos_rt_map_keys_i64"),
                },
                _ => None,
            },
            "values" => match &receiver_kind_flat {
                TyKind::HashMap { .. } if self.map_value_is_carrier(receiver_ty) => {
                    Some(if self.map_keys_unsigned(receiver_ty) {
                        "gos_rt_map_values_carrier_u64"
                    } else {
                        "gos_rt_map_values_carrier"
                    })
                }
                TyKind::HashMap { .. } => match self.hash_map_value_kind(receiver_ty) {
                    Some(MapValueKind::String) => Some("gos_rt_map_values_str"),
                    Some(MapValueKind::Bytes) => Some("gos_rt_map_values_vec"),
                    _ if self.map_keys_unsigned(receiver_ty) => Some("gos_rt_map_values_u64"),
                    _ => Some("gos_rt_map_values_i64"),
                },
                _ => None,
            },
            // Mutex<T> / WaitGroup / Atomic / heap-Vec
            // primitives. Each method dispatches by name -
            // the runtime function takes the receiver
            // pointer as its first arg, matching the rest of
            // the table.
            "lock" => Some("gos_rt_mutex_lock"),
            "unlock" => Some("gos_rt_mutex_unlock"),
            "add" => Some("gos_rt_wg_add"),
            "done" => Some("gos_rt_wg_done"),
            "wait" => Some("gos_rt_wg_wait"),
            "wait_ctx" => Some("gos_rt_wg_wait_ctx"),
            "load" => Some("gos_rt_atomic_i64_load"),
            "store" => Some("gos_rt_atomic_i64_store"),
            "fetch_add" => Some("gos_rt_atomic_i64_fetch_add"),
            "fetch_sub" => Some("gos_rt_atomic_i64_fetch_sub"),
            // The AtomicBool receiver is matched by kind above; every other
            // atomic compares through the i64 storage they all share.
            "compare_exchange" => Some("gos_rt_atomic_i64_cas"),
            "set_at" => Some("gos_rt_heap_i64_set"),
            "get_at" => Some("gos_rt_heap_i64_get"),
            "vec_len" => Some("gos_rt_heap_i64_len"),
            "write_range_to_stdout" => Some("gos_rt_heap_i64_write_bytes_to_stdout"),
            "write_lines_to_stdout" => Some("gos_rt_heap_i64_write_lines_to_stdout"),
            // U8Vec methods. Distinct names from the I64Vec
            // family because MIR's method dispatch is by name
            // alone - sharing `set_at` between i64 and u8
            // receivers would silently write through the
            // i64-stride helper to a u8 buffer, corrupting
            // adjacent bytes.
            "set_byte" => Some("gos_rt_heap_u8_set"),
            "window_key" => Some("gos_rt_heap_u8_window_key"),
            "count_singles" => Some("gos_rt_heap_u8_count_singles"),
            "count_pairs" => Some("gos_rt_heap_u8_count_pairs"),
            "count_kmers" => Some("gos_rt_heap_u8_count_kmers"),
            "get_byte" => Some("gos_rt_heap_u8_get"),
            "byte_len" => Some("gos_rt_heap_u8_len"),
            "write_byte_range_to_stdout" => Some("gos_rt_heap_u8_write_bytes_to_stdout"),
            "write_byte_lines_to_stdout" => Some("gos_rt_heap_u8_write_lines_to_stdout"),
            _ => None,
        })
    }

    /// Receiver-runtime-kind dispatch table (flag/http/stdlib handles).
    pub(super) fn kind_dispatch_symbol(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        heap_reverse_i64: bool,
        heap_float_elem: bool,
    ) -> Option<&'static str> {
        self.kind_dispatch_symbol_a(
            rk,
            method,
            args,
            receiver_ty,
            heap_reverse_i64,
            heap_float_elem,
        )
        .or_else(|| {
            self.kind_dispatch_symbol_b(
                rk,
                method,
                args,
                receiver_ty,
                heap_reverse_i64,
                heap_float_elem,
            )
        })
    }

    /// First half of the receiver-runtime-kind dispatch table.
    pub(super) fn kind_dispatch_symbol_a(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        _args: &[HirExpr],
        receiver_ty: Ty,
        heap_reverse_i64: bool,
        heap_float_elem: bool,
    ) -> Option<&'static str> {
        if matches!(
            rk,
            Some("collections::BinaryHeap" | "collections::MaxHeap" | "collections::MinHeap")
        ) {
            return self.binary_heap_runtime_symbol(
                rk,
                receiver_ty,
                method,
                heap_reverse_i64,
                heap_float_elem,
            );
        }
        match (rk, method.name.as_str()) {
            (Some("flag::Set"), "string") => Some("gos_rt_flag_set_string"),
            (Some("flag::Set"), "int") => Some("gos_rt_flag_set_int"),
            (Some("flag::Set"), "uint") => Some("gos_rt_flag_set_uint"),
            (Some("flag::Set"), "float") => Some("gos_rt_flag_set_float"),
            (Some("flag::Set"), "bool") => Some("gos_rt_flag_set_bool"),
            (Some("flag::Set"), "duration") => Some("gos_rt_flag_set_duration"),
            (Some("flag::Set"), "string_list") => Some("gos_rt_flag_set_string_list"),
            (Some("flag::Set"), "short") => Some("gos_rt_flag_set_short"),
            (Some("flag::Set"), "usage") => Some("gos_rt_flag_set_usage"),
            (Some("flag::Set"), "parse") => Some("gos_rt_flag_set_parse"),
            // 0.4.0 stateful HTTP types - method-call dispatch.
            (Some("http::Router"), "add") => Some("gos_rt_router_add"),
            (Some("http::Router"), "get") => Some("gos_rt_router_get"),
            (Some("http::Router"), "post") => Some("gos_rt_router_post"),
            (Some("http::Router"), "put") => Some("gos_rt_router_put"),
            (Some("http::Router"), "delete") => Some("gos_rt_router_delete"),
            (Some("http::Router"), "patch") => Some("gos_rt_router_patch"),
            (Some("http::Router"), "head") => Some("gos_rt_router_head"),
            (Some("http::Router"), "options") => Some("gos_rt_router_options"),
            (Some("http::Router"), "serve") => Some("gos_rt_router_serve"),
            (Some("http::Server"), "read_header_timeout_ms") => {
                Some("gos_rt_http_server_read_header_timeout_ms")
            }
            (Some("http::Server"), "request_timeout_ms") => {
                Some("gos_rt_http_server_request_timeout_ms")
            }
            (Some("http::Server"), "read_body_timeout_ms") => {
                Some("gos_rt_http_server_read_body_timeout_ms")
            }
            (Some("http::Server"), "write_timeout_ms") => {
                Some("gos_rt_http_server_write_timeout_ms")
            }
            (Some("http::Server"), "idle_timeout_ms") => Some("gos_rt_http_server_idle_timeout_ms"),
            (Some("http::Server"), "max_header_bytes") => {
                Some("gos_rt_http_server_max_header_bytes")
            }
            (Some("http::Server"), "max_body_bytes") => Some("gos_rt_http_server_max_body_bytes"),
            (Some("http::Server"), "max_connections") => Some("gos_rt_http_server_max_connections"),
            (Some("http::Server"), "server_name") => Some("gos_rt_http_server_server_name"),
            (Some("http::ResponseStream"), "write") => Some("gos_rt_http_response_stream_write"),
            (Some("http::ResponseStream"), "write_bytes") => {
                Some("gos_rt_http_response_stream_write_bytes")
            }
            (Some("http::ResponseStream"), "close") => Some("gos_rt_http_response_stream_close"),
            (Some("http::ResponseStream"), "is_open") => {
                Some("gos_rt_http_response_stream_is_open")
            }
            (Some("http::Server"), "listen") => Some("gos_rt_http_server_listen"),
            (Some("http::Server"), "addr") => Some("gos_rt_http_server_addr"),
            (Some("http::Server"), "shutdown") => Some("gos_rt_http_server_shutdown"),
            (Some("http::FileServer"), "serve") => Some("gos_rt_file_server_serve"),
            (Some("http::NativeClient"), "get") => Some("gos_rt_native_client_get"),
            (Some("http::Proxy"), "forward") => Some("gos_rt_proxy_forward"),
            (Some("http::Client"), "get") => Some("gos_rt_http_client_get"),
            (Some("http::Client"), "post") => Some("gos_rt_http_client_post"),
            (Some("http::Client"), "put") => Some("gos_rt_http_client_put"),
            (Some("http::Client"), "options") => Some("gos_rt_http_client_options"),
            (Some("http::Client"), "delete") => Some("gos_rt_http_client_delete"),
            (Some("http::Client"), "head") => Some("gos_rt_http_client_head"),
            (Some("http::Client"), "request") => Some("gos_rt_http_client_request"),
            (Some("http::Client"), "request_bytes") => Some("gos_rt_http_client_request_bytes"),
            (Some("http::ClientBuilder"), "max_redirects") => {
                Some("gos_rt_http_client_builder_max_redirects")
            }
            (Some("http::ClientBuilder"), "timeout_ms") => {
                Some("gos_rt_http_client_builder_timeout_ms")
            }
            (Some("http::ClientBuilder"), "cookie_jar") => {
                Some("gos_rt_http_client_builder_cookie_jar")
            }
            (Some("http::ClientBuilder"), "proxy") => Some("gos_rt_http_client_builder_proxy"),
            (Some("http::ClientBuilder"), "build") => Some("gos_rt_http_client_builder_build"),
            (Some("http::Request"), "header") => Some("gos_rt_http_request_header"),
            (Some("http::Request"), "body") => Some("gos_rt_http_request_body"),
            (Some("http::Request"), "send") => Some("gos_rt_http_request_send"),
            (Some("http::Request"), "path") => Some("gos_rt_http_request_path"),
            (Some("http::Request"), "path_value") => Some("gos_rt_http_request_path_value"),
            (Some("http::Request"), "path_int") => Some("gos_rt_http_request_path_int"),
            (Some("http::Request"), "path_float") => Some("gos_rt_http_request_path_float"),
            (Some("http::Request"), "method") => Some("gos_rt_http_request_method"),
            (Some("http::Request"), "value") => Some("gos_rt_http_request_value"),
            (Some("http::Request"), "set_value") => Some("gos_rt_http_request_set_value"),
            (Some("http::Request"), "form_value") => Some("gos_rt_http_request_form_value"),
            (Some("http::Request"), "basic_auth") => Some("gos_rt_http_request_basic_auth"),
            (Some("http::Response"), "with_header") => Some("gos_rt_http_response_with_header"),
            (Some("http::Response"), "status") => Some("gos_rt_http_response_status"),
            (Some("http::Response"), "body") => Some("gos_rt_http_response_body"),
            (Some("bufio::Scanner"), "scan") => Some("gos_rt_bufio_scanner_scan"),
            (Some("bufio::Scanner"), "text") => Some("gos_rt_bufio_scanner_text"),
            (Some("bufio::Scanner"), "next") => Some("gos_rt_bufio_scanner_next"),
            (Some("errors::Error"), "message") => Some("gos_rt_error_message"),
            // `{}` on an error renders the colon-joined chain, and
            // `to_string` is the same contract by another spelling.
            (Some("errors::Error"), "to_string") => Some("gos_rt_error_display"),
            (Some("errors::Error"), "cause") => Some("gos_rt_error_cause"),
            (Some("errors::Error"), "is") => Some("gos_rt_error_is"),
            (Some("errors::Error"), "chain") => Some("gos_rt_error_chain"),
            (Some("errors::Error"), "with_field") => Some("gos_rt_error_with_field"),
            (Some("errors::Error"), "field") => Some("gos_rt_error_field"),
            (Some("errors::Error"), "fields") => Some("gos_rt_error_fields"),
            (Some("regex::Pattern"), "is_match") => Some("gos_rt_regex_is_match"),
            (Some("regex::Pattern"), "count") => Some("gos_rt_regex_count"),
            (Some("regex::Pattern"), "find") => Some("gos_rt_regex_find"),
            (Some("regex::Pattern"), "find_all") => Some("gos_rt_regex_find_all"),
            (Some("regex::Pattern"), "replace") => Some("gos_rt_regex_replace"),
            (Some("regex::Pattern"), "replace_all") => Some("gos_rt_regex_replace_all"),
            (Some("regex::Pattern"), "split") => Some("gos_rt_regex_split"),
            (Some("collections::HashSet" | "collections::BTreeSet"), "insert") => {
                Some("gos_rt_set_insert")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "contains") => {
                Some("gos_rt_set_contains")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "remove") => {
                Some("gos_rt_set_remove")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "len") => {
                Some("gos_rt_set_len")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_empty") => {
                Some("gos_rt_set_is_empty")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "union") => {
                Some("gos_rt_set_union")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "intersection") => {
                Some("gos_rt_set_intersection")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "difference") => {
                Some("gos_rt_set_difference")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "symmetric_difference") => {
                Some("gos_rt_set_symmetric_difference")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_subset") => {
                Some("gos_rt_set_is_subset")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_superset") => {
                Some("gos_rt_set_is_superset")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "is_disjoint") => {
                Some("gos_rt_set_is_disjoint")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "to_vec" | "iter") => {
                Some("gos_rt_set_to_vec")
            }
            (Some("collections::HashSet" | "collections::BTreeSet"), "clear") => {
                Some("gos_rt_set_clear")
            }
            // A `BTreeSet`'s ordered slices, which its `first`, `pop_first`,
            // `range`, and kin desugar to.
            (Some("collections::BTreeSet"), "__window") => Some("gos_rt_set_window"),
            (Some("collections::BTreeSet"), "__range") => Some("gos_rt_set_range_str"),
            // A `GosSet` table is reached through a handle carrying no count
            // of its holders, so the clone takes a table of its own.
            (Some("collections::HashSet" | "collections::BTreeSet"), "clone") => {
                Some("gos_rt_set_clone")
            }
            (Some("collections::VecDeque"), "push_back") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecDeque"), "push_front") => Some("gos_rt_deque_push_front"),
            (Some("collections::VecDeque"), "pop_front") => Some("gos_rt_deque_pop_front"),
            (Some("collections::VecDeque"), "pop_back") => Some("gos_rt_deque_pop_back"),
            (Some("collections::VecDeque"), "peek_front") => Some("gos_rt_deque_peek_front"),
            (Some("collections::VecDeque"), "peek_back") => Some("gos_rt_deque_peek_back"),
            (Some("collections::VecDeque"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecDeque"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecDeque"), "clear") => Some("gos_rt_deque_clear"),
            (Some("collections::VecQueue"), "push") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecQueue"), "pop") => Some("gos_rt_deque_pop_front"),
            (Some("collections::VecQueue"), "peek") => Some("gos_rt_deque_peek_front"),
            (Some("collections::VecQueue"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecQueue"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecQueue"), "clear") => Some("gos_rt_deque_clear"),
            (Some("collections::VecStack"), "push") => Some("gos_rt_deque_push_back"),
            (Some("collections::VecStack"), "pop") => Some("gos_rt_deque_pop_back"),
            (Some("collections::VecStack"), "peek") => Some("gos_rt_deque_peek_back"),
            (Some("collections::VecStack"), "len") => Some("gos_rt_deque_len"),
            (Some("collections::VecStack"), "is_empty") => Some("gos_rt_deque_is_empty"),
            (Some("collections::VecStack"), "clear") => Some("gos_rt_deque_clear"),
            _ => None,
        }
    }

    pub(super) fn binary_heap_runtime_symbol(
        &self,
        rk: Option<&'static str>,
        receiver_ty: Ty,
        method: &Ident,
        heap_reverse_i64: bool,
        float_elem: bool,
    ) -> Option<&'static str> {
        let min_heap = rk == Some("collections::MinHeap")
            || heap_reverse_i64
            || self.binary_heap_ty_is_min(receiver_ty)
            || self.binary_heap_elem_is_reverse_i64(receiver_ty);
        // A float element is stored as its bit pattern, whose integer order is
        // not the float order, so the sift compares the value the bits spell.
        // Peek reads the root without comparing, so it stays on the integer
        // entry point.
        let float_elem = float_elem || self.heap_elem_is_float(receiver_ty);
        // An element the one-word integer and float entry points cannot
        // order - a `String`, a tuple, a struct, a sequence, an `Option`, an
        // enum - is compared through its ordering descriptor instead.
        let desc_elem = !float_elem
            && self
                .first_generic_of(receiver_ty)
                .is_some_and(|elem| self.heap_elem_needs_desc(elem));
        if desc_elem {
            return match (method.name.as_str(), min_heap) {
                ("push", true) => Some("gos_rt_bheap_min_push_desc"),
                ("pop", true) => Some("gos_rt_bheap_min_pop_desc"),
                ("push", false) => Some("gos_rt_bheap_max_push_desc"),
                ("pop", false) => Some("gos_rt_bheap_max_pop_desc"),
                ("peek", _) => Some("gos_rt_bheap_peek_elem"),
                ("len", _) => Some("gos_rt_bheap_len"),
                ("is_empty", _) => Some("gos_rt_bheap_is_empty"),
                ("clear", _) => Some("gos_rt_bheap_clear"),
                _ => None,
            };
        }
        match (method.name.as_str(), min_heap) {
            ("push", true) if float_elem => Some("gos_rt_bheap_min_push_f64"),
            ("pop", true) if float_elem => Some("gos_rt_bheap_min_pop_f64"),
            ("push", false) if float_elem => Some("gos_rt_bheap_max_push_f64"),
            ("pop", false) if float_elem => Some("gos_rt_bheap_max_pop_f64"),
            ("push", true) => Some("gos_rt_bheap_min_push_i64"),
            ("pop", true) => Some("gos_rt_bheap_min_pop_i64"),
            ("peek", true) => Some("gos_rt_bheap_min_peek_i64"),
            ("push", false) => Some("gos_rt_bheap_max_push_i64"),
            ("pop", false) => Some("gos_rt_bheap_max_pop_i64"),
            ("peek", false) => Some("gos_rt_bheap_max_peek_i64"),
            ("len", _) => Some("gos_rt_bheap_len"),
            ("is_empty", _) => Some("gos_rt_bheap_is_empty"),
            ("clear", _) => Some("gos_rt_bheap_clear"),
            _ => None,
        }
    }

    /// Emits `gos_rt_f64_to_bits(value)` and answers the local holding the
    /// word, so a float reaches a one-word store as its own bits rather than
    /// as the integer the value converts to.
    pub(crate) fn emit_float_bits(&mut self, value: Local, span: Span) -> Local {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let bits = self.fresh(i64_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_f64_to_bits".to_string())),
            args: vec![Operand::Copy(Place::local(value))],
            destination: Place::local(bits),
            target: Some(next),
        });
        self.set_current(next);
        bits
    }

    /// Whether a heap element orders through its descriptor rather than as
    /// one integer or float word: everything but the scalars a slot holds
    /// and compares directly.
    pub(crate) fn heap_elem_needs_desc(&self, elem: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = elem;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        !matches!(
            self.tcx.kind_of(cur),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::Duration
                | TyKind::Instant
                | TyKind::Var(_)
        )
    }

    /// Whether a heap receiver's element is a 64-bit unsigned integer, whose
    /// slots span the whole unsigned range and so order as `u64` rather than
    /// as the signed value the same bits spell.
    pub(crate) fn heap_elem_is_unsigned(&self, ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { substs, .. }) = self.tcx.kind(cur) else {
            return false;
        };
        substs.types().first().is_some_and(|elem| {
            matches!(
                self.tcx.kind_of(*elem),
                TyKind::Int(IntTy::U64 | IntTy::Usize)
            )
        })
    }

    /// Whether a heap receiver's element is a float, whose slots hold bit
    /// patterns the ordering entry points read as floats.
    pub(crate) fn heap_elem_is_float(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { substs, .. }) = self.tcx.kind(cur) else {
            return false;
        };
        substs
            .types()
            .first()
            .is_some_and(|elem| matches!(self.tcx.kind_of(*elem), TyKind::Float(_)))
    }

    pub(crate) fn binary_heap_elem_is_reverse_i64(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(cur) else {
            return false;
        };
        if def.local != BINARY_HEAP_DEF_LOCAL && self.tcx.def_name(*def) != Some("BinaryHeap") {
            return false;
        }
        let Some(elem) = substs.types().first().copied() else {
            return false;
        };
        let mut elem = elem;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(elem) {
            elem = *inner;
        }
        self.is_reverse_i64_ty(elem)
    }

    pub(crate) fn binary_heap_ty_is_min(&self, ty: Ty) -> bool {
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(self.tcx.kind(cur), Some(TyKind::Adt { def, .. }) if def.local == MIN_HEAP_DEF_LOCAL)
    }

    pub(crate) fn is_reverse_i64_ty(&self, ty: Ty) -> bool {
        use gossamer_types::{IntTy, TyKind};
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(cur) else {
            return false;
        };
        (def.local == REVERSE_DEF_LOCAL || self.tcx.def_name(*def) == Some("Reverse"))
            && substs.types().first().is_some_and(|payload| {
                matches!(self.tcx.kind_of(*payload), TyKind::Int(IntTy::I64))
            })
    }

    /// Second half of the receiver-runtime-kind dispatch table.
    pub(super) fn kind_dispatch_symbol_b(
        &self,
        rk: Option<&'static str>,
        method: &Ident,
        args: &[HirExpr],
        receiver_ty: Ty,
        _heap_reverse_i64: bool,
        _heap_float_elem: bool,
    ) -> Option<&'static str> {
        let _ = receiver_ty;
        if let Some(symbol) =
            rk.and_then(|kind| super::types::sync_method_symbol(kind, method.name.as_str()))
        {
            return Some(symbol);
        }
        match (rk, method.name.as_str()) {
            (Some("sync::Map"), "insert") => Some("gos_rt_sync_map_set"),
            (Some("sync::Map"), "get") => Some("gos_rt_sync_map_get"),
            (Some("sync::Map"), "remove") => Some("gos_rt_sync_map_delete"),
            (Some("sync::Map"), "len") => Some("gos_rt_sync_map_len"),
            (Some("sync::Map"), "contains_key") => Some("gos_rt_sync_map_contains"),
            (Some("sync::Map"), "keys") => Some("gos_rt_sync_map_keys"),
            (Some("math::rand::Rng"), "next_u64") => Some("gos_rt_math_rng_next_u64"),
            (Some("math::rand::Rng"), "next_u32") => Some("gos_rt_math_rng_next_u32"),
            (Some("math::rand::Rng"), "range_u64") => Some("gos_rt_math_rng_range_u64"),
            (Some("math::rand::Rng"), "next_f64") => Some("gos_rt_math_rng_next_f64"),
            (Some("validate::FieldError"), "path") => Some("gos_rt_field_error_path"),
            (Some("validate::FieldError"), "message") => Some("gos_rt_field_error_message"),
            (Some("validate::FieldError"), "code") => Some("gos_rt_field_error_code"),
            (Some("validate::Errors"), "add") => Some("gos_rt_validate_errors_add"),
            (Some("validate::Errors"), "is_empty") => Some("gos_rt_validate_errors_is_empty"),
            (Some("validate::Errors"), "len") => Some("gos_rt_validate_errors_len"),
            (Some("validate::Errors"), "count") => Some("gos_rt_validate_errors_count"),
            (Some("validate::Errors"), "get") => Some("gos_rt_validate_errors_get"),
            (Some("validate::Errors"), "collect") => Some("gos_rt_validate_errors_collect"),
            (Some("sync::RwLock"), "read") => Some("gos_rt_rwlock_get"),
            (Some("sync::RwLock"), "write") => Some("gos_rt_rwlock_set"),
            (Some("sync::Shared"), "get") => Some("gos_rt_shared_get"),
            (Some("sync::Shared"), "set") => Some("gos_rt_shared_set"),
            // AtomicBool load/store route to the bool-typed shims so
            // the load result renders `true` / `false`; the name-only
            // table below keeps AtomicI64 on the i64 path.
            (Some("sync::AtomicBool"), "load") => Some("gos_rt_atomic_bool_load"),
            (Some("sync::AtomicBool"), "store") => Some("gos_rt_atomic_bool_store"),
            (Some("sync::AtomicBool"), "compare_exchange") => Some("gos_rt_atomic_bool_cas"),
            (Some("context::Context"), "is_cancelled") => Some("gos_rt_ctx_is_cancelled"),
            (Some("context::Context"), "cancel") => Some("gos_rt_ctx_cancel"),
            (Some("context::Context"), "done") => Some("gos_rt_ctx_done"),
            (Some("context::Context"), "done_chan") => Some("gos_rt_ctx_cancelled"),
            (Some("metrics::Counter"), "inc") => Some("gos_rt_metrics_counter_inc"),
            (Some("metrics::Counter"), "value") => Some("gos_rt_metrics_counter_value"),
            (Some("metrics::Gauge"), "set") => Some("gos_rt_metrics_gauge_set"),
            (Some("metrics::Gauge"), "inc") => Some("gos_rt_metrics_gauge_inc"),
            (Some("metrics::Gauge"), "dec") => Some("gos_rt_metrics_gauge_dec"),
            (Some("metrics::Gauge"), "value") => Some("gos_rt_metrics_gauge_value"),
            (Some("metrics::Histogram"), "observe") => Some("gos_rt_metrics_histogram_observe"),
            (Some("metrics::Histogram"), "sum") => Some("gos_rt_metrics_histogram_sum"),
            (Some("metrics::Histogram"), "count") => Some("gos_rt_metrics_histogram_count"),
            (Some("metrics::Registry"), "register") => Some("gos_rt_metrics_registry_register"),
            (Some("metrics::Registry"), "render") => Some("gos_rt_metrics_registry_render"),
            (Some("trace::Tracer"), "start_span") => Some("gos_rt_trace_tracer_start_span"),
            (Some("trace::Span"), "set_attribute") => Some("gos_rt_trace_span_set_attribute"),
            (Some("trace::Span"), "set_status") => Some("gos_rt_trace_span_set_status"),
            (Some("trace::Span"), "end") => Some("gos_rt_trace_span_end"),
            (Some("trace::EndedSpan"), "to_otlp_json") => Some("gos_rt_trace_ended_to_otlp_json"),
            (Some("bytes::Builder"), "write") => Some("gos_rt_bytes_builder_write"),
            (Some("bytes::Builder"), "write_char") => Some("gos_rt_bytes_builder_write_char"),
            (Some("bytes::Builder"), "build") => Some("gos_rt_bytes_builder_build"),
            (Some("bytes::Builder"), "as_str") => Some("gos_rt_bytes_builder_as_str"),
            (Some("bytes::Builder"), "len") => Some("gos_rt_bytes_builder_len"),
            (Some("bytes::Buffer"), "write_str") => Some("gos_rt_bytes_buffer_write_str"),
            (Some("bytes::Buffer"), "push") => Some("gos_rt_bytes_buffer_push"),
            (Some("bytes::Buffer"), "len") => Some("gos_rt_bytes_buffer_len"),
            (Some("bytes::Buffer"), "is_empty") => Some("gos_rt_bytes_buffer_is_empty"),
            (Some("bytes::Buffer"), "clear") => Some("gos_rt_bytes_buffer_clear"),
            (Some("bytes::Buffer"), "to_string") => Some("gos_rt_bytes_buffer_to_string"),
            (Some("net::TcpListener"), "accept") => Some("gos_rt_tcp_listener_accept"),
            (Some("net::TcpListener"), "local_addr") => Some("gos_rt_tcp_listener_local_addr"),
            (Some("net::TcpListener"), "close") => Some("gos_rt_tcp_listener_close"),
            (Some("net::TcpStream"), "read") => Some("gos_rt_tcp_stream_read"),
            (Some("net::TcpStream"), "read_into") => Some("gos_rt_tcp_stream_read_into"),
            (Some("net::TcpStream"), "read_to_string") => Some("gos_rt_tcp_stream_read_to_string"),
            (Some("net::TcpStream"), "write" | "write_all") => Some("gos_rt_tcp_stream_write"),
            (Some("net::TcpStream"), "set_read_timeout_ms") => {
                Some("gos_rt_tcp_stream_set_read_timeout_ms")
            }
            (Some("net::TcpStream"), "set_write_timeout_ms") => {
                Some("gos_rt_tcp_stream_set_write_timeout_ms")
            }
            (Some("net::TcpStream"), "set_nodelay") => Some("gos_rt_tcp_stream_set_nodelay"),
            (Some("net::TcpStream"), "clear_read_timeout") => {
                Some("gos_rt_tcp_stream_clear_read_timeout")
            }
            (Some("net::TcpStream"), "clear_write_timeout") => {
                Some("gos_rt_tcp_stream_clear_write_timeout")
            }
            (Some("net::TcpStream"), "start_tls") => Some("gos_rt_tcp_start_tls"),
            (Some("net::TcpStream"), "start_tls_insecure") => Some("gos_rt_tcp_start_tls_insecure"),
            (Some("net::TcpStream"), "start_tls_ca") => Some("gos_rt_tcp_start_tls_ca"),
            (Some("net::TcpStream"), "peer_certificate") => Some("gos_rt_tcp_tls_peer_cert"),
            (Some("net::TcpStream"), "close") => Some("gos_rt_tcp_stream_close"),
            (Some("fs::File"), "read") => Some("gos_rt_fs_file_read"),
            (Some("fs::File"), "read_to_string") => Some("gos_rt_fs_file_read_to_string"),
            (Some("fs::File"), "write" | "write_all") => Some("gos_rt_fs_file_write"),
            (Some("fs::File"), "write_bytes") => Some("gos_rt_fs_file_write_bytes"),
            (Some("fs::File"), "read_at") => Some("gos_rt_fs_file_read_at"),
            (Some("fs::File"), "read_at_into") => Some("gos_rt_fs_file_read_at_into"),
            (Some("fs::File"), "write_at") => Some("gos_rt_fs_file_write_at"),
            (Some("fs::File"), "seek") => Some("gos_rt_fs_file_seek"),
            (Some("fs::File"), "set_len") => Some("gos_rt_fs_file_set_len"),
            (Some("fs::File"), "len") => Some("gos_rt_fs_file_len"),
            (Some("fs::File"), "fd") => Some("gos_rt_fs_file_fd"),
            (Some("fs::File"), "sync_all") => Some("gos_rt_fs_file_sync_all"),
            (Some("fs::File"), "sync_data") => Some("gos_rt_fs_file_sync_data"),
            (Some("fs::File"), "try_lock_range") => Some("gos_rt_fs_file_try_lock_range"),
            (Some("fs::File"), "unlock_range") => Some("gos_rt_fs_file_unlock_range"),
            (Some("fs::File"), "try_lock_shared") => Some("gos_rt_fs_file_try_lock_shared"),
            (Some("fs::File"), "try_lock_exclusive") => Some("gos_rt_fs_file_try_lock_exclusive"),
            (Some("fs::File"), "unlock") => Some("gos_rt_fs_file_unlock"),
            (Some("fs::File"), "flush") => Some("gos_rt_fs_file_flush"),
            (Some("fs::File"), "close") => Some("gos_rt_fs_file_close"),
            (Some("fs::OpenOptions"), "read") => Some("gos_rt_fs_open_options_read"),
            (Some("fs::OpenOptions"), "write") => Some("gos_rt_fs_open_options_write"),
            (Some("fs::OpenOptions"), "append") => Some("gos_rt_fs_open_options_append"),
            (Some("fs::OpenOptions"), "truncate") => Some("gos_rt_fs_open_options_truncate"),
            (Some("fs::OpenOptions"), "create") => Some("gos_rt_fs_open_options_create"),
            (Some("fs::OpenOptions"), "create_new") => Some("gos_rt_fs_open_options_create_new"),
            (Some("fs::OpenOptions"), "open") => Some("gos_rt_fs_open_options_open"),
            (Some("net::UnixListener"), "accept") => Some("gos_rt_unix_listener_accept"),
            (Some("net::UnixListener"), "close") => Some("gos_rt_unix_listener_close"),
            (Some("net::UnixStream"), "read") => Some("gos_rt_unix_stream_read"),
            (Some("net::UnixStream"), "read_to_string") => {
                Some("gos_rt_unix_stream_read_to_string")
            }
            (Some("net::UnixStream"), "write" | "write_all") => Some("gos_rt_unix_stream_write"),
            (Some("net::UnixStream"), "close") => Some("gos_rt_unix_stream_close"),
            (Some("net::UdpSocket"), "send_to") => Some("gos_rt_udp_send_to"),
            (Some("net::UdpSocket"), "recv_from") => Some("gos_rt_udp_recv_from"),
            (Some("net::UdpSocket"), "local_addr") => Some("gos_rt_udp_local_addr"),
            (Some("net::UdpSocket"), "close") => Some("gos_rt_udp_close"),
            (Some("io::Stream"), "write_byte") => Some("gos_rt_stream_write_byte"),
            (Some("io::Stream"), "write_byte_array" | "write_bytes") => {
                Some("gos_rt_stream_write_byte_array")
            }
            (Some("io::Stream"), "write" | "write_str") => Some("gos_rt_stream_write_str"),
            (Some("io::Stream"), "flush") => Some("gos_rt_stream_flush"),
            (Some("io::Stream"), "read_line") => Some(if args.is_empty() {
                "gos_rt_stream_next_line"
            } else {
                "gos_rt_stream_read_line"
            }),
            (Some("io::Stream"), "read_to_string") => Some("gos_rt_stream_read_to_string"),
            (Some("signal::Notifier"), "wait") => Some("gos_rt_signal_wait"),
            (Some("signal::Notifier"), "try_wait") => Some("gos_rt_signal_try_wait"),
            (Some("signal::Notifier"), "stop") => Some("gos_rt_signal_stop"),
            _ => None,
        }
    }
}
