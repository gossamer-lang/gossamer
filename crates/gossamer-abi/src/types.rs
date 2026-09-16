/// Type of a single parameter or return value in the C-ABI.
///
/// Matches the LLVM IR types used in `declare` statements. `U64` is
/// `i64` at the IR level but carries unsigned semantics at the Rust
/// level. `Bool` maps to `i8` in C (zero = false, non-zero = true).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbiType {
    /// `void` - only valid as a return type.
    Void,
    /// `i8` - used for bool returns and narrow C types.
    I8,
    /// `i32` - used for C `int` and discriminants.
    I32,
    /// `i64` - the default integer width.
    I64,
    /// `i64` at the IR level; unsigned contract at the Rust level.
    U64,
    /// `i128` - the 2-word by-value representation of `Result`/`Option`
    /// (discriminant in the low 64 bits, payload in the high 64 bits).
    I128,
    /// `double` - 64-bit IEEE 754 float.
    F64,
    /// Opaque pointer (`ptr` in LLVM opaque-pointer mode).
    Ptr,
}

/// Signature of a runtime symbol: parameter list + return type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiSig {
    /// Ordered parameter types.
    pub params: &'static [AbiType],
    /// Return type. Use `AbiType::Void` for functions that return nothing.
    pub ret: AbiType,
}

/// Which compiled backend tier(s) emit calls to a runtime symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Called by both the LLVM and Cranelift backends.
    Both,
    /// Called only by the Cranelift backend.
    Cranelift,
    /// Called only by the LLVM backend.
    Llvm,
}

/// The ABI class a shim interprets a sequence buffer's elements as.
///
/// The C ABI has no generics, so a combinator shim's element crossing is
/// carried by the symbol it is called through. This names that crossing so
/// the compiler can derive the symbol from the element type rather than
/// having each lowering arm spell it, and so a mismatch is checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElemClass {
    /// One integer register per element: `i64`, `bool`, `char`, and the
    /// managed pointer word a `String` or a boxed value is reached through.
    Word,
    /// One SSE register per element: `f64`.
    Float,
    /// The address of element storage wider than one slot, or of a slot whose
    /// fields are read at an offset: a tuple, a struct, a fixed array.
    Ptr,
}

impl ElemClass {
    /// The suffix this class contributes to a shim's symbol name.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            ElemClass::Word => "i64",
            ElemClass::Float => "f64",
            ElemClass::Ptr => "ptr",
        }
    }
}

/// The element crossing a combinator shim implements.
///
/// A shim's C-ABI signature does not distinguish `gos_rt_iter_take_while_i64`
/// from a `_ptr` twin - both are `(Ptr, Ptr) -> Ptr`. What differs is the
/// class each interprets its sequence buffer as, which is why that class is
/// declared here rather than left implicit in the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CombinatorAbi {
    /// The `iter::` combinator this shim implements, without the module
    /// prefix: `"take_while"`, `"windows"`, `"min_by_key"`.
    pub combinator: &'static str,
    /// The class the shim reads its input sequence buffer as.
    pub elem: ElemClass,
    /// The class of the element the shim produces, when the symbol
    /// distinguishes it: `map`'s output, `fold`'s accumulator, `sum_by`'s
    /// projection. `None` when the combinator's result shape is fixed by the
    /// input, so the symbol carries one class only.
    pub result: Option<ElemClass>,
}

/// One entry in the ABI registry describing a `gos_rt_*` symbol.
#[derive(Debug, Clone)]
pub struct RuntimeEntry {
    /// Symbol name without the `@` sigil, e.g. `"gos_rt_vec_push"`.
    pub name: &'static str,
    /// Full C-ABI signature.
    pub sig: AbiSig,
    /// Which compiled backend tier(s) use this symbol.
    pub tier: Tier,
    /// One-line description for `gos explain` output.
    pub docs: &'static str,
    /// When true the LLVM declaration gains `noreturn cold nounwind` attributes.
    /// Only set for functions that provably never return (abort / panic paths).
    pub noreturn: bool,
    /// When true the function may unwind, so the declaration omits the
    /// `nounwind` attribute (even when `noreturn` is set). Required for
    /// `gos_rt_panic`, which raises a Rust panic on the goroutine path
    /// that must propagate across its caller to the coroutine catch.
    pub unwinds: bool,
    /// The element crossing, for a sequence-combinator shim. `None` for every
    /// other symbol. This is the half of a combinator's contract its C-ABI
    /// signature cannot express, and the compiler derives the symbol from it.
    pub combinator: Option<CombinatorAbi>,
    /// When true the `Ptr` this symbol answers is a freshly allocated
    /// `String` the caller owns and must release; when false the pointer
    /// aliases storage the runtime or an argument still owns.
    ///
    /// A `Ptr` return says nothing about ownership on its own, so the drop
    /// pass reads this instead of inferring one. The runtime spells the same
    /// fact as `-> *mut c_char`, and a drift test holds the two together.
    pub mints_string: bool,
}

impl AbiType {
    /// LLVM IR type name for this ABI type.
    #[must_use]
    pub fn llvm_ir(self) -> &'static str {
        match self {
            AbiType::Void => "void",
            AbiType::I8 => "i8",
            AbiType::I32 => "i32",
            AbiType::I64 | AbiType::U64 => "i64",
            AbiType::I128 => "i128",
            AbiType::F64 => "double",
            AbiType::Ptr => "ptr",
        }
    }
}

impl RuntimeEntry {
    /// Produces the full `declare <ret> @<name>(<params>)` LLVM IR string.
    ///
    /// When `noreturn` is set the declaration gains `noreturn cold nounwind`
    /// attributes. LLVM uses these to classify the call site as a trap exit
    /// rather than a live successor, which lets the loop vectoriser treat
    /// loops with a guarded bounds check as effectively single-exit.
    #[must_use]
    pub fn llvm_declare(&self) -> String {
        self.llvm_declare_for(cfg!(windows))
    }

    /// `llvm_declare` parameterised on whether the target is Win64, so the
    /// platform-specific `i128` marshalling is unit-testable on any host.
    ///
    /// `Win64` marshals the 2-word `i128` (Fat) representation across the
    /// `extern "C"` boundary differently from a GP register pair: an `i128`
    /// *argument* is passed by pointer, and an `i128` *return* comes back in
    /// a 16-byte vector register (`<16 x i8>`). This matches how rustc
    /// lowers `i128` in an `extern "C"` signature on `x86_64-pc-windows`;
    /// emitting a bare `i128` makes llc pick the GP-pair ABI, which the
    /// Rust runtime does not use, corrupting every Result/Option crossing
    /// the boundary. The matching call-site marshalling lives in
    /// `lower_runtime_call_intrinsic` / `emit_named_call`. On `SysV`
    /// (Linux/macOS) bare `i128` already agrees between llc and rustc.
    #[must_use]
    pub fn llvm_declare_for(&self, win: bool) -> String {
        let param_ir = |t: &AbiType| -> &'static str {
            if win && *t == AbiType::I128 {
                "ptr"
            } else {
                t.llvm_ir()
            }
        };
        let ret_ir = if win && self.sig.ret == AbiType::I128 {
            "<16 x i8>"
        } else {
            self.sig.ret.llvm_ir()
        };
        let params = self
            .sig
            .params
            .iter()
            .map(param_ir)
            .collect::<Vec<_>>()
            .join(", ");
        if self.noreturn {
            // `noreturn` functions never return normally, but one that
            // may unwind (`gos_rt_panic` on the goroutine path) must NOT
            // be `nounwind` - that would abort the unwind at any cleanup
            // frame. LLVM permits `noreturn` together with a may-unwind
            // function.
            let tail = if self.unwinds {
                "noreturn cold"
            } else {
                "noreturn cold nounwind"
            };
            format!("declare {} @{}({}) {tail}", ret_ir, self.name, params)
        } else {
            // Every `gos_rt_*` symbol is an `extern "C"` Rust function, and
            // unwinding out of an `extern "C"` boundary aborts (Rust never
            // propagates a panic across it) - so the call cannot unwind. The
            // `nounwind` attribute makes that explicit to LLVM, which would
            // otherwise treat every runtime call as a potential exception
            // edge: that blocks reordering, hoisting (LICM), and CSE of the
            // surrounding loads/stores in every hot loop that calls a runtime
            // helper. (`willreturn`/`memory` are intentionally not blanket-
            // applied: a helper may abort on a Rust panic - not a return - and
            // most touch global allocator state.)
            //
            // A small audited allowlist of pure getters additionally gets
            // `memory(argmem: read)`: they only *read* memory reachable
            // through their pointer arguments (no writes, no global state).
            // This is what lets `opt` hoist a loop-invariant `graph[node]` /
            // `visited[nb]` read out of a loop and CSE repeated reads -
            // `nounwind` alone is insufficient because, without a memory-effect
            // bound, LLVM must assume the call clobbers all memory.
            let attrs = if self.unwinds {
                ""
            } else if PURE_ARGMEM_READ.contains(&self.name) {
                "nounwind memory(argmem: read)"
            } else if PURE_READ.contains(&self.name) {
                "nounwind memory(read)"
            } else {
                "nounwind"
            };
            let ret_attr = if NOALIAS_RET.contains(&self.name) {
                "noalias "
            } else {
                ""
            };
            format!(
                "declare {ret_attr}{} @{}({}) {attrs}",
                ret_ir, self.name, params
            )
        }
    }
}

/// Runtime getters whose every access lands in the allocation one of their
/// pointer arguments addresses - a `GosVec` header word, a string's own
/// content and index footer - with no writes and no global state. Marked
/// `memory(argmem: read)` so the optimiser can hoist and fold them across a
/// loop body that writes elsewhere. A reader that follows a pointer stored in
/// that allocation belongs in [`PURE_READ`] instead, and a helper that writes
/// or touches globals belongs in neither: a wrong entry is a miscompile.
const PURE_ARGMEM_READ: &[&str] = &[
    "gos_rt_vec_len",
    "gos_rt_str_len",
    "gos_rt_str_byte_at",
    "gos_rt_str_byte_len",
    "gos_rt_str_eq",
];

/// Runtime getters that read no memory the caller can write through a pointer
/// of its own, but whose reads are not confined to their arguments: an element
/// read follows the data pointer the header holds, and `gos_rt_arr_len`
/// consults the process argv statics. LLVM reads `argmem` as the memory
/// *based on* a pointer argument - derived from it by offset - which a buffer
/// loaded out of the argument is not, so these take the weaker
/// `memory(read)`: the optimiser may still fold repeated reads and hoist one
/// out of a loop that writes nothing, and a store to the buffer still orders
/// against them. Keep this list read-only: an entry that writes is a
/// miscompile.
const PURE_READ: &[&str] = &[
    "gos_rt_vec_get_i64",
    "gos_rt_vec_get_i64_unchecked",
    "gos_rt_vec_get_i128",
    "gos_rt_vec_get_opt",
    "gos_rt_vec_get_ptr",
    "gos_rt_vec_get_ptr_unchecked",
    "gos_rt_arr_len",
    "gos_rt_heap_i64_get",
];

/// Runtime allocators that return a FRESH, uniquely-owned heap allocation (a
/// `Box::into_raw`'d `GosVec` header). Like `malloc`, the returned pointer is
/// based on a distinct underlying object and cannot alias any pointer the
/// caller already holds, so the return is marked `noalias`. That lets LLVM
/// prove a store through an unrelated pointer (e.g. a stack fixed-array such as
/// radix-sort's `count[256]`) cannot clobber the vec's header, so it hoists the
/// loop-invariant `len`/data-pointer loads out of a hot element loop instead of
/// reloading the header on every access. Keep conservative: an entry that could
/// return a shared / non-fresh pointer would be a miscompile.
const NOALIAS_RET: &[&str] = &[
    "gos_rt_vec_new",
    "gos_rt_vec_new_typed",
    "gos_rt_vec_with_capacity",
    "gos_rt_vec_with_capacity_typed",
];

#[cfg(test)]
mod memory_effect_tests {
    use super::{PURE_ARGMEM_READ, PURE_READ};
    use crate::lookup;

    /// A getter claims `argmem` only when every read lands in the allocation
    /// an argument addresses; one that follows the header's data pointer, or
    /// reads a global, claims the weaker `memory(read)`.
    #[test]
    fn an_element_read_does_not_claim_argument_memory() {
        for name in ["gos_rt_vec_len", "gos_rt_str_byte_at"] {
            let decl = lookup(name).expect(name).llvm_declare_for(false);
            assert!(decl.contains("memory(argmem: read)"), "{name}: {decl}");
        }
        for name in ["gos_rt_vec_get_i64", "gos_rt_vec_get_ptr", "gos_rt_arr_len"] {
            let decl = lookup(name).expect(name).llvm_declare_for(false);
            assert!(decl.contains("memory(read)"), "{name}: {decl}");
            assert!(!decl.contains("argmem"), "{name}: {decl}");
        }
        for name in PURE_ARGMEM_READ.iter().chain(PURE_READ.iter()) {
            assert!(lookup(name).is_some(), "{name} is not a registry symbol");
        }
        for name in PURE_ARGMEM_READ {
            assert!(!PURE_READ.contains(name), "{name} is in both lists");
        }
    }
}
