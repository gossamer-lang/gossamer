//! Register-based bytecode for the VM.
//! Each `FnChunk` owns a flat vector of [`Op`] instructions plus a
//! constant pool. Registers are virtual `u16` indices into the active
//! frame's register file. The compiler in [`crate::compile`] allocates
//! a contiguous register for every HIR local and intermediate value.

#![forbid(unsafe_code)]
#![allow(missing_docs, unreachable_pub)]

use std::sync::Arc;

use crate::value::Value;
use gossamer_types::IntTy;

/// Virtual register index within a frame's register file.
pub type Reg = u16;

/// Index into a function's constant pool.
pub type ConstIdx = u16;

/// Global symbol index resolved at link time.
pub type GlobalIdx = u16;

/// Absolute instruction index inside a chunk.
pub type InstrIdx = u32;

/// Bytecode instructions. The VM dispatch loop is a `match` over this
/// enum - fast enough for the compiled-tier parity bar
/// and trivially safe. Every variant's payload is `Copy`, so the
/// dispatch loop can pull instructions without cloning. The
/// explicit `u16` discriminant keeps the per-op memory footprint
/// (and therefore the memcpy per dispatch) as small as the
/// largest variant's payload allows.
#[derive(Debug, Clone, Copy)]
#[repr(u16)]
pub enum Op {
    /// `dst = consts[idx]`.
    LoadConst { dst: Reg, idx: ConstIdx },
    /// `dst = globals[idx]`.
    LoadGlobal { dst: Reg, idx: GlobalIdx },
    /// `globals[idx] = src`. Stores `src` into the `static mut` cell
    /// named by `globals[idx]` (a `Global::MutStatic`). The cell is
    /// shared across goroutines, so the write is published under the
    /// cell's `Mutex`.
    StoreStatic { name_idx: GlobalIdx, src: Reg },
    /// `dst = src`.
    Move { dst: Reg, src: Reg },
    /// `dst = *src`. Resolves a `__Cell` flag handle to its
    /// current value via the per-thread `CELL_REGISTRY`; passes
    /// other shapes through unchanged.
    Deref { dst: Reg, src: Reg },
    /// Generic boxed-`Value` addition: `dst = lhs + rhs`. Carries
    /// an inline-cache slot index that the runtime fills on first
    /// execution with the observed `(lhs, rhs)` shape (see
    /// `ArithCacheSlot` / `ARITH_*` constants); subsequent
    /// dispatches branch directly into the specialised arm and
    /// skip the per-call `(Value, Value)` discriminant match.
    /// Tier C2 of the interp wow plan.
    AddInt {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        cache_idx: u16,
    },
    /// `dst = lhs - rhs` on boxed `Value`. Adaptive - see [`Op::AddInt`].
    SubInt {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        cache_idx: u16,
    },
    /// `dst = lhs * rhs` on boxed `Value`. Adaptive - see [`Op::AddInt`].
    MulInt {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        cache_idx: u16,
    },
    /// `dst = lhs / rhs` on boxed `Value`. Adaptive - see [`Op::AddInt`].
    DivInt {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        cache_idx: u16,
    },
    /// `dst = lhs % rhs` on boxed `Value`. Adaptive - see [`Op::AddInt`].
    RemInt {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        cache_idx: u16,
    },
    /// `dst = -operand` on `Int` or `Float`.
    Neg { dst: Reg, operand: Reg },
    /// `dst = !operand` on `Bool`.
    Not { dst: Reg, operand: Reg },
    /// `dst = lhs == rhs`, kind-aware.
    Eq { dst: Reg, lhs: Reg, rhs: Reg },
    /// `dst = lhs != rhs`.
    Ne { dst: Reg, lhs: Reg, rhs: Reg },
    /// `dst = lhs < rhs`.
    Lt { dst: Reg, lhs: Reg, rhs: Reg },
    /// `dst = lhs <= rhs`.
    Le { dst: Reg, lhs: Reg, rhs: Reg },
    /// `dst = lhs > rhs`.
    Gt { dst: Reg, lhs: Reg, rhs: Reg },
    /// `dst = lhs >= rhs`.
    Ge { dst: Reg, lhs: Reg, rhs: Reg },
    /// Unconditional jump to `target`.
    Jump { target: InstrIdx },
    /// Branch to `target` when `cond` is truthy; fall through otherwise.
    BranchIf { cond: Reg, target: InstrIdx },
    /// Branch to `target` when `cond` is falsy.
    BranchIfNot { cond: Reg, target: InstrIdx },
    /// Call `callee` with `argc` arguments drawn from consecutive
    /// registers starting at `args`. Stores the result in `dst`.
    Call {
        /// Destination register for the returned value.
        dst: Reg,
        /// Register holding the callee value.
        callee: Reg,
        /// First argument register. Arguments live in
        /// `[args .. args + argc)`.
        args: Reg,
        /// Number of arguments.
        argc: u16,
        /// Index into the chunk's `call_caches` slot. The slot
        /// caches the resolved `crate::vm::Global` for the most
        /// recently seen callee identity, skipping the
        /// `Value::String → globals.get` path on subsequent calls
        /// from the same site.
        cache_idx: u16,
        /// `true` when at least one argument could evaluate to a
        /// `flag::Cell` (`__Cell`) handle that needs auto-dereferencing
        /// at the call boundary. `false` when every argument is a
        /// primitive scalar (which can never be a `__Cell`), letting
        /// the dispatch skip the per-argument cell check.
        may_have_cells: bool,
    },
    /// Calls the named global at `global_idx` without first materializing its
    /// name in a value register. Statically resolved path calls use this form;
    /// dynamic callable values continue to use [`Op::Call`].
    CallGlobal {
        /// Destination register for the returned value.
        dst: Reg,
        /// Index into [`FnChunk::globals`].
        global_idx: GlobalIdx,
        /// First argument register.
        args: Reg,
        /// Number of arguments.
        argc: u16,
        /// Per-VM call-cache slot.
        cache_idx: u16,
        /// Whether arguments may contain auto-dereferenced cells.
        may_have_cells: bool,
    },
    /// `ret value`.
    Return { value: Reg },
    /// `ret ()`.
    ReturnUnit,
    /// Drops the live values in `count` consecutive `Value` registers from
    /// `start`, setting each to `Value::Void`. Emitted at a loop back-edge
    /// to release the iteration's per-iteration aggregates (the freshly
    /// built tree, the destructure tuple temporary) in a single dispatch,
    /// so the next iteration allocates against a reclaimed working set
    /// rather than overlapping its predecessor. Output-invariant: the
    /// cleared registers are the loop body's own dead temporaries.
    ClearRegs { start: Reg, count: Reg },
    /// Diverges with `RuntimeError::Panic`, reading the message from the
    /// `Value::String` at `consts[msg]`. Emitted on a `match`'s
    /// fall-through path so a value that escapes every arm (a guard gap
    /// the exhaustiveness checker could not see) panics cleanly, matching
    /// the compiled tiers' `Terminator::Panic` rather than returning a
    /// zero value.
    Panic { msg: ConstIdx },
    /// Diverges with `RuntimeError::Type`, reading the source-level message
    /// from the `Value::String` at `consts[msg]`.
    TypeError { msg: ConstIdx },
    /// `dst = closure value` - builds a [`crate::value::Closure`]
    /// from the proto at `FnChunk::closure_protos[proto]`. The
    /// handler snapshots each register named in the proto's
    /// `capture_regs` into the closure's upvalue list, then forms a
    /// `Value::Closure` referencing the proto's compiled body chunk.
    MakeClosure {
        /// Destination register for the `Value::Closure`.
        dst: Reg,
        /// Index into [`FnChunk::closure_protos`].
        proto: u32,
    },
    /// Native `select { … }` dispatch over [`Value::Channel`] arms.
    /// The arm metadata for this select occupies the contiguous range
    /// `FnChunk::select_arms[first .. first + count]`. The handler
    /// polls communication arms in pseudo-random order (recv via
    /// `try_recv`, send via `try_send`, a `default` arm last), parking
    /// across all channel arms when nothing is ready and no `default` exists. On a
    /// winning recv it writes the received value into the arm's
    /// `bind_reg`; in every case it sets `pc` to the winning arm's
    /// `body_block`, which moves the arm's result into the shared
    /// select-result register and jumps to the continuation.
    Select {
        /// First arm's index into [`FnChunk::select_arms`].
        first: u32,
        /// Number of arms in this select.
        count: u16,
    },
    /// Records one line-coverage hit at a pre-registered counter slot.
    /// Emitted at every statement boundary only when `gos test
    /// --coverage` compiles the program with a source map published
    /// (see [`crate::vm::Vm::set_source_map`]); the slot is the
    /// [`gossamer_runtime::coverage::register`] index the compiler
    /// resolved the statement's `(file, line)` to. The handler bumps
    /// that global counter, so the bytecode tier feeds the same lcov
    /// table the LLVM AOT tier instruments.
    CovHit {
        /// Index into the global `gossamer_runtime::coverage` table.
        slot: u32,
    },
    /// `dst = receiver.method_name(args…)` - native method
    /// dispatch. `name_idx` is a `ConstIdx` into the chunk's
    /// globals table (keyed by the bare method name). The VM
    /// puts the receiver value at `args` and the remaining args
    /// at `args+1..args+argc+1`, then calls the looked-up
    /// builtin / closure.
    MethodCall {
        /// Destination register.
        dst: Reg,
        /// Register holding the receiver value.
        receiver: Reg,
        /// Index into `FnChunk::globals` - holds the bare
        /// method name.
        name_idx: GlobalIdx,
        /// First user-arg register. Receiver is stored at
        /// `args - 1` during dispatch so the call frame sees
        /// `[receiver, a0, a1, …]`.
        args: Reg,
        /// Number of user-supplied arguments.
        argc: u16,
        /// Index into the chunk's `call_caches` slot. The slot
        /// caches the resolved `crate::vm::Global` for the most
        /// recently seen receiver type, skipping the
        /// `qualified_key`/`HashMap::get` chain on subsequent
        /// calls from the same site.
        cache_idx: u16,
    },
    /// Specialised `<stream>.write_byte(<byte>)` - fused
    /// super-instruction emitted whenever the compiler sees a
    /// method call whose name is `write_byte` and whose argc is 1.
    /// fasta's hot loop is dominated by per-character calls
    /// through this exact shape; bypassing the
    /// `MethodCall` + IC + Vec-args + builtin-extract chain saves
    /// the receiver clone + per-call buf-init + indirect dispatch.
    /// The handler verifies the receiver is a `Value::Struct`
    /// named `"Stream"` at runtime and falls back to a regular
    /// `MethodCall`-shaped lookup if not - so emitting this op
    /// for any user-defined `write_byte` is still correct, just
    /// not as fast.
    StreamWriteByte {
        /// Destination register (always written `Value::Unit`
        /// since `write_byte` returns unit).
        dst: Reg,
        /// Register holding the stream value (a
        /// `Value::struct_("Stream", [(fd)`
        /// in the steady state).
        stream_reg: Reg,
        /// Register holding the byte (a `Value::Int` in
        /// `[0, 255]` in the steady state).
        byte_reg: Reg,
    },
    /// Specialised `<u8vec>.set_byte(<idx>, <byte>)` - the
    /// `U8Vec` counterpart to [`Op::StreamWriteByte`]. The runtime
    /// inlines the handle lookup and `AtomicU8::store`, skipping
    /// the `Op::MethodCall` IC + builtin `&[Value]` round-trip
    /// per call. fasta's per-byte buffer fill rides this op.
    /// Falls back to a generic method dispatch on shape miss.
    U8VecSetByte {
        /// Destination register (always `Value::Unit` since
        /// `set_byte` returns unit).
        dst: Reg,
        /// Register holding the `U8Vec` receiver
        /// (`Value::Struct{ name: "U8Vec", … }`).
        u8vec_reg: Reg,
        /// Register holding the byte index (`Value::Int`).
        idx_reg: Reg,
        /// Register holding the byte value (`Value::Int` in
        /// `[0, 255]`).
        byte_reg: Reg,
    },
    /// Specialised `<u8vec>.get_byte(<idx>)` returning into a
    /// typed `i64` register. Mirror of [`Op::U8VecSetByte`] for
    /// reads - the typed destination lets a downstream `Op::AddI64`
    /// chain off the result without a `Value::Int` round-trip.
    U8VecGetByte {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Register holding the `U8Vec` receiver.
        u8vec_reg: Reg,
        /// Register holding the byte index (`Value::Int`).
        idx_reg: Reg,
    },
    /// Specialised `<str>.substring(<start>, <end>) -> String` -
    /// fused super-instruction for the sliding-window k-mer loop.
    /// Bypasses the `Op::MethodCall` IC + receiver clone + `&[Value]`
    /// buffer + builtin indirection, reading the receiver and bounds
    /// straight from their registers. Verifies the receiver is a
    /// `Value::String` and both bounds are `Value::Int` at runtime,
    /// falling back to a generic `substring` dispatch on shape miss -
    /// so emitting it for any user-defined `substring` is still
    /// correct, just not as fast.
    StrSubstring {
        /// Destination value register (the resulting `Value::String`).
        dst: Reg,
        /// Register holding the receiver string (`Value::String`).
        recv_reg: Reg,
        /// Register holding the start byte index (`Value::Int`).
        start_reg: Reg,
        /// Register holding the end byte index (`Value::Int`).
        end_reg: Reg,
    },
    /// Specialised `m.inc(key[, by]) -> i64` counter increment for a
    /// `HashMap`-typed receiver - the method form, distinct from the
    /// `Op::MapInc` / `Op::IntMapInc` ops that fuse the
    /// `m.insert(k, m.get_or(k, 0) + by)` pattern. Acquires the map's
    /// lock once and increments in place, skipping the `Op::MethodCall`
    /// IC + map-handle clone + `&[Value]` round-trip that dominate a
    /// counting loop. Dispatches on the actual map storage
    /// (`StrIntMap` / `IntMap` / boxed `Map`) at runtime and falls back
    /// to a generic `inc` dispatch on shape miss.
    MapIncMethod {
        /// Destination register (the post-increment `Value::Int`).
        dst: Reg,
        /// Register holding the map receiver.
        map_reg: Reg,
        /// Register holding the key.
        key_reg: Reg,
        /// Register holding the increment (`Value::Int`; defaults to
        /// a loaded `1` for the `m.inc(key)` form).
        by_reg: Reg,
    },
    /// In-place generic map insertion without method lookup or argument
    /// marshalling. The destination receives the same map handle returned by
    /// the source-level `insert` method.
    MapInsert {
        dst: Reg,
        map_reg: Reg,
        key_reg: Reg,
        value_reg: Reg,
    },
    /// Specialised `m.insert(k, m.get_or(k, 0) + by)` - fused
    /// counter-increment super-instruction. Collapses the two
    /// `MethodCall`s, two IC probes, two arg-vec materialisations,
    /// and (crucially) the two `parking_lot::Mutex` acquisitions
    /// into a single `entry()`-API increment under one lock.
    /// Counter-style hot loops are dominated by this pattern.
    MapInc {
        /// Destination register (the resulting Map handle, mirroring
        /// the original `insert` return value).
        dst: Reg,
        /// Register holding the Map (`Value::Map`).
        map_reg: Reg,
        /// Register holding the key (any hashable Value).
        key_reg: Reg,
        /// Register holding the increment (`Value::Int`).
        by_reg: Reg,
    },
    /// Specialised `m.inc_at(seq, start, len, by)` - zero-copy
    /// slice-hash counter that hashes `seq[start..start+len]`
    /// directly, matching `*m.entry(&seq[i..i+k]).or_insert(0)
    /// += by`. Skips the generic builtin-call overhead by
    /// inlining the slice-hash + entry increment under one Mutex
    /// acquisition. Result register holds the post-increment
    /// value as a `Value::Int`. Carried via `WideOp::MapIncAt` in
    /// the chunk's `wide_ops` side-table - see `Op::Wide`.
    Wide {
        /// Index into `FnChunk::wide_ops`.
        idx: u16,
    },
    /// Builds a `Value::IntArray` from `count` consecutive typed
    /// `i64` registers starting at `first_i`. Counterpart of
    /// `WideOp::BuildFloatArray` for primitive integer arrays
    /// (`[i64; N]` literals).
    BuildIntArray {
        /// Destination value register.
        dst_v: Reg,
        /// First `i64` register holding the array's elements
        /// (contiguous, length `count`).
        first_i: Reg,
        /// Number of elements.
        count: u16,
    },
    /// Builds a packed `Value::ByteArray` from consecutive `i64` registers.
    BuildByteArray {
        /// Destination value register.
        dst_v: Reg,
        /// First register holding zero-extended byte values.
        first_i: Reg,
        /// Number of elements.
        count: u16,
    },
    /// Builds a packed repeated byte array without an intermediate wide array.
    BuildByteArrayRepeat {
        /// Destination value register.
        dst_v: Reg,
        /// Register holding the repeated byte value.
        value_i: Reg,
        /// Register holding the non-negative repeat count.
        count_v: Reg,
    },
    /// Rejects a negative `i64` before using it as a collection capacity.
    CheckNonNegativeCapacity {
        /// Capacity in the typed integer register file.
        capacity_i: Reg,
    },
    /// Builds a `Value::Tuple` from `count` consecutive value
    /// registers starting at `first` for an `(a, b, …)` literal.
    /// `Arc::clone`s each register and assembles the tuple.
    BuildTuple {
        /// Destination value register.
        dst: Reg,
        /// First value register holding the tuple's elements.
        first: Reg,
        /// Number of elements.
        count: u16,
    },
    /// Builds a `Value::Array` from `count` consecutive value
    /// registers starting at `first`. The generic array-literal
    /// (`[a, b, c]`) counterpart of `BuildTuple` for element types
    /// the typed-storage builders (`BuildIntArray` / `BuildFloatVec`)
    /// don't specialise - strings, structs, bools, nested arrays.
    BuildArray {
        /// Destination value register.
        dst: Reg,
        /// First value register holding the array's elements.
        first: Reg,
        /// Number of elements.
        count: u16,
    },
    /// Builds a `Value::Array` of `registers[count]` clones of
    /// `registers[value]`. The generic `[v; n]` repeat counterpart for
    /// element types the typed-storage repeat builders don't
    /// specialise. The count register holds a `Value::Int`.
    BuildArrayRepeat {
        /// Destination value register.
        dst: Reg,
        /// Register holding the element to clone.
        value: Reg,
        /// Register holding the repeat count (`Value::Int`).
        count: Reg,
    },
    /// Builds a lazy standalone integer range.
    BuildRange {
        /// Destination value register.
        dst: Reg,
        /// Register holding the lower bound (`Value::Int`).
        start: Reg,
        /// Register holding the upper bound (`Value::Int`).
        end: Reg,
        /// `true` when the upper bound is inclusive (`a..=b`).
        inclusive: bool,
        /// Whether the source omitted the lower bound.
        start_open: bool,
        /// Whether the source omitted the upper bound.
        end_open: bool,
    },
    /// Constructs a one-field enum variant without generic call dispatch.
    BuildVariant1 {
        dst: Reg,
        name_idx: ConstIdx,
        field: Reg,
        take_field: bool,
    },
    /// Constructs a two-field enum variant without generic call dispatch.
    BuildVariant2 {
        dst: Reg,
        name_idx: ConstIdx,
        first: Reg,
        second: Reg,
        take_first: bool,
        take_second: bool,
    },
    /// Typed numeric cast: `i64 as f64`. Reads from the `i64`
    /// register file and writes to the `f64` register file with
    /// no boxing.
    IntToFloatF64 {
        /// Destination `f64` register.
        dst_f: Reg,
        /// Source `i64` register.
        src_i: Reg,
    },
    /// Typed numeric cast: `f64 as i64` (truncation toward
    /// zero, matching Rust `as` semantics).
    FloatToIntI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Source `f64` register.
        src_f: Reg,
    },
    /// Narrowing integer cast - truncates an i64 register to a
    /// target width (in bits) and sign- or zero-extends back to i64.
    /// Implements Rust-style wrapping `as` semantics for `i64 as i32`,
    /// `i64 as u8`, etc.
    TruncCastI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Source `i64` register.
        src_i: Reg,
        /// Bits to shift: `64 - target_bits` (e.g. 32 for i32/u32).
        shift: u8,
        /// `true` → arithmetic (sign-extending); `false` → logical
        /// (zero-extending) right shift after the left shift.
        signed: bool,
    },
    /// Typed read into an `i64` register from a `Value::IntArray`
    /// base. Skips the per-read enum match + boxing the generic
    /// `Op::IndexGet` performs. fasta's TWO/THREE inner loops
    /// hit this op ~5 times per output byte.
    IntArrayGetI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Value register holding the `Value::IntArray`.
        base: Reg,
        /// `i64` register holding the index. Negative indices
        /// surface as a runtime error.
        index_i: Reg,
    },
    /// Typed write of an `i64` register into a `Value::IntArray`
    /// at `index_i`. Mirrors [`Op::FloatVecSetF64`] for integer
    /// arrays - fannkuch's `perm[j] = perm1[j]` and similar
    /// in-place updates avoid the box/unbox round-trip the
    /// generic `Op::IndexSet` imposes.
    IntArraySetI64 {
        /// Value register holding the `Value::IntArray`.
        base: Reg,
        /// `i64` register holding the index.
        index_i: Reg,
        /// `i64` register holding the new element.
        value_i: Reg,
    },
    /// Fused discarded-result swap on a `Value::IntArray`. Replaces the
    /// 4-op sequence (two `IntArrayGetI64` + two `IntArraySetI64`)
    /// the swap super-instruction would otherwise emit. fannkuch's
    /// `perm.swap(a, k - a)` runs millions of times per workload.
    IntArraySwap {
        /// Value register holding the `Value::IntArray`.
        base: Reg,
        /// `i64` register holding the first index.
        i_i: Reg,
        /// `i64` register holding the second index.
        j_i: Reg,
    },
    /// Fused in-place swap on a `Value::FloatVec`. Same shape as
    /// [`Op::IntArraySwap`] but for primitive `[f64; N]` storage.
    FloatVecSwap {
        /// Value register holding the `Value::FloatVec`.
        base: Reg,
        /// `i64` register holding the first index.
        i_i: Reg,
        /// `i64` register holding the second index.
        j_i: Reg,
    },
    /// Builds a `Value::FloatVec` by copying `count` consecutive
    /// `f64` registers starting at `first_f`. Mirrors `BuildIntArray`
    /// but for primitive `[f64; N]` literals so subsequent indexed
    /// reads route through the typed-`f64` fast path.
    BuildFloatVec {
        /// Destination `Value` register.
        dst_v: Reg,
        /// First float register in the source span.
        first_f: Reg,
        /// Number of f64 elements to gather.
        count: u16,
    },
    /// Typed read into an `f64` register from a `Value::FloatVec`
    /// at `index_i`. Skips the boxed `Value::Float` round-trip the
    /// generic `Op::IndexGet` would impose.
    FloatVecGetF64 {
        /// Destination `f64` register.
        dst_f: Reg,
        /// Value register holding the `Value::FloatVec`.
        base: Reg,
        /// `i64` register holding the index.
        index_i: Reg,
    },
    /// Typed write into a `Value::FloatVec` from an `f64` register.
    /// `Arc::make_mut` mutates the inner `Vec<f64>` in place when
    /// the `FloatVec` has unique ownership.
    FloatVecSetF64 {
        /// Value register holding the `Value::FloatVec`.
        base: Reg,
        /// `i64` register holding the index.
        index_i: Reg,
        /// Source `f64` register.
        value_f: Reg,
    },
    /// Constructs an empty `Value::IntMap` (typed `HashMap<i64, i64>`).
    /// Emitted in place of a `HashMap::new()` call when the type
    /// checker can prove the map's key + value types are both
    /// `i64`. Hot integer counter loops route through this op.
    BuildIntMap {
        /// Destination `Value` register.
        dst_v: Reg,
    },
    /// Constructs an empty `Value::StrIntMap` (typed
    /// `HashMap<String, i64>`). Emitted in place of `HashMap::new()`
    /// when the type checker proves the key is `String` and the value
    /// is `i64`. String-keyed counter loops route their entries
    /// through the unboxed `(SmolStr, i64)` storage.
    BuildStrIntMap {
        /// Destination `Value` register.
        dst_v: Reg,
    },
    /// Typed counterpart to [`Op::MapInc`] for `Value::IntMap`. Reads
    /// the key and increment from the i64 register file, mutates
    /// the map's slot in place, and writes the post-increment value
    /// to `dst_i`. Skips the `MapKey` enum dispatch and the
    /// `Value::Int` box that the generic `Op::MapInc` does.
    IntMapInc {
        /// Destination `i64` register receiving the post-increment value.
        dst_i: Reg,
        /// `Value` register holding the `Value::IntMap`.
        map_reg: Reg,
        /// `i64` register holding the key.
        key_i: Reg,
        /// `i64` register holding the increment amount.
        by_i: Reg,
    },
    /// `dst_i = map.get_or(key, default)` for `Value::IntMap`.
    IntMapGetOr {
        /// Destination `i64` register.
        dst_i: Reg,
        /// `Value` register holding the `Value::IntMap`.
        map_reg: Reg,
        /// `i64` register holding the key.
        key_i: Reg,
        /// `i64` register holding the default to return on miss.
        default_i: Reg,
    },
    /// `map.insert(key, value)` for `Value::IntMap`. The map handle
    /// stays in `map_reg`; `dst_v` receives the previous value as `Option<i64>`.
    IntMapInsert {
        /// Destination `Value` register receiving `Option<i64>`.
        dst_v: Reg,
        /// `Value` register holding the `Value::IntMap`.
        map_reg: Reg,
        /// `i64` register holding the key.
        key_i: Reg,
        /// `i64` register holding the value to store.
        value_i: Reg,
    },
    /// `dst_i = map.len()` for `Value::IntMap`. Locks once, reads
    /// `len()`, returns. No `MapKey` allocation.
    IntMapLen {
        /// Destination `i64` register.
        dst_i: Reg,
        /// `Value` register holding the `Value::IntMap`.
        map_reg: Reg,
    },
    /// `dst_v = bool(map.contains_key(key))` for `Value::IntMap`.
    IntMapContainsKey {
        /// Destination `Value` register (holds `Value::Bool`).
        dst_v: Reg,
        /// `Value` register holding the `Value::IntMap`.
        map_reg: Reg,
        /// `i64` register holding the key.
        key_i: Reg,
    },
    /// `go receiver.method_name(args[0..argc])` - spawns a    /// `dst = base[index]` - native indexed read over arrays,
    /// strings, tuples, vecs, and structs (tuple-struct
    /// projection).
    IndexGet {
        /// Destination register.
        dst: Reg,
        /// Register holding the base (array / string / …).
        base: Reg,
        /// Register holding the index value.
        index: Reg,
    },
    /// `s.byte_at(i)` specialised for a statically-`String` receiver:
    /// the UTF-8 byte at index `i` as an `i64`, or `0` when the receiver
    /// is not a string, the index is not an integer, or the index is
    /// out of `[0, len)`. Emitted only when the receiver's static type
    /// is `String`, bypassing the `MethodCall` arg-materialisation,
    /// inline-cache probe, and builtin dispatch that dominate
    /// byte-scanning loops in bytecode.
    StrByteAt {
        /// Destination register.
        dst: Reg,
        /// Register holding the string receiver.
        recv: Reg,
        /// Register holding the index value.
        idx: Reg,
    },
    /// Typed form of [`Op::StrByteAt`] for integer-producing expressions.
    /// Keeping both the index and result in the `i64` register file avoids a
    /// `Value::Int` box/unbox pair for every byte in string-scanning loops.
    StrByteAtI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Value register holding the string receiver.
        recv: Reg,
        /// `i64` register holding the index.
        idx_i: Reg,
    },
    /// Fused wrapping addition of a string byte to an integer accumulator.
    /// This is the typed lowering of `sum.wrapping_add(s.byte_at(i))`.
    StrByteAtAddI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// `i64` register holding the accumulator.
        lhs_i: Reg,
        /// Value register holding the string receiver.
        recv: Reg,
        /// `i64` register holding the byte index.
        idx_i: Reg,
    },
    /// Byte length of a statically typed string into the integer register
    /// file, avoiding generic method dispatch and result boxing in loops.
    StrLenI64 {
        /// Destination `i64` register.
        dst_i: Reg,
        /// Value register holding the string receiver.
        recv: Reg,
    },
    /// `base[index]` where the element is an aggregate (struct / tuple /
    /// array). An out-of-range index panics with `index out of bounds`
    /// instead of yielding the lenient zero value, matching the compiled
    /// tiers (a zero aggregate cannot be cheaply materialized, and reading a
    /// missing aggregate element would otherwise feed a bogus value into a
    /// field/element access). Primitive-element indexing keeps `IndexGet`.
    IndexGetChecked {
        /// Destination register.
        dst: Reg,
        /// Register holding the base.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
    },
    /// `base[index] = value` - native indexed write.
    IndexSet {
        /// Register holding the base.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Register holding the value to store.
        value: Reg,
    },
    /// `dst = receiver.field_name` - native struct-field read.
    /// `name_idx` is a const-pool index holding a
    /// `Value::String` with the field name.
    FieldGet {
        /// Destination register.
        dst: Reg,
        /// Register holding the struct value.
        receiver: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
        /// Per-`Vm` field-cache slot. On hit, the dispatcher
        /// jumps straight to `inner.fields[offset].1.clone()`,
        /// skipping the linear name scan that the generic
        /// fallback does. On miss (observed struct shape
        /// changed), refill the slot. PEP 659-style.
        cache_idx: u16,
    },
    /// `receiver.field_name = value` - native struct-field
    /// write. Mutates the fields vector in place (`Arc::make_mut`
    /// semantics).
    FieldSet {
        /// Register holding the struct value.
        receiver: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
        /// Register holding the value to store.
        value: Reg,
    },
    /// Writes an integer register directly into a declaration-order struct
    /// field, avoiding `BoxI64` and a field-name lookup.
    FieldSetI64ByOffset {
        /// Register holding the struct value.
        receiver: Reg,
        /// Declaration-order field offset.
        offset: u16,
        /// Source integer register.
        value_i: Reg,
    },
    /// `receiver.push(value)` - in-place append. `Arc::make_mut`s the
    /// receiver register's backing storage (`Array` / `IntArray` /
    /// `FloatVec`) and pushes, retaining spare capacity for amortized
    /// O(1) growth. Emitted only for a bare-local Vec receiver in
    /// statement position, where the method's result is discarded.
    VecPush {
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// Register holding the value to append.
        value: Reg,
    },
    /// `place += rhs` for a `String` place - in-place append. Grows
    /// the receiver register's `String` via `Arc::make_mut` +
    /// `push_str`, retaining spare capacity for amortized O(1)
    /// growth. Emitted only when the place resolves to a local Value
    /// register (`s += x`, or `*out += x` for a `&mut String` local),
    /// where the lowered RHS is `place + rhs`. Replaces the
    /// concat-then-store path that copies the whole string per append.
    StrAppend {
        /// Register holding the String, mutated in place.
        receiver: Reg,
        /// Register holding the value to append.
        value: Reg,
    },
    /// `receiver.push(value)` for a local String. Mutates unique `SmolStr`
    /// storage directly and falls back to copy-on-write when shared.
    StrPush {
        /// Register holding the String, mutated in place.
        receiver: Reg,
        /// Register holding the character or byte value.
        value: Reg,
        /// Interpret the integer argument as a byte when true.
        byte: bool,
    },
    /// `__concat(prefix, integer)` fast path used by two-piece `format!`
    /// expansions. Builds the result in one allocation without a builtin call
    /// frame or boxed integer argument.
    StrConcatI64 {
        /// Destination string value register.
        dst: Reg,
        /// String prefix value register.
        prefix: Reg,
        /// Signed integer register to append in decimal form.
        value_i: Reg,
    },
    /// `dst = receiver.pop()` - in-place removal of the last element.
    /// `dst` receives `Some(last)` / `None`; the receiver register's
    /// backing storage shrinks in place, retaining capacity.
    VecPop {
        /// Destination register for the popped `Option`.
        dst: Reg,
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
    },
    /// `dst = receiver.insert(index, value)` - bounds-checked in-place insert.
    /// Mutates the receiver only on success and writes `Result<(), Error>`.
    VecInsert {
        /// Register receiving `Ok(())` or `Err(errors::Error)`.
        dst: Reg,
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// Register holding the insertion index.
        index: Reg,
        /// Register holding the value to insert.
        value: Reg,
    },
    /// `dst = receiver.swap(a, b)`. An index outside `[0, len)` is a bounds
    /// panic, matching an indexed write.
    VecSwap {
        /// Register receiving the unit result.
        dst: Reg,
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// First index.
        a: Reg,
        /// Second index.
        b: Reg,
    },
    /// `receiver.swap(a, b)` in statement position, where the unit result
    /// needs no register. Bounds are checked as in [`Op::VecSwap`].
    VecSwapDiscard {
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// First index.
        a: Reg,
        /// Second index.
        b: Reg,
    },
    /// `receiver.remove(index)` - in-place removal at `index`. Mutates
    /// the receiver register's backing storage in place and panics when
    /// `index` is outside `0..len`. Emitted only for a bare-local Vec
    /// receiver in statement position.
    VecRemove {
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// Register holding the index to remove.
        index: Reg,
    },
    /// `dst = Vec::remove(&mut receiver, index)` - qualified in-place
    /// removal. Writes `Result<T, Error>` into `dst`.
    VecRemoveAt {
        /// Register receiving the removed element.
        dst: Reg,
        /// Register holding the Vec, mutated in place.
        receiver: Reg,
        /// Register holding the index to remove.
        index: Reg,
    },
    /// `dst = receiver.N` - native tuple / positional-field
    /// read.
    TupleIndex {
        /// Destination register.
        dst: Reg,
        /// Register holding the tuple.
        receiver: Reg,
        /// Zero-based index.
        index: u32,
    },
    /// `receiver.N = value` - native tuple / positional-field
    /// write. Mutates the element vector in place (`Arc::make_mut`
    /// semantics).
    TupleSet {
        /// Register holding the tuple.
        receiver: Reg,
        /// Zero-based index.
        index: u32,
        /// Register holding the value to store.
        value: Reg,
    },
    /// `dst = tuple[len - offset_from_end - 1]` - tail-anchored
    /// element access for rest patterns like `(first, .., last)`.
    TupleTailIndex {
        /// Destination register.
        dst: Reg,
        /// Register holding the tuple.
        receiver: Reg,
        /// How many positions from the end (0 = last element).
        offset_from_end: u32,
    },
    /// `base[index].field_name = value` - fused in-place
    /// write. Avoids the `IndexGet` / `FieldSet` / `IndexSet`
    /// round-trip (and its O(n) Vec clones) that dominates
    /// hot loops iterating over arrays of structs
    /// (e.g. nbody's `bodies[i].vx = ...`). `base` must be a
    /// local register holding the array; since no other
    /// register holds the same Arc, `Arc::make_mut` hits the
    /// non-cloning path and the whole op becomes O(1).
    IndexedFieldSet {
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
        /// Register holding the value to store.
        value: Reg,
    },

    // ----- Phase 1: unboxed f64 register-file ops -----
    //
    // Operands named `*_f` live in the frame's float register
    // file (`Vec<f64>`); operands named `*_v` live in the
    // regular `Value` register file. All other Reg slots in
    // these ops refer to the indicated file - the compiler
    // keeps them straight.
    /// `floats[dst_f] = f64_consts[idx]`. Uses a dedicated
    /// f64 constant pool so the `Op` enum stays small (the
    /// largest variant drives enum size, which the dispatch
    /// loop copies per instruction).
    LoadConstF64 { dst_f: Reg, idx: ConstIdx },
    /// `floats[dst_f] = floats[lhs_f] + floats[rhs_f]`.
    AddF64 { dst_f: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `floats[dst_f] = floats[lhs_f] - floats[rhs_f]`.
    SubF64 { dst_f: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `floats[dst_f] = floats[lhs_f] * floats[rhs_f]`.
    MulF64 { dst_f: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `floats[dst_f] = floats[lhs_f] / floats[rhs_f]`.
    DivF64 { dst_f: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `floats[dst_f] = floats[lhs_f] / (ints[rhs_i] as f64)`.
    DivF64ByI64 { dst_f: Reg, lhs_f: Reg, rhs_i: Reg },
    /// `floats[dst_f] = -floats[src_f]`.
    NegF64 { dst_f: Reg, src_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] < floats[rhs_f])`.
    LtF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] <= floats[rhs_f])`.
    LeF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] > floats[rhs_f])`.
    GtF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] >= floats[rhs_f])`.
    GeF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] == floats[rhs_f])`.
    EqF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `registers[dst_v] = Bool(floats[lhs_f] != floats[rhs_f])`.
    NeF64 { dst_v: Reg, lhs_f: Reg, rhs_f: Reg },
    /// `floats[dst_f] = src_v.as_float()` - unbox an f64 out
    /// of a `Value::Float` for use with the typed ops. `peer_v` is present
    /// for a binary operation so a type mismatch can report both operands.
    UnboxF64 {
        dst_f: Reg,
        src_v: Reg,
        peer_v: Option<Reg>,
    },
    /// `registers[dst_v] = Value::Float(floats[src_f])` -
    /// re-box an f64 register for ABI-crossing use (calls,
    /// field stores, returns).
    BoxF64 { dst_v: Reg, src_f: Reg },
    /// `floats[dst_f] = sqrt(floats[src_f])` - inlined
    /// `math::sqrt` intrinsic.
    SqrtF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = sin(floats[src_f])`.
    SinF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = cos(floats[src_f])`.
    CosF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = floats[src_f].abs()`.
    AbsF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = floats[src_f].floor()`.
    FloorF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = floats[src_f].ceil()`.
    CeilF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = floats[src_f].exp()`.
    ExpF64 { dst_f: Reg, src_f: Reg },
    /// `floats[dst_f] = floats[src_f].ln()`.
    LnF64 { dst_f: Reg, src_f: Reg },
    /// Multiply-add: `floats[dst_f] = floats[a_f] *
    /// floats[b_f] + floats[c_f]`, rounded twice. Emitted when the
    /// compiler sees `a * b + c` (or `c + a * b`), which is extremely
    /// common in vector math (`x + dt * vx`). One dispatch instead of
    /// two; the arithmetic stays exactly what the source wrote, so the
    /// value matches the compiled tiers bit for bit.
    MulAddF64 {
        dst_f: Reg,
        a_f: Reg,
        b_f: Reg,
        c_f: Reg,
    },
    /// Multiply-subtract: `floats[dst_f] = floats[c_f] -
    /// floats[a_f] * floats[b_f]`, rounded twice like
    /// [`Op::MulAddF64`]. Matches the `vx - dx * mag` shape.
    MulSubF64 {
        dst_f: Reg,
        a_f: Reg,
        b_f: Reg,
        c_f: Reg,
    },

    // ----- Phase 1: unboxed i64 register-file ops -----
    /// `ints[dst_i] = i64_consts[idx]`.
    LoadConstI64 { dst_i: Reg, idx: ConstIdx },
    /// Wrapping `ints[dst_i] = ints[lhs_i] + ints[rhs_i]`.
    AddI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Debug-checked integer addition for the declared integer type.
    CheckedAddI64 {
        dst_i: Reg,
        lhs_i: Reg,
        rhs_i: Reg,
        overflow_ty: IntTy,
    },
    /// Wrapping `ints[dst_i] = ints[lhs_i] - ints[rhs_i]`.
    SubI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Debug-checked integer subtraction for the declared integer type.
    CheckedSubI64 {
        dst_i: Reg,
        lhs_i: Reg,
        rhs_i: Reg,
        overflow_ty: IntTy,
    },
    /// Wrapping `ints[dst_i] = ints[lhs_i] * ints[rhs_i]`.
    MulI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Debug-checked integer multiplication for the declared integer type.
    CheckedMulI64 {
        dst_i: Reg,
        lhs_i: Reg,
        rhs_i: Reg,
        overflow_ty: IntTy,
    },
    /// Checked `ints[dst_i] = ints[lhs_i] / ints[rhs_i]`.
    DivI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Checked `ints[dst_i] = ints[lhs_i] % ints[rhs_i]`.
    RemI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Checked unsigned `ints[dst_i] = (ints[lhs_i] as u64) / (ints[rhs_i] as u64)`.
    DivU64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Checked unsigned `ints[dst_i] = (ints[lhs_i] as u64) % (ints[rhs_i] as u64)`.
    RemU64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Wrapping `ints[dst_i] = -ints[src_i]`.
    NegI64 { dst_i: Reg, src_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] < ints[rhs_i])`.
    LtI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] <= ints[rhs_i])`.
    LeI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] > ints[rhs_i])`.
    GtI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] >= ints[rhs_i])`.
    GeI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] == ints[rhs_i])`.
    EqI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `registers[dst_v] = Bool(ints[lhs_i] != ints[rhs_i])`.
    NeI64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Unsigned `registers[dst_v] = Bool((ints[lhs_i] as u64) < (ints[rhs_i] as u64))`.
    LtU64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Unsigned `registers[dst_v] = Bool((ints[lhs_i] as u64) <= (ints[rhs_i] as u64))`.
    LeU64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Unsigned `registers[dst_v] = Bool((ints[lhs_i] as u64) > (ints[rhs_i] as u64))`.
    GtU64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Unsigned `registers[dst_v] = Bool((ints[lhs_i] as u64) >= (ints[rhs_i] as u64))`.
    GeU64 { dst_v: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Bitwise `ints[dst_i] = ints[lhs_i] & ints[rhs_i]`.
    BitAndI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Bitwise `ints[dst_i] = ints[lhs_i] | ints[rhs_i]`.
    BitOrI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Bitwise `ints[dst_i] = ints[lhs_i] ^ ints[rhs_i]`.
    BitXorI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Wrapping `ints[dst_i] = ints[lhs_i] << (ints[rhs_i] & 63)`.
    ShlI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Arithmetic `ints[dst_i] = ints[lhs_i] >> (ints[rhs_i] & 63)`
    /// (matches Rust's `i64 >> i64` semantics - sign-preserving).
    ShrI64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// Logical `ints[dst_i] = ((ints[lhs_i] as u64) >> (ints[rhs_i] & 63)) as i64`
    /// (matches Rust's `u64 >> u64` semantics - zero-filling). Used when
    /// the shifted operand's declared type is unsigned.
    ShrU64 { dst_i: Reg, lhs_i: Reg, rhs_i: Reg },
    /// `ints[dst_i] = src_v.as_int()`. `peer_v` is present for a binary
    /// operation so a type mismatch can report both operands.
    UnboxI64 {
        dst_i: Reg,
        src_v: Reg,
        peer_v: Option<Reg>,
    },
    /// `registers[dst_v] = Value::Int(ints[src_i])`.
    BoxI64 { dst_v: Reg, src_i: Reg },
    /// `floats[dst_f] = floats[src_f]` - float-file copy,
    /// used for `x = y` when both are in the float file.
    MoveF64 { dst_f: Reg, src_f: Reg },
    /// `ints[dst_i] = ints[src_i]`.
    MoveI64 { dst_i: Reg, src_i: Reg },

    // ----- Phase 2: fused / typed field access -----
    //
    // These opcodes let the compiler avoid the intermediate
    // `Value::Struct` clone that would otherwise happen between
    // `IndexGet` and `FieldGet`. The receiver's aggregate is
    // walked by-reference and only the scalar field value is
    // cloned or unboxed.
    /// `floats[dst_f] = receiver.field_name` for a
    /// `Value::Struct` whose named field is a `Value::Float`.
    /// Skips the intermediate `Value::Float` → `UnboxF64`
    /// round-trip that would otherwise happen between
    /// `FieldGet` and a typed arithmetic consumer.
    FieldGetF64 {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the struct value.
        receiver: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
    },
    /// Typed integer counterpart of [`Self::FieldGetF64`]. Reads an `i64`
    /// struct field directly into the integer register file.
    FieldGetI64 {
        /// Destination integer register.
        dst_i: Reg,
        /// Register holding the struct value.
        receiver: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
    },
    /// Builds a two-field integer struct directly from typed registers. This
    /// bypasses the synthetic `__struct` builtin, its call-argument buffer,
    /// and boxing of the two scalar operands.
    Struct2I64 {
        /// Destination boxed aggregate register.
        dst: Reg,
        /// Struct type name in [`FnChunk::shape_names`].
        type_name: ConstIdx,
        /// First field name in [`FnChunk::shape_names`].
        field0: ConstIdx,
        /// Second field name in [`FnChunk::shape_names`].
        field1: ConstIdx,
        /// First source integer register.
        first_i: Reg,
        /// Second source integer register.
        second_i: Reg,
    },
    /// `dst = base[index].field_name` - fused indexed field
    /// read. Avoids cloning the inner struct `Arc` that a
    /// separate `IndexGet` + `FieldGet` would produce; reads
    /// the field directly from the array slot by reference.
    IndexedFieldGet {
        /// Destination register.
        dst: Reg,
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
    },
    /// `floats[dst_f] = base[index].field_name` - fused
    /// typed indexed field read. Same `Arc`-clone savings as
    /// `IndexedFieldGet` plus the `Value::Float` unbox into
    /// the float register file happens in one step. This is
    /// nbody's hot-loop primitive.
    IndexedFieldGetF64 {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
    },
    /// `base[index].field_name = floats[value_f]` - fused
    /// typed indexed field write. Counterpart to
    /// `IndexedFieldGetF64`.
    IndexedFieldSetF64 {
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Const-pool index of the field-name string.
        name_idx: ConstIdx,
        /// Source float register.
        value_f: Reg,
    },

    // ----- Phase 2: offset-resolved typed field ops -----
    //
    // The VM compiler emits these when the receiver's struct
    // type is known at compile time. `__struct` lays out
    // every matching literal in declaration order, so a
    // compile-time `offset` is guaranteed correct and the
    // runtime scan over field names goes away.
    /// `floats[dst_f] = base[index].<struct field at offset>`.
    IndexedFieldGetF64ByOffset {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Declaration-order offset into the struct's
        /// field vec.
        offset: u16,
    },
    /// `base[index].<struct field at offset> = floats[value_f]`.
    IndexedFieldSetF64ByOffset {
        /// Register holding the base array.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Declaration-order offset.
        offset: u16,
        /// Source float register.
        value_f: Reg,
    },
    /// Fused compare-and-branch ops. Halve the dispatch
    /// overhead on the common `while i < n { ... }` shape by
    /// combining the compare with the conditional jump into a
    /// single opcode - saves ~one match + one register write
    /// per loop iteration.
    ///
    /// Branch to `target` when `ints[lhs_i] < ints[rhs_i]`.
    BranchIfLtI64 {
        lhs_i: Reg,
        rhs_i: Reg,
        target: InstrIdx,
    },
    /// Branch to `target` when `ints[lhs_i] >= ints[rhs_i]`.
    BranchIfGeI64 {
        lhs_i: Reg,
        rhs_i: Reg,
        target: InstrIdx,
    },
    /// Branch to `target` when `ints[lhs_i] > ints[rhs_i]`.
    /// Used by the inclusive-range for-loop fast path: `for i in a..=b`
    /// exits when `i > b`.
    BranchIfGtI64 {
        lhs_i: Reg,
        rhs_i: Reg,
        target: InstrIdx,
    },
    /// Branch to `target` when `floats[lhs_f] < floats[rhs_f]`.
    BranchIfLtF64 {
        lhs_f: Reg,
        rhs_f: Reg,
        target: InstrIdx,
    },
    /// Branch to `target` when `floats[lhs_f] >= floats[rhs_f]`.
    BranchIfGeF64 {
        lhs_f: Reg,
        rhs_f: Reg,
        target: InstrIdx,
    },
    /// Fused increment + back-edge for the bottom of a `for i in
    /// a..b { ... }` loop. Computes `ints[counter_i] += 1` then
    /// branches to `target` when the post-increment counter is
    /// `< ints[end_i]`. Saves the two-op `AddI64` + `Jump` + the
    /// header `BranchIfGeI64` re-check that the pre-increment
    /// shape pays per iteration. The for-range emitter falls
    /// through past the fused op into the loop's `exit` block when
    /// the comparison fails. Tier B5 of the bytecode push.
    IncJumpIfLtI64 {
        counter_i: Reg,
        end_i: Reg,
        target: InstrIdx,
    },
    /// Same as [`Op::IncJumpIfLtI64`] but uses `<=` for the
    /// inclusive-range form (`for i in a..=b`).
    IncJumpIfLeI64 {
        counter_i: Reg,
        end_i: Reg,
        target: InstrIdx,
    },

    /// Typed i64 arithmetic with an inline immediate right-hand
    /// operand, fusing the `LoadConstI64` + arith pair a constant-
    /// operand expression (`i % 7`, `n + 1`) would otherwise pay as
    /// two dispatches. Wrapping semantics, identical to the two-op
    /// form. `Div` / `Rem` immediates are never zero: the compiler
    /// keeps the two-op form for a zero literal so the runtime's
    /// divide-by-zero panic path stays shared.
    ArithImmI64 {
        kind: ImmArithKind,
        dst_i: Reg,
        lhs_i: Reg,
        imm: i32,
    },

    /// `floats[dst_f] = receiver.<struct field at offset>`.
    FieldGetF64ByOffset {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the struct value.
        receiver: Reg,
        /// Declaration-order offset.
        offset: u16,
    },
    /// Compile-time-offset integer field read, avoiding both the name lookup
    /// and the boxed `Value::Int` intermediate.
    FieldGetI64ByOffset {
        /// Destination integer register.
        dst_i: Reg,
        /// Register holding the struct value.
        receiver: Reg,
        /// Declaration-order field offset.
        offset: u16,
    },

    /// FloatArray-only fused read, statically proven. Skips
    /// the `Value::FloatArray` discriminant check since the
    /// compiler proved `base` holds a flat aggregate via a
    /// preceding `BuildFloatArray`. Drops ~1 branch + one
    /// enum match per iteration on the nbody-shape hot loop.
    FlatGetF64 {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the `Value::FloatArray`.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Element stride (f64s per element).
        stride: u16,
        /// Field offset within an element.
        offset: u16,
    },
    /// FloatArray-only fused write, statically proven.
    FlatSetF64 {
        /// Register holding the `Value::FloatArray`.
        base: Reg,
        /// Register holding the index value.
        index: Reg,
        /// Element stride (f64s per element).
        stride: u16,
        /// Field offset within an element.
        offset: u16,
        /// Source float register.
        value_f: Reg,
    },
    /// Like `FlatGetF64` but the element index is read straight
    /// from the int register file, skipping the per-access
    /// `BoxI64` a `Value`-register index would need. Emitted when
    /// the index expression compiles to an `i64` register - the
    /// common loop-counter case (`bodies[a].x`).
    FlatGetF64I {
        /// Destination float register.
        dst_f: Reg,
        /// Register holding the `Value::FloatArray`.
        base: Reg,
        /// Int register holding the element index.
        index_i: Reg,
        /// Element stride (f64s per element).
        stride: u16,
        /// Field offset within an element.
        offset: u16,
    },
    /// Like `FlatSetF64` but with an int-register index. See
    /// [`Op::FlatGetF64I`].
    FlatSetF64I {
        /// Register holding the `Value::FloatArray`.
        base: Reg,
        /// Int register holding the element index.
        index_i: Reg,
        /// Element stride (f64s per element).
        stride: u16,
        /// Field offset within an element.
        offset: u16,
        /// Source float register.
        value_f: Reg,
    },
    // BuildFloatArray (assembles `Value::FloatArray` from a
    // contiguous block of float registers for `[S; N]` literals
    // where `S` has all-f64 fields) lives in the `wide_ops`
    // side-table - see `Op::Wide` and `WideOp::BuildFloatArray`.
    /// `registers[dst] = registers[src]` with the integers a descriptor names
    /// re-boxed as `Value::Uint`, so a `u64` at or above `i64::MAX` renders as
    /// its own decimal. `desc_idx` is a const-pool index holding the
    /// descriptor string (see `value::uint_desc`), which the compiler builds
    /// from the rendered argument's declared type. Only the rendered copy is
    /// converted; the source value keeps its own representation.
    UintLeaves {
        /// Destination Value register.
        dst: Reg,
        /// Source Value register.
        src: Reg,
        /// Const-pool index of the descriptor string.
        desc_idx: ConstIdx,
    },
    /// `registers[dst_v] = Value::Uint(ints[src_i] as u64)`.
    /// Produces an unsigned 64-bit display value for `x as u64` / `x as usize`.
    I64ToUint {
        /// Destination Value register.
        dst_v: Reg,
        /// Source i64 register.
        src_i: Reg,
    },
    /// `registers[dst] = cast_scalar(registers[src], target)`.
    /// Whitelisted scalar cast over Value registers - the combos the
    /// typed-register cast ops don't reach (f32 / bool / char sources,
    /// `char` and `f32` targets). Keeps every GT0005-whitelisted cast
    /// native.
    CastScalar {
        /// Destination Value register.
        dst: Reg,
        /// Source Value register.
        src: Reg,
        /// Resolved cast destination shape.
        target: crate::cast::CastTarget,
    },
    /// `registers[dst] = MutCell(registers[src])` - wraps a
    /// `&mut Vec<T>` / `&mut [T]` call argument in a shared
    /// write-back cell. The callee unwraps it at frame entry and
    /// publishes the final parameter value back on return, giving
    /// the caller write-through semantics under the VM's
    /// clone-on-write value model.
    CellNew {
        /// Destination Value register (holds the cell during the call).
        dst: Reg,
        /// Register holding the aggregate to share.
        src: Reg,
    },
    /// `registers[dst] = MutCell(take(registers[src]))` - like
    /// [`Op::CellNew`] but *moves* the source register's value into the
    /// cell, leaving it `Unit`. Used for a `&mut self` method receiver
    /// rooted at a local: the receiver is republished into the same place
    /// by the matching [`Op::CellTake`] on return, and moving (rather than
    /// cloning) keeps its refcount at one so the callee's first field
    /// write mutates in place instead of forcing a copy-on-write clone.
    /// The emitter evaluates every call argument before this op, so an
    /// argument that reads the receiver still sees its live value.
    CellNewMove {
        /// Destination Value register (holds the cell during the call).
        dst: Reg,
        /// Register whose value is moved into the cell (left `Unit`).
        src: Reg,
    },
    /// `registers[dst] = cell.inner` - reads a write-back cell's
    /// final value into the argument's home register after the
    /// call returns.
    CellTake {
        /// Destination Value register (the argument's home register).
        dst: Reg,
        /// Register holding the `MutCell` created by [`Op::CellNew`].
        cell: Reg,
    },
    /// `registers[dst] = CaptureCell(take(registers[src]))` - installs a
    /// fresh capture cell as a local's storage. Emitted where the
    /// binding receives a whole new value (its `let`, a parameter that a
    /// closure captures, a reassignment), so the identity a previously
    /// created closure holds is left untouched.
    CaptureCellNew {
        /// Destination Value register (the binding's cell register).
        dst: Reg,
        /// Register whose value is moved into the cell (left `Unit`).
        src: Reg,
    },
    /// `registers[dst] = cell.inner.clone()` - loads a capture cell's
    /// value into the binding's home register for an instruction that
    /// only reads it.
    CaptureCellGet {
        /// Destination Value register (the binding's home register).
        dst: Reg,
        /// Register holding the `CaptureCell`.
        cell: Reg,
    },
    /// `registers[dst] = take(cell.inner)` - moves a capture cell's
    /// value into the binding's home register for the duration of one
    /// instruction that may mutate it. Moving keeps the aggregate's
    /// refcount at one so the mutation happens in place; the matching
    /// [`Op::CaptureCellSet`] returns the value on the next instruction.
    CaptureCellTake {
        /// Destination Value register (the binding's home register).
        dst: Reg,
        /// Register holding the `CaptureCell`.
        cell: Reg,
    },
    /// `cell.inner = take(registers[src])` - returns the home register's
    /// value to the capture cell, publishing the instruction's mutation
    /// to every closure that captured the binding.
    CaptureCellSet {
        /// Register holding the `CaptureCell`.
        cell: Reg,
        /// Register whose value is moved into the cell.
        src: Reg,
    },
    /// `dst = (src is Value::Variant with name == consts[name_idx]
    /// and field count == arity)`. Drives native `match` arm tests
    /// on enum / tuple-struct patterns. The name is interned the
    /// same way the runtime interns variant names, so the equality
    /// check is a pointer compare in the steady state.
    VariantIs {
        /// Destination bool register.
        dst: Reg,
        /// Register holding the scrutinee value.
        src: Reg,
        /// `ConstIdx` of the expected variant name (a `Value::String`).
        name_idx: ConstIdx,
        /// Expected positional field count.
        arity: u16,
    },
    /// `dst = src.fields[idx]` for a `Value::Variant`. Extracts a
    /// positional payload field so a native `match` arm can bind
    /// sub-patterns. Assumes the preceding `VariantIs` already
    /// proved the shape; out-of-range / non-variant yields
    /// `Value::Unit` rather than trapping (the test gates the
    /// extract).
    VariantField {
        /// Destination value register.
        dst: Reg,
        /// Register holding the `Value::Variant`.
        src: Reg,
        /// Positional field index.
        idx: u16,
    },
    /// `receiver.fields[idx] = registers[src]` on a `Value::Variant`.
    ///
    /// A payload matched through a mutable place is named in place, but the
    /// interpreter's values are copy-on-write, so a mutation lands on the
    /// binding's own value. This publishes it back into the variant it came
    /// from, which is what makes `match self { V(x) => x.set(..) }` reach the
    /// enum a `&mut self` method was called on.
    VariantFieldSet {
        /// Register holding the `Value::Variant`.
        receiver: Reg,
        /// Positional field index.
        idx: u16,
        /// Register holding the replacement payload.
        src: Reg,
    },
    /// `dst = (src is Value::Struct named consts[name_idx])`. Drives
    /// native `match` arm tests on struct patterns.
    StructIs {
        /// Destination bool register.
        dst: Reg,
        /// Register holding the scrutinee value.
        src: Reg,
        /// `ConstIdx` of the expected struct name (a `Value::String`).
        name_idx: ConstIdx,
    },
    /// `dst = take(src)` - moves `src`'s value into `dst`, leaving
    /// `Value::Void` behind. Emitted in place of [`Op::Move`] when the
    /// source is a single-segment path to a local the consumability
    /// analysis proved is read exactly once at this point (see
    /// `compile::consume`), so handing the aggregate over instead of
    /// cloning frees the input as it is consumed. The emptied slot is
    /// never read again, so the move is unobservable.
    MoveConsume {
        /// Destination register.
        dst: Reg,
        /// Source register, emptied to `Value::Void`.
        src: Reg,
    },
    /// `dst = src.deep_clone()` for a `Map` / `IntMap` / `StrIntMap`
    /// value - emitted in place of [`Op::Move`] at a `let` binding or
    /// by-value call argument whose source is a bare path to a `Map` or
    /// `Set` local. Those variants hold their entries behind
    /// `Arc<Mutex<_>>`, so a plain [`Op::Move`] (an `Arc` clone) would
    /// alias the same backing table between the binding and its source -
    /// unlike `Vec`, whose `Arc<Vec<Value>>` has no interior mutability
    /// and gets independent storage for free from `Arc::make_mut` at the
    /// next mutating op. Any other `Value` kind reaching this op behaves
    /// like `Op::Move` (a cheap `Arc`/scalar clone), so a conservative
    /// compile-time gate is safe even if it over-applies.
    CloneMapLike {
        /// Destination register.
        dst: Reg,
        /// Source register, left unchanged.
        src: Reg,
    },
    /// Like [`Op::VariantField`] but drains the payload out of a
    /// uniquely-owned scrutinee. When `Arc::get_mut` on the `src`
    /// `Value::Variant` succeeds the field is moved into `dst`
    /// (leaving `Value::Void`); a shared variant (refcount > 1) clones
    /// exactly like `VariantField`. Emitted only for a guard-free
    /// `match` whose scrutinee is a consumable local, so the drained
    /// scrutinee is never matched against again.
    VariantFieldConsume {
        /// Destination value register.
        dst: Reg,
        /// Register holding the `Value::Variant`.
        src: Reg,
        /// Positional field index.
        idx: u16,
    },
    /// Like [`Op::IndexGet`] but drains a uniquely-owned `Array` /
    /// `Tuple` element. When `Arc::get_mut` on the base succeeds the
    /// element at `index` is moved into `dst` (leaving `Value::Void`);
    /// a shared aggregate, a non-`Array`/`Tuple` base, or an
    /// out-of-range index behaves exactly like `IndexGet` (clone /
    /// lenient zero). Emitted for a for-loop whose source collection is
    /// a consumable local, draining the input as the loop advances.
    IndexGetConsume {
        /// Destination register.
        dst: Reg,
        /// Register holding the base (`Array` / `Tuple`).
        base: Reg,
        /// Register holding the index value.
        index: Reg,
    },
    /// Like [`Op::TupleIndex`] but drains a uniquely-owned tuple field.
    /// When `Arc::get_mut` on the `receiver` `Value::Tuple` succeeds the
    /// field is moved into `dst` (leaving `Value::Void`); a shared tuple
    /// clones exactly like `TupleIndex`. Emitted when destructuring a
    /// for-loop element that was itself drained from a consumable
    /// source, so the emptied tuple is never read again.
    TupleIndexConsume {
        /// Destination register.
        dst: Reg,
        /// Register holding the tuple.
        receiver: Reg,
        /// Zero-based index.
        index: u32,
    },
}

/// Resolved builtin call pointer cached in [`CacheSlot::builtin_fn`].
/// Same shape as the value the [`Value::Builtin`] variant carries
/// internally; pulled out into a type alias because clippy's
/// `very_complex_type` lint flags the inlined form on the slot.
pub(crate) type BuiltinFnPtr =
    fn(&[crate::value::Value]) -> crate::value::RuntimeResult<crate::value::Value>;

/// One inline-cache slot, one per dispatch-shaped opcode
/// (`Op::Call` / `Op::MethodCall`).
///
/// Hit when the slot's `type_token` matches the receiver / callee's
/// current token; the cached `Global` is used directly, skipping
/// the qualified-key build + `HashMap::get` chain. Miss falls
/// through to the slow path which writes back into the slot.
///
/// `type_token == 0` is the empty sentinel: a fresh chunk starts
/// with all slots zero-initialised, and the dispatch path treats
/// "non-cacheable receiver" (primitives, etc.) the same way by
/// returning a zero token.
///
/// The optional `SmolStr` keeps named calls exact without a global interner;
/// it adds one word only to the per-VM cache, not to bytecode or `Value`.
/// Pre-D8 the `resolved`
/// field stored a full `Option<Global>` (~24 B by itself) for
/// 40 B total per slot; we now cache only the resolved
/// `Arc<FnChunk>` (the dominant hit shape) and let closures /
/// `Value::Native` / `Value::Variant` callees take the slow
/// path on every call.
#[derive(Debug, Clone, Default)]
pub(crate) struct CacheSlot {
    /// Stable identity for the receiver / callee the slot last
    /// resolved against. `0` means empty / non-cacheable.
    pub type_token: u64,
    /// Exact named-callee spelling for [`Op::Call`]. Method-call slots leave
    /// this empty and key solely on the receiver type token. Keeping this
    /// value in the per-`Vm` cache makes dynamically-created callable names
    /// reclaimable at VM teardown instead of leaking them into a thread-local
    /// `&'static str` interner.
    pub callee_name: Option<crate::value::SmolStr>,
    /// Snapshot of the owning `Vm`'s `globals_generation` when the
    /// slot was populated. The dispatch arm compares this against
    /// the live counter on every hit; a mismatch (i.e. globals
    /// were reassigned since this slot was filled) demotes the
    /// hit to a miss and forces a fresh resolution. `0` is the
    /// empty-slot sentinel and never matches a live counter
    /// (which starts at 1).
    pub generation: u32,
    /// Fast path: when the resolved dispatch target is a
    /// `Value::Builtin`, we cache its raw `call` fn pointer
    /// here so the hit path is a single indirect call, no
    /// `match Global::Value(Value::Builtin { .. })` chain. This
    /// is the steady state for the vast majority of method
    /// dispatches in the bench programs and the wider stdlib.
    /// Mirrors `CPython` 3.11's `LOAD_METHOD_NO_DICT` specialised
    /// opcode storing the resolved `__call__` directly.
    pub builtin_fn: Option<BuiltinFnPtr>,
    /// General path: when the resolved target is a Gossamer
    /// function (`Global::Fn(Arc<FnChunk>)`) - i.e. user code
    /// or stdlib body, not a builtin - its chunk is cached
    /// here. `None` covers both the empty-slot state and any
    /// resolved-but-uncached shape (closures / native / value).
    pub fn_chunk: Option<std::sync::Arc<FnChunk>>,
}

/// Side-table-backed payload for [`Op::Wide`]. Members carry the
/// payload of the rare 6-field ops (`MapIncAt`, `BuildFloatArray`)
/// so the in-line `Op` enum can stay narrow on the hot path.
#[derive(Debug, Clone)]
pub enum WideOp {
    /// `__concat(prefix, __fmt_pad(__concat(integer), width, fill, align))`.
    /// Renders, pads, and concatenates in one allocation.
    StrConcatPadI64 {
        /// Destination string value register.
        dst: Reg,
        /// String prefix value register.
        prefix: Reg,
        /// Signed integer register to render.
        value: Reg,
        /// Width value register.
        width: Reg,
        /// Fill character value register.
        fill: Reg,
        /// Alignment value register.
        align: Reg,
    },
    /// `m.inc_at(seq, start, len, by)` - see the original
    /// `Op::MapIncAt` doc; moved to the side table because the
    /// 6-register payload bloated every `Op` slot.
    MapIncAt {
        /// Destination register (post-increment value, `Value::Int`).
        dst: Reg,
        /// Register holding the Map (`Value::Map`).
        map_reg: Reg,
        /// Register holding the seq String (`Value::String`).
        seq_reg: Reg,
        /// Register holding the slice start offset (`Value::Int`).
        start_reg: Reg,
        /// Register holding the slice length (`Value::Int`).
        len_reg: Reg,
        /// Register holding the increment (`Value::Int`).
        by_reg: Reg,
    },
    /// Builds a `Value::FloatArray` from `stride * elem_count`
    /// consecutive `f64` registers starting at `first_f`. Same
    /// shape as the original `Op::BuildFloatArray`; moved here
    /// because the 6-field payload was the other op driving the
    /// in-line `Op` enum to its widest case.
    BuildFloatArray {
        /// Destination value register.
        dst_v: Reg,
        /// Const-pool index of a `Value::String` holding the
        /// element struct's name.
        name_idx: ConstIdx,
        /// Const-pool index of a `Value::Array<Value::String>`
        /// holding the field names in declaration order.
        fields_idx: ConstIdx,
        /// Number of `f64` fields per element.
        stride: u16,
        /// Number of struct elements.
        elem_count: u16,
        /// First float register of the flat data block.
        first_f: Reg,
    },
    /// Builds a packed `Value::FloatArray` from consecutive value
    /// registers containing instances of the same all-`f64` struct.
    /// This preserves the packed representation when array elements
    /// come from calls or other expressions rather than direct literals.
    BuildFloatArrayFromStructs {
        /// Destination value register.
        dst_v: Reg,
        /// First value register containing a struct element.
        first_v: Reg,
        /// Number of struct elements.
        elem_count: u16,
        /// Const-pool index of the element struct name.
        name_idx: ConstIdx,
        /// Const-pool index of field names in declaration order.
        fields_idx: ConstIdx,
    },
}

/// One closure literal's compile-time template, referenced by
/// [`Op::MakeClosure`] through [`FnChunk::closure_protos`].
///
/// `MakeClosure` snapshots the enclosing frame's [`Self::capture_regs`]
/// into a [`crate::value::Closure`]: the VM runs [`Self::chunk`], whose
/// leading parameters are the captured upvalues followed by the
/// closure's declared parameters.
#[derive(Debug)]
pub struct ClosureProto {
    /// Native body chunk run by the VM.
    pub chunk: Arc<FnChunk>,
    /// Enclosing-frame registers to snapshot as upvalues, in capture order.
    pub capture_regs: Vec<Reg>,
}

/// Which arithmetic operation an [`Op::ArithImmI64`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImmArithKind {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Operation an [`Op::Select`] arm performs, recorded in
/// [`SelectArmMeta`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectArmKind {
    /// `pat = chan.recv()` - receive, binding the value before the body.
    Recv,
    /// `chan.send(value)` - send on the channel, then run the body.
    Send,
    /// `default` - chosen when no other arm is ready.
    Default,
}

/// One arm of a native `select { … }`, referenced by [`Op::Select`]
/// through [`FnChunk::select_arms`]. The operand registers are
/// evaluated before the `Op::Select` runs; the handler reads the
/// channel/value out of them, writes any received value into
/// `bind_reg`, and jumps to `body_block`.
#[derive(Debug, Clone, Copy)]
pub struct SelectArmMeta {
    /// Which operation this arm performs.
    pub kind: SelectArmKind,
    /// Register holding the channel value (`Recv` / `Send` arms;
    /// unused and `0` for `Default`).
    pub channel_reg: Reg,
    /// Register holding the value to send (`Send` arms; `0` otherwise).
    pub value_reg: Reg,
    /// Register the received value is written into before the body
    /// block destructures it (`Recv` arms; `0` otherwise).
    pub bind_reg: Reg,
    /// Instruction index of this arm's body basic block.
    pub body_block: InstrIdx,
}

/// Source position associated with a bytecode instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    /// Display name of the source file.
    pub file: &'static str,
    /// One-based source line.
    pub line: u32,
    /// One-based source column.
    pub column: u32,
}

/// A change point in a chunk's source-location table.
///
/// Entries are sorted by instruction index. A location applies until the next
/// entry, avoiding a full-width source position beside every bytecode op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstructionLocation {
    /// First instruction carrying `location`.
    pub instruction: InstrIdx,
    /// Source position for this run of instructions.
    pub location: Option<SourceLocation>,
}

/// Compiled function - the unit of bytecode the VM can call.
#[derive(Debug, Default)]
pub struct FnChunk {
    /// Source-level name (useful in diagnostics). Interned into the
    /// process-global pool at construction so recursive call stacks
    /// don't allocate a heap String per frame.
    pub name: &'static str,
    /// Number of parameters the function takes.
    pub arity: u16,
    /// Total Value register file size reserved per call.
    pub register_count: u16,
    /// Unboxed `f64` register file size - Phase 1.
    pub float_count: u16,
    /// Unboxed `i64` register file size - Phase 1.
    pub int_count: u16,
    /// Linear instruction stream.
    pub instrs: Vec<Op>,
    /// Run-length encoded source positions for [`Self::instrs`].
    pub instruction_locations: Vec<InstructionLocation>,
    /// Side-table for op payloads that don't fit in the in-line
    /// `Op` variant width without forcing every other op to
    /// pay the worst-case slot. Indexed by `Op::Wide(idx)`. The
    /// dispatch loop takes one extra deref through this Vec for
    /// the rare wide ops, in exchange for keeping the per-op
    /// memcpy on the hot path narrow.
    pub wide_ops: Vec<WideOp>,
    /// Interned constants referenced by `LoadConst`.
    pub consts: Vec<Value>,
    /// Raw `f64` constants referenced by `LoadConstF64`. Kept
    /// separate from `consts` so the `Op` enum can stay narrow
    /// (the dispatch loop copies each op per instruction).
    pub f64_consts: Vec<f64>,
    /// Raw `i64` constants referenced by `LoadConstI64`.
    pub i64_consts: Vec<i64>,
    /// Global names referenced by `LoadGlobal`.
    pub globals: Vec<Box<str>>,
    /// INTERNED variant/struct names referenced by `VariantIs` /
    /// `StructIs` (`name_idx` indexes here, not `consts`). Both sides
    /// of the shape test come from `intern_type_name`, so the run
    /// loop compares one pointer instead of string content.
    pub shape_names: Vec<&'static str>,
    /// Number of inline-cache slots this chunk needs (`Op::Call`
    /// / `Op::MethodCall` sites). The actual `Vec<CacheSlot>` lives
    /// per-`Vm` inside `crate::vm::ChunkState`, not on the chunk -
    /// goroutines spawned from a parent VM each get their own
    /// `ChunkState` so cache writes don't bounce cache lines across
    /// CPUs. `FnChunk` stays purely-immutable and `Sync`.
    pub call_cache_count: u16,
    /// Number of adaptive-arith cache slots this chunk needs
    /// (`Op::AddInt` / `Op::SubInt` / etc. sites). Same per-`Vm`
    /// ownership story as [`Self::call_cache_count`].
    pub arith_cache_count: u16,
    /// Number of field-access cache slots this chunk needs
    /// (`Op::FieldGet` sites). PEP 659-style per-instruction
    /// inline caching for struct field reads.
    pub field_cache_count: u16,
    /// Parameter registers declared as `&mut Vec<T>` / `&mut [T]`.
    /// When a caller passes a [`Value::MutCell`](crate::value::Value)
    /// for one of these, the frame unwraps it at entry and publishes
    /// the final register value back into the cell on every return
    /// path (write-through `&mut` parameter semantics). Empty for
    /// the vast majority of chunks, so the entry probe is one
    /// `is_empty` test.
    pub mut_ref_params: Vec<Reg>,
    /// Plain integer parameters copied from their ABI `Value` slot into the
    /// typed integer register file when a frame starts. Each pair is
    /// `(value_parameter_index, integer_register)`.
    pub i64_params: Vec<(Reg, Reg)>,
    /// Closure templates referenced by [`Op::MakeClosure`]. Empty for
    /// chunks that build no closures.
    pub closure_protos: Vec<ClosureProto>,
    /// `select` arm metadata referenced by [`Op::Select`] as
    /// contiguous `[first .. first + count]` ranges. Empty for chunks
    /// containing no `select`.
    pub select_arms: Vec<SelectArmMeta>,
}

/// One adaptive-arith inline-cache slot. Tier C2 of the interp
/// wow plan - held inside [`crate::vm::ChunkState`].
#[derive(Debug, Default)]
pub(crate) struct ArithCacheSlot {
    /// Observed operand shape, encoded as one of the `ARITH_*`
    /// constants. `Cell<u8>` because the slot lives in per-`Vm`
    /// state; only the owning thread mutates it.
    pub(crate) shape: std::cell::Cell<u8>,
}

/// PEP 659-style inline cache for `Op::FieldGet`. Records the
/// last observed struct-name pointer + the offset its fields
/// list resolved to. On hit, the dispatcher reads the field by
/// offset directly; on miss, it refills the slot.
#[derive(Debug, Default)]
pub(crate) struct FieldCacheSlot {
    /// Stable interned-name pointer of the receiver struct
    /// (`intern_type_name(name).as_ptr() as u64`). `0` means
    /// empty / non-cacheable receiver.
    pub(crate) type_token: std::cell::Cell<u64>,
    /// Offset of the named field within the struct's fields
    /// vector, valid only when `type_token != 0`.
    pub(crate) offset: std::cell::Cell<u16>,
}

/// Sentinel for an arith cache slot that has not yet observed an
/// operand pair. Forces the dispatcher into the slow observe-and-
/// specialise path on the first call from the site.
pub(crate) const ARITH_UNKNOWN: u8 = 0;
/// Slot specialised on `(Value::Int, Value::Int)`. The dispatcher
/// reads the integers directly and emits a wrapping op without a
/// discriminant match.
pub(crate) const ARITH_INT_INT: u8 = 1;
/// Slot specialised on `(Value::Float, Value::Float)`.
pub(crate) const ARITH_FLOAT_FLOAT: u8 = 2;
/// Slot specialised on `(Value::String, Value::String)` - only
/// reached for `Op::AddInt` (string concatenation). The other
/// arith ops never set this shape; their observers degrade to
/// polymorphic when they see strings.
pub(crate) const ARITH_STRING_STRING: u8 = 3;
/// Slot has seen multiple incompatible shapes (e.g. an
/// `(Int, Float)` after specialising on `(Int, Int)`). Future
/// dispatches go straight through the generic helper without
/// trying to re-specialise.
pub(crate) const ARITH_POLYMORPHIC: u8 = 255;

/// Baseline call-entry budget before a typical (~50-instr) chunk
/// trips the deferred JIT compile. Used by [`hot_threshold_for`]
/// as the reference point; bigger chunks scale linearly to a
/// lower budget so a 5000-instr function tiers up after the
/// floor [`HOT_THRESHOLD_FLOOR`] entries instead of waiting for
/// 100 full calls of its expensive body.
pub(crate) const HOT_THRESHOLD_BASE: i32 = 100;

/// Lower bound for the dynamic threshold. A 1-instr stub tracks
/// against this floor; without it, the formula would push tier-up
/// to 1 entry on tiny stubs and waste compile time on bodies the
/// JIT can't meaningfully outrun.
pub(crate) const HOT_THRESHOLD_FLOOR: i32 = 16;

/// Sentinel that the `hot_counter` is initialised to when the JIT
/// is permanently disabled at chunk construction time. The Cell
/// can never realistically be decremented past `i32::MIN + 1`, so
/// using `i32::MAX` as a "never trips" marker is safe.
pub(crate) const HOT_DISABLED: i32 = i32::MAX;

/// Minimum observed bytecode work before a hot counter may spend the
/// fixed Cranelift compile tax. The unit is approximately
/// `instr_count * function_entries`; it is intentionally a work floor,
/// not a benchmark-name special case.
pub(crate) const JIT_MIN_OBSERVED_WORK: u64 = 8192;

/// Computes the per-chunk hot-counter initial value. Big chunks
/// tier up sooner because each apply runs more bytecode; the
/// `(BASE * 50) / max(50, instr_count)` form keeps a 50-instr
/// chunk on the legacy threshold (100) and shrinks linearly from
/// there, clamped at [`HOT_THRESHOLD_FLOOR`].
///
/// `GOSSAMER_JIT_THRESHOLD`, if set to a parseable positive `i32`,
/// overrides the formula entirely (per-chunk). Useful for
/// reproducing pre-scaling behaviour or aggressive tuning. Read
/// once on first lookup and cached for subsequent chunks.
#[must_use]
pub(crate) fn hot_threshold_for(instr_count: usize) -> i32 {
    if let Some(override_val) = jit_threshold_override() {
        return override_val;
    }
    let denom = instr_count.max(50) as i32;
    let scaled = (HOT_THRESHOLD_BASE.saturating_mul(50)) / denom;
    scaled.max(HOT_THRESHOLD_FLOOR)
}

#[must_use]
pub(crate) fn jit_min_work_for(_instr_count: usize) -> u64 {
    if let Some(override_val) = jit_min_work_override() {
        return override_val;
    }
    JIT_MIN_OBSERVED_WORK
}

fn jit_threshold_override() -> Option<i32> {
    use std::sync::OnceLock;
    static OVERRIDE: OnceLock<Option<i32>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("GOSSAMER_JIT_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|n| *n > 0)
    })
}

fn jit_min_work_override() -> Option<u64> {
    use std::sync::OnceLock;
    static OVERRIDE: OnceLock<Option<u64>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("GOSSAMER_JIT_MIN_WORK")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
    })
}

impl FnChunk {
    /// Releases excess Vec capacity after compilation is complete.
    ///
    /// `compile_fn` grows the Vec fields incrementally; this trims
    /// each one to its occupied length so the chunk holds no wasted
    /// allocation beyond what the bytecode actually requires.
    pub fn compact(&mut self) {
        self.instrs.shrink_to_fit();
        self.instruction_locations.shrink_to_fit();
        self.wide_ops.shrink_to_fit();
        self.consts.shrink_to_fit();
        self.f64_consts.shrink_to_fit();
        self.i64_consts.shrink_to_fit();
        self.globals.shrink_to_fit();
        self.shape_names.shrink_to_fit();
        self.mut_ref_params.shrink_to_fit();
        self.closure_protos.shrink_to_fit();
        self.select_arms.shrink_to_fit();
    }

    /// Returns the source position for `instruction`, when the compiler could
    /// associate that bytecode with a source expression.
    #[must_use]
    pub fn source_location(&self, instruction: InstrIdx) -> Option<SourceLocation> {
        let after = self
            .instruction_locations
            .partition_point(|entry| entry.instruction <= instruction);
        after
            .checked_sub(1)
            .and_then(|idx| self.instruction_locations[idx].location)
    }

    /// Produces a `Arc<Self>` so multiple callers share the same chunk.
    #[must_use]
    #[allow(
        clippy::arc_with_non_send_sync,
        reason = "FnChunk holds RefCell/Cell interior mutability; Arc shape is needed for goroutine-pool shared ownership even though the chunk itself is !Sync"
    )]
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod op_layout_tests {
    use super::Op;

    #[test]
    fn op_stays_within_dispatch_budget() {
        // Every dispatch copies one `Op` out of the instruction stream;
        // growing the enum grows every chunk and the per-op fetch. The
        // widest variant (`Call`) sets the 16-byte footprint; new
        // variants must pack within it (immediate operands are i32 for
        // this reason).
        let n = std::mem::size_of::<Op>();
        assert!(n <= 16, "Op grew to {n} bytes (budget 16)");
    }
}
