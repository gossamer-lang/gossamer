#[cfg(test)]
mod elision_tests {
    use gossamer_lex::{SourceMap, Span};
    use gossamer_types::TyCtxt;

    use super::{
        bounds_check_elim, elide_borrowed_holder_rc, elide_moved_aggregate_shares,
        elide_redundant_rc_pairs, elide_settled_guarded_walks,
        elide_vec_clone_of_fresh_temporary,
        fuse_slice_parse_ranges, local_branch_bounds_check_elim, loop_body_has_exactly_one_vec_push,
        reserve_bound_available_at_entry, reserve_vecs_for_counted_push_loops,
        scalar_replace_short_lived_aggregates,
    };
    use crate::ir::{
        BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Projection,
        Rvalue, Statement, StatementKind, Terminator,
    };

    fn span() -> Span {
        let mut map = SourceMap::new();
        Span::new(map.add_file("t.gos", ""), 0, 0)
    }

    fn decl(ty: gossamer_types::Ty) -> LocalDecl {
        LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        }
    }

    fn assign(place: Place, rvalue: Rvalue) -> Statement {
        Statement {
            kind: StatementKind::Assign { place, rvalue },
            span: span(),
        }
    }

    fn copy(dst: u32, src: u32) -> Statement {
        assign(
            Place::local(Local(dst)),
            Rvalue::Use(Operand::Copy(Place::local(Local(src)))),
        )
    }

    fn rc_call(dst: u32, name: &'static str, arg: Place) -> Statement {
        assign(
            Place::local(Local(dst)),
            Rvalue::CallIntrinsic {
                name,
                args: vec![Operand::Copy(arg)],
            },
        )
    }

    fn is_nop(s: &Statement) -> bool {
        matches!(s.kind, StatementKind::Nop)
    }

    #[test]
    fn elides_clone_of_fresh_vec_result_payload() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let u8t = tcx.int_ty(IntTy::U8);
        let i64t = tcx.int_ty(IntTy::I64);
        let vec_u8 = tcx.intern(gossamer_types::TyKind::Vec(u8t));
        let sp = span();
        let mut body = Body {
            name: "fresh_vec_result_payload".into(),
            def: None,
            arity: 0,
            locals: vec![
                decl(unit),   // return
                decl(i64t),   // array pointer placeholder
                decl(i64t),   // result from byte array slice
                decl(vec_u8), // payload extracted from result
                decl(vec_u8), // binding copied from payload
                decl(vec_u8), // cloned binding
                decl(unit),   // retain unit
            ],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_bytearr_slice_result".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Const(ConstValue::Int(32)),
                            Operand::Const(ConstValue::Int(0)),
                            Operand::Const(ConstValue::Int(32)),
                        ],
                        destination: Place::local(Local(2)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![
                        assign(
                            Place::local(Local(3)),
                            Rvalue::CallIntrinsic {
                                name: "gos_rt_result_payload",
                                args: vec![Operand::Copy(Place::local(Local(2)))],
                            },
                        ),
                        copy(4, 3),
                    ],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_clone".to_string())),
                        args: vec![Operand::Copy(Place::local(Local(4)))],
                        destination: Place::local(Local(5)),
                        target: Some(BlockId(2)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        elide_vec_clone_of_fresh_temporary(&mut body, &tcx);

        assert!(matches!(
            body.blocks[1].terminator,
            Terminator::Goto {
                target: BlockId(2)
            }
        ));
        assert!(matches!(
            &body.blocks[1].stmts[2].kind,
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } if *name == "gos_rt_vec_retain"
                && args == &[Operand::Copy(Place::local(Local(4)))]
        ));
        assert!(matches!(
            &body.blocks[1].stmts[3].kind,
            StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(source)),
            } if *place == Place::local(Local(5)) && *source == Place::local(Local(4))
        ));
    }

    #[test]
    fn scalar_replaces_non_escaping_field_only_aggregate() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let i64t = tcx.int_ty(IntTy::I64);
        let pair = tcx.intern(gossamer_types::TyKind::Tuple(vec![i64t, i64t]));
        let mut body = Body {
            name: "scalar_replace".into(),
            def: None,
            arity: 0,
            locals: vec![decl(i64t), decl(i64t), decl(i64t), decl(pair), decl(i64t)],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        Place::local(Local(1)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(10))),
                    ),
                    assign(
                        Place::local(Local(2)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(20))),
                    ),
                    assign(
                        Place::local(Local(3)),
                        Rvalue::Aggregate {
                            kind: crate::AggregateKind::Tuple,
                            operands: vec![
                                Operand::Copy(Place::local(Local(1))),
                                Operand::Copy(Place::local(Local(2))),
                            ],
                        },
                    ),
                    assign(
                        Place::local(Local(4)),
                        Rvalue::Use(Operand::Copy(Place {
                            local: Local(3),
                            projection: vec![Projection::Field(1)],
                        })),
                    ),
                    copy(0, 4),
                ],
                terminator: Terminator::Return,
                span: span(),
            }],
            span: span(),
        };

        scalar_replace_short_lived_aggregates(&mut body);

        assert!(is_nop(&body.blocks[0].stmts[2]));
        assert!(matches!(
            &body.blocks[0].stmts[3].kind,
            StatementKind::Assign {
                rvalue: Rvalue::Use(Operand::Copy(place)),
                ..
            } if *place == Place::local(Local(2))
        ));
    }

    #[test]
    fn scalar_replacement_preserves_construction_time_snapshot() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let i64t = tcx.int_ty(IntTy::I64);
        let pair = tcx.intern(gossamer_types::TyKind::Tuple(vec![i64t, i64t]));
        let mut body = Body {
            name: "scalar_replace_snapshot".into(),
            def: None,
            arity: 0,
            locals: vec![decl(i64t), decl(i64t), decl(i64t), decl(pair), decl(i64t)],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        Place::local(Local(1)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(10))),
                    ),
                    assign(
                        Place::local(Local(2)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(20))),
                    ),
                    assign(
                        Place::local(Local(3)),
                        Rvalue::Aggregate {
                            kind: crate::AggregateKind::Tuple,
                            operands: vec![
                                Operand::Copy(Place::local(Local(1))),
                                Operand::Copy(Place::local(Local(2))),
                            ],
                        },
                    ),
                    assign(
                        Place::local(Local(1)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(99))),
                    ),
                    assign(
                        Place::local(Local(4)),
                        Rvalue::Use(Operand::Copy(Place {
                            local: Local(3),
                            projection: vec![Projection::Field(0)],
                        })),
                    ),
                ],
                terminator: Terminator::Return,
                span: span(),
            }],
            span: span(),
        };

        scalar_replace_short_lived_aggregates(&mut body);

        assert!(
            !is_nop(&body.blocks[0].stmts[2]),
            "rewriting through a later source mutation would lose the snapshot"
        );
    }

    #[test]
    fn scalar_replacement_keeps_aggregate_used_by_successor_block() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let i64t = tcx.int_ty(IntTy::I64);
        let pair = tcx.intern(gossamer_types::TyKind::Tuple(vec![i64t, i64t]));
        let mut body = Body {
            name: "scalar_replace_successor_use".into(),
            def: None,
            arity: 0,
            locals: vec![decl(i64t), decl(i64t), decl(i64t), decl(pair), decl(i64t)],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![
                        assign(
                            Place::local(Local(1)),
                            Rvalue::Use(Operand::Const(ConstValue::Int(10))),
                        ),
                        assign(
                            Place::local(Local(2)),
                            Rvalue::Use(Operand::Const(ConstValue::Int(20))),
                        ),
                        assign(
                            Place::local(Local(3)),
                            Rvalue::Aggregate {
                                kind: crate::AggregateKind::Tuple,
                                operands: vec![
                                    Operand::Copy(Place::local(Local(1))),
                                    Operand::Copy(Place::local(Local(2))),
                                ],
                            },
                        ),
                    ],
                    terminator: Terminator::Goto {
                        target: BlockId(1),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::Use(Operand::Copy(Place {
                            local: Local(3),
                            projection: vec![Projection::Field(1)],
                        })),
                    )],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        };

        scalar_replace_short_lived_aggregates(&mut body);

        assert!(
            !is_nop(&body.blocks[0].stmts[2]),
            "an aggregate read in a successor block must remain materialized"
        );
    }

    fn intrinsic_name(s: &Statement) -> Option<&str> {
        match &s.kind {
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            } => Some(name),
            _ => None,
        }
    }

    /// A single block: `holder = Copy(x); <mid>; retain(x); <between>;
    /// release(x); <tail>` and a `Return`. `x` is `Local(1)` (a String),
    /// `holder` is `Local(2)`.
    fn body_with(
        tcx: &mut TyCtxt,
        mid: Vec<Statement>,
        between: Vec<Statement>,
        tail: Vec<Statement>,
    ) -> Body {
        let unit = tcx.unit();
        let s = tcx.string_ty();
        let locals = vec![
            decl(unit), // L0 return
            decl(s),    // L1 x
            decl(s),    // L2 holder
            decl(s),    // L3 spare String
            decl(unit), // L4 retain dest
            decl(unit), // L5 release dest
        ];
        let mut stmts = vec![copy(2, 1)];
        stmts.extend(mid);
        stmts.push(rc_call(4, "gos_rt_rc_retain", Place::local(Local(1))));
        stmts.extend(between);
        stmts.push(rc_call(5, "gos_rt_rc_release", Place::local(Local(1))));
        stmts.extend(tail);
        Body {
            name: "t".into(),
            def: None,
            arity: 0,
            locals,
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts,
                terminator: Terminator::Return,
                span: span(),
            }],
            span: span(),
        }
    }

    /// `L2 = Copy(L1.0); retain(L2); <mid>; L3 = gos_rt_vec_len(L2)` then
    /// `release(L2); Return` in a second block. `L1` is a by-value
    /// `(Vec<i64>, i64)` parameter and `L2` the holder its Vec is read through.
    fn holder_body(tcx: &mut TyCtxt, mid: Vec<Statement>) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let vec_ty = tcx.intern(gossamer_types::TyKind::Vec(i64_ty));
        let pair = tcx.intern(gossamer_types::TyKind::Tuple(vec![vec_ty, i64_ty]));
        let locals = vec![
            decl(unit),   // L0 return
            decl(pair),   // L1 parameter
            decl(vec_ty), // L2 holder
            decl(i64_ty), // L3 len
            decl(unit),   // L4 retain dest
            decl(unit),   // L5 release dest
            decl(vec_ty), // L6 another Vec
        ];
        let mut stmts = vec![assign(
            Place::local(Local(2)),
            Rvalue::Use(Operand::Copy(Place {
                local: Local(1),
                projection: vec![Projection::Field(0)],
            })),
        )];
        stmts.push(rc_call(4, "gos_rt_vec_retain", Place::local(Local(2))));
        stmts.extend(mid);
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts,
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".into())),
                        args: vec![Operand::Copy(Place::local(Local(2)))],
                        destination: Place::local(Local(3)),
                        target: Some(BlockId(1)),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![rc_call(5, "gos_rt_vec_free", Place::local(Local(2)))],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    #[test]
    fn elides_share_of_holder_that_borrows_a_field() {
        let mut tcx = TyCtxt::new();
        let mut body = holder_body(&mut tcx, vec![]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert!(is_nop(&body.blocks[0].stmts[1]), "retain of the holder should go");
        assert!(is_nop(&body.blocks[1].stmts[0]), "release of the holder should go");
        assert!(
            matches!(body.blocks[0].stmts[0].kind, StatementKind::Assign { .. }),
            "the borrowing copy itself stays"
        );
    }

    #[test]
    fn keeps_share_of_holder_when_its_source_is_written_before_the_read() {
        let mut tcx = TyCtxt::new();
        // `L1.0 = Copy(L6)` replaces the Vec the holder borrows before the
        // holder is read, so the holder's own share is what keeps it alive.
        let overwrite = assign(
            Place {
                local: Local(1),
                projection: vec![Projection::Field(0)],
            },
            Rvalue::Use(Operand::Copy(Place::local(Local(6)))),
        );
        let mut body = holder_body(&mut tcx, vec![overwrite]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(intrinsic_name(&body.blocks[0].stmts[1]), Some("gos_rt_vec_retain"));
        assert_eq!(intrinsic_name(&body.blocks[1].stmts[0]), Some("gos_rt_vec_free"));
    }

    /// `L2 = Copy(L1.0); vec_retain(L2.0); f(L2)` then `vec_free(L2.0); Return`.
    /// `L1` is a `&mut Outer` parameter and `L2` a copy of its `Inner` field,
    /// whose `Vec` field is what carries the copy's share.
    fn field_holder_body(tcx: &mut TyCtxt, mid: Vec<Statement>) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let vec_ty = tcx.intern(gossamer_types::TyKind::Vec(i64_ty));
        let inner = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(1),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_struct_fields(gossamer_resolve::DefId::local(1), vec![vec_ty, i64_ty]);
        let outer = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(2),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_struct_fields(gossamer_resolve::DefId::local(2), vec![inner, i64_ty]);
        let outer_ref = tcx.intern(gossamer_types::TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner: outer,
        });
        let field = |local: u32| Place {
            local: Local(local),
            projection: vec![Projection::Field(0)],
        };
        let locals = vec![
            decl(unit),      // L0 return
            decl(outer_ref), // L1 &mut Outer
            decl(inner),     // L2 holder
            decl(i64_ty),    // L3 call result
            decl(unit),      // L4 retain dest
            decl(unit),      // L5 release dest
            decl(inner),     // L6 spare Inner
        ];
        let mut stmts = vec![assign(
            Place::local(Local(2)),
            Rvalue::Use(Operand::Copy(field(1))),
        )];
        stmts.push(rc_call(4, "gos_rt_vec_retain", field(2)));
        stmts.extend(mid);
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts,
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("Inner::get".into())),
                        args: vec![Operand::Copy(Place::local(Local(2)))],
                        destination: Place::local(Local(3)),
                        target: Some(BlockId(1)),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![rc_call(5, "gos_rt_vec_free", field(2))],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    /// A statement calling `name(Copy(local), Const(meta))`, the shape a
    /// guarded aggregate's copy-blob walk takes.
    fn walk_call(dst: u32, name: &'static str, local: u32, meta: &str) -> Statement {
        assign(
            Place::local(Local(dst)),
            Rvalue::CallIntrinsic {
                name,
                args: vec![
                    Operand::Copy(Place::local(Local(local))),
                    Operand::Const(ConstValue::Str(meta.to_string())),
                ],
            },
        )
    }

    /// `L2 = Copy(L1.0); aggr_retain_children(L2, meta); f(L2)` then
    /// `aggr_release_children(L2, meta); Return`. `L1` is a tuple parameter
    /// holding the guarded struct the holder copies.
    fn guarded_holder_body(tcx: &mut TyCtxt, meta: &str) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let node = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(7),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_aggr_copy_meta(node, "gos_rc_meta_copyblob_1");
        let pair = tcx.intern(gossamer_types::TyKind::Tuple(vec![node, i64_ty]));
        let locals = vec![
            decl(unit),   // L0 return
            decl(pair),   // L1 parameter
            decl(node),   // L2 holder
            decl(i64_ty), // L3 call result
            decl(unit),   // L4 retain dest
            decl(unit),   // L5 release dest
        ];
        let stmts = vec![
            assign(
                Place::local(Local(2)),
                Rvalue::Use(Operand::Copy(Place {
                    local: Local(1),
                    projection: vec![Projection::Field(0)],
                })),
            ),
            walk_call(4, "gos_rt_aggr_retain_children", 2, "gos_rc_meta_copyblob_1"),
        ];
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts,
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("Node::sym".into())),
                        args: vec![Operand::Copy(Place::local(Local(2)))],
                        destination: Place::local(Local(3)),
                        target: Some(BlockId(1)),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![walk_call(5, "gos_rt_aggr_release_children", 2, meta)],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    /// `zero(L2); release(L2); L2 = <mint>; retain(L2); L3 = L2; retain(L3);
    /// release(L2); zero(L2)` then a return sweep releasing both.
    fn moved_aggregate_body(tcx: &mut TyCtxt) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let node = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(31),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_aggr_copy_meta(node, "gos_rc_meta_copyblob_5");
        let meta = "gos_rc_meta_copyblob_5";
        let locals = vec![
            decl(unit),   // L0 return
            decl(i64_ty), // L1 parameter
            decl(node),   // L2 the minted value
            decl(node),   // L3 where it moves
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
        ];
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![
                        walk_call(4, "gos_rt_aggr_zero_guarded", 2, meta),
                        walk_call(5, "gos_rt_aggr_release_children", 2, meta),
                        assign(
                            Place::local(Local(2)),
                            Rvalue::Aggregate {
                                kind: crate::ir::AggregateKind::Tuple,
                                operands: vec![Operand::Copy(Place::local(Local(1)))],
                            },
                        ),
                        walk_call(6, "gos_rt_aggr_retain_children", 2, meta),
                        copy(3, 2),
                        walk_call(7, "gos_rt_aggr_retain_children", 3, meta),
                        walk_call(8, "gos_rt_aggr_release_children", 2, meta),
                        walk_call(9, "gos_rt_aggr_zero_guarded", 2, meta),
                        walk_call(10, "gos_rt_aggr_release_children", 2, meta),
                    ],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    #[test]
    fn cancels_the_pair_a_whole_aggregate_move_leaves_behind() {
        let mut tcx = TyCtxt::new();
        let mut body = moved_aggregate_body(&mut tcx);
        elide_moved_aggregate_shares(&mut body, &tcx);
        let names: Vec<Option<&str>> = body.blocks[0].stmts.iter().map(intrinsic_name).collect();
        assert!(
            is_nop(&body.blocks[0].stmts[5]),
            "the destination's retain moves the source's share, so it goes: {names:?}"
        );
        assert!(
            is_nop(&body.blocks[0].stmts[6]),
            "the source's release goes with it: {names:?}"
        );
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[3]),
            Some("gos_rt_aggr_retain_children"),
            "the mint's own share stays"
        );
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[7]),
            Some("gos_rt_aggr_zero_guarded"),
            "the zero stays so every later walk is a no-op"
        );
    }

    #[test]
    fn drops_the_walk_that_stands_on_zeroed_words_and_the_zero_nothing_reads() {
        let mut tcx = TyCtxt::new();
        let mut body = moved_aggregate_body(&mut tcx);
        elide_settled_guarded_walks(&mut body);
        assert!(
            is_nop(&body.blocks[0].stmts[1]),
            "the release right after the entry zero walks nothing"
        );
        assert!(
            is_nop(&body.blocks[0].stmts[0]),
            "the entry zero has no reader left"
        );
        assert!(
            is_nop(&body.blocks[0].stmts[8]),
            "the sweep release stands on the words the second zero cleared"
        );
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[6]),
            Some("gos_rt_aggr_release_children"),
            "the release of live words stays"
        );
    }

    #[test]
    fn elides_the_guarded_walk_of_a_holder_copied_out_of_a_parameter() {
        let mut tcx = TyCtxt::new();
        let mut body = guarded_holder_body(&mut tcx, "gos_rc_meta_copyblob_1");
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert!(
            is_nop(&body.blocks[0].stmts[1]),
            "the copy's walk over the children should go"
        );
        assert!(
            is_nop(&body.blocks[1].stmts[0]),
            "the matching release walk should go with it"
        );
    }

    #[test]
    fn keeps_a_guarded_walk_whose_release_names_another_copy_blob() {
        let mut tcx = TyCtxt::new();
        let mut body = guarded_holder_body(&mut tcx, "gos_rc_meta_copyblob_2");
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[1]),
            Some("gos_rt_aggr_retain_children")
        );
        assert_eq!(
            intrinsic_name(&body.blocks[1].stmts[0]),
            Some("gos_rt_aggr_release_children")
        );
    }

    /// `L2 = Copy(L1.1); option_slot_retain(L2); <mid>;
    /// L3 = result_payload(L2); aggr_retain_children(L3, meta); f(L3)` then
    /// both releases and `Return`. `L1` is a `&Outer` parameter.
    fn payload_holder_body(tcx: &mut TyCtxt, mid: Vec<Statement>) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let node = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(11),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_aggr_copy_meta(node, "gos_rc_meta_copyblob_3");
        let opt = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs: gossamer_types::Substs::from_types([node]),
        });
        let outer = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(12),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_struct_fields(gossamer_resolve::DefId::local(12), vec![i64_ty, opt]);
        let outer_ref = tcx.intern(gossamer_types::TyKind::Ref {
            mutability: gossamer_types::Mutbl::Not,
            inner: outer,
        });
        let locals = vec![
            decl(unit),      // L0 return
            decl(outer_ref), // L1 &Outer
            decl(opt),       // L2 carrier holder
            decl(node),      // L3 payload holder
            decl(i64_ty),    // L4 call result
            decl(unit),      // L5 slot retain dest
            decl(unit),      // L6 walk retain dest
            decl(unit),      // L7 walk release dest
            decl(unit),      // L8 slot release dest
            decl(opt),       // L9 spare Option
        ];
        let mut stmts = vec![
            assign(
                Place::local(Local(2)),
                Rvalue::Use(Operand::Copy(Place {
                    local: Local(1),
                    projection: vec![Projection::Field(1)],
                })),
            ),
            rc_call(5, "gos_rt_option_slot_retain", Place::local(Local(2))),
        ];
        stmts.extend(mid);
        stmts.push(assign(
            Place::local(Local(3)),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload",
                args: vec![Operand::Copy(Place::local(Local(2)))],
            },
        ));
        stmts.push(walk_call(
            6,
            "gos_rt_aggr_retain_children",
            3,
            "gos_rc_meta_copyblob_3",
        ));
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts,
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("Node::sum".into())),
                        args: vec![Operand::Copy(Place::local(Local(3)))],
                        destination: Place::local(Local(4)),
                        target: Some(BlockId(1)),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![
                        walk_call(
                            7,
                            "gos_rt_aggr_release_children",
                            3,
                            "gos_rc_meta_copyblob_3",
                        ),
                        rc_call(8, "gos_rt_option_slot_release", Place::local(Local(2))),
                    ],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    #[test]
    fn elides_both_shares_of_a_payload_extracted_out_of_a_borrowed_carrier() {
        let mut tcx = TyCtxt::new();
        let mut body = payload_holder_body(&mut tcx, vec![]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert!(is_nop(&body.blocks[0].stmts[1]), "the slot retain should go");
        assert!(is_nop(&body.blocks[0].stmts[3]), "the walk retain should go");
        assert!(
            is_nop(&body.blocks[1].stmts[0]),
            "the walk release should go"
        );
        assert!(
            is_nop(&body.blocks[1].stmts[1]),
            "the slot release should go"
        );
    }

    #[test]
    fn keeps_both_shares_when_the_carrier_slot_is_replaced() {
        let mut tcx = TyCtxt::new();
        // `L1.1 = Copy(L9)` replaces the carrier the holder copied before the
        // payload is read, so neither share is redundant.
        let overwrite = assign(
            Place {
                local: Local(1),
                projection: vec![Projection::Field(1)],
            },
            Rvalue::Use(Operand::Copy(Place::local(Local(9)))),
        );
        let mut body = payload_holder_body(&mut tcx, vec![overwrite]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[1]),
            Some("gos_rt_option_slot_retain")
        );
        assert_eq!(
            intrinsic_name(&body.blocks[1].stmts[1]),
            Some("gos_rt_option_slot_release")
        );
    }

    /// The walk shape: `L2` (the cursor) is written from the parameter and
    /// from `L3`, `L3` from the payload `L4` extracts out of `L5`, and `L5` is
    /// copied back out of `L2`. No member has a definition the others do not
    /// feed, so only the set has one owner outside it.
    fn cursor_class_body(tcx: &mut TyCtxt, mid: Vec<Statement>) -> Body {
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let node = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(21),
            substs: gossamer_types::Substs::new(),
        });
        tcx.register_aggr_copy_meta(node, "gos_rc_meta_copyblob_9");
        let opt = tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs: gossamer_types::Substs::from_types([node]),
        });
        tcx.register_struct_fields(gossamer_resolve::DefId::local(21), vec![i64_ty, opt]);
        let meta = "gos_rc_meta_copyblob_9";
        let locals = vec![
            decl(unit),   // L0 return
            decl(node),   // L1 by-value parameter
            decl(node),   // L2 cursor
            decl(node),   // L3 unwrapped child
            decl(opt),    // L4 the child slot
            decl(opt),    // L5 spare Option
            decl(unit),   // L6..L11 accounting destinations
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
            decl(unit),
            decl(i64_ty), // L12 loop guard
        ];
        // bb0: seed the cursor from the parameter.
        let mut bb0 = vec![
            copy(2, 1),
            walk_call(6, "gos_rt_aggr_retain_children", 2, meta),
        ];
        bb0.extend(mid);
        // bb1: read the cursor's slot, unwrap it, and rebind the cursor.
        let bb1 = vec![
            rc_call(7, "gos_rt_option_slot_release", Place::local(Local(4))),
            assign(
                Place::local(Local(4)),
                Rvalue::Use(Operand::Copy(Place {
                    local: Local(2),
                    projection: vec![Projection::Field(1)],
                })),
            ),
            rc_call(8, "gos_rt_option_slot_retain", Place::local(Local(4))),
            assign(
                Place::local(Local(3)),
                Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload",
                    args: vec![Operand::Copy(Place::local(Local(4)))],
                },
            ),
            walk_call(9, "gos_rt_aggr_retain_children", 3, meta),
            walk_call(10, "gos_rt_aggr_release_children", 2, meta),
            copy(2, 3),
            walk_call(11, "gos_rt_aggr_retain_children", 2, meta),
        ];
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: bb0,
                    terminator: Terminator::Goto {
                        target: BlockId(1),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: bb1,
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(12))),
                        arms: vec![(0, BlockId(2))],
                        default: BlockId(1),
                    },
                    span: span(),
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![
                        walk_call(6, "gos_rt_aggr_release_children", 2, meta),
                        walk_call(7, "gos_rt_aggr_release_children", 3, meta),
                        rc_call(8, "gos_rt_option_slot_release", Place::local(Local(4))),
                    ],
                    terminator: Terminator::Return,
                    span: span(),
                },
            ],
            span: span(),
        }
    }

    #[test]
    fn elides_every_share_in_a_cursor_class_rooted_at_a_parameter() {
        let mut tcx = TyCtxt::new();
        let mut body = cursor_class_body(&mut tcx, vec![]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        let live: Vec<&str> = body
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter().filter_map(intrinsic_name))
            .filter(|name| !crate::opt::extracts_carrier_payload(name))
            .collect();
        assert!(
            live.is_empty(),
            "every member's accounting should go, left: {live:?}"
        );
    }

    #[test]
    fn keeps_a_cursor_class_whose_root_is_written_inside_the_walk() {
        let mut tcx = TyCtxt::new();
        // A bare write of the root replaces the tree the whole class views.
        let overwrite = copy(1, 3);
        let mut body = cursor_class_body(&mut tcx, vec![overwrite]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        let live: Vec<&str> = body
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter().filter_map(intrinsic_name))
            .collect();
        assert!(
            live.contains(&"gos_rt_aggr_retain_children"),
            "the class must keep its shares, left: {live:?}"
        );
    }

    #[test]
    fn elides_field_share_of_a_struct_holder_read_through_a_reference() {
        let mut tcx = TyCtxt::new();
        let mut body = field_holder_body(&mut tcx, vec![]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert!(
            is_nop(&body.blocks[0].stmts[1]),
            "the copy's share of the Vec field should go"
        );
        assert!(
            is_nop(&body.blocks[1].stmts[0]),
            "the matching field release should go with it"
        );
    }

    #[test]
    fn keeps_field_share_when_the_source_field_is_replaced() {
        let mut tcx = TyCtxt::new();
        // `L1.0 = Copy(L6)` replaces the struct the holder copied before the
        // call reads it, so the copy's own share is what keeps the Vec alive.
        let overwrite = assign(
            Place {
                local: Local(1),
                projection: vec![Projection::Field(0)],
            },
            Rvalue::Use(Operand::Copy(Place::local(Local(6)))),
        );
        let mut body = field_holder_body(&mut tcx, vec![overwrite]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[1]),
            Some("gos_rt_vec_retain")
        );
        assert_eq!(
            intrinsic_name(&body.blocks[1].stmts[0]),
            Some("gos_rt_vec_free")
        );
    }

    #[test]
    fn elides_share_of_holder_whose_read_is_an_element_store() {
        let mut tcx = TyCtxt::new();
        let mut body = holder_body(&mut tcx, vec![]);
        let unit = tcx.unit();
        body.locals.push(decl(unit)); // L7: the store's unit destination
        body.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_set_i64".into())),
            args: vec![
                Operand::Copy(Place::local(Local(2))),
                Operand::Copy(Place::local(Local(3))),
                Operand::Copy(Place::local(Local(3))),
            ],
            destination: Place::local(Local(7)),
            target: Some(BlockId(1)),
        };
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert!(
            is_nop(&body.blocks[0].stmts[1]),
            "an element store leaves the receiver's count alone, so the retain should go"
        );
        assert!(
            is_nop(&body.blocks[1].stmts[0]),
            "the matching release should go with it"
        );
    }

    #[test]
    fn keeps_share_of_holder_written_through_before_its_read() {
        let mut tcx = TyCtxt::new();
        // A push through the holder changes the structure the chain aliases,
        // so the later read is not covered by the source's own share.
        let push = assign(
            Place::local(Local(5)),
            Rvalue::CallIntrinsic {
                name: "gos_rt_vec_push",
                args: vec![
                    Operand::Copy(Place::local(Local(2))),
                    Operand::Copy(Place::local(Local(3))),
                ],
            },
        );
        let mut body = holder_body(&mut tcx, vec![push]);
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(
            intrinsic_name(&body.blocks[0].stmts[1]),
            Some("gos_rt_vec_retain")
        );
    }

    #[test]
    fn keeps_share_of_holder_passed_to_a_consuming_helper() {
        let mut tcx = TyCtxt::new();
        let mut body = holder_body(&mut tcx, vec![]);
        body.blocks[0].terminator = Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".into())),
            args: vec![
                Operand::Copy(Place::local(Local(6))),
                Operand::Copy(Place::local(Local(2))),
            ],
            destination: Place::local(Local(3)),
            target: Some(BlockId(1)),
        };
        elide_borrowed_holder_rc(&mut body, &tcx);
        assert_eq!(intrinsic_name(&body.blocks[0].stmts[1]), Some("gos_rt_vec_retain"));
    }

    #[test]
    fn cancels_tight_nonescaping_pair() {
        let mut tcx = TyCtxt::new();
        let mut body = body_with(&mut tcx, vec![], vec![], vec![]);
        elide_redundant_rc_pairs(&mut body, &tcx);
        let stmts = &body.blocks[0].stmts;
        // retain at index 1, release at index 2 are both cancelled.
        assert!(
            is_nop(&stmts[1]),
            "retain should be cancelled: {:?}",
            stmts[1].kind
        );
        assert!(
            is_nop(&stmts[2]),
            "release should be cancelled: {:?}",
            stmts[2].kind
        );
    }

    #[test]
    fn keeps_pair_when_value_used_between() {
        let mut tcx = TyCtxt::new();
        // `L3 = Copy(L1)` between the retain and the release reads `x`,
        // so the bracket is not tight - both ops are preserved.
        let mut body = body_with(&mut tcx, vec![], vec![copy(3, 1)], vec![]);
        elide_redundant_rc_pairs(&mut body, &tcx);
        let names: Vec<Option<&str>> = body.blocks[0].stmts.iter().map(intrinsic_name).collect();
        assert!(
            names.contains(&Some("gos_rt_rc_retain")) && names.contains(&Some("gos_rt_rc_release")),
            "pair must be kept when the value is used between retain and release"
        );
    }

    #[test]
    fn keeps_pair_when_value_live_after_release() {
        let mut tcx = TyCtxt::new();
        // `L3 = Copy(L1)` after the release keeps `x` live, so the
        // forward-liveness guard preserves both ops.
        let mut body = body_with(&mut tcx, vec![], vec![], vec![copy(3, 1)]);
        elide_redundant_rc_pairs(&mut body, &tcx);
        let names: Vec<Option<&str>> = body.blocks[0].stmts.iter().map(intrinsic_name).collect();
        assert!(
            names.contains(&Some("gos_rt_rc_retain")) && names.contains(&Some("gos_rt_rc_release")),
            "pair must be kept when the value is read after the release"
        );
    }

    #[test]
    fn keeps_field_projection_release() {
        let mut tcx = TyCtxt::new();
        // A field-projected release arg (`x.0`) is never a bare-local
        // pair: the aggregate-teardown accounting must be left intact.
        let mut body = body_with(&mut tcx, vec![], vec![], vec![]);
        if let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { args, .. },
            ..
        } = &mut body.blocks[0].stmts[2].kind
        {
            args[0] = Operand::Copy(Place {
                local: Local(1),
                projection: vec![Projection::Field(0)],
            });
        }
        elide_redundant_rc_pairs(&mut body, &tcx);
        assert!(
            !is_nop(&body.blocks[0].stmts[1]),
            "retain must be kept when the release is field-projected"
        );
    }

    #[test]
    fn cancels_value_moved_into_returned_holder() {
        let mut tcx = TyCtxt::new();
        // `x` (Local 1) is moved into the holder (`holder = Copy(x)`, the
        // forwarding use) and the holder is then returned (`L0 =
        // Copy(holder)`), so `x` transitively escapes the function. `x`
        // itself is dead after the release and not goroutine-shared, so
        // the move into the surviving holder is a pure ownership transfer
        // and the pair cancels. This is the binary-trees case: a child
        // moved into a returned node. The pair was previously kept because
        // the escape gate flagged the transitive escape.
        let mut body = body_with(&mut tcx, vec![], vec![], vec![copy(0, 2)]);
        elide_redundant_rc_pairs(&mut body, &tcx);
        assert!(
            is_nop(&body.blocks[0].stmts[1]),
            "retain should be cancelled for a value moved into a returned holder: {:?}",
            body.blocks[0].stmts[1].kind
        );
        assert!(
            is_nop(&body.blocks[0].stmts[2]),
            "release should be cancelled for a value moved into a returned holder: {:?}",
            body.blocks[0].stmts[2].kind
        );
    }

    #[test]
    fn keeps_goroutine_shared_value() {
        let mut tcx = TyCtxt::new();
        // `x` (Local 1) is marked shared (it crosses a goroutine
        // boundary), so another goroutine may concurrently adjust its
        // count and the balanced pair is load-bearing for the atomic
        // protocol - both ops must be preserved.
        let mut body = body_with(&mut tcx, vec![], vec![], vec![]);
        body.blocks[0]
            .stmts
            .push(rc_call(4, "gos_rt_rc_mark_shared", Place::local(Local(1))));
        elide_redundant_rc_pairs(&mut body, &tcx);
        let names: Vec<Option<&str>> = body.blocks[0].stmts.iter().map(intrinsic_name).collect();
        assert!(
            names.contains(&Some("gos_rt_rc_retain")) && names.contains(&Some("gos_rt_rc_release")),
            "pair must be kept for a goroutine-shared value"
        );
    }

    /// Builds a `for i in 0..len(xs)` loop over an `[i64]` vec that reads
    /// `xs[i]`, matching the lowerer's post-optimise shape, and returns
    /// the body plus a `TyCtxt`.
    fn counted_loop_body(tcx: &mut TyCtxt) -> Body {
        use gossamer_types::IntTy;
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let slice = tcx.intern(gossamer_types::TyKind::Slice(i64t));
        let boolt = tcx.intern(gossamer_types::TyKind::Bool);
        // L0 ret(unit), L1 xs(slice), L2 bound, L3 counter, L4 cmp(bool),
        // L5 idx, L6 elem, L7 unit
        let locals = vec![
            decl(unit),
            decl(slice),
            decl(i64t),
            decl(i64t),
            decl(boolt),
            decl(i64t),
            decl(i64t),
            decl(unit),
        ];
        let sp = span();
        let call = |callee: &str, args: Vec<Operand>, dst: u32, target: u32| Terminator::Call {
            callee: Operand::Const(ConstValue::Str(callee.to_string())),
            args,
            destination: Place::local(Local(dst)),
            target: Some(BlockId(target)),
        };
        let blocks = vec![
            // bb0: bound = len(xs); init counter = 0
            BasicBlock {
                id: BlockId(0),
                stmts: vec![assign(
                    Place::local(Local(3)),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                )],
                terminator: call(
                    "gos_rt_vec_len",
                    vec![Operand::Copy(Place::local(Local(1)))],
                    2,
                    1,
                ),
                span: sp,
            },
            // bb1 header: cmp = counter < bound; switch
            BasicBlock {
                id: BlockId(1),
                stmts: vec![assign(
                    Place::local(Local(4)),
                    Rvalue::BinaryOp {
                        op: BinOp::Lt,
                        lhs: Operand::Copy(Place::local(Local(3))),
                        rhs: Operand::Copy(Place::local(Local(2))),
                    },
                )],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(Local(4))),
                    arms: vec![(0, BlockId(3))],
                    default: BlockId(2),
                },
                span: sp,
            },
            // bb2 body: idx = counter; elem = xs[idx]; -> latch via call target
            BasicBlock {
                id: BlockId(2),
                stmts: vec![copy(5, 3)],
                terminator: call(
                    "gos_rt_vec_get_i64",
                    vec![
                        Operand::Copy(Place::local(Local(1))),
                        Operand::Copy(Place::local(Local(5))),
                    ],
                    6,
                    4,
                ),
                span: sp,
            },
            // bb3 exit
            BasicBlock {
                id: BlockId(3),
                stmts: vec![],
                terminator: Terminator::Return,
                span: sp,
            },
            // bb4 latch: counter += 1; goto header
            BasicBlock {
                id: BlockId(4),
                stmts: vec![assign(
                    Place::local(Local(3)),
                    Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(Place::local(Local(3))),
                        rhs: Operand::Const(ConstValue::Int(1)),
                    },
                )],
                terminator: Terminator::Goto { target: BlockId(1) },
                span: sp,
            },
        ];
        Body {
            name: "t".into(),
            def: None,
            arity: 1,
            locals,
            blocks,
            span: sp,
        }
    }

    #[test]
    fn bounds_rewrites_counted_loop_get() {
        let mut tcx = TyCtxt::new();
        let mut body = counted_loop_body(&mut tcx);
        bounds_check_elim(&mut body, &tcx);
        let Terminator::Call { callee, .. } = &body.blocks[2].terminator else {
            panic!("expected call terminator")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str("gos_rt_vec_get_i64_unchecked".to_string())),
            "the proven in-range read should be rewritten to the unchecked callee"
        );
    }

    #[test]
    fn bounds_keeps_check_when_bound_is_not_len() {
        let mut tcx = TyCtxt::new();
        let mut body = counted_loop_body(&mut tcx);
        // Make the bound an opaque non-negative constant instead of
        // `len(xs)`: the read must stay checked.
        body.blocks[0].terminator = Terminator::Goto { target: BlockId(1) };
        body.blocks[0].stmts.push(assign(
            Place::local(Local(2)),
            Rvalue::Use(Operand::Const(ConstValue::Int(3))),
        ));
        bounds_check_elim(&mut body, &tcx);
        let Terminator::Call { callee, .. } = &body.blocks[2].terminator else {
            panic!("expected call terminator")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string())),
            "a bound that is not the vec's length must keep the checked read"
        );
    }

    #[test]
    fn bounds_rewrites_direct_branch_guarded_get() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let vec_i64 = tcx.intern(gossamer_types::TyKind::Vec(i64t));
        let boolt = tcx.bool_ty();
        let locals = vec![
            decl(unit),
            decl(vec_i64), // xs
            decl(i64t),    // len
            decl(i64t),    // idx
            decl(boolt),   // cmp
            decl(i64t),    // elem
        ];
        let sp = span();
        let blocks = vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![assign(
                    Place::local(Local(3)),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                )],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                    args: vec![Operand::Copy(Place::local(Local(1)))],
                    destination: Place::local(Local(2)),
                    target: Some(BlockId(1)),
                },
                span: sp,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![assign(
                    Place::local(Local(4)),
                    Rvalue::BinaryOp {
                        op: BinOp::Lt,
                        lhs: Operand::Copy(Place::local(Local(3))),
                        rhs: Operand::Copy(Place::local(Local(2))),
                    },
                )],
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::local(Local(4))),
                    arms: vec![(0, BlockId(3))],
                    default: BlockId(2),
                },
                span: sp,
            },
            BasicBlock {
                id: BlockId(2),
                stmts: vec![],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(Local(1))),
                        Operand::Copy(Place::local(Local(3))),
                    ],
                    destination: Place::local(Local(5)),
                    target: Some(BlockId(3)),
                },
                span: sp,
            },
            BasicBlock {
                id: BlockId(3),
                stmts: vec![],
                terminator: Terminator::Return,
                span: sp,
            },
        ];
        let mut body = Body {
            name: "branch".into(),
            def: None,
            arity: 1,
            locals,
            blocks,
            span: sp,
        };
        local_branch_bounds_check_elim(&mut body, &tcx);
        let Terminator::Call { callee, .. } = &body.blocks[2].terminator else {
            panic!("expected call terminator")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str("gos_rt_vec_get_i64_unchecked".to_string()))
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression keeps the complete branch-and-induction MIR shape visible"
    )]
    fn bounds_rewrites_unit_induction_after_branch_guard() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let vec_i64 = tcx.intern(gossamer_types::TyKind::Vec(i64t));
        let boolt = tcx.bool_ty();
        let sp = span();
        let mut body = Body {
            name: "branch_induction".into(),
            def: None,
            arity: 1,
            locals: vec![
                decl(unit),
                decl(vec_i64), // xs
                decl(i64t),    // len
                decl(i64t),    // index
                decl(boolt),   // cmp
                decl(i64t),    // element
                decl(i64t),    // increment temporary
            ],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![assign(
                        Place::local(Local(3)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    )],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                        args: vec![Operand::Copy(Place::local(Local(1)))],
                        destination: Place::local(Local(2)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::BinaryOp {
                            op: BinOp::Lt,
                            lhs: Operand::Copy(Place::local(Local(3))),
                            rhs: Operand::Copy(Place::local(Local(2))),
                        },
                    )],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(4))),
                        arms: vec![(0, BlockId(4))],
                        default: BlockId(2),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_vec_get_i64".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(3))),
                        ],
                        destination: Place::local(Local(5)),
                        target: Some(BlockId(3)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(3),
                    stmts: vec![
                        assign(
                            Place::local(Local(6)),
                            Rvalue::BinaryOp {
                                op: BinOp::Add,
                                lhs: Operand::Copy(Place::local(Local(3))),
                                rhs: Operand::Const(ConstValue::Int(1)),
                            },
                        ),
                        copy(3, 6),
                    ],
                    terminator: Terminator::Goto { target: BlockId(1) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(4),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        local_branch_bounds_check_elim(&mut body, &tcx);
        let Terminator::Call { callee, .. } = &body.blocks[2].terminator else {
            panic!("expected call terminator")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str("gos_rt_vec_get_i64_unchecked".to_string()))
        );

        let StatementKind::Assign {
            rvalue: Rvalue::BinaryOp { rhs, .. },
            ..
        } = &mut body.blocks[3].stmts[0].kind
        else {
            panic!("expected increment")
        };
        *rhs = Operand::Const(ConstValue::Int(2));
        if let Terminator::Call { callee, .. } = &mut body.blocks[2].terminator {
            *callee = Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string()));
        }
        local_branch_bounds_check_elim(&mut body, &tcx);
        assert!(matches!(
            &body.blocks[2].terminator,
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                ..
            } if name == "gos_rt_vec_get_i64"
        ));
    }

    #[test]
    fn bounds_rewrites_zero_index_after_len_positive_guard() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let vec_i64 = tcx.intern(gossamer_types::TyKind::Vec(i64t));
        let boolt = tcx.bool_ty();
        let sp = span();
        let mut body = Body {
            name: "len_positive_zero".into(),
            def: None,
            arity: 1,
            locals: vec![
                decl(unit),
                decl(vec_i64), // xs
                decl(i64t),    // len
                decl(boolt),   // len > 0
                decl(i64t),    // zero index
                decl(i64t),    // element
            ],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                        args: vec![Operand::Copy(Place::local(Local(1)))],
                        destination: Place::local(Local(2)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(3)),
                        Rvalue::BinaryOp {
                            op: BinOp::Gt,
                            lhs: Operand::Copy(Place::local(Local(2))),
                            rhs: Operand::Const(ConstValue::Int(0)),
                        },
                    )],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(3))),
                        arms: vec![(0, BlockId(3))],
                        default: BlockId(2),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    )],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(4))),
                        ],
                        destination: Place::local(Local(5)),
                        target: Some(BlockId(3)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(3),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        local_branch_bounds_check_elim(&mut body, &tcx);
        let Terminator::Call { callee, .. } = &body.blocks[2].terminator else {
            panic!("expected call terminator")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str("gos_rt_vec_get_i64_unchecked".to_string()))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one test whose length is the chain it walks")]
    fn bounds_fact_flows_through_straight_line_access_chain() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let vec_i64 = tcx.intern(gossamer_types::TyKind::Vec(i64t));
        let boolt = tcx.bool_ty();
        let sp = span();
        let mut body = Body {
            name: "branch_chain".into(),
            def: None,
            arity: 1,
            locals: vec![
                decl(unit),
                decl(vec_i64), // xs
                decl(i64t),    // len
                decl(i64t),    // idx
                decl(boolt),   // cmp
                decl(i64t),    // first
                decl(i64t),    // second
            ],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![assign(
                        Place::local(Local(3)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    )],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_len".to_string())),
                        args: vec![Operand::Copy(Place::local(Local(1)))],
                        destination: Place::local(Local(2)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::BinaryOp {
                            op: BinOp::Lt,
                            lhs: Operand::Copy(Place::local(Local(3))),
                            rhs: Operand::Copy(Place::local(Local(2))),
                        },
                    )],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(4))),
                        arms: vec![(0, BlockId(5))],
                        default: BlockId(2),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(3))),
                        ],
                        destination: Place::local(Local(5)),
                        target: Some(BlockId(3)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(3),
                    stmts: vec![],
                    terminator: Terminator::Goto { target: BlockId(4) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(4),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_i64".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(3))),
                        ],
                        destination: Place::local(Local(6)),
                        target: Some(BlockId(5)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(5),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        local_branch_bounds_check_elim(&mut body, &tcx);
        for block in [&body.blocks[2], &body.blocks[4]] {
            assert!(matches!(
                &block.terminator,
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    ..
                } if name == "gos_rt_vec_get_i64_unchecked"
            ));
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "synthetic MIR fixture needs explicit block structure"
    )]
    fn fuse_slice_parse_ranges_rewrites_parse_only_slice() {
        use gossamer_types::IntTy;
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64t = tcx.int_ty(IntTy::I64);
        let string = tcx.string_ty();
        let result = tcx.intern(gossamer_types::TyKind::Tuple(vec![i64t, i64t]));
        let locals = vec![
            decl(unit),   // 0 return
            decl(string), // 1 input
            decl(i64t),   // 2 start
            decl(i64t),   // 3 end
            decl(result), // 4 slice result
            decl(i64t),   // 5 temp payload
            decl(string), // 6 unwrapped string temp
            decl(result), // 7 parse result
            decl(unit),   // 8 release unit
        ];
        let sp = span();
        let blocks = vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        Place::local(Local(6)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    ),
                    assign(
                        Place::local(Local(2)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(1))),
                    ),
                    assign(
                        Place::local(Local(3)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(3))),
                    ),
                ],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_str_slice".to_string())),
                    args: vec![
                        Operand::Copy(Place::local(Local(1))),
                        Operand::Copy(Place::local(Local(2))),
                        Operand::Copy(Place::local(Local(3))),
                    ],
                    destination: Place::local(Local(4)),
                    target: Some(BlockId(1)),
                },
                span: sp,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![
                    assign(
                        Place::local(Local(5)),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_payload",
                            args: vec![Operand::Copy(Place::local(Local(4)))],
                        },
                    ),
                    assign(
                        Place::local(Local(8)),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_rc_release",
                            args: vec![Operand::Copy(Place::local(Local(6)))],
                        },
                    ),
                    copy(6, 5),
                ],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_strconv_parse_i64".to_string())),
                    args: vec![Operand::Copy(Place::local(Local(6)))],
                    destination: Place::local(Local(7)),
                    target: Some(BlockId(2)),
                },
                span: sp,
            },
            BasicBlock {
                id: BlockId(2),
                stmts: vec![assign(
                    Place::local(Local(8)),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_rc_release",
                        args: vec![Operand::Copy(Place::local(Local(6)))],
                    },
                )],
                terminator: Terminator::Return,
                span: sp,
            },
        ];
        let mut body = Body {
            name: "parse".into(),
            def: None,
            arity: 1,
            locals,
            blocks,
            span: sp,
        };

        fuse_slice_parse_ranges(&mut body);

        assert!(matches!(
            body.blocks[0].terminator,
            Terminator::Goto { target: BlockId(1) }
        ));
        assert!(is_nop(&body.blocks[1].stmts[2]));
        let Terminator::Call { callee, args, .. } = &body.blocks[1].terminator else {
            panic!("expected parse call")
        };
        assert_eq!(
            callee,
            &Operand::Const(ConstValue::Str(
                "gos_rt_strconv_parse_i64_range".to_string()
            ))
        );
        assert_eq!(
            args,
            &vec![
                Operand::Copy(Place::local(Local(1))),
                Operand::Copy(Place::local(Local(2))),
                Operand::Copy(Place::local(Local(3))),
            ]
        );
    }

    #[test]
    fn reserve_vecs_rewrites_counted_push_loop_constructor() {
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let locals = vec![
            decl(unit),   // L0 return
            decl(i64_ty), // L1 vec handle in this synthetic test
            decl(i64_ty), // L2 counter
            decl(i64_ty), // L3 bound
            decl(i64_ty), // L4 cond
            decl(i64_ty), // L5 value
            decl(unit),   // L6 push result
        ];
        let sp = span();
        let mut body = Body {
            name: "reserve".into(),
            def: None,
            arity: 3,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
                        args: vec![Operand::Const(ConstValue::Int(8))],
                        destination: Place::local(Local(1)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::BinaryOp {
                            op: BinOp::Lt,
                            lhs: Operand::Copy(Place::local(Local(2))),
                            rhs: Operand::Copy(Place::local(Local(3))),
                        },
                    )],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(4))),
                        arms: vec![(0, BlockId(4))],
                        default: BlockId(2),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(5))),
                        ],
                        destination: Place::local(Local(6)),
                        target: Some(BlockId(3)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(3),
                    stmts: vec![assign(
                        Place::local(Local(2)),
                        Rvalue::BinaryOp {
                            op: BinOp::Add,
                            lhs: Operand::Copy(Place::local(Local(2))),
                            rhs: Operand::Const(ConstValue::Int(1)),
                        },
                    )],
                    terminator: Terminator::Goto { target: BlockId(1) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(4),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        reserve_vecs_for_counted_push_loops(&mut body);

        let Terminator::Call { callee, args, .. } = &body.blocks[0].terminator else {
            panic!("expected constructor call")
        };
        assert!(
            matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_vec_with_capacity")
        );
        assert_eq!(args.len(), 2);
        assert!(matches!(
            args[1],
            Operand::Copy(Place {
                local: Local(3),
                projection: _
            })
        ));
    }

    #[test]
    fn exact_reserve_requires_one_push_on_every_loop_path() {
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let sp = span();
        let push = |target| Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
            args: vec![
                Operand::Copy(Place::local(Local(1))),
                Operand::Const(ConstValue::Int(1)),
            ],
            destination: Place::local(Local(2)),
            target: Some(target),
        };
        let mut body = Body {
            name: "one_push".into(),
            def: None,
            arity: 0,
            locals: vec![decl(unit), decl(unit), decl(unit)],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![],
                    terminator: push(BlockId(1)),
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![],
                    terminator: Terminator::Goto { target: BlockId(2) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };
        assert!(loop_body_has_exactly_one_vec_push(
            &body,
            BlockId(0),
            BlockId(2),
            Local(1)
        ));

        body.blocks[0].terminator = Terminator::SwitchInt {
            discriminant: Operand::Const(ConstValue::Bool(false)),
            arms: vec![(0, BlockId(2))],
            default: BlockId(1),
        };
        assert!(!loop_body_has_exactly_one_vec_push(
            &body,
            BlockId(0),
            BlockId(2),
            Local(1)
        ));

        body.blocks[0].terminator = push(BlockId(1));
        body.blocks[1].terminator = push(BlockId(2));
        assert!(!loop_body_has_exactly_one_vec_push(
            &body,
            BlockId(0),
            BlockId(2),
            Local(1)
        ));
    }

    #[test]
    fn reserve_vecs_skips_bounds_computed_after_constructor() {
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let locals = vec![
            decl(unit),   // L0 return
            decl(i64_ty), // L1 vec
            decl(i64_ty), // L2 counter
            decl(i64_ty), // L3 bound, assigned after constructor
            decl(i64_ty), // L4 cond
            decl(i64_ty), // L5 value
            decl(unit),   // L6 push result
        ];
        let sp = span();
        let mut body = Body {
            name: "reserve_skip".into(),
            def: None,
            arity: 0,
            locals,
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("Vec::new".to_string())),
                        args: vec![Operand::Const(ConstValue::Int(8))],
                        destination: Place::local(Local(1)),
                        target: Some(BlockId(1)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![assign(
                        Place::local(Local(3)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(10))),
                    )],
                    terminator: Terminator::Goto { target: BlockId(2) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(2),
                    stmts: vec![assign(
                        Place::local(Local(4)),
                        Rvalue::BinaryOp {
                            op: BinOp::Lt,
                            lhs: Operand::Copy(Place::local(Local(2))),
                            rhs: Operand::Copy(Place::local(Local(3))),
                        },
                    )],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::local(Local(4))),
                        arms: vec![(0, BlockId(5))],
                        default: BlockId(3),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(3),
                    stmts: vec![],
                    terminator: Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_push".to_string())),
                        args: vec![
                            Operand::Copy(Place::local(Local(1))),
                            Operand::Copy(Place::local(Local(5))),
                        ],
                        destination: Place::local(Local(6)),
                        target: Some(BlockId(4)),
                    },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(4),
                    stmts: vec![assign(
                        Place::local(Local(2)),
                        Rvalue::BinaryOp {
                            op: BinOp::Add,
                            lhs: Operand::Copy(Place::local(Local(2))),
                            rhs: Operand::Const(ConstValue::Int(1)),
                        },
                    )],
                    terminator: Terminator::Goto { target: BlockId(2) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(5),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };

        reserve_vecs_for_counted_push_loops(&mut body);

        let Terminator::Call { callee, args, .. } = &body.blocks[0].terminator else {
            panic!("expected constructor call")
        };
        assert!(matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "Vec::new"));
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn reserve_bound_accepts_one_dominating_immutable_local() {
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
        let sp = span();
        let mut body = Body {
            name: "local_reserve_bound".into(),
            def: None,
            arity: 0,
            locals: vec![decl(unit), decl(i64_ty)],
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    stmts: vec![assign(
                        Place::local(Local(1)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(10))),
                    )],
                    terminator: Terminator::Goto { target: BlockId(1) },
                    span: sp,
                },
                BasicBlock {
                    id: BlockId(1),
                    stmts: vec![],
                    terminator: Terminator::Return,
                    span: sp,
                },
            ],
            span: sp,
        };
        let bound = Operand::Copy(Place::local(Local(1)));
        assert!(reserve_bound_available_at_entry(&body, &bound, BlockId(1)));

        body.blocks[1].stmts.push(assign(
            Place::local(Local(1)),
            Rvalue::Use(Operand::Const(ConstValue::Int(11))),
        ));
        assert!(!reserve_bound_available_at_entry(&body, &bound, BlockId(1)));
    }
}
