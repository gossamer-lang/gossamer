//! Bytecode chunk validator.
//!
//! The VM's dispatch loop relies on a "compiler always emits in-bounds
//! register / const / jump indices" invariant to skip per-op bounds
//! checks (see the unsafe-block doc in [`crate::vm`]). When that
//! invariant is silently broken by a compile.rs regression, the
//! result is UB rather than a clean panic. [`validate_chunk`] runs at
//! [`Vm::load`](crate::vm::Vm::load) time so
//! malformed bytecode surfaces as a clear `RuntimeError` instead of a
//! segfault.

use std::fmt;

use crate::bytecode::{FnChunk, Op, Reg, WideOp};

/// Diagnostics from [`validate_chunk`]. Each variant points to the
/// offending instruction by linear index plus the specific bound it
/// violated.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::enum_variant_names,
    reason = "every violation is shape `XOutOfBounds`; the prefix is the discriminator"
)]
pub(crate) enum ValidationError {
    /// A chunk-level declaration is internally inconsistent before any
    /// instruction is executed.
    InvalidChunkShape {
        /// Human-readable invariant that failed.
        reason: String,
    },
    /// A jump-shaped op targets an instruction past the end of the
    /// chunk's instruction stream.
    PcOutOfBounds {
        /// Index of the offending op within `chunk.instrs`.
        op_idx: usize,
        /// Target instruction index named by the op.
        target: usize,
        /// Number of instructions actually in the chunk.
        instr_count: usize,
    },
    /// A register-shaped operand exceeds the chunk's declared
    /// register-file size for its file (value / f64 / i64).
    RegisterOutOfBounds {
        /// Index of the offending op within `chunk.instrs`.
        op_idx: usize,
        /// Operand register number.
        reg: u32,
        /// Declared size of the register file the operand targets.
        count: u32,
        /// Register-file kind (for diagnostics).
        file: RegFile,
    },
    /// A constant-pool index is out of range for the targeted pool
    /// (boxed value pool / f64 pool / i64 pool / globals / deferred
    /// expressions / wide-ops side table).
    ConstantOutOfBounds {
        /// Index of the offending op within `chunk.instrs`.
        op_idx: usize,
        /// The index named by the op.
        idx: u32,
        /// Length of the targeted pool.
        len: usize,
        /// Pool kind (for diagnostics).
        pool: PoolKind,
    },
    /// An inline-cache operand exceeds the chunk's declared cache-slot count.
    CacheOutOfBounds {
        /// Index of the offending op within `chunk.instrs`.
        op_idx: usize,
        /// Cache slot named by the op.
        idx: u16,
        /// Declared number of slots for this cache kind.
        count: u16,
        /// Cache family used for diagnostics.
        cache: CacheKind,
    },
    /// A reachable instruction can continue past the end of the
    /// instruction stream instead of returning, panicking, or jumping.
    ControlFlowFallsOffEnd {
        /// The last reachable instruction before the invalid fall-through.
        op_idx: usize,
    },
    /// A reachable instruction reads an unboxed register that no preceding
    /// write on every control-flow path has initialized. The unboxed files'
    /// storage is reused between frames and the dispatch loop intentionally
    /// uses unchecked indexing on the validated bytecode path. Boxed
    /// registers deliberately do not use this error: every frame materializes
    /// them as `Value::Void`, which is a defined language value rather than
    /// uninitialized host memory.
    RegisterUninitialized {
        /// Index of the instruction that performs the invalid read.
        op_idx: usize,
        /// Register number read before initialization.
        reg: u32,
        /// Register file containing the invalid read.
        file: RegFile,
    },
}

/// Register-file discriminator for [`ValidationError::RegisterOutOfBounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegFile {
    /// Boxed `Value` register file.
    Value,
    /// Unboxed `f64` register file.
    Float,
    /// Unboxed `i64` register file.
    Int,
}

impl fmt::Display for RegFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value => f.write_str("value"),
            Self::Float => f.write_str("f64"),
            Self::Int => f.write_str("i64"),
        }
    }
}

/// Constant-pool discriminator for [`ValidationError::ConstantOutOfBounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PoolKind {
    /// Boxed value constant pool (`consts`).
    Consts,
    /// `f64` constant pool (`f64_consts`).
    F64Consts,
    /// `i64` constant pool (`i64_consts`).
    I64Consts,
    /// Global-name table (`globals`).
    Globals,
    /// Wide-op side table (`wide_ops`).
    WideOps,
    /// Closure-proto table (`closure_protos`).
    ClosureProtos,
    /// `select` arm metadata table (`select_arms`).
    SelectArms,
    /// Variant and struct shape-name table (`shape_names`).
    ShapeNames,
}

/// Inline-cache discriminator for [`ValidationError::CacheOutOfBounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheKind {
    /// Call and method dispatch cache.
    Call,
    /// Adaptive arithmetic cache.
    Arithmetic,
    /// Struct field lookup cache.
    Field,
}

impl fmt::Display for CacheKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call => f.write_str("call"),
            Self::Arithmetic => f.write_str("arithmetic"),
            Self::Field => f.write_str("field"),
        }
    }
}

impl fmt::Display for PoolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Consts => f.write_str("consts"),
            Self::F64Consts => f.write_str("f64_consts"),
            Self::I64Consts => f.write_str("i64_consts"),
            Self::Globals => f.write_str("globals"),
            Self::WideOps => f.write_str("wide_ops"),
            Self::ClosureProtos => f.write_str("closure_protos"),
            Self::SelectArms => f.write_str("select_arms"),
            Self::ShapeNames => f.write_str("shape_names"),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChunkShape { reason } => {
                write!(f, "bytecode validator: invalid chunk shape: {reason}")
            }
            Self::PcOutOfBounds {
                op_idx,
                target,
                instr_count,
            } => write!(
                f,
                "bytecode validator: op #{op_idx} jumps to {target}, but chunk has {instr_count} instructions"
            ),
            Self::RegisterOutOfBounds {
                op_idx,
                reg,
                count,
                file,
            } => write!(
                f,
                "bytecode validator: op #{op_idx} references {file} register {reg}, but chunk declares {count}"
            ),
            Self::ConstantOutOfBounds {
                op_idx,
                idx,
                len,
                pool,
            } => write!(
                f,
                "bytecode validator: op #{op_idx} references {pool}[{idx}], but pool has length {len}"
            ),
            Self::CacheOutOfBounds {
                op_idx,
                idx,
                count,
                cache,
            } => write!(
                f,
                "bytecode validator: op #{op_idx} references {cache} cache slot {idx}, but chunk declares {count}"
            ),
            Self::ControlFlowFallsOffEnd { op_idx } => write!(
                f,
                "bytecode validator: reachable op #{op_idx} falls off the end of the chunk"
            ),
            Self::RegisterUninitialized { op_idx, reg, file } => write!(
                f,
                "bytecode validator: op #{op_idx} reads uninitialized {file} register {reg}"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Walks every instruction in `chunk` and confirms every register,
/// constant-pool, and jump target stays within the chunk's declared
/// counts. Returns the first violation; surrounding callers report it
/// as a `RuntimeError` so the failure is observable rather than
/// surfacing later as UB inside the dispatch loop.
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "one giant match over every Op variant - splitting would harm readability of the validator's per-op invariants"
)]
pub(crate) fn validate_chunk(chunk: &FnChunk) -> Result<(), ValidationError> {
    let instr_count = chunk.instrs.len();
    let v_count = u32::from(chunk.register_count);
    let f_count = u32::from(chunk.float_count);
    let i_count = u32::from(chunk.int_count);
    let consts_len = chunk.consts.len();
    let f_consts_len = chunk.f64_consts.len();
    let i_consts_len = chunk.i64_consts.len();
    let globals_len = chunk.globals.len();
    let shape_names_len = chunk.shape_names.len();
    let closure_protos_len = chunk.closure_protos.len();
    let select_arms_len = chunk.select_arms.len();
    let wide_len = chunk.wide_ops.len();
    let call_cache_count = chunk.call_cache_count;
    let arith_cache_count = chunk.arith_cache_count;
    let field_cache_count = chunk.field_cache_count;

    if chunk.arity > chunk.register_count {
        return Err(ValidationError::InvalidChunkShape {
            reason: format!(
                "arity {} exceeds {} value registers",
                chunk.arity, chunk.register_count
            ),
        });
    }
    for &reg in &chunk.mut_ref_params {
        if reg >= chunk.arity {
            return Err(ValidationError::InvalidChunkShape {
                reason: format!(
                    "mutable-reference parameter register {reg} is outside arity {}",
                    chunk.arity
                ),
            });
        }
    }
    for &(param, int_reg) in &chunk.i64_params {
        if param >= chunk.arity || int_reg >= chunk.int_count {
            return Err(ValidationError::InvalidChunkShape {
                reason: format!(
                    "typed integer parameter ({param}, {int_reg}) is outside arity {} or integer register count {}",
                    chunk.arity, chunk.int_count
                ),
            });
        }
    }

    let check_v = |op_idx: usize, r: Reg| -> Result<(), ValidationError> {
        let r = u32::from(r);
        if r >= v_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: r,
                count: v_count,
                file: RegFile::Value,
            });
        }
        Ok(())
    };
    let check_f = |op_idx: usize, r: Reg| -> Result<(), ValidationError> {
        let r = u32::from(r);
        if r >= f_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: r,
                count: f_count,
                file: RegFile::Float,
            });
        }
        Ok(())
    };
    let check_i = |op_idx: usize, r: Reg| -> Result<(), ValidationError> {
        let r = u32::from(r);
        if r >= i_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: r,
                count: i_count,
                file: RegFile::Int,
            });
        }
        Ok(())
    };
    let check_v_span = |op_idx: usize, first: Reg, n: u16| -> Result<(), ValidationError> {
        if n == 0 {
            return Ok(());
        }
        let last = u32::from(first).saturating_add(u32::from(n) - 1);
        if last >= v_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: last,
                count: v_count,
                file: RegFile::Value,
            });
        }
        Ok(())
    };
    let check_i_span = |op_idx: usize, first: Reg, n: u16| -> Result<(), ValidationError> {
        if n == 0 {
            return Ok(());
        }
        let last = u32::from(first).saturating_add(u32::from(n) - 1);
        if last >= i_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: last,
                count: i_count,
                file: RegFile::Int,
            });
        }
        Ok(())
    };
    let check_f_span = |op_idx: usize, first: Reg, n: u32| -> Result<(), ValidationError> {
        if n == 0 {
            return Ok(());
        }
        let last = u32::from(first).saturating_add(n - 1);
        if last >= f_count {
            return Err(ValidationError::RegisterOutOfBounds {
                op_idx,
                reg: last,
                count: f_count,
                file: RegFile::Float,
            });
        }
        Ok(())
    };
    let check_target = |op_idx: usize, target: u32| -> Result<(), ValidationError> {
        if (target as usize) >= instr_count {
            return Err(ValidationError::PcOutOfBounds {
                op_idx,
                target: target as usize,
                instr_count,
            });
        }
        Ok(())
    };
    let check_pool =
        |op_idx: usize, idx: u32, len: usize, pool: PoolKind| -> Result<(), ValidationError> {
            if (idx as usize) >= len {
                return Err(ValidationError::ConstantOutOfBounds {
                    op_idx,
                    idx,
                    len,
                    pool,
                });
            }
            Ok(())
        };
    let check_cache =
        |op_idx: usize, idx: u16, count: u16, cache: CacheKind| -> Result<(), ValidationError> {
            if idx >= count {
                return Err(ValidationError::CacheOutOfBounds {
                    op_idx,
                    idx,
                    count,
                    cache,
                });
            }
            Ok(())
        };

    for (op_idx, op) in chunk.instrs.iter().enumerate() {
        match *op {
            // Loads / moves.
            Op::LoadConst { dst, idx } => {
                check_v(op_idx, dst)?;
                check_pool(op_idx, u32::from(idx), consts_len, PoolKind::Consts)?;
            }
            Op::LoadGlobal { dst, idx } => {
                check_v(op_idx, dst)?;
                check_pool(op_idx, u32::from(idx), globals_len, PoolKind::Globals)?;
            }
            Op::StoreStatic { name_idx, src } => {
                check_v(op_idx, src)?;
                check_pool(op_idx, u32::from(name_idx), globals_len, PoolKind::Globals)?;
            }
            Op::Move { dst, src } | Op::Deref { dst, src } | Op::CloneMapLike { dst, src } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::ClearRegs { start, count } => {
                check_v_span(op_idx, start, count)?;
            }

            // Adaptive arith - boxed value lhs/rhs/dst.
            Op::AddInt {
                dst,
                lhs,
                rhs,
                cache_idx,
            }
            | Op::SubInt {
                dst,
                lhs,
                rhs,
                cache_idx,
            }
            | Op::MulInt {
                dst,
                lhs,
                rhs,
                cache_idx,
            }
            | Op::DivInt {
                dst,
                lhs,
                rhs,
                cache_idx,
            }
            | Op::RemInt {
                dst,
                lhs,
                rhs,
                cache_idx,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, lhs)?;
                check_v(op_idx, rhs)?;
                check_cache(op_idx, cache_idx, arith_cache_count, CacheKind::Arithmetic)?;
            }
            Op::Neg { dst, operand } | Op::Not { dst, operand } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, operand)?;
            }
            Op::Eq { dst, lhs, rhs }
            | Op::Ne { dst, lhs, rhs }
            | Op::Lt { dst, lhs, rhs }
            | Op::Le { dst, lhs, rhs }
            | Op::Gt { dst, lhs, rhs }
            | Op::Ge { dst, lhs, rhs } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, lhs)?;
                check_v(op_idx, rhs)?;
            }

            // Jumps.
            Op::Jump { target } => check_target(op_idx, target)?,
            Op::BranchIf { cond, target } | Op::BranchIfNot { cond, target } => {
                check_v(op_idx, cond)?;
                check_target(op_idx, target)?;
            }

            // Calls.
            Op::Call {
                dst,
                callee,
                args,
                argc,
                cache_idx,
                ..
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, callee)?;
                check_v_span(op_idx, args, argc)?;
                check_cache(op_idx, cache_idx, call_cache_count, CacheKind::Call)?;
            }
            Op::CallGlobal {
                dst,
                global_idx,
                args,
                argc,
                cache_idx,
                ..
            } => {
                check_v(op_idx, dst)?;
                check_pool(
                    op_idx,
                    u32::from(global_idx),
                    globals_len,
                    PoolKind::Globals,
                )?;
                check_v_span(op_idx, args, argc)?;
                check_cache(op_idx, cache_idx, call_cache_count, CacheKind::Call)?;
            }
            Op::Return { value } => check_v(op_idx, value)?,
            Op::ReturnUnit => {}
            Op::Panic { msg } => {
                check_pool(op_idx, u32::from(msg), consts_len, PoolKind::Consts)?;
            }
            Op::TypeError { msg } => {
                check_pool(op_idx, u32::from(msg), consts_len, PoolKind::Consts)?;
            }
            Op::MakeClosure { dst, proto } => {
                check_v(op_idx, dst)?;
                check_pool(op_idx, proto, closure_protos_len, PoolKind::ClosureProtos)?;
            }
            Op::Select { first, count } => {
                if count == 0 {
                    return Err(ValidationError::InvalidChunkShape {
                        reason: format!("select op #{op_idx} has no arms"),
                    });
                }
                let last = first.saturating_add(u32::from(count) - 1);
                check_pool(op_idx, last, select_arms_len, PoolKind::SelectArms)?;
                let start = first as usize;
                for arm in &chunk.select_arms[start..start + count as usize] {
                    check_target(op_idx, arm.body_block)?;
                    match arm.kind {
                        crate::bytecode::SelectArmKind::Recv => {
                            check_v(op_idx, arm.channel_reg)?;
                            check_v(op_idx, arm.bind_reg)?;
                        }
                        crate::bytecode::SelectArmKind::Send => {
                            check_v(op_idx, arm.channel_reg)?;
                            check_v(op_idx, arm.value_reg)?;
                        }
                        crate::bytecode::SelectArmKind::Default => {}
                    }
                }
            }
            // `slot` indexes the process-global coverage table, not a
            // per-chunk pool, so there is nothing chunk-local to bound.
            Op::CovHit { .. } => {}
            Op::MethodCall {
                dst,
                receiver,
                name_idx,
                args,
                argc,
                cache_idx,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
                check_pool(op_idx, u32::from(name_idx), globals_len, PoolKind::Globals)?;
                check_v_span(op_idx, args, argc)?;
                check_cache(op_idx, cache_idx, call_cache_count, CacheKind::Call)?;
            }

            // Fused super-instructions over Value registers.
            Op::StreamWriteByte {
                dst,
                stream_reg,
                byte_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, stream_reg)?;
                check_v(op_idx, byte_reg)?;
            }
            Op::U8VecSetByte {
                dst,
                u8vec_reg,
                idx_reg,
                byte_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, u8vec_reg)?;
                check_v(op_idx, idx_reg)?;
                check_v(op_idx, byte_reg)?;
            }
            Op::U8VecGetByte {
                dst_i,
                u8vec_reg,
                idx_reg,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, u8vec_reg)?;
                check_v(op_idx, idx_reg)?;
            }
            Op::StrSubstring {
                dst,
                recv_reg,
                start_reg,
                end_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, recv_reg)?;
                check_v(op_idx, start_reg)?;
                check_v(op_idx, end_reg)?;
            }
            Op::MapIncMethod {
                dst,
                map_reg,
                key_reg,
                by_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, map_reg)?;
                check_v(op_idx, key_reg)?;
                check_v(op_idx, by_reg)?;
            }
            Op::MapInsert {
                dst,
                map_reg,
                key_reg,
                value_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, map_reg)?;
                check_v(op_idx, key_reg)?;
                check_v(op_idx, value_reg)?;
            }
            Op::MapInc {
                dst,
                map_reg,
                key_reg,
                by_reg,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, map_reg)?;
                check_v(op_idx, key_reg)?;
                check_v(op_idx, by_reg)?;
            }
            Op::Wide { idx } => {
                check_pool(op_idx, u32::from(idx), wide_len, PoolKind::WideOps)?;
                validate_wide_op(
                    op_idx,
                    &chunk.wide_ops[idx as usize],
                    &check_v,
                    &check_f,
                    consts_len,
                    chunk,
                )?;
            }

            Op::BuildIntArray {
                dst_v,
                first_i,
                count,
            }
            | Op::BuildByteArray {
                dst_v,
                first_i,
                count,
            } => {
                check_v(op_idx, dst_v)?;
                check_i_span(op_idx, first_i, count)?;
            }
            Op::BuildByteArrayRepeat {
                dst_v,
                value_i,
                count_v,
            } => {
                check_v(op_idx, dst_v)?;
                check_i(op_idx, value_i)?;
                check_v(op_idx, count_v)?;
            }
            Op::CheckNonNegativeCapacity { capacity_i } => {
                check_i(op_idx, capacity_i)?;
            }
            Op::BuildTuple { dst, first, count } => {
                check_v(op_idx, dst)?;
                check_v_span(op_idx, first, count)?;
            }
            Op::BuildArray { dst, first, count } => {
                check_v(op_idx, dst)?;
                check_v_span(op_idx, first, count)?;
            }
            Op::BuildArrayRepeat { dst, value, count } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, value)?;
                check_v(op_idx, count)?;
            }
            Op::BuildRange {
                dst, start, end, ..
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, start)?;
                check_v(op_idx, end)?;
            }
            Op::BuildVariant1 {
                dst,
                name_idx,
                field,
                ..
            } => {
                check_v(op_idx, dst)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_v(op_idx, field)?;
            }
            Op::BuildVariant2 {
                dst,
                name_idx,
                first,
                second,
                ..
            } => {
                check_v(op_idx, dst)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_v(op_idx, first)?;
                check_v(op_idx, second)?;
            }
            Op::IntToFloatF64 { dst_f, src_i } => {
                check_f(op_idx, dst_f)?;
                check_i(op_idx, src_i)?;
            }
            Op::DivF64ByI64 {
                dst_f,
                lhs_f,
                rhs_i,
            } => {
                check_f(op_idx, dst_f)?;
                check_f(op_idx, lhs_f)?;
                check_i(op_idx, rhs_i)?;
            }
            Op::FloatToIntI64 { dst_i, src_f } => {
                check_i(op_idx, dst_i)?;
                check_f(op_idx, src_f)?;
            }
            Op::TruncCastI64 { dst_i, src_i, .. } => {
                check_i(op_idx, dst_i)?;
                check_i(op_idx, src_i)?;
            }
            Op::CastScalar { dst, src, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::CellNew { dst, src } | Op::CellNewMove { dst, src } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::CellTake { dst, cell }
            | Op::CaptureCellGet { dst, cell }
            | Op::CaptureCellTake { dst, cell } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, cell)?;
            }
            Op::CaptureCellNew { dst, src } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::CaptureCellSet { cell, src } => {
                check_v(op_idx, cell)?;
                check_v(op_idx, src)?;
            }
            Op::VariantFieldSet { receiver, src, .. } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, src)?;
            }
            Op::IntArrayGetI64 {
                dst_i,
                base,
                index_i,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
            }
            Op::IntArraySetI64 {
                base,
                index_i,
                value_i,
            } => {
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
                check_i(op_idx, value_i)?;
            }
            Op::IntArraySwap { base, i_i, j_i } | Op::FloatVecSwap { base, i_i, j_i } => {
                check_v(op_idx, base)?;
                check_i(op_idx, i_i)?;
                check_i(op_idx, j_i)?;
            }
            Op::BuildFloatVec {
                dst_v,
                first_f,
                count,
            } => {
                check_v(op_idx, dst_v)?;
                check_f_span(op_idx, first_f, u32::from(count))?;
            }
            Op::FloatVecGetF64 {
                dst_f,
                base,
                index_i,
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
            }
            Op::FloatVecSetF64 {
                base,
                index_i,
                value_f,
            } => {
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
                check_f(op_idx, value_f)?;
            }
            Op::BuildIntMap { dst_v } | Op::BuildStrIntMap { dst_v } => check_v(op_idx, dst_v)?,
            Op::IntMapInc {
                dst_i,
                map_reg,
                key_i,
                by_i,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, map_reg)?;
                check_i(op_idx, key_i)?;
                check_i(op_idx, by_i)?;
            }
            Op::IntMapGetOr {
                dst_i,
                map_reg,
                key_i,
                default_i,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, map_reg)?;
                check_i(op_idx, key_i)?;
                check_i(op_idx, default_i)?;
            }
            Op::IntMapInsert {
                dst_v,
                map_reg,
                key_i,
                value_i,
            } => {
                check_v(op_idx, dst_v)?;
                check_v(op_idx, map_reg)?;
                check_i(op_idx, key_i)?;
                check_i(op_idx, value_i)?;
            }
            Op::IntMapLen { dst_i, map_reg } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, map_reg)?;
            }
            Op::IntMapContainsKey {
                dst_v,
                map_reg,
                key_i,
            } => {
                check_v(op_idx, dst_v)?;
                check_v(op_idx, map_reg)?;
                check_i(op_idx, key_i)?;
            }

            Op::IndexGet { dst, base, index } | Op::IndexGetChecked { dst, base, index } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
            }
            Op::StrByteAt { dst, recv, idx } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, recv)?;
                check_v(op_idx, idx)?;
            }
            Op::StrByteAtI64 { dst_i, recv, idx_i } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, recv)?;
                check_i(op_idx, idx_i)?;
            }
            Op::StrByteAtAddI64 {
                dst_i,
                lhs_i,
                recv,
                idx_i,
            } => {
                check_i(op_idx, dst_i)?;
                check_i(op_idx, lhs_i)?;
                check_v(op_idx, recv)?;
                check_i(op_idx, idx_i)?;
            }
            Op::StrLenI64 { dst_i, recv } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, recv)?;
            }
            Op::IndexSet { base, index, value } => {
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_v(op_idx, value)?;
            }
            Op::FieldGet {
                dst,
                receiver,
                name_idx,
                cache_idx,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_cache(op_idx, cache_idx, field_cache_count, CacheKind::Field)?;
            }
            Op::FieldSet {
                receiver,
                name_idx,
                value,
            } => {
                check_v(op_idx, receiver)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_v(op_idx, value)?;
            }
            Op::VecPush { receiver, value } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, value)?;
            }
            Op::StrAppend { receiver, value }
            | Op::StrPush {
                receiver, value, ..
            } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, value)?;
            }
            Op::StrConcatI64 {
                dst,
                prefix,
                value_i,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, prefix)?;
                check_i(op_idx, value_i)?;
            }
            Op::VecPop { dst, receiver } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
            }
            Op::VecInsert {
                dst,
                receiver,
                index,
                value,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
                check_v(op_idx, index)?;
                check_v(op_idx, value)?;
            }
            Op::VecSwap {
                dst,
                receiver,
                a,
                b,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
                check_v(op_idx, a)?;
                check_v(op_idx, b)?;
            }
            Op::VecSwapDiscard { receiver, a, b } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, a)?;
                check_v(op_idx, b)?;
            }
            Op::VecRemove { receiver, index } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, index)?;
            }
            Op::VecRemoveAt {
                dst,
                receiver,
                index,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
                check_v(op_idx, index)?;
            }
            Op::TupleIndex { dst, receiver, .. } | Op::TupleTailIndex { dst, receiver, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
            }
            Op::TupleSet {
                receiver, value, ..
            } => {
                check_v(op_idx, receiver)?;
                check_v(op_idx, value)?;
            }
            Op::IndexedFieldSet {
                base,
                index,
                name_idx,
                value,
            } => {
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_v(op_idx, value)?;
            }

            // Unboxed f64 register-file ops.
            Op::LoadConstF64 { dst_f, idx } => {
                check_f(op_idx, dst_f)?;
                check_pool(op_idx, u32::from(idx), f_consts_len, PoolKind::F64Consts)?;
            }
            Op::AddF64 {
                dst_f,
                lhs_f,
                rhs_f,
            }
            | Op::SubF64 {
                dst_f,
                lhs_f,
                rhs_f,
            }
            | Op::MulF64 {
                dst_f,
                lhs_f,
                rhs_f,
            }
            | Op::DivF64 {
                dst_f,
                lhs_f,
                rhs_f,
            } => {
                check_f(op_idx, dst_f)?;
                check_f(op_idx, lhs_f)?;
                check_f(op_idx, rhs_f)?;
            }
            Op::NegF64 { dst_f, src_f }
            | Op::SqrtF64 { dst_f, src_f }
            | Op::SinF64 { dst_f, src_f }
            | Op::CosF64 { dst_f, src_f }
            | Op::AbsF64 { dst_f, src_f }
            | Op::FloorF64 { dst_f, src_f }
            | Op::CeilF64 { dst_f, src_f }
            | Op::ExpF64 { dst_f, src_f }
            | Op::LnF64 { dst_f, src_f }
            | Op::MoveF64 { dst_f, src_f } => {
                check_f(op_idx, dst_f)?;
                check_f(op_idx, src_f)?;
            }
            Op::LtF64 {
                dst_v,
                lhs_f,
                rhs_f,
            }
            | Op::LeF64 {
                dst_v,
                lhs_f,
                rhs_f,
            }
            | Op::GtF64 {
                dst_v,
                lhs_f,
                rhs_f,
            }
            | Op::GeF64 {
                dst_v,
                lhs_f,
                rhs_f,
            }
            | Op::EqF64 {
                dst_v,
                lhs_f,
                rhs_f,
            }
            | Op::NeF64 {
                dst_v,
                lhs_f,
                rhs_f,
            } => {
                check_v(op_idx, dst_v)?;
                check_f(op_idx, lhs_f)?;
                check_f(op_idx, rhs_f)?;
            }
            Op::UnboxF64 {
                dst_f,
                src_v,
                peer_v,
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, src_v)?;
                if let Some(peer_v) = peer_v {
                    check_v(op_idx, peer_v)?;
                }
            }
            Op::BoxF64 { dst_v, src_f } => {
                check_v(op_idx, dst_v)?;
                check_f(op_idx, src_f)?;
            }
            Op::MulAddF64 {
                dst_f,
                a_f,
                b_f,
                c_f,
            }
            | Op::MulSubF64 {
                dst_f,
                a_f,
                b_f,
                c_f,
            } => {
                check_f(op_idx, dst_f)?;
                check_f(op_idx, a_f)?;
                check_f(op_idx, b_f)?;
                check_f(op_idx, c_f)?;
            }

            // Unboxed i64 register-file ops.
            Op::LoadConstI64 { dst_i, idx } => {
                check_i(op_idx, dst_i)?;
                check_pool(op_idx, u32::from(idx), i_consts_len, PoolKind::I64Consts)?;
            }
            Op::AddI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::CheckedAddI64 {
                dst_i,
                lhs_i,
                rhs_i,
                ..
            }
            | Op::SubI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::CheckedSubI64 {
                dst_i,
                lhs_i,
                rhs_i,
                ..
            }
            | Op::MulI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::CheckedMulI64 {
                dst_i,
                lhs_i,
                rhs_i,
                ..
            }
            | Op::DivI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::RemI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::DivU64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::RemU64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::BitAndI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::BitOrI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::BitXorI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::ShlI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::ShrI64 {
                dst_i,
                lhs_i,
                rhs_i,
            }
            | Op::ShrU64 {
                dst_i,
                lhs_i,
                rhs_i,
            } => {
                check_i(op_idx, dst_i)?;
                check_i(op_idx, lhs_i)?;
                check_i(op_idx, rhs_i)?;
            }
            Op::NegI64 { dst_i, src_i } | Op::MoveI64 { dst_i, src_i } => {
                check_i(op_idx, dst_i)?;
                check_i(op_idx, src_i)?;
            }
            Op::ArithImmI64 { dst_i, lhs_i, .. } => {
                check_i(op_idx, dst_i)?;
                check_i(op_idx, lhs_i)?;
            }
            Op::LtI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::LeI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::GtI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::GeI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::EqI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::NeI64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::LtU64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::LeU64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::GtU64 {
                dst_v,
                lhs_i,
                rhs_i,
            }
            | Op::GeU64 {
                dst_v,
                lhs_i,
                rhs_i,
            } => {
                check_v(op_idx, dst_v)?;
                check_i(op_idx, lhs_i)?;
                check_i(op_idx, rhs_i)?;
            }
            Op::UnboxI64 {
                dst_i,
                src_v,
                peer_v,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, src_v)?;
                if let Some(peer_v) = peer_v {
                    check_v(op_idx, peer_v)?;
                }
            }
            Op::BoxI64 { dst_v, src_i } => {
                check_v(op_idx, dst_v)?;
                check_i(op_idx, src_i)?;
            }

            // Phase 2 fused / typed field access.
            Op::FieldGetF64 {
                dst_f,
                receiver,
                name_idx,
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, receiver)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
            }
            Op::FieldGetI64 {
                dst_i,
                receiver,
                name_idx,
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, receiver)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
            }
            Op::Struct2I64 {
                dst,
                type_name,
                field0,
                field1,
                first_i,
                second_i,
            } => {
                check_v(op_idx, dst)?;
                check_i(op_idx, first_i)?;
                check_i(op_idx, second_i)?;
                check_pool(
                    op_idx,
                    u32::from(type_name),
                    shape_names_len,
                    PoolKind::ShapeNames,
                )?;
                check_pool(
                    op_idx,
                    u32::from(field0),
                    shape_names_len,
                    PoolKind::ShapeNames,
                )?;
                check_pool(
                    op_idx,
                    u32::from(field1),
                    shape_names_len,
                    PoolKind::ShapeNames,
                )?;
            }
            Op::IndexedFieldGet {
                dst,
                base,
                index,
                name_idx,
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
            }
            Op::IndexedFieldGetF64 {
                dst_f,
                base,
                index,
                name_idx,
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
            }
            Op::IndexedFieldSetF64 {
                base,
                index,
                name_idx,
                value_f,
            } => {
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_pool(op_idx, u32::from(name_idx), consts_len, PoolKind::Consts)?;
                check_f(op_idx, value_f)?;
            }
            Op::IndexedFieldGetF64ByOffset {
                dst_f, base, index, ..
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
            }
            Op::IndexedFieldSetF64ByOffset {
                base,
                index,
                value_f,
                ..
            } => {
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_f(op_idx, value_f)?;
            }

            // Fused compare-and-branch.
            Op::BranchIfLtI64 {
                lhs_i,
                rhs_i,
                target,
            }
            | Op::BranchIfGeI64 {
                lhs_i,
                rhs_i,
                target,
            }
            | Op::BranchIfGtI64 {
                lhs_i,
                rhs_i,
                target,
            } => {
                check_i(op_idx, lhs_i)?;
                check_i(op_idx, rhs_i)?;
                check_target(op_idx, target)?;
            }
            Op::BranchIfLtF64 {
                lhs_f,
                rhs_f,
                target,
            }
            | Op::BranchIfGeF64 {
                lhs_f,
                rhs_f,
                target,
            } => {
                check_f(op_idx, lhs_f)?;
                check_f(op_idx, rhs_f)?;
                check_target(op_idx, target)?;
            }
            Op::IncJumpIfLtI64 {
                counter_i,
                end_i,
                target,
            }
            | Op::IncJumpIfLeI64 {
                counter_i,
                end_i,
                target,
            } => {
                check_i(op_idx, counter_i)?;
                check_i(op_idx, end_i)?;
                check_target(op_idx, target)?;
            }

            Op::FieldGetF64ByOffset {
                dst_f, receiver, ..
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, receiver)?;
            }
            Op::FieldGetI64ByOffset {
                dst_i, receiver, ..
            } => {
                check_i(op_idx, dst_i)?;
                check_v(op_idx, receiver)?;
            }
            Op::FieldSetI64ByOffset {
                receiver, value_i, ..
            } => {
                check_v(op_idx, receiver)?;
                check_i(op_idx, value_i)?;
            }
            Op::FlatGetF64 {
                dst_f, base, index, ..
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
            }
            Op::FlatSetF64 {
                base,
                index,
                value_f,
                ..
            } => {
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
                check_f(op_idx, value_f)?;
            }
            Op::FlatGetF64I {
                dst_f,
                base,
                index_i,
                ..
            } => {
                check_f(op_idx, dst_f)?;
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
            }
            Op::FlatSetF64I {
                base,
                index_i,
                value_f,
                ..
            } => {
                check_v(op_idx, base)?;
                check_i(op_idx, index_i)?;
                check_f(op_idx, value_f)?;
            }
            Op::I64ToUint { dst_v, src_i } => {
                check_v(op_idx, dst_v)?;
                check_i(op_idx, src_i)?;
            }
            Op::UintLeaves { dst, src, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::VariantIs {
                dst, src, name_idx, ..
            } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
                check_pool(
                    op_idx,
                    u32::from(name_idx),
                    shape_names_len,
                    PoolKind::ShapeNames,
                )?;
            }
            Op::VariantField { dst, src, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::StructIs { dst, src, name_idx } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
                check_pool(
                    op_idx,
                    u32::from(name_idx),
                    shape_names_len,
                    PoolKind::ShapeNames,
                )?;
            }
            Op::MoveConsume { dst, src } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::VariantFieldConsume { dst, src, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, src)?;
            }
            Op::IndexGetConsume { dst, base, index } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, base)?;
                check_v(op_idx, index)?;
            }
            Op::TupleIndexConsume { dst, receiver, .. } => {
                check_v(op_idx, dst)?;
                check_v(op_idx, receiver)?;
            }
        }
    }

    validate_control_flow(chunk)?;
    validate_register_initialization(chunk)
}

/// Verifies the reachable control-flow graph after per-op target bounds are
/// known valid. Unreachable instructions are permitted: lowering deliberately
/// keeps some label scaffolding and an unconditional jump can make a following
/// return unreachable. What must never happen is a reachable ordinary op at
/// the final instruction, because VM dispatch would then advance outside the
/// chunk instead of reaching an explicit terminator.
fn validate_control_flow(chunk: &FnChunk) -> Result<(), ValidationError> {
    if chunk.instrs.is_empty() {
        return Err(ValidationError::InvalidChunkShape {
            reason: "chunk has no instructions".to_string(),
        });
    }

    let mut reachable = vec![false; chunk.instrs.len()];
    let mut pending = vec![0usize];
    while let Some(op_idx) = pending.pop() {
        if reachable[op_idx] {
            continue;
        }
        reachable[op_idx] = true;
        let mut add_successor = |target: usize| {
            if !reachable[target] {
                pending.push(target);
            }
        };
        match chunk.instrs[op_idx] {
            Op::Return { .. } | Op::ReturnUnit | Op::Panic { .. } | Op::TypeError { .. } => {}
            Op::Jump { target } => add_successor(target as usize),
            Op::BranchIf { target, .. }
            | Op::BranchIfNot { target, .. }
            | Op::BranchIfLtI64 { target, .. }
            | Op::BranchIfGeI64 { target, .. }
            | Op::BranchIfGtI64 { target, .. }
            | Op::BranchIfLtF64 { target, .. }
            | Op::BranchIfGeF64 { target, .. }
            | Op::IncJumpIfLtI64 { target, .. }
            | Op::IncJumpIfLeI64 { target, .. } => {
                add_successor(target as usize);
                if let Some(next) = op_idx
                    .checked_add(1)
                    .filter(|next| *next < chunk.instrs.len())
                {
                    add_successor(next);
                } else {
                    return Err(ValidationError::ControlFlowFallsOffEnd { op_idx });
                }
            }
            Op::Select { first, count } => {
                let start = first as usize;
                for arm in &chunk.select_arms[start..start + count as usize] {
                    add_successor(arm.body_block as usize);
                }
            }
            _ => {
                if let Some(next) = op_idx
                    .checked_add(1)
                    .filter(|next| *next < chunk.instrs.len())
                {
                    add_successor(next);
                } else {
                    return Err(ValidationError::ControlFlowFallsOffEnd { op_idx });
                }
            }
        }
    }
    Ok(())
}

/// Per-instruction register reads, writes, and explicit invalidations. Keeping
/// this separate from the bounds pass makes the write-before-read audit
/// exhaustive without adding any cost to bytecode execution. The compiler
/// reads the same table when it brackets an instruction that names a
/// capture-cell binding with the cell's load / store.
#[derive(Default)]
pub(crate) struct RegisterEffects {
    pub(crate) v_reads: Vec<Reg>,
    f_reads: Vec<Reg>,
    i_reads: Vec<Reg>,
    pub(crate) v_writes: Vec<Reg>,
    f_writes: Vec<Reg>,
    i_writes: Vec<Reg>,
    v_clears: Vec<Reg>,
}

#[derive(Clone, PartialEq, Eq)]
struct RegisterInitialization {
    values: Vec<bool>,
    floats: Vec<bool>,
    ints: Vec<bool>,
}

impl RegisterInitialization {
    fn entry(chunk: &FnChunk) -> Self {
        let mut values = vec![false; usize::from(chunk.register_count)];
        values[..usize::from(chunk.arity)].fill(true);
        let mut ints = vec![false; usize::from(chunk.int_count)];
        for &(_, int_reg) in &chunk.i64_params {
            ints[usize::from(int_reg)] = true;
        }
        Self {
            values,
            floats: vec![false; usize::from(chunk.float_count)],
            ints,
        }
    }

    fn intersect_assign(&mut self, other: &Self) -> bool {
        let before = self.clone();
        for (left, right) in self.values.iter_mut().zip(&other.values) {
            *left &= *right;
        }
        for (left, right) in self.floats.iter_mut().zip(&other.floats) {
            *left &= *right;
        }
        for (left, right) in self.ints.iter_mut().zip(&other.ints) {
            *left &= *right;
        }
        *self != before
    }

    fn apply(&mut self, effects: &RegisterEffects) {
        for &reg in &effects.v_clears {
            self.values[usize::from(reg)] = false;
        }
        for &reg in &effects.v_writes {
            self.values[usize::from(reg)] = true;
        }
        for &reg in &effects.f_writes {
            self.floats[usize::from(reg)] = true;
        }
        for &reg in &effects.i_writes {
            self.ints[usize::from(reg)] = true;
        }
    }
}

fn add_v_span(registers: &mut Vec<Reg>, first: Reg, count: u16) {
    registers.extend((0..count).map(|offset| first + offset));
}

fn add_i_span(registers: &mut Vec<Reg>, first: Reg, count: u16) {
    registers.extend((0..count).map(|offset| first + offset));
}

fn add_f_span(registers: &mut Vec<Reg>, first: Reg, count: u32) {
    registers.extend((0..count).map(|offset| first + offset as Reg));
}

/// Proves every reachable source register was initialized along all paths.
/// Value-register parameters are initialized at entry; typed registers have no
/// ABI parameters and must therefore be written by bytecode first. A meet at
/// each control-flow join uses intersection, the standard definite-assignment
/// rule. The pass also models consuming ops and `ClearRegs` as invalidations,
/// preventing a later compiler regression from silently reading `Void`.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive opcode table is the validation contract"
)]
fn validate_register_initialization(chunk: &FnChunk) -> Result<(), ValidationError> {
    let mut incoming = vec![None; chunk.instrs.len()];
    incoming[0] = Some(RegisterInitialization::entry(chunk));
    let mut pending = vec![0usize];

    while let Some(op_idx) = pending.pop() {
        let state = incoming[op_idx].as_ref().expect("queued state").clone();
        let effects = op_effects(chunk, op_idx);
        let mut out = state;
        out.apply(&effects);

        let mut propagate = |target: usize, next: RegisterInitialization| {
            if target == 0 {
                // Entry always includes the initial call-frame state. A loop
                // back-edge can only add facts, never weaken that requirement.
                return;
            }
            let changed = match &mut incoming[target] {
                Some(current) => current.intersect_assign(&next),
                slot @ None => {
                    *slot = Some(next);
                    true
                }
            };
            if changed {
                pending.push(target);
            }
        };

        match chunk.instrs[op_idx] {
            Op::Return { .. } | Op::ReturnUnit | Op::Panic { .. } | Op::TypeError { .. } => {}
            Op::Jump { target } => propagate(target as usize, out),
            Op::BranchIf { target, .. }
            | Op::BranchIfNot { target, .. }
            | Op::BranchIfLtI64 { target, .. }
            | Op::BranchIfGeI64 { target, .. }
            | Op::BranchIfGtI64 { target, .. }
            | Op::BranchIfLtF64 { target, .. }
            | Op::BranchIfGeF64 { target, .. }
            | Op::IncJumpIfLtI64 { target, .. }
            | Op::IncJumpIfLeI64 { target, .. } => {
                propagate(target as usize, out.clone());
                propagate(op_idx + 1, out);
            }
            Op::Select { first, count } => {
                for arm in &chunk.select_arms[first as usize..first as usize + count as usize] {
                    let mut arm_out = out.clone();
                    if matches!(arm.kind, crate::bytecode::SelectArmKind::Recv) {
                        arm_out.values[usize::from(arm.bind_reg)] = true;
                    }
                    propagate(arm.body_block as usize, arm_out);
                }
            }
            _ => propagate(op_idx + 1, out),
        }
    }

    for (op_idx, state) in incoming.into_iter().enumerate() {
        let Some(state) = state else { continue };
        let effects = op_effects(chunk, op_idx);
        // The boxed register file is materialized as `Value::Void` on every
        // frame entry. Reading a compiler-temporary before a write is still a
        // lowering bug, but it cannot expose uninitialized host memory; a
        // conservative CFG intersection here also rejects valid loop and
        // match lowering that deliberately reuses a Void scratch slot. The
        // unboxed files below are the release-safety boundary and must remain
        // definitely assigned before every read.
        for reg in effects.f_reads {
            if !state.floats[usize::from(reg)] {
                return Err(ValidationError::RegisterUninitialized {
                    op_idx,
                    reg: u32::from(reg),
                    file: RegFile::Float,
                });
            }
        }
        for reg in effects.i_reads {
            if !state.ints[usize::from(reg)] {
                return Err(ValidationError::RegisterUninitialized {
                    op_idx,
                    reg: u32::from(reg),
                    file: RegFile::Int,
                });
            }
        }
    }
    Ok(())
}

/// [`register_effects`] for the instruction at `op_idx` of `chunk`.
fn op_effects(chunk: &FnChunk, op_idx: usize) -> RegisterEffects {
    register_effects(
        chunk.instrs[op_idx],
        &chunk.closure_protos,
        &chunk.select_arms,
        &chunk.wide_ops,
    )
}

/// Register reads, writes, and invalidations performed by one instruction.
/// `closure_protos` / `select_arms` / `wide_ops` are the side tables whose
/// entries carry the remaining operands of `Op::MakeClosure`, `Op::Select`,
/// and `Op::Wide`.
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive table keeps every opcode's reads explicit"
)]
pub(crate) fn register_effects(
    op: Op,
    closure_protos: &[crate::bytecode::ClosureProto],
    select_arms: &[crate::bytecode::SelectArmMeta],
    wide_ops: &[WideOp],
) -> RegisterEffects {
    let mut effect = RegisterEffects::default();
    match op {
        Op::LoadConst { dst, .. }
        | Op::LoadGlobal { dst, .. }
        | Op::MakeClosure { dst, .. }
        | Op::BuildTuple { dst, .. }
        | Op::BuildArray { dst, .. }
        | Op::BuildArrayRepeat { dst, .. }
        | Op::BuildRange { dst, .. }
        | Op::BuildVariant1 { dst, .. }
        | Op::BuildVariant2 { dst, .. }
        | Op::CastScalar { dst, .. }
        | Op::CellNew { dst, .. }
        | Op::CellNewMove { dst, .. }
        | Op::CellTake { dst, .. }
        | Op::CaptureCellNew { dst, .. }
        | Op::CaptureCellGet { dst, .. }
        | Op::CaptureCellTake { dst, .. }
        | Op::IndexGet { dst, .. }
        | Op::IndexGetChecked { dst, .. }
        | Op::StrByteAt { dst, .. }
        | Op::FieldGet { dst, .. }
        | Op::VecPop { dst, .. }
        | Op::VecRemoveAt { dst, .. }
        | Op::TupleIndex { dst, .. }
        | Op::TupleTailIndex { dst, .. }
        | Op::IndexedFieldGet { dst, .. }
        | Op::VariantIs { dst, .. }
        | Op::VariantField { dst, .. }
        | Op::StructIs { dst, .. }
        | Op::MoveConsume { dst, .. }
        | Op::VariantFieldConsume { dst, .. }
        | Op::IndexGetConsume { dst, .. }
        | Op::TupleIndexConsume { dst, .. }
        | Op::StreamWriteByte { dst, .. }
        | Op::U8VecSetByte { dst, .. }
        | Op::StrSubstring { dst, .. }
        | Op::MapIncMethod { dst, .. }
        | Op::MapInsert { dst, .. }
        | Op::MapInc { dst, .. }
        | Op::MethodCall { dst, .. }
        | Op::Call { dst, .. }
        | Op::CallGlobal { dst, .. }
        | Op::AddInt { dst, .. }
        | Op::SubInt { dst, .. }
        | Op::MulInt { dst, .. }
        | Op::DivInt { dst, .. }
        | Op::RemInt { dst, .. }
        | Op::Neg { dst, .. }
        | Op::Not { dst, .. }
        | Op::Deref { dst, .. }
        | Op::Move { dst, .. }
        | Op::CloneMapLike { dst, .. }
        | Op::UintLeaves { dst, .. }
        | Op::Eq { dst, .. }
        | Op::Ne { dst, .. }
        | Op::Lt { dst, .. }
        | Op::Le { dst, .. }
        | Op::Gt { dst, .. }
        | Op::Ge { dst, .. } => effect.v_writes.push(dst),
        Op::BuildIntArray { dst_v, .. }
        | Op::BuildByteArray { dst_v, .. }
        | Op::BuildByteArrayRepeat { dst_v, .. }
        | Op::BuildFloatVec { dst_v, .. }
        | Op::BuildIntMap { dst_v }
        | Op::BuildStrIntMap { dst_v }
        | Op::IntMapInsert { dst_v, .. }
        | Op::BoxF64 { dst_v, .. }
        | Op::BoxI64 { dst_v, .. }
        | Op::I64ToUint { dst_v, .. }
        | Op::LtF64 { dst_v, .. }
        | Op::LeF64 { dst_v, .. }
        | Op::GtF64 { dst_v, .. }
        | Op::GeF64 { dst_v, .. }
        | Op::EqF64 { dst_v, .. }
        | Op::NeF64 { dst_v, .. }
        | Op::LtI64 { dst_v, .. }
        | Op::LeI64 { dst_v, .. }
        | Op::GtI64 { dst_v, .. }
        | Op::GeI64 { dst_v, .. }
        | Op::EqI64 { dst_v, .. }
        | Op::NeI64 { dst_v, .. }
        | Op::LtU64 { dst_v, .. }
        | Op::LeU64 { dst_v, .. }
        | Op::GtU64 { dst_v, .. }
        | Op::GeU64 { dst_v, .. } => effect.v_writes.push(dst_v),
        Op::Struct2I64 {
            dst,
            first_i,
            second_i,
            ..
        } => {
            effect.v_writes.push(dst);
            effect.i_reads.extend([first_i, second_i]);
        }
        Op::LoadConstF64 { dst_f, .. }
        | Op::IntToFloatF64 { dst_f, .. }
        | Op::UnboxF64 { dst_f, .. }
        | Op::FieldGetF64 { dst_f, .. }
        | Op::IndexedFieldGetF64 { dst_f, .. }
        | Op::IndexedFieldGetF64ByOffset { dst_f, .. }
        | Op::FieldGetF64ByOffset { dst_f, .. }
        | Op::FlatGetF64 { dst_f, .. }
        | Op::FlatGetF64I { dst_f, .. }
        | Op::FloatVecGetF64 { dst_f, .. }
        | Op::NegF64 { dst_f, .. }
        | Op::SqrtF64 { dst_f, .. }
        | Op::SinF64 { dst_f, .. }
        | Op::CosF64 { dst_f, .. }
        | Op::AbsF64 { dst_f, .. }
        | Op::FloorF64 { dst_f, .. }
        | Op::CeilF64 { dst_f, .. }
        | Op::ExpF64 { dst_f, .. }
        | Op::LnF64 { dst_f, .. }
        | Op::MoveF64 { dst_f, .. }
        | Op::AddF64 { dst_f, .. }
        | Op::SubF64 { dst_f, .. }
        | Op::MulF64 { dst_f, .. }
        | Op::DivF64 { dst_f, .. }
        | Op::DivF64ByI64 { dst_f, .. }
        | Op::MulAddF64 { dst_f, .. }
        | Op::MulSubF64 { dst_f, .. } => effect.f_writes.push(dst_f),
        Op::LoadConstI64 { dst_i, .. }
        | Op::FloatToIntI64 { dst_i, .. }
        | Op::TruncCastI64 { dst_i, .. }
        | Op::UnboxI64 { dst_i, .. }
        | Op::U8VecGetByte { dst_i, .. }
        | Op::IntArrayGetI64 { dst_i, .. }
        | Op::StrByteAtI64 { dst_i, .. }
        | Op::StrByteAtAddI64 { dst_i, .. }
        | Op::StrLenI64 { dst_i, .. }
        | Op::IntMapInc { dst_i, .. }
        | Op::IntMapGetOr { dst_i, .. }
        | Op::IntMapLen { dst_i, .. }
        | Op::FieldGetI64 { dst_i, .. }
        | Op::FieldGetI64ByOffset { dst_i, .. }
        | Op::NegI64 { dst_i, .. }
        | Op::MoveI64 { dst_i, .. }
        | Op::ArithImmI64 { dst_i, .. }
        | Op::AddI64 { dst_i, .. }
        | Op::CheckedAddI64 { dst_i, .. }
        | Op::SubI64 { dst_i, .. }
        | Op::CheckedSubI64 { dst_i, .. }
        | Op::MulI64 { dst_i, .. }
        | Op::CheckedMulI64 { dst_i, .. }
        | Op::DivI64 { dst_i, .. }
        | Op::RemI64 { dst_i, .. }
        | Op::DivU64 { dst_i, .. }
        | Op::RemU64 { dst_i, .. }
        | Op::BitAndI64 { dst_i, .. }
        | Op::BitOrI64 { dst_i, .. }
        | Op::BitXorI64 { dst_i, .. }
        | Op::ShlI64 { dst_i, .. }
        | Op::ShrI64 { dst_i, .. }
        | Op::ShrU64 { dst_i, .. } => effect.i_writes.push(dst_i),
        _ => {}
    }

    match op {
        // `Return` intentionally permits an untouched Value register: unit
        // expression tails are represented by the frame's `Value::Void`
        // sentinel. Typed register files have no such semantic zero value.
        Op::StoreStatic { src, .. } => effect.v_reads.push(src),
        Op::Move { src, .. }
        | Op::CloneMapLike { src, .. }
        | Op::UintLeaves { src, .. }
        | Op::Deref { src, .. }
        | Op::Neg { operand: src, .. }
        | Op::Not { operand: src, .. }
        | Op::CastScalar { src, .. }
        | Op::CellNew { src, .. }
        | Op::CellNewMove { src, .. }
        | Op::CaptureCellNew { src, .. }
        | Op::MoveConsume { src, .. }
        | Op::VariantIs { src, .. }
        | Op::VariantField { src, .. }
        | Op::StructIs { src, .. }
        | Op::VariantFieldConsume { src, .. } => effect.v_reads.push(src),
        Op::VariantFieldSet { receiver, src, .. } => {
            effect.v_reads.push(src);
            effect.v_reads.push(receiver);
            effect.v_writes.push(receiver);
        }
        Op::AddInt { lhs, rhs, .. }
        | Op::SubInt { lhs, rhs, .. }
        | Op::MulInt { lhs, rhs, .. }
        | Op::DivInt { lhs, rhs, .. }
        | Op::RemInt { lhs, rhs, .. }
        | Op::Eq { lhs, rhs, .. }
        | Op::Ne { lhs, rhs, .. }
        | Op::Lt { lhs, rhs, .. }
        | Op::Le { lhs, rhs, .. }
        | Op::Gt { lhs, rhs, .. }
        | Op::Ge { lhs, rhs, .. } => effect.v_reads.extend([lhs, rhs]),
        Op::BranchIf { cond, .. } | Op::BranchIfNot { cond, .. } => effect.v_reads.push(cond),
        Op::Call {
            callee, args, argc, ..
        } => {
            effect.v_reads.push(callee);
            add_v_span(&mut effect.v_reads, args, argc);
        }
        Op::CallGlobal { args, argc, .. } => {
            add_v_span(&mut effect.v_reads, args, argc);
        }
        Op::MethodCall {
            receiver,
            args,
            argc,
            ..
        } => {
            effect.v_reads.push(receiver);
            add_v_span(&mut effect.v_reads, args, argc);
        }
        Op::StreamWriteByte {
            stream_reg,
            byte_reg,
            ..
        } => effect.v_reads.extend([stream_reg, byte_reg]),
        Op::U8VecSetByte {
            u8vec_reg,
            idx_reg,
            byte_reg,
            ..
        } => effect.v_reads.extend([u8vec_reg, idx_reg, byte_reg]),
        Op::U8VecGetByte {
            u8vec_reg, idx_reg, ..
        }
        | Op::StrByteAt {
            recv: u8vec_reg,
            idx: idx_reg,
            ..
        } => effect.v_reads.extend([u8vec_reg, idx_reg]),
        Op::StrSubstring {
            recv_reg,
            start_reg,
            end_reg,
            ..
        } => effect.v_reads.extend([recv_reg, start_reg, end_reg]),
        Op::MapIncMethod {
            map_reg,
            key_reg,
            by_reg,
            ..
        }
        | Op::MapInc {
            map_reg,
            key_reg,
            by_reg,
            ..
        } => effect.v_reads.extend([map_reg, key_reg, by_reg]),
        Op::BuildTuple { first, count, .. } | Op::BuildArray { first, count, .. } => {
            add_v_span(&mut effect.v_reads, first, count);
        }
        Op::BuildArrayRepeat { value, count, .. } => effect.v_reads.extend([value, count]),
        Op::Struct2I64 {
            first_i, second_i, ..
        } => effect.i_reads.extend([first_i, second_i]),
        Op::BuildRange { start, end, .. } => effect.v_reads.extend([start, end]),
        Op::BuildVariant1 { field, .. } => effect.v_reads.push(field),
        Op::BuildVariant2 { first, second, .. } => effect.v_reads.extend([first, second]),
        Op::CellTake { cell, .. }
        | Op::CaptureCellGet { cell, .. }
        | Op::CaptureCellTake { cell, .. } => effect.v_reads.push(cell),
        Op::CaptureCellSet { cell, src } => effect.v_reads.extend([cell, src]),
        Op::IndexGet { base, index, .. }
        | Op::IndexGetChecked { base, index, .. }
        | Op::IndexGetConsume { base, index, .. }
        | Op::IndexedFieldGet { base, index, .. }
        | Op::IndexedFieldGetF64 { base, index, .. }
        | Op::IndexedFieldGetF64ByOffset { base, index, .. }
        | Op::FlatGetF64 { base, index, .. } => effect.v_reads.extend([base, index]),
        Op::IndexSet { base, index, value }
        | Op::IndexedFieldSet {
            base, index, value, ..
        } => effect.v_reads.extend([base, index, value]),
        Op::FieldGet { receiver, .. }
        | Op::VecPop { receiver, .. }
        | Op::TupleIndex { receiver, .. }
        | Op::TupleTailIndex { receiver, .. }
        | Op::TupleIndexConsume { receiver, .. }
        | Op::FieldGetF64 { receiver, .. }
        | Op::FieldGetF64ByOffset { receiver, .. }
        | Op::FieldGetI64 { receiver, .. }
        | Op::FieldGetI64ByOffset { receiver, .. } => effect.v_reads.push(receiver),
        Op::FieldSetI64ByOffset {
            receiver, value_i, ..
        } => {
            effect.v_reads.push(receiver);
            effect.i_reads.push(value_i);
        }
        Op::FieldSet {
            receiver, value, ..
        }
        | Op::TupleSet {
            receiver, value, ..
        }
        | Op::VecPush { receiver, value }
        | Op::StrAppend { receiver, value }
        | Op::StrPush {
            receiver, value, ..
        } => effect.v_reads.extend([receiver, value]),
        Op::StrConcatI64 {
            dst,
            prefix,
            value_i,
        } => {
            effect.v_reads.push(prefix);
            effect.i_reads.push(value_i);
            effect.v_writes.push(dst);
        }
        Op::VecInsert {
            dst,
            receiver,
            index,
            value,
        } => {
            effect.v_reads.extend([receiver, index, value]);
            effect.v_writes.push(dst);
        }
        Op::VecSwap {
            dst,
            receiver,
            a,
            b,
        } => {
            effect.v_reads.extend([receiver, a, b]);
            effect.v_writes.push(dst);
        }
        Op::VecSwapDiscard { receiver, a, b } => {
            effect.v_reads.extend([receiver, a, b]);
        }
        Op::VecRemove { receiver, index }
        | Op::VecRemoveAt {
            receiver, index, ..
        } => effect.v_reads.extend([receiver, index]),
        Op::BuildIntArray { first_i, count, .. } | Op::BuildByteArray { first_i, count, .. } => {
            add_i_span(&mut effect.i_reads, first_i, count);
        }
        Op::BuildByteArrayRepeat {
            value_i, count_v, ..
        } => {
            effect.i_reads.push(value_i);
            effect.v_reads.push(count_v);
        }
        Op::CheckNonNegativeCapacity { capacity_i } => effect.i_reads.push(capacity_i),
        Op::BuildFloatVec { first_f, count, .. } => {
            add_f_span(&mut effect.f_reads, first_f, u32::from(count));
        }
        Op::DivF64ByI64 { lhs_f, rhs_i, .. } => {
            effect.f_reads.push(lhs_f);
            effect.i_reads.push(rhs_i);
        }
        Op::IntToFloatF64 { src_i, .. }
        | Op::TruncCastI64 { src_i, .. }
        | Op::I64ToUint { src_i, .. }
        | Op::BoxI64 { src_i, .. } => effect.i_reads.push(src_i),
        Op::FloatToIntI64 { src_f, .. } | Op::BoxF64 { src_f, .. } => effect.f_reads.push(src_f),
        Op::UnboxI64 { src_v, peer_v, .. } | Op::UnboxF64 { src_v, peer_v, .. } => {
            effect.v_reads.push(src_v);
            if let Some(peer_v) = peer_v {
                effect.v_reads.push(peer_v);
            }
        }
        Op::IntArrayGetI64 { base, index_i, .. } | Op::FloatVecGetF64 { base, index_i, .. } => {
            effect.v_reads.push(base);
            effect.i_reads.push(index_i);
        }
        Op::StrByteAtI64 { recv, idx_i, .. } => {
            effect.v_reads.push(recv);
            effect.i_reads.push(idx_i);
        }
        Op::StrByteAtAddI64 {
            lhs_i, recv, idx_i, ..
        } => {
            effect.v_reads.push(recv);
            effect.i_reads.extend([lhs_i, idx_i]);
        }
        Op::StrLenI64 { recv, .. } => effect.v_reads.push(recv),
        Op::IntArraySetI64 {
            base,
            index_i,
            value_i,
        } => {
            effect.v_reads.push(base);
            effect.i_reads.extend([index_i, value_i]);
        }
        Op::IntArraySwap { base, i_i, j_i } | Op::FloatVecSwap { base, i_i, j_i } => {
            effect.v_reads.push(base);
            effect.i_reads.extend([i_i, j_i]);
        }
        Op::FloatVecSetF64 {
            base,
            index_i,
            value_f,
        } => {
            effect.v_reads.push(base);
            effect.i_reads.push(index_i);
            effect.f_reads.push(value_f);
        }
        Op::IntMapInc {
            map_reg,
            key_i,
            by_i,
            ..
        } => {
            effect.v_reads.push(map_reg);
            effect.i_reads.extend([key_i, by_i]);
        }
        Op::IntMapGetOr {
            map_reg,
            key_i,
            default_i,
            ..
        } => {
            effect.v_reads.push(map_reg);
            effect.i_reads.extend([key_i, default_i]);
        }
        Op::IntMapInsert {
            map_reg,
            key_i,
            value_i,
            ..
        } => {
            effect.v_reads.push(map_reg);
            effect.i_reads.extend([key_i, value_i]);
        }
        Op::MapInsert {
            map_reg,
            key_reg,
            value_reg,
            ..
        } => effect.v_reads.extend([map_reg, key_reg, value_reg]),
        Op::IntMapLen { map_reg, .. } => effect.v_reads.push(map_reg),
        Op::IntMapContainsKey { map_reg, key_i, .. } => {
            effect.v_reads.push(map_reg);
            effect.i_reads.push(key_i);
        }
        Op::LoadConst { .. }
        | Op::LoadGlobal { .. }
        | Op::Jump { .. }
        | Op::Return { .. }
        | Op::ReturnUnit
        | Op::Panic { .. }
        | Op::TypeError { .. }
        | Op::CovHit { .. }
        | Op::BuildIntMap { .. }
        | Op::BuildStrIntMap { .. }
        | Op::LoadConstF64 { .. }
        | Op::LoadConstI64 { .. } => {}
        Op::AddF64 { lhs_f, rhs_f, .. }
        | Op::SubF64 { lhs_f, rhs_f, .. }
        | Op::MulF64 { lhs_f, rhs_f, .. }
        | Op::DivF64 { lhs_f, rhs_f, .. }
        | Op::LtF64 { lhs_f, rhs_f, .. }
        | Op::LeF64 { lhs_f, rhs_f, .. }
        | Op::GtF64 { lhs_f, rhs_f, .. }
        | Op::GeF64 { lhs_f, rhs_f, .. }
        | Op::EqF64 { lhs_f, rhs_f, .. }
        | Op::NeF64 { lhs_f, rhs_f, .. }
        | Op::BranchIfLtF64 { lhs_f, rhs_f, .. }
        | Op::BranchIfGeF64 { lhs_f, rhs_f, .. } => effect.f_reads.extend([lhs_f, rhs_f]),
        Op::NegF64 { src_f, .. }
        | Op::SqrtF64 { src_f, .. }
        | Op::SinF64 { src_f, .. }
        | Op::CosF64 { src_f, .. }
        | Op::AbsF64 { src_f, .. }
        | Op::FloorF64 { src_f, .. }
        | Op::CeilF64 { src_f, .. }
        | Op::ExpF64 { src_f, .. }
        | Op::LnF64 { src_f, .. }
        | Op::MoveF64 { src_f, .. } => effect.f_reads.push(src_f),
        Op::MulAddF64 { a_f, b_f, c_f, .. } | Op::MulSubF64 { a_f, b_f, c_f, .. } => {
            effect.f_reads.extend([a_f, b_f, c_f]);
        }
        Op::AddI64 { lhs_i, rhs_i, .. }
        | Op::CheckedAddI64 { lhs_i, rhs_i, .. }
        | Op::SubI64 { lhs_i, rhs_i, .. }
        | Op::CheckedSubI64 { lhs_i, rhs_i, .. }
        | Op::MulI64 { lhs_i, rhs_i, .. }
        | Op::CheckedMulI64 { lhs_i, rhs_i, .. }
        | Op::DivI64 { lhs_i, rhs_i, .. }
        | Op::RemI64 { lhs_i, rhs_i, .. }
        | Op::DivU64 { lhs_i, rhs_i, .. }
        | Op::RemU64 { lhs_i, rhs_i, .. }
        | Op::BitAndI64 { lhs_i, rhs_i, .. }
        | Op::BitOrI64 { lhs_i, rhs_i, .. }
        | Op::BitXorI64 { lhs_i, rhs_i, .. }
        | Op::ShlI64 { lhs_i, rhs_i, .. }
        | Op::ShrI64 { lhs_i, rhs_i, .. }
        | Op::ShrU64 { lhs_i, rhs_i, .. }
        | Op::LtI64 { lhs_i, rhs_i, .. }
        | Op::LeI64 { lhs_i, rhs_i, .. }
        | Op::GtI64 { lhs_i, rhs_i, .. }
        | Op::GeI64 { lhs_i, rhs_i, .. }
        | Op::EqI64 { lhs_i, rhs_i, .. }
        | Op::NeI64 { lhs_i, rhs_i, .. }
        | Op::LtU64 { lhs_i, rhs_i, .. }
        | Op::LeU64 { lhs_i, rhs_i, .. }
        | Op::GtU64 { lhs_i, rhs_i, .. }
        | Op::GeU64 { lhs_i, rhs_i, .. }
        | Op::BranchIfLtI64 { lhs_i, rhs_i, .. }
        | Op::BranchIfGeI64 { lhs_i, rhs_i, .. }
        | Op::BranchIfGtI64 { lhs_i, rhs_i, .. } => effect.i_reads.extend([lhs_i, rhs_i]),
        Op::NegI64 { src_i, .. }
        | Op::MoveI64 { src_i, .. }
        | Op::ArithImmI64 { lhs_i: src_i, .. } => effect.i_reads.push(src_i),
        Op::IncJumpIfLtI64 {
            counter_i, end_i, ..
        }
        | Op::IncJumpIfLeI64 {
            counter_i, end_i, ..
        } => effect.i_reads.extend([counter_i, end_i]),
        Op::IndexedFieldSetF64 {
            base,
            index,
            value_f,
            ..
        }
        | Op::IndexedFieldSetF64ByOffset {
            base,
            index,
            value_f,
            ..
        } => {
            effect.v_reads.extend([base, index]);
            effect.f_reads.push(value_f);
        }
        Op::FlatSetF64 {
            base,
            index,
            value_f,
            ..
        } => {
            effect.v_reads.extend([base, index]);
            effect.f_reads.push(value_f);
        }
        Op::FlatGetF64I { base, index_i, .. } => {
            effect.v_reads.push(base);
            effect.i_reads.push(index_i);
        }
        Op::FlatSetF64I {
            base,
            index_i,
            value_f,
            ..
        } => {
            effect.v_reads.push(base);
            effect.i_reads.push(index_i);
            effect.f_reads.push(value_f);
        }
        Op::MakeClosure { proto, .. } => effect
            .v_reads
            .extend(closure_protos[proto as usize].capture_regs.iter().copied()),
        Op::Select { first, count } => {
            for arm in &select_arms[first as usize..first as usize + count as usize] {
                match arm.kind {
                    crate::bytecode::SelectArmKind::Recv => effect.v_reads.push(arm.channel_reg),
                    crate::bytecode::SelectArmKind::Send => {
                        effect.v_reads.extend([arm.channel_reg, arm.value_reg]);
                    }
                    crate::bytecode::SelectArmKind::Default => {}
                }
            }
        }
        Op::Wide { idx } => match &wide_ops[idx as usize] {
            WideOp::StrConcatPadI64 {
                dst,
                prefix,
                value,
                width,
                fill,
                align,
            } => {
                effect
                    .v_reads
                    .extend([*prefix, *value, *width, *fill, *align]);
                effect.v_writes.push(*dst);
            }
            WideOp::MapIncAt {
                dst,
                map_reg,
                seq_reg,
                start_reg,
                len_reg,
                by_reg,
                ..
            } => {
                effect
                    .v_reads
                    .extend([*map_reg, *seq_reg, *start_reg, *len_reg, *by_reg]);
                effect.v_writes.push(*dst);
            }
            WideOp::BuildFloatArray {
                dst_v,
                first_f,
                stride,
                elem_count,
                ..
            } => {
                add_f_span(
                    &mut effect.f_reads,
                    *first_f,
                    u32::from(*stride) * u32::from(*elem_count),
                );
                effect.v_writes.push(*dst_v);
            }
            WideOp::BuildFloatArrayFromStructs {
                dst_v,
                first_v,
                elem_count,
                ..
            } => {
                add_v_span(&mut effect.v_reads, *first_v, *elem_count);
                effect.v_writes.push(*dst_v);
            }
        },
        Op::ClearRegs { start, count } => add_v_span(&mut effect.v_clears, start, count),
    }

    match op {
        Op::CellNewMove { src, .. }
        | Op::CaptureCellNew { src, .. }
        | Op::CaptureCellSet { src, .. }
        | Op::MoveConsume { src, .. } => effect.v_clears.push(src),
        Op::IncJumpIfLtI64 { counter_i, .. } | Op::IncJumpIfLeI64 { counter_i, .. } => {
            effect.i_writes.push(counter_i);
        }
        _ => {}
    }
    effect
}

/// Helper for [`validate_chunk`] - descends into the wide-op side
/// table since `Op::Wide` only carries the side-table index.
fn validate_wide_op(
    op_idx: usize,
    wide: &WideOp,
    check_v: &dyn Fn(usize, Reg) -> Result<(), ValidationError>,
    check_f: &dyn Fn(usize, Reg) -> Result<(), ValidationError>,
    consts_len: usize,
    chunk: &FnChunk,
) -> Result<(), ValidationError> {
    let f_count = u32::from(chunk.float_count);
    match *wide {
        WideOp::StrConcatPadI64 {
            dst,
            prefix,
            value,
            width,
            fill,
            align,
        } => {
            for reg in [dst, prefix, value, width, fill, align] {
                check_v(op_idx, reg)?;
            }
        }
        WideOp::MapIncAt {
            dst,
            map_reg,
            seq_reg,
            start_reg,
            len_reg,
            by_reg,
        } => {
            check_v(op_idx, dst)?;
            check_v(op_idx, map_reg)?;
            check_v(op_idx, seq_reg)?;
            check_v(op_idx, start_reg)?;
            check_v(op_idx, len_reg)?;
            check_v(op_idx, by_reg)?;
        }
        WideOp::BuildFloatArray {
            dst_v,
            name_idx,
            fields_idx,
            stride,
            elem_count,
            first_f,
        } => {
            check_v(op_idx, dst_v)?;
            if (u32::from(name_idx) as usize) >= consts_len {
                return Err(ValidationError::ConstantOutOfBounds {
                    op_idx,
                    idx: u32::from(name_idx),
                    len: consts_len,
                    pool: PoolKind::Consts,
                });
            }
            if (u32::from(fields_idx) as usize) >= consts_len {
                return Err(ValidationError::ConstantOutOfBounds {
                    op_idx,
                    idx: u32::from(fields_idx),
                    len: consts_len,
                    pool: PoolKind::Consts,
                });
            }
            let n = u32::from(stride).saturating_mul(u32::from(elem_count));
            if n > 0 {
                let last = u32::from(first_f).saturating_add(n - 1);
                if last >= f_count {
                    return Err(ValidationError::RegisterOutOfBounds {
                        op_idx,
                        reg: last,
                        count: f_count,
                        file: RegFile::Float,
                    });
                }
            }
            check_f(op_idx, first_f).ok();
        }
        WideOp::BuildFloatArrayFromStructs {
            dst_v,
            first_v,
            elem_count,
            name_idx,
            fields_idx,
        } => {
            check_v(op_idx, dst_v)?;
            for idx in [name_idx, fields_idx] {
                if usize::from(idx) >= consts_len {
                    return Err(ValidationError::ConstantOutOfBounds {
                        op_idx,
                        idx: u32::from(idx),
                        len: consts_len,
                        pool: PoolKind::Consts,
                    });
                }
            }
            for offset in 0..elem_count {
                check_v(op_idx, first_v.saturating_add(offset))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Op, SelectArmKind, SelectArmMeta, WideOp};
    use crate::value::Value;

    fn minimal_chunk() -> FnChunk {
        FnChunk {
            name: "test",
            arity: 0,
            register_count: 2,
            float_count: 1,
            int_count: 1,
            instrs: Vec::new(),
            instruction_locations: Vec::new(),
            wide_ops: Vec::new(),
            consts: vec![Value::Int(0)],
            f64_consts: vec![0.0],
            i64_consts: vec![0],
            globals: vec!["g".into()],
            shape_names: vec!["TestVariant"],
            call_cache_count: 0,
            arith_cache_count: 0,
            field_cache_count: 0,
            mut_ref_params: Vec::new(),
            i64_params: Vec::new(),
            closure_protos: Vec::new(),
            select_arms: Vec::new(),
        }
    }

    #[test]
    fn validate_chunk_accepts_well_formed_bytecode() {
        let mut chunk = minimal_chunk();
        chunk.instrs.push(Op::LoadConst { dst: 0, idx: 0 });
        chunk.instrs.push(Op::Move { dst: 1, src: 0 });
        chunk.instrs.push(Op::Jump { target: 0 });
        chunk.instrs.push(Op::Return { value: 1 });
        assert!(validate_chunk(&chunk).is_ok());
    }

    #[test]
    fn float_vec_get_initializes_its_float_destination() {
        let mut chunk = minimal_chunk();
        chunk.instrs = vec![
            Op::LoadConst { dst: 0, idx: 0 },
            Op::LoadConstI64 { dst_i: 0, idx: 0 },
            Op::FloatVecGetF64 {
                dst_f: 0,
                base: 0,
                index_i: 0,
            },
            Op::BoxF64 { dst_v: 1, src_f: 0 },
            Op::Return { value: 1 },
        ];
        assert!(validate_chunk(&chunk).is_ok());
    }

    #[test]
    fn validate_chunk_rejects_invalid_parameter_layout() {
        let mut chunk = minimal_chunk();
        chunk.arity = 3;
        let err = validate_chunk(&chunk).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidChunkShape { .. }));

        chunk.arity = 1;
        chunk.mut_ref_params = vec![1];
        let err = validate_chunk(&chunk).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidChunkShape { .. }));
    }

    #[test]
    fn validate_chunk_rejects_out_of_range_register() {
        let mut chunk = minimal_chunk();
        chunk.instrs.push(Op::Move { dst: 99, src: 0 });
        let err = validate_chunk(&chunk).expect_err("must reject");
        match err {
            ValidationError::RegisterOutOfBounds {
                reg,
                file: RegFile::Value,
                ..
            } => assert_eq!(reg, 99),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_chunk_rejects_jump_past_end() {
        let mut chunk = minimal_chunk();
        chunk.instrs.push(Op::Jump { target: 42 });
        let err = validate_chunk(&chunk).expect_err("must reject");
        match err {
            ValidationError::PcOutOfBounds {
                target,
                instr_count,
                ..
            } => {
                assert_eq!(target, 42);
                assert_eq!(instr_count, 1);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_chunk_rejects_constant_pool_overflow() {
        let mut chunk = minimal_chunk();
        chunk.instrs.push(Op::LoadConst { dst: 0, idx: 7 });
        let err = validate_chunk(&chunk).expect_err("must reject");
        match err {
            ValidationError::ConstantOutOfBounds {
                idx,
                len,
                pool: PoolKind::Consts,
                ..
            } => {
                assert_eq!(idx, 7);
                assert_eq!(len, 1);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_chunk_rejects_inline_cache_overflow() {
        let mut chunk = minimal_chunk();
        chunk.register_count = 3;
        chunk.instrs.push(Op::AddInt {
            dst: 0,
            lhs: 1,
            rhs: 2,
            cache_idx: 0,
        });
        chunk.instrs.push(Op::ReturnUnit);

        let err = validate_chunk(&chunk).expect_err("must reject missing cache slot");
        assert!(matches!(
            err,
            ValidationError::CacheOutOfBounds {
                cache: CacheKind::Arithmetic,
                idx: 0,
                count: 0,
                ..
            }
        ));
    }

    #[test]
    fn validate_chunk_rejects_reachable_fallthrough_and_empty_chunks() {
        let mut chunk = minimal_chunk();
        chunk.instrs.push(Op::LoadConst { dst: 0, idx: 0 });
        assert!(matches!(
            validate_chunk(&chunk),
            Err(ValidationError::ControlFlowFallsOffEnd { op_idx: 0 })
        ));

        chunk.instrs.clear();
        assert!(matches!(
            validate_chunk(&chunk),
            Err(ValidationError::InvalidChunkShape { .. })
        ));
    }

    #[test]
    fn validate_chunk_rejects_uninitialized_typed_register_read() {
        let mut chunk = minimal_chunk();
        chunk.instrs = vec![
            Op::AddI64 {
                dst_i: 0,
                lhs_i: 0,
                rhs_i: 0,
            },
            Op::ReturnUnit,
        ];

        assert!(matches!(
            validate_chunk(&chunk),
            Err(ValidationError::RegisterUninitialized {
                op_idx: 0,
                reg: 0,
                file: RegFile::Int,
            })
        ));
    }

    #[test]
    fn validate_chunk_permits_boxed_void_scratch_after_clear() {
        let mut chunk = minimal_chunk();
        chunk.instrs = vec![
            Op::LoadConst { dst: 0, idx: 0 },
            Op::ClearRegs { start: 0, count: 1 },
            Op::FieldSet {
                receiver: 0,
                name_idx: 0,
                value: 1,
            },
            Op::ReturnUnit,
        ];

        assert!(validate_chunk(&chunk).is_ok());
    }

    #[test]
    fn validate_chunk_permits_boxed_void_scratch_at_a_join() {
        let mut chunk = minimal_chunk();
        chunk.instrs = vec![
            Op::LoadConst { dst: 0, idx: 0 },
            Op::BranchIf { cond: 0, target: 3 },
            Op::LoadConst { dst: 1, idx: 0 },
            Op::Move { dst: 0, src: 1 },
            Op::ReturnUnit,
        ];

        assert!(validate_chunk(&chunk).is_ok());
    }

    fn assert_register_mutation_rejected(label: &str, op: Op, file: RegFile) {
        let mut chunk = minimal_chunk();
        chunk.register_count = 3;
        chunk.float_count = 3;
        chunk.int_count = 3;
        chunk.instrs = vec![op, Op::ReturnUnit];

        assert!(
            matches!(
                validate_chunk(&chunk),
                Err(ValidationError::RegisterOutOfBounds {
                    file: actual_file,
                    ..
                }) if actual_file == file
            ),
            "{label} must reject its mutated {file} register operand"
        );
    }

    /// This is deliberately deterministic rather than property-test based:
    /// it is cheap enough for every test run and gives every register operand
    /// *class* a stable regression case. Each case starts from a legal shape
    /// and changes just one operand to the first out-of-range register.
    #[test]
    fn deterministic_register_operand_mutations_are_rejected() {
        const OUT: Reg = 3;
        let mutations = [
            (
                "value destination",
                Op::LoadConst { dst: OUT, idx: 0 },
                RegFile::Value,
            ),
            (
                "value span",
                Op::BuildArray {
                    dst: 0,
                    first: OUT,
                    count: 1,
                },
                RegFile::Value,
            ),
            (
                "call argument span",
                Op::Call {
                    dst: 0,
                    callee: 0,
                    args: OUT,
                    argc: 1,
                    cache_idx: 0,
                    may_have_cells: false,
                },
                RegFile::Value,
            ),
            (
                "mixed value source",
                Op::U8VecGetByte {
                    dst_i: 0,
                    u8vec_reg: OUT,
                    idx_reg: 0,
                },
                RegFile::Value,
            ),
            (
                "float destination",
                Op::LoadConstF64 { dst_f: OUT, idx: 0 },
                RegFile::Float,
            ),
            (
                "float span",
                Op::BuildFloatVec {
                    dst_v: 0,
                    first_f: OUT,
                    count: 1,
                },
                RegFile::Float,
            ),
            (
                "mixed float destination",
                Op::IntToFloatF64 {
                    dst_f: OUT,
                    src_i: 0,
                },
                RegFile::Float,
            ),
            (
                "integer destination",
                Op::LoadConstI64 { dst_i: OUT, idx: 0 },
                RegFile::Int,
            ),
            (
                "integer span",
                Op::BuildIntArray {
                    dst_v: 0,
                    first_i: OUT,
                    count: 1,
                },
                RegFile::Int,
            ),
            (
                "mixed integer destination",
                Op::FloatToIntI64 {
                    dst_i: OUT,
                    src_f: 0,
                },
                RegFile::Int,
            ),
        ];

        for (label, op, file) in mutations {
            assert_register_mutation_rejected(label, op, file);
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one table-driven test whose length is its case list"
    )]
    fn deterministic_non_register_operand_mutations_are_rejected() {
        let cases = [
            (
                "boxed constant",
                Op::LoadConst { dst: 0, idx: 1 },
                PoolKind::Consts,
            ),
            (
                "float constant",
                Op::LoadConstF64 { dst_f: 0, idx: 1 },
                PoolKind::F64Consts,
            ),
            (
                "integer constant",
                Op::LoadConstI64 { dst_i: 0, idx: 1 },
                PoolKind::I64Consts,
            ),
            (
                "global",
                Op::LoadGlobal { dst: 0, idx: 1 },
                PoolKind::Globals,
            ),
            (
                "closure prototype",
                Op::MakeClosure { dst: 0, proto: 0 },
                PoolKind::ClosureProtos,
            ),
            ("wide-op table", Op::Wide { idx: 0 }, PoolKind::WideOps),
            (
                "select-arm table",
                Op::Select { first: 0, count: 1 },
                PoolKind::SelectArms,
            ),
            (
                "shape-name table",
                Op::StructIs {
                    dst: 0,
                    src: 0,
                    name_idx: 1,
                },
                PoolKind::ShapeNames,
            ),
        ];

        for (label, op, pool) in cases {
            let mut chunk = minimal_chunk();
            chunk.instrs = vec![op, Op::ReturnUnit];
            assert!(
                matches!(
                    validate_chunk(&chunk),
                    Err(ValidationError::ConstantOutOfBounds {
                        pool: actual_pool,
                        ..
                    }) if actual_pool == pool
                ),
                "{label} must reject its mutated side-table operand"
            );
        }

        let cache_cases = [
            (
                "call cache",
                Op::Call {
                    dst: 0,
                    callee: 0,
                    args: 0,
                    argc: 0,
                    cache_idx: 0,
                    may_have_cells: false,
                },
                CacheKind::Call,
            ),
            (
                "arithmetic cache",
                Op::AddInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                CacheKind::Arithmetic,
            ),
            (
                "field cache",
                Op::FieldGet {
                    dst: 0,
                    receiver: 0,
                    name_idx: 0,
                    cache_idx: 0,
                },
                CacheKind::Field,
            ),
        ];
        for (label, op, cache) in cache_cases {
            let mut chunk = minimal_chunk();
            chunk.instrs = vec![op, Op::ReturnUnit];
            assert!(
                matches!(
                    validate_chunk(&chunk),
                    Err(ValidationError::CacheOutOfBounds {
                        cache: actual_cache,
                        ..
                    }) if actual_cache == cache
                ),
                "{label} must reject its mutated cache operand"
            );
        }
    }

    #[test]
    fn deterministic_jump_operand_mutations_are_rejected() {
        let cases = [
            ("plain jump", Op::Jump { target: 9 }),
            ("conditional jump", Op::BranchIf { cond: 0, target: 9 }),
            (
                "typed fused jump",
                Op::BranchIfLtI64 {
                    lhs_i: 0,
                    rhs_i: 0,
                    target: 9,
                },
            ),
            (
                "increment fused jump",
                Op::IncJumpIfLtI64 {
                    counter_i: 0,
                    end_i: 0,
                    target: 9,
                },
            ),
        ];

        for (label, op) in cases {
            let mut chunk = minimal_chunk();
            chunk.int_count = 1;
            chunk.instrs = vec![op, Op::ReturnUnit];
            assert!(
                matches!(
                    validate_chunk(&chunk),
                    Err(ValidationError::PcOutOfBounds { .. })
                ),
                "{label} must reject its mutated instruction target"
            );
        }
    }

    #[test]
    fn deterministic_select_arm_operand_mutations_are_rejected() {
        let mut bad_body = minimal_chunk();
        bad_body.select_arms.push(SelectArmMeta {
            kind: SelectArmKind::Default,
            channel_reg: 0,
            value_reg: 0,
            bind_reg: 0,
            body_block: 99,
        });
        bad_body.instrs = vec![Op::Select { first: 0, count: 1 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_body),
            Err(ValidationError::PcOutOfBounds { .. })
        ));

        let mut bad_recv_reg = minimal_chunk();
        bad_recv_reg.register_count = 1;
        bad_recv_reg.select_arms.push(SelectArmMeta {
            kind: SelectArmKind::Recv,
            channel_reg: 1,
            value_reg: 0,
            bind_reg: 0,
            body_block: 1,
        });
        bad_recv_reg.instrs = vec![Op::Select { first: 0, count: 1 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_recv_reg),
            Err(ValidationError::RegisterOutOfBounds {
                file: RegFile::Value,
                ..
            })
        ));

        let mut bad_send_reg = minimal_chunk();
        bad_send_reg.register_count = 1;
        bad_send_reg.select_arms.push(SelectArmMeta {
            kind: SelectArmKind::Send,
            channel_reg: 0,
            value_reg: 1,
            bind_reg: 0,
            body_block: 1,
        });
        bad_send_reg.instrs = vec![Op::Select { first: 0, count: 1 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_send_reg),
            Err(ValidationError::RegisterOutOfBounds {
                file: RegFile::Value,
                ..
            })
        ));
    }

    #[test]
    fn deterministic_wide_operand_mutations_are_rejected() {
        let mut bad_map_reg = minimal_chunk();
        bad_map_reg.register_count = 1;
        bad_map_reg.wide_ops.push(WideOp::MapIncAt {
            dst: 0,
            map_reg: 1,
            seq_reg: 0,
            start_reg: 0,
            len_reg: 0,
            by_reg: 0,
        });
        bad_map_reg.instrs = vec![Op::Wide { idx: 0 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_map_reg),
            Err(ValidationError::RegisterOutOfBounds {
                file: RegFile::Value,
                ..
            })
        ));

        let mut bad_float_span = minimal_chunk();
        bad_float_span.float_count = 1;
        bad_float_span.wide_ops.push(WideOp::BuildFloatArray {
            dst_v: 0,
            name_idx: 0,
            fields_idx: 0,
            stride: 2,
            elem_count: 1,
            first_f: 0,
        });
        bad_float_span.instrs = vec![Op::Wide { idx: 0 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_float_span),
            Err(ValidationError::RegisterOutOfBounds {
                file: RegFile::Float,
                ..
            })
        ));

        let mut bad_const = minimal_chunk();
        bad_const.wide_ops.push(WideOp::BuildFloatArray {
            dst_v: 0,
            name_idx: 1,
            fields_idx: 0,
            stride: 1,
            elem_count: 1,
            first_f: 0,
        });
        bad_const.instrs = vec![Op::Wide { idx: 0 }, Op::ReturnUnit];
        assert!(matches!(
            validate_chunk(&bad_const),
            Err(ValidationError::ConstantOutOfBounds {
                pool: PoolKind::Consts,
                ..
            })
        ));
    }

    #[test]
    fn deterministic_malformed_operand_corpus_never_panics() {
        let mut bad_wide_payload = minimal_chunk();
        bad_wide_payload.float_count = 1;
        bad_wide_payload.wide_ops.push(WideOp::BuildFloatArray {
            dst_v: 0,
            name_idx: 0,
            fields_idx: 0,
            stride: 2,
            elem_count: 1,
            first_f: 0,
        });
        bad_wide_payload.instrs = vec![Op::Wide { idx: 0 }, Op::ReturnUnit];

        let mut bad_jump = minimal_chunk();
        bad_jump.instrs = vec![Op::Jump { target: u32::MAX }];

        let mut bad_select_range = minimal_chunk();
        bad_select_range.instrs = vec![Op::Select {
            first: u32::MAX,
            count: 1,
        }];

        let mut bad_value_span = minimal_chunk();
        bad_value_span.instrs = vec![
            Op::BuildTuple {
                dst: 0,
                first: u16::MAX,
                count: 1,
            },
            Op::ReturnUnit,
        ];

        for (label, chunk) in [
            ("wide payload", bad_wide_payload),
            ("jump target", bad_jump),
            ("select range", bad_select_range),
            ("value span", bad_value_span),
        ] {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| validate_chunk(&chunk)));
            assert!(result.is_ok(), "validator panicked on malformed {label}");
            assert!(
                result.expect("checked above").is_err(),
                "accepted malformed {label}"
            );
        }
    }

    #[test]
    fn boxed_value_void_reads_are_not_false_positive_definite_assignment_errors() {
        let operations = [
            ("move", Op::Move { dst: 0, src: 1 }),
            (
                "call",
                Op::Call {
                    dst: 0,
                    callee: 1,
                    args: 2,
                    argc: 1,
                    cache_idx: 0,
                    may_have_cells: false,
                },
            ),
            (
                "aggregate span",
                Op::BuildArray {
                    dst: 0,
                    first: 1,
                    count: 2,
                },
            ),
            (
                "mutating receiver",
                Op::FieldSet {
                    receiver: 1,
                    name_idx: 0,
                    value: 2,
                },
            ),
        ];

        for (label, op) in operations {
            let mut chunk = minimal_chunk();
            chunk.register_count = 4;
            chunk.call_cache_count = 1;
            chunk.instrs = vec![Op::ClearRegs { start: 0, count: 4 }, op, Op::ReturnUnit];
            assert!(
                validate_chunk(&chunk).is_ok(),
                "boxed Value::Void read in {label} must not be treated as uninitialized host memory"
            );
        }
    }
}
