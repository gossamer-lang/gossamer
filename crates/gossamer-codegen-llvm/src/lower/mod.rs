#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! MIR → LLVM IR text lowering.
//!
//! One [`Lowerer`] per [`gossamer_mir::Body`]. It walks the
//! MIR in block order, allocates a single SSA value per MIR
//! local via an `alloca` in the entry block, and emits
//! `load` / `store` instructions around each statement. This
//! matches what `rustc` does at its `-O0` setting; `llc -O3`
//! folds the redundant loads away during mem2reg.

use std::fmt::Write;

use gossamer_mir::{
    BinOp, Body, ConstValue, Local, Operand, Place, Projection, Rvalue, Statement, StatementKind,
    Terminator, UnOp,
};
use gossamer_types::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

use crate::emit::BuildError;
use crate::ty::{
    NumericKind, STACK_AGGREGATE_SPILL_BYTES, aggregate_storage_bytes, elem_slots,
    field_slot_offset, int_signed, int_width, is_aggregate, is_unit, numeric_kind, param_llvm_ty,
    render_ty, slot_count,
};

/// Adds the typed `declare` for `name` from the ABI registry into `refs`.
///
/// This is the single source of truth for every `gos_rt_*`
/// declaration in the emitted LLVM IR. Panics if `name` is not in
/// the registry - that surface is intentional: a missing entry is a
/// compiler bug (the lowerer is about to emit a call whose ABI the
/// codebase has not committed to), and failing at LLVM IR emission
/// time gives a clearer signal than waiting for the verifier or
/// runtime to surface the mismatch.
fn declare_rt(refs: &mut std::collections::BTreeSet<String>, name: &str) {
    let entry = gossamer_abi::lookup(name).unwrap_or_else(|| {
        panic!(
            "declare_rt: unknown runtime symbol {name:?} - add it to gossamer-abi/src/registry.rs"
        )
    });
    refs.insert(entry.llvm_declare_for(crate::emit::target_is_windows()));
}

/// Emits one function's LLVM IR text, including the required
/// `declare` statements for any `gos_rt_*` symbols it calls.
pub(crate) struct Lowerer<'a> {
    pub(crate) body: &'a Body,
    pub(crate) tcx: &'a TyCtxt,
    /// Accumulator for the function body text.
    pub(crate) out: String,
    /// Monotonically increasing counter for SSA value names
    /// (`%t0`, `%t1`, …) - LLVM requires unique numbering
    /// within a function.
    pub(crate) next_ssa: u32,
    /// Runtime function signatures we've referenced so the
    /// enclosing module can emit the matching `declare`s.
    pub(crate) runtime_refs: std::collections::BTreeSet<String>,
    /// Line this body's call-stack frame was last moved to, so a run of
    /// statements on one source line emits a single update.
    pub(crate) last_frame_line: Option<u32>,
    /// `DefId.local` → function name map so `Operand::FnRef`
    /// resolves to the exported symbol. Populated by the
    /// emitter before calling [`Lowerer::lower`].
    pub(crate) fn_name_by_def: std::collections::HashMap<u32, String>,
    /// Callee fn-name → parameter MIR types map so `emit_named_call`
    /// can adapt argument lowering to the callee's signature
    /// (e.g. load the heap pointer from a slot when the param
    /// is `&Adt` rather than passing the slot address).
    pub(crate) param_tys_by_name: std::collections::HashMap<String, Vec<Ty>>,
    /// String-constant pool - the emitter materialises each
    /// entry as a `@.str_N = private unnamed_addr constant
    /// [len x i8] c"..."` module-level global so
    /// `ConstValue::Str(_)` operands can reference real
    /// `.rodata` bytes instead of `null`. Entries are shared
    /// with the module-wide pool via an `Rc<RefCell<...>>`
    /// populated by the emitter before calling
    /// [`Lowerer::lower`].
    pub(crate) strings: std::rc::Rc<std::cell::RefCell<StringPool>>,
    /// MIR block currently being lowered. Terminator lowering compares jump
    /// targets against it to detect back-edges and place the cooperative poll.
    pub(crate) current_block: Option<u32>,
    /// Unique suffix for native preemption blocks in the emitted LLVM IR.
    pub(crate) preempt_seq: u32,
    /// Inter-procedural capture summary. The emitter populates
    /// this once per module (via `build_capture_summary`) and the
    /// cleanup pass uses it to skip escape marks for callee
    /// parameters that are known not to capture, unlocking precise
    /// per-block drops for owning bindings whose only outbound use
    /// is a non-capturing user fn.
    pub(crate) capture_summary: gossamer_mir::CaptureSummary,
    /// User functions whose address is handed to a runtime callback
    /// (`http::serve` / `http::serve_h2c`) and so are invoked by the
    /// rustc-compiled runtime through the `extern "C" fn(..) -> i128`
    /// ABI. On Win64 that ABI returns the 2-word `i128` in a vector
    /// register (xmm0), but a gossamer `define i128`/`ret i128` returns
    /// it in the GP-register pair - so `gos_fn_addr` on these names is
    /// redirected to a `<16 x i8>` C-ABI return thunk (`name$cabi`).
    /// Maps the handler name to its parameter arity. Empty off Windows.
    pub(crate) cabi_handlers: std::collections::BTreeMap<String, usize>,
    /// `alloca` lines for the call-scoped temporaries the body needs, spliced
    /// into the entry block once the blocks are lowered.
    ///
    /// LLVM promotes an `alloca` only in the entry block, and one emitted in
    /// the block that holds a call allocates again on every iteration of a
    /// loop around it - stack that is not reclaimed until the function
    /// returns. One instruction per call site, executed once, is what these
    /// temporaries actually need.
    pub(crate) entry_allocas: Vec<String>,
}

/// Module-scoped string intern pool.
#[derive(Debug, Default)]
pub(crate) struct StringPool {
    /// Source-text → (`global_name`, `byte_length`) map.
    entries: std::collections::HashMap<String, (String, usize)>,
    next_id: u32,
}

impl StringPool {
    pub(crate) fn intern(&mut self, text: &str) -> (String, usize) {
        if let Some(hit) = self.entries.get(text) {
            return hit.clone();
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = format!("@.gstr_{id}");
        let entry = (name, text.len() + 1);
        self.entries.insert(text.to_string(), entry.clone());
        entry
    }

    /// Renders every interned string as an LLVM global with a
    /// length-carrying header so `gos_rt_str_len` / `gos_rt_str_slice`
    /// are O(1) on string literals (matching heap strings).
    ///
    /// Each literal has an explicit owner followed by the legacy
    /// `<rc, cap, len, tag, bytes>` suffix. The body is at `base + 29`, so
    /// its carrier shape matches heap strings while `ptr[-1]` / `ptr[-5]`
    /// keep their established tag/length offsets.
    ///
    /// The backing constant is deliberately *not* `unnamed_addr`: the
    /// body alias is an interior pointer (`base + 29`), so the constant's
    /// address and layout are significant. Marking it `unnamed_addr`
    /// lets the Mach-O backend file 4/8/16-byte constants into the
    /// mergeable `__literal{4,8,16}` pools, where ld64 coalesces and
    /// reorders individual literals and does not honour the interior
    /// `.alt_entry` body symbol - the alias then resolves into the wrong
    /// literal and the runtime reads a corrupt length/tag header
    /// (SIGSEGV/SIGBUS on macOS). Plain `constant` keeps it in `__const`,
    /// which preserves interior symbols on every target.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        // Constant layout is part of the emitted module, so the pool is
        // walked in a total order over the literal text rather than in
        // hash order: identical input must yield identical IR.
        let mut ordered: Vec<(&String, &(String, usize))> = self.entries.iter().collect();
        ordered.sort_unstable_by_key(|(text, _)| *text);
        for (text, (name, size)) in ordered {
            let escaped = escape_c_string(text);
            let content_len = text.len();
            let index_slots = content_len / 32 + 2;
            let mut index = vec![0u32; index_slots];
            // All-ASCII content states itself with the sentinel the runtime
            // writes for the same case, so a character index is a byte offset
            // and the per-block offsets are the identity. Both producers of a
            // typed string agree on the convention, which is what lets a
            // reader take one path whatever built the string.
            if text.is_ascii() {
                index[0] = gossamer_abi::string_layout::INDEX_ASCII;
            } else {
                index[0] = text.chars().count() as u32;
                for (char_index, (byte_index, _)) in text.char_indices().enumerate() {
                    if char_index.is_multiple_of(gossamer_abi::string_layout::INDEX_STRIDE) {
                        index[1 + char_index / gossamer_abi::string_layout::INDEX_STRIDE] =
                            byte_index as u32;
                    }
                }
            }
            let index_values = index
                .iter()
                .map(|offset| format!("i32 {offset}"))
                .collect::<Vec<_>>()
                .join(", ");
            let data = format!("{name}.data");
            let ty = format!(
                "<{{ [16 x i8], i32, i32, i32, i8, [{size} x i8], [{index_slots} x i32] }}>"
            );
            // The runtime reaches a string body at `base + 29` and selects its
            // carrier by the body's address modulo 8, so an 8-aligned base is
            // what makes every body congruent to 5 and keeps the header read
            // inside this blob. A packed type has an ABI alignment of 1, so the
            // blob states the alignment it needs rather than inheriting one.
            let _ = writeln!(
                out,
                "{data} = private constant {ty} \
                 <{{ [16 x i8] [i8 1, i8 0, i8 2, i8 0, i8 3, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0, i8 0], i32 0, i32 {content_len}, i32 {content_len}, i8 -88, [{size} x i8] c\"{escaped}\\00\", [{index_slots} x i32] [{index_values}] }}>, align 8"
            );
            // `-88` is `0xA8` (STR_STATIC_TAG) as a signed i8.
            let _ = writeln!(
                out,
                "{name} = private unnamed_addr alias i8, ptr getelementptr inbounds \
                 ({ty}, ptr {data}, i32 0, i32 5, i32 0)"
            );
        }
        out
    }
}

/// Operand classification for `__concat`'s per-arg dispatch.
///
/// `Unsupported` is an internal classifier for operand types that
/// need richer display planning before they can be concatenated.
/// Reaching it after frontend validation is treated as a backend
/// invariant failure.
#[derive(Debug, Clone)]
pub(super) enum ConcatKind {
    StrPtr,
    /// A unit value renders as the literal `()`, the same text every
    /// other tier prints for it.
    Unit,
    Int,
    /// Unsigned integer (u8/u16/u32/u64/u128/usize). Routed to
    /// `gos_rt_print_u64` / `gos_rt_concat_u64` so values
    /// `>= 2^63` print without a leading `-`.
    Uint,
    Float,
    Bool,
    Char,
    /// `Vec<i64>` (or any 8-byte-elem Vec) formatted via
    /// `gos_rt_vec_format_i64`.
    VecI64,
    /// `Vec<u64>` / `Vec<usize>`: the same slots read as unsigned, so an
    /// element at or above `i64::MAX` renders as its own decimal.
    VecUint,
    /// `Set<u64>` / `BTreeSet<usize>`: the elements read as unsigned. The
    /// `bool` is true for the ordered spelling.
    SetUint(bool),
    VecF64,
    VecBool,
    VecString,
    VecChar,
    VecVecI64,
    VecVecF64,
    VecVecString,
    /// `Vec<Adt>` whose element type derives `Debug`, rendered element-wise
    /// via `gos_rt_vec_format_adt`. The `String` names that type's `fmt`; the
    /// `bool` is true when `fmt` takes the element's slot address (a struct)
    /// rather than the element word itself (an inline enum).
    VecAdt(String, bool),
    /// Map-elem Vec.
    VecMap,
    /// Vec whose elements are described by a descriptor stream.
    VecDesc(ValueDesc),
    /// The same rendering as the kind it wraps, written in the bare-bracket
    /// spelling a fixed array and a slice take. A slice shares the `Vec`
    /// runtime representation, so only the static type tells them apart.
    Seq(Box<ConcatKind>),
    /// A tuple with a field no flat tag describes, rendered by walking the
    /// tuple's own descriptor over its slot buffer.
    TupleDesc(ValueDesc),
    /// Map described by one stream holding the key descriptor then the value's.
    MapDesc(ValueDesc, usize),
    /// Tuple-elem Vec: the per-element tag bytes and the tuple's arity.
    VecTuple(Vec<u8>, usize),
    /// Map whose keys and values carry the given tuple tags (`0` for a signed
    /// scalar, `1` for an unsigned one).
    MapTagged(u8, u8),
    /// Map whose values are aggregates rendered by a derived `fmt`.
    MapAdt(String),
    /// Map whose values are tuples: the per-element tag bytes and the arity.
    MapTuple(Vec<u8>, usize),
    /// `[i64; N]` flat-buffer literal; the embedded length is
    /// passed alongside the buffer pointer to the runtime helper.
    ArrI64(i64),
    /// `[u8; N]` flat buffer. This backend lays a byte array out packed
    /// rather than one element per slot, so its debug form reads with a
    /// stride of one. The Cranelift backend keeps whole slots and formats
    /// the same array through [`ConcatKind::ArrI64`]; each tier renders the
    /// layout it emitted, and the printed text agrees.
    ArrU8(i64),
    ArrF64(i64),
    ArrBool(i64),
    ArrChar(i64),
    ArrString(i64),
    /// `[Adt; N]` whose element type derives `Debug`. Carries the length, the
    /// per-element stride in bytes, the `fmt` symbol, and whether `fmt` takes
    /// the element's slot address (see [`ConcatKind::VecAdt`]).
    ArrAdt(i64, i64, String, bool),
    /// `[[i64; M]; N]` flat-buffer nested array (`N * M` contiguous
    /// slots, rows inline); both static lengths are passed to
    /// `gos_rt_arr_format_arr_i64`.
    ArrArrI64(i64, i64),
    ArrArrF64(i64, i64),
    ArrArrBool(i64, i64),
    /// `json::Value` rendered via `gos_rt_json_display`.
    JsonValue,
    /// `DynValue` rendered via `gos_rt_dyn_format`.
    DynValue,
    /// `errors::Error` rendered via `gos_rt_error_message`.
    ErrorMessage,
    /// A tuple of scalar elements rendered via `gos_rt_tuple_format`.
    /// The per-element tag array is computed at the emit site from the
    /// tuple's element types.
    Tuple,
    /// A scalar-keyed, scalar/string-valued `HashMap` rendered via
    /// `gos_rt_map_format`.
    Map,
    /// `HashSet<T>` / `BTreeSet<T>` with i64 elements. The bool is true for
    /// `BTreeSet`, which only changes the display prefix.
    SetI64(bool),
    /// `HashSet<T>` / `BTreeSet<T>` with String elements. The bool is true
    /// for `BTreeSet`, which only changes the display prefix.
    SetString(bool),
    /// Set whose aggregate elements render through a descriptor stream.
    SetDesc(ValueDesc, bool),
    /// `Set<T>` / `BTreeSet<T>` whose elements are one word read through a
    /// tag - a `bool`, a `char`, an `f64`. The bool is true for `BTreeSet`.
    SetTagged(u8, bool),
    /// `Set<T>` / `BTreeSet<T>` of enum elements: each stored slot addresses
    /// the node its descriptor reads. The bool is true for `BTreeSet`.
    SetEkey(ValueDesc, bool),
    /// A container handle - `Deque` / `Queue` / `Stack` / `MaxHeap` /
    /// `MinHeap` - rendered by the named runtime shim, which owns the one
    /// text form every tier prints.
    HandleFormat(&'static str),
    /// A container handle whose elements are read through a descriptor
    /// stream, for an element the one-word integer shim cannot render.
    HandleFormatDesc(&'static str, ValueDesc),
    /// `{:?}` of an `Option<T>`, rendered via the runtime's option debug
    /// helper with the payload's rendering plan.
    Option(DebugPayload),
    /// `{:?}` of a `Result<T, E>`, rendered via the runtime's result debug
    /// helper with the Ok / Err payload rendering plans.
    Result(DebugPayload, DebugPayload),
    Unsupported,
}

/// A rendering descriptor: the tag byte stream, plus the ordered table of
/// per-type derived `fmt` symbols its `DESC_ADT` entries index into. The two
/// travel together in one emitted global (see
/// `Lowerer::intern_value_descriptor`), so a struct or enum nested anywhere
/// in a shape renders through the same formatter a bare `{:?}` on it calls.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ValueDesc {
    pub(super) bytes: Vec<u8>,
    pub(super) fns: Vec<String>,
}

/// How the runtime renders one `Option` / `Result` payload word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DebugPayload {
    /// A scalar, String, or collection payload the runtime renders from a
    /// fixed tag (see `debug_payload_kind`).
    Tag(u8),
    /// An aggregate payload: the word is the address of its slot buffer and
    /// the `String` names the type's derived `fmt`.
    Fmt(String),
    /// A tuple payload: the word is its slot buffer and the bytes are a tag
    /// stream opening with the nested marker and the tuple's arity.
    Tuple(Vec<u8>),
    /// A payload rendered through a recursive descriptor stream.
    Desc(ValueDesc),
}

impl DebugPayload {
    /// The `payload_kind` tag byte the runtime dispatches on.
    pub(super) fn tag(&self) -> u8 {
        match self {
            Self::Tag(t) => *t,
            Self::Fmt(_) => gossamer_abi::DEBUG_PAYLOAD_ADT,
            Self::Tuple(_) => gossamer_abi::DEBUG_PAYLOAD_TUPLE,
            Self::Desc(_) => gossamer_abi::DEBUG_PAYLOAD_DESC,
        }
    }
}

/// `true` if `t` is one of `u8 / u16 / u32 / u64 / u128 / usize`.
/// Mirror of cranelift's `int_ty_is_unsigned`; kept here so the
/// LLVM module doesn't need to depend on the cranelift crate.
fn int_ty_is_unsigned_llvm(t: IntTy) -> bool {
    matches!(
        t,
        IntTy::U8 | IntTy::U16 | IntTy::U32 | IntTy::U64 | IntTy::U128 | IntTy::Usize
    )
}

/// 0.6.0 deep-free element-kind tags. Mirrors `vec_elem_kind` in
/// `gossamer-runtime/src/c_abi.rs`. Keep in sync with the runtime
/// constants and the Cranelift backend's `vec_elem_kind_codegen`.
mod vec_elem_kind_llvm {
    pub(super) const PRIMITIVE: i32 = 0;
    pub(super) const STRING: i32 = 1;
    pub(super) const VEC: i32 = 2;
    pub(super) const MAP: i32 = 3;
    pub(super) const ERROR: i32 = 4;
    /// A struct, tuple, or fixed array of a single slot, held inline.
    pub(super) const AGGR_FLAT: i32 = 10;
}

/// Derives the `elem_kind` discriminator for a `Vec<T>` destination
/// local. Inspects the local's MIR type and returns the tag the
/// runtime's deep-free path uses to reclaim element payloads.
///
/// Returns `PRIMITIVE` for unresolved types and non-Vec shapes -
/// the runtime treats PRIMITIVE as shallow-free, which is correct
/// for any element type that owns no further heap memory.
fn llvm_vec_elem_kind_from_local(body: &Body, tcx: &TyCtxt, dest_local: Local) -> i32 {
    let ty = body.local_ty(dest_local);
    let inner = match tcx.kind(ty) {
        Some(TyKind::Vec(inner) | TyKind::Slice(inner) | TyKind::Iterator(inner)) => *inner,
        _ => return vec_elem_kind_llvm::PRIMITIVE,
    };
    match tcx.kind(inner) {
        Some(TyKind::String) => vec_elem_kind_llvm::STRING,
        Some(TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Iterator(_)) => vec_elem_kind_llvm::VEC,
        Some(TyKind::HashMap { .. }) => vec_elem_kind_llvm::MAP,
        Some(TyKind::DynError) => vec_elem_kind_llvm::ERROR,
        _ if tcx.is_flat_inline_aggregate(inner) => vec_elem_kind_llvm::AGGR_FLAT,
        _ => vec_elem_kind_llvm::PRIMITIVE,
    }
}

/// Per-element slot byte width for a `Vec<T>` / `Slice<T>` destination
/// local, mirroring the MIR builder's `elem_bytes_of` so a bare
/// `Vec::new()` (which carries no element-width argument) allocates the
/// same stride a `[]` literal of the same type does. Returns `None`
/// when the destination is not a statically-known vec/slice, so the
/// caller keeps its scalar default.
fn llvm_vec_elem_bytes_from_local(body: &Body, tcx: &TyCtxt, dest_local: Local) -> Option<i64> {
    let ty = body.local_ty(dest_local);
    let inner = match tcx.kind(ty) {
        Some(TyKind::Vec(inner) | TyKind::Slice(inner) | TyKind::Iterator(inner)) => *inner,
        _ => return None,
    };
    let bytes = match tcx.kind(inner) {
        Some(TyKind::Bool | TyKind::Int(gossamer_types::IntTy::U8)) => 1,
        Some(TyKind::Char | TyKind::Int(_) | TyKind::Float(_) | TyKind::String) => 8,
        Some(TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. }) => {
            i64::from(tcx.slot_bytes(inner))
        }
        _ => 8,
    };
    Some(bytes)
}

/// `!tbaa` suffix for a load/store of an aggregate *header* field: a `GosVec`
/// / `GosI64Vec` / `GosU8Vec` len/cap/elem_bytes/data-pointer, or a string
/// builder's rc/cap/len/tag prefix. Pairs with [`TBAA_DATA`]; the two reference
/// the sibling TBAA type nodes defined by `crate::emit::TBAA_METADATA` (`!4` =
/// header access tag, `!5` = element-data access tag). The header lives in a
/// distinct allocation from - or a disjoint byte range of the same allocation
/// as - the element buffer, so tagging the two distinctly lets `-O3` hoist a
/// `len`/`data` load out of an element loop it would otherwise treat as
/// clobbered by every element store.
pub(crate) const TBAA_HEADER: &str = ", !tbaa !4";
/// `!tbaa` suffix for a load/store of aggregate payload memory: element /
/// string-content bytes (the memory a header's data pointer addresses) and the
/// flat i64 slot slabs that hold struct fields, tuple elements, and fixed-array
/// elements. Pairs with [`TBAA_HEADER`]: every access carrying this tag lands
/// in payload memory, never on a runtime header word.
pub(crate) const TBAA_DATA: &str = ", !tbaa !5";

mod emit_aggregate;
mod emit_misc;
mod misc;
mod operand;
mod rvalue;
mod setup;
mod stmt;

fn local_slot(local: Local) -> String {
    format!("%l{}", local.as_u32())
}

fn render_const(cv: &ConstValue) -> String {
    match cv {
        ConstValue::Unit => String::new(),
        // Emit bool constants as 0/1 rather than false/true:
        // LLVM accepts both for `i1`, but only the numeric form is
        // valid when the same constant is later used as `i64`
        // (e.g. when the binop pipeline widens an i1 operand to
        // i64). Avoids `and i64 true, %t` shapes that opt rejects.
        ConstValue::Bool(false) => "0".to_string(),
        ConstValue::Bool(true) => "1".to_string(),
        ConstValue::Int(n) => n.to_string(),
        ConstValue::Float(bits) => {
            // `ConstValue::Float` already stores the bit
            // pattern of an IEEE-754 binary64. LLVM accepts
            // hex-encoded literals via `0xH…` - use that for
            // exact round-tripping.
            format!("0x{bits:016X}")
        }
        ConstValue::Char(c) => (*c as u32).to_string(),
        ConstValue::Str(_) => {
            // Strings go through the runtime; MVP doesn't
            // support them yet as value-level constants.
            "null".to_string()
        }
    }
}

/// Textual LLVM type for a constant. The MIR `ConstValue`
/// always carries enough tag to pick the right LLVM family;
/// we bake the default widths (`i64`, `double`) that the
/// frontend's literal-lowering produces.
fn const_llvm_ty(cv: &ConstValue) -> &'static str {
    match cv {
        ConstValue::Unit => "void",
        ConstValue::Bool(_) => "i1",
        ConstValue::Int(_) => "i64",
        ConstValue::Float(_) => "double",
        ConstValue::Char(_) => "i32",
        ConstValue::Str(_) => "ptr",
    }
}

fn is_cmp(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    )
}

fn int_cmp_pred(op: BinOp, signed: bool) -> &'static str {
    match (op, signed) {
        (BinOp::Eq, _) => "eq",
        (BinOp::Ne, _) => "ne",
        (BinOp::Lt, true) => "slt",
        (BinOp::Lt, false) => "ult",
        (BinOp::Le, true) => "sle",
        (BinOp::Le, false) => "ule",
        (BinOp::Gt, true) => "sgt",
        (BinOp::Gt, false) => "ugt",
        (BinOp::Ge, true) => "sge",
        (BinOp::Ge, false) => "uge",
        _ => "eq",
    }
}

fn float_cmp_pred(op: BinOp) -> &'static str {
    match op {
        // `oeq` is false when either operand is NaN, so `NaN == NaN` is
        // false - matching IEEE and the VM. `!=` must be the *unordered*
        // `une` (true when either side is NaN) so `NaN != NaN` is true; the
        // ordered `one` would wrongly report `false` for any NaN operand.
        BinOp::Eq => "oeq",
        BinOp::Ne => "une",
        BinOp::Lt => "olt",
        BinOp::Le => "ole",
        BinOp::Gt => "ogt",
        BinOp::Ge => "oge",
        _ => "oeq",
    }
}

/// Rewrites `::` and other path punctuation so the resulting
/// identifier is a legal LLVM function name when rendered
/// inside quotes.
fn escape_ident(name: &str) -> String {
    name.replace('"', "\\\"")
}

/// Returns the LLVM-side function symbol for a Gossamer function
/// name. The user's `main` becomes `gos_main` so the C runtime can
/// own the real `main` (it sets up argv, calls into `gos_main`,
/// then forwards the i64 return through `gos_rt_main_exit_code`).
/// Every other name passes through unchanged.
///
/// Centralising this here lets both the `define` line in `lower`
/// and the declaration emitter in `emit` agree without a post-hoc
/// `out.replace("@\"main\"", ...)` pass that doubled the IR
/// string's peak heap on big programs.
pub(crate) fn mangle_fn_name(name: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    // The user's `main` becomes `gos_main`; the C runtime owns the real
    // `main`.
    if name == "main" {
        return Cow::Borrowed("gos_main");
    }
    // Only a user function whose name shadows a libc / system symbol the
    // statically-linked Rust runtime calls is renamed. Renaming every user
    // function (a reserved-namespace prefix) regressed RC-heavy code, so the
    // mangle is scoped to the exact collision: a user `fn getenv` otherwise
    // interposes libc's `getenv` and the runtime's `gos_rt_os_env` ->
    // `std::env::var` -> `getenv` path recurses into the user function until
    // the stack overflows. `gosu_<name>` cannot collide with any of these.
    if shadows_c_runtime_symbol(name) {
        return Cow::Owned(format!("gosu_{name}"));
    }
    Cow::Borrowed(name)
}

/// True when `name` matches a libc / system symbol the statically-linked
/// Rust runtime (libstd + gossamer-runtime + mimalloc) references, so a
/// user function of the same name would interpose it at link time. The set
/// is the C runtime surface those crates actually call - the allocator,
/// the environment, the memory/string intrinsics, process control, and the
/// raw I/O / socket syscalls. (Compiler-synthesized helpers carry `gos_*`
/// / `__*` names and never reach here.)
fn shadows_c_runtime_symbol(name: &str) -> bool {
    matches!(
        name,
        // environment
        "getenv" | "setenv" | "unsetenv" | "putenv" | "environ"
        // allocator
        | "malloc" | "calloc" | "realloc" | "free" | "aligned_alloc"
        | "posix_memalign" | "malloc_usable_size" | "reallocarray" | "valloc"
        // memory / string intrinsics
        | "memcpy" | "memmove" | "memset" | "memcmp" | "memchr" | "bcmp"
        | "strlen" | "strnlen" | "strcmp" | "strncmp" | "strcpy" | "strncpy"
        | "strcat" | "strncat" | "strchr" | "strrchr" | "strstr" | "strdup"
        // stdio / formatted output
        | "printf" | "fprintf" | "sprintf" | "snprintf" | "vsnprintf"
        | "puts" | "fputs" | "fputc" | "putchar" | "fwrite" | "fread"
        | "fopen" | "fclose" | "fflush" | "fdopen" | "setvbuf"
        // raw I/O / fs syscalls
        | "read" | "write" | "pread" | "pwrite" | "open" | "openat" | "close"
        | "lseek" | "fsync" | "fstat" | "stat" | "lstat" | "fcntl" | "ioctl"
        | "dup" | "dup2" | "pipe" | "poll" | "select" | "mmap" | "munmap"
        | "mprotect" | "madvise"
        // sockets
        | "socket" | "connect" | "bind" | "listen" | "accept" | "send"
        | "recv" | "sendto" | "recvfrom" | "setsockopt" | "getsockopt"
        | "shutdown" | "getaddrinfo" | "freeaddrinfo" | "gethostbyname"
        | "getsockname" | "getpeername"
        // process / signals / threads
        | "exit" | "_exit" | "abort" | "atexit" | "system" | "getpid"
        | "fork" | "execve" | "execvp" | "waitpid" | "wait" | "kill"
        | "signal" | "sigaction" | "raise" | "sysconf" | "getrandom"
        | "pthread_create" | "pthread_join" | "pthread_mutex_lock"
        | "pthread_mutex_unlock" | "pthread_self" | "sched_yield"
        | "dlopen" | "dlsym" | "dlclose" | "dlerror"
        // time
        | "time" | "clock_gettime" | "gettimeofday" | "nanosleep"
        | "usleep" | "sleep" | "localtime" | "gmtime" | "mktime"
        // rng / math the runtime may pull
        | "rand" | "srand" | "random" | "srandom" | "arc4random"
    )
}

/// Maps user-level math path names onto the LLVM intrinsic
/// that `llc` will lower to the host's SIMD/FP instruction.
/// Recognises both the bare (`sqrt`) and module-qualified
/// (`math::sqrt`) spellings so the match fires regardless of
/// whether the user writes `sqrt(x)` or `math::sqrt(x)`.
fn math_intrinsic(name: &str) -> Option<&'static str> {
    let tail = name.rsplit("::").next().unwrap_or(name);
    let llvm = match tail {
        "sqrt" => "llvm.sqrt.f64",
        "sin" => "llvm.sin.f64",
        "cos" => "llvm.cos.f64",
        "abs" | "fabs" => "llvm.fabs.f64",
        "floor" => "llvm.floor.f64",
        "ceil" => "llvm.ceil.f64",
        "exp" => "llvm.exp.f64",
        "ln" | "log" => "llvm.log.f64",
        _ => return None,
    };
    Some(llvm)
}

/// Maps a prelude / stdlib call name to the runtime symbol the
/// LLVM module should emit a `call` against. Each arm mirrors
/// the equivalent Cranelift intrinsic dispatch arm so the LLVM
/// backend covers the same surface area without per-program
/// patches. Names without a known mapping pass through verbatim
/// so user-defined functions still resolve.
fn map_prelude_symbol(name: &str) -> &str {
    match name {
        "println" | "print" | "eprintln" | "eprint" => "gos_rt_print_str",
        "panic" => "gos_rt_panic",
        "os::args" | "env::args" => "gos_rt_os_args",
        "os::program_name" | "env::program_name" => "gos_rt_os_program_name",
        "env::temp_dir" | "os::temp_dir" => "gos_rt_env_temp_dir",
        "env::home_dir" | "os::home_dir" => "gos_rt_env_home_dir",
        "env::vars" => "gos_rt_env_vars",
        "os::exit" | "process::exit" => "gos_rt_exit",
        "process::id" => "gos_rt_process_id",
        "process::abort" => "gos_rt_process_abort",
        "io::stdout" => "gos_rt_io_stdout",
        "io::stderr" => "gos_rt_io_stderr",
        "io::stdin" => "gos_rt_io_stdin",
        "time::now" => "gos_rt_time_now",
        "time::now_ms" => "gos_rt_time_now_ms",
        "time::now_ns" | "time::now_nanos" => "gos_rt_now_ns",
        "time::monotonic_ms" => "gos_rt_monotonic_ms",
        "time::monotonic_nanos" => "gos_rt_monotonic_nanos",
        "time::sleep" => "gos_rt_sleep_ms",
        "math::pow" => "gos_rt_math_pow",
        "math::abs" => "gos_rt_math_abs",
        "math::sqrt" => "gos_rt_math_sqrt",
        // 0.7.0 scalar cmp helpers - MIR routes user calls
        // through `gos_rt_{min,max,clamp}_{i64,f64}` directly via
        // `lower_stdlib_free_call`, so the LLVM tier never reaches
        // these mappings via name lookup. They are listed here so
        // `dispatch_parity::every_runtime_helper_has_llvm_declaration`
        // sees the symbols referenced in lower.rs.
        "min::i64" => "gos_rt_min_i64",
        "max::i64" => "gos_rt_max_i64",
        "clamp::i64" => "gos_rt_clamp_i64",
        "min::f64" => "gos_rt_min_f64",
        "max::f64" => "gos_rt_max_f64",
        "clamp::f64" => "gos_rt_clamp_f64",
        "math::sin" => "gos_rt_math_sin",
        "math::cos" => "gos_rt_math_cos",
        "math::ln" | "math::log" => "gos_rt_math_log",
        "math::exp" => "gos_rt_math_exp",
        "math::floor" => "gos_rt_math_floor",
        "math::ceil" => "gos_rt_math_ceil",
        "sync::yield_now" | "runtime::yield_now" => "gos_rt_go_yield",
        "Mutex::new" | "sync::Mutex::new" | "mutex::new" => "gos_rt_mutex_new",
        "Map::new" | "sync::Map::new" => "gos_rt_sync_map_new",
        "WaitGroup::new" | "sync::WaitGroup::new" | "wg::new" => "gos_rt_wg_new",
        "I64Vec::new" | "heap_i64::new" => "gos_rt_heap_i64_new",
        "U8Vec::new" | "heap_u8::new" => "gos_rt_heap_u8_new",
        // HeapU8 (U8Vec) method calls - already named correctly; listed
        // explicitly so the dispatch-parity test sees a text reference.
        "gos_rt_heap_u8_new" => "gos_rt_heap_u8_new",
        "gos_rt_heap_u8_get" => "gos_rt_heap_u8_get",
        "gos_rt_heap_u8_set" => "gos_rt_heap_u8_set",
        "gos_rt_heap_u8_len" => "gos_rt_heap_u8_len",
        "gos_rt_heap_u8_to_string" => "gos_rt_heap_u8_to_string",
        "gos_rt_heap_u8_write_bytes_to_stdout" => "gos_rt_heap_u8_write_bytes_to_stdout",
        "gos_rt_heap_u8_write_lines_to_stdout" => "gos_rt_heap_u8_write_lines_to_stdout",
        "gos_rt_heap_u8_free" => "gos_rt_heap_u8_free",
        "gos_rt_heap_i64_free" => "gos_rt_heap_i64_free",
        "gos_rt_chan_drop" => "gos_rt_chan_drop",
        // String allocator reclamation for owning bindings the
        // cleanup pass schedules to drop at body return.
        "gos_rt_str_free" => "gos_rt_str_free",
        "gos_rt_str_free_typed" => "gos_rt_str_free_typed",
        "gos_rt_str_retain_typed" => "gos_rt_str_retain_typed",
        "Atomic::new"
        | "sync::Atomic::new"
        | "atomic::new"
        | "AtomicI64::new"
        | "sync::AtomicI64::new"
        | "AtomicU64::new"
        | "sync::AtomicU64::new"
        | "AtomicI32::new"
        | "sync::AtomicI32::new" => "gos_rt_atomic_i64_new",
        "AtomicBool::new" | "sync::AtomicBool::new" => "gos_rt_atomic_bool_new",
        "lcg::jump" | "lcg_jump" => "gos_rt_lcg_jump",
        other => other,
    }
}

/// Writes the outgoing terminator branch for a Call/Math
/// instruction: a `br label %bbN` for the success target or an
/// `unreachable` when the call is `noreturn`.
fn emit_terminator_branch(out: &mut String, target: Option<&gossamer_mir::BlockId>) {
    match target {
        Some(t) => {
            writeln!(out, "  br label %bb{}", t.as_u32()).unwrap();
        }
        None => {
            writeln!(out, "  unreachable").unwrap();
        }
    }
}

/// LLVM `\HH` hex-escape for string constants. Any byte that
/// isn't a printable ASCII character gets rendered as `\HH`.
fn escape_c_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            0x20..=0x7E if *b != b'"' && *b != b'\\' => out.push(*b as char),
            _ => {
                let _ = write!(out, "\\{b:02X}");
            }
        }
    }
    out
}

mod lower_call;
mod lower_diagnostic;
mod lower_expr_ops;
mod lower_inline;

#[cfg(test)]
mod tests {
    use super::StringPool;

    #[test]
    fn header_string_constant_is_not_unnamed_addr() {
        // A 2-char literal is an 8-byte header'd constant. With
        // `unnamed_addr` the Mach-O backend files it into the mergeable
        // `__literal8` pool, where ld64 coalesces/reorders literals and
        // ignores the interior `.alt_entry` body symbol - corrupting the
        // `base + 29` body pointer (SIGSEGV/SIGBUS on macOS). The backing
        // constant must therefore stay a plain (address-significant)
        // `constant` so it lands in `__const`.
        let mut pool = StringPool::default();
        pool.intern("w=");
        let ir = pool.render();
        let data_line = ir
            .lines()
            .find(|l| l.contains(".data = "))
            .expect("a .data constant line");
        assert!(
            !data_line.contains("unnamed_addr"),
            "header string constant must not be unnamed_addr (mergeable-literal hazard):\n{data_line}"
        );
        // The body alias must still be an interior pointer at field 5
        // (`base + 29`): owner plus the legacy string header.
        assert!(
            ir.contains("i32 0, i32 5, i32 0"),
            "body alias must point past the 29-byte owner/header:\n{ir}"
        );
    }
}
