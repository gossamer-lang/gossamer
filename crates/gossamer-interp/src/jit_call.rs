//! Trampoline that dispatches into a JIT-compiled body.
//!
//! Every call into native code goes through `invoke_prepared`: it inspects
//! the [`JitFn`]'s parameter and return kinds, marshals the VM's
//! boxed `Value`s into raw scalars, transmutes the function pointer
//! to a typed `extern "C"` callable, and calls it.
//!
//! Confining the raw-pointer dispatch here keeps the surface where
//! we have to reason about ABI safety down to a single module.
//!
//! # Safety invariants
//!
//! Every transmute below relies on the following invariants:
//! - `jit.ptr` was produced by `JITModule::get_finalized_function` before the
//!   module was dropped. The artifact still owns its native allocations, and
//!   the pointer names a body whose Cranelift signature exactly matches the
//!   chosen `extern "C" fn` shape - that match is guaranteed by
//!   `JitArtifact::compile_to_jit`'s own type classification, which
//!   only registers a [`JitFn`] when `JitKind` for every slot lines
//!   up with the MIR-derived cranelift type.
//! - The owning `JitArtifact` is still alive: the VM holds it in
//!   `Vm::_jit` for the entire lifetime of the `Global::Jit`
//!   entries that hand `JitFn`s to this module.
//! - The Gossamer language is single-threaded at the VM layer; the
//!   trampoline is therefore not re-entered from a foreign thread
//!   while a `JITed` body is running.
//!
//! Shapes the trampoline does not cover (e.g. heterogeneous mixes of
//! `i64`/`f64` beyond the listed patterns) return [`Dispatch::Fallback`]
//! so the caller can retry through the bytecode interpreter.

#![allow(unsafe_code)]
// Trampoline expands one arity-shape stub per `JitKind` permutation;
// the macro-generated dispatch keeps each shape in one place.
#![allow(clippy::too_many_lines)]

use std::ffi::c_char;
use std::mem;
use std::sync::Arc;

#[cfg(target_arch = "wasm32")]
use crate::jit_stub::{ArrayElem, JitFn, JitKind, TupleElem};
use gossamer_abi::jit_carrier::{CarrierNode, CarrierShape};
#[cfg(not(target_arch = "wasm32"))]
use gossamer_codegen_cranelift::{ArrayElem, JitFn, JitKind, TupleElem};
use gossamer_runtime::c_abi as rt;

use crate::value::{
    NativeEnumShape, NativeFieldKind, NativeFieldPlacement, NativeStructShape, SmolStr,
    StructInner, ThreadConfinedCell, Value, VariantInner, native_struct_shape,
};

/// One trampoline-owned native object built for an aggregate parameter,
/// recorded so it can be written back (for `&mut` params) and freed once
/// the JIT body returns. `cell` is `Some` only for a `&mut` argument
/// whose mutations the caller must observe.
type NativeArg = (JitKind, i64, Option<Arc<ThreadConfinedCell>>);

/// Builds a fresh, trampoline-owned native object for an aggregate
/// parameter from the VM value, returning its heap pointer as `i64`.
/// `None` when `value` is not a shape this kind can marshal (the caller
/// then falls back to bytecode).
fn build_native_arg(kind: JitKind, value: &Value) -> Option<i64> {
    let result = match kind {
        JitKind::NativeVecI64 => build_native_vec_i64(value),
        JitKind::NativeVecStr => build_native_vec_str(value),
        JitKind::NativeVecF64 => build_native_vec_f64(value),
        JitKind::NativeVecTupleIF => build_native_vec_tuple_if(value),
        JitKind::NativeStr => build_native_str(value),
        JitKind::U8VecHandle => build_native_u8vec(value),
        _ => None,
    };
    if result.is_some() {
        let bytes = match (kind, value) {
            (JitKind::NativeVecStr, Value::Array(values)) => values.len().saturating_mul(24),
            (JitKind::NativeVecI64, Value::IntArray(values)) => values.len().saturating_mul(8),
            (JitKind::NativeVecI64, Value::Array(values)) => values.len().saturating_mul(8),
            (JitKind::NativeVecF64, Value::FloatVec(values)) => values.len().saturating_mul(8),
            (JitKind::NativeVecF64, Value::FloatArray(values)) => {
                values.data.len().saturating_mul(8)
            }
            (JitKind::NativeVecF64, Value::Array(values)) => values.len().saturating_mul(8),
            (JitKind::NativeVecTupleIF, Value::Array(values)) => values.len().saturating_mul(16),
            (JitKind::NativeStr, Value::String(value)) => value.len(),
            (JitKind::U8VecHandle, _) => {
                crate::builtins::u8vec_snapshot_bytes(value).map_or(0, |bytes| bytes.len())
            }
            _ => 0,
        };
        rt::ledger::benchmark_boundary_copy(bytes);
    }
    result
}

/// Builds an owned `*mut GosVec` of 8-byte `f64` slots from a VM float
/// vector. Returns the pointer as `i64` (trampoline-owned).
fn build_native_vec_f64(value: &Value) -> Option<i64> {
    // SAFETY: `gos_rt_vec_new_typed` returns an owned header or null;
    // `gos_rt_vec_push` copies 8 bytes from the float's bit pattern.
    unsafe {
        // Same one-copy path as the integer case: a `FloatVec` is a packed
        // `Vec<f64>`, whose in-memory form is the native element layout.
        if let Value::FloatVec(arc) = value {
            let len = i64::try_from(arc.len()).ok()?;
            let v = rt::gos_rt_vec_from_packed_arr(8, arc.as_ptr().cast::<u8>(), len);
            return (!v.is_null()).then_some(v as i64);
        }
        let v = rt::gos_rt_vec_new_typed(8, rt::vec::vec_elem_kind::PRIMITIVE);
        if v.is_null() {
            return None;
        }
        let push_bits = |bits: u64| {
            let b = bits.to_ne_bytes();
            rt::gos_rt_vec_push(v, b.as_ptr());
        };
        match value {
            // A `Vec<f64>` is stored flat as `FloatVec`, and an array of an
            // all-f64 struct as `FloatArray`; both back their data with a
            // `Vec<f64>` the runtime can copy element-wise. Mirroring the
            // `IntArray` arm of `build_native_vec_i64`, these are the shapes a
            // `Vec<f64>` argument actually arrives as - without them every
            // `Vec<f64>` param falls back per call and the body demotes.
            Value::FloatVec(arc) => {
                for &x in arc.iter() {
                    push_bits(x.to_bits());
                }
            }
            Value::FloatArray(inner) => {
                for &x in inner.data.iter() {
                    push_bits(x.to_bits());
                }
            }
            Value::Array(arc) => {
                for elem in arc.iter() {
                    match elem {
                        Value::Float(x) => push_bits(x.to_bits()),
                        Value::Int(n) => push_bits((*n as f64).to_bits()),
                        _ => {
                            rt::gos_rt_vec_free(v);
                            return None;
                        }
                    }
                }
            }
            _ => {
                rt::gos_rt_vec_free(v);
                return None;
            }
        }
        Some(v as i64)
    }
}

/// Builds an owned `*mut GosVec` of 16-byte `(i64, f64)` tuple slots
/// (`[i64 @ +0][f64 @ +8]`, the compiled-tier layout) from a VM vector of
/// 2-tuples. Returns the pointer as `i64` (trampoline-owned).
fn build_native_vec_tuple_if(value: &Value) -> Option<i64> {
    // SAFETY: 16-byte primitive elements, no heap children; the runtime
    // copies each 16-byte slot by value and frees the buffer wholesale.
    unsafe {
        let vec_ptr = rt::gos_rt_vec_new_typed(16, rt::vec::vec_elem_kind::PRIMITIVE);
        if vec_ptr.is_null() {
            return None;
        }
        let Value::Array(arc) = value else {
            rt::gos_rt_vec_free(vec_ptr);
            return None;
        };
        for elem in arc.iter() {
            let Value::Tuple(tuple) = elem else {
                rt::gos_rt_vec_free(vec_ptr);
                return None;
            };
            let (Some(Value::Int(ival)), Some(second)) = (tuple.first(), tuple.get(1)) else {
                rt::gos_rt_vec_free(vec_ptr);
                return None;
            };
            let fbits = match second {
                Value::Float(fval) => fval.to_bits(),
                Value::Int(n) => (*n as f64).to_bits(),
                _ => {
                    rt::gos_rt_vec_free(vec_ptr);
                    return None;
                }
            };
            let mut slot = [0u8; 16];
            slot[0..8].copy_from_slice(&ival.to_ne_bytes());
            slot[8..16].copy_from_slice(&fbits.to_ne_bytes());
            rt::gos_rt_vec_push(vec_ptr, slot.as_ptr());
        }
        Some(vec_ptr as i64)
    }
}

/// Builds an owned native `Vec<Vec<i64>>`: an outer `*mut GosVec` tagged
/// `vec_elem_kind::VEC` (8-byte pointer slots) whose i-th slot holds a
/// pointer to a fresh inner `*mut GosVec` of i64 - the exact nested layout
/// the AOT tier uses, so the JIT body reads `graph[node]` (an inner `GosVec`
/// pointer) and iterates it identically to compiled code. Returns the outer
/// pointer as `i64` (RC = 1, owner-allocated). One `gos_rt_vec_free` of the
/// outer recursively reclaims every inner vec (the `VEC` element kind drives
/// the deep free). `None` if any element isn't an int-vec - the caller then
/// falls back to bytecode.
fn build_native_vec_vec_i64(elems: &[Value]) -> Option<i64> {
    // SAFETY: `gos_rt_vec_new_typed` returns an owned `VEC`-kind header or
    // null; each push copies one 8-byte inner-`GosVec` pointer into a slot.
    // On any failure the partial outer is freed, which recursively frees the
    // inner vecs already pushed - no leak, no double free.
    unsafe {
        let outer = rt::gos_rt_vec_new_typed(8, rt::vec::vec_elem_kind::VEC);
        if outer.is_null() {
            return None;
        }
        for elem in elems {
            let Some(inner_ptr) = build_native_vec_i64(elem) else {
                rt::gos_rt_vec_free(outer);
                return None;
            };
            let slot = inner_ptr.to_ne_bytes();
            rt::gos_rt_vec_push(outer, slot.as_ptr());
        }
        Some(outer as i64)
    }
}

/// Builds a fresh native `*mut GosU8Vec` from a VM `U8Vec`'s registry
/// bytes. The bytecode VM and native code use different `U8Vec` backings,
/// so the trampoline copies the bytes in (and copies them back after the
/// call - see `invoke_prepared_native`). Returns the pointer as `i64`.
fn build_native_u8vec(value: &Value) -> Option<i64> {
    let bytes = crate::builtins::u8vec_snapshot_bytes(value)?;
    // SAFETY: `gos_rt_heap_u8_new` returns an owned `*mut GosU8Vec` whose
    // `data` is a live buffer of exactly `len` bytes, so the whole snapshot
    // is one copy into it rather than a runtime call per byte.
    unsafe {
        let v = rt::gos_rt_heap_u8_new(bytes.len() as i64);
        if v.is_null() {
            return None;
        }
        let header = &*v;
        if !bytes.is_empty() && !header.data.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), header.data, bytes.len());
        }
        Some(v as i64)
    }
}

/// Reads a native `*mut GosU8Vec`'s bytes back so they can be written into
/// the VM `U8Vec`'s registry buffer (the body's in-place mutations).
fn read_native_u8vec(ptr: i64) -> Vec<u8> {
    if ptr == 0 {
        return Vec::new();
    }
    let v = ptr as *const rt::GosU8Vec;
    // SAFETY: `v` is a live `GosU8Vec` the trampoline built, so `data`
    // addresses exactly `len` initialised bytes and the whole buffer comes
    // back as one copy instead of a runtime call per byte.
    let bytes = unsafe {
        let header = &*v;
        let len = usize::try_from(header.len.max(0)).unwrap_or(0);
        if len == 0 || header.data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(header.data, len).to_vec()
        }
    };
    rt::ledger::benchmark_boundary_copy(bytes.len());
    bytes
}

/// Builds an owned `*mut GosVec` of 8-byte `i64` slots from a VM integer
/// vector. Returns the pointer as `i64` (RC = 1, trampoline-owned).
/// Builds a `STRING`-kind `GosVec` whose slots are owned cstrings, the layout
/// both compiled tiers give a `Vec<String>`. Answers `None` for a sequence
/// holding anything else, which keeps the body on bytecode.
fn build_native_vec_str(value: &Value) -> Option<i64> {
    let Value::Array(arc) = value else {
        return None;
    };
    // SAFETY: a fresh owned header; each push moves in a cstring the vec's
    // `STRING` element kind makes it responsible for freeing.
    unsafe {
        let v = rt::gos_rt_vec_new_typed(8, rt::vec::vec_elem_kind::STRING);
        if v.is_null() {
            return None;
        }
        for elem in arc.iter() {
            let Value::String(text) = elem else {
                rt::gos_rt_vec_free(v);
                return None;
            };
            let Some(ptr) = build_native_str(&Value::String(text.clone())) else {
                rt::gos_rt_vec_free(v);
                return None;
            };
            rt::gos_rt_vec_push_i64(v, ptr);
        }
        Some(v as i64)
    }
}

fn build_native_vec_i64(value: &Value) -> Option<i64> {
    // SAFETY: `gos_rt_vec_new_typed` returns an owned header (RC = 1) or
    // null; `gos_rt_vec_push_i64` copies each value into the buffer. We
    // own the result until `free_native` reclaims it.
    unsafe {
        // A packed integer vector is already the element layout the native
        // side expects, so it crosses as one copy rather than a call per
        // element - the boundary cost of a large argument is then a `memcpy`,
        // not a loop over the runtime ABI.
        if let Value::IntArray(arc) = value {
            let len = i64::try_from(arc.len()).ok()?;
            let v = rt::gos_rt_vec_from_packed_arr(8, arc.as_ptr().cast::<u8>(), len);
            return (!v.is_null()).then_some(v as i64);
        }
        let v = rt::gos_rt_vec_new_typed(8, rt::vec::vec_elem_kind::PRIMITIVE);
        if v.is_null() {
            return None;
        }
        // Only a boxed element sequence remains: its elements need
        // conversion, not copying, so they still cross one at a time.
        let Value::Array(arc) = value else {
            rt::gos_rt_vec_free(v);
            return None;
        };
        for elem in arc.iter() {
            match elem {
                Value::Int(n) => rt::gos_rt_vec_push_i64(v, *n),
                Value::Bool(b) => rt::gos_rt_vec_push_i64(v, i64::from(*b)),
                Value::Char(c) => rt::gos_rt_vec_push_i64(v, *c as i64),
                _ => {
                    rt::gos_rt_vec_free(v);
                    return None;
                }
            }
        }
        Some(v as i64)
    }
}

/// Builds an owned `*mut c_char` cstring from a VM string. Returns the
/// pointer as `i64` (trampoline-owned).
fn build_native_str(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => Some(rt::alloc_cstring(s.as_str().as_bytes()) as i64),
        _ => None,
    }
}

/// RC child-layout kind for an enum node (mirrors `gossamer_abi::rc::RC_KIND_ENUM`).
const RC_KIND_ENUM: i64 = 0;

thread_local! {
    /// Per-shape RC child-layout descriptor, built once and leaked so the
    /// pointer stays stable across calls (`gos_rt_rc_alloc_tagged` interns
    /// the meta by pointer).
    static ENUM_META_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<u32, &'static [i64]>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Builds (and caches) the child-layout descriptor a native enum node of
/// `shape` needs - `[RC_KIND_ENUM, n_variants, (disc, n_children,
/// child_slot...)...]` - so the runtime retains / releases the `Str` and
/// `Enum` children of each variant.
fn enum_shape_meta(shape: &crate::value::NativeEnumShape) -> &'static [i64] {
    ENUM_META_CACHE.with(|cache| {
        if let Some(m) = cache.borrow().get(&shape.index) {
            return *m;
        }
        let mut meta: Vec<i64> = vec![RC_KIND_ENUM, shape.variants.len() as i64];
        for (disc, v) in shape.variants.iter().enumerate() {
            let children: Vec<i64> = v
                .fields
                .iter()
                .enumerate()
                .filter(|(_, k)| {
                    matches!(
                        k,
                        crate::value::NativeFieldKind::Str | crate::value::NativeFieldKind::Enum(_)
                    )
                })
                .map(|(i, _)| i as i64)
                .collect();
            meta.push(disc as i64);
            meta.push(children.len() as i64);
            meta.extend(children);
        }
        let leaked: &'static [i64] = Box::leak(meta.into_boxed_slice());
        cache.borrow_mut().insert(shape.index, leaked);
        leaked
    })
}

/// Marshals a bytecode `Value::Variant` into a freshly allocated native
/// (compiled-representation) enum node so it can cross the JIT boundary,
/// returning the tagged native pointer (strong count 1, caller-owned), or
/// `None` if any field isn't marshallable (caller then falls back to
/// bytecode). One `gos_rt_rc_release` of the returned pointer reclaims the
/// node and every child it recursively built.
pub(crate) fn build_variant_to_native_enum(
    inner: &VariantInner,
    shape: &NativeEnumShape,
) -> Option<i64> {
    build_variant_to_native_enum_inner(inner, shape, false).map(|built| built.ptr)
}

pub(crate) struct NativeEnumBuild {
    pub(crate) ptr: i64,
    pub(crate) exclusive: bool,
    #[allow(
        dead_code,
        reason = "the moving-transfer path is reached only from the native-enum builder a target may not compile"
    )]
    actions: Vec<NativeFieldAction>,
}

impl NativeEnumBuild {
    #[allow(
        dead_code,
        reason = "the moving-transfer path is reached only from the native-enum builder a target may not compile"
    )]
    pub(crate) fn apply_to_fields(self, inner: &mut VariantInner) -> (i64, bool) {
        for action in self.actions {
            match action {
                NativeFieldAction::DropOriginal(i) => {
                    inner.fields[i] = Value::Unit;
                }
                NativeFieldAction::TransferOriginal(i) => {
                    let old = std::mem::replace(&mut inner.fields[i], Value::Unit);
                    std::mem::forget(old);
                }
            }
        }
        (self.ptr, self.exclusive)
    }
}

#[derive(Clone, Copy)]
#[allow(
    dead_code,
    reason = "the moving-transfer path is reached only from the native-enum builder a target may not compile"
)]
enum NativeFieldAction {
    DropOriginal(usize),
    TransferOriginal(usize),
}

#[derive(Clone, Copy)]
enum BuiltFieldOwnership {
    Scalar,
    FreshOwned,
    RetainedOne,
    BorrowedForTransfer,
}

struct BuiltField {
    kind: NativeFieldKind,
    word: i64,
    ownership: BuiltFieldOwnership,
}

#[allow(
    dead_code,
    reason = "the moving-transfer path is reached only from the native-enum builder a target may not compile"
)]
pub(crate) fn build_variant_to_native_enum_moving(
    inner: &VariantInner,
    shape: &NativeEnumShape,
) -> Option<NativeEnumBuild> {
    build_variant_to_native_enum_inner(inner, shape, true)
}

fn build_variant_to_native_enum_inner(
    inner: &VariantInner,
    shape: &NativeEnumShape,
    transfer_unique: bool,
) -> Option<NativeEnumBuild> {
    use crate::value::NativeFieldKind;
    let disc = shape
        .variants
        .iter()
        .position(|v| v.name == inner.name.as_str())?;
    let vshape = &shape.variants[disc];
    if vshape.fields.len() != inner.fields.len() {
        return None;
    }
    let nfields = vshape.fields.len();
    if nfields == 0 {
        // Unit variant: tagged repr is disc-in-pointer over a null base;
        // header repr is a shared immortal singleton. Neither needs freeing.
        return if shape.tagged {
            Some(NativeEnumBuild {
                ptr: (disc as i64) << 1,
                exclusive: true,
                actions: Vec::new(),
            })
        } else {
            let p = rt::gos_rt_enum_unit(disc as i64);
            (!p.is_null()).then_some(NativeEnumBuild {
                ptr: p as i64,
                exclusive: true,
                actions: Vec::new(),
            })
        };
    }
    let meta = enum_shape_meta(shape);
    let size = (nfields * 8) as u64;
    // SAFETY: `meta` describes the layout and `size` matches the slot count.
    // The interpreter never holds an arena region active, so this takes the
    // headered global path the disc-byte and child writes below assume.
    let payload = unsafe { rt::gos_rt_rc_alloc_tagged(size, meta.as_ptr()) };
    if payload.is_null() {
        return None;
    }
    let base = payload as usize;
    // The tagged alloc is unzeroed; zero the slots so a mid-build bail
    // releases only real children, never stack garbage.
    // SAFETY: `nfields * 8` bytes were just allocated at `base`.
    unsafe { std::ptr::write_bytes(base as *mut u8, 0, nfields * 8) };
    // Header disc byte: read by `visit_children_raw` to pick the variant's
    // child slots (and by `native_enum_disc` for header-repr reads).
    // SAFETY: headered global node; the disc byte lives at payload-3.
    unsafe { *((base - 3) as *mut u8) = disc as u8 };
    // Build every field word into a local buffer first; commit to the
    // node's slots only once all succeed. On a mid-build bail the words
    // already built are freed per their kind (the node-meta release reaches
    // only `Str` / `Enum` children, never the `Vec` fields), then the
    // all-zero node is released - no leak, no double free.
    let mut fields: Vec<BuiltField> = Vec::with_capacity(nfields);
    let mut actions: Vec<NativeFieldAction> = Vec::new();
    let mut exclusive = true;
    for (i, kind) in vshape.fields.iter().enumerate() {
        let built: Option<BuiltField> = match (kind, &inner.fields[i]) {
            (NativeFieldKind::I64, Value::Int(n)) => Some(BuiltField {
                kind: *kind,
                word: *n,
                ownership: BuiltFieldOwnership::Scalar,
            }),
            (NativeFieldKind::I64, Value::Uint(u)) => Some(BuiltField {
                kind: *kind,
                word: *u as i64,
                ownership: BuiltFieldOwnership::Scalar,
            }),
            (NativeFieldKind::F64, Value::Float(f)) => Some(BuiltField {
                kind: *kind,
                word: f.to_bits() as i64,
                ownership: BuiltFieldOwnership::Scalar,
            }),
            (NativeFieldKind::Bool, Value::Bool(b)) => Some(BuiltField {
                kind: *kind,
                word: i64::from(*b),
                ownership: BuiltFieldOwnership::Scalar,
            }),
            (NativeFieldKind::Char, Value::Char(c)) => Some(BuiltField {
                kind: *kind,
                word: *c as i64,
                ownership: BuiltFieldOwnership::Scalar,
            }),
            (NativeFieldKind::Str, Value::String(s)) => Some(BuiltField {
                kind: *kind,
                word: rt::alloc_cstring(s.as_str().as_bytes()) as i64,
                ownership: BuiltFieldOwnership::FreshOwned,
            }),
            (NativeFieldKind::Enum(sidx), Value::Variant(child)) => {
                let child = crate::value::native_shape(*sidx)
                    .and_then(|cs| build_variant_to_native_enum_inner(child, &cs, false));
                child.map(|built| {
                    exclusive &= built.exclusive;
                    BuiltField {
                        kind: *kind,
                        word: built.ptr,
                        ownership: BuiltFieldOwnership::FreshOwned,
                    }
                })
            }
            (NativeFieldKind::Enum(_), Value::NativeEnum(h)) => {
                let base = h.ptr & !7;
                let unique_native = base != 0
                    && Arc::strong_count(h) == 1
                    && unsafe { rt::gos_rt_rc_strong_count(base as *mut u8) } == 1;
                if transfer_unique && unique_native {
                    actions.push(NativeFieldAction::TransferOriginal(i));
                    Some(BuiltField {
                        kind: *kind,
                        word: h.ptr as i64,
                        ownership: BuiltFieldOwnership::BorrowedForTransfer,
                    })
                } else {
                    // SAFETY: co-owning an already-native child; retain so the
                    // parent's release balances it.
                    unsafe { rt::gos_rt_rc_retain(h.ptr as *mut u8) };
                    actions.push(NativeFieldAction::DropOriginal(i));
                    exclusive = false;
                    Some(BuiltField {
                        kind: *kind,
                        word: h.ptr as i64,
                        ownership: BuiltFieldOwnership::RetainedOne,
                    })
                }
            }
            (NativeFieldKind::VecEnum(eidx), Value::Array(arc)) => {
                exclusive = false;
                marshal_vec_enum(arc, *eidx).map(|word| BuiltField {
                    kind: *kind,
                    word,
                    ownership: BuiltFieldOwnership::FreshOwned,
                })
            }
            (NativeFieldKind::VecStrEnumTuple(eidx), Value::Array(arc)) => {
                exclusive = false;
                marshal_vec_str_enum(arc, *eidx).map(|word| BuiltField {
                    kind: *kind,
                    word,
                    ownership: BuiltFieldOwnership::FreshOwned,
                })
            }
            _ => None,
        };
        let Some(built) = built else {
            for built in &fields {
                free_built_field(built.kind, built.word, built.ownership);
            }
            // SAFETY: live node whose slots are all still zero; the
            // children built so far were freed above.
            unsafe { rt::gos_rt_rc_release(payload) };
            return None;
        };
        fields.push(built);
    }
    for (i, built) in fields.iter().enumerate() {
        // SAFETY: slot `i` is within the just-allocated payload.
        unsafe { *((base + i * 8) as *mut i64) = built.word };
    }
    let ptr = if shape.tagged {
        (base as i64) | ((disc as i64) << 1)
    } else {
        base as i64
    };
    Some(NativeEnumBuild {
        ptr,
        exclusive,
        actions,
    })
}

/// Marshals a VM `Value::Array` of heap-enum children into a native
/// `Vec<E>` field (a `*mut GosVec` of 8-byte `PRIMITIVE` slots, each a
/// native enum pointer - the compiled-tier `Vec<Enum>` byte layout
/// [`crate::value::native_vec_enum_to_array`] reads and
/// [`free_native_vec_enum`] frees). Returns the vec pointer as `i64`, or
/// `None` (freeing any partial state) if an element isn't a marshallable
/// enum.
fn marshal_vec_enum(elems: &[Value], eidx: u32) -> Option<i64> {
    let eshape = crate::value::native_shape(eidx)?;
    // SAFETY: an owned `PRIMITIVE` 8-byte-slot vec; each push copies one
    // 8-byte native-enum pointer. On any element failure the partial vec
    // is freed (children + buffer) before returning `None`.
    unsafe {
        let v = rt::gos_rt_vec_new_typed(8, rt::vec::vec_elem_kind::PRIMITIVE);
        if v.is_null() {
            return None;
        }
        for elem in elems {
            let child = match elem {
                Value::Variant(inner) => build_variant_to_native_enum(inner, &eshape),
                // A live VM node is marshalled as a FRESH, exclusively-owned
                // native copy (round-trip through a Variant), never an alias:
                // the vec then owns every element outright, so teardown is a
                // uniform drain-to-zero with no shared-node double free and the
                // VM keeps its own node untouched.
                Value::NativeEnum(h) if h.shape.index == eidx => {
                    match crate::value::native_enum_to_variant(h) {
                        Value::Variant(inner) => build_variant_to_native_enum(&inner, &eshape),
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some(cptr) = child else {
                free_native_vec_enum(v as i64, eidx);
                return None;
            };
            let slot = cptr.to_ne_bytes();
            rt::gos_rt_vec_push(v, slot.as_ptr());
        }
        Some(v as i64)
    }
}

/// Marshals a VM `Value::Array` of `(String, E)` 2-tuples into a native
/// `Vec<(String, E)>` field (a `*mut GosVec` of 16-byte `PRIMITIVE` slots
/// laid out `[*c_char @ +0][native-enum ptr @ +8]` - the compiled-tier
/// `Vec<(String, Enum)>` byte layout [`crate::value::native_vec_str_enum_to_array`]
/// reads and [`free_native_vec_str_enum`] frees). Returns the vec pointer as
/// `i64`, or `None` (freeing any partial state) if an element isn't a
/// marshallable `(String, enum)` pair.
fn marshal_vec_str_enum(elems: &[Value], eidx: u32) -> Option<i64> {
    let eshape = crate::value::native_shape(eidx)?;
    // SAFETY: an owned 16-byte-slot vec; each push copies one
    // `[cstr][enum]` slot. On any element failure the partial vec is freed
    // (children + buffer) before returning `None`.
    unsafe {
        let v = rt::gos_rt_vec_new_typed(16, rt::vec::vec_elem_kind::PRIMITIVE);
        if v.is_null() {
            return None;
        }
        for elem in elems {
            let Value::Tuple(t) = elem else {
                free_native_vec_str_enum(v as i64, eidx);
                return None;
            };
            let (Some(Value::String(key)), Some(vval)) = (t.first(), t.get(1)) else {
                free_native_vec_str_enum(v as i64, eidx);
                return None;
            };
            let key_ptr = rt::alloc_cstring(key.as_str().as_bytes()) as i64;
            let child = match vval {
                Value::Variant(inner) => build_variant_to_native_enum(inner, &eshape),
                // Fresh exclusively-owned native copy of a live VM node (never
                // an alias) - see `marshal_vec_enum` for the ownership rationale.
                Value::NativeEnum(h) if h.shape.index == eidx => {
                    match crate::value::native_enum_to_variant(h) {
                        Value::Variant(inner) => build_variant_to_native_enum(&inner, &eshape),
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some(val_ptr) = child else {
                rt::gos_rt_str_free(key_ptr as *mut c_char);
                free_native_vec_str_enum(v as i64, eidx);
                return None;
            };
            let mut slot = [0u8; 16];
            slot[0..8].copy_from_slice(&key_ptr.to_ne_bytes());
            slot[8..16].copy_from_slice(&val_ptr.to_ne_bytes());
            rt::gos_rt_vec_push(v, slot.as_ptr());
        }
        Some(v as i64)
    }
}

/// Frees one native field word built by [`build_variant_to_native_enum`],
/// per its kind. Used only on the mid-build bail path (the node-meta
/// release cannot reach `Vec` fields). Scalars own nothing and are no-ops.
fn free_built_field(kind: NativeFieldKind, word: i64, ownership: BuiltFieldOwnership) {
    match (kind, ownership) {
        (_, BuiltFieldOwnership::Scalar | BuiltFieldOwnership::BorrowedForTransfer) => {}
        (NativeFieldKind::Enum(_), BuiltFieldOwnership::RetainedOne) => {
            let base = (word as usize) & !7;
            if base != 0 {
                // SAFETY: this balances exactly the retain taken while building
                // the parent field. The original VM handle still owns its ref.
                unsafe { rt::gos_rt_rc_release(base as *mut u8) };
            }
        }
        (NativeFieldKind::Str, BuiltFieldOwnership::FreshOwned) => {
            if word != 0 {
                // SAFETY: a live cstring built for this `Str` field.
                unsafe { rt::gos_rt_str_free(word as *mut c_char) };
            }
        }
        (NativeFieldKind::Enum(eidx), BuiltFieldOwnership::FreshOwned) => {
            if let Some(s) = crate::value::native_shape(eidx) {
                free_native_enum(word, &s);
            }
        }
        (NativeFieldKind::VecEnum(eidx), BuiltFieldOwnership::FreshOwned) => {
            free_native_vec_enum(word, eidx);
        }
        (NativeFieldKind::VecStrEnumTuple(eidx), BuiltFieldOwnership::FreshOwned) => {
            free_native_vec_str_enum(word, eidx);
        }
        (
            NativeFieldKind::I64
            | NativeFieldKind::F64
            | NativeFieldKind::Bool
            | NativeFieldKind::Char
            | NativeFieldKind::Str
            | NativeFieldKind::VecEnum(_)
            | NativeFieldKind::VecStrEnumTuple(_),
            _,
        ) => {}
    }
}

/// Recursively frees a native `Vec<E>` field word (a `*mut GosVec` of 8-byte
/// native-enum pointer slots): each element enum is released by its whole
/// strong count, then the `PRIMITIVE` buffer is shallow-freed.
fn free_native_vec_enum(word: i64, eidx: u32) {
    if word == 0 {
        return;
    }
    let v = word as *mut rt::vec::GosVec;
    if let Some(s) = crate::value::native_shape(eidx) {
        // SAFETY: live `GosVec` of 8-byte enum-pointer slots.
        let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
        for j in 0..len {
            let elem = unsafe { rt::gos_rt_vec_get_i64(v, j) };
            free_native_enum(elem, &s);
        }
    }
    // SAFETY: owns this `PRIMITIVE` vec; shallow-frees the buffer (elements
    // were freed above).
    unsafe { rt::gos_rt_vec_free(v) };
}

/// Recursively frees a native `Vec<(String, E)>` field word (a `*mut GosVec`
/// of 16-byte `[cstr][enum]` slots): each key cstring and element enum is
/// freed, both slot words are nulled, then the buffer is reclaimed (the
/// nulled slots make any deep-free walk a no-op - no double free).
fn free_native_vec_str_enum(word: i64, eidx: u32) {
    if word == 0 {
        return;
    }
    let v = word as *mut rt::vec::GosVec;
    let s = crate::value::native_shape(eidx);
    // SAFETY: live `GosVec` of 16-byte `[cstr][enum]` slots.
    let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
    for j in 0..len {
        let p = unsafe { rt::gos_rt_vec_get_ptr(v, j) };
        if p.is_null() {
            continue;
        }
        // SAFETY: 16-byte slot: cstring word at +0, enum pointer at +8.
        let key_word = unsafe { p.cast::<i64>().read_unaligned() };
        if key_word != 0 {
            unsafe { rt::gos_rt_str_free(key_word as *mut c_char) };
        }
        let val_word = unsafe { p.add(8).cast::<i64>().read_unaligned() };
        if let Some(s) = &s {
            free_native_enum(val_word, s);
        }
        // SAFETY: writing slot words of a vec we own.
        unsafe {
            p.cast::<i64>().write_unaligned(0);
            p.add(8).cast::<i64>().write_unaligned(0);
        }
    }
    // SAFETY: owns this vec; with slots nulled its own free walks nothing.
    unsafe { rt::gos_rt_vec_free(v) };
}

/// Releases the native enum temporaries built to cross the JIT boundary,
/// on every exit path of a dispatch (successful call or mid-marshal
/// fallback). The JIT body never frees its enum args (its `Drop` terminator
/// is a no-op), so the trampoline owns each temporary end to end.
struct BuiltEnums(Vec<i64>);

impl Drop for BuiltEnums {
    fn drop(&mut self) {
        for &p in &self.0 {
            let payload = (p as usize) & !7;
            if payload != 0 {
                // SAFETY: each `p` is a node allocated with strong count 1
                // and not yet freed; release reclaims it and its children.
                unsafe { rt::gos_rt_rc_release(payload as *mut u8) };
            }
        }
    }
}

/// Owns a marshalled flat struct block and every heap child in its slots.
struct NativeStructBacking {
    slots: Box<[i64]>,
    shape: Arc<NativeStructShape>,
}

impl NativeStructBacking {
    fn as_ptr(&self) -> i64 {
        self.slots.as_ptr() as i64
    }
}

impl Drop for NativeStructBacking {
    fn drop(&mut self) {
        for (place, (_, kind)) in self.shape.placements.iter().zip(self.shape.fields.iter()) {
            let slot = read_struct_field(&self.slots, *place);
            if matches!(kind, NativeFieldKind::Str) && slot != 0 {
                // SAFETY: string slots are owned native strings built by
                // `build_native_struct` or written by a native body into this
                // trampoline-owned struct block.
                unsafe { free_native(JitKind::NativeStr, slot) };
            }
        }
    }
}

/// Encodes one VM value into its 8-byte slot, or `None` when the value does
/// not match the element class the parameter's type declares.
fn array_slot_word(value: &Value, class: ArrayElem) -> Option<i64> {
    match (class, value) {
        (ArrayElem::I64, Value::Int(n)) => Some(*n),
        (ArrayElem::I64, Value::Uint(u)) => Some(*u as i64),
        (ArrayElem::F64, Value::Float(f)) => Some(f.to_bits() as i64),
        (ArrayElem::F64, Value::Int(n)) => Some((*n as f64).to_bits() as i64),
        (ArrayElem::Char, Value::Char(c)) => Some(i64::from(u32::from(*c))),
        _ => None,
    }
}

/// Builds a flat block of `len` 8-byte slots from a VM array value - the
/// layout the compiled tier indexes by static stride. Declines any value
/// whose length or element shape does not match the parameter's type.
fn build_native_array_block(value: &Value, len: u32, class: ArrayElem) -> Option<Box<[i64]>> {
    let len = len as usize;
    let slots: Box<[i64]> = match value {
        Value::IntArray(values) if values.len() == len && class == ArrayElem::I64 => values
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        Value::FloatVec(values) if values.len() == len && class == ArrayElem::F64 => values
            .iter()
            .map(|f| f.to_bits() as i64)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        Value::FloatArray(values) if values.data.len() == len && class == ArrayElem::F64 => values
            .data
            .iter()
            .map(|f| f.to_bits() as i64)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        Value::Array(values) if values.len() == len => {
            let mut slots = Vec::with_capacity(len);
            for element in values.iter() {
                slots.push(array_slot_word(element, class)?);
            }
            slots.into_boxed_slice()
        }
        _ => return None,
    };
    Some(slots)
}

/// The word a struct field holds, widened from its placement's bytes.
fn read_struct_field(slots: &[i64], place: NativeFieldPlacement) -> i64 {
    let start = place.offset as usize;
    let end = start + usize::from(place.bytes);
    let mut bytes = [0u8; 8];
    for (i, byte) in bytes.iter_mut().enumerate().take(end - start) {
        let at = start + i;
        *byte = slots
            .get(at / 8)
            .map_or(0, |word| word.to_le_bytes()[at % 8]);
    }
    let raw = u64::from_le_bytes(bytes);
    match (place.bytes, place.signed) {
        (1, true) => i64::from(raw as u8 as i8),
        (2, true) => i64::from(raw as u16 as i16),
        (4, true) => i64::from(raw as u32 as i32),
        _ => raw as i64,
    }
}

/// Writes the low bytes of `word` a struct field's placement spans.
fn write_struct_field(slots: &mut [i64], place: NativeFieldPlacement, word: i64) {
    let start = place.offset as usize;
    for (i, byte) in word
        .to_le_bytes()
        .iter()
        .take(usize::from(place.bytes))
        .enumerate()
    {
        let at = start + i;
        if let Some(slot) = slots.get_mut(at / 8) {
            let mut le = slot.to_le_bytes();
            le[at % 8] = *byte;
            *slot = i64::from_le_bytes(le);
        }
    }
}

/// Marshals a supported `Value::Struct` into a freshly allocated flat
/// block laid out as the compiled tiers lay the struct out, with NO RC
/// header. String fields are
/// copied into owned native strings held by the backing block. The caller
/// passes the block pointer to the JIT body and keeps it alive across the
/// call; a `&mut self` body mutates its slots in place and those slots are
/// read back by [`read_native_struct`]. `None` means the value did not match
/// the registered struct shape, so the caller falls back to bytecode.
fn build_native_struct(
    value: &Value,
    shape: Arc<NativeStructShape>,
) -> Option<NativeStructBacking> {
    let Value::Struct(inner) = value else {
        return None;
    };
    if inner.fields.len() != shape.fields.len() {
        return None;
    }
    let mut slots = vec![0i64; shape.words].into_boxed_slice();
    for (i, (_, kind)) in shape.fields.iter().enumerate() {
        let word = match (kind, &inner.fields[i]) {
            (NativeFieldKind::I64, Value::Int(n)) => *n,
            (NativeFieldKind::I64, Value::Uint(u)) => *u as i64,
            (NativeFieldKind::F64, Value::Float(f)) => f.to_bits() as i64,
            (NativeFieldKind::F64, Value::Int(n)) => (*n as f64).to_bits() as i64,
            (NativeFieldKind::Bool, Value::Bool(b)) => i64::from(*b),
            (NativeFieldKind::Char, Value::Char(c)) => *c as i64,
            (NativeFieldKind::Str, Value::String(s)) => {
                rt::alloc_cstring(s.as_str().as_bytes()) as i64
            }
            // Other heap fields cannot reach a registered struct shape, and a
            // value whose kind does not match the field declines the marshal.
            _ => {
                free_native_struct_slots(&slots, &shape);
                return None;
            }
        };
        write_struct_field(&mut slots, shape.placements[i], word);
    }
    Some(NativeStructBacking { slots, shape })
}

fn free_native_struct_slots(slots: &[i64], shape: &NativeStructShape) {
    for (place, (_, kind)) in shape.placements.iter().zip(shape.fields.iter()) {
        let slot = read_struct_field(slots, *place);
        if matches!(kind, NativeFieldKind::Str) && slot != 0 {
            // SAFETY: only slots already written by `build_native_struct` are
            // non-zero here, and each is an owned native string.
            unsafe { free_native(JitKind::NativeStr, slot) };
        }
    }
}

/// Reads a native flat struct block back into an owned `Value::Struct`,
/// decoding each slot per the shape's field kind. Used after a `&mut self`
/// JIT call so the caller observes the body's in-place field mutations.
fn read_native_struct(ptr: i64, shape: &NativeStructShape) -> Value {
    if ptr == 0 {
        return Value::Unit;
    }
    // SAFETY: `ptr` is the trampoline-owned backing buffer of `shape.words`
    // initialised i64 slots.
    let slots = unsafe { std::slice::from_raw_parts(ptr as *const i64, shape.words) };
    let fields: Box<[(&'static str, Value)]> = shape
        .fields
        .iter()
        .zip(&shape.placements)
        .map(|((name, kind), place)| {
            let word = read_struct_field(slots, *place);
            let v = match kind {
                NativeFieldKind::I64 => Value::Int(word),
                NativeFieldKind::F64 => Value::Float(f64::from_bits(word as u64)),
                NativeFieldKind::Bool => Value::Bool(word != 0),
                NativeFieldKind::Char => {
                    Value::Char(char::from_u32(word as u32).unwrap_or('\u{0}'))
                }
                NativeFieldKind::Str => native_ptr_to_value(JitKind::NativeStr, word),
                NativeFieldKind::Enum(_)
                | NativeFieldKind::VecEnum(_)
                | NativeFieldKind::VecStrEnumTuple(_) => Value::Unit,
            };
            (*name, v)
        })
        .collect();
    Value::Struct(std::sync::Arc::new(StructInner {
        name: crate::value::intern_type_tag(shape.struct_name),
        fields: crate::value::StructFields::new(fields.into_vec()),
    }))
}

/// Recursively frees a uniquely-owned native heap-enum DOM (the value a
/// JIT-compiled `parse` allocated and returned): each `Vec` field's elements
/// and buffer, each tuple key cstring, each string field, and nested enum
/// nodes, then the node block itself. Every child slot is zeroed before the
/// node's own release, so the node-meta child walk finds null slots and frees
/// nothing twice - the explicit recursion (which alone reaches the
/// `PRIMITIVE`-tagged `Vec` elements the meta cannot) owns the teardown. The
/// DOM is uniquely owned (the parse result, never aliased), so each node is
/// released by its whole strong count, absorbing the leaked `?`-propagation
/// retains the compiled tier leaves on every extracted payload (the same
/// over-retention the AOT tier exhibits).
fn free_native_enum(ptr: i64, shape: &crate::value::NativeEnumShape) {
    use crate::value::native_enum_disc;
    let base = (ptr as usize) & !7;
    if base == 0 {
        return;
    }
    let disc = native_enum_disc(ptr as usize, shape);
    if let Some(variant) = shape.variants.get(disc) {
        for (i, kind) in variant.fields.iter().enumerate() {
            let slot = (base + i * 8) as *mut i64;
            // SAFETY: payload slot `i` is inside the node's allocation.
            let word = unsafe { *slot };
            match kind {
                NativeFieldKind::Str => {
                    if word != 0 {
                        // SAFETY: a live cstring built for this Str payload.
                        unsafe { rt::gos_rt_str_free(word as *mut c_char) };
                    }
                }
                NativeFieldKind::Enum(eidx) => {
                    if let Some(s) = crate::value::native_shape(*eidx) {
                        free_native_enum(word, &s);
                    }
                }
                NativeFieldKind::VecEnum(eidx) => free_native_vec_enum(word, *eidx),
                NativeFieldKind::VecStrEnumTuple(eidx) => {
                    free_native_vec_str_enum(word, *eidx);
                }
                NativeFieldKind::I64
                | NativeFieldKind::F64
                | NativeFieldKind::Bool
                | NativeFieldKind::Char => {}
            }
            // Null the slot so the node-meta release below skips this child
            // (it was already reclaimed above) - no double free.
            // SAFETY: writing a payload slot we own.
            unsafe { *slot = 0 };
        }
    }
    // Reclaim the node fully: release it by its whole strong count (>= 1).
    // Children were freed and nulled above, so the meta walk on the final
    // (count-reaching-zero) release frees nothing twice. A region / immortal
    // node reports count 0 and is released once as a harmless no-op.
    // SAFETY: `base` is a uniquely-owned node; releasing it to zero reclaims it.
    let strong = unsafe { rt::gos_rt_rc_strong_count(base as *mut u8) }.max(1);
    for _ in 0..strong {
        unsafe { rt::gos_rt_rc_release(base as *mut u8) };
    }
}

/// Carriers built for a call's parameters, each word pair at a stable address
/// the body reads through. Each holds one share of its payload and the body
/// only borrows it, so the shares are given back when the dispatch ends, by
/// whichever path it ends.
struct BuiltCarriers(Vec<(CarrierShape, Box<[i64; 2]>)>);

impl Drop for BuiltCarriers {
    fn drop(&mut self) {
        for (shape, words) in &self.0 {
            release_carrier_words(*shape, 0, words[0], words[1]);
        }
    }
}

/// The VM value of the carrier at node `at` of `shape` whose words are `disc`
/// and `payload`. Reads only: every share stays with the words.
fn carrier_to_value(shape: CarrierShape, at: usize, disc: i64, payload: i64) -> Value {
    let (present, absent) = if shape.node(at) == CarrierNode::Option {
        ("Some", "None")
    } else {
        ("Ok", "Err")
    };
    match shape.arm(at, disc) {
        Some(arm) => Value::variant(
            if disc == 0 { present } else { absent },
            vec![carrier_payload_to_value(shape, arm, payload)],
        ),
        None => Value::variant(absent, vec![]),
    }
}

/// The VM value of the payload word `word` whose node is `at`.
fn carrier_payload_to_value(shape: CarrierShape, at: usize, word: i64) -> Value {
    match shape.node(at) {
        CarrierNode::Unit => Value::Unit,
        CarrierNode::I64 => Value::Int(word),
        CarrierNode::F64 => Value::Float(f64::from_bits(word as u64)),
        CarrierNode::Bool => Value::Bool(word & 1 != 0),
        CarrierNode::Char => Value::Char(char::from_u32(word as u32).unwrap_or('\u{fffd}')),
        CarrierNode::Str => native_ptr_to_value(JitKind::NativeStr, word),
        CarrierNode::Error => read_native_error(word),
        CarrierNode::Option | CarrierNode::Result => {
            let inner = rt::gos_rt_carrier_from_box(word);
            carrier_to_value(shape, at, inner as i64, (inner >> 64) as i64)
        }
    }
}

/// Gives back the share the words of the carrier at node `at` hold on their
/// arm's payload: a string body, or the box of a nested carrier. A scalar
/// owns nothing, and the runtime keeps every error node for the process.
fn release_carrier_words(shape: CarrierShape, at: usize, disc: i64, payload: i64) {
    let Some(arm) = shape.arm(at, disc) else {
        return;
    };
    if payload == 0 {
        return;
    }
    match shape.node(arm) {
        // SAFETY: an arm whose node is a `String` holds a string body the
        // words own one share of.
        CarrierNode::Str => unsafe { free_native(JitKind::NativeStr, payload) },
        CarrierNode::Option | CarrierNode::Result => {
            let slot = [disc, payload];
            // SAFETY: `slot` is a live two-word carrier whose payload word is a
            // box; the release confirms the box is a counted blob first.
            unsafe { rt::gos_rt_option_slot_release(slot.as_ptr()) };
        }
        _ => {}
    }
}

/// Builds the two words of the carrier at node `at` of `shape` from a VM
/// value, each heap payload holding one fresh share. `metas` names the
/// counted-box layout of every nested carrier node. `None`, with nothing left
/// allocated, when the value is not of this shape.
fn build_native_carrier(
    shape: CarrierShape,
    metas: &[Option<&'static [i64]>],
    at: usize,
    value: &Value,
) -> Option<[i64; 2]> {
    let Value::Variant(variant) = value else {
        return None;
    };
    let is_option = shape.node(at) == CarrierNode::Option;
    let disc = match (is_option, variant.name.as_str()) {
        (true, "Some") | (false, "Ok") => 0,
        (true, "None") => return Some([1, 0]),
        (false, "Err") => 1,
        _ => return None,
    };
    let arm = shape.arm(at, disc)?;
    let payload = build_carrier_payload(shape, metas, arm, variant.fields.first()?)?;
    Some([disc, payload])
}

/// The payload word for node `at` built from `value`, holding a fresh share
/// when it is heap storage.
fn build_carrier_payload(
    shape: CarrierShape,
    metas: &[Option<&'static [i64]>],
    at: usize,
    value: &Value,
) -> Option<i64> {
    match (shape.node(at), value) {
        (CarrierNode::Unit, Value::Unit) => Some(0),
        (CarrierNode::I64, Value::Int(n)) => Some(*n),
        (CarrierNode::F64, Value::Float(x)) => Some(x.to_bits() as i64),
        (CarrierNode::Bool, Value::Bool(b)) => Some(i64::from(*b)),
        (CarrierNode::Char, Value::Char(c)) => Some(i64::from(u32::from(*c))),
        (CarrierNode::Str, Value::String(_)) => build_native_str(value),
        (CarrierNode::Option | CarrierNode::Result, _) => {
            let meta = metas.get(at).copied().flatten()?;
            let words = build_native_carrier(shape, metas, at, value)?;
            // SAFETY: `words` are two initialised words laid out as `meta`
            // describes; the box takes the shares they hold.
            let boxed =
                unsafe { rt::gos_rt_rc_alloc_move(16, meta.as_ptr(), words.as_ptr().cast()) };
            if boxed.is_null() {
                release_carrier_words(shape, at, words[0], words[1]);
                return None;
            }
            Some(boxed as i64)
        }
        _ => None,
    }
}

/// Reads a native `*mut GosError` (the `Err` payload of a `Result` return)
/// into the VM's `errors::Error` struct value. The native error's leaked
/// message copy is freed; the error node itself is left to process teardown
/// (the `Err` path is the cold branch and never aliases the trampoline's
/// owned inputs).
fn read_native_error(ptr: i64) -> Value {
    // A wrapped error is a chain, and `{}` prints every link colon-joined, so
    // the whole chain crosses back - a `cause: None` here would silently
    // shorten what an `errors::wrap` chain prints on this tier alone. The
    // nodes themselves stay owned by the runtime, which holds an error for
    // the life of the process.
    let message = native_error_message(ptr);
    let cause = if ptr == 0 {
        Value::variant("None", vec![])
    } else {
        // SAFETY: `ptr` is a live `*mut GosError`; the shim answers the
        // `Option<Error>` carrier for its cause.
        let carrier = unsafe { rt::gos_rt_error_cause(ptr as *const rt::GosError) };
        let disc = (carrier as u64) as i64;
        let payload = ((carrier as u128) >> 64) as u64 as i64;
        if disc == 0 && payload != 0 {
            Value::variant("Some", vec![read_native_error(payload)])
        } else {
            Value::variant("None", vec![])
        }
    };
    Value::struct_(
        "errors::Error",
        vec![
            ("message", Value::String(SmolStr::from_str(&message))),
            ("cause", cause),
        ],
    )
}

/// The top message of a native error, or the empty string for a null error
/// or one carrying no message.
fn native_error_message(ptr: i64) -> String {
    if ptr == 0 {
        return String::new();
    }
    // SAFETY: `ptr` is a live `*mut GosError`; `gos_rt_error_message`
    // returns a freshly leaked cstring copy of the top message or null.
    let c = unsafe { rt::gos_rt_error_message(ptr as *const rt::GosError) };
    if c.is_null() {
        return String::new();
    }
    // `gos_rt_str_len` counts Unicode scalars; the slice below is bytes, so
    // a message with any multibyte character would lose its tail.
    let len = unsafe { rt::gos_rt_str_byte_len(c) }.max(0) as usize;
    let bytes = unsafe { std::slice::from_raw_parts(c.cast::<u8>(), len) };
    let s = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: the message copy is owned by us; free it now.
    unsafe { rt::gos_rt_str_free(c) };
    s
}

/// Reads a native return pointer back into an owned VM value WITHOUT
/// freeing the native object (the caller frees it, deduped against the
/// params, so a body returning its own param frees exactly once).
fn native_ptr_to_value(kind: JitKind, ptr: i64) -> Value {
    match kind {
        // A flat block of exactly `len` slots: the length is part of the
        // parameter's type, so it is read from the kind rather than from
        // the block, which carries no header.
        JitKind::ArrayBlockPtr(len, class) => {
            if ptr == 0 {
                return Value::Array(Arc::new(Vec::new()));
            }
            // SAFETY: `ptr` addresses a block this call built with exactly
            // `len` initialised slots, live until the call returns.
            let slots = unsafe { std::slice::from_raw_parts(ptr as *const i64, len as usize) };
            if class == ArrayElem::I64 {
                return Value::IntArray(Arc::new(slots.to_vec()));
            }
            let elements = slots
                .iter()
                .map(|word| match class {
                    ArrayElem::I64 => Value::Int(*word),
                    ArrayElem::F64 => Value::Float(f64::from_bits(*word as u64)),
                    ArrayElem::Char => Value::Char(
                        u32::try_from(*word)
                            .ok()
                            .and_then(char::from_u32)
                            .unwrap_or('\0'),
                    ),
                })
                .collect::<Vec<_>>();
            Value::Array(Arc::new(elements))
        }
        JitKind::NativeVecStr => {
            if ptr == 0 {
                return Value::empty_array();
            }
            let v = ptr as *const rt::vec::GosVec;
            // SAFETY: `v` is a live `STRING`-kind `GosVec`; each slot holds a
            // cstring the vec owns, read here without taking that ownership.
            let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                let word = unsafe { rt::gos_rt_vec_get_i64(v, i) };
                out.push(native_ptr_to_value(JitKind::NativeStr, word));
            }
            Value::Array(Arc::new(out))
        }
        JitKind::NativeVecI64 => {
            if ptr == 0 {
                return Value::IntArray(Arc::new(Vec::new()));
            }
            let v = ptr as *const rt::vec::GosVec;
            // SAFETY: `v` is a live `GosVec` (a param we built or the
            // body's owned return); `len`/`get` read initialised slots.
            let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                out.push(unsafe { rt::gos_rt_vec_get_i64(v, i) });
            }
            Value::IntArray(Arc::new(out))
        }
        JitKind::NativeVecF64 => {
            if ptr == 0 {
                return Value::Array(Arc::new(Vec::new()));
            }
            let v = ptr as *const rt::vec::GosVec;
            // SAFETY: live `GosVec` of 8-byte slots; each slot's bits are a
            // valid `f64`.
            let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                let bits = unsafe { rt::gos_rt_vec_get_i64(v, i) } as u64;
                out.push(Value::Float(f64::from_bits(bits)));
            }
            Value::Array(Arc::new(out))
        }
        JitKind::NativeVecTupleIF => {
            if ptr == 0 {
                return Value::Array(Arc::new(Vec::new()));
            }
            let v = ptr as *const rt::vec::GosVec;
            // SAFETY: live `GosVec` of 16-byte `[i64][f64]` slots; the
            // element pointer is valid for the 16 bytes read here.
            let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                let p = unsafe { rt::gos_rt_vec_get_ptr(v, i) };
                if p.is_null() {
                    out.push(Value::Tuple(Arc::from(vec![
                        Value::Int(0),
                        Value::Float(0.0),
                    ])));
                    continue;
                }
                let a = unsafe { p.cast::<i64>().read_unaligned() };
                let b = unsafe { p.add(8).cast::<f64>().read_unaligned() };
                out.push(Value::Tuple(Arc::from(vec![
                    Value::Int(a),
                    Value::Float(b),
                ])));
            }
            Value::Array(Arc::new(out))
        }
        JitKind::NativeStr => {
            if ptr == 0 {
                return Value::String(SmolStr::default());
            }
            let s = ptr as *const c_char;
            // SAFETY: `s` is a live cstring; `gos_rt_str_byte_len` reads its
            // length header and the bytes are valid for that length. The
            // byte length is the one this slice needs - `gos_rt_str_len`
            // counts Unicode scalars, which is fewer for any multibyte text.
            let len = unsafe { rt::gos_rt_str_byte_len(s) }.max(0) as usize;
            let bytes = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) };
            Value::String(SmolStr::from_str(&String::from_utf8_lossy(bytes)))
        }
        JitKind::NativeVecVecI64 => {
            if ptr == 0 {
                return Value::Array(Arc::new(Vec::new()));
            }
            let outer = ptr as *const rt::vec::GosVec;
            // SAFETY: `outer` is a live `VEC`-kind `GosVec` whose 8-byte slots
            // each hold an inner `*mut GosVec` of i64 - the layout
            // `build_native_vec_vec_i64` builds and the body returns.
            let len = unsafe { rt::gos_rt_vec_len(outer) }.max(0);
            let mut rows = Vec::with_capacity(len as usize);
            for i in 0..len {
                let slot = unsafe { rt::gos_rt_vec_get_ptr(outer, i) };
                if slot.is_null() {
                    rows.push(Value::IntArray(Arc::new(Vec::new())));
                    continue;
                }
                // The slot stores the inner `GosVec` pointer by value.
                let inner = unsafe { slot.cast::<*const rt::vec::GosVec>().read_unaligned() };
                rows.push(native_ptr_to_value(JitKind::NativeVecI64, inner as i64));
            }
            Value::Array(Arc::new(rows))
        }
        JitKind::StructPtr(idx) => match native_struct_shape(idx) {
            Some(shape) => read_native_struct(ptr, &shape),
            None => Value::Unit,
        },
        _ => Value::Unit,
    }
}

/// Decodes one slot of a 2-tuple return into a VM value. An `Enum` element
/// transfers ownership of the native node (no copy); a `Str` element is
/// copied out and its native string freed by the caller; scalars are read
/// directly.
fn decode_tuple_elem(elem: TupleElem, word: i64) -> Value {
    match elem {
        TupleElem::I64 => Value::Int(word),
        TupleElem::F64 => Value::Float(f64::from_bits(word as u64)),
        TupleElem::Bool => Value::Bool(word != 0),
        TupleElem::Char => char::from_u32(word as u32).map_or(Value::Char('\0'), Value::Char),
        TupleElem::Str => native_ptr_to_value(JitKind::NativeStr, word),
        TupleElem::Enum(shape_idx) => match crate::value::native_shape(shape_idx) {
            Some(shape) => Value::NativeEnum(Arc::new(crate::value::NativeEnumOwner {
                ptr: word as usize,
                shape,
                owned: true,
            })),
            None => Value::Unit,
        },
    }
}

/// Reads a 2-tuple return `block` (a `gos_rt_gc_alloc` 16-byte aggregate of
/// two 8-byte slots) into a `Value::Tuple`. Enum slots transfer ownership of
/// their native node; string slots are copied out and the native string
/// freed here (the tuple owned it); the 16-byte block itself is freed shallow
/// by the caller via `gos_rt_aggr_free`.
///
/// # Safety
/// `block` must be a live 16-byte aggregate returned by a `TupleReturn`
/// body, with slot `i` matching `elems[i]`.
unsafe fn decode_tuple_return(block: i64, elems: &[TupleElem; 2]) -> Value {
    let mut out = Vec::with_capacity(2);
    for (i, elem) in elems.iter().enumerate() {
        // SAFETY: the block is two 8-byte slots; index `i` (0 or 1) is in range.
        let word = unsafe { (block as *const i64).add(i).read() };
        out.push(decode_tuple_elem(*elem, word));
        // A copied-out string slot's native buffer is freed once here; an enum
        // slot's node ownership transferred into the value above (no free).
        if matches!(elem, TupleElem::Str) && word != 0 {
            // SAFETY: a live native string in this slot, copied out by
            // `decode_tuple_elem` above and freed exactly once here.
            unsafe { free_native(JitKind::NativeStr, word) };
        }
    }
    Value::Tuple(Arc::new(out))
}

/// Frees one trampoline-owned native object through its runtime
/// reference-counted reclaim entry (`gos_rt_vec_free` decrements the vec
/// header RC; `gos_rt_str_free` checks the allocator tag).
///
/// # Safety
/// `ptr` must be a live object built by `build_native_arg` (or returned
/// by the body for that kind) and not already freed.
unsafe fn free_native(kind: JitKind, ptr: i64) {
    match kind {
        // A `VEC`-kind outer vec's free recursively reclaims every inner
        // `GosVec`, so `NativeVecVecI64` frees through the same call.
        JitKind::NativeVecI64
        | JitKind::NativeVecF64
        | JitKind::NativeVecStr
        | JitKind::NativeVecTupleIF
        | JitKind::NativeVecVecI64 => unsafe {
            rt::gos_rt_vec_free(ptr as *mut rt::vec::GosVec);
        },
        JitKind::NativeStr => unsafe { rt::gos_rt_str_free(ptr as *mut c_char) },
        JitKind::U8VecHandle => unsafe { rt::gos_rt_heap_u8_free(ptr as *mut rt::GosU8Vec) },
        _ => {}
    }
}

/// Reads each `&mut` parameter's mutated native object back into its VM
/// write-back cell so the caller observes in-place mutations.
fn writeback_natives(natives: &[NativeArg]) {
    for (kind, ptr, cell) in natives {
        if let Some(cell) = cell {
            *cell.lock() = native_ptr_to_value(*kind, *ptr);
        }
    }
}

/// Frees every trampoline-owned native object exactly once, deduped by
/// pointer among the params. `ret` is the native aggregate return (if any).
/// A compiled body that returns one of its own counted params (a `Vec` or a
/// `String`) takes a share of it for the caller, so that return is freed as
/// well as the param; a return of any other kind that names a param is the
/// same single allocation and is freed once.
fn free_natives(natives: &[NativeArg], ret: Option<(JitKind, i64)>) {
    let mut freed: Vec<i64> = Vec::with_capacity(natives.len() + 1);
    let free_once = |kind: JitKind, ptr: i64, freed: &mut Vec<i64>| {
        if ptr == 0 || freed.contains(&ptr) {
            return;
        }
        freed.push(ptr);
        // SAFETY: each pointer is a distinct live object we built (or the
        // body's owned return), freed at most once by the dedup above.
        unsafe { free_native(kind, ptr) };
    };
    for (kind, ptr, _) in natives {
        free_once(*kind, *ptr, &mut freed);
    }
    if let Some((kind, ptr)) = ret {
        let counted_return = matches!(
            kind,
            JitKind::NativeVecI64
                | JitKind::NativeVecF64
                | JitKind::NativeVecStr
                | JitKind::NativeVecTupleIF
                | JitKind::NativeStr
        );
        if counted_return && ptr != 0 {
            // SAFETY: the body's return holds a share of its own, distinct
            // from the one the param build took.
            unsafe { free_native(kind, ptr) };
        } else {
            free_once(kind, ptr, &mut freed);
        }
    }
}

/// One `&mut String` write-through cell: a heap-boxed slot holding the native
/// string pointer (so the JIT body's pointer-to-slot append / realloc updates
/// it), paired with the caller's binding the final value is read back into.
type StrCell = (Box<i64>, Arc<ThreadConfinedCell>);

/// Recursively frees the native enum trees marshalled in for `EnumPtr`
/// `Value::Variant` params (each was built by `build_variant_to_native_enum`
/// with strong count 1, owned end-to-end by the trampoline).
fn free_built_enums(built: &[(i64, u32)]) {
    for &(ptr, idx) in built {
        if let Some(s) = crate::value::native_shape(idx) {
            free_native_enum(ptr, &s);
        }
    }
}

/// Frees every native temporary built for a native-path dispatch that bails
/// before (or on) the call: marshalled aggregates, marshalled-in enum trees,
/// and `&mut String` cells whose native strings the body never touched.
fn free_in_flight(natives: &[NativeArg], built_enums: &[(i64, u32)], str_cells: &[StrCell]) {
    free_natives(natives, None);
    free_built_enums(built_enums);
    for (cell, _) in str_cells {
        let ptr = **cell;
        if ptr != 0 {
            // SAFETY: a live native string built for a `&mut String` slot;
            // the call never ran, so it is freed exactly once here.
            unsafe { free_native(JitKind::NativeStr, ptr) };
        }
    }
}

/// Per-Vm cache of marshalled `Vec<Vec<i64>>` graphs, keyed by the source
/// `Value::Array`'s `Arc` pointer identity, so a graph passed to many JIT
/// calls (e.g. a BFS run that reads one immutable graph ten times) is
/// marshalled once instead of per call.
///
/// # Soundness
/// Each entry holds a strong clone of the source `Arc`, which pins that
/// `Arc`'s address for the entry's lifetime: a different array can never be
/// allocated at the same address while the entry lives, so an `Arc::as_ptr`
/// key uniquely identifies the same, immutable contents (the graph crosses
/// only as a read-shared `&[[i64]]`; a `&mut` vec-of-vec param keeps the body
/// on bytecode). A grown / reallocated outer Vec produces a fresh `Arc` at a
/// new address -> a cache miss -> a correct re-marshal. The native graphs are
/// owned solely by the cache (never handed to `free_natives`) and freed when
/// the cache is cleared at Vm teardown or between pooled worker tasks, so
/// there is no double free and no per-run leak.
struct GraphCacheEntry {
    ptr: i64,
    _src: Arc<Vec<Value>>,
    bytes: usize,
}

#[derive(Default)]
struct GraphCacheState {
    entries: rustc_hash::FxHashMap<usize, GraphCacheEntry>,
    lru: std::collections::VecDeque<usize>,
    candidate: Option<(usize, std::sync::Weak<Vec<Value>>)>,
    bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct GraphCacheMetrics {
    pub(crate) bytes: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
}

#[derive(Default)]
pub(crate) struct GraphCache {
    state: std::cell::RefCell<GraphCacheState>,
}

impl GraphCache {
    const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

    fn max_bytes() -> usize {
        std::env::var("GOS_JIT_GRAPH_CACHE_BYTES")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(Self::DEFAULT_MAX_BYTES)
    }

    fn graph_bytes(src: &[Value]) -> usize {
        let payload = src.iter().fold(0usize, |total, row| {
            let len = match row {
                Value::IntArray(values) => values.len(),
                Value::Array(values) => values.len(),
                _ => 0,
            };
            total.saturating_add(len.saturating_mul(std::mem::size_of::<i64>()))
        });
        payload.saturating_add(src.len().saturating_mul(std::mem::size_of::<usize>()))
    }

    /// Cached native outer-`GosVec` pointer for `key`, if marshalled before.
    fn get(&self, key: usize) -> Option<i64> {
        let mut state = self.state.borrow_mut();
        let ptr = state.entries.get(&key).map(|entry| entry.ptr);
        if ptr.is_some() {
            state.hits = state.hits.saturating_add(1);
            state.lru.retain(|cached| *cached != key);
            state.lru.push_back(key);
        } else {
            state.misses = state.misses.saturating_add(1);
        }
        ptr
    }

    /// `true` when `ptr` is a cache-owned native graph. A body that returns
    /// one of its `&[[i64]]` params hands back a pointer the cache still owns;
    /// the trampoline must not free it (the cache frees it at teardown).
    fn owns_ptr(&self, ptr: i64) -> bool {
        self.state
            .borrow()
            .entries
            .values()
            .any(|entry| entry.ptr == ptr)
    }

    /// Returns true only on a repeated call with the same still-live source.
    /// One-off streaming graphs are therefore marshalled for the call and
    /// immediately freed instead of entering the retained cache.
    fn should_cache(&self, key: usize, src: &Arc<Vec<Value>>) -> bool {
        let mut state = self.state.borrow_mut();
        let repeated = state.candidate.as_ref().is_some_and(|(candidate, weak)| {
            *candidate == key && weak.upgrade().is_some_and(|value| Arc::ptr_eq(&value, src))
        });
        state.candidate = Some((key, Arc::downgrade(src)));
        repeated
    }

    /// Records `ptr` under a byte-accounted LRU budget. Oversized graphs are
    /// not retained at all.
    fn insert(&self, key: usize, ptr: i64, src: Arc<Vec<Value>>) -> bool {
        let bytes = Self::graph_bytes(&src);
        let max_bytes = Self::max_bytes();
        if max_bytes == 0 || bytes > max_bytes {
            return false;
        }
        let mut state = self.state.borrow_mut();
        while state.bytes.saturating_add(bytes) > max_bytes {
            let Some(old_key) = state.lru.pop_front() else {
                break;
            };
            if let Some(old) = state.entries.remove(&old_key) {
                state.bytes = state.bytes.saturating_sub(old.bytes);
                state.evictions = state.evictions.saturating_add(1);
                // SAFETY: the removed entry solely owns this native graph.
                unsafe { rt::gos_rt_vec_free(old.ptr as *mut rt::vec::GosVec) };
            }
        }
        state.bytes = state.bytes.saturating_add(bytes);
        state.lru.push_back(key);
        state.entries.insert(
            key,
            GraphCacheEntry {
                ptr,
                _src: src,
                bytes,
            },
        );
        true
    }

    pub(crate) fn metrics(&self) -> GraphCacheMetrics {
        let state = self.state.borrow();
        GraphCacheMetrics {
            bytes: state.bytes,
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
        }
    }

    /// Frees every cached native graph and empties the cache. Called at Vm
    /// teardown (via [`Drop`]) and between pooled worker tasks.
    pub(crate) fn clear(&self) {
        let mut state = self.state.borrow_mut();
        for (_, entry) in state.entries.drain() {
            // SAFETY: each `ptr` is a live native outer `GosVec` built by
            // `build_native_vec_vec_i64`, owned solely by this cache (it is
            // never pushed into a call's `natives`, so `free_natives` never
            // touches it), and freed exactly once here. Freeing the outer
            // recursively reclaims every inner vec.
            unsafe { rt::gos_rt_vec_free(entry.ptr as *mut rt::vec::GosVec) };
        }
        state.lru.clear();
        state.candidate = None;
        state.bytes = 0;
    }
}

impl Drop for GraphCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod graph_cache_tests {
    use super::*;

    #[test]
    fn graph_cache_admits_only_after_reuse_and_reports_bytes() {
        let cache = GraphCache::default();
        let graph = Arc::new(vec![Value::IntArray(Arc::new(vec![1, 2, 3]))]);
        let key = Arc::as_ptr(&graph) as usize;

        assert!(cache.get(key).is_none());
        assert!(!cache.should_cache(key, &graph));
        let first = build_native_vec_vec_i64(&graph).expect("first graph marshal");
        // SAFETY: the first-use graph was deliberately not retained.
        unsafe { rt::gos_rt_vec_free(first as *mut rt::vec::GosVec) };

        assert!(cache.get(key).is_none());
        assert!(cache.should_cache(key, &graph));
        let second = build_native_vec_vec_i64(&graph).expect("second graph marshal");
        assert!(cache.insert(key, second, Arc::clone(&graph)));
        assert_eq!(cache.get(key), Some(second));
        let metrics = cache.metrics();
        assert_eq!(metrics.hits, 1);
        assert_eq!(metrics.misses, 2);
        assert!(metrics.bytes >= 3 * std::mem::size_of::<i64>());
        cache.clear();
        assert_eq!(cache.metrics().bytes, 0);
    }
}

/// Result of attempting to dispatch through the JIT trampoline.
pub(crate) enum Dispatch {
    /// The JIT body ran and produced a value.
    Ok(Value),
    /// The JIT body cannot be invoked with these args (shape
    /// unsupported, or a runtime arg's type didn't match the JIT
    /// signature). The caller falls back to the bytecode chunk.
    Fallback,
}

const MAX_ARGS: usize = 12;

#[derive(Clone, Copy)]
pub(crate) enum Slot {
    I(i64),
    F(f64),
}

fn slot_i(s: Slot) -> i64 {
    match s {
        Slot::I(n) => n,
        Slot::F(_) => 0,
    }
}

fn slot_f(s: Slot) -> f64 {
    match s {
        Slot::F(x) => x,
        Slot::I(_) => 0.0,
    }
}

/// Calls the JIT body through a reified `extern "C"` signature
/// derived from the supplied parameter and return kinds. Used by
/// every per-arity-shape stub below so each only has to bind its
/// args; the four return-kind branches live in one place.
macro_rules! call_through {
    ($ptr:expr, $ret:expr, [$($a:ident: $t:ty),* $(,)?]) => {{
        match $ret {
            JitKind::I64 => {
                let f: extern "C" fn($($t),*) -> i64 = unsafe { mem::transmute($ptr) };
                Some(Value::Int(f($($a),*)))
            }
            JitKind::F64 => {
                let f: extern "C" fn($($t),*) -> f64 = unsafe { mem::transmute($ptr) };
                Some(Value::Float(f($($a),*)))
            }
            JitKind::Bool => {
                let f: extern "C" fn($($t),*) -> i8 = unsafe { mem::transmute($ptr) };
                Some(Value::Bool(f($($a),*) != 0))
            }
            JitKind::Char => {
                let f: extern "C" fn($($t),*) -> i64 = unsafe { mem::transmute($ptr) };
                let word = f($($a),*) as u32;
                Some(Value::Char(char::from_u32(word).unwrap_or('\u{0}')))
            }
            JitKind::Unit => {
                let f: extern "C" fn($($t),*) = unsafe { mem::transmute($ptr) };
                f($($a),*);
                Some(Value::Unit)
            }
            // Aggregate (`String`, `Tuple`, `Adt`, channel, …):
            // the JIT body returns a `GossamerValue` u64 handle in
            // an integer register, which we decode back through
            // `Value::from_raw`. `GossamerValue` is a transparent
            // `u64` so the i64-shaped return register holds the
            // exact bit pattern.
            JitKind::Value => {
                let f: extern "C" fn($($t),*) -> i64 = unsafe { mem::transmute($ptr) };
                let raw = f($($a),*) as u64;
                Some(Value::from_raw(raw))
            }
            // Canonicalized to I64 before dispatch; never reaches a stub.
            JitKind::EnumPtr(_) => unreachable!("EnumPtr returns are canonicalized to I64"),
            // `Result<Enum, _>`: the body's two-word `[disc, payload]` carrier
            // crosses through an out-pointer thunk (`emit_carrier_entry_thunk`),
            // so `$ptr` here is the thunk - it takes a buffer pointer first,
            // calls the real body, and stores the carrier there. A pointer
            // argument has an identical ABI on every target, unlike an `i128`
            // return (which Windows x64 places in a register Rust reads
            // differently). `invoke_prepared_native` decodes the carrier tuple.
            JitKind::ResultEnumPtr(_) | JitKind::Carrier(_) => {
                // A `u128` slot is 16-byte aligned, matching the thunk's
                // aligned `i128` store; `disc` is the low word, `payload`
                // the high word.
                let mut carrier: u128 = 0;
                let f: extern "C" fn(*mut u128, $($t),*) = unsafe { mem::transmute($ptr) };
                f(&raw mut carrier, $($a),*);
                let disc = (carrier as u64) as i64;
                let payload = ((carrier >> 64) as u64) as i64;
                Some(Value::Tuple(Arc::from(vec![
                    Value::Int(disc),
                    Value::Int(payload),
                ])))
            }
            // Native aggregate returns are canonicalized to I64 in `prepare`
            // and re-wrapped by `invoke_prepared_native`; the stub only ever
            // sees the I64 shape.
            JitKind::NativeStr
            | JitKind::NativeVecI64
            | JitKind::NativeVecF64
            | JitKind::NativeVecStr
            | JitKind::NativeVecTupleIF
            | JitKind::NativeVecVecI64
            | JitKind::U8VecHandle => {
                unreachable!("native aggregate returns are canonicalized to I64")
            }
            // Struct returns are canonicalized to I64 in `prepare` and decoded
            // from their caller-owned sret block by `invoke_prepared_native`.
            JitKind::StructPtr(_) => unreachable!("struct returns are decoded in invoke_prepared_native"),
            // A flat array block is a parameter-only shape; `body_kinds`
            // keeps a body returning one on bytecode.
            JitKind::ArrayBlockPtr(..) => unreachable!("array blocks are parameter-only"),
            // A tuple return is canonicalised to `I64` in `prepare` and decoded
            // in `invoke_prepared_native`; the stub only ever sees the `I64`.
            JitKind::TupleReturn(_) => {
                unreachable!("tuple returns are decoded in invoke_prepared_native")
            }
        }
    }};
}

/// Maps the per-slot shape token (`i` / `f`) to the matching
/// `slot_*` accessor.
macro_rules! slot_for {
    (i, $s:expr, $idx:expr) => {
        slot_i($s[$idx])
    };
    (f, $s:expr, $idx:expr) => {
        slot_f($s[$idx])
    };
}

/// Maps the per-slot shape token (`i` / `f`) to the corresponding
/// Rust ABI type. Used to spell out the `extern "C" fn(...)`
/// signature inside `call_through!`.
macro_rules! ty_for {
    (i) => {
        i64
    };
    (f) => {
        f64
    };
}

/// Generates a `call_<arity><shape>` function for one (arity, shape)
/// combination. Distinct binding names per slot (`a0`, `a1`, …) are
/// required so each `let` introduces a fresh local instead of
/// shadowing the previous one - `call_through!` then sees every
/// argument in scope simultaneously when it expands the
/// `extern "C"` call.
macro_rules! gen_call {
    ($name:ident, $c0:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            call_through!(ptr, ret, [a0: ty_for!($c0)])
        }
    };
    ($name:ident, $c0:ident, $c1:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            call_through!(ptr, ret, [a0: ty_for!($c0), a1: ty_for!($c1)])
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1), a2: ty_for!($c2)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident, $c5:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident, $c5:ident, $c6:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident, $c5:ident, $c6:ident, $c7:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            let a7 = slot_for!($c7, s, 7);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6), a7: ty_for!($c7)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident,
     $c5:ident, $c6:ident, $c7:ident, $c8:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            let a7 = slot_for!($c7, s, 7);
            let a8 = slot_for!($c8, s, 8);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6), a7: ty_for!($c7),
                 a8: ty_for!($c8)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident,
     $c5:ident, $c6:ident, $c7:ident, $c8:ident, $c9:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            let a7 = slot_for!($c7, s, 7);
            let a8 = slot_for!($c8, s, 8);
            let a9 = slot_for!($c9, s, 9);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6), a7: ty_for!($c7),
                 a8: ty_for!($c8), a9: ty_for!($c9)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident,
     $c5:ident, $c6:ident, $c7:ident, $c8:ident, $c9:ident, $c10:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            let a7 = slot_for!($c7, s, 7);
            let a8 = slot_for!($c8, s, 8);
            let a9 = slot_for!($c9, s, 9);
            let a10 = slot_for!($c10, s, 10);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6), a7: ty_for!($c7),
                 a8: ty_for!($c8), a9: ty_for!($c9),
                 a10: ty_for!($c10)]
            )
        }
    };
    ($name:ident, $c0:ident, $c1:ident, $c2:ident, $c3:ident, $c4:ident,
     $c5:ident, $c6:ident, $c7:ident, $c8:ident, $c9:ident, $c10:ident, $c11:ident) => {
        unsafe fn $name(ptr: *const u8, s: &[Slot], ret: JitKind) -> Option<Value> {
            let a0 = slot_for!($c0, s, 0);
            let a1 = slot_for!($c1, s, 1);
            let a2 = slot_for!($c2, s, 2);
            let a3 = slot_for!($c3, s, 3);
            let a4 = slot_for!($c4, s, 4);
            let a5 = slot_for!($c5, s, 5);
            let a6 = slot_for!($c6, s, 6);
            let a7 = slot_for!($c7, s, 7);
            let a8 = slot_for!($c8, s, 8);
            let a9 = slot_for!($c9, s, 9);
            let a10 = slot_for!($c10, s, 10);
            let a11 = slot_for!($c11, s, 11);
            call_through!(
                ptr, ret,
                [a0: ty_for!($c0), a1: ty_for!($c1),
                 a2: ty_for!($c2), a3: ty_for!($c3),
                 a4: ty_for!($c4), a5: ty_for!($c5),
                 a6: ty_for!($c6), a7: ty_for!($c7),
                 a8: ty_for!($c8), a9: ty_for!($c9),
                 a10: ty_for!($c10), a11: ty_for!($c11)]
            )
        }
    };
}

// Arity 1.
gen_call!(call_1i, i);
gen_call!(call_1f, f);
// Arity 2.
gen_call!(call_2ii, i, i);
gen_call!(call_2if, i, f);
gen_call!(call_2fi, f, i);
gen_call!(call_2ff, f, f);
// Arity 3.
gen_call!(call_3iii, i, i, i);
gen_call!(call_3fii, f, i, i);
gen_call!(call_3ifi, i, f, i);
gen_call!(call_3ffi, f, f, i);
gen_call!(call_3iif, i, i, f);
gen_call!(call_3fif, f, i, f);
gen_call!(call_3iff, i, f, f);
gen_call!(call_3fff, f, f, f);
// Arity 4.
gen_call!(call_4iiii, i, i, i, i);
gen_call!(call_4fiii, f, i, i, i);
gen_call!(call_4ifii, i, f, i, i);
gen_call!(call_4ffii, f, f, i, i);
gen_call!(call_4iifi, i, i, f, i);
gen_call!(call_4fifi, f, i, f, i);
gen_call!(call_4iffi, i, f, f, i);
gen_call!(call_4fffi, f, f, f, i);
gen_call!(call_4iiif, i, i, i, f);
gen_call!(call_4fiif, f, i, i, f);
gen_call!(call_4ifif, i, f, i, f);
gen_call!(call_4ffif, f, f, i, f);
gen_call!(call_4iiff, i, i, f, f);
gen_call!(call_4fiff, f, i, f, f);
gen_call!(call_4ifff, i, f, f, f);
gen_call!(call_4ffff, f, f, f, f);
// Arity 5-8: every int/float shape permutation.
gen_call!(call_5iiiii, i, i, i, i, i);
gen_call!(call_5fiiii, f, i, i, i, i);
gen_call!(call_5ifiii, i, f, i, i, i);
gen_call!(call_5ffiii, f, f, i, i, i);
gen_call!(call_5iifii, i, i, f, i, i);
gen_call!(call_5fifii, f, i, f, i, i);
gen_call!(call_5iffii, i, f, f, i, i);
gen_call!(call_5fffii, f, f, f, i, i);
gen_call!(call_5iiifi, i, i, i, f, i);
gen_call!(call_5fiifi, f, i, i, f, i);
gen_call!(call_5ififi, i, f, i, f, i);
gen_call!(call_5ffifi, f, f, i, f, i);
gen_call!(call_5iiffi, i, i, f, f, i);
gen_call!(call_5fiffi, f, i, f, f, i);
gen_call!(call_5ifffi, i, f, f, f, i);
gen_call!(call_5ffffi, f, f, f, f, i);
gen_call!(call_5iiiif, i, i, i, i, f);
gen_call!(call_5fiiif, f, i, i, i, f);
gen_call!(call_5ifiif, i, f, i, i, f);
gen_call!(call_5ffiif, f, f, i, i, f);
gen_call!(call_5iifif, i, i, f, i, f);
gen_call!(call_5fifif, f, i, f, i, f);
gen_call!(call_5iffif, i, f, f, i, f);
gen_call!(call_5fffif, f, f, f, i, f);
gen_call!(call_5iiiff, i, i, i, f, f);
gen_call!(call_5fiiff, f, i, i, f, f);
gen_call!(call_5ififf, i, f, i, f, f);
gen_call!(call_5ffiff, f, f, i, f, f);
gen_call!(call_5iifff, i, i, f, f, f);
gen_call!(call_5fifff, f, i, f, f, f);
gen_call!(call_5iffff, i, f, f, f, f);
gen_call!(call_5fffff, f, f, f, f, f);
gen_call!(call_6iiiiii, i, i, i, i, i, i);
gen_call!(call_6fiiiii, f, i, i, i, i, i);
gen_call!(call_6ifiiii, i, f, i, i, i, i);
gen_call!(call_6ffiiii, f, f, i, i, i, i);
gen_call!(call_6iifiii, i, i, f, i, i, i);
gen_call!(call_6fifiii, f, i, f, i, i, i);
gen_call!(call_6iffiii, i, f, f, i, i, i);
gen_call!(call_6fffiii, f, f, f, i, i, i);
gen_call!(call_6iiifii, i, i, i, f, i, i);
gen_call!(call_6fiifii, f, i, i, f, i, i);
gen_call!(call_6ififii, i, f, i, f, i, i);
gen_call!(call_6ffifii, f, f, i, f, i, i);
gen_call!(call_6iiffii, i, i, f, f, i, i);
gen_call!(call_6fiffii, f, i, f, f, i, i);
gen_call!(call_6ifffii, i, f, f, f, i, i);
gen_call!(call_6ffffii, f, f, f, f, i, i);
gen_call!(call_6iiiifi, i, i, i, i, f, i);
gen_call!(call_6fiiifi, f, i, i, i, f, i);
gen_call!(call_6ifiifi, i, f, i, i, f, i);
gen_call!(call_6ffiifi, f, f, i, i, f, i);
gen_call!(call_6iififi, i, i, f, i, f, i);
gen_call!(call_6fififi, f, i, f, i, f, i);
gen_call!(call_6iffifi, i, f, f, i, f, i);
gen_call!(call_6fffifi, f, f, f, i, f, i);
gen_call!(call_6iiiffi, i, i, i, f, f, i);
gen_call!(call_6fiiffi, f, i, i, f, f, i);
gen_call!(call_6ififfi, i, f, i, f, f, i);
gen_call!(call_6ffiffi, f, f, i, f, f, i);
gen_call!(call_6iifffi, i, i, f, f, f, i);
gen_call!(call_6fifffi, f, i, f, f, f, i);
gen_call!(call_6iffffi, i, f, f, f, f, i);
gen_call!(call_6fffffi, f, f, f, f, f, i);
gen_call!(call_6iiiiif, i, i, i, i, i, f);
gen_call!(call_6fiiiif, f, i, i, i, i, f);
gen_call!(call_6ifiiif, i, f, i, i, i, f);
gen_call!(call_6ffiiif, f, f, i, i, i, f);
gen_call!(call_6iifiif, i, i, f, i, i, f);
gen_call!(call_6fifiif, f, i, f, i, i, f);
gen_call!(call_6iffiif, i, f, f, i, i, f);
gen_call!(call_6fffiif, f, f, f, i, i, f);
gen_call!(call_6iiifif, i, i, i, f, i, f);
gen_call!(call_6fiifif, f, i, i, f, i, f);
gen_call!(call_6ififif, i, f, i, f, i, f);
gen_call!(call_6ffifif, f, f, i, f, i, f);
gen_call!(call_6iiffif, i, i, f, f, i, f);
gen_call!(call_6fiffif, f, i, f, f, i, f);
gen_call!(call_6ifffif, i, f, f, f, i, f);
gen_call!(call_6ffffif, f, f, f, f, i, f);
gen_call!(call_6iiiiff, i, i, i, i, f, f);
gen_call!(call_6fiiiff, f, i, i, i, f, f);
gen_call!(call_6ifiiff, i, f, i, i, f, f);
gen_call!(call_6ffiiff, f, f, i, i, f, f);
gen_call!(call_6iififf, i, i, f, i, f, f);
gen_call!(call_6fififf, f, i, f, i, f, f);
gen_call!(call_6iffiff, i, f, f, i, f, f);
gen_call!(call_6fffiff, f, f, f, i, f, f);
gen_call!(call_6iiifff, i, i, i, f, f, f);
gen_call!(call_6fiifff, f, i, i, f, f, f);
gen_call!(call_6ififff, i, f, i, f, f, f);
gen_call!(call_6ffifff, f, f, i, f, f, f);
gen_call!(call_6iiffff, i, i, f, f, f, f);
gen_call!(call_6fiffff, f, i, f, f, f, f);
gen_call!(call_6ifffff, i, f, f, f, f, f);
gen_call!(call_6ffffff, f, f, f, f, f, f);
gen_call!(call_7iiiiiii, i, i, i, i, i, i, i);
gen_call!(call_7fiiiiii, f, i, i, i, i, i, i);
gen_call!(call_7ifiiiii, i, f, i, i, i, i, i);
gen_call!(call_7ffiiiii, f, f, i, i, i, i, i);
gen_call!(call_7iifiiii, i, i, f, i, i, i, i);
gen_call!(call_7fifiiii, f, i, f, i, i, i, i);
gen_call!(call_7iffiiii, i, f, f, i, i, i, i);
gen_call!(call_7fffiiii, f, f, f, i, i, i, i);
gen_call!(call_7iiifiii, i, i, i, f, i, i, i);
gen_call!(call_7fiifiii, f, i, i, f, i, i, i);
gen_call!(call_7ififiii, i, f, i, f, i, i, i);
gen_call!(call_7ffifiii, f, f, i, f, i, i, i);
gen_call!(call_7iiffiii, i, i, f, f, i, i, i);
gen_call!(call_7fiffiii, f, i, f, f, i, i, i);
gen_call!(call_7ifffiii, i, f, f, f, i, i, i);
gen_call!(call_7ffffiii, f, f, f, f, i, i, i);
gen_call!(call_7iiiifii, i, i, i, i, f, i, i);
gen_call!(call_7fiiifii, f, i, i, i, f, i, i);
gen_call!(call_7ifiifii, i, f, i, i, f, i, i);
gen_call!(call_7ffiifii, f, f, i, i, f, i, i);
gen_call!(call_7iififii, i, i, f, i, f, i, i);
gen_call!(call_7fififii, f, i, f, i, f, i, i);
gen_call!(call_7iffifii, i, f, f, i, f, i, i);
gen_call!(call_7fffifii, f, f, f, i, f, i, i);
gen_call!(call_7iiiffii, i, i, i, f, f, i, i);
gen_call!(call_7fiiffii, f, i, i, f, f, i, i);
gen_call!(call_7ififfii, i, f, i, f, f, i, i);
gen_call!(call_7ffiffii, f, f, i, f, f, i, i);
gen_call!(call_7iifffii, i, i, f, f, f, i, i);
gen_call!(call_7fifffii, f, i, f, f, f, i, i);
gen_call!(call_7iffffii, i, f, f, f, f, i, i);
gen_call!(call_7fffffii, f, f, f, f, f, i, i);
gen_call!(call_7iiiiifi, i, i, i, i, i, f, i);
gen_call!(call_7fiiiifi, f, i, i, i, i, f, i);
gen_call!(call_7ifiiifi, i, f, i, i, i, f, i);
gen_call!(call_7ffiiifi, f, f, i, i, i, f, i);
gen_call!(call_7iifiifi, i, i, f, i, i, f, i);
gen_call!(call_7fifiifi, f, i, f, i, i, f, i);
gen_call!(call_7iffiifi, i, f, f, i, i, f, i);
gen_call!(call_7fffiifi, f, f, f, i, i, f, i);
gen_call!(call_7iiififi, i, i, i, f, i, f, i);
gen_call!(call_7fiififi, f, i, i, f, i, f, i);
gen_call!(call_7ifififi, i, f, i, f, i, f, i);
gen_call!(call_7ffififi, f, f, i, f, i, f, i);
gen_call!(call_7iiffifi, i, i, f, f, i, f, i);
gen_call!(call_7fiffifi, f, i, f, f, i, f, i);
gen_call!(call_7ifffifi, i, f, f, f, i, f, i);
gen_call!(call_7ffffifi, f, f, f, f, i, f, i);
gen_call!(call_7iiiiffi, i, i, i, i, f, f, i);
gen_call!(call_7fiiiffi, f, i, i, i, f, f, i);
gen_call!(call_7ifiiffi, i, f, i, i, f, f, i);
gen_call!(call_7ffiiffi, f, f, i, i, f, f, i);
gen_call!(call_7iififfi, i, i, f, i, f, f, i);
gen_call!(call_7fififfi, f, i, f, i, f, f, i);
gen_call!(call_7iffiffi, i, f, f, i, f, f, i);
gen_call!(call_7fffiffi, f, f, f, i, f, f, i);
gen_call!(call_7iiifffi, i, i, i, f, f, f, i);
gen_call!(call_7fiifffi, f, i, i, f, f, f, i);
gen_call!(call_7ififffi, i, f, i, f, f, f, i);
gen_call!(call_7ffifffi, f, f, i, f, f, f, i);
gen_call!(call_7iiffffi, i, i, f, f, f, f, i);
gen_call!(call_7fiffffi, f, i, f, f, f, f, i);
gen_call!(call_7ifffffi, i, f, f, f, f, f, i);
gen_call!(call_7ffffffi, f, f, f, f, f, f, i);
gen_call!(call_7iiiiiif, i, i, i, i, i, i, f);
gen_call!(call_7fiiiiif, f, i, i, i, i, i, f);
gen_call!(call_7ifiiiif, i, f, i, i, i, i, f);
gen_call!(call_7ffiiiif, f, f, i, i, i, i, f);
gen_call!(call_7iifiiif, i, i, f, i, i, i, f);
gen_call!(call_7fifiiif, f, i, f, i, i, i, f);
gen_call!(call_7iffiiif, i, f, f, i, i, i, f);
gen_call!(call_7fffiiif, f, f, f, i, i, i, f);
gen_call!(call_7iiifiif, i, i, i, f, i, i, f);
gen_call!(call_7fiifiif, f, i, i, f, i, i, f);
gen_call!(call_7ififiif, i, f, i, f, i, i, f);
gen_call!(call_7ffifiif, f, f, i, f, i, i, f);
gen_call!(call_7iiffiif, i, i, f, f, i, i, f);
gen_call!(call_7fiffiif, f, i, f, f, i, i, f);
gen_call!(call_7ifffiif, i, f, f, f, i, i, f);
gen_call!(call_7ffffiif, f, f, f, f, i, i, f);
gen_call!(call_7iiiifif, i, i, i, i, f, i, f);
gen_call!(call_7fiiifif, f, i, i, i, f, i, f);
gen_call!(call_7ifiifif, i, f, i, i, f, i, f);
gen_call!(call_7ffiifif, f, f, i, i, f, i, f);
gen_call!(call_7iififif, i, i, f, i, f, i, f);
gen_call!(call_7fififif, f, i, f, i, f, i, f);
gen_call!(call_7iffifif, i, f, f, i, f, i, f);
gen_call!(call_7fffifif, f, f, f, i, f, i, f);
gen_call!(call_7iiiffif, i, i, i, f, f, i, f);
gen_call!(call_7fiiffif, f, i, i, f, f, i, f);
gen_call!(call_7ififfif, i, f, i, f, f, i, f);
gen_call!(call_7ffiffif, f, f, i, f, f, i, f);
gen_call!(call_7iifffif, i, i, f, f, f, i, f);
gen_call!(call_7fifffif, f, i, f, f, f, i, f);
gen_call!(call_7iffffif, i, f, f, f, f, i, f);
gen_call!(call_7fffffif, f, f, f, f, f, i, f);
gen_call!(call_7iiiiiff, i, i, i, i, i, f, f);
gen_call!(call_7fiiiiff, f, i, i, i, i, f, f);
gen_call!(call_7ifiiiff, i, f, i, i, i, f, f);
gen_call!(call_7ffiiiff, f, f, i, i, i, f, f);
gen_call!(call_7iifiiff, i, i, f, i, i, f, f);
gen_call!(call_7fifiiff, f, i, f, i, i, f, f);
gen_call!(call_7iffiiff, i, f, f, i, i, f, f);
gen_call!(call_7fffiiff, f, f, f, i, i, f, f);
gen_call!(call_7iiififf, i, i, i, f, i, f, f);
gen_call!(call_7fiififf, f, i, i, f, i, f, f);
gen_call!(call_7ifififf, i, f, i, f, i, f, f);
gen_call!(call_7ffififf, f, f, i, f, i, f, f);
gen_call!(call_7iiffiff, i, i, f, f, i, f, f);
gen_call!(call_7fiffiff, f, i, f, f, i, f, f);
gen_call!(call_7ifffiff, i, f, f, f, i, f, f);
gen_call!(call_7ffffiff, f, f, f, f, i, f, f);
gen_call!(call_7iiiifff, i, i, i, i, f, f, f);
gen_call!(call_7fiiifff, f, i, i, i, f, f, f);
gen_call!(call_7ifiifff, i, f, i, i, f, f, f);
gen_call!(call_7ffiifff, f, f, i, i, f, f, f);
gen_call!(call_7iififff, i, i, f, i, f, f, f);
gen_call!(call_7fififff, f, i, f, i, f, f, f);
gen_call!(call_7iffifff, i, f, f, i, f, f, f);
gen_call!(call_7fffifff, f, f, f, i, f, f, f);
gen_call!(call_7iiiffff, i, i, i, f, f, f, f);
gen_call!(call_7fiiffff, f, i, i, f, f, f, f);
gen_call!(call_7ififfff, i, f, i, f, f, f, f);
gen_call!(call_7ffiffff, f, f, i, f, f, f, f);
gen_call!(call_7iifffff, i, i, f, f, f, f, f);
gen_call!(call_7fifffff, f, i, f, f, f, f, f);
gen_call!(call_7iffffff, i, f, f, f, f, f, f);
gen_call!(call_7fffffff, f, f, f, f, f, f, f);
gen_call!(call_8iiiiiiii, i, i, i, i, i, i, i, i);
gen_call!(call_8fiiiiiii, f, i, i, i, i, i, i, i);
gen_call!(call_8ifiiiiii, i, f, i, i, i, i, i, i);
gen_call!(call_8ffiiiiii, f, f, i, i, i, i, i, i);
gen_call!(call_8iifiiiii, i, i, f, i, i, i, i, i);
gen_call!(call_8fifiiiii, f, i, f, i, i, i, i, i);
gen_call!(call_8iffiiiii, i, f, f, i, i, i, i, i);
gen_call!(call_8fffiiiii, f, f, f, i, i, i, i, i);
gen_call!(call_8iiifiiii, i, i, i, f, i, i, i, i);
gen_call!(call_8fiifiiii, f, i, i, f, i, i, i, i);
gen_call!(call_8ififiiii, i, f, i, f, i, i, i, i);
gen_call!(call_8ffifiiii, f, f, i, f, i, i, i, i);
gen_call!(call_8iiffiiii, i, i, f, f, i, i, i, i);
gen_call!(call_8fiffiiii, f, i, f, f, i, i, i, i);
gen_call!(call_8ifffiiii, i, f, f, f, i, i, i, i);
gen_call!(call_8ffffiiii, f, f, f, f, i, i, i, i);
gen_call!(call_8iiiifiii, i, i, i, i, f, i, i, i);
gen_call!(call_8fiiifiii, f, i, i, i, f, i, i, i);
gen_call!(call_8ifiifiii, i, f, i, i, f, i, i, i);
gen_call!(call_8ffiifiii, f, f, i, i, f, i, i, i);
gen_call!(call_8iififiii, i, i, f, i, f, i, i, i);
gen_call!(call_8fififiii, f, i, f, i, f, i, i, i);
gen_call!(call_8iffifiii, i, f, f, i, f, i, i, i);
gen_call!(call_8fffifiii, f, f, f, i, f, i, i, i);
gen_call!(call_8iiiffiii, i, i, i, f, f, i, i, i);
gen_call!(call_8fiiffiii, f, i, i, f, f, i, i, i);
gen_call!(call_8ififfiii, i, f, i, f, f, i, i, i);
gen_call!(call_8ffiffiii, f, f, i, f, f, i, i, i);
gen_call!(call_8iifffiii, i, i, f, f, f, i, i, i);
gen_call!(call_8fifffiii, f, i, f, f, f, i, i, i);
gen_call!(call_8iffffiii, i, f, f, f, f, i, i, i);
gen_call!(call_8fffffiii, f, f, f, f, f, i, i, i);
gen_call!(call_8iiiiifii, i, i, i, i, i, f, i, i);
gen_call!(call_8fiiiifii, f, i, i, i, i, f, i, i);
gen_call!(call_8ifiiifii, i, f, i, i, i, f, i, i);
gen_call!(call_8ffiiifii, f, f, i, i, i, f, i, i);
gen_call!(call_8iifiifii, i, i, f, i, i, f, i, i);
gen_call!(call_8fifiifii, f, i, f, i, i, f, i, i);
gen_call!(call_8iffiifii, i, f, f, i, i, f, i, i);
gen_call!(call_8fffiifii, f, f, f, i, i, f, i, i);
gen_call!(call_8iiififii, i, i, i, f, i, f, i, i);
gen_call!(call_8fiififii, f, i, i, f, i, f, i, i);
gen_call!(call_8ifififii, i, f, i, f, i, f, i, i);
gen_call!(call_8ffififii, f, f, i, f, i, f, i, i);
gen_call!(call_8iiffifii, i, i, f, f, i, f, i, i);
gen_call!(call_8fiffifii, f, i, f, f, i, f, i, i);
gen_call!(call_8ifffifii, i, f, f, f, i, f, i, i);
gen_call!(call_8ffffifii, f, f, f, f, i, f, i, i);
gen_call!(call_8iiiiffii, i, i, i, i, f, f, i, i);
gen_call!(call_8fiiiffii, f, i, i, i, f, f, i, i);
gen_call!(call_8ifiiffii, i, f, i, i, f, f, i, i);
gen_call!(call_8ffiiffii, f, f, i, i, f, f, i, i);
gen_call!(call_8iififfii, i, i, f, i, f, f, i, i);
gen_call!(call_8fififfii, f, i, f, i, f, f, i, i);
gen_call!(call_8iffiffii, i, f, f, i, f, f, i, i);
gen_call!(call_8fffiffii, f, f, f, i, f, f, i, i);
gen_call!(call_8iiifffii, i, i, i, f, f, f, i, i);
gen_call!(call_8fiifffii, f, i, i, f, f, f, i, i);
gen_call!(call_8ififffii, i, f, i, f, f, f, i, i);
gen_call!(call_8ffifffii, f, f, i, f, f, f, i, i);
gen_call!(call_8iiffffii, i, i, f, f, f, f, i, i);
gen_call!(call_8fiffffii, f, i, f, f, f, f, i, i);
gen_call!(call_8ifffffii, i, f, f, f, f, f, i, i);
gen_call!(call_8ffffffii, f, f, f, f, f, f, i, i);
gen_call!(call_8iiiiiifi, i, i, i, i, i, i, f, i);
gen_call!(call_8fiiiiifi, f, i, i, i, i, i, f, i);
gen_call!(call_8ifiiiifi, i, f, i, i, i, i, f, i);
gen_call!(call_8ffiiiifi, f, f, i, i, i, i, f, i);
gen_call!(call_8iifiiifi, i, i, f, i, i, i, f, i);
gen_call!(call_8fifiiifi, f, i, f, i, i, i, f, i);
gen_call!(call_8iffiiifi, i, f, f, i, i, i, f, i);
gen_call!(call_8fffiiifi, f, f, f, i, i, i, f, i);
gen_call!(call_8iiifiifi, i, i, i, f, i, i, f, i);
gen_call!(call_8fiifiifi, f, i, i, f, i, i, f, i);
gen_call!(call_8ififiifi, i, f, i, f, i, i, f, i);
gen_call!(call_8ffifiifi, f, f, i, f, i, i, f, i);
gen_call!(call_8iiffiifi, i, i, f, f, i, i, f, i);
gen_call!(call_8fiffiifi, f, i, f, f, i, i, f, i);
gen_call!(call_8ifffiifi, i, f, f, f, i, i, f, i);
gen_call!(call_8ffffiifi, f, f, f, f, i, i, f, i);
gen_call!(call_8iiiififi, i, i, i, i, f, i, f, i);
gen_call!(call_8fiiififi, f, i, i, i, f, i, f, i);
gen_call!(call_8ifiififi, i, f, i, i, f, i, f, i);
gen_call!(call_8ffiififi, f, f, i, i, f, i, f, i);
gen_call!(call_8iifififi, i, i, f, i, f, i, f, i);
gen_call!(call_8fifififi, f, i, f, i, f, i, f, i);
gen_call!(call_8iffififi, i, f, f, i, f, i, f, i);
gen_call!(call_8fffififi, f, f, f, i, f, i, f, i);
gen_call!(call_8iiiffifi, i, i, i, f, f, i, f, i);
gen_call!(call_8fiiffifi, f, i, i, f, f, i, f, i);
gen_call!(call_8ififfifi, i, f, i, f, f, i, f, i);
gen_call!(call_8ffiffifi, f, f, i, f, f, i, f, i);
gen_call!(call_8iifffifi, i, i, f, f, f, i, f, i);
gen_call!(call_8fifffifi, f, i, f, f, f, i, f, i);
gen_call!(call_8iffffifi, i, f, f, f, f, i, f, i);
gen_call!(call_8fffffifi, f, f, f, f, f, i, f, i);
gen_call!(call_8iiiiiffi, i, i, i, i, i, f, f, i);
gen_call!(call_8fiiiiffi, f, i, i, i, i, f, f, i);
gen_call!(call_8ifiiiffi, i, f, i, i, i, f, f, i);
gen_call!(call_8ffiiiffi, f, f, i, i, i, f, f, i);
gen_call!(call_8iifiiffi, i, i, f, i, i, f, f, i);
gen_call!(call_8fifiiffi, f, i, f, i, i, f, f, i);
gen_call!(call_8iffiiffi, i, f, f, i, i, f, f, i);
gen_call!(call_8fffiiffi, f, f, f, i, i, f, f, i);
gen_call!(call_8iiififfi, i, i, i, f, i, f, f, i);
gen_call!(call_8fiififfi, f, i, i, f, i, f, f, i);
gen_call!(call_8ifififfi, i, f, i, f, i, f, f, i);
gen_call!(call_8ffififfi, f, f, i, f, i, f, f, i);
gen_call!(call_8iiffiffi, i, i, f, f, i, f, f, i);
gen_call!(call_8fiffiffi, f, i, f, f, i, f, f, i);
gen_call!(call_8ifffiffi, i, f, f, f, i, f, f, i);
gen_call!(call_8ffffiffi, f, f, f, f, i, f, f, i);
gen_call!(call_8iiiifffi, i, i, i, i, f, f, f, i);
gen_call!(call_8fiiifffi, f, i, i, i, f, f, f, i);
gen_call!(call_8ifiifffi, i, f, i, i, f, f, f, i);
gen_call!(call_8ffiifffi, f, f, i, i, f, f, f, i);
gen_call!(call_8iififffi, i, i, f, i, f, f, f, i);
gen_call!(call_8fififffi, f, i, f, i, f, f, f, i);
gen_call!(call_8iffifffi, i, f, f, i, f, f, f, i);
gen_call!(call_8fffifffi, f, f, f, i, f, f, f, i);
gen_call!(call_8iiiffffi, i, i, i, f, f, f, f, i);
gen_call!(call_8fiiffffi, f, i, i, f, f, f, f, i);
gen_call!(call_8ififfffi, i, f, i, f, f, f, f, i);
gen_call!(call_8ffiffffi, f, f, i, f, f, f, f, i);
gen_call!(call_8iifffffi, i, i, f, f, f, f, f, i);
gen_call!(call_8fifffffi, f, i, f, f, f, f, f, i);
gen_call!(call_8iffffffi, i, f, f, f, f, f, f, i);
gen_call!(call_8fffffffi, f, f, f, f, f, f, f, i);
gen_call!(call_8iiiiiiif, i, i, i, i, i, i, i, f);
gen_call!(call_8fiiiiiif, f, i, i, i, i, i, i, f);
gen_call!(call_8ifiiiiif, i, f, i, i, i, i, i, f);
gen_call!(call_8ffiiiiif, f, f, i, i, i, i, i, f);
gen_call!(call_8iifiiiif, i, i, f, i, i, i, i, f);
gen_call!(call_8fifiiiif, f, i, f, i, i, i, i, f);
gen_call!(call_8iffiiiif, i, f, f, i, i, i, i, f);
gen_call!(call_8fffiiiif, f, f, f, i, i, i, i, f);
gen_call!(call_8iiifiiif, i, i, i, f, i, i, i, f);
gen_call!(call_8fiifiiif, f, i, i, f, i, i, i, f);
gen_call!(call_8ififiiif, i, f, i, f, i, i, i, f);
gen_call!(call_8ffifiiif, f, f, i, f, i, i, i, f);
gen_call!(call_8iiffiiif, i, i, f, f, i, i, i, f);
gen_call!(call_8fiffiiif, f, i, f, f, i, i, i, f);
gen_call!(call_8ifffiiif, i, f, f, f, i, i, i, f);
gen_call!(call_8ffffiiif, f, f, f, f, i, i, i, f);
gen_call!(call_8iiiifiif, i, i, i, i, f, i, i, f);
gen_call!(call_8fiiifiif, f, i, i, i, f, i, i, f);
gen_call!(call_8ifiifiif, i, f, i, i, f, i, i, f);
gen_call!(call_8ffiifiif, f, f, i, i, f, i, i, f);
gen_call!(call_8iififiif, i, i, f, i, f, i, i, f);
gen_call!(call_8fififiif, f, i, f, i, f, i, i, f);
gen_call!(call_8iffifiif, i, f, f, i, f, i, i, f);
gen_call!(call_8fffifiif, f, f, f, i, f, i, i, f);
gen_call!(call_8iiiffiif, i, i, i, f, f, i, i, f);
gen_call!(call_8fiiffiif, f, i, i, f, f, i, i, f);
gen_call!(call_8ififfiif, i, f, i, f, f, i, i, f);
gen_call!(call_8ffiffiif, f, f, i, f, f, i, i, f);
gen_call!(call_8iifffiif, i, i, f, f, f, i, i, f);
gen_call!(call_8fifffiif, f, i, f, f, f, i, i, f);
gen_call!(call_8iffffiif, i, f, f, f, f, i, i, f);
gen_call!(call_8fffffiif, f, f, f, f, f, i, i, f);
gen_call!(call_8iiiiifif, i, i, i, i, i, f, i, f);
gen_call!(call_8fiiiifif, f, i, i, i, i, f, i, f);
gen_call!(call_8ifiiifif, i, f, i, i, i, f, i, f);
gen_call!(call_8ffiiifif, f, f, i, i, i, f, i, f);
gen_call!(call_8iifiifif, i, i, f, i, i, f, i, f);
gen_call!(call_8fifiifif, f, i, f, i, i, f, i, f);
gen_call!(call_8iffiifif, i, f, f, i, i, f, i, f);
gen_call!(call_8fffiifif, f, f, f, i, i, f, i, f);
gen_call!(call_8iiififif, i, i, i, f, i, f, i, f);
gen_call!(call_8fiififif, f, i, i, f, i, f, i, f);
gen_call!(call_8ifififif, i, f, i, f, i, f, i, f);
gen_call!(call_8ffififif, f, f, i, f, i, f, i, f);
gen_call!(call_8iiffifif, i, i, f, f, i, f, i, f);
gen_call!(call_8fiffifif, f, i, f, f, i, f, i, f);
gen_call!(call_8ifffifif, i, f, f, f, i, f, i, f);
gen_call!(call_8ffffifif, f, f, f, f, i, f, i, f);
gen_call!(call_8iiiiffif, i, i, i, i, f, f, i, f);
gen_call!(call_8fiiiffif, f, i, i, i, f, f, i, f);
gen_call!(call_8ifiiffif, i, f, i, i, f, f, i, f);
gen_call!(call_8ffiiffif, f, f, i, i, f, f, i, f);
gen_call!(call_8iififfif, i, i, f, i, f, f, i, f);
gen_call!(call_8fififfif, f, i, f, i, f, f, i, f);
gen_call!(call_8iffiffif, i, f, f, i, f, f, i, f);
gen_call!(call_8fffiffif, f, f, f, i, f, f, i, f);
gen_call!(call_8iiifffif, i, i, i, f, f, f, i, f);
gen_call!(call_8fiifffif, f, i, i, f, f, f, i, f);
gen_call!(call_8ififffif, i, f, i, f, f, f, i, f);
gen_call!(call_8ffifffif, f, f, i, f, f, f, i, f);
gen_call!(call_8iiffffif, i, i, f, f, f, f, i, f);
gen_call!(call_8fiffffif, f, i, f, f, f, f, i, f);
gen_call!(call_8ifffffif, i, f, f, f, f, f, i, f);
gen_call!(call_8ffffffif, f, f, f, f, f, f, i, f);
gen_call!(call_8iiiiiiff, i, i, i, i, i, i, f, f);
gen_call!(call_8fiiiiiff, f, i, i, i, i, i, f, f);
gen_call!(call_8ifiiiiff, i, f, i, i, i, i, f, f);
gen_call!(call_8ffiiiiff, f, f, i, i, i, i, f, f);
gen_call!(call_8iifiiiff, i, i, f, i, i, i, f, f);
gen_call!(call_8fifiiiff, f, i, f, i, i, i, f, f);
gen_call!(call_8iffiiiff, i, f, f, i, i, i, f, f);
gen_call!(call_8fffiiiff, f, f, f, i, i, i, f, f);
gen_call!(call_8iiifiiff, i, i, i, f, i, i, f, f);
gen_call!(call_8fiifiiff, f, i, i, f, i, i, f, f);
gen_call!(call_8ififiiff, i, f, i, f, i, i, f, f);
gen_call!(call_8ffifiiff, f, f, i, f, i, i, f, f);
gen_call!(call_8iiffiiff, i, i, f, f, i, i, f, f);
gen_call!(call_8fiffiiff, f, i, f, f, i, i, f, f);
gen_call!(call_8ifffiiff, i, f, f, f, i, i, f, f);
gen_call!(call_8ffffiiff, f, f, f, f, i, i, f, f);
gen_call!(call_8iiiififf, i, i, i, i, f, i, f, f);
gen_call!(call_8fiiififf, f, i, i, i, f, i, f, f);
gen_call!(call_8ifiififf, i, f, i, i, f, i, f, f);
gen_call!(call_8ffiififf, f, f, i, i, f, i, f, f);
gen_call!(call_8iifififf, i, i, f, i, f, i, f, f);
gen_call!(call_8fifififf, f, i, f, i, f, i, f, f);
gen_call!(call_8iffififf, i, f, f, i, f, i, f, f);
gen_call!(call_8fffififf, f, f, f, i, f, i, f, f);
gen_call!(call_8iiiffiff, i, i, i, f, f, i, f, f);
gen_call!(call_8fiiffiff, f, i, i, f, f, i, f, f);
gen_call!(call_8ififfiff, i, f, i, f, f, i, f, f);
gen_call!(call_8ffiffiff, f, f, i, f, f, i, f, f);
gen_call!(call_8iifffiff, i, i, f, f, f, i, f, f);
gen_call!(call_8fifffiff, f, i, f, f, f, i, f, f);
gen_call!(call_8iffffiff, i, f, f, f, f, i, f, f);
gen_call!(call_8fffffiff, f, f, f, f, f, i, f, f);
gen_call!(call_8iiiiifff, i, i, i, i, i, f, f, f);
gen_call!(call_8fiiiifff, f, i, i, i, i, f, f, f);
gen_call!(call_8ifiiifff, i, f, i, i, i, f, f, f);
gen_call!(call_8ffiiifff, f, f, i, i, i, f, f, f);
gen_call!(call_8iifiifff, i, i, f, i, i, f, f, f);
gen_call!(call_8fifiifff, f, i, f, i, i, f, f, f);
gen_call!(call_8iffiifff, i, f, f, i, i, f, f, f);
gen_call!(call_8fffiifff, f, f, f, i, i, f, f, f);
gen_call!(call_8iiififff, i, i, i, f, i, f, f, f);
gen_call!(call_8fiififff, f, i, i, f, i, f, f, f);
gen_call!(call_8ifififff, i, f, i, f, i, f, f, f);
gen_call!(call_8ffififff, f, f, i, f, i, f, f, f);
gen_call!(call_8iiffifff, i, i, f, f, i, f, f, f);
gen_call!(call_8fiffifff, f, i, f, f, i, f, f, f);
gen_call!(call_8ifffifff, i, f, f, f, i, f, f, f);
gen_call!(call_8ffffifff, f, f, f, f, i, f, f, f);
gen_call!(call_8iiiiffff, i, i, i, i, f, f, f, f);
gen_call!(call_8fiiiffff, f, i, i, i, f, f, f, f);
gen_call!(call_8ifiiffff, i, f, i, i, f, f, f, f);
gen_call!(call_8ffiiffff, f, f, i, i, f, f, f, f);
gen_call!(call_8iififfff, i, i, f, i, f, f, f, f);
gen_call!(call_8fififfff, f, i, f, i, f, f, f, f);
gen_call!(call_8iffiffff, i, f, f, i, f, f, f, f);
gen_call!(call_8fffiffff, f, f, f, i, f, f, f, f);
gen_call!(call_8iiifffff, i, i, i, f, f, f, f, f);
gen_call!(call_8fiifffff, f, i, i, f, f, f, f, f);
gen_call!(call_8ififffff, i, f, i, f, f, f, f, f);
gen_call!(call_8ffifffff, f, f, i, f, f, f, f, f);
gen_call!(call_8iiffffff, i, i, f, f, f, f, f, f);
gen_call!(call_8fiffffff, f, i, f, f, f, f, f, f);
gen_call!(call_8ifffffff, i, f, f, f, f, f, f, f);
gen_call!(call_8ffffffff, f, f, f, f, f, f, f, f);
gen_call!(call_9i, i, i, i, i, i, i, i, i, i);
gen_call!(call_9f, f, f, f, f, f, f, f, f, f);
gen_call!(call_10i, i, i, i, i, i, i, i, i, i, i);
gen_call!(call_10f, f, f, f, f, f, f, f, f, f, f);
gen_call!(call_11i, i, i, i, i, i, i, i, i, i, i, i);
gen_call!(call_11f, f, f, f, f, f, f, f, f, f, f, f);
gen_call!(call_12i, i, i, i, i, i, i, i, i, i, i, i, i);
gen_call!(call_12f, f, f, f, f, f, f, f, f, f, f, f, f);

/// A monomorphised dispatch stub: marshals an arg-slot slice through a
/// reified `extern "C"` signature and returns the boxed result.
pub(crate) type StubFn = unsafe fn(*const u8, &[Slot], JitKind) -> Option<Value>;

/// Arity-0 stub (no `gen_call!` slot to bind).
unsafe fn call_0(ptr: *const u8, _s: &[Slot], ret: JitKind) -> Option<Value> {
    call_through!(ptr, ret, [])
}

/// Returns the stub for an `(arity, shape)` pair, or `None` for a shape
/// the trampoline doesn't cover. Mirrors the `match` in `invoke_prepared`.
pub(crate) fn resolve_stub(arity: usize, shape: u32) -> Option<StubFn> {
    let f: StubFn = match (arity, shape) {
        (0, _) => call_0,
        (1, 0b0) => call_1i,
        (1, 0b1) => call_1f,
        (2, 0b00) => call_2ii,
        (2, 0b01) => call_2fi,
        (2, 0b10) => call_2if,
        (2, 0b11) => call_2ff,
        (3, 0b000) => call_3iii,
        (3, 0b001) => call_3fii,
        (3, 0b010) => call_3ifi,
        (3, 0b011) => call_3ffi,
        (3, 0b100) => call_3iif,
        (3, 0b101) => call_3fif,
        (3, 0b110) => call_3iff,
        (3, 0b111) => call_3fff,
        (4, 0b0000) => call_4iiii,
        (4, 0b0001) => call_4fiii,
        (4, 0b0010) => call_4ifii,
        (4, 0b0011) => call_4ffii,
        (4, 0b0100) => call_4iifi,
        (4, 0b0101) => call_4fifi,
        (4, 0b0110) => call_4iffi,
        (4, 0b0111) => call_4fffi,
        (4, 0b1000) => call_4iiif,
        (4, 0b1001) => call_4fiif,
        (4, 0b1010) => call_4ifif,
        (4, 0b1011) => call_4ffif,
        (4, 0b1100) => call_4iiff,
        (4, 0b1101) => call_4fiff,
        (4, 0b1110) => call_4ifff,
        (4, 0b1111) => call_4ffff,
        (5, 0b00000) => call_5iiiii,
        (5, 0b00001) => call_5fiiii,
        (5, 0b00010) => call_5ifiii,
        (5, 0b00011) => call_5ffiii,
        (5, 0b00100) => call_5iifii,
        (5, 0b00101) => call_5fifii,
        (5, 0b00110) => call_5iffii,
        (5, 0b00111) => call_5fffii,
        (5, 0b01000) => call_5iiifi,
        (5, 0b01001) => call_5fiifi,
        (5, 0b01010) => call_5ififi,
        (5, 0b01011) => call_5ffifi,
        (5, 0b01100) => call_5iiffi,
        (5, 0b01101) => call_5fiffi,
        (5, 0b01110) => call_5ifffi,
        (5, 0b01111) => call_5ffffi,
        (5, 0b10000) => call_5iiiif,
        (5, 0b10001) => call_5fiiif,
        (5, 0b10010) => call_5ifiif,
        (5, 0b10011) => call_5ffiif,
        (5, 0b10100) => call_5iifif,
        (5, 0b10101) => call_5fifif,
        (5, 0b10110) => call_5iffif,
        (5, 0b10111) => call_5fffif,
        (5, 0b11000) => call_5iiiff,
        (5, 0b11001) => call_5fiiff,
        (5, 0b11010) => call_5ififf,
        (5, 0b11011) => call_5ffiff,
        (5, 0b11100) => call_5iifff,
        (5, 0b11101) => call_5fifff,
        (5, 0b11110) => call_5iffff,
        (5, 0b11111) => call_5fffff,
        (6, 0b000000) => call_6iiiiii,
        (6, 0b000001) => call_6fiiiii,
        (6, 0b000010) => call_6ifiiii,
        (6, 0b000011) => call_6ffiiii,
        (6, 0b000100) => call_6iifiii,
        (6, 0b000101) => call_6fifiii,
        (6, 0b000110) => call_6iffiii,
        (6, 0b000111) => call_6fffiii,
        (6, 0b001000) => call_6iiifii,
        (6, 0b001001) => call_6fiifii,
        (6, 0b001010) => call_6ififii,
        (6, 0b001011) => call_6ffifii,
        (6, 0b001100) => call_6iiffii,
        (6, 0b001101) => call_6fiffii,
        (6, 0b001110) => call_6ifffii,
        (6, 0b001111) => call_6ffffii,
        (6, 0b010000) => call_6iiiifi,
        (6, 0b010001) => call_6fiiifi,
        (6, 0b010010) => call_6ifiifi,
        (6, 0b010011) => call_6ffiifi,
        (6, 0b010100) => call_6iififi,
        (6, 0b010101) => call_6fififi,
        (6, 0b010110) => call_6iffifi,
        (6, 0b010111) => call_6fffifi,
        (6, 0b011000) => call_6iiiffi,
        (6, 0b011001) => call_6fiiffi,
        (6, 0b011010) => call_6ififfi,
        (6, 0b011011) => call_6ffiffi,
        (6, 0b011100) => call_6iifffi,
        (6, 0b011101) => call_6fifffi,
        (6, 0b011110) => call_6iffffi,
        (6, 0b011111) => call_6fffffi,
        (6, 0b100000) => call_6iiiiif,
        (6, 0b100001) => call_6fiiiif,
        (6, 0b100010) => call_6ifiiif,
        (6, 0b100011) => call_6ffiiif,
        (6, 0b100100) => call_6iifiif,
        (6, 0b100101) => call_6fifiif,
        (6, 0b100110) => call_6iffiif,
        (6, 0b100111) => call_6fffiif,
        (6, 0b101000) => call_6iiifif,
        (6, 0b101001) => call_6fiifif,
        (6, 0b101010) => call_6ififif,
        (6, 0b101011) => call_6ffifif,
        (6, 0b101100) => call_6iiffif,
        (6, 0b101101) => call_6fiffif,
        (6, 0b101110) => call_6ifffif,
        (6, 0b101111) => call_6ffffif,
        (6, 0b110000) => call_6iiiiff,
        (6, 0b110001) => call_6fiiiff,
        (6, 0b110010) => call_6ifiiff,
        (6, 0b110011) => call_6ffiiff,
        (6, 0b110100) => call_6iififf,
        (6, 0b110101) => call_6fififf,
        (6, 0b110110) => call_6iffiff,
        (6, 0b110111) => call_6fffiff,
        (6, 0b111000) => call_6iiifff,
        (6, 0b111001) => call_6fiifff,
        (6, 0b111010) => call_6ififff,
        (6, 0b111011) => call_6ffifff,
        (6, 0b111100) => call_6iiffff,
        (6, 0b111101) => call_6fiffff,
        (6, 0b111110) => call_6ifffff,
        (6, 0b111111) => call_6ffffff,
        (7, 0b0000000) => call_7iiiiiii,
        (7, 0b0000001) => call_7fiiiiii,
        (7, 0b0000010) => call_7ifiiiii,
        (7, 0b0000011) => call_7ffiiiii,
        (7, 0b0000100) => call_7iifiiii,
        (7, 0b0000101) => call_7fifiiii,
        (7, 0b0000110) => call_7iffiiii,
        (7, 0b0000111) => call_7fffiiii,
        (7, 0b0001000) => call_7iiifiii,
        (7, 0b0001001) => call_7fiifiii,
        (7, 0b0001010) => call_7ififiii,
        (7, 0b0001011) => call_7ffifiii,
        (7, 0b0001100) => call_7iiffiii,
        (7, 0b0001101) => call_7fiffiii,
        (7, 0b0001110) => call_7ifffiii,
        (7, 0b0001111) => call_7ffffiii,
        (7, 0b0010000) => call_7iiiifii,
        (7, 0b0010001) => call_7fiiifii,
        (7, 0b0010010) => call_7ifiifii,
        (7, 0b0010011) => call_7ffiifii,
        (7, 0b0010100) => call_7iififii,
        (7, 0b0010101) => call_7fififii,
        (7, 0b0010110) => call_7iffifii,
        (7, 0b0010111) => call_7fffifii,
        (7, 0b0011000) => call_7iiiffii,
        (7, 0b0011001) => call_7fiiffii,
        (7, 0b0011010) => call_7ififfii,
        (7, 0b0011011) => call_7ffiffii,
        (7, 0b0011100) => call_7iifffii,
        (7, 0b0011101) => call_7fifffii,
        (7, 0b0011110) => call_7iffffii,
        (7, 0b0011111) => call_7fffffii,
        (7, 0b0100000) => call_7iiiiifi,
        (7, 0b0100001) => call_7fiiiifi,
        (7, 0b0100010) => call_7ifiiifi,
        (7, 0b0100011) => call_7ffiiifi,
        (7, 0b0100100) => call_7iifiifi,
        (7, 0b0100101) => call_7fifiifi,
        (7, 0b0100110) => call_7iffiifi,
        (7, 0b0100111) => call_7fffiifi,
        (7, 0b0101000) => call_7iiififi,
        (7, 0b0101001) => call_7fiififi,
        (7, 0b0101010) => call_7ifififi,
        (7, 0b0101011) => call_7ffififi,
        (7, 0b0101100) => call_7iiffifi,
        (7, 0b0101101) => call_7fiffifi,
        (7, 0b0101110) => call_7ifffifi,
        (7, 0b0101111) => call_7ffffifi,
        (7, 0b0110000) => call_7iiiiffi,
        (7, 0b0110001) => call_7fiiiffi,
        (7, 0b0110010) => call_7ifiiffi,
        (7, 0b0110011) => call_7ffiiffi,
        (7, 0b0110100) => call_7iififfi,
        (7, 0b0110101) => call_7fififfi,
        (7, 0b0110110) => call_7iffiffi,
        (7, 0b0110111) => call_7fffiffi,
        (7, 0b0111000) => call_7iiifffi,
        (7, 0b0111001) => call_7fiifffi,
        (7, 0b0111010) => call_7ififffi,
        (7, 0b0111011) => call_7ffifffi,
        (7, 0b0111100) => call_7iiffffi,
        (7, 0b0111101) => call_7fiffffi,
        (7, 0b0111110) => call_7ifffffi,
        (7, 0b0111111) => call_7ffffffi,
        (7, 0b1000000) => call_7iiiiiif,
        (7, 0b1000001) => call_7fiiiiif,
        (7, 0b1000010) => call_7ifiiiif,
        (7, 0b1000011) => call_7ffiiiif,
        (7, 0b1000100) => call_7iifiiif,
        (7, 0b1000101) => call_7fifiiif,
        (7, 0b1000110) => call_7iffiiif,
        (7, 0b1000111) => call_7fffiiif,
        (7, 0b1001000) => call_7iiifiif,
        (7, 0b1001001) => call_7fiifiif,
        (7, 0b1001010) => call_7ififiif,
        (7, 0b1001011) => call_7ffifiif,
        (7, 0b1001100) => call_7iiffiif,
        (7, 0b1001101) => call_7fiffiif,
        (7, 0b1001110) => call_7ifffiif,
        (7, 0b1001111) => call_7ffffiif,
        (7, 0b1010000) => call_7iiiifif,
        (7, 0b1010001) => call_7fiiifif,
        (7, 0b1010010) => call_7ifiifif,
        (7, 0b1010011) => call_7ffiifif,
        (7, 0b1010100) => call_7iififif,
        (7, 0b1010101) => call_7fififif,
        (7, 0b1010110) => call_7iffifif,
        (7, 0b1010111) => call_7fffifif,
        (7, 0b1011000) => call_7iiiffif,
        (7, 0b1011001) => call_7fiiffif,
        (7, 0b1011010) => call_7ififfif,
        (7, 0b1011011) => call_7ffiffif,
        (7, 0b1011100) => call_7iifffif,
        (7, 0b1011101) => call_7fifffif,
        (7, 0b1011110) => call_7iffffif,
        (7, 0b1011111) => call_7fffffif,
        (7, 0b1100000) => call_7iiiiiff,
        (7, 0b1100001) => call_7fiiiiff,
        (7, 0b1100010) => call_7ifiiiff,
        (7, 0b1100011) => call_7ffiiiff,
        (7, 0b1100100) => call_7iifiiff,
        (7, 0b1100101) => call_7fifiiff,
        (7, 0b1100110) => call_7iffiiff,
        (7, 0b1100111) => call_7fffiiff,
        (7, 0b1101000) => call_7iiififf,
        (7, 0b1101001) => call_7fiififf,
        (7, 0b1101010) => call_7ifififf,
        (7, 0b1101011) => call_7ffififf,
        (7, 0b1101100) => call_7iiffiff,
        (7, 0b1101101) => call_7fiffiff,
        (7, 0b1101110) => call_7ifffiff,
        (7, 0b1101111) => call_7ffffiff,
        (7, 0b1110000) => call_7iiiifff,
        (7, 0b1110001) => call_7fiiifff,
        (7, 0b1110010) => call_7ifiifff,
        (7, 0b1110011) => call_7ffiifff,
        (7, 0b1110100) => call_7iififff,
        (7, 0b1110101) => call_7fififff,
        (7, 0b1110110) => call_7iffifff,
        (7, 0b1110111) => call_7fffifff,
        (7, 0b1111000) => call_7iiiffff,
        (7, 0b1111001) => call_7fiiffff,
        (7, 0b1111010) => call_7ififfff,
        (7, 0b1111011) => call_7ffiffff,
        (7, 0b1111100) => call_7iifffff,
        (7, 0b1111101) => call_7fifffff,
        (7, 0b1111110) => call_7iffffff,
        (7, 0b1111111) => call_7fffffff,
        (8, 0b00000000) => call_8iiiiiiii,
        (8, 0b00000001) => call_8fiiiiiii,
        (8, 0b00000010) => call_8ifiiiiii,
        (8, 0b00000011) => call_8ffiiiiii,
        (8, 0b00000100) => call_8iifiiiii,
        (8, 0b00000101) => call_8fifiiiii,
        (8, 0b00000110) => call_8iffiiiii,
        (8, 0b00000111) => call_8fffiiiii,
        (8, 0b00001000) => call_8iiifiiii,
        (8, 0b00001001) => call_8fiifiiii,
        (8, 0b00001010) => call_8ififiiii,
        (8, 0b00001011) => call_8ffifiiii,
        (8, 0b00001100) => call_8iiffiiii,
        (8, 0b00001101) => call_8fiffiiii,
        (8, 0b00001110) => call_8ifffiiii,
        (8, 0b00001111) => call_8ffffiiii,
        (8, 0b00010000) => call_8iiiifiii,
        (8, 0b00010001) => call_8fiiifiii,
        (8, 0b00010010) => call_8ifiifiii,
        (8, 0b00010011) => call_8ffiifiii,
        (8, 0b00010100) => call_8iififiii,
        (8, 0b00010101) => call_8fififiii,
        (8, 0b00010110) => call_8iffifiii,
        (8, 0b00010111) => call_8fffifiii,
        (8, 0b00011000) => call_8iiiffiii,
        (8, 0b00011001) => call_8fiiffiii,
        (8, 0b00011010) => call_8ififfiii,
        (8, 0b00011011) => call_8ffiffiii,
        (8, 0b00011100) => call_8iifffiii,
        (8, 0b00011101) => call_8fifffiii,
        (8, 0b00011110) => call_8iffffiii,
        (8, 0b00011111) => call_8fffffiii,
        (8, 0b00100000) => call_8iiiiifii,
        (8, 0b00100001) => call_8fiiiifii,
        (8, 0b00100010) => call_8ifiiifii,
        (8, 0b00100011) => call_8ffiiifii,
        (8, 0b00100100) => call_8iifiifii,
        (8, 0b00100101) => call_8fifiifii,
        (8, 0b00100110) => call_8iffiifii,
        (8, 0b00100111) => call_8fffiifii,
        (8, 0b00101000) => call_8iiififii,
        (8, 0b00101001) => call_8fiififii,
        (8, 0b00101010) => call_8ifififii,
        (8, 0b00101011) => call_8ffififii,
        (8, 0b00101100) => call_8iiffifii,
        (8, 0b00101101) => call_8fiffifii,
        (8, 0b00101110) => call_8ifffifii,
        (8, 0b00101111) => call_8ffffifii,
        (8, 0b00110000) => call_8iiiiffii,
        (8, 0b00110001) => call_8fiiiffii,
        (8, 0b00110010) => call_8ifiiffii,
        (8, 0b00110011) => call_8ffiiffii,
        (8, 0b00110100) => call_8iififfii,
        (8, 0b00110101) => call_8fififfii,
        (8, 0b00110110) => call_8iffiffii,
        (8, 0b00110111) => call_8fffiffii,
        (8, 0b00111000) => call_8iiifffii,
        (8, 0b00111001) => call_8fiifffii,
        (8, 0b00111010) => call_8ififffii,
        (8, 0b00111011) => call_8ffifffii,
        (8, 0b00111100) => call_8iiffffii,
        (8, 0b00111101) => call_8fiffffii,
        (8, 0b00111110) => call_8ifffffii,
        (8, 0b00111111) => call_8ffffffii,
        (8, 0b01000000) => call_8iiiiiifi,
        (8, 0b01000001) => call_8fiiiiifi,
        (8, 0b01000010) => call_8ifiiiifi,
        (8, 0b01000011) => call_8ffiiiifi,
        (8, 0b01000100) => call_8iifiiifi,
        (8, 0b01000101) => call_8fifiiifi,
        (8, 0b01000110) => call_8iffiiifi,
        (8, 0b01000111) => call_8fffiiifi,
        (8, 0b01001000) => call_8iiifiifi,
        (8, 0b01001001) => call_8fiifiifi,
        (8, 0b01001010) => call_8ififiifi,
        (8, 0b01001011) => call_8ffifiifi,
        (8, 0b01001100) => call_8iiffiifi,
        (8, 0b01001101) => call_8fiffiifi,
        (8, 0b01001110) => call_8ifffiifi,
        (8, 0b01001111) => call_8ffffiifi,
        (8, 0b01010000) => call_8iiiififi,
        (8, 0b01010001) => call_8fiiififi,
        (8, 0b01010010) => call_8ifiififi,
        (8, 0b01010011) => call_8ffiififi,
        (8, 0b01010100) => call_8iifififi,
        (8, 0b01010101) => call_8fifififi,
        (8, 0b01010110) => call_8iffififi,
        (8, 0b01010111) => call_8fffififi,
        (8, 0b01011000) => call_8iiiffifi,
        (8, 0b01011001) => call_8fiiffifi,
        (8, 0b01011010) => call_8ififfifi,
        (8, 0b01011011) => call_8ffiffifi,
        (8, 0b01011100) => call_8iifffifi,
        (8, 0b01011101) => call_8fifffifi,
        (8, 0b01011110) => call_8iffffifi,
        (8, 0b01011111) => call_8fffffifi,
        (8, 0b01100000) => call_8iiiiiffi,
        (8, 0b01100001) => call_8fiiiiffi,
        (8, 0b01100010) => call_8ifiiiffi,
        (8, 0b01100011) => call_8ffiiiffi,
        (8, 0b01100100) => call_8iifiiffi,
        (8, 0b01100101) => call_8fifiiffi,
        (8, 0b01100110) => call_8iffiiffi,
        (8, 0b01100111) => call_8fffiiffi,
        (8, 0b01101000) => call_8iiififfi,
        (8, 0b01101001) => call_8fiififfi,
        (8, 0b01101010) => call_8ifififfi,
        (8, 0b01101011) => call_8ffififfi,
        (8, 0b01101100) => call_8iiffiffi,
        (8, 0b01101101) => call_8fiffiffi,
        (8, 0b01101110) => call_8ifffiffi,
        (8, 0b01101111) => call_8ffffiffi,
        (8, 0b01110000) => call_8iiiifffi,
        (8, 0b01110001) => call_8fiiifffi,
        (8, 0b01110010) => call_8ifiifffi,
        (8, 0b01110011) => call_8ffiifffi,
        (8, 0b01110100) => call_8iififffi,
        (8, 0b01110101) => call_8fififffi,
        (8, 0b01110110) => call_8iffifffi,
        (8, 0b01110111) => call_8fffifffi,
        (8, 0b01111000) => call_8iiiffffi,
        (8, 0b01111001) => call_8fiiffffi,
        (8, 0b01111010) => call_8ififfffi,
        (8, 0b01111011) => call_8ffiffffi,
        (8, 0b01111100) => call_8iifffffi,
        (8, 0b01111101) => call_8fifffffi,
        (8, 0b01111110) => call_8iffffffi,
        (8, 0b01111111) => call_8fffffffi,
        (8, 0b10000000) => call_8iiiiiiif,
        (8, 0b10000001) => call_8fiiiiiif,
        (8, 0b10000010) => call_8ifiiiiif,
        (8, 0b10000011) => call_8ffiiiiif,
        (8, 0b10000100) => call_8iifiiiif,
        (8, 0b10000101) => call_8fifiiiif,
        (8, 0b10000110) => call_8iffiiiif,
        (8, 0b10000111) => call_8fffiiiif,
        (8, 0b10001000) => call_8iiifiiif,
        (8, 0b10001001) => call_8fiifiiif,
        (8, 0b10001010) => call_8ififiiif,
        (8, 0b10001011) => call_8ffifiiif,
        (8, 0b10001100) => call_8iiffiiif,
        (8, 0b10001101) => call_8fiffiiif,
        (8, 0b10001110) => call_8ifffiiif,
        (8, 0b10001111) => call_8ffffiiif,
        (8, 0b10010000) => call_8iiiifiif,
        (8, 0b10010001) => call_8fiiifiif,
        (8, 0b10010010) => call_8ifiifiif,
        (8, 0b10010011) => call_8ffiifiif,
        (8, 0b10010100) => call_8iififiif,
        (8, 0b10010101) => call_8fififiif,
        (8, 0b10010110) => call_8iffifiif,
        (8, 0b10010111) => call_8fffifiif,
        (8, 0b10011000) => call_8iiiffiif,
        (8, 0b10011001) => call_8fiiffiif,
        (8, 0b10011010) => call_8ififfiif,
        (8, 0b10011011) => call_8ffiffiif,
        (8, 0b10011100) => call_8iifffiif,
        (8, 0b10011101) => call_8fifffiif,
        (8, 0b10011110) => call_8iffffiif,
        (8, 0b10011111) => call_8fffffiif,
        (8, 0b10100000) => call_8iiiiifif,
        (8, 0b10100001) => call_8fiiiifif,
        (8, 0b10100010) => call_8ifiiifif,
        (8, 0b10100011) => call_8ffiiifif,
        (8, 0b10100100) => call_8iifiifif,
        (8, 0b10100101) => call_8fifiifif,
        (8, 0b10100110) => call_8iffiifif,
        (8, 0b10100111) => call_8fffiifif,
        (8, 0b10101000) => call_8iiififif,
        (8, 0b10101001) => call_8fiififif,
        (8, 0b10101010) => call_8ifififif,
        (8, 0b10101011) => call_8ffififif,
        (8, 0b10101100) => call_8iiffifif,
        (8, 0b10101101) => call_8fiffifif,
        (8, 0b10101110) => call_8ifffifif,
        (8, 0b10101111) => call_8ffffifif,
        (8, 0b10110000) => call_8iiiiffif,
        (8, 0b10110001) => call_8fiiiffif,
        (8, 0b10110010) => call_8ifiiffif,
        (8, 0b10110011) => call_8ffiiffif,
        (8, 0b10110100) => call_8iififfif,
        (8, 0b10110101) => call_8fififfif,
        (8, 0b10110110) => call_8iffiffif,
        (8, 0b10110111) => call_8fffiffif,
        (8, 0b10111000) => call_8iiifffif,
        (8, 0b10111001) => call_8fiifffif,
        (8, 0b10111010) => call_8ififffif,
        (8, 0b10111011) => call_8ffifffif,
        (8, 0b10111100) => call_8iiffffif,
        (8, 0b10111101) => call_8fiffffif,
        (8, 0b10111110) => call_8ifffffif,
        (8, 0b10111111) => call_8ffffffif,
        (8, 0b11000000) => call_8iiiiiiff,
        (8, 0b11000001) => call_8fiiiiiff,
        (8, 0b11000010) => call_8ifiiiiff,
        (8, 0b11000011) => call_8ffiiiiff,
        (8, 0b11000100) => call_8iifiiiff,
        (8, 0b11000101) => call_8fifiiiff,
        (8, 0b11000110) => call_8iffiiiff,
        (8, 0b11000111) => call_8fffiiiff,
        (8, 0b11001000) => call_8iiifiiff,
        (8, 0b11001001) => call_8fiifiiff,
        (8, 0b11001010) => call_8ififiiff,
        (8, 0b11001011) => call_8ffifiiff,
        (8, 0b11001100) => call_8iiffiiff,
        (8, 0b11001101) => call_8fiffiiff,
        (8, 0b11001110) => call_8ifffiiff,
        (8, 0b11001111) => call_8ffffiiff,
        (8, 0b11010000) => call_8iiiififf,
        (8, 0b11010001) => call_8fiiififf,
        (8, 0b11010010) => call_8ifiififf,
        (8, 0b11010011) => call_8ffiififf,
        (8, 0b11010100) => call_8iifififf,
        (8, 0b11010101) => call_8fifififf,
        (8, 0b11010110) => call_8iffififf,
        (8, 0b11010111) => call_8fffififf,
        (8, 0b11011000) => call_8iiiffiff,
        (8, 0b11011001) => call_8fiiffiff,
        (8, 0b11011010) => call_8ififfiff,
        (8, 0b11011011) => call_8ffiffiff,
        (8, 0b11011100) => call_8iifffiff,
        (8, 0b11011101) => call_8fifffiff,
        (8, 0b11011110) => call_8iffffiff,
        (8, 0b11011111) => call_8fffffiff,
        (8, 0b11100000) => call_8iiiiifff,
        (8, 0b11100001) => call_8fiiiifff,
        (8, 0b11100010) => call_8ifiiifff,
        (8, 0b11100011) => call_8ffiiifff,
        (8, 0b11100100) => call_8iifiifff,
        (8, 0b11100101) => call_8fifiifff,
        (8, 0b11100110) => call_8iffiifff,
        (8, 0b11100111) => call_8fffiifff,
        (8, 0b11101000) => call_8iiififff,
        (8, 0b11101001) => call_8fiififff,
        (8, 0b11101010) => call_8ifififff,
        (8, 0b11101011) => call_8ffififff,
        (8, 0b11101100) => call_8iiffifff,
        (8, 0b11101101) => call_8fiffifff,
        (8, 0b11101110) => call_8ifffifff,
        (8, 0b11101111) => call_8ffffifff,
        (8, 0b11110000) => call_8iiiiffff,
        (8, 0b11110001) => call_8fiiiffff,
        (8, 0b11110010) => call_8ifiiffff,
        (8, 0b11110011) => call_8ffiiffff,
        (8, 0b11110100) => call_8iififfff,
        (8, 0b11110101) => call_8fififfff,
        (8, 0b11110110) => call_8iffiffff,
        (8, 0b11110111) => call_8fffiffff,
        (8, 0b11111000) => call_8iiifffff,
        (8, 0b11111001) => call_8fiifffff,
        (8, 0b11111010) => call_8ififffff,
        (8, 0b11111011) => call_8ffifffff,
        (8, 0b11111100) => call_8iiffffff,
        (8, 0b11111101) => call_8fiffffff,
        (8, 0b11111110) => call_8ifffffff,
        (8, 0b11111111) => call_8ffffffff,
        (9, 0b000000000) => call_9i,
        (9, 0b111111111) => call_9f,
        (10, 0b0000000000) => call_10i,
        (10, 0b1111111111) => call_10f,
        (11, 0b00000000000) => call_11i,
        (11, 0b11111111111) => call_11f,
        (12, 0b000000000000) => call_12i,
        (12, 0b111111111111) => call_12f,
        _ => return None,
    };
    Some(f)
}

/// Pre-resolved dispatch data for one [`JitFn`], computed once at
/// override-resolution time so the hot path skips the per-call bitmask
/// and the `(arity, shape)` match.
pub(crate) struct Prepared {
    /// The native entry plus its slot kinds.
    pub(crate) jit: std::sync::Arc<JitFn>,
    /// The stub resolved for this body's `(arity, shape)`.
    stub: StubFn,
    /// Return kind with `EnumPtr` and native aggregates canonicalised to
    /// `I64` (the stub reads a raw integer register; the post-call step
    /// re-wraps it).
    ret_kind: JitKind,
    /// `Some(shape_idx)` when the real return was `EnumPtr`.
    enum_return: Option<u32>,
    /// `Some(ok_shape_idx)` when the real return was `ResultEnumPtr`: the
    /// stub yields the `[disc, payload]` carrier tuple and the native path
    /// decodes the `Ok` enum (this shape) / `Err` error and frees it.
    result_enum: Option<u32>,
    /// `Some(kind)` when the real return was a native aggregate
    /// (`NativeStr` / native `Vec` / `StructPtr`); the raw pointer is read
    /// back and freed after the call when it owns a runtime allocation.
    native_return: Option<JitKind>,
    /// `Some(shape_idx)` when the real return is a registered flat struct.
    /// The compiled body uses its sret ABI, so dispatch provides a
    /// caller-owned flat field block as a hidden trailing parameter and
    /// decodes it after the call. The shape index sizes and types that block.
    struct_return: Option<u32>,
    /// `Some(elems)` when the real return was a 2-tuple: the stub yields a
    /// pointer to the `gos_rt_aggr_alloc` block (`[elem@i*8]`); the native
    /// path reads each slot per `TupleElem`, builds a `Value::Tuple`, and
    /// frees the block (shallow - element ownership transfers to the tuple).
    tuple_return: Option<[TupleElem; 2]>,
    /// `true` when any param or the return is a native aggregate, routing
    /// dispatch through `invoke_prepared_native`. The scalar/enum fast
    /// path (no native marshalling, no per-call allocation) stays
    /// untouched when this is `false`.
    has_native: bool,
    /// Set after the first full-shape marshal succeeds. Subsequent
    /// calls trust the proven scalar shapes (the type checker fixed the
    /// call site) and skip the per-kind `match`, still re-checking
    /// `EnumPtr` shape indices and falling back on any surprise.
    verified: std::cell::Cell<bool>,
    /// `true` once any call marshalled successfully (native hit). A body
    /// that has ever run native is never demoted - it pays its way.
    ever_hit: std::cell::Cell<bool>,
    /// Consecutive marshal failures for a body that has *never* hit
    /// native. Used to demote a permanently-unmarshallable body (e.g. one
    /// whose enum args arrive as bytecode `Value::Variant`, which the
    /// scalar/enum ABI cannot accept) so the per-call marshal attempt
    /// stops taxing every call. Reset is unnecessary: once `ever_hit` is
    /// set the count is ignored.
    miss_streak: std::cell::Cell<u32>,
}

/// Consecutive marshal failures, on a body that has never run native,
/// after which it is demoted back to bytecode-only. Small enough that the
/// wasted-attempt tax is negligible, large enough to ride out a brief
/// warm-up where a constructor has not yet produced native values.
pub(crate) const JIT_DEMOTE_MISS_STREAK: u32 = 8;

impl Prepared {
    /// Records a successful native call; the body is now never demoted.
    pub(crate) fn record_hit(&self) {
        self.ever_hit.set(true);
    }

    /// Records a marshal fallback and returns `true` when the body should
    /// be demoted (never hit native, and the miss streak crossed the
    /// threshold). A body that has ever hit native never demotes.
    pub(crate) fn record_fallback_should_demote(&self) -> bool {
        if self.ever_hit.get() {
            return false;
        }
        let streak = self.miss_streak.get() + 1;
        self.miss_streak.set(streak);
        streak >= JIT_DEMOTE_MISS_STREAK
    }
}

/// Logs, under `GOS_JIT_TRACE`, that a promoted body could not be prepared
/// for native dispatch and will run on bytecode. Without this a body that
/// promoted but whose shape the trampoline cannot marshal is invisible.
fn trace_prepare_fail(name: &str, reason: &str) {
    if jit_trace() {
        eprintln!("jit: prepare failed for {name} ({reason})");
    }
}

/// Resolves dispatch data for `jit`, or `None` if the trampoline can't
/// cover its shape (the caller then keeps the body on bytecode).
pub(crate) fn prepare(jit: std::sync::Arc<JitFn>) -> Option<Prepared> {
    // Structural returns add a hidden sret pointer arg, so the body's native
    // arity is one more than its user param count.
    let is_sret_return = matches!(jit.returns, JitKind::TupleReturn(_) | JitKind::StructPtr(_));
    let native_arity = jit.params.len() + usize::from(is_sret_return);
    if native_arity > MAX_ARGS {
        trace_prepare_fail(&jit.name, "native arity exceeds MAX_ARGS");
        return None;
    }
    let mut shape: u32 = 0;
    for (i, k) in jit.params.iter().enumerate() {
        if matches!(k, JitKind::F64) {
            shape |= 1 << i;
        }
    }
    let Some(stub) = resolve_stub(native_arity, shape) else {
        trace_prepare_fail(
            &jit.name,
            &format!("no dispatch stub for arity {native_arity} shape {shape:#b}"),
        );
        return None;
    };
    let (ret_kind, enum_return, native_return, result_enum) = match jit.returns {
        JitKind::EnumPtr(idx) => (JitKind::I64, Some(idx), None, None),
        JitKind::ResultEnumPtr(idx) => (JitKind::ResultEnumPtr(idx), None, None, Some(idx)),
        k @ (JitKind::NativeStr
        | JitKind::NativeVecI64
        | JitKind::NativeVecF64
        | JitKind::NativeVecStr
        | JitKind::NativeVecTupleIF) => (JitKind::I64, None, Some(k), None),
        // A `U8Vec` return would need re-registering the native buffer into
        // the VM registry; not supported, so keep such bodies on bytecode.
        JitKind::U8VecHandle => {
            trace_prepare_fail(&jit.name, "U8Vec return not marshalled");
            return None;
        }
        // `Vec<Vec<i64>>` returns re-wrap through `invoke_prepared_native`.
        JitKind::NativeVecVecI64 => (JitKind::I64, None, Some(JitKind::NativeVecVecI64), None),
        // Struct returns use the same structural-return ABI as tuples: the
        // caller supplies a flat field-slot block and the body returns that
        // stable pointer after filling it.
        k @ JitKind::StructPtr(_) => (JitKind::I64, None, Some(k), None),
        // A 2-tuple return is a pointer to a heap (`gos_rt_aggr_alloc`)
        // block; the stub reads it as `I64` and the native path decodes the
        // slots into a `Value::Tuple`.
        JitKind::TupleReturn(_) => (JitKind::I64, None, None, None),
        other => (other, None, None, None),
    };
    let tuple_return = match jit.returns {
        JitKind::TupleReturn(elems) => Some(elems),
        _ => None,
    };
    let struct_return = match jit.returns {
        JitKind::StructPtr(idx) => Some(idx),
        _ => None,
    };
    // An `EnumPtr` param or return whose shape carries `Vec`-bearing variants
    // (a `Vec<Enum>` / `Vec<(String, Enum)>` field, transitively) routes
    // through the native path: marshalling a bytecode `Value::Variant` in
    // (deep, including its `Vec` fields) and freeing the temporaries needs
    // `free_native_enum`, which the flat node-meta release of the scalar fast
    // path can't do. A scalar/string-only enum (e.g. an arithmetic-expression
    // tree) stays on the cheaper fast path - its shape has no `Vec` fields.
    let enum_param_deep = jit
        .params
        .iter()
        .any(|k| matches!(k, JitKind::EnumPtr(idx) if shape_needs_deep_free(*idx)));
    let enum_return_deep =
        matches!(jit.returns, JitKind::EnumPtr(idx) if shape_needs_deep_free(idx));
    // A two-word carrier return is decoded into its `Ok` / `Err` (or
    // `Some` / `None`) arm on the native path; the scalar fast path has
    // no decode step and would hand the caller the raw `[disc, payload]`
    // pair as a tuple.
    let carrier_return = matches!(jit.returns, JitKind::Carrier(_));
    let has_native = native_return.is_some()
        || result_enum.is_some()
        || carrier_return
        || tuple_return.is_some()
        || enum_param_deep
        || enum_return_deep
        || jit.params.iter().any(|k| {
            matches!(
                k,
                JitKind::NativeStr
                    | JitKind::NativeVecI64
                    | JitKind::NativeVecF64
                    | JitKind::NativeVecStr
                    | JitKind::NativeVecTupleIF
                    | JitKind::NativeVecVecI64
                    | JitKind::U8VecHandle
                    | JitKind::StructPtr(_)
                    | JitKind::ArrayBlockPtr(..)
                    | JitKind::Carrier(_)
            )
        });
    Some(Prepared {
        jit,
        stub,
        ret_kind,
        enum_return,
        result_enum,
        native_return,
        struct_return,
        tuple_return,
        has_native,
        verified: std::cell::Cell::new(false),
        ever_hit: std::cell::Cell::new(false),
        miss_streak: std::cell::Cell::new(0),
    })
}

/// `true` when a native enum `shape` (transitively) carries a `Vec<Enum>` or
/// `Vec<(String, Enum)>` variant field, so a value of it must be torn down
/// with [`free_native_enum`] rather than a flat node-meta release.
fn shape_needs_deep_free(idx: u32) -> bool {
    fn walk(idx: u32, seen: &mut Vec<u32>) -> bool {
        if seen.contains(&idx) {
            return false;
        }
        seen.push(idx);
        let Some(shape) = crate::value::native_shape(idx) else {
            return false;
        };
        shape.variants.iter().any(|v| {
            v.fields.iter().any(|f| match f {
                NativeFieldKind::VecEnum(_) | NativeFieldKind::VecStrEnumTuple(_) => true,
                NativeFieldKind::Enum(e) => walk(*e, seen),
                _ => false,
            })
        })
    }
    walk(idx, &mut Vec::new())
}

/// Clears the JIT fault breadcrumb when a dispatch returns, by any path.
struct BreadcrumbGuard;

impl Drop for BreadcrumbGuard {
    fn drop(&mut self) {
        gossamer_runtime::stack_guard::clear_jit_breadcrumb();
    }
}

/// Hot-path dispatch through a [`Prepared`]. Marshals args, calls the
/// cached stub under `catch_unwind`, then re-wraps an `EnumPtr` return.
pub(crate) fn invoke_prepared(p: &Prepared, args: &[Value], graph_cache: &GraphCache) -> Dispatch {
    let jit = &p.jit;
    if jit.params.len() != args.len() {
        return Dispatch::Fallback;
    }
    // Name the body for the fault handler: a hard crash inside this native
    // code (or its result marshalling) carries no Rust frame, so the guard
    // attributes the fault to `jit.name` and the guard clears it on return.
    gossamer_runtime::stack_guard::set_jit_breadcrumb(jit.name.as_ref());
    let _crumb = BreadcrumbGuard;
    // Bodies with a native aggregate param or return marshal through a
    // separate path that owns the runtime objects it builds; the scalar
    // fast path below stays allocation-free.
    if p.has_native {
        return invoke_prepared_native(p, args, graph_cache);
    }
    let mut slots: [Slot; MAX_ARGS] = [Slot::I(0); MAX_ARGS];
    // Fast marshal once shapes are proven for this resolved entry.
    // `EnumPtr` is always re-checked (the shape index guards a real
    // mismatch); other scalar kinds trust the prior verification, with
    // a debug assertion catching any divergence in debug builds.
    let fast = p.verified.get();
    // A bytecode `Value::Variant` enum arg is marshalled into a native node
    // (`build_variant_to_native_enum`) so an enum-recursive body can run
    // native instead of falling back. The JIT never frees its enum args (its
    // `Drop` is a no-op), so the trampoline frees each temporary after the
    // call - safe only when no native enum *result* can alias (and be left
    // dangling by) the freed input. That holds when the return is a scalar,
    // or when the body's return provably originates from a fresh allocation
    // (`returns_fresh`, e.g. a tree-rebuilding `simplify` / `transform`)
    // rather than a passthrough of the input. `built` frees them on every
    // exit path.
    let marshal_variant = jit.returns_fresh
        || matches!(
            jit.returns,
            JitKind::I64 | JitKind::F64 | JitKind::Bool | JitKind::Unit
        );
    let mut built = BuiltEnums(Vec::new());
    for (i, (kind, value)) in jit.params.iter().zip(args.iter()).enumerate() {
        let slot = match (kind, fast) {
            (JitKind::EnumPtr(idx), _) => match value {
                Value::NativeEnum(h) if h.shape.index == *idx => Slot::I(h.ptr as i64),
                Value::Variant(vinner) if marshal_variant => {
                    match crate::value::native_shape(*idx)
                        .and_then(|s| build_variant_to_native_enum(vinner, &s))
                    {
                        Some(ptr) => {
                            built.0.push(ptr);
                            Slot::I(ptr)
                        }
                        None => return Dispatch::Fallback,
                    }
                }
                _ => return Dispatch::Fallback,
            },
            (JitKind::Value, _) => Slot::I(value.to_raw() as i64),
            (JitKind::F64, true) => {
                debug_assert!(matches!(value, Value::Float(_)));
                match value {
                    Value::Float(x) => Slot::F(*x),
                    _ => return Dispatch::Fallback,
                }
            }
            // Native aggregate kinds (and struct params) are dispatched by
            // `invoke_prepared_native` above; the `has_native` guard makes
            // this unreachable here.
            (
                JitKind::NativeStr
                | JitKind::NativeVecI64
                | JitKind::NativeVecF64
                | JitKind::NativeVecStr
                | JitKind::NativeVecTupleIF
                | JitKind::NativeVecVecI64
                | JitKind::U8VecHandle
                | JitKind::StructPtr(_)
                | JitKind::ArrayBlockPtr(..)
                | JitKind::ResultEnumPtr(_)
                | JitKind::Carrier(_)
                | JitKind::TupleReturn(_),
                _,
            ) => return Dispatch::Fallback,
            (_, true) => match value {
                Value::Int(n) => Slot::I(*n),
                Value::Bool(b) => Slot::I(i64::from(*b)),
                Value::Char(c) => Slot::I(i64::from(u32::from(*c))),
                Value::Unit => Slot::I(0),
                _ => return Dispatch::Fallback,
            },
            // Slow (unverified) path: full per-kind check.
            (JitKind::I64, false) => match value {
                Value::Int(n) => Slot::I(*n),
                _ => return Dispatch::Fallback,
            },
            (JitKind::Char, false) => match value {
                Value::Char(c) => Slot::I(i64::from(u32::from(*c))),
                _ => return Dispatch::Fallback,
            },
            (JitKind::F64, false) => match value {
                Value::Float(x) => Slot::F(*x),
                _ => return Dispatch::Fallback,
            },
            (JitKind::Bool, false) => match value {
                Value::Bool(b) => Slot::I(i64::from(*b)),
                _ => return Dispatch::Fallback,
            },
            (JitKind::Unit, false) => match value {
                Value::Unit => Slot::I(0),
                _ => return Dispatch::Fallback,
            },
        };
        slots[i] = slot;
    }
    p.verified.set(true);
    let n = jit.params.len();
    // SAFETY: `prepare` resolved `stub` for exactly this body's
    // `(arity, shape, ret)` triple, so the reified `extern "C"`
    // signature matches the cranelift-emitted entry. `catch_unwind`
    // demotes a panic unwound through the boundary to a `Fallback`.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        (p.stub)(jit.ptr, &slots[..n], p.ret_kind)
    }));
    match outcome {
        Ok(Some(value)) => {
            if let Some(shape_idx) = p.enum_return {
                let Value::Int(raw) = value else {
                    return Dispatch::Fallback;
                };
                let Some(shape) = crate::value::native_shape(shape_idx) else {
                    return Dispatch::Fallback;
                };
                return Dispatch::Ok(Value::NativeEnum(std::sync::Arc::new(
                    crate::value::NativeEnumOwner {
                        ptr: raw as usize,
                        shape,
                        owned: true,
                    },
                )));
            }
            Dispatch::Ok(value)
        }
        Ok(None) => Dispatch::Fallback,
        Err(_) => {
            eprintln!("jit: panic inside JIT-compiled body; falling back to bytecode");
            Dispatch::Fallback
        }
    }
}

/// Dispatch for a body with a native aggregate (`String` / `Vec<i64>`)
/// param or return. Builds a fresh runtime object for each aggregate
/// param from the VM value, calls the stub, reads any aggregate return
/// back into a VM value, writes mutated `&mut` params back, then frees
/// every object it built. All native objects are trampoline-owned and
/// reclaimed through the runtime's RC reclaim entries, so the VM values
/// the caller passed are never aliased or freed by this path.
fn invoke_prepared_native(p: &Prepared, args: &[Value], graph_cache: &GraphCache) -> Dispatch {
    let jit = &p.jit;
    let mut slots: [Slot; MAX_ARGS] = [Slot::I(0); MAX_ARGS];
    let mut natives: Vec<NativeArg> = Vec::new();
    // `U8Vec` is registry-backed in the VM but a native `*mut GosU8Vec`
    // in the body, so its bytes are copied in here and copied back after
    // the call: `(native ptr, the original VM U8Vec value)`.
    let mut u8vec_writebacks: Vec<(i64, Value)> = Vec::new();
    // Backing buffers for marshalled struct params, kept alive for the
    // whole call (the JIT body reads / mutates their slots through the
    // pointer recorded in `natives`). They are freed when this Vec drops
    // at function exit, after write-back has read any `&mut self` mutation.
    let mut struct_backings: Vec<NativeStructBacking> = Vec::new();
    // Each block stays owned here for the length of the call and is
    // reclaimed with this vector once the body returns.
    let mut array_backings: Vec<Box<[i64]>> = Vec::new();
    // Native enum trees marshalled in for `EnumPtr` `Value::Variant` params:
    // `(native ptr, shape index)`. The trampoline owns each end to end and
    // frees it with `free_native_enum` after the call (the body only borrows
    // it - params are borrowed, the caller releases - so a fresh result can't
    // alias and be left dangling).
    let mut built_enums: Vec<(i64, u32)> = Vec::new();
    // `&mut String` write-through cells: a heap-boxed slot holding the native
    // string pointer, passed as `*mut *mut c_char` so the body's append /
    // realloc updates the slot, read back into the caller's binding after.
    let mut str_cells: Vec<StrCell> = Vec::new();
    let mut built_carriers = BuiltCarriers(Vec::new());
    for (i, (kind, value)) in jit.params.iter().zip(args.iter()).enumerate() {
        let slot = match kind {
            JitKind::NativeStr => match value {
                Value::MutCell(c) => {
                    // `&mut String`: the native body expects a pointer-to-slot
                    // (`*mut *mut c_char`) so its append / realloc writes
                    // through. Box the native string pointer as the slot and
                    // pass the box's stable heap address; the body's final
                    // pointer is read back into the caller's binding after.
                    let inner = c.lock().clone();
                    let Some(sptr) = build_native_str(&inner) else {
                        free_in_flight(&natives, &built_enums, &str_cells);
                        return Dispatch::Fallback;
                    };
                    let mut cell = Box::new(sptr);
                    let slot_addr = std::ptr::from_mut::<i64>(cell.as_mut()) as i64;
                    str_cells.push((cell, c.clone()));
                    Slot::I(slot_addr)
                }
                other => {
                    let Some(ptr) = build_native_str(other) else {
                        free_in_flight(&natives, &built_enums, &str_cells);
                        return Dispatch::Fallback;
                    };
                    natives.push((JitKind::NativeStr, ptr, None));
                    Slot::I(ptr)
                }
            },
            JitKind::NativeVecI64
            | JitKind::NativeVecF64
            | JitKind::NativeVecStr
            | JitKind::NativeVecTupleIF => {
                // Unwrap a `&mut` write-back cell so we marshal its inner
                // aggregate; record the cell so mutations flow back.
                let (inner, cell) = match value {
                    Value::MutCell(c) => (c.lock().clone(), Some(c.clone())),
                    other => (other.clone(), None),
                };
                let Some(ptr) = build_native_arg(*kind, &inner) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                natives.push((*kind, ptr, cell));
                Slot::I(ptr)
            }
            JitKind::NativeVecVecI64 => {
                // Read-shared `&[[i64]]` only: a `&mut` vec-of-vec param keeps
                // the body on bytecode (`body_jit_unsupported`), so the source
                // is a bare `Value::Array`, never a `MutCell`. Marshal once per
                // source `Arc` through the Arc-identity cache; the cached native
                // graph is owned by the cache (not pushed into `natives`), so
                // `free_natives` never frees it - it is reclaimed at teardown.
                let Value::Array(arc) = value else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                let key = Arc::as_ptr(arc) as usize;
                let ptr = if let Some(cached) = graph_cache.get(key) {
                    cached
                } else {
                    let Some(built) = build_native_vec_vec_i64(arc) else {
                        free_in_flight(&natives, &built_enums, &str_cells);
                        return Dispatch::Fallback;
                    };
                    if !graph_cache.should_cache(key, arc)
                        || !graph_cache.insert(key, built, arc.clone())
                    {
                        natives.push((JitKind::NativeVecVecI64, built, None));
                    }
                    built
                };
                Slot::I(ptr)
            }
            JitKind::U8VecHandle => {
                let Some(ptr) = build_native_u8vec(value) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                natives.push((*kind, ptr, None));
                u8vec_writebacks.push((ptr, value.clone()));
                Slot::I(ptr)
            }
            JitKind::EnumPtr(idx) => match value {
                // Already native (e.g. a prior native call's handle): the VM
                // value owns it; pass the pointer, the body borrows it.
                Value::NativeEnum(h) if h.shape.index == *idx => Slot::I(h.ptr as i64),
                // Bytecode enum: marshal the whole `Value::Variant` tree (its
                // scalar / string / enum / `Vec` fields) into the compiled
                // representation, recording it for `free_native_enum` teardown.
                Value::Variant(vinner) => {
                    let Some(ptr) = crate::value::native_shape(*idx)
                        .and_then(|s| build_variant_to_native_enum(vinner, &s))
                    else {
                        free_in_flight(&natives, &built_enums, &str_cells);
                        return Dispatch::Fallback;
                    };
                    built_enums.push((ptr, *idx));
                    Slot::I(ptr)
                }
                _ => {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                }
            },
            JitKind::ArrayBlockPtr(len, class) => {
                // Unwrap a `&mut` write-back cell so the block is built from
                // its inner array; recording the cell flows element writes
                // back into the caller's binding.
                let (inner, cell) = match value {
                    Value::MutCell(c) => (c.lock().clone(), Some(c.clone())),
                    other => (other.clone(), None),
                };
                let Some(mut backing) = build_native_array_block(&inner, *len, *class) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                let ptr = backing.as_mut_ptr() as i64;
                array_backings.push(backing);
                // The block is owned by `array_backings`, so `free_natives`
                // leaves it alone; the entry exists for the write-back.
                natives.push((*kind, ptr, cell));
                Slot::I(ptr)
            }
            JitKind::StructPtr(idx) => {
                let Some(shape) = native_struct_shape(*idx) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                // Unwrap a `&mut self` write-back cell so we marshal its
                // inner struct; record the cell so field mutations flow back.
                let (inner, cell) = match value {
                    Value::MutCell(c) => (c.lock().clone(), Some(c.clone())),
                    other => (other.clone(), None),
                };
                let Some(backing) = build_native_struct(&inner, shape) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                let ptr = backing.as_ptr();
                struct_backings.push(backing);
                natives.push((*kind, ptr, cell));
                Slot::I(ptr)
            }
            JitKind::Value => Slot::I(value.to_raw() as i64),
            JitKind::F64 => {
                if let Value::Float(x) = value {
                    Slot::F(*x)
                } else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                }
            }
            JitKind::I64 | JitKind::Bool | JitKind::Char | JitKind::Unit => match value {
                Value::Int(n) => Slot::I(*n),
                Value::Bool(b) => Slot::I(i64::from(*b)),
                Value::Char(c) => Slot::I(i64::from(u32::from(*c))),
                Value::Unit => Slot::I(0),
                _ => {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                }
            },
            // A carrier is lent to the body as a pointer to its two words,
            // which stay at a stable address for the whole call.
            JitKind::Carrier(shape) => {
                let metas = jit.carrier_box_metas.get(i).map_or(&[][..], |m| &m[..]);
                let Some(words) = build_native_carrier(*shape, metas, 0, value) else {
                    free_in_flight(&natives, &built_enums, &str_cells);
                    return Dispatch::Fallback;
                };
                let words = Box::new(words);
                let addr = words.as_ptr() as i64;
                built_carriers.0.push((*shape, words));
                Slot::I(addr)
            }
            // `Result<Enum, errors::Error>` and `TupleReturn` are return-only
            // kinds (rejected as params by `body_kinds`).
            JitKind::ResultEnumPtr(_) | JitKind::TupleReturn(_) => {
                free_in_flight(&natives, &built_enums, &str_cells);
                return Dispatch::Fallback;
            }
        };
        slots[i] = slot;
    }
    let n = jit.params.len();
    // Structural returns use a caller-owned hidden trailing result block. A
    // tuple's fixed two slots live on this stack frame; a struct's shape-sized
    // field block owns any string slots the body returns and releases them
    // when it drops after decoding.
    let mut sret_buf = [0i64; 2];
    let mut struct_sret = match p.struct_return {
        Some(idx) => {
            let Some(shape) = native_struct_shape(idx) else {
                free_in_flight(&natives, &built_enums, &str_cells);
                return Dispatch::Fallback;
            };
            Some(NativeStructBacking {
                // Empty structs still cross through a one-word ABI slot.
                slots: vec![0; shape.words.max(1)].into_boxed_slice(),
                shape,
            })
        }
        None => None,
    };
    let n_call = if p.tuple_return.is_some() {
        slots[n] = Slot::I(sret_buf.as_mut_ptr() as i64);
        n + 1
    } else if let Some(backing) = &mut struct_sret {
        slots[n] = Slot::I(backing.as_ptr());
        n + 1
    } else {
        n
    };
    // SAFETY: `prepare` resolved `stub` for this body's `(arity, shape,
    // ret)` triple; native aggregate slots cross as pointer-sized i64
    // values matching the flat-ABI signature. `catch_unwind` demotes a
    // boundary panic to a `Fallback`.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        (p.stub)(jit.ptr, &slots[..n_call], p.ret_kind)
    }));
    let raw = match outcome {
        Ok(Some(v)) => v,
        Ok(None) => {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        }
        Err(_) => {
            eprintln!("jit: panic inside JIT-compiled body; falling back to bytecode");
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        }
    };
    // Copy each marshalled `U8Vec`'s (mutated) bytes back to its registry
    // buffer so the caller observes the body's in-place writes. Done while
    // the native buffers are still live, before `free_natives` reclaims them.
    for (ptr, val) in &u8vec_writebacks {
        let bytes = read_native_u8vec(*ptr);
        crate::builtins::u8vec_write_back(val, &bytes);
    }
    // Decode the return into a VM value (and the native aggregate to dedup
    // against the params, if any). A malformed return bails after freeing the
    // native temporaries; the `&mut String` cells are still un-written here,
    // so the caller's binding is left untouched for a clean bytecode re-run.
    let (result, native_ret): (Value, Option<(JitKind, i64)>) = if let Some(shape_idx) =
        p.result_enum
    {
        // `Result<Enum, _>`: the stub handed back the `[disc, payload]` carrier
        // tuple. Decode the `Ok` enum (deep-copy out, then free the native DOM)
        // or the `Err` error, wrap in the VM's `Ok` / `Err` variant.
        let Value::Tuple(t) = &raw else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let (Some(Value::Int(disc)), Some(Value::Int(payload))) = (t.first(), t.get(1)) else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let (disc, payload) = (*disc, *payload);
        // A well-formed `Result<Enum, _>` carrier always decodes to a disc
        // of 0 (`Ok`) or 1 (`Err`) and a payload that is either null or an
        // 8-aligned heap pointer. Anything else means the two-word carrier
        // was recovered wrong at the JIT boundary (the Windows x64 i128
        // path). Report the raw words so the corruption is visible before
        // the misformed pointer is read; debug builds only.
        if cfg!(debug_assertions)
            && (!(0..=1).contains(&disc) || (payload != 0 && payload & 0x7 != 0))
        {
            use std::io::Write as _;
            let mut err = std::io::stderr().lock();
            let _ = writeln!(
                err,
                "gossamer[jit-carrier]: body '{}' returned a corrupt Result carrier: disc={disc} payload={:#018x}",
                p.jit.name, payload as u64
            );
            let _ = err.flush();
        }
        let v = if disc == 0 {
            let Some(shape) = crate::value::native_shape(shape_idx) else {
                free_in_flight(&natives, &built_enums, &str_cells);
                return Dispatch::Fallback;
            };
            // Keep the `Ok` payload as a native handle (no copy to a VM
            // `Variant` tree): the next JIT body in a parse->transform->...
            // pipeline takes it back as a native pointer with zero marshalling,
            // and `NativeEnumOwner::Drop` reclaims its `Vec` children. The VM
            // inspects it directly (`VariantIs` / `VariantField` handle the
            // native shape). This is what removes the marshalling DOM doubling.
            let v = Value::NativeEnum(Arc::new(crate::value::NativeEnumOwner {
                ptr: payload as usize,
                shape,
                owned: true,
            }));
            Value::variant("Ok", vec![v])
        } else {
            Value::variant("Err", vec![read_native_error(payload)])
        };
        (v, None)
    } else if let JitKind::Carrier(shape) = p.jit.returns {
        // The caller owns the answered words: read them into a VM value, then
        // give back the shares they hold.
        let Value::Tuple(t) = &raw else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let (Some(Value::Int(disc)), Some(Value::Int(payload))) = (t.first(), t.get(1)) else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let v = carrier_to_value(shape, 0, *disc, *payload);
        release_carrier_words(shape, 0, *disc, *payload);
        (v, None)
    } else if let Some(nret) = p.native_return {
        let Value::Int(ret_ptr) = raw else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let value = native_ptr_to_value(nret, ret_ptr);
        // A `Vec<Vec<i64>>` body that returns one of its `&[[i64]]` params
        // hands back a cache-owned pointer; freeing it would leave the cache
        // holding a dangling graph. Skip the free in that case - the cache
        // reclaims it at teardown. A freshly-built return (the `build_graph`
        // shape) is not in the cache and is freed normally.
        let free_ret = if matches!(nret, JitKind::NativeVecVecI64) && graph_cache.owns_ptr(ret_ptr)
        {
            None
        } else {
            Some((nret, ret_ptr))
        };
        (value, free_ret)
    } else if let Some(shape_idx) = p.enum_return {
        let Value::Int(ret_ptr) = raw else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        let Some(shape) = crate::value::native_shape(shape_idx) else {
            free_in_flight(&natives, &built_enums, &str_cells);
            return Dispatch::Fallback;
        };
        // Keep every enum return as a native handle - including a `Vec`-bearing
        // one, now that `NativeEnumOwner::Drop` reclaims its `Vec` children. The
        // value flows on to the next JIT body with zero marshalling, and the VM
        // inspects it through the native-aware `VariantIs` / `VariantField` ops.
        let v = Value::NativeEnum(Arc::new(crate::value::NativeEnumOwner {
            ptr: ret_ptr as usize,
            shape,
            owned: true,
        }));
        (v, None)
    } else if let Some(elems) = p.tuple_return {
        // A 2-tuple return via the sret ABI: the body wrote the two result
        // words into our stack buffer (`sret_buf`). Decode each slot; ownership
        // of an enum element transferred into the value. Nothing to free - the
        // buffer is this frame's stack, reclaimed on return.
        let block = sret_buf.as_ptr() as i64;
        // SAFETY: `prepare` set `tuple_return` only for a `TupleReturn` body,
        // which the sret-aware call above filled with two words matching `elems`.
        let v = unsafe { decode_tuple_return(block, &elems) };
        (v, None)
    } else {
        (raw, None)
    };
    // `&mut String` write-through: read each cell's final native string back
    // into the caller's binding, then free it.
    for (cell, mutcell) in &str_cells {
        let final_ptr = **cell;
        *mutcell.lock() = native_ptr_to_value(JitKind::NativeStr, final_ptr);
        if final_ptr != 0 {
            // SAFETY: a live native string (the body's final append result),
            // copied out above and freed exactly once here.
            unsafe { free_native(JitKind::NativeStr, final_ptr) };
        }
    }
    // Free the marshalled-in enum params (the body borrowed them), write back
    // `&mut` aggregate params, then free every native object once (a native
    // aggregate return is deduped against the params).
    free_built_enums(&built_enums);
    writeback_natives(&natives);
    free_natives(&natives, native_ret);
    Dispatch::Ok(result)
}

use std::sync::atomic::{AtomicBool, Ordering};

/// CLI override for the JIT default. `Vm::load` consults this
/// flag so `gos run --no-jit` can disable the JIT without mutating
/// the process environment. The JIT is on by default per Tier D
/// of the interp wow plan; this flag (or `GOS_JIT=0`) is the only
/// way to turn it back off.
static JIT_DISABLED: AtomicBool = AtomicBool::new(false);

/// CLI hook used by `gos run --no-jit` to suppress every JIT
/// compile attempt regardless of `GOS_JIT`. Pair with
/// [`force_jit_enable`] to scope the disable to a defined region in
/// long-lived processes (REPL, test runners, etc.) - see the
/// `set_stdout_writer` companion in `builtins.rs` for the canonical
/// scoped-disable shape.
pub fn force_jit_disabled() {
    JIT_DISABLED.store(true, Ordering::Relaxed);
}

/// Reverses a prior [`force_jit_disabled`] call. Long-lived processes
/// (REPLs, test harnesses that swap stdout writers between cases)
/// previously lost the JIT permanently once any caller flipped the
/// flag; this companion lets them restore it.
///
/// `gos run --no-jit` does not call this - the flag stays set for the
/// process lifetime in CLI mode. Test code that installs a custom
/// stdout writer should bracket the override with
/// `force_jit_disabled` / `force_jit_enable` if it wants to recover
/// the JIT path after the test exits.
pub fn force_jit_enable() {
    JIT_DISABLED.store(false, Ordering::Relaxed);
}

/// Returns `true` when [`force_jit_disabled`] has been called and not
/// yet reversed by [`force_jit_enable`]. Lets callers inspect the
/// flag without flipping it.
#[must_use]
pub fn jit_force_disabled_state() -> bool {
    JIT_DISABLED.load(Ordering::Relaxed)
}

/// Returns `true` when JIT compilation is permitted in this
/// process. Default is `true` (Tier D promoted JIT to the steady-
/// state execution path); the only ways to suppress it are
/// `gos run --no-jit` (which calls [`force_jit_disabled`]) or
/// setting `GOS_JIT=0` / `GOS_JIT=false` in the environment.
/// This is intentionally not memoised so tests can flip the env
/// between runs.
pub(crate) fn jit_enabled() -> bool {
    if JIT_DISABLED.load(Ordering::Relaxed) {
        return false;
    }
    !matches!(
        std::env::var("GOS_JIT").ok().as_deref(),
        Some("0" | "false")
    )
}

/// Returns `true` when `GOS_JIT_TRACE` is set, in which case the VM
/// emits per-function compile / dispatch diagnostics on stderr.
pub(crate) fn jit_trace() -> bool {
    matches!(std::env::var("GOS_JIT_TRACE").ok().as_deref(), Some(s) if !s.is_empty() && s != "0")
}
