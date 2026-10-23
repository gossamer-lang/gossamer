//! End-to-end tests for AST → HIR lowering.

use gossamer_hir::{
    HirBinaryOp, HirExprKind, HirItemKind, HirPatKind, HirStmtKind, lower_source_file,
};
use gossamer_lex::SourceMap;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn lower(source: &str) -> (gossamer_hir::HirProgram, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    // The compile/analysis entry, not the raw parser: it folds an entry
    // file's bare top-level statements into the implicit `fn main`, which
    // every tier reaches through here. Lowering the raw parse would see no
    // items at all for a program written without an explicit main.
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    (program, tcx)
}

#[test]
fn simple_function_lowers_to_hir_fn() {
    let (program, _tcx) = lower("fn main() {}\n");
    assert_eq!(program.items.len(), 1);
    let HirItemKind::Fn(f) = &program.items[0].kind else {
        panic!("expected fn");
    };
    assert_eq!(f.name.name, "main");
    assert!(f.body.is_some());
    assert!(!f.has_self);
}

#[test]
fn pipe_into_a_bare_callable_appends_the_piped_argument() {
    let (program, _tcx) =
        lower("fn wrap(a: i32) -> i32 { a }\n\nfn caller(x: i32) -> i32 { x |> wrap }\n");
    let tail = tail_of(&program, "caller");
    match &tail.kind {
        HirExprKind::Call { args, .. } => {
            assert_eq!(args.len(), 1, "the piped value is the only argument");
            match &args[0].kind {
                HirExprKind::Path { segments, .. } => assert_eq!(segments[0].name, "x"),
                other => panic!("unexpected argument: {other:?}"),
            }
        }
        other => panic!("pipe did not rewrite to call: {other:?}"),
    }
}

#[test]
fn a_closure_step_lowers_to_the_call_it_stands_for() {
    // The closure is a spelling of the call, so the step binds its parameter
    // in the caller's frame rather than building a closure to invoke.
    let (program, _tcx) = lower(
        "fn wrap(a: i32, b: i32) -> i32 { a }\n\nfn caller(x: i32) -> i32 { x |> |v| wrap(0i32, v) }\n",
    );
    let tail = tail_of(&program, "caller");
    let HirExprKind::Block(block) = &tail.kind else {
        panic!("expected the step's block: {:?}", tail.kind);
    };
    assert_eq!(block.stmts.len(), 1, "one binding for the piped value");
    let HirStmtKind::Let {
        init: Some(init), ..
    } = &block.stmts[0].kind
    else {
        panic!("expected a let binding: {:?}", block.stmts[0].kind);
    };
    match &init.kind {
        HirExprKind::Path { segments, .. } => assert_eq!(segments[0].name, "x"),
        other => panic!("the binding takes the piped value: {other:?}"),
    }
    let tail = block.tail.as_ref().expect("body tail");
    assert!(
        matches!(tail.kind, HirExprKind::Call { .. }),
        "the body is the call as written: {:?}",
        tail.kind
    );
}

#[test]
fn a_closure_step_whose_body_returns_keeps_its_closure() {
    // `return` targets the closure, so splicing the body into the caller
    // would return from the wrong function.
    let (program, _tcx) = lower("fn caller(x: i32) -> i32 { x |> |v| { return v } }\n");
    let tail = tail_of(&program, "caller");
    match &tail.kind {
        HirExprKind::Call { callee, args } => {
            assert!(
                matches!(callee.kind, HirExprKind::Closure { .. }),
                "the step stays a closure: {:?}",
                callee.kind
            );
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected a closure call: {other:?}"),
    }
}

/// The tail expression of the named function in `program`.
fn tail_of<'a>(program: &'a gossamer_hir::HirProgram, name: &str) -> &'a gossamer_hir::HirExpr {
    let f = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == name => Some(f),
            _ => None,
        })
        .expect("function lowered");
    f.body
        .as_ref()
        .expect("body")
        .block
        .tail
        .as_ref()
        .expect("tail present")
}

#[test]
fn try_operator_lowers_to_match() {
    let (program, _tcx) =
        lower("fn main() -> i32 { let x = ok()?\n    x }\nfn ok() -> i32 { 0i32 }\n");
    let main = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "main" => Some(f),
            _ => None,
        })
        .expect("main lowered");
    let body = main.body.as_ref().unwrap();
    let let_init = match &body.block.stmts[0].kind {
        HirStmtKind::Let { init, .. } => init.as_ref().unwrap(),
        other => panic!("expected let: {other:?}"),
    };
    match &let_init.kind {
        HirExprKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 2);
            match &arms[0].pattern.kind {
                HirPatKind::Variant { name, .. } => assert_eq!(name.name, "Ok"),
                other => panic!("unexpected Ok arm: {other:?}"),
            }
            match &arms[1].pattern.kind {
                HirPatKind::Variant { name, .. } => assert_eq!(name.name, "Err"),
                other => panic!("unexpected Err arm: {other:?}"),
            }
        }
        other => panic!("try did not lower to match: {other:?}"),
    }
}

#[test]
fn unknown_trait_bound_emits_diagnostic() {
    // `fn f<T: Hashabel>(...)` declares a bound on an unknown
    // trait. The typechecker should surface a `GT0011
    // unknown-trait-bound` diagnostic at declaration time so the
    // typo doesn't slip past to a runtime "no method" error.
    let mut map = SourceMap::new();
    let source = "fn need_hash<T: Hashabel>(x: T) -> T { x }\n";
    let file = map.add_file("bound.gos", source.to_string());
    let (mut sf, parse_diags) = gossamer_parse::parse_source_file(source, file);
    assert!(parse_diags.is_empty());
    let (mut resolutions, _) = gossamer_resolve::resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (_table, diags) = gossamer_types::typecheck_source_file(&sf, &resolutions, &mut tcx);
    let found = diags.iter().any(|d| d.error.code() == "GT0011");
    assert!(
        found,
        "expected GT0011 unknown-trait-bound for `Hashabel`, got: {diags:?}",
    );
}

#[test]
fn known_builtin_trait_bound_is_accepted() {
    // `Iterator` is a built-in trait name; a bound on it must
    // NOT produce an unknown-trait diagnostic.
    let mut map = SourceMap::new();
    let source = "fn collect<T: Iterator>(it: T) -> T { it }\n";
    let file = map.add_file("bound.gos", source.to_string());
    let (mut sf, parse_diags) = gossamer_parse::parse_source_file(source, file);
    assert!(parse_diags.is_empty());
    let (mut resolutions, _) = gossamer_resolve::resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (_table, diags) = gossamer_types::typecheck_source_file(&sf, &resolutions, &mut tcx);
    let any_unknown_bound = diags.iter().any(|d| d.error.code() == "GT0011");
    assert!(
        !any_unknown_bound,
        "Iterator bound should be accepted, got: {diags:?}",
    );
}

#[test]
fn try_on_option_lowers_to_some_none_match() {
    // `?` applied to an Option-returning expression desugars to
    // `Some(v) => v, None => return None`. Pre-fix this fell
    // through to the Ok/Err arms and produced nonsense.
    let (program, _tcx) = lower(
        "fn maybe() -> Option<i64> { Some(7) }\n\
         fn caller() -> Option<i64> { let x = maybe()?\n    Some(x) }\n",
    );
    let caller = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "caller" => Some(f),
            _ => None,
        })
        .expect("caller lowered");
    let body = caller.body.as_ref().unwrap();
    let let_init = match &body.block.stmts[0].kind {
        HirStmtKind::Let { init, .. } => init.as_ref().unwrap(),
        other => panic!("expected let: {other:?}"),
    };
    match &let_init.kind {
        HirExprKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 2);
            match &arms[0].pattern.kind {
                HirPatKind::Variant { name, .. } => {
                    assert_eq!(name.name, "Some", "first arm should be Some");
                }
                other => panic!("unexpected first arm: {other:?}"),
            }
            match &arms[1].pattern.kind {
                HirPatKind::Variant { name, .. } => {
                    assert_eq!(name.name, "None", "second arm should be None");
                }
                other => panic!("unexpected second arm: {other:?}"),
            }
        }
        other => panic!("`?` on Option did not lower to Some/None match: {other:?}"),
    }
}

#[test]
fn for_loop_lowers_to_loop_plus_match() {
    let (program, _tcx) = lower("fn main() { for x in 0..10 { let y = x } }\n");
    let main = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "main" => Some(f),
            _ => None,
        })
        .expect("main lowered");
    let body = main.body.as_ref().unwrap();
    let tail = body.block.tail.as_ref().expect("tail present");
    match &tail.kind {
        HirExprKind::Loop { body, .. } => match &body.kind {
            HirExprKind::Block(block) => {
                let inner_tail = block.tail.as_ref().expect("loop tail");
                match &inner_tail.kind {
                    HirExprKind::Match { arms, .. } => {
                        assert_eq!(arms.len(), 2);
                        match &arms[1].body.kind {
                            HirExprKind::Break { .. } => {}
                            other => panic!("None arm should break: {other:?}"),
                        }
                    }
                    other => panic!("expected match in loop: {other:?}"),
                }
            }
            other => panic!("expected block: {other:?}"),
        },
        other => panic!("for did not lower to loop: {other:?}"),
    }
}

#[test]
fn binary_ops_round_trip_through_lowering() {
    let (program, _tcx) = lower("fn add(a: i32, b: i32) -> i32 { a + b }\n");
    let add = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "add" => Some(f),
            _ => None,
        })
        .expect("add lowered");
    let tail = add
        .body
        .as_ref()
        .and_then(|body| body.block.tail.as_ref())
        .expect("tail present");
    match &tail.kind {
        HirExprKind::Binary { op, .. } => assert_eq!(*op, HirBinaryOp::Add),
        other => panic!("unexpected expr kind: {other:?}"),
    }
}

#[test]
fn every_lowered_expr_has_a_type() {
    let (program, tcx) = lower("fn add(a: i32, b: i32) -> i32 { a + b }\n");
    let add = program
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == "add" => Some(f),
            _ => None,
        })
        .expect("add lowered");
    let tail = add
        .body
        .as_ref()
        .and_then(|body| body.block.tail.as_ref())
        .expect("tail present");
    assert!(
        tcx.kind(tail.ty).is_some(),
        "tail ty was not interned by this ctx"
    );
}

#[test]
fn example_programs_lower_without_panics() {
    for name in ["hello_world.gos", "line_count.gos", "web_server.gos"] {
        let path = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).expect("read example");
        let (program, _tcx) = lower(&source);
        assert!(!program.items.is_empty(), "{path}: no items lowered");
    }
}

/// The types of the capture prologue's `let` bindings in the lifted
/// function `name`, in environment-slot order.
fn capture_slot_tys(source: &str, name: &str) -> (Vec<gossamer_types::Ty>, TyCtxt) {
    let (hir, mut tcx) = lower(source);
    let lifted = gossamer_hir::lift_closures(hir, &mut tcx);
    let decl = lifted
        .items
        .iter()
        .find_map(|item| match &item.kind {
            gossamer_hir::HirItemKind::Fn(decl) if decl.name.name == name => Some(decl),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no lifted item named {name}"));
    let block = &decl.body.as_ref().expect("a lifted body").block;
    let tys = block
        .stmts
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            gossamer_hir::HirStmtKind::Let {
                ty,
                init: Some(init),
                ..
            } if gossamer_hir::is_capture_env_load(init) => Some(*ty),
            _ => None,
        })
        .collect();
    (tys, tcx)
}

/// A capture's environment slot is typed by the value it holds, whatever
/// expression the capture is reached through. Typing it by anything else
/// (the closure's return type, say) lays out and reference-counts an
/// `i64` as a `String`, which the compiled tiers fault on.
#[test]
fn a_capture_reached_through_an_aggregate_keeps_its_own_type() {
    // `y` is an i64 and each closure returns a String, so a slot typed
    // from the return would hold `y`'s bits under a String's contract.
    for source in [
        // Tuple.
        "fn main() { let y = 5\n let f = |x: i64| { let t = (x, y)\n if t.0 == 1 { \"#\" } else { \" \" } }\n let _ = f(1) }\n",
        // Vec literal.
        "fn main() { let y = 5\n let f = |x: i64| { let v = #[x, y]\n if v[0] == 1 { \"#\" } else { \" \" } }\n let _ = f(1) }\n",
        // Range.
        "fn main() { let y = 5\n let f = |x: i64| { let r = x..y\n if x == 1 { \"#\" } else { \" \" } }\n let _ = f(1) }\n",
    ] {
        let (tys, mut tcx) = capture_slot_tys(source, "__closure_0.main");
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        assert_eq!(tys, vec![i64_ty], "{source}");
    }
}

/// Captures of different types keep them apart, so each env slot is laid
/// out independently of the others and of the return type.
#[test]
fn captures_of_mixed_types_each_keep_their_own() {
    let source = "fn main() { let scale = 3\n let bias = 0.5\n let label = \"n\"\n \
                  let f = |x: i64| { let t = (x, scale, bias, label)\n t.1 }\n let _ = f(1) }\n";
    let (tys, mut tcx) = capture_slot_tys(source, "__closure_0.main");
    let expected = vec![
        tcx.int_ty(gossamer_types::IntTy::I64),
        tcx.float_ty(gossamer_types::FloatTy::F64),
        tcx.string_ty(),
    ];
    assert_eq!(tys, expected);
}
