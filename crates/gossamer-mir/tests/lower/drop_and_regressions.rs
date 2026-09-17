/// A `HashMap` allocated only inside an `if` arm is reclaimed
/// without ever freeing uninitialised memory.
///
/// The owning slot is zero-initialised at function entry, so every
/// `gos_rt_map_free` the drop pass schedules (the pre-overwrite
/// guard and the at-`Return` reclaim) is a null-safe no-op on the
/// `else` path that never allocated. The map must still be freed on
/// the `if` path (no leak), and every free must be dominated by the
/// entry zero-init so the conditional shape can never free a live
/// uninit slot.
#[test]
fn drop_pass_guards_conditionally_initialised_local() {
    let source = r"
fn maybe_build(flag: bool) -> i64 {
    if flag {
        let mut m: Map<i64, i64> = Map::new()
        m.insert(1, 2)
        m.len()
    } else {
        0
    }
}
";
    let (bodies, _) = build(source);
    let body = bodies
        .iter()
        .find(|b| b.name == "maybe_build")
        .expect("body");
    // Every local freed by `gos_rt_map_free`, by the slot the call
    // releases.
    let freed_locals: Vec<Local> = body
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } if *name == "gos_rt_map_free" => match args.first() {
                Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local),
                _ => None,
            },
            _ => None,
        })
        .collect();

    // The conditionally-allocated map is reclaimed (no leak).
    assert!(
        !freed_locals.is_empty(),
        "conditionally-initialised map must still be freed (no leak)"
    );

    // Every freed slot is zero-initialised in the entry block, so the
    // free is a null-safe no-op on the path that never allocated.
    let entry = &body.blocks[0];
    for local in &freed_locals {
        let zero_init = entry.stmts.iter().any(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                } if place.projection.is_empty() && place.local == *local
            )
        });
        assert!(
            zero_init,
            "freed local {local:?} must be zero-initialised at entry so its free is null-safe"
        );
    }
}

/// `gos_rt_http_response_content_type` mints a fresh owned c-string
/// (`mints_owned_string`), so `let c = r.content_type` must move the
/// minted reference into the binding: no `gos_rt_rc_retain` anywhere
/// on the copy chain (move elision transfers the single reference)
/// and a `gos_rt_rc_release` on the binding. Without the
/// `mints_owned_string` entry the call temp is treated as a borrow,
/// the copy retains (+1), the binding releases (-1), and the minted
/// reference itself is never dropped - one leaked string per
/// `.content_type` read in compiled code.
#[test]
fn drop_pass_releases_http_response_content_type_string() {
    let source = r#"
use std::http

fn ct(url: String) -> i64 {
    match http::get(url, Vec::new()) {
        Ok(r) => {
            let c = r.content_type
            if c == "x" { 1 } else { 0 }
        }
        Err(_) => 0,
    }
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "ct").expect("body");

    let dest = body
        .blocks
        .iter()
        .find_map(|b| match &b.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                ..
            } if name == "gos_rt_http_response_content_type" => Some(destination.local),
            _ => None,
        })
        .expect("content_type accessor call");

    // Move elision may transfer the minted reference along bare-Copy
    // chains (`let c = r.content_type`), so the release can land on
    // any alias of the call destination.
    let mut aliases = vec![dest];
    loop {
        let mut grew = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && p.projection.is_empty()
                    && aliases.contains(&p.local)
                    && !aliases.contains(&place.local)
                {
                    aliases.push(place.local);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }

    let alias_rc_calls = |wanted: &str| -> usize {
        body.blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .filter(|stmt| {
                matches!(
                    &stmt.kind,
                    StatementKind::Assign {
                        rvalue: Rvalue::CallIntrinsic { name, args },
                        ..
                    } if *name == wanted
                        && matches!(
                            args.first(),
                            Some(Operand::Copy(p))
                                if p.projection.is_empty() && aliases.contains(&p.local)
                        )
                )
            })
            .count()
    };
    assert!(
        alias_rc_calls("gos_rt_rc_release") + alias_rc_calls("gos_rt_str_free_typed") > 0,
        "minted content_type string must be released (aliases: {aliases:?})"
    );
    assert_eq!(
        alias_rc_calls("gos_rt_rc_retain") + alias_rc_calls("gos_rt_str_retain_typed"),
        0,
        "the minted reference must move into the binding, not be retained - \
         a retain here means the call temp was treated as a borrow and the \
         minted string leaks (aliases: {aliases:?})"
    );
}

/// Signed integer `to_string().chars()` is allocation-fused into a fresh
/// chars vector. It must not materialise an intermediate String or deep-clone
/// the vector when moving it into the binding.
#[test]
fn numeric_to_string_chars_fuses_formatting_without_cloning_vec() {
    let source = r"
fn digits_len(n: i64) -> i64 {
    let digits = n.to_string().chars()
    digits.count()
}

fn inferred_range_digits() -> i64 {
    let mut total = 0
    for n in 1..=3 {
        let digits = n.to_string().chars()
        total += digits.count()
    }
    total
}
";
    let (bodies, _) = build(source);
    for function in ["digits_len", "inferred_range_digits"] {
        let body = bodies
            .iter()
            .find(|body| body.name == function)
            .unwrap_or_else(|| panic!("missing {function} body"));

        assert!(
            body.blocks.iter().any(|block| matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    ..
                } if name == "gos_rt_i64_chars"
            )),
            "{function}: numeric formatting and chars conversion must be fused"
        );
        assert!(
            body.blocks.iter().all(|block| !matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    ..
                } if matches!(name.as_str(), "gos_rt_i64_to_str" | "gos_rt_str_chars")
            )),
            "{function}: fused conversion must not materialise an intermediate String"
        );
        assert!(
            body.blocks.iter().all(|block| !matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    ..
                } if name == "gos_rt_vec_clone"
            )),
            "{function}: fresh chars vector must move directly into its binding"
        );
    }
}

/// C18 - drop pass keeps unconditional drops intact.
///
/// When a `HashMap` is allocated at the top of the function and
/// every path through the body keeps it owned by this frame, the
/// drop must still fire on `Return` to release the heap storage.
#[test]
fn drop_pass_keeps_unconditional_drop_intact() {
    let source = r"
fn build() -> i64 {
    let mut m: Map<i64, i64> = Map::new()
    m.insert(1, 2)
    m.len()
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "build").expect("body");
    let frees: Vec<_> = body
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } => {
                if *name == "gos_rt_map_free" {
                    Some(*name)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    assert!(
        !frees.is_empty(),
        "drop pass must free an unconditionally-allocated local"
    );
}

/// Destination local of a `X = Copy(tuple.0)` field-extract, the binding
/// produced when a tuple's first element is destructured.
fn field0_extract_dest(body: &gossamer_mir::Body) -> Option<Local> {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .find_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } if place.projection.is_empty()
                && matches!(
                    src.projection.as_slice(),
                    [gossamer_mir::Projection::Field(0)]
                ) =>
            {
                Some(place.local)
            }
            _ => None,
        })
}

/// Count of `name` intrinsic calls whose first argument is `local`.
fn rc_calls_on(body: &gossamer_mir::Body, name: &str, local: Local) -> usize {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name: n, args },
                    ..
                } if *n == name
                    && matches!(
                        args.first(),
                        Some(Operand::Copy(p)) if p.projection.is_empty() && p.local == local
                    )
            )
        })
        .count()
}

/// Count of `name` intrinsic calls on `local.field`.
fn field_rc_calls_on(body: &gossamer_mir::Body, name: &str, local: Local, field: u32) -> usize {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name: n, args },
                    ..
                } if *n == name
                    && matches!(
                        args.first(),
                        Some(Operand::Copy(p)) if p.local == local
                            && p.projection.as_slice() == [gossamer_mir::Projection::Field(field)]
                    )
            )
        })
        .count()
}

/// The tuple slot a field-0 extract copies from.
fn field0_extract_source(body: &gossamer_mir::Body) -> Option<Local> {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .find_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } if place.projection.is_empty()
                && matches!(
                    src.projection.as_slice(),
                    [gossamer_mir::Projection::Field(0)]
                ) =>
            {
                Some(src.local)
            }
            _ => None,
        })
}

/// A by-value tuple is a stack slot whose RC-managed elements are owned
/// per-field: `let t, n = make()` (where `make -> (String, i64)`) must keep
/// the `String` share it received in the slot's field, retained when the
/// slot is copied and released at the slot's end of life, so every round of
/// a tuple-returning allocator gives back what it took. The extracted binding
/// reads that share as a borrow for as long as the slot outlives its uses,
/// so its own accounting is balanced: a retain of its own paired with a
/// release, or neither.
#[test]
fn drop_pass_balances_tuple_extracted_rc_field() {
    let source = r#"
fn make() -> (String, i64) {
    let s = "node"
    (s, 1)
}

fn use_it() -> i64 {
    let t, n = make()
    n + t.byte_at(0)
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "use_it").expect("body");
    let dest = field0_extract_dest(body).expect("field-0 tuple extract");
    let slot = field0_extract_source(body).expect("field-0 tuple extract");

    assert!(
        field_rc_calls_on(body, "gos_rt_rc_retain", slot, 0) > 0,
        "the tuple slot's String field must be retained when the slot is copied"
    );
    assert!(
        field_rc_calls_on(body, "gos_rt_rc_release", slot, 0) > 0,
        "the tuple slot's String field must be released at the slot's end of life"
    );
    let retains = rc_calls_on(body, "gos_rt_rc_retain", dest)
        + rc_calls_on(body, "gos_rt_str_retain_typed", dest);
    let releases = rc_calls_on(body, "gos_rt_rc_release", dest)
        + rc_calls_on(body, "gos_rt_str_free_typed", dest);
    assert_eq!(
        retains == 0,
        releases == 0,
        "the extracted binding's share must be balanced: {retains} retain(s), {releases} release(s)"
    );
    assert_eq!(
        retains, 0,
        "the extracted binding borrows the slot's share while the slot outlives it"
    );
}

/// A `Result` / `Option` tuple element is a 2-word by-value, never an RC
/// pointer, so per-field accounting must skip it: destructuring
/// `(Result<String, _>, i64)` emits no retain on the field-0 extract.
/// Treating the packed value as a pointer would corrupt the heap.
#[test]
fn drop_pass_skips_result_tuple_element() {
    let source = r#"
use std::errors

fn make() -> (Result<String, errors::Error>, i64) {
    (Ok("node"), 1)
}

fn use_it() -> i64 {
    let _r, n = make()
    n
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "use_it").expect("body");
    if let Some(dest) = field0_extract_dest(body) {
        assert_eq!(
            rc_calls_on(body, "gos_rt_rc_retain", dest),
            0,
            "a Result tuple element is by-value and must not be RC-retained"
        );
    }
}

fn optimised(source: &str) -> gossamer_mir::Body {
    let (mut bodies, tcx) = build(source);
    let mut body = bodies.remove(0);
    optimise(&mut body, &tcx);
    body
}

fn has_binary_op(body: &gossamer_mir::Body) -> bool {
    body.blocks.iter().flat_map(|b| b.stmts.iter()).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { .. },
                ..
            }
        )
    })
}

#[test]
fn identity_fold_add_zero_either_side() {
    let body = optimised("fn f(x: i64) -> i64 { x + 0 }\n");
    assert!(!has_binary_op(&body), "x + 0 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { 0 + x }\n");
    assert!(!has_binary_op(&body), "0 + x must fold to x");
}

#[test]
fn identity_fold_sub_zero_rhs_only() {
    let body = optimised("fn f(x: i64) -> i64 { x - 0 }\n");
    assert!(!has_binary_op(&body), "x - 0 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { 0 - x }\n");
    assert!(has_binary_op(&body), "0 - x is a negation, not an identity");
}

#[test]
fn identity_fold_mul_one_either_side() {
    let body = optimised("fn f(x: i64) -> i64 { x * 1 }\n");
    assert!(!has_binary_op(&body), "x * 1 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { 1 * x }\n");
    assert!(!has_binary_op(&body), "1 * x must fold to x");
}

#[test]
fn absorbing_fold_mul_zero_to_const_zero() {
    let body = optimised("fn f(x: i64) -> i64 { x * 0 }\n");
    assert!(!has_binary_op(&body), "x * 0 must fold to 0");
    assert_eq!(
        const_value_of(&body, Local::RETURN),
        Some(ConstValue::Int(0)),
        "return slot must hold the absorbed 0"
    );
}

#[test]
fn identity_fold_div_rem_one_rhs() {
    let body = optimised("fn f(x: i64) -> i64 { x / 1 }\n");
    assert!(!has_binary_op(&body), "x / 1 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { x % 1 }\n");
    assert!(!has_binary_op(&body), "x % 1 must fold to 0");
    assert_eq!(
        const_value_of(&body, Local::RETURN),
        Some(ConstValue::Int(0))
    );
}

#[test]
fn no_fold_for_nonconstant_divisor() {
    let body = optimised("fn f(x: i64) -> i64 { 0 / x }\n");
    assert!(
        has_binary_op(&body),
        "0 / x must keep its runtime division (x may be zero)"
    );
    let body = optimised("fn f(x: i64) -> i64 { 0 % x }\n");
    assert!(
        has_binary_op(&body),
        "0 % x must keep its runtime remainder (x may be zero)"
    );
}

#[test]
fn identity_fold_bitwise_zero() {
    let body = optimised("fn f(x: i64) -> i64 { x | 0 }\n");
    assert!(!has_binary_op(&body), "x | 0 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { x ^ 0 }\n");
    assert!(!has_binary_op(&body), "x ^ 0 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { x & 0 }\n");
    assert!(!has_binary_op(&body), "x & 0 must fold to 0");
    assert_eq!(
        const_value_of(&body, Local::RETURN),
        Some(ConstValue::Int(0))
    );
}

#[test]
fn identity_fold_shift_zero_amount() {
    let body = optimised("fn f(x: i64) -> i64 { x << 0 }\n");
    assert!(!has_binary_op(&body), "x << 0 must fold to x");
    let body = optimised("fn f(x: i64) -> i64 { x >> 0 }\n");
    assert!(!has_binary_op(&body), "x >> 0 must fold to x");
}

#[test]
fn identity_fold_bool_operands() {
    let body = optimised("fn f(b: bool) -> bool { b & true }\n");
    assert!(!has_binary_op(&body), "b & true must fold to b");
    let body = optimised("fn f(b: bool) -> bool { b | false }\n");
    assert!(!has_binary_op(&body), "b | false must fold to b");
    let body = optimised("fn f(b: bool) -> bool { b ^ false }\n");
    assert!(!has_binary_op(&body), "b ^ false must fold to b");
    let body = optimised("fn f(b: bool) -> bool { b & false }\n");
    assert!(!has_binary_op(&body), "b & false must fold to false");
    assert_eq!(
        const_value_of(&body, Local::RETURN),
        Some(ConstValue::Bool(false))
    );
    let body = optimised("fn f(b: bool) -> bool { b | true }\n");
    assert!(!has_binary_op(&body), "b | true must fold to true");
    assert_eq!(
        const_value_of(&body, Local::RETURN),
        Some(ConstValue::Bool(true))
    );
}

#[test]
fn no_identity_fold_for_floats() {
    let body = optimised("fn f(y: f64) -> f64 { y + 0.0 }\n");
    assert!(
        has_binary_op(&body),
        "y + 0.0 is not an identity under IEEE-754 (-0.0 + 0.0 == +0.0)"
    );
    let body = optimised("fn f(y: f64) -> f64 { y * 1.0 }\n");
    assert!(has_binary_op(&body), "float ops stay unfolded");
}

#[test]
fn no_identity_fold_for_nonidentity_constant() {
    let body = optimised("fn f(x: i64) -> i64 { x + 1 }\n");
    assert!(has_binary_op(&body), "x + 1 must stay a runtime add");
}

// ----------------------------------------------------------------
// Bare-`http::Response` handler thunk synthesis.
//
// The HTTP runtime invokes every registered handler through the
// packed-Result i128 C-ABI, so a serve method (or router fn) that
// declares a bare `http::Response` return gets a synthesized
// `::__ok_wrap` body that calls the real handler and packs its
// return into `Ok` via `gos_rt_result_new`. The registration site
// must point `gos_fn_addr` at the thunk.
// ----------------------------------------------------------------

const BARE_SERVE_SOURCE: &str = r#"
use std::http

struct App { }

impl http::Handler for App {
    fn serve(&self, _r: http::Request) -> http::Response {
        http::Response::text(200, "ok")
    }
}

fn main() {
    let _ = http::serve("127.0.0.1:8080", App { })
}
"#;

fn gos_fn_addr_targets(body: &gossamer_mir::Body) -> Vec<String> {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue:
                    Rvalue::CallIntrinsic {
                        name: "gos_fn_addr",
                        args,
                    },
                ..
            } => match args.first() {
                Some(Operand::Const(ConstValue::Str(s))) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn bare_response_serve_method_synthesizes_ok_wrap_thunk() {
    let (bodies, _) = build(BARE_SERVE_SOURCE);
    let wrap = bodies
        .iter()
        .find(|b| b.name == "App::serve::__ok_wrap")
        .expect("synthesized ::__ok_wrap body for bare-Response serve");
    assert_eq!(wrap.arity, 2, "env thunk forwards (self, request)");
    let calls_serve = wrap.blocks.iter().any(|b| {
        matches!(
            &b.terminator,
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(s)),
                ..
            } if s == "App::serve"
        )
    });
    assert!(calls_serve, "thunk must call the wrapped App::serve");
    let packs_ok = wrap.blocks.iter().flat_map(|b| b.stmts.iter()).any(|stmt| {
        matches!(
            &stmt.kind,
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_result_new",
                    ..
                },
                ..
            }
        )
    });
    assert!(packs_ok, "thunk must pack the Response into Ok");
}

#[test]
fn response_stream_lowers_to_three_arg_stream_new_call() {
    let source = r#"
use std::http

struct App { }

impl http::Handler for App {
    fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {
        match http::stream("GET", "http://127.0.0.1:1/x", "", Vec::new()) {
            Ok(up) => Ok(http::Response::stream(up.status, up.content_type, up)),
            Err(e) => Err(e),
        }
    }
}

fn main() {
    let _ = http::serve("127.0.0.1:8080", App { })
}
"#;
    let (bodies, _) = build(source);
    let serve = bodies
        .iter()
        .find(|b| b.name == "App::serve")
        .expect("serve body");
    let arg_count = serve.blocks.iter().find_map(|b| match &b.terminator {
        Terminator::Call {
            callee: Operand::Const(ConstValue::Str(s)),
            args,
            ..
        } if s == "gos_rt_http_response_stream_new" => Some(args.len()),
        _ => None,
    });
    assert_eq!(
        arg_count,
        Some(3),
        "Response::stream must lower to the (status, content_type, rs) shim call"
    );
}

#[test]
fn bare_response_serve_registration_dispatches_through_thunk() {
    let (bodies, _) = build(BARE_SERVE_SOURCE);
    let main_body = bodies.iter().find(|b| b.name == "main").expect("main body");
    assert_eq!(
        gos_fn_addr_targets(main_body),
        vec!["App::serve::__ok_wrap".to_string()],
        "http::serve must register the Ok-packing thunk"
    );
}

#[test]
fn result_serve_method_keeps_direct_dispatch() {
    let source = r#"
use std::http

struct App { }

impl http::Handler for App {
    fn serve(&self, _r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, "ok"))
    }
}

fn main() {
    let _ = http::serve("127.0.0.1:8080", App { })
}
"#;
    let (bodies, _) = build(source);
    assert!(
        !bodies.iter().any(|b| b.name.ends_with("::__ok_wrap")),
        "Result-returning serve needs no thunk"
    );
    let main_body = bodies.iter().find(|b| b.name == "main").expect("main body");
    assert_eq!(
        gos_fn_addr_targets(main_body),
        vec!["App::serve".to_string()],
        "Result-returning serve dispatches directly"
    );
}

#[test]
fn bare_response_router_fn_registers_ok_wrap_thunk() {
    let source = r#"
use std::http
use std::http::router

fn hello(_r: http::Request) -> http::Response {
    http::Response::text(200, "ok")
}

fn main() {
    let r = router::Router::new()
    r.get("/hello", hello)
    let _ = http::serve("127.0.0.1:8080", r)
}
"#;
    let (bodies, _) = build(source);
    assert!(
        bodies.iter().any(|b| b.name == "hello::__ok_wrap"),
        "bare-Response router fn gets a thunk"
    );
    let main_body = bodies.iter().find(|b| b.name == "main").expect("main body");
    assert!(
        gos_fn_addr_targets(main_body).contains(&"hello::__ok_wrap".to_string()),
        "router registration must point gos_fn_addr at the thunk"
    );
}

/// Lowers `source` through the native pipeline shape: HIR lowering
/// plus the closure-lift pass, mirroring what `gos build` runs before
/// MIR. Needed for assertions about lifted closure bodies.
fn build_with_lift(source: &str) -> (Vec<gossamer_mir::Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(type_diags.is_empty(), "typecheck: {type_diags:?}");
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let hir = gossamer_hir::lift_closures(hir, &mut tcx);
    let bodies = lower_program(&hir, &mut tcx);
    (bodies, tcx)
}

#[test]
fn lifted_map_err_closure_param_keeps_string_type() {
    // The Err payload is a String; the lifted closure body's param
    // local must stay String after the lift pass. Before the checker
    // grew Result-combinator signatures the param reached the lift
    // unresolved and was pinned to i64, so `format!("{e}")` rendered
    // the payload pointer as an integer on the compiled tiers.
    let source = "fn fail() -> Result<i64, String> { Err(\"boom\") }\n\
                  fn main() { let r = fail().map_err(|e| format(\"w: {e}\"))\n\
                  let _ = r }\n";
    let (bodies, tcx) = build_with_lift(source);
    let lifted = bodies
        .iter()
        .find(|b| b.name.starts_with("__closure_"))
        .expect("lifted closure body");
    assert_eq!(lifted.arity, 1, "non-capturing closure takes one param");
    let param_ty = lifted.locals[1].ty;
    assert!(
        matches!(tcx.kind_of(param_ty), gossamer_types::TyKind::String),
        "lifted map_err closure param must be String, got {:?}",
        tcx.kind_of(param_ty)
    );
}

#[test]
fn lifted_iter_map_closure_param_keeps_string_type() {
    let source = "use std::iter\n\
                  fn main() { let xs: Vec<String> = Vec::from([\"a\", \"b\"])\n\
                  let ys = iter::map(|s| format(\"[{s}]\"), xs)\n\
                  let _ = ys }\n";
    let (bodies, tcx) = build_with_lift(source);
    let lifted = bodies
        .iter()
        .find(|b| b.name.starts_with("__closure_"))
        .expect("lifted closure body");
    let param_ty = lifted.locals[1].ty;
    assert!(
        matches!(tcx.kind_of(param_ty), gossamer_types::TyKind::String),
        "lifted iter::map closure param must be String, got {:?}",
        tcx.kind_of(param_ty)
    );
}

#[test]
fn positional_method_chains_fuse_into_one_loop() {
    let source = "fn main() {\n\
                  let xs = #[1, 2, 3, 4]\n\
                  let ys = #[5, 6, 7]\n\
                  let a = xs.iter().enumerate().map(|(i, v)| i * v).sum()\n\
                  let b = xs.iter().zip(ys.iter()).skip(1).step_by(2).count()\n\
                  let c = (0..10).rev().take(3).fold(0, |acc, v| acc + v)\n\
                  let _ = a + b + c\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("gos_rt_lazy_iter_") || name.starts_with("gos_rt_iter_")),
        "positional stages over a range or a sequence must fuse into one loop: {names:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.name.starts_with("__closure_")),
        "fused stage closures are inlined, so none is lifted"
    );
}

#[test]
fn map_entry_chains_fuse_into_the_map_walk() {
    let source = "fn main() {\n\
                  let mut m: Map<i64, i64> = Map::new()\n\
                  m.insert(1, 2)\n\
                  let a = m.iter().filter(|p| p.1 > 1).map(|p| p.0).sum()\n\
                  let b = m.values().max()\n\
                  let c = m.keys().take(1).count()\n\
                  let _ = a + c\n\
                  let _ = b\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        !names.iter().any(|name| name.starts_with("gos_rt_lazy_iter_")
            || name.starts_with("gos_rt_iter_")
            || name.starts_with("gos_rt_map_iter")
            || name.starts_with("gos_rt_map_values")),
        "a Map entry chain walks the entries in one loop: {names:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.name.starts_with("__closure_")),
        "fused stage closures are inlined, so none is lifted"
    );
}

#[test]
fn for_loops_over_positional_chains_walk_in_one_loop() {
    let source = "fn walk(data: [u8], n: i64) -> i64 {\n\
                  let mut acc = 0\n\
                  for (i, b) in data.iter().enumerate() { acc += i * (b as i64) }\n\
                  for i in (0..n).rev() { if i % 2 == 0 { continue }\n\
                  acc += i }\n\
                  'outer: for (i, x) in (0..n).enumerate() {\n\
                  for j in (0..8).rev().step_by(3) { if j == x { continue 'outer }\n\
                  acc += i + j } }\n\
                  acc\n\
                  }\n\
                  fn main() { let _ = walk(#[1, 2, 3], 5) }\n";
    let (bodies, _) = build_with_lift(source);
    let walk = bodies.iter().find(|b| b.name == "walk").expect("walk");
    let names = call_symbol_names(walk);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("gos_rt_lazy_iter_") || name.starts_with("gos_rt_iter_")),
        "a for loop over positional stages walks its source in one loop: {names:?}"
    );
}

#[test]
fn aggregate_unwrap_or_copies_the_payload_out_of_the_carrier() {
    let source = "use std::{option, result}\n\
                  struct Inner { a: i64, b: String }\n\
                  fn pick(i: i64) -> Option<Inner> { if i > 0 { Some(Inner { a: i, b: \"x\" }) } else { None } }\n\
                  fn load(i: i64) -> Result<Inner, errors::Error> { if i > 0 { Ok(Inner { a: i, b: \"y\" }) } else { Err(errors::new(\"no\")) } }\n\
                  fn main() {\n\
                  let a = pick(1).unwrap_or(Inner { a: 0, b: \"f\" })\n\
                  let b = load(1).unwrap_or(Inner { a: 0, b: \"g\" })\n\
                  let c = option::unwrap_or(pick(2), Inner { a: 0, b: \"h\" })\n\
                  let d = result::unwrap_or(load(2), Inner { a: 0, b: \"i\" })\n\
                  let _ = a.a + b.a + c.a + d.a\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        !names.iter().any(|name| name.starts_with("gos_rt_result_unwrap_or")
            || name.starts_with("gos_rt_result_default")
            || name.starts_with("gos_rt_option_default")),
        "an aggregate payload is copied out in place, not answered by a runtime unwrap: {names:?}"
    );
    let payload_reads = main
        .blocks
        .iter()
        .flat_map(|block| block.stmts.iter())
        .filter(|stmt| {
            matches!(&stmt.kind, StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } if *name == "gos_rt_result_payload")
        })
        .count();
    assert!(
        payload_reads >= 4,
        "each unwrap_or reads its carrier's payload in place: {payload_reads}"
    );
}

#[test]
fn generic_reader_shares_its_vec_when_the_element_is_a_copy() {
    let source = "fn count_above<T: Ord>(xs: Vec<T>, lo: T) -> i64 {\n\
                  xs.iter().filter(|v| v > lo).count()\n\
                  }\n\
                  fn ints(xs: Vec<i64>) -> i64 { count_above(xs, 1) }\n\
                  fn words(xs: Vec<String>) -> i64 { count_above(xs, \"m\") }\n\
                  fn main() {\n\
                  let _ = ints(#[1, 2]) + words(#[\"a\"])\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let clones = |name: &str| {
        let body = bodies.iter().find(|b| b.name == name).expect(name);
        call_names(body)
            .iter()
            .filter(|callee| callee.as_str() == "gos_rt_vec_clone")
            .count()
    };
    assert_eq!(
        clones("ints"),
        0,
        "an instantiation that reads copies of its elements shares the caller's Vec"
    );
    assert_eq!(
        clones("words"),
        1,
        "an element that is not a copy can reach the caller's storage, so the Vec is copied"
    );
}

#[test]
fn eager_chain_with_two_closures_keeps_its_stage_order() {
    let source = "fn main() {\n\
                  let xs = #[1, 2, 3, 4]\n\
                  let hit = xs.map(|v| v + 1).any(|v| v == 3)\n\
                  let _ = hit\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        names.iter().any(|name| name.starts_with("gos_rt_iter_map")),
        "an eager map finishes every element before `any` runs, so the chain is not fused: {names:?}"
    );
}

#[test]
fn lifted_closure_destructures_tuple_parameter_before_body() {
    let source = "fn main() {\n\
                  let values = #[1, 2, 3, 4]\n\
                  let shifted = values.iter().enumerate().map(|(i, value)| value + i)\n\
                  let _ = shifted\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        names.iter().any(|name| name == "gos_rt_lazy_iter_enumerate_i64"),
        "method-form enumerate must lower through the typed runtime shim: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "enumerate"),
        "method-form enumerate must not lower to an unresolved callee: {names:?}"
    );
    let lifted = bodies
        .iter()
        .find(|b| b.name.starts_with("__closure_"))
        .expect("lifted closure body");
    assert_eq!(lifted.arity, 1, "destructured closure has one tuple param");
    for expected in ["i", "value"] {
        assert!(
            lifted.locals.iter().any(|local| {
                local
                    .debug_name
                    .as_ref()
                    .is_some_and(|name| name.name == expected)
            }),
            "lifted closure must bind destructured local `{expected}`: {lifted:#?}"
        );
    }
}

#[test]
fn native_lowers_chunks_and_tuple_get_method_forms() {
    let source = "fn main() {\n\
                  let input = \"0222112222120000\"\n\
                  let layers = input.chars().chunks(4)\n\
                  let min_layer_idx = layers\n\
                  .iter()\n\
                  .map(|layer| layer.count_of('0'))\n\
                  .enumerate()\n\
                  .min_by_key(|t| t.1)\n\
                  .unwrap()\n\
                  .get(0)\n\
                  .unwrap()\n\
                  let pixels = (0..4).map(|idx| \"#\")\n\
                  pixels.chunks(2).iter().map(|chunk| chunk.join(\"\")).for_each(println)\n\
                  let _ = min_layer_idx\n\
                  }\n";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        names
            .iter()
            .any(|name| name == "gos_rt_iter_chunk_by_size_i64"),
        "method-form chunks must lower through the typed runtime shim: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name == "gos_rt_iter_min_by_key_ptr"),
        "min_by_key over enumerate tuples must use the aggregate runtime shim: {names:?}"
    );
    assert!(
        gos_fn_addr_targets(main)
            .iter()
            .any(|name| name == "gos_rt_println_fn_str_word"),
        "println as a String callback must lower to a callable runtime shim: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|name| name == "chunks" || name == "get" || name == "println"),
        "method forms and function values must not lower to unresolved callees: {names:?}"
    );
}

#[test]
fn result_map_err_free_call_lowers_to_runtime_shim() {
    // `result::map_err(f, r)` (the piped/free form) must lower to the
    // `gos_rt_result_map_err` shim instead of an undefined
    // `@result::map_err` symbol that fails the native link.
    let source = "use std::result\n\
                  fn fail() -> Result<i64, String> { Err(\"boom\") }\n\
                  fn main() { let r = fail() |> |v| result::map_err(|e| format(\"p: {e}\"), v)\n\
                  let _ = r }\n";
    let (bodies, _) = build_with_lift(source);
    let calls_shim = bodies
        .iter()
        .flat_map(call_names)
        .any(|sym| sym == "gos_rt_result_map_err");
    assert!(calls_shim, "expected a gos_rt_result_map_err call terminator");
}

// ---------------------------------------------------------------
// http::Response struct literals - must lower to the runtime
// constructor + setter chain on compiled tiers, never to the
// undefined `__struct` symbol (which fails the native build).
// ---------------------------------------------------------------

fn call_names(body: &gossamer_mir::Body) -> Vec<String> {
    body.blocks
        .iter()
        .filter_map(|b| match &b.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(n)),
                ..
            } => Some(n.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn string_plus_assign_flattens_rhs_concat_chain_to_in_place_appends() {
    let source = r#"
fn main() {
    let mut out = ""
    let mut i: i64 = 0
    while i < 3 {
        let name = "user-" + i.to_string()
        out += "{\"id\":" + i.to_string() + ",\"name\":\"" + name + "\"}"
        i += 1
    }
    println(out)
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "main").expect("main");

    let mut saw_in_place_append = false;
    for (block_idx, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if matches!(
            name.as_str(),
            "gos_rt_str_concat_drop_a"
                | "gos_rt_str_append_i64"
                | "gos_rt_str_append_f64"
                | "gos_rt_str_append_bytes"
        ) {
            saw_in_place_append = true;
        }
        if name == "__concat"
            && let Some(succ) = target
            && let Some(first) = body.blocks[succ.0 as usize].stmts.first()
            && matches!(
                &first.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                    ..
                } if src.local == destination.local && src.projection.is_empty()
            )
        {
            panic!("block {block_idx}: string accumulation lowered through __concat copy-back");
        }
    }
    assert!(
        saw_in_place_append,
        "expected string accumulation to use append runtime helpers"
    );
}

#[test]
fn scalar_static_mut_callee_does_not_block_auto_region() {
    let source = r"
enum Node {
    Leaf(i64),
    Pair(Node, Node),
}

static mut SEED: i64 = 1

fn rand() -> i64 {
    SEED = SEED * 6364136223846793005 + 1442695040888963407
    SEED
}

fn build(depth: i64) -> Node {
    let v = rand()
    if depth == 0 {
        return Node::Leaf(v)
    }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn count(n: Node) -> i64 {
    match n {
        Node::Leaf(_) => 1,
        Node::Pair(l, r) => 1 + count(l) + count(r),
    }
}

fn main() {
    let mut total = 0
    for _ in 0..3 {
        let tree = build(4)
        total += count(tree)
    }
}
";
    let (bodies, _) = build(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        names.iter().any(|n| n == "gos_rt_arena_push"),
        "allocating loop should be auto-regioned despite scalar static mut callee: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "gos_rt_arena_pop"),
        "allocating loop should close its auto-region: {names:?}"
    );
}

#[test]
fn wrapping_integer_methods_lower_without_runtime_calls_and_do_not_block_auto_region() {
    let source = r"
enum Node {
    Leaf(i64),
    Pair(Node, Node),
}

static mut SEED: i64 = 1

fn rand() -> i64 {
    SEED = SEED *% 6364136223846793005 +% 1442695040888963407
    SEED
}

fn build(depth: i64) -> Node {
    let v = rand()
    if depth == 0 {
        return Node::Leaf(v)
    }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn count(n: Node) -> i64 {
    match n {
        Node::Leaf(_) => 1,
        Node::Pair(l, r) => 1 + count(l) + count(r),
    }
}

fn main() {
    let mut total = 0
    for _ in 0..3 {
        let tree = build(4)
        total += count(tree)
    }
}
";
    let (bodies, _) = build(source);
    let rand = bodies.iter().find(|b| b.name == "rand").expect("rand");
    let mut saw_wrapping_add = false;
    let mut saw_wrapping_mul = false;
    for block in &rand.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { op, .. },
                ..
            } = &stmt.kind
            {
                saw_wrapping_add |= matches!(op, BinOp::WrappingAdd);
                saw_wrapping_mul |= matches!(op, BinOp::WrappingMul);
            }
        }
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            ..
        } = &block.terminator
        {
            assert!(
                !matches!(
                    name.as_str(),
                    "gos_rt_int_wrapping_add" | "gos_rt_int_wrapping_mul"
                ),
                "wrapping integer methods must not lower to runtime calls: {name}"
            );
        }
    }
    assert!(saw_wrapping_add, "rand must contain a WrappingAdd MIR op");
    assert!(saw_wrapping_mul, "rand must contain a WrappingMul MIR op");

    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        names.iter().any(|n| n == "gos_rt_arena_push"),
        "scalar wrapping methods in callees must not block auto-regioning: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "gos_rt_arena_pop"),
        "auto-regioned loop must close its arena: {names:?}"
    );
}

#[test]
fn cycle_collection_inside_auto_region_runs_after_arena_pop() {
    let source = r"
use std::runtime

enum Node {
    Leaf(i64),
    Pair(Node, Node),
}

fn build(depth: i64) -> Node {
    if depth == 0 { return Node::Leaf(1) }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn count(n: Node) -> i64 {
    match n {
        Node::Leaf(_) => 1,
        Node::Pair(l, r) => 1 + count(l) + count(r),
    }
}

fn main() {
    let mut total = 0
    for _ in 0..3 {
        let tree = build(4)
        total += count(tree)
        runtime::collect_cycles()
    }
}
";
    let (bodies, _) = build(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    let pop = names
        .iter()
        .position(|name| name == "gos_rt_arena_pop")
        .unwrap_or_else(|| panic!("auto region must emit arena_pop: {names:?}"));
    let collect = names
        .iter()
        .position(|name| name == "gos_rt_collect_cycles")
        .unwrap_or_else(|| panic!("collection must remain in the lowered program: {names:?}"));
    assert!(pop < collect, "collection must follow arena_pop: {names:?}");
}

#[test]
fn auto_regions_reject_early_exit_and_only_region_the_inner_nested_loop() {
    let early_exit = r"
enum Node { Leaf(i64), Pair(Node, Node) }

fn build(depth: i64) -> Node {
    if depth == 0 { return Node::Leaf(1) }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn main() {
    for i in 0..3 {
        let tree = build(3)
        if i == 1 { break }
        println(tree)
    }
}
";
    let (bodies, _) = build(early_exit);
    let main = bodies
        .iter()
        .find(|body| body.name == "main")
        .expect("main");
    let names = call_names(main);
    assert!(
        !names.iter().any(|name| name == "gos_rt_arena_push"),
        "an early exit must not leave an automatic region open: {names:?}"
    );

    let nested = r"
enum Node { Leaf(i64), Pair(Node, Node) }

fn build(depth: i64) -> Node {
    if depth == 0 { return Node::Leaf(1) }
    Node::Pair(build(depth - 1), build(depth - 1))
}

fn count(n: Node) -> i64 {
    match n {
        Node::Leaf(_) => 1,
        Node::Pair(left, right) => 1 + count(left) + count(right),
    }
}

fn main() {
    let mut total = 0
    for _ in 0..2 {
        for _ in 0..2 {
            let tree = build(3)
            total += count(tree)
        }
    }
    println(total)
}
";
    let (bodies, _) = build(nested);
    let main = bodies
        .iter()
        .find(|body| body.name == "main")
        .expect("main");
    let names = call_names(main);
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "gos_rt_arena_push")
            .count(),
        1,
        "only the allocation-owning inner loop may be regioned: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "gos_rt_arena_pop")
            .count(),
        1,
        "the one nested region must be closed exactly once: {names:?}"
    );
}

#[test]
fn nested_nonescaping_block_gets_one_automatic_region() {
    let source = r"
enum Node { Leaf(i64), Pair(Node, Node) }

fn count(n: Node) -> i64 {
    match n { Node::Leaf(_) => 1, Node::Pair(a, b) => count(a) + count(b) }
}

fn main() {
    let answer = {
        let tree = Node::Pair(Node::Leaf(1), Node::Leaf(2))
        count(tree)
    }
    let _ = answer
}
";
    let (bodies, _) = build(source);
    let main = bodies
        .iter()
        .find(|body| body.name == "main")
        .expect("main");
    let names = call_names(main);
    assert_eq!(
        names
            .iter()
            .filter(|name| *name == "gos_rt_arena_push")
            .count(),
        1,
        "nested nonescaping allocation block should be regioned: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| *name == "gos_rt_arena_pop")
            .count(),
        1,
        "nested nonescaping allocation block should close its region: {names:?}"
    );

    let escaping = r"
enum Node { Leaf(i64), Pair(Node, Node) }

fn main() {
    let tree = { Node::Pair(Node::Leaf(1), Node::Leaf(2)) }
    let _ = match tree { Node::Leaf(n) => n, Node::Pair(_, _) => 2 }
}
";
    let (bodies, _) = build(escaping);
    let main = bodies
        .iter()
        .find(|body| body.name == "main")
        .expect("main");
    let names = call_names(main);
    assert!(
        !names.iter().any(|name| name == "gos_rt_arena_push"),
        "a block result that escapes must never be regioned: {names:?}"
    );
}

#[test]
fn heap_static_mut_callee_still_blocks_auto_region() {
    let source = r#"
static mut HOLD: String = ""

fn make() -> String { "x" }

fn stash(s: String) {
    HOLD = s
}

fn main() {
    for _ in 0..3 {
        let s = make()
        stash(s)
    }
}
"#;
    let (bodies, _) = build(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let names = call_names(main);
    assert!(
        !names
            .iter()
            .any(|n| n == "gos_rt_arena_push" || n == "gos_rt_arena_pop"),
        "heap static mut write must remain region-unsafe: {names:?}"
    );
}

#[test]
fn http_response_literal_full_lowers_to_constructor_and_setters() {
    let source = "use std::http\n\
                  fn h() -> http::Response {\n\
                  http::Response { status: 201, body: \"x\", content_type: \"t\",\n\
                  headers: Vec::from([(\"a\", \"b\"), (\"c\", \"d\")]) } }\n";
    let (bodies, _) = build(source);
    let h = bodies.iter().find(|b| b.name == "h").expect("h body");
    let names = call_names(h);
    assert!(
        !names.iter().any(|n| n == "__struct"),
        "literal must not lower to __struct: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "gos_rt_http_response_text_new"),
        "expected text_new constructor: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| n == "gos_rt_http_response_set_content_type"),
        "expected content-type setter: {names:?}"
    );
    let with_header_count = names
        .iter()
        .filter(|n| n.as_str() == "gos_rt_http_response_with_header")
        .count();
    assert_eq!(
        with_header_count, 1,
        "Vec headers must lower through one loop body call: {names:?}"
    );
}

#[test]
fn http_response_literal_omitted_fields_use_constructor_defaults() {
    let source = "use std::http\n\
                  fn h() -> http::Response { http::Response { } }\n";
    let (bodies, _) = build(source);
    let h = bodies.iter().find(|b| b.name == "h").expect("h body");
    let names = call_names(h);
    assert!(
        !names.iter().any(|n| n == "__struct"),
        "literal must not lower to __struct: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "gos_rt_http_response_text_new"),
        "expected text_new constructor: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n == "gos_rt_http_response_set_content_type"),
        "omitted content_type keeps the text_new default: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n == "gos_rt_http_response_with_header"),
        "omitted headers attach nothing: {names:?}"
    );
}

#[test]
fn http_response_literal_dynamic_headers_emit_vec_loop() {
    let source = "use std::http\n\
                  fn h(hs: Vec<(String, String)>) -> http::Response {\n\
                  http::Response { status: 200, body: \"x\", headers: hs } }\n";
    let (bodies, _) = build(source);
    let h = bodies.iter().find(|b| b.name == "h").expect("h body");
    let names = call_names(h);
    assert!(
        !names.iter().any(|n| n == "__struct"),
        "literal must not lower to __struct: {names:?}"
    );
    for expected in [
        "gos_rt_vec_len",
        "gos_rt_vec_get_ptr",
        "gos_rt_http_response_with_header",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "dynamic header arrays loop via {expected}: {names:?}"
        );
    }
    let has_back_edge = h.blocks.iter().enumerate().any(|(i, b)| {
        matches!(&b.terminator, Terminator::Goto { target } if target.0 < u32::try_from(i).unwrap_or(0))
    });
    assert!(
        has_back_edge,
        "expected a loop back-edge over the header vec"
    );
}

#[test]
fn http_response_literal_byte_body_routes_through_set_body_bytes() {
    let source = "use std::http\n\
                  fn h() -> http::Response {\n\
                  http::Response { status: 200, body: [104u8, 105u8] } }\n";
    let (bodies, _) = build(source);
    let h = bodies.iter().find(|b| b.name == "h").expect("h body");
    let names = call_names(h);
    assert!(
        names
            .iter()
            .any(|n| n == "gos_rt_http_response_set_body_bytes"),
        "byte-array bodies route through set_body_bytes: {names:?}"
    );
}

#[test]
fn user_defined_response_struct_still_lowers_as_aggregate() {
    let source = "struct Response { status: i64 }\n\
                  fn h() -> Response { Response { status: 7 } }\n";
    let (bodies, _) = build(source);
    let h = bodies.iter().find(|b| b.name == "h").expect("h body");
    let names = call_names(h);
    assert!(
        !names.iter().any(|n| n == "gos_rt_http_response_text_new"),
        "a user Response struct must keep the aggregate lowering: {names:?}"
    );
    let has_aggregate = h.blocks.iter().flat_map(|b| b.stmts.iter()).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::Aggregate { .. },
                ..
            }
        )
    });
    assert!(
        has_aggregate,
        "expected an Aggregate assign for the user struct"
    );
}

// ---------------------------------------------------------------
// Task 22 - per-name combinator matrix: every closure-taking std
// combinator the checker has a signature row for must lower its
// free data-last call to a concrete gos_rt_* shim, never to an
// undefined `@module::name` symbol.
// ---------------------------------------------------------------

/// (label, source, expected shim) rows for the per-name matrix.
const COMBINATOR_MATRIX: &[(&str, &str, &str)] = &[
    (
        "result::and_then",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Ok(2)\n\
             let m = r |> |v| result::and_then(|x: i64| if x > 0 { Ok(x) } else { Err(errors::new(\"n\")) }, v)\nlet _ = m }",
        "gos_rt_result_and_then",
    ),
    (
        "result::or_else",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Err(errors::new(\"b\"))\n\
             let m = r |> |v| result::or_else(v, |_e| Ok(7))\nlet _ = m }",
        "gos_rt_result_or_else",
    ),
    (
        "result::ok",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Ok(4)\n\
             let m = r |> result::ok\nlet _ = m }",
        "gos_rt_result_to_opt_ok",
    ),
    (
        "result::err",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Ok(4)\n\
             let m = r |> result::err\nlet _ = m }",
        "gos_rt_result_to_opt_err",
    ),
    (
        "result::is_ok",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Ok(4)\n\
             let m = r |> result::is_ok\nlet _ = m }",
        "gos_rt_result_is_ok",
    ),
    (
        "result::is_err",
        "use std::{errors, result}\nfn main() { let r: Result<i64, errors::Error> = Ok(4)\n\
             let m = r |> result::is_err\nlet _ = m }",
        "gos_rt_result_is_err",
    ),
    (
        "option::and_then",
        "use std::option\nfn main() { let o: Option<i64> = Some(3)\n\
             let m = o |> |v| option::and_then(v, |x: i64| if x > 2 { Some(x) } else { None })\nlet _ = m }",
        "gos_rt_option_and_then",
    ),
    (
        "option::filter",
        "use std::option\nfn main() { let o: Option<i64> = Some(3)\n\
             let m = o |> |v| option::filter(v, |x: i64| x > 2)\nlet _ = m }",
        "gos_rt_option_filter",
    ),
    (
        "option::or",
        "use std::option\nfn main() { let o: Option<i64> = None\n\
             let m = o |> |v| option::or(v, Some(8))\nlet _ = m }",
        "gos_rt_option_or",
    ),
    (
        "option::or_else",
        "use std::option\nfn main() { let o: Option<i64> = None\n\
             let m = o |> |v| option::or_else(v, || Some(8))\nlet _ = m }",
        "gos_rt_option_or_else",
    ),
    (
        "option::unwrap_or_else",
        "use std::option\nfn main() { let o: Option<i64> = None\n\
             let v = o |> |v| option::unwrap_or_else(v, || 6)\nlet _ = v }",
        "gos_rt_option_default_with",
    ),
    (
        "option::flatten",
        "use std::option\nfn main() { let o: Option<Option<i64>> = Some(Some(4))\n\
             let m = o |> option::flatten\nlet _ = m }",
        "gos_rt_option_flatten",
    ),
    (
        "option::iter",
        "use std::option\nfn main() { let o: Option<i64> = Some(9)\n\
             let xs = o |> option::iter\nlet _ = xs }",
        "gos_rt_option_iter",
    ),
    (
        "option::is_some",
        "use std::option\nfn main() { let o: Option<i64> = Some(9)\n\
             let v = o |> option::is_some\nlet _ = v }",
        "gos_rt_option_is_some",
    ),
    (
        "iter::filter_map",
        "use std::iter\nfn main() { let xs = #[1, 2] |> |v| iter::filter_map(v, |x: i64| if x > 1 { Some(x) } else { None })\nlet _ = xs }",
        "gos_rt_iter_filter_map_i64",
    ),
    (
        "iter::collect",
        "use std::iter\nfn main() { let xs = iter::collect(#[1, 2].iter())\nlet _ = xs }",
        "gos_rt_lazy_iter_collect_i64",
    ),
    (
        "Vec::collect",
        "fn main() { let xs = Vec::from([1, 2]).iter().collect()\nlet _ = xs }",
        "gos_rt_lazy_iter_collect_i64",
    ),
    (
        "iter::once",
        "use std::iter\nfn main() { let xs = iter::once(7)\nlet _ = xs }",
        "gos_rt_iter_repeat_i64",
    ),
    (
        "iter::empty",
        "use std::iter\nfn main() { let xs: Vec<i64> = iter::empty()\nlet _ = xs }",
        "gos_rt_vec_with_capacity",
    ),
    (
        "iter::step_by",
        "use std::iter\nfn main() { let xs = #[1, 2, 3] |> |v| iter::step_by(v, 2)\nlet _ = xs }",
        "gos_rt_vec_step_by",
    ),
    (
        "iter::flat_map (fixed array literal)",
        "use std::iter\nfn main() { let xs = [1, 2] |> |v| iter::flat_map(v, |x: i64| [x, x * 10])\nlet _ = xs }",
        "gos_rt_iter_flat_map_arr_i64",
    ),
    (
        "iter::product_by",
        "use std::iter\nfn main() { let v = #[1, 2] |> |v| iter::product_by(v, |x: i64| x + 1)\nlet _ = v }",
        "gos_rt_iter_product_by_i64",
    ),
    (
        "iter::position",
        "use std::iter\nfn main() { let v = #[5, 6] |> |v| iter::position(v, |x: i64| x == 6)\nlet _ = v }",
        "gos_rt_iter_position_i64",
    ),
    (
        "iter::find_map",
        "use std::iter\nfn main() { let v = #[1, 2] |> |v| iter::find_map(v, |x: i64| if x > 1 { Some(x) } else { None })\nlet _ = v }",
        "gos_rt_iter_find_map_i64",
    ),
    (
        "iter::take_while",
        "use std::iter\nfn main() { let xs = #[1, 9] |> |v| iter::take_while(v, |x: i64| x < 5)\nlet _ = xs }",
        "gos_rt_iter_take_while_i64",
    ),
    (
        "iter::skip_while",
        "use std::iter\nfn main() { let xs = #[1, 9] |> |v| iter::skip_while(v, |x: i64| x < 5)\nlet _ = xs }",
        "gos_rt_iter_skip_while_i64",
    ),
    (
        "iter::partition",
        "use std::iter\nfn main() { let a, b = #[1, 2] |> |v| iter::partition(v, |x: i64| x % 2 == 0)\nlet _ = a\nlet _ = b }",
        "gos_rt_iter_partition_i64",
    ),
    (
        "iter::sort_by",
        "use std::iter\nfn main() { let xs = #[3, 1] |> |v| iter::sort_by(v, |a: i64, b: i64| a - b)\nlet _ = xs }",
        "gos_rt_iter_sorted_by_i64",
    ),
    (
        "iter::sort_by_key",
        "use std::iter\nfn main() { let xs = #[3, 1] |> |v| iter::sort_by_key(v, |x: i64| 0 - x)\nlet _ = xs }",
        "gos_rt_iter_sorted_by_key_i64",
    ),
    (
        "iter::min_by",
        "use std::iter\nfn main() { let v = #[3, 1] |> |v| iter::min_by(v, |a: i64, b: i64| a - b)\nlet _ = v }",
        "gos_rt_iter_min_by_i64",
    ),
    (
        "iter::max_by",
        "use std::iter\nfn main() { let v = #[3, 1] |> |v| iter::max_by(v, |a: i64, b: i64| a - b)\nlet _ = v }",
        "gos_rt_iter_max_by_i64",
    ),
    (
        "iter::min_by_key",
        "use std::iter\nfn main() { let v = #[3, 1] |> |v| iter::min_by_key(v, |x: i64| 0 - x)\nlet _ = v }",
        "gos_rt_iter_min_by_key_i64",
    ),
    (
        "iter::max_by_key",
        "use std::iter\nfn main() { let v = #[3, 1] |> |v| iter::max_by_key(v, |x: i64| 0 - x)\nlet _ = v }",
        "gos_rt_iter_max_by_key_i64",
    ),
    (
        "iter::chunk_by",
        "use std::iter\nfn main() { let m = #[1, 2] |> |v| iter::chunk_by(v, |x: i64| x % 2)\nlet _ = m }",
        "gos_rt_iter_group_by_i64",
    ),
    (
        "iter::count_by",
        "use std::iter\nfn main() { let m = #[1, 2] |> |v| iter::count_by(v, |x: i64| x % 2)\nlet _ = m }",
        "gos_rt_iter_count_by_i64",
    ),
];

/// `iter::reduce` and `iter::scan` over a sequence of scalars walk it in a
/// counted loop that calls the closure per element, with no runtime iterator
/// helper between the sequence and the closure.
#[test]
fn reduce_and_scan_free_calls_walk_the_sequence_inline() {
    for (label, source, shim) in [
        (
            "iter::reduce",
            "use std::iter\nfn main() { let v = #[1, 2] |> |v| iter::reduce(v, |a: i64, b: i64| a + b)\nlet _ = v }",
            "gos_rt_iter_reduce_i64",
        ),
        (
            "iter::scan",
            "use std::iter\nfn main() { let xs = #[1, 2] |> |v| iter::scan(v, 0, |a: i64, x: i64| a + x)\nlet _ = xs }",
            "gos_rt_iter_scan_i64",
        ),
    ] {
        let (bodies, _) = build_with_lift(source);
        let names: Vec<String> = bodies.iter().flat_map(call_names).collect();
        assert!(
            !names.iter().any(|n| n == shim),
            "{label}: no `{shim}` call: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "gos_rt_vec_get_i64_unchecked"),
            "{label}: the loop reads each element: {names:?}"
        );
    }
}

/// `option::zip` builds its pair in MIR and wraps it the way `Some((a, b))`
/// is, so the carrier holds a copy its own release reclaims: each operand's
/// discriminant is read, and no runtime zip is called.
#[test]
fn option_zip_free_call_builds_the_pair_inline() {
    let source = "use std::option\nfn main() { let a: Option<i64> = Some(1)\n\
                  let b: Option<i64> = Some(2)\nlet m = a |> |v| option::zip(v, b)\nlet _ = m }";
    let (bodies, _) = build_with_lift(source);
    let calls: Vec<String> = bodies.iter().flat_map(call_names).collect();
    assert!(
        !calls.iter().any(|n| n == "gos_rt_option_zip"),
        "no runtime zip is called: {calls:?}"
    );
    let disc_reads = bodies
        .iter()
        .flat_map(|b| b.blocks.iter())
        .flat_map(|b| b.stmts.iter())
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } if *name == "gos_rt_result_disc"
            )
        })
        .count();
    assert!(disc_reads >= 2, "both operands' arms are checked: {disc_reads} read(s)");
}

#[test]
fn combinator_free_calls_lower_to_runtime_shims() {
    let mut missing = Vec::new();
    for (label, source, shim) in COMBINATOR_MATRIX {
        let (bodies, _) = build_with_lift(source);
        assert!(
            bodies.iter().any(|b| b.name == "main"),
            "{label}: missing main body"
        );
        // A closure `|>` step lifts its body into its own MIR body, so the
        // shim is looked for across the program rather than in `main` alone.
        let names: Vec<String> = bodies.iter().flat_map(call_names).collect();
        let fresh_collect_elided = matches!(*label, "iter::collect" | "Vec::collect")
            && *shim == "gos_rt_vec_clone"
            && names.iter().any(|n| n == "gos_rt_vec_from_arr");
        if !(names.iter().any(|n| n == shim) || fresh_collect_elided) {
            missing.push(format!("{label}: expected `{shim}` call, got {names:?}"));
        }
        assert!(
            !names
                .iter()
                .any(|n| n.contains("::") && n != "Vec::new"),
            "{label}: undefined high-level callee leaked into MIR: {names:?}"
        );
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

// ---------------------------------------------------------------
// Task 22 - std fns as values (eta-expansion): a tabled std fn in
// a callable slot must resolve to its runtime symbol; the source
// path must not survive into MIR (it has no native symbol).
// ---------------------------------------------------------------

fn const_strings(body: &gossamer_mir::Body) -> Vec<String> {
    let mut out = Vec::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                match rvalue {
                    Rvalue::Use(Operand::Const(ConstValue::Str(s))) => out.push(s.clone()),
                    Rvalue::CallIntrinsic { args, .. } => {
                        for arg in args {
                            if let Operand::Const(ConstValue::Str(s)) = arg {
                                out.push(s.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // An eta-expanded std function reaches its runtime helper from the
        // lifted closure's own body, where the helper is the block's call
        // terminator rather than a statement operand.
        if let gossamer_mir::Terminator::Call { callee, args, .. } = &block.terminator {
            if let Operand::Const(ConstValue::Str(s)) = callee {
                out.push(s.clone());
            }
            for arg in args {
                if let Operand::Const(ConstValue::Str(s)) = arg {
                    out.push(s.clone());
                }
            }
        }
    }
    out
}

#[test]
fn std_fn_value_map_err_resolves_to_runtime_symbol() {
    let source = "use std::errors\n\
                  fn main() { let r: Result<i64, String> = Err(\"boom\")\n\
                  let m = r.map_err(errors::new)\nlet _ = m }";
    let (bodies, _) = build_with_lift(source);
    // The std function named in the callable slot becomes the closure the
    // call it stands for, so its runtime helper is reached from the lifted
    // body rather than from `main`.
    let strings: Vec<String> = bodies.iter().flat_map(const_strings).collect();
    assert!(
        strings.iter().any(|s| s == "gos_rt_error_new"),
        "expected the runtime symbol in MIR: {strings:?}"
    );
    assert!(
        !strings.iter().any(|s| s == "errors::new"),
        "source path must not leak into MIR: {strings:?}"
    );
}

#[test]
fn std_fn_value_iter_map_resolves_to_runtime_symbol() {
    let source = "use std::{iter, strings}\n\
                  fn main() { let out = #[\"ab\"] |> |v| iter::map(v, strings::to_uppercase)\nlet _ = out }";
    let (bodies, _) = build_with_lift(source);
    let strings: Vec<String> = bodies.iter().flat_map(const_strings).collect();
    assert!(
        strings.iter().any(|s| s == "gos_rt_str_to_upper"),
        "expected the runtime symbol in MIR: {strings:?}"
    );
    assert!(
        !strings.iter().any(|s| s == "strings::to_uppercase"),
        "source path must not leak into MIR: {strings:?}"
    );
}

#[test]
fn path_prefixes_free_function_lowers_to_runtime_symbol() {
    let source = "use std::path\n\
                  fn main() {\n\
                  let ps = path::prefixes(\"/a//b\")\n\
                  let ups = path::unique_prefixes(\"a/b\\na/c\\n\")\n\
                  let _ = ps\n\
                  let _ = ups\n\
                  }";
    let (bodies, _) = build_with_lift(source);
    let main = bodies.iter().find(|b| b.name == "main").expect("main");
    let strings = call_names(main);
    assert!(
        strings.iter().any(|s| s == "gos_rt_path_prefixes"),
        "expected the runtime symbol in MIR: {strings:?}"
    );
    assert!(
        strings
            .iter()
            .any(|s| s == "gos_rt_path_unique_prefixes"),
        "expected the bulk runtime symbol in MIR: {strings:?}"
    );
    assert!(
        !strings
            .iter()
            .any(|s| s == "path::prefixes" || s == "path::unique_prefixes"),
        "source path must not leak into MIR: {strings:?}"
    );
}

#[test]
fn rebindable_recursive_enum_borrow_has_distinct_non_owning_cursor_local() {
    let source = r"
enum ListNode { Link(ListNode, i64), End }

fn walk(n: i64) -> i64 {
    let mut head = ListNode::End
    for i in 0..n { head = ListNode::Link(head, i) }
    let mut count = 0
    let mut cursor = &head
    loop {
        match cursor {
            ListNode::Link(next, _) => { count += 1; cursor = next },
            ListNode::End => break,
        }
    }
    count
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|body| body.name == "walk").expect("walk");
    let named = |wanted: &str| {
        body.locals
            .iter()
            .enumerate()
            .find(|(_, local)| {
                local
                    .debug_name
                    .as_ref()
                    .is_some_and(|name| name.name.as_str() == wanted)
            })
            .map_or_else(
                || panic!("missing local {wanted}"),
                |(index, _)| Local(u32::try_from(index).unwrap()),
            )
    };
    let head = named("head");
    let cursor = named("cursor");
    assert_ne!(head, cursor, "a rebindable borrow must not alias its owner slot");
    assert!(body.blocks.iter().flat_map(|block| &block.stmts).any(|stmt| {
        matches!(
            &stmt.kind,
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Ref { place: borrowed, .. },
            } if place.local == cursor && borrowed.local == head
        )
    }));
    assert!(
        body.blocks.iter().flat_map(|block| &block.stmts).all(|stmt| {
            !matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } if matches!(*name, "gos_rt_rc_retain" | "gos_rt_rc_release")
                    && matches!(args.first(), Some(Operand::Copy(place)) if place.local == cursor)
            )
        }),
        "the pointer-valued cursor is a borrow, not another strong owner"
    );
    assert!(
        body.blocks.iter().flat_map(|block| &block.stmts).any(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } if *name == "gos_rt_rc_release"
                    && matches!(args.first(), Some(Operand::Copy(place)) if place.local == head)
            )
        }),
        "the pinned owner must still be released at function exit"
    );
}

/// Statement index of the first `name` intrinsic call in `block` whose
/// first argument is `local`.
fn intrinsic_index_on(
    block: &gossamer_mir::BasicBlock,
    name: &str,
    local: Local,
) -> Option<usize> {
    block.stmts.iter().position(|stmt| {
        matches!(
            &stmt.kind,
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name: n, args },
                ..
            } if *n == name
                && matches!(
                    args.first(),
                    Some(Operand::Copy(p)) if p.projection.is_empty() && p.local == local
                )
        )
    })
}

/// A `let` binding of an `Option` whose payload is a copy-blob takes its
/// own share at the copy, and the temporary the copy read is dead right
/// after it. The retain that hands the binding its share must run before
/// the temporary's release: with a sole reference the other order frees
/// the payload and then retains the freed node, so a later read of the
/// binding reaches reused memory.
#[test]
fn drop_pass_retains_option_binding_before_releasing_the_moved_temporary() {
    let source = r#"
fn read_map(m: Map<String, i64>) -> String {
    let found = m.find(|(_, n)| n > 1)
    let kept = m.filter(|(_, n)| n > 1)
    format("find {:?} kept {}", found, kept.len())
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "read_map").expect("body");

    let mut checked = 0;
    for block in &body.blocks {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
            else {
                continue;
            };
            if !place.projection.is_empty() || !src.projection.is_empty() {
                continue;
            }
            let Some(release) = intrinsic_index_on(block, "gos_rt_option_slot_release", src.local)
            else {
                continue;
            };
            if release <= si {
                continue;
            }
            let retain = intrinsic_index_on(block, "gos_rt_option_slot_retain", place.local)
                .expect("the copy target takes its own share");
            assert!(
                retain < release,
                "the option binding must be retained before the temporary it copied is released"
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the option-returning traversal must lower to a copy the drop pass accounts for"
    );
}

const READ_ONLY_PARAM_SOURCE: &str = r#"
fn read_at(xs: Vec<i64>, i: i64) -> i64 { xs[i] }
fn hand_back(xs: Vec<i64>) -> Vec<i64> { xs }
fn grow(mut xs: Vec<i64>) -> i64 { xs.push(1); xs.len() }

fn main() {
    let items = #[1, 2, 3]
    println("{}", read_at(items, 1))
    println("{}", grow(items))
    let same = hand_back(items)
    println("{}", same.len())
}
"#;

/// Names of the intrinsics `body` calls, in order.
fn intrinsic_calls(body: &gossamer_mir::Body) -> Vec<String> {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } => Some((*name).to_string()),
            _ => None,
        })
        .chain(body.blocks.iter().filter_map(|b| match &b.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(s)),
                ..
            } => Some(s.clone()),
            _ => None,
        }))
        .collect()
}

#[test]
fn a_read_only_container_argument_crosses_without_a_copy() {
    // Value semantics only need independent storage where the callee can
    // write the parameter or let something derived from it outlive the call.
    // Copying unconditionally made passing a collection cost its length.
    let (bodies, _) = build(READ_ONLY_PARAM_SOURCE);
    let main = bodies
        .iter()
        .find(|b| b.name == "main")
        .expect("main body");
    let clones = intrinsic_calls(main)
        .iter()
        .filter(|name| name.as_str() == "gos_rt_vec_clone")
        .count();
    assert_eq!(
        clones, 2,
        "one copy for the `mut` parameter and one for the callee that hands \
         the value back; the read-only call needs none",
    );
}

const CARRIER_PAYLOAD_SOURCE: &str = r#"
fn make(n: i64) -> Result<Vec<i64>, String> {
    if n > 0 { Ok(#[n, n + 1]) } else { Err("empty") }
}

fn main() {
    let mut total = 0
    let mut i = 0
    while i < 4 {
        match make(i) {
            Ok(v) => total += v.len()
            Err(_e) => total += 0
        }
        i += 1
    }
    println("{}", total)
}
"#;

#[test]
fn a_sequence_payload_bound_out_of_a_carrier_is_released() {
    // The carrier itself frees nothing and a GosVec carries no RC header, so
    // the binding the payload is matched into is its only owner. Without a
    // release there, every fallible call in a loop kept its result.
    let (bodies, _) = build(CARRIER_PAYLOAD_SOURCE);
    let main = bodies
        .iter()
        .find(|b| b.name == "main")
        .expect("main body");
    let calls = intrinsic_calls(main);
    assert!(
        calls.iter().any(|name| name == "gos_rt_result_payload"),
        "the match arm reads the payload out of the carrier: {calls:?}",
    );
    // The binding is reclaimed either by its own release or, when the loop
    // body is regioned, wholesale at the arena pop.
    assert!(
        calls
            .iter()
            .any(|name| name == "gos_rt_vec_free" || name == "gos_rt_arena_pop"),
        "the payload binding must be reclaimed: {calls:?}",
    );
}

#[test]
fn a_sequence_wrapped_into_a_returned_carrier_mints_no_extra_share() {
    // The wrap hands the payload's share to the caller with the carrier. A
    // retain there is a share the frame never gets back, since the local's own
    // release is suppressed for the same reason.
    let (bodies, _) = build(CARRIER_PAYLOAD_SOURCE);
    let make = bodies
        .iter()
        .find(|b| b.name == "make")
        .expect("make body");
    let calls = intrinsic_calls(make);
    assert!(
        calls.iter().any(|name| name == "gos_rt_result_new"),
        "make wraps its sequence in a carrier: {calls:?}",
    );
    assert!(
        !calls.iter().any(|name| name == "gos_rt_vec_retain"),
        "no share is minted for a payload the caller takes over: {calls:?}",
    );
}

/// A by-value aggregate parameter the body only reads takes no share of its
/// own: the caller holds every field for the whole call, so the entry retain
/// and the return release would cancel. A `Map` field's retain is a copy of
/// the whole table (a `GosMap` has no reference count), so an accessor that
/// keeps one costs the map's size on every call.
#[test]
fn read_only_by_value_aggregate_param_borrows_its_map_field() {
    let source = r"
struct K {
    m: Map<i64, i64>
    pos: i64
}

impl K {
    fn read(self, k: i64) -> i64 {
        self.m.get_or(k, 0)
    }
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "K::read").expect("body");
    let receiver = Local(1);
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_map_field_clone", receiver, 0),
        0,
        "a read-only receiver must borrow the caller's table, not copy it"
    );
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_map_field_release", receiver, 0),
        0,
        "nothing was minted for the receiver, so nothing is released"
    );
}

/// A by-value aggregate parameter the body WRITES keeps its own share: the
/// store lands on the callee's value, and the caller's map is untouched.
#[test]
fn written_by_value_aggregate_param_owns_its_map_field() {
    let source = r"
struct K {
    m: Map<i64, i64>
    pos: i64
}

fn bump(mut k: K) -> i64 {
    k.m.insert(1, 2)
    k.m.len()
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "bump").expect("body");
    let param = Local(1);
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_map_field_clone", param, 0),
        1,
        "a written parameter takes one table of its own at entry"
    );
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_map_field_release", param, 0),
        1,
        "and gives it back at the return"
    );
}

/// A struct copy takes storage of its own for every container field, not just
/// the `Map` one: a `Set`, a `Deque`, and a heap each reach their elements
/// through a handle that carries no count of its holders, so two structs
/// naming one store would see each other's writes and free it twice.
#[test]
fn struct_copy_clones_every_container_field() {
    let source = r"
struct Bag {
    set: Set<i64>
    deque: Deque<i64>
    heap: MinHeap<i64>
}

fn copy_it() -> i64 {
    let a = Bag { set: #{1}, deque: Deque::from([1]), heap: MinHeap::from([1]) }
    let mut b = a
    b.set.insert(2)
    a.set.len() + b.set.len()
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "copy_it").expect("body");
    for helper in [
        "gos_rt_set_field_clone",
        "gos_rt_deque_field_clone",
        "gos_rt_bheap_field_clone",
    ] {
        assert!(
            call_symbol_names(body).iter().any(|n| n == helper),
            "a struct copy must give each container field storage of its own ({helper})"
        );
    }
    for helper in [
        "gos_rt_set_field_release",
        "gos_rt_deque_field_release",
        "gos_rt_bheap_field_release",
    ] {
        assert!(
            call_symbol_names(body).iter().any(|n| n == helper),
            "and the storage it took has to be freed ({helper})"
        );
    }
}

/// A by-value parameter the body WRITES owns each container field it carries,
/// exactly as it owns a `Map` one.
#[test]
fn written_by_value_param_owns_every_container_field() {
    let source = r"
struct Bag {
    set: Set<i64>
    deque: Deque<i64>
}

fn fill(mut b: Bag) -> i64 {
    b.set.insert(2)
    b.set.len() + b.deque.len()
}
";
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "fill").expect("body");
    let param = Local(1);
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_set_field_clone", param, 0),
        1,
        "a written parameter takes one set of its own at entry"
    );
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_deque_field_clone", param, 1),
        1,
        "and one deque of its own"
    );
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_set_field_release", param, 0),
        1,
        "both given back at the return"
    );
    assert_eq!(
        field_rc_calls_on(body, "gos_rt_deque_field_release", param, 1),
        1,
        "both given back at the return"
    );
}

/// A payload read out of a container and wrapped in `Ok(..)` hands the caller a
/// share of its own.
///
/// The wrapped payload is the copy blob the lookup answered, whose fields the
/// blob holds counted shares of, so the return slot takes a share of the blob
/// rather than one per field. A sibling match arm whose expression is a call
/// leaves the body's other ownership sets empty, and the share is still taken.
#[test]
fn wrapped_container_payload_retains_its_fields_without_other_owned_locals() {
    let source = r#"
struct Def { name: String, cols: Vec<String> }
struct Eng { tables: Map<String, Def> }

fn get_def(eng: Eng, table: String) -> Result<Def, String> {
    match eng.tables.get(table) {
        Some(d) => return Ok(d),
        None => return Err("no such table: " + table),
    }
}
"#;
    let (bodies, _) = build(source);
    let body = bodies.iter().find(|b| b.name == "get_def").expect("body");
    assert!(
        rc_calls_on(body, "gos_rt_option_slot_retain", Local::RETURN) >= 1,
        "the returned carrier takes a share of the payload blob for the caller"
    );
    assert_eq!(
        projected_rc_calls(body, "gos_rt_rc_retain", 0)
            + projected_rc_calls(body, "gos_rt_vec_retain", 1),
        0,
        "the blob holds its fields' shares, so no field is retained on its own"
    );
}

/// Calls to `name` whose argument is `<some local>.field`.
fn projected_rc_calls(body: &gossamer_mir::Body, name: &str, field: u32) -> usize {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name: n, args },
                    ..
                } if *n == name
                    && matches!(
                        args.first(),
                        Some(Operand::Copy(p))
                            if p.projection.as_slice()
                                == [gossamer_mir::Projection::Field(field)]
                    )
            )
        })
        .count()
}

/// The `gos_rc_alloc` meta symbols a body builds environments with.
fn closure_env_metas(body: &gossamer_mir::Body) -> Vec<String> {
    body.blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } if *name == "gos_rc_alloc" => match args.get(1) {
                Some(Operand::Const(ConstValue::Str(sym))) => Some(sym.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A closure capturing a multi-slot struct holds it as a counted box its
/// environment lists as a child, so sharing, release, and teardown walks over
/// the environment reach the struct's `String` fields.
#[test]
fn closure_environment_owns_a_boxed_struct_capture() {
    let source = r#"
struct App { dir: String, web: String }

fn make(base: String) -> Fn() -> String {
    let app = App { dir: base + "/data", web: base + "/web" }
    let f = || app.dir + "/LOCK"
    f
}
"#;
    let (bodies, tcx) = build_with_lift(source);
    let body = bodies.iter().find(|b| b.name == "make").expect("body");
    assert!(
        call_symbol_names(body).iter().any(|n| n == "gos_rt_enum_box_aggr"),
        "the struct capture is boxed with its child meta"
    );
    let metas = closure_env_metas(body);
    let meta = metas
        .iter()
        .find(|sym| sym.starts_with("gos_rc_meta_closure_"))
        .expect("the environment carries a layout meta");
    let blob = tcx.rc_meta(meta).expect("registered meta");
    assert_eq!(
        blob,
        &[gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, 1, 1],
        "the capture word is an RC child of the environment"
    );
}

/// A struct sent on a channel marks each counted field shared in place before
/// it is enqueued, whichever lowering produced the send.
#[test]
fn channel_send_of_a_struct_marks_its_fields_shared() {
    let source = r#"
use std::sync::channel

struct App { dir: String, n: i64, tags: Vec<String> }

fn main() {
    let tx, rx = channel()
    let app = App { dir: format("{}", 1), n: 2, tags: #[format("{}", 3)] }
    tx.send(app)
    tx.close()
    let _ = rx.recv()
}
"#;
    let (bodies, tcx) = build(source);
    let body = bodies.iter().find(|b| b.name == "main").expect("body");
    let marker = body
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .find_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } if *name == "gos_rt_aggr_mark_shared_children" => match args.get(1) {
                Some(Operand::Const(ConstValue::Str(sym))) => Some(sym.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the send marks the struct's fields shared");
    let blob = tcx.rc_meta(&marker).expect("registered meta");
    let rc = gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT;
    let vec = gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT;
    assert_eq!(
        blob,
        &[gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, 2, rc, vec | 2],
        "the String at word 0 and the Vec at word 2 are marked"
    );
}
