//! Optional profiling instrumentation for the bytecode VM.
//!
//! Off by default. Enable with `--features profile` to make the
//! dispatch loop bump thread-local counters per opcode, per
//! opcode pair, and at a handful of dispatch / inline-cache
//! sites. After a run, [`dump_report`] returns a textual report
//! of the hottest opcodes / pairs / cache hit rates.
//!
//! The feature is *runtime opaque*: when off, every public hook
//! in this module compiles to an empty inline. The dispatch
//! loop stays branch-free with respect to the profiler.

#![allow(dead_code)] // every helper has a `cfg(feature)` user

use std::cell::RefCell;
#[cfg(feature = "profile")]
use std::fmt::Write as _;

#[allow(
    unused_imports,
    reason = "the import is used only by the counted build of this module"
)]
use crate::bytecode::Op;

/// Maximum opcode tag we count. The current `Op` enum has ~140
/// variants; 256 leaves room for several years of growth.
pub(crate) const MAX_OPS: usize = 256;

/// Sentinel for "no previous opcode" - the very first op of
/// every chunk doesn't have a real predecessor.
const NO_PREV: usize = MAX_OPS;

/// Per-thread counter bag. The dispatch loop pushes into a
/// `thread_local!` so child goroutines write into their own
/// counters; the [`dump_report`] entry point sums them on
/// demand.
struct Counters {
    instr_count: u64,
    ops: [u64; MAX_OPS],
    pairs: Vec<u64>, // Lazily allocated MAX_OPS * MAX_OPS Vec.

    // Inline-cache metrics. "miss" here means the slow path
    // ran; "hit" means the cache satisfied the request.
    call_ic_hit: u64,
    call_ic_miss: u64,
    method_ic_hit: u64,
    method_ic_miss: u64,
    field_ic_hit: u64,
    field_ic_miss: u64,
    arith_ic_hit: u64,
    arith_ic_miss: u64,

    // FramePool metrics - warm reuse vs cold alloc.
    pool_value_hit: u64,
    pool_value_miss: u64,
    pool_float_hit: u64,
    pool_float_miss: u64,
    pool_int_hit: u64,
    pool_int_miss: u64,
    pool_arg_hit: u64,
    pool_arg_miss: u64,

    // Calls leaving the bytecode VM into the slow / generic
    // dispatch path (`dispatch_call`).
    slow_call_dispatch: u64,
    /// One per call frame entered from the bytecode VM. Lets us
    /// compute "instructions per call" - a useful denominator
    /// for "is dispatch the bottleneck or is per-op work?".
    frame_enters: u64,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            instr_count: 0,
            ops: [0; MAX_OPS],
            pairs: Vec::new(),
            call_ic_hit: 0,
            call_ic_miss: 0,
            method_ic_hit: 0,
            method_ic_miss: 0,
            field_ic_hit: 0,
            field_ic_miss: 0,
            arith_ic_hit: 0,
            arith_ic_miss: 0,
            pool_value_hit: 0,
            pool_value_miss: 0,
            pool_float_hit: 0,
            pool_float_miss: 0,
            pool_int_hit: 0,
            pool_int_miss: 0,
            pool_arg_hit: 0,
            pool_arg_miss: 0,
            slow_call_dispatch: 0,
            frame_enters: 0,
        }
    }
}

thread_local! {
    static CTRS: RefCell<Counters> = RefCell::new(Counters::default());
    /// Last opcode tag the dispatch loop saw, for pair counting.
    static PREV_TAG: RefCell<usize> = const { RefCell::new(NO_PREV) };
}

/// Returns the discriminant of `op` as a `usize`, exploiting the
/// `#[repr(u16)]` layout of the `Op` enum.
#[inline]
#[cfg(feature = "profile")]
fn op_tag(op: Op) -> usize {
    // SAFETY: `Op` is `#[repr(u16)]`; the first two bytes are
    // the discriminant. We read it as a `u16` and widen.
    #[allow(unsafe_code)]
    unsafe {
        *(std::ptr::from_ref(&op).cast::<u16>()) as usize
    }
}

/// Cheap stub when the feature is off.
#[inline]
#[cfg(not(feature = "profile"))]
#[allow(
    unsafe_code,
    reason = "the opcode tag is read off the enum's discriminant"
)]
fn op_tag(_op: Op) -> usize {
    0
}

/// Bump the per-op + per-pair counters. The dispatch loop calls
/// this once per executed instruction.
#[inline]
#[cfg(feature = "profile")]
pub(crate) fn record_op(op: Op) {
    let tag = op_tag(op);
    CTRS.with(|c| {
        let mut c = c.borrow_mut();
        c.instr_count += 1;
        if tag < MAX_OPS {
            c.ops[tag] += 1;
        }
        if c.pairs.is_empty() {
            c.pairs = vec![0u64; MAX_OPS * MAX_OPS];
        }
        let prev = PREV_TAG.with(|p| *p.borrow());
        if prev < MAX_OPS && tag < MAX_OPS {
            let idx = prev * MAX_OPS + tag;
            c.pairs[idx] += 1;
        }
    });
    PREV_TAG.with(|p| *p.borrow_mut() = tag);
}

#[inline]
#[cfg(not(feature = "profile"))]
pub(crate) fn record_op(_op: Op) {}

/// Reset the dispatch loop's prev-op tracker - called on entry
/// to each frame so cross-call pairs are dropped.
#[inline]
#[cfg(feature = "profile")]
pub(crate) fn enter_frame() {
    CTRS.with(|c| c.borrow_mut().frame_enters += 1);
    PREV_TAG.with(|p| *p.borrow_mut() = NO_PREV);
}

#[inline]
#[cfg(not(feature = "profile"))]
pub(crate) fn enter_frame() {}

macro_rules! make_bumper {
    ($name:ident, $field:ident) => {
        #[inline]
        #[cfg(feature = "profile")]
        pub(crate) fn $name() {
            CTRS.with(|c| c.borrow_mut().$field += 1);
        }
        #[inline]
        #[cfg(not(feature = "profile"))]
        pub(crate) fn $name() {}
    };
}

make_bumper!(bump_call_hit, call_ic_hit);
make_bumper!(bump_call_miss, call_ic_miss);
make_bumper!(bump_method_hit, method_ic_hit);
make_bumper!(bump_method_miss, method_ic_miss);
make_bumper!(bump_field_hit, field_ic_hit);
make_bumper!(bump_field_miss, field_ic_miss);
make_bumper!(bump_arith_hit, arith_ic_hit);
make_bumper!(bump_arith_miss, arith_ic_miss);
make_bumper!(bump_pool_value_hit, pool_value_hit);
make_bumper!(bump_pool_value_miss, pool_value_miss);
make_bumper!(bump_pool_float_hit, pool_float_hit);
make_bumper!(bump_pool_float_miss, pool_float_miss);
make_bumper!(bump_pool_int_hit, pool_int_hit);
make_bumper!(bump_pool_int_miss, pool_int_miss);
make_bumper!(bump_pool_arg_hit, pool_arg_hit);
make_bumper!(bump_pool_arg_miss, pool_arg_miss);
make_bumper!(bump_slow_call, slow_call_dispatch);

/// Reset all per-thread counters. Useful between phases of a
/// single benchmark run.
#[cfg(feature = "profile")]
pub fn reset() {
    CTRS.with(|c| *c.borrow_mut() = Counters::default());
    PREV_TAG.with(|p| *p.borrow_mut() = NO_PREV);
}

/// No-op when the `profile` feature is disabled.
#[cfg(not(feature = "profile"))]
pub fn reset() {}

/// Render a textual report of the counters collected on this
/// thread. Empty when the `profile` feature is disabled.
#[cfg(feature = "profile")]
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    reason = "one report whose length is its column list"
)]
pub fn dump_report() -> String {
    CTRS.with(|c| {
        let c = c.borrow();
        let mut out = String::new();
        let _ = writeln!(out, "## Profiler report");
        let _ = writeln!(out, "instructions: {}", c.instr_count);
        let _ = writeln!(out, "frame_enters: {}", c.frame_enters);
        if c.frame_enters > 0 {
            let _ = writeln!(
                out,
                "instructions/frame: {:.1}",
                c.instr_count as f64 / c.frame_enters as f64
            );
        }
        let _ = writeln!(out);

        // Top opcodes by frequency.
        let _ = writeln!(out, "### Top 25 opcodes");
        let mut op_pairs: Vec<(usize, u64)> = (0..MAX_OPS)
            .map(|i| (i, c.ops[i]))
            .filter(|(_, n)| *n > 0)
            .collect();
        op_pairs.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (i, (tag, count)) in op_pairs.iter().take(25).enumerate() {
            let pct = if c.instr_count > 0 {
                100.0 * (*count as f64) / (c.instr_count as f64)
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "  {:>2}. {:<28} {:>14}  {:>5.1}%",
                i + 1,
                op_label(*tag),
                count,
                pct
            );
        }
        let _ = writeln!(out);

        // Top pairs (prev -> curr).
        if !c.pairs.is_empty() {
            let _ = writeln!(out, "### Top 25 opcode pairs (prev -> curr)");
            let mut pair_v: Vec<(usize, usize, u64)> = c
                .pairs
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(i, n)| (i / MAX_OPS, i % MAX_OPS, *n))
                .collect();
            pair_v.sort_by_key(|entry| std::cmp::Reverse(entry.2));
            for (i, (p, c2, count)) in pair_v.iter().take(25).enumerate() {
                let _ = writeln!(
                    out,
                    "  {:>2}. {:<26} -> {:<26} {:>12}",
                    i + 1,
                    op_label(*p),
                    op_label(*c2),
                    count
                );
            }
            let _ = writeln!(out);
        }

        // IC hit/miss summary.
        let _ = writeln!(out, "### Inline-cache hit rates");
        for (label, hit, miss) in [
            ("call", c.call_ic_hit, c.call_ic_miss),
            ("method", c.method_ic_hit, c.method_ic_miss),
            ("field", c.field_ic_hit, c.field_ic_miss),
            ("arith", c.arith_ic_hit, c.arith_ic_miss),
        ] {
            let total = hit + miss;
            let pct = if total > 0 {
                100.0 * (hit as f64) / (total as f64)
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "  {:<7}  hit={:>10}  miss={:>10}  hit%={:>5.1}",
                label, hit, miss, pct
            );
        }
        let _ = writeln!(out);

        // Pool hit/miss summary.
        let _ = writeln!(out, "### FramePool reuse");
        for (label, hit, miss) in [
            ("value", c.pool_value_hit, c.pool_value_miss),
            ("float", c.pool_float_hit, c.pool_float_miss),
            ("int", c.pool_int_hit, c.pool_int_miss),
            ("args", c.pool_arg_hit, c.pool_arg_miss),
        ] {
            let total = hit + miss;
            let pct = if total > 0 {
                100.0 * (hit as f64) / (total as f64)
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "  {:<6}  hit={:>10}  miss={:>10}  hit%={:>5.1}",
                label, hit, miss, pct
            );
        }
        let _ = writeln!(out);

        let _ = writeln!(out, "### Slow paths");
        let _ = writeln!(
            out,
            "  dispatch_call (non-cached): {}",
            c.slow_call_dispatch
        );
        let _ = writeln!(out);

        // Layout sizes - useful to see drift.
        let _ = writeln!(out, "### Layout sizes (bytes)");
        let _ = writeln!(out, "  size_of::<Op>     = {}", std::mem::size_of::<Op>());
        let _ = writeln!(
            out,
            "  size_of::<Value>  = {}",
            std::mem::size_of::<crate::value::Value>()
        );

        out
    })
}

/// No-op when the `profile` feature is disabled.
#[cfg(not(feature = "profile"))]
#[must_use]
pub fn dump_report() -> String {
    String::new()
}

/// Map an op tag back to its variant name. Built by exhaustive
/// match so adding a variant without updating here is a compile
/// error inside the `profile` build.
#[cfg(feature = "profile")]
#[allow(
    clippy::too_many_lines,
    reason = "one match whose length is the opcode set"
)]
fn op_label(tag: usize) -> &'static str {
    use crate::bytecode::Op as O;
    // Build a once-cell array indexed by tag → variant name. We
    // construct it by emitting one prototype per variant and
    // reading its tag back through `op_tag`. This keeps the
    // table in lockstep with the `Op` enum without a manual
    // mapping.
    use std::sync::OnceLock;
    static TABLE: OnceLock<[Option<&'static str>; MAX_OPS]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut out: [Option<&'static str>; MAX_OPS] = [None; MAX_OPS];
        let zero = 0u16;
        let r = |op: O| op_tag(op);
        // Emit a prototype variant for each known opcode. Most
        // payloads are `Reg` (`u16`) - we use 0 as a stand-in
        // since we only care about the discriminant.
        let entries: &[(O, &str)] = &[
            (O::LoadConst { dst: 0, idx: 0 }, "LoadConst"),
            (O::LoadGlobal { dst: 0, idx: 0 }, "LoadGlobal"),
            (
                O::StoreStatic {
                    name_idx: 0,
                    src: 0,
                },
                "StoreStatic",
            ),
            (O::Move { dst: 0, src: 0 }, "Move"),
            (O::Deref { dst: 0, src: 0 }, "Deref"),
            (
                O::AddInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                "AddInt",
            ),
            (
                O::SubInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                "SubInt",
            ),
            (
                O::MulInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                "MulInt",
            ),
            (
                O::DivInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                "DivInt",
            ),
            (
                O::RemInt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                    cache_idx: 0,
                },
                "RemInt",
            ),
            (O::Neg { dst: 0, operand: 0 }, "Neg"),
            (O::Not { dst: 0, operand: 0 }, "Not"),
            (
                O::Eq {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Eq",
            ),
            (
                O::Ne {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Ne",
            ),
            (
                O::Lt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Lt",
            ),
            (
                O::Le {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Le",
            ),
            (
                O::Gt {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Gt",
            ),
            (
                O::Ge {
                    dst: 0,
                    lhs: 0,
                    rhs: 0,
                },
                "Ge",
            ),
            (O::Jump { target: 0 }, "Jump"),
            (O::BranchIf { cond: 0, target: 0 }, "BranchIf"),
            (O::BranchIfNot { cond: 0, target: 0 }, "BranchIfNot"),
            (
                O::Call {
                    dst: 0,
                    callee: 0,
                    args: 0,
                    argc: 0,
                    cache_idx: 0,
                    may_have_cells: false,
                },
                "Call",
            ),
            (O::Return { value: 0 }, "Return"),
            (O::ReturnUnit, "ReturnUnit"),
            (
                O::MethodCall {
                    dst: 0,
                    receiver: 0,
                    name_idx: 0,
                    args: 0,
                    argc: 0,
                    cache_idx: 0,
                },
                "MethodCall",
            ),
            (
                O::StreamWriteByte {
                    dst: 0,
                    stream_reg: 0,
                    byte_reg: 0,
                },
                "StreamWriteByte",
            ),
            (
                O::U8VecSetByte {
                    dst: 0,
                    u8vec_reg: 0,
                    idx_reg: 0,
                    byte_reg: 0,
                },
                "U8VecSetByte",
            ),
            (
                O::U8VecGetByte {
                    dst_i: 0,
                    u8vec_reg: 0,
                    idx_reg: 0,
                },
                "U8VecGetByte",
            ),
            (
                O::StrSubstring {
                    dst: 0,
                    recv_reg: 0,
                    start_reg: 0,
                    end_reg: 0,
                },
                "StrSubstring",
            ),
            (
                O::MapIncMethod {
                    dst: 0,
                    map_reg: 0,
                    key_reg: 0,
                    by_reg: 0,
                },
                "MapIncMethod",
            ),
            (
                O::MapInc {
                    dst: 0,
                    map_reg: 0,
                    key_reg: 0,
                    by_reg: 0,
                },
                "MapInc",
            ),
            (O::Wide { idx: 0 }, "Wide"),
            (
                O::BuildIntArray {
                    dst_v: 0,
                    first_i: 0,
                    count: 0,
                },
                "BuildIntArray",
            ),
            (
                O::BuildVecWithCapacity {
                    dst_v: 0,
                    capacity_i: 0,
                    storage: crate::bytecode::FlatVecStorage::I64,
                },
                "BuildVecWithCapacity",
            ),
            (
                O::BuildTuple {
                    dst: 0,
                    first: 0,
                    count: 0,
                },
                "BuildTuple",
            ),
            (O::IntToFloatF64 { dst_f: 0, src_i: 0 }, "IntToFloatF64"),
            (
                O::DivF64ByI64 {
                    dst_f: 0,
                    lhs_f: 0,
                    rhs_i: 0,
                },
                "DivF64ByI64",
            ),
            (O::FloatToIntI64 { dst_i: 0, src_f: 0 }, "FloatToIntI64"),
            (
                O::IntArrayGetI64 {
                    dst_i: 0,
                    base: 0,
                    index_i: 0,
                },
                "IntArrayGetI64",
            ),
            (
                O::BuildFloatVec {
                    dst_v: 0,
                    first_f: 0,
                    count: 0,
                },
                "BuildFloatVec",
            ),
            (
                O::FloatVecGetF64 {
                    dst_f: 0,
                    base: 0,
                    index_i: 0,
                },
                "FloatVecGetF64",
            ),
            (
                O::FloatVecSetF64 {
                    base: 0,
                    index_i: 0,
                    value_f: 0,
                },
                "FloatVecSetF64",
            ),
            (O::BuildIntMap { dst_v: 0 }, "BuildIntMap"),
            (
                O::BuildIntMapWithCapacity {
                    dst_v: 0,
                    capacity_i: 0,
                },
                "BuildIntMapWithCapacity",
            ),
            (
                O::BuildStrIntMapWithCapacity {
                    dst_v: 0,
                    capacity_i: 0,
                },
                "BuildStrIntMapWithCapacity",
            ),
            (
                O::IntMapInc {
                    dst_i: 0,
                    map_reg: 0,
                    key_i: 0,
                    by_i: 0,
                },
                "IntMapInc",
            ),
            (
                O::IntMapGetOr {
                    dst_i: 0,
                    map_reg: 0,
                    key_i: 0,
                    default_i: 0,
                },
                "IntMapGetOr",
            ),
            (
                O::IntMapInsert {
                    dst_v: 0,
                    map_reg: 0,
                    key_i: 0,
                    value_i: 0,
                },
                "IntMapInsert",
            ),
            (
                O::IntMapLen {
                    dst_i: 0,
                    map_reg: 0,
                },
                "IntMapLen",
            ),
            (
                O::IntMapContainsKey {
                    dst_v: 0,
                    map_reg: 0,
                    key_i: 0,
                },
                "IntMapContainsKey",
            ),
            (
                O::IndexGet {
                    dst: 0,
                    base: 0,
                    index: 0,
                },
                "IndexGet",
            ),
            (
                O::IndexSet {
                    base: 0,
                    index: 0,
                    value: 0,
                },
                "IndexSet",
            ),
            (
                O::FieldGet {
                    dst: 0,
                    receiver: 0,
                    name_idx: 0,
                    cache_idx: 0,
                },
                "FieldGet",
            ),
            (
                O::FieldSet {
                    receiver: 0,
                    name_idx: 0,
                    value: 0,
                },
                "FieldSet",
            ),
            (
                O::TupleIndex {
                    dst: 0,
                    receiver: 0,
                    index: 0,
                },
                "TupleIndex",
            ),
            (
                O::IndexedFieldSet {
                    base: 0,
                    index: 0,
                    name_idx: 0,
                    value: 0,
                },
                "IndexedFieldSet",
            ),
            (O::LoadConstF64 { dst_f: 0, idx: 0 }, "LoadConstF64"),
            (
                O::AddF64 {
                    dst_f: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "AddF64",
            ),
            (
                O::SubF64 {
                    dst_f: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "SubF64",
            ),
            (
                O::MulF64 {
                    dst_f: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "MulF64",
            ),
            (
                O::DivF64 {
                    dst_f: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "DivF64",
            ),
            (O::NegF64 { dst_f: 0, src_f: 0 }, "NegF64"),
            (
                O::LtF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "LtF64",
            ),
            (
                O::LeF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "LeF64",
            ),
            (
                O::GtF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "GtF64",
            ),
            (
                O::GeF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "GeF64",
            ),
            (
                O::EqF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "EqF64",
            ),
            (
                O::NeF64 {
                    dst_v: 0,
                    lhs_f: 0,
                    rhs_f: 0,
                },
                "NeF64",
            ),
            (
                O::UnboxF64 {
                    dst_f: 0,
                    src_v: 0,
                    peer_v: None,
                },
                "UnboxF64",
            ),
            (O::BoxF64 { dst_v: 0, src_f: 0 }, "BoxF64"),
            (O::SqrtF64 { dst_f: 0, src_f: 0 }, "SqrtF64"),
            (O::SinF64 { dst_f: 0, src_f: 0 }, "SinF64"),
            (O::CosF64 { dst_f: 0, src_f: 0 }, "CosF64"),
            (O::AbsF64 { dst_f: 0, src_f: 0 }, "AbsF64"),
            (O::FloorF64 { dst_f: 0, src_f: 0 }, "FloorF64"),
            (O::CeilF64 { dst_f: 0, src_f: 0 }, "CeilF64"),
            (O::ExpF64 { dst_f: 0, src_f: 0 }, "ExpF64"),
            (O::LnF64 { dst_f: 0, src_f: 0 }, "LnF64"),
            (
                O::MulAddF64 {
                    dst_f: 0,
                    a_f: 0,
                    b_f: 0,
                    c_f: 0,
                },
                "MulAddF64",
            ),
            (
                O::MulSubF64 {
                    dst_f: 0,
                    a_f: 0,
                    b_f: 0,
                    c_f: 0,
                },
                "MulSubF64",
            ),
            (O::LoadConstI64 { dst_i: 0, idx: 0 }, "LoadConstI64"),
            (
                O::AddI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "AddI64",
            ),
            (
                O::SubI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "SubI64",
            ),
            (
                O::MulI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "MulI64",
            ),
            (
                O::DivI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "DivI64",
            ),
            (
                O::RemI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "RemI64",
            ),
            (
                O::DivU64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "DivU64",
            ),
            (
                O::RemU64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "RemU64",
            ),
            (O::NegI64 { dst_i: 0, src_i: 0 }, "NegI64"),
            (
                O::LtI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "LtI64",
            ),
            (
                O::LeI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "LeI64",
            ),
            (
                O::GtI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "GtI64",
            ),
            (
                O::GeI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "GeI64",
            ),
            (
                O::EqI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "EqI64",
            ),
            (
                O::NeI64 {
                    dst_v: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "NeI64",
            ),
            (
                O::BitAndI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "BitAndI64",
            ),
            (
                O::BitOrI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "BitOrI64",
            ),
            (
                O::BitXorI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "BitXorI64",
            ),
            (
                O::ShlI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "ShlI64",
            ),
            (
                O::ShrI64 {
                    dst_i: 0,
                    lhs_i: 0,
                    rhs_i: 0,
                },
                "ShrI64",
            ),
            (
                O::UnboxI64 {
                    dst_i: 0,
                    src_v: 0,
                    peer_v: None,
                },
                "UnboxI64",
            ),
            (O::BoxI64 { dst_v: 0, src_i: 0 }, "BoxI64"),
            (O::MoveF64 { dst_f: 0, src_f: 0 }, "MoveF64"),
            (O::MoveI64 { dst_i: 0, src_i: 0 }, "MoveI64"),
            (
                O::FieldGetF64 {
                    dst_f: 0,
                    receiver: 0,
                    name_idx: 0,
                },
                "FieldGetF64",
            ),
            (
                O::IndexedFieldGet {
                    dst: 0,
                    base: 0,
                    index: 0,
                    name_idx: 0,
                },
                "IndexedFieldGet",
            ),
            (
                O::IndexedFieldGetF64 {
                    dst_f: 0,
                    base: 0,
                    index: 0,
                    name_idx: 0,
                },
                "IndexedFieldGetF64",
            ),
            (
                O::IndexedFieldSetF64 {
                    base: 0,
                    index: 0,
                    name_idx: 0,
                    value_f: 0,
                },
                "IndexedFieldSetF64",
            ),
            (
                O::IndexedFieldGetF64ByOffset {
                    dst_f: 0,
                    base: 0,
                    index: 0,
                    offset: 0,
                },
                "IndexedFieldGetF64ByOffset",
            ),
            (
                O::IndexedFieldSetF64ByOffset {
                    base: 0,
                    index: 0,
                    offset: 0,
                    value_f: 0,
                },
                "IndexedFieldSetF64ByOffset",
            ),
            (
                O::BranchIfLtI64 {
                    lhs_i: 0,
                    rhs_i: 0,
                    target: 0,
                },
                "BranchIfLtI64",
            ),
            (
                O::BranchIfGeI64 {
                    lhs_i: 0,
                    rhs_i: 0,
                    target: 0,
                },
                "BranchIfGeI64",
            ),
            (
                O::BranchIfGtI64 {
                    lhs_i: 0,
                    rhs_i: 0,
                    target: 0,
                },
                "BranchIfGtI64",
            ),
            (
                O::BranchIfLtF64 {
                    lhs_f: 0,
                    rhs_f: 0,
                    target: 0,
                },
                "BranchIfLtF64",
            ),
            (
                O::BranchIfGeF64 {
                    lhs_f: 0,
                    rhs_f: 0,
                    target: 0,
                },
                "BranchIfGeF64",
            ),
            (
                O::FieldGetF64ByOffset {
                    dst_f: 0,
                    receiver: 0,
                    offset: 0,
                },
                "FieldGetF64ByOffset",
            ),
            (
                O::FlatGetF64 {
                    dst_f: 0,
                    base: 0,
                    index: 0,
                    stride: 0,
                    offset: 0,
                },
                "FlatGetF64",
            ),
            (
                O::FlatSetF64 {
                    base: 0,
                    index: 0,
                    stride: 0,
                    offset: 0,
                    value_f: 0,
                },
                "FlatSetF64",
            ),
            (
                O::IncJumpIfLtI64 {
                    counter_i: 0,
                    end_i: 0,
                    target: 0,
                },
                "IncJumpIfLtI64",
            ),
            (
                O::IncJumpIfLeI64 {
                    counter_i: 0,
                    end_i: 0,
                    target: 0,
                },
                "IncJumpIfLeI64",
            ),
            (
                O::ArithImmI64 {
                    kind: crate::bytecode::ImmArithKind::Add,
                    dst_i: 0,
                    lhs_i: 0,
                    imm: 0,
                },
                "ArithImmI64",
            ),
        ];
        let _ = zero;
        let _ = r;
        for (op, name) in entries {
            let t = op_tag(*op);
            if t < MAX_OPS {
                out[t] = Some(*name);
            }
        }
        out
    });
    table.get(tag).copied().flatten().unwrap_or("?unknown")
}

#[cfg(not(feature = "profile"))]
fn op_label(_tag: usize) -> &'static str {
    "?"
}
