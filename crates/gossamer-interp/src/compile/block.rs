#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_block(&mut self, block: &HirBlock) -> RuntimeResult<BlockResult> {
        self.compile_block_inner(block, false)
    }

    /// Compiles a block whose tail value is discarded (a loop body or a
    /// statement-position block). The tail is compiled in statement
    /// context, so a discardable in-place mutation (`v.push(x)`) lowers
    /// to its dedicated op rather than the value-returning builtin path.
    pub(crate) fn compile_loop_body(&mut self, body: &HirExpr) -> RuntimeResult<()> {
        if let HirExprKind::Block(block) = &body.kind {
            self.compile_block_inner(block, true)?;
        } else {
            self.compile_expr_discarded(body)?;
        }
        Ok(())
    }

    pub(crate) fn compile_block_inner(
        &mut self,
        block: &HirBlock,
        tail_discarded: bool,
    ) -> RuntimeResult<BlockResult> {
        self.push_scope();
        self.defer_stack.push(Vec::new());
        let clear_after_stmt = crate::compile::consume::block_last_use_clears(block);
        let mut diverges = false;
        for (idx, stmt) in block.stmts.iter().enumerate() {
            let register_mark = self.register_mark();
            if self.compile_stmt(stmt)? {
                diverges = true;
            }
            if !diverges && let Some(names) = clear_after_stmt.get(idx) {
                self.emit_last_use_clears(names);
            }
            // Expression and goroutine statements cannot introduce a local
            // visible to a later statement. Spawn/call/container operations
            // clone or transfer every escaping value before the instruction
            // completes, so their compiler temporaries are dead here.
            if !diverges && matches!(&stmt.kind, HirStmtKind::Expr { .. }) {
                self.restore_register_mark(register_mark);
            }
        }
        let result = if diverges {
            BlockResult::Diverges
        } else if let Some(tail) = &block.tail {
            // A discarded tail (loop body / statement-position block) is
            // compiled in statement context: an assignment lowers to its
            // store and an in-place Vec mutation to its dedicated op, so
            // neither leaves the dead `LoadConst(Unit)` its expression form
            // would yield - the per-iteration unit nothing reads.
            if tail_discarded {
                self.compile_expr_discarded(tail)?;
                BlockResult::Unit
            } else {
                let reg = self.compile_expr(tail)?;
                BlockResult::ValueIn(reg)
            }
        } else {
            BlockResult::Unit
        };
        // Block-scoped `defer`: on a normal (non-diverging) exit, run this
        // block's deferred expressions LIFO after its value is computed. A
        // diverging block already emitted the pending frames at the
        // `return` / `break` / `continue` edge via `emit_defers_above`, so
        // it must not re-emit here. Deferred expressions allocate fresh
        // registers, so the result register stays intact.
        let frame = self.defer_stack.pop().unwrap_or_default();
        if !diverges {
            self.emit_defer_frame(&frame)?;
        }
        self.pop_scope();
        Ok(result)
    }

    fn emit_last_use_clears(&mut self, names: &[String]) {
        for name in names {
            let Some(typed) = self.lookup_local(name) else {
                continue;
            };
            if typed.kind == RegKind::Value && !self.reference_alias_regs.contains(&typed.reg) {
                self.emit(Op::ClearRegs {
                    start: typed.reg,
                    count: 1,
                });
            }
        }
    }
}

#[cfg(test)]
mod elide_unit_load_tests {
    use std::collections::{HashMap, HashSet};

    use gossamer_hir::{HirExprKind, HirFn, HirItemKind, lower_source_file};
    use gossamer_lex::SourceMap;
    use gossamer_parse::{parse_source_file, synthesize_entry_main};
    use gossamer_resolve::resolve_source_file;
    use gossamer_types::{TyCtxt, typecheck_source_file};

    use crate::bytecode::{FnChunk, Op};
    use crate::value::Value;

    /// Compiles a single named function from `source` to its bytecode
    /// chunk, driving the full front-end with an empty compile context
    /// (no struct layouts / inlinable wrappers / consts).
    fn compile_named(source: &str, fn_name: &str) -> (FnChunk, HirFn) {
        let mut map = SourceMap::new();
        let file = map.add_file("test.gos", source.to_string());
        let (mut sf, parse_diags) = parse_source_file(source, file);
        assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
        let entry_diags = synthesize_entry_main(&mut sf);
        assert!(entry_diags.is_empty(), "entry main: {entry_diags:?}");
        let (resolutions, _) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
        let mut tcx = TyCtxt::new();
        let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
        let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
        let decl = program
            .items
            .iter()
            .find_map(|item| match &item.kind {
                HirItemKind::Fn(decl) if decl.name.name.as_str() == fn_name => Some(decl.clone()),
                _ => None,
            })
            .expect("named function not found");
        let chunk = super::compile_fn(
            &decl,
            &tcx,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &super::ConstValues::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            None,
            None,
            None,
        )
        .expect("compile_fn");
        (chunk, decl)
    }

    /// `true` when `op` loads a `Value::Unit` from the constant pool.
    fn is_unit_load(op: &Op, chunk: &FnChunk) -> bool {
        matches!(op, Op::LoadConst { idx, .. } if matches!(chunk.consts.get(*idx as usize), Some(Value::Unit)))
    }

    /// Index of the loop's back-edge: the jump that targets an earlier
    /// instruction. The loop body is everything before it.
    fn back_edge_idx(chunk: &FnChunk) -> usize {
        chunk
            .instrs
            .iter()
            .enumerate()
            .find_map(|(idx, op)| match op {
                Op::Jump { target } if (*target as usize) < idx => Some(idx),
                _ => None,
            })
            .expect("loop back-edge jump not found")
    }

    #[test]
    fn discarded_flat_vec_swap_uses_allocation_free_opcode() {
        let source = r"
fn flip(mut values: Vec<i64>, a: i64, b: i64) -> i64 {
    values.swap(a, b)
    let _ = values.swap(b, a)
    values[0]
}
";
        let (chunk, _) = compile_named(source, "flip");
        assert_eq!(
            chunk
                .instrs
                .iter()
                .filter(|op| matches!(op, Op::IntArraySwap { .. }))
                .count(),
            2,
            "discarded scalar Vec swap should not allocate a Result: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::VecSwap { .. })),
            "discarded swap used value-producing opcode: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn assignment_tail_in_loop_body_emits_no_dead_unit() {
        let source = r"
fn count(n: i64) -> i64 {
    let mut i = 0
    while i < n {
        i = i + 1
    }
    i
}
";
        let (chunk, decl) = compile_named(source, "count");

        // Precondition: the while body's tail really is the assignment,
        // so this test exercises the discarded-tail path - not the
        // statement path, which already elided the unit.
        let body = decl.body.as_ref().expect("body");
        let while_expr = body
            .block
            .stmts
            .iter()
            .find_map(|stmt| match &stmt.kind {
                gossamer_hir::HirStmtKind::Expr { expr, .. } => match &expr.kind {
                    HirExprKind::While { body, .. } => Some(body.as_ref()),
                    _ => None,
                },
                _ => None,
            })
            .expect("while statement");
        let HirExprKind::Block(while_body) = &while_expr.kind else {
            panic!("while body is not a block");
        };
        let tail = while_body.tail.as_ref().expect("while body tail");
        assert!(
            matches!(tail.kind, HirExprKind::Assign { .. }),
            "expected the loop body tail to be an assignment"
        );

        // The optimization: the compiled loop body (everything before the
        // back-edge jump) contains no dead `LoadConst(Unit)`. Pre-fix the
        // assignment tail materialised one such unit per loop body.
        let back_edge = back_edge_idx(&chunk);
        let units_in_body = chunk.instrs[..back_edge]
            .iter()
            .filter(|op| is_unit_load(op, &chunk))
            .count();
        assert_eq!(
            units_in_body, 0,
            "loop body should emit no dead unit load; chunk: {:?}",
            chunk.instrs
        );

        // The increment itself must survive: removing the dead unit must
        // not remove the `i + 1` add the loop body still needs.
        let has_add = chunk.instrs[..back_edge].iter().any(|op| {
            matches!(
                op,
                Op::AddInt { .. }
                    | Op::AddI64 { .. }
                    | Op::CheckedAddI64 { .. }
                    | Op::ArithImmI64 {
                        kind: crate::bytecode::ImmArithKind::Add,
                        ..
                    }
            )
        });
        assert!(
            has_add,
            "loop body lost its `i + 1` computation; chunk: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn block_value_bound_to_let_still_materialises() {
        // A block whose value IS used (bound to a `let`) must still
        // produce its value, so its tail is not elided.
        let source = r"
fn pick(flag: bool) -> i64 {
    let v = { if flag { 10 } else { 20 } }
    v
}
";
        let (chunk, _) = compile_named(source, "pick");
        // The block tail is an `if`-as-value; a real value register must
        // flow into `v`. The function must end by returning a value, not
        // unit.
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::Return { .. })),
            "value-producing block must return a value; chunk: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn block_clears_value_local_after_last_statement_use() {
        let source = r#"
fn f() {
    let s = "abcdef"
    let n = s.len()
    println("{}", n)
}
"#;
        let (chunk, _) = compile_named(source, "f");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::ClearRegs { count: 1, .. })),
            "expected a statement-level last-use clear; chunk: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn effect_statement_temporaries_reuse_physical_registers() {
        let one = r#"
fn f() {
    println("{}", (1, 2))
}
"#;
        let mut many = String::from("fn f() {\n");
        for _ in 0..24 {
            many.push_str("    println(\"{}\", (1, 2))\n");
        }
        many.push_str("}\n");

        let (one_chunk, _) = compile_named(one, "f");
        let (many_chunk, _) = compile_named(&many, "f");
        assert!(
            many_chunk.register_count <= one_chunk.register_count.saturating_add(2),
            "effect-only statements should share one temporary register span: one={}, many={}, instrs={:?}",
            one_chunk.register_count,
            many_chunk.register_count,
            many_chunk.instrs
        );
    }

    #[test]
    fn top_level_block_clears_value_local_after_last_statement_use() {
        let source = r#"
let s = "abcdef"
let n = s.len()
println("{}", n)
"#;
        let (chunk, _) = compile_named(source, "main");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::ClearRegs { count: 1, .. })),
            "expected top-level statement last-use clear; chunk: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn large_float_repeat_uses_runtime_repeat_not_register_expansion() {
        let source = r"
fn f() -> f64 {
    let xs: [f64; 40000] = [0.0; 40000]
    xs[39999]
}
";
        let (chunk, _) = compile_named(source, "f");
        assert!(
            chunk.float_count < 64,
            "large scalar repeat must not reserve one float register per element; \
             float_count={} instrs={:?}",
            chunk.float_count,
            chunk.instrs
        );
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::BuildArrayRepeat { .. })),
            "large scalar repeat should lower to BuildArrayRepeat; chunk: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::BuildFloatVec { count, .. } if *count > 1024)),
            "large scalar repeat must not expand through BuildFloatVec; chunk: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn i64_struct_fields_read_directly_into_integer_registers() {
        let source = r"
struct Cursor { pos: i64, limit: i64 }
fn advance(c: Cursor) -> i64 { c.pos + c.limit }
";
        let (chunk, _) = compile_named(source, "advance");
        let typed_reads = chunk
            .instrs
            .iter()
            .filter(|op| matches!(op, Op::FieldGetI64 { .. } | Op::FieldGetI64ByOffset { .. }))
            .count();
        assert_eq!(
            typed_reads, 2,
            "both i64 fields should bypass boxed Value reads: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::UnboxI64 { .. })),
            "typed field reads must not be followed by UnboxI64: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn two_i64_positional_constructor_skips_generic_struct_call() {
        let source = r"
struct Pair { left: i64, right: i64 }
fn make(a: i64, b: i64) -> Pair { Pair(a, b) }
";
        let (chunk, _) = compile_named(source, "make");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::Struct2I64 { .. })),
            "two-integer positional constructor must use Struct2I64: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk.instrs.iter().any(|op| matches!(op, Op::Call { .. })),
            "direct constructor must not retain a generic call: {:?}",
            chunk.instrs
        );
    }

    /// `Point::new(a, b)` and `Point(a, b)` both carry the struct's
    /// `DefId`, but only the second is a construction. The associated
    /// function must be called, so its body decides the field values.
    #[test]
    fn an_associated_function_is_called_not_treated_as_a_constructor() {
        let source = r"
struct Point { x: i64, y: i64 }
impl Point { fn new(a: i64, b: i64) -> Point { Point { x: a * 10, y: b * 10 } } }
fn build(a: i64, b: i64) -> Point { Point::new(a, b) }
";
        let (chunk, _) = compile_named(source, "build");
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::Struct2I64 { .. })),
            "an associated function must not be packed into a struct: {:?}",
            chunk.instrs
        );
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::Call { .. } | Op::CallGlobal { .. })),
            "the associated function's body must still be called: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn nullary_enum_constructor_skips_generic_call() {
        let source = r"
enum Tree { Leaf, Node(Tree, Tree) }
fn make_leaf() -> Tree { Tree::Leaf }
";
        let (chunk, _) = compile_named(source, "make_leaf");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::LoadGlobal { .. })),
            "nullary constructor should load its canonical sentinel: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk.instrs.iter().any(|op| matches!(op, Op::Call { .. })),
            "nullary constructor must not retain generic dispatch: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn payload_enum_constructor_skips_generic_call() {
        let source = r"
enum Tree { Leaf, Node(Tree, Tree) }
fn node(left: Tree, right: Tree) -> Tree { Tree::Node(left, right) }
";
        let (chunk, _) = compile_named(source, "node");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::BuildVariant2 { .. })),
            "payload constructor should use BuildVariant2: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk.instrs.iter().any(|op| matches!(op, Op::Call { .. })),
            "payload constructor must not retain generic dispatch: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn i64_to_f64_divisor_uses_fused_typed_opcode() {
        let source = r"
fn recip(i: i64) -> f64 {
    1.0 / (i as f64)
}
";
        let (chunk, _) = compile_named(source, "recip");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::DivF64ByI64 { .. })),
            "expected fused DivF64ByI64: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .windows(2)
                .any(|ops| matches!(ops, [Op::IntToFloatF64 { .. }, Op::DivF64 { .. }])),
            "IntToFloatF64 + DivF64 pair should be fused: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn fma_accumulator_does_not_emit_dead_float_move() {
        let source = r"
fn step(a: f64, b: f64, c: f64) -> f64 {
    let mut sum = c
    sum += a * b
    sum
}
";
        let (chunk, _) = compile_named(source, "step");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::MulAddF64 { .. })),
            "expected MulAddF64: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk.instrs.windows(2).any(|ops| matches!(
                ops,
                [
                    Op::MulAddF64 { dst_f, .. } | Op::MulSubF64 { dst_f, .. },
                    Op::MoveF64 { src_f, .. }
                ] if dst_f == src_f
            )),
            "post-FMA MoveF64 should be folded into the fused op: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn local_string_character_pushes_use_in_place_opcode() {
        let source = r"
fn build() -> String {
    let mut s = String::with_capacity(16)
    s.push('a')
    s.push_char('b')
    s.push_byte(33)
    s
}
";
        let (chunk, _) = compile_named(source, "build");
        assert_eq!(
            chunk
                .instrs
                .iter()
                .filter(|op| matches!(op, Op::StrPush { .. }))
                .count(),
            3,
            "all local character pushes should mutate the receiver directly: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn string_byte_checksum_uses_fused_typed_opcode() {
        let source = r"
fn checksum(s: String) -> i64 {
    let mut sum: i64 = 0
    let mut i: i64 = 0
    while i < s.len() {
        sum = sum +% s.byte_at(i)
        i = i +% 1
    }
    sum
}
";
        let (chunk, _) = compile_named(source, "checksum");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::StrByteAtAddI64 { .. })),
            "expected fused StrByteAtAddI64: {:?}",
            chunk.instrs
        );
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::StrLenI64 { .. })),
            "expected typed StrLenI64: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::MethodCall { .. })),
            "string checksum loop should not retain generic method dispatch: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn negative_wrapping_immediate_is_one_typed_opcode() {
        let (chunk, _) = compile_named("fn dec(value: i64) -> i64 { value +% -1 }\n", "dec");
        assert!(
            chunk.instrs.iter().any(|op| matches!(
                op,
                Op::ArithImmI64 {
                    kind: crate::bytecode::ImmArithKind::Add,
                    imm: -1,
                    ..
                }
            )),
            "expected one immediate wrapping add: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::NegI64 { .. })),
            "negative immediate should not materialize a negation: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn integer_parameters_stay_in_the_typed_register_file() {
        let (chunk, _) = compile_named("fn twice(value: i64) -> i64 { value + value }\n", "twice");
        assert_eq!(chunk.i64_params.len(), 1);
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::UnboxI64 { .. })),
            "typed integer parameter should not be repeatedly unboxed: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn last_use_shared_borrow_transfers_aggregate_handle() {
        let source = r"
struct Item { value: i64 }
fn read(item: Item) -> i64 { item.value }
fn consume(item: Item) -> i64 { read(item) }
";
        let (chunk, _) = compile_named(source, "consume");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::MoveConsume { .. })),
            "last-use shared borrow should transfer its aggregate handle: {:?}",
            chunk.instrs
        );
    }

    #[test]
    fn statically_named_function_call_skips_global_load() {
        let source = r"
fn add_one(value: i64) -> i64 { value + 1 }
fn caller(value: i64) -> i64 { add_one(value) }
";
        let (chunk, _) = compile_named(source, "caller");
        assert!(
            chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::CallGlobal { .. })),
            "static function path should use CallGlobal: {:?}",
            chunk.instrs
        );
        assert!(
            !chunk
                .instrs
                .iter()
                .any(|op| matches!(op, Op::LoadGlobal { .. } | Op::Call { .. })),
            "static call must not materialize a callable name: {:?}",
            chunk.instrs
        );
    }
}
