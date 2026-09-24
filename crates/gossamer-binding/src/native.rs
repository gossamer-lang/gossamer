//! Compiled-mode export ABI for binding items.
//!
//! Bridges Rust types in user binding signatures to C-ABI shapes
//! the gossamer codegen emits calls against. Each supported
//! `Type` variant lowers to a stable C-ABI input/output type via
//! [`BindingAbi`]; the `register_module!` macro uses these
//! associated types to synthesize an `extern "C"` thunk per
//! binding fn.
//!
//! See `~/dev/contexts/lang/ffi_compiled.md` Stage 1.

// FFI bridge: pointer reinterprets are deliberate. The runtime
// lays out `GosVec<T>` payloads as a tightly-packed buffer of
// `T`-sized cells; `cast::<T>()` lets us read through that buffer
// without copying. Alignment is enforced upstream by the GC's
// 8-byte allocator, so the cast-ptr-alignment lint is wrong here.
#![allow(clippy::cast_ptr_alignment)]
// `unsafe extern "C"` thunks: every call comes from generated code
// over a contract documented at the call site (see ffi_compiled.md).
#![allow(
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref
)]

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;

use crate::conv::{Bytes, DynValue};
use crate::types::Type;

// Bring `gos_rt_gc_alloc` into scope. Defined in
// `gossamer-runtime`'s `c_abi.rs`; the binding crate links it
// transitively via `gossamer-interp`. Allocations from this
// arena are what the runtime expects to read past for compound
// types - matching domains is what makes Vec/String/Option/etc.
// flow correctly through the compiled-mode boundary.
unsafe extern "C" {
    fn gos_rt_gc_alloc(size: u64) -> *mut u8;
    fn gos_rt_vec_with_capacity(elem_bytes: u32, cap: i64) -> *mut GosVec;
}

/// Arena-backed allocator used by every compound `to_output`
/// path. Returns a raw pointer (not `Box`) so the runtime's
/// `gos_rt_*` readers can dereference it safely; the runtime
/// owns reclamation via `gos_rt_gc_reset`.
///
/// Rounds the request up to a multiple of 8 bytes. The
/// underlying bump allocator is byte-aligned, so without this
/// rounding a header struct following a non-multiple-of-8
/// allocation (e.g. a 13-byte cstring) would land at a
/// misaligned offset and trip `ptr::copy_nonoverlapping`'s
/// alignment precondition. 8 bytes covers every shape this
/// crate writes (`GosVec`, `GosVariant`, `GosTuple`,
/// `GosVariantValue`).
fn arena_alloc(bytes: usize) -> *mut u8 {
    if bytes == 0 {
        return std::ptr::null_mut();
    }
    let aligned = bytes.div_ceil(8) * 8;
    // SAFETY: `gos_rt_gc_alloc` is part of `gossamer-runtime`'s
    // C-ABI surface; the binding's staticlib links it. Returns a
    // pointer into a thread-local arena valid until the next
    // `gos_rt_gc_reset` (the runtime's tick boundary).
    unsafe { gos_rt_gc_alloc(aligned as u64) }
}

/// Allocates one `T`-shaped slot in the arena, writes `value`
/// into it, and returns the pointer. Used to manufacture
/// header structs (`GosVec`, `GosVariant`, `GosTuple`,
/// `GosVariantValue`) without going through Box.
fn arena_box<T>(value: T) -> *mut T {
    let p = arena_alloc(std::mem::size_of::<T>()).cast::<T>();
    if !p.is_null() {
        // SAFETY: `p` is a fresh arena allocation aligned for
        // `T` (the runtime's bump arena returns word-aligned
        // pointers, which suffices for every shape we
        // manufacture here - `GosVec`, `GosVariant`, etc. all
        // have alignment ≤ 8).
        unsafe {
            std::ptr::write(p, value);
        }
    }
    p
}

/// Aggregate matching the runtime's `gos_rt_*` vec ABI.
///
/// Storage layout is deliberately fixed so codegen on either tier
/// can manufacture and consume these without reaching into
/// `gossamer-runtime` internals. `len` / `cap` are element counts
/// (not byte counts); `elem_bytes` records the homogeneous element
/// size; `ptr` points at `len * elem_bytes` bytes.
#[repr(C)]
#[derive(Debug)]
pub struct GosVec {
    /// Number of elements.
    pub len: i64,
    /// Allocated element capacity.
    pub cap: i64,
    /// Element width in bytes.
    pub elem_bytes: u32,
    /// Element data buffer.
    pub ptr: *mut u8,
}

/// Aggregate matching the runtime's `gos_rt_variant` ABI for
/// `Option`, `Result`, and other tagged sums.
#[repr(C)]
#[derive(Debug)]
pub struct GosVariant {
    /// Variant tag - the macro encodes:
    /// - `0` for `None` / `Err`
    /// - `1` for `Some` / `Ok`
    ///
    /// Bindings authoring custom enums set their own values.
    pub tag: i32,
    /// Number of payload values.
    pub payload_len: i32,
    /// Payload pointer; layout is `payload_len`
    /// `GosVariantValue`s. Null when there is no payload.
    pub payload: *mut GosVariantValue,
}

/// Tagged-union element used inside a [`GosVariant`] payload or
/// a [`GosTuple`] field array. The tag picks which member of the
/// `data` field is live.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GosVariantValue {
    /// Tag (`0` = i64, `1` = f64, `2` = bool, `3` = char,
    /// `4` = string, `5` = vec, `6` = variant, `7` = tuple,
    /// `8` = opaque).
    pub tag: i32,
    /// Payload data - readers consult `tag` to pick the live
    /// member.
    pub data: GosVariantPayload,
}

/// Untagged-union payload sized to the largest variant. Reading
/// fields that don't match the [`GosVariantValue::tag`] is
/// undefined behaviour at the C-ABI level.
#[repr(C)]
#[derive(Clone, Copy)]
pub union GosVariantPayload {
    /// `i64` payload.
    pub i64_: i64,
    /// `f64` payload.
    pub f64_: f64,
    /// `bool` payload.
    pub bool_: bool,
    /// `char` payload (`u32` Unicode code point).
    pub char_: u32,
    /// String payload (NUL-terminated, arena-allocated).
    pub string: *mut c_char,
    /// Nested vec payload.
    pub vec: *mut GosVec,
    /// Nested variant payload.
    pub variant: *mut GosVariant,
    /// Nested tuple payload.
    pub tuple: *mut GosTuple,
}

impl std::fmt::Debug for GosVariantPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<gos variant payload>")
    }
}

/// Aggregate matching the runtime's tuple ABI. Stores `len`
/// fields each as a [`GosVariantValue`]; the field count and
/// per-field types are fixed by the binding signature.
#[repr(C)]
#[derive(Debug)]
pub struct GosTuple {
    /// Field count.
    pub len: i32,
    /// Field array; layout matches `payload` of [`GosVariant`].
    pub fields: *mut GosVariantValue,
}

/// Aggregate matching the runtime's `Bytes` ABI.
///
/// ABI 0.4. Header lives on the heap (boxed via `Box::into_raw`)
/// so the runtime's `gos_rt_bytes_free` reclaims it the same way
/// `GosVec` is reclaimed. Data buffer is a heap `Vec<u8>` whose
/// pointer is leaked into `ptr` (cap = len in v1).
#[repr(C)]
#[derive(Debug)]
pub struct GosBytes {
    /// Byte length.
    pub len: i64,
    /// Allocated capacity.
    pub cap: i64,
    /// Byte buffer.
    pub ptr: *mut u8,
}

/// Aggregate matching the binding-side `Map<K, V>` ABI.
///
/// ABI 0.4. Two parallel [`GosVec`]s: `keys[i]` pairs with
/// `values[i]`. Order is not significant for set semantics; for
/// `HashMap::from_gos`, the keys vec is walked in declaration
/// order and the first entry for a duplicate key wins.
///
/// This struct is DELIBERATELY NOT the same layout as the
/// runtime's `GosMap` (which uses a `parking_lot::Mutex<...>` over
/// an internal storage enum, not parallel `GosVec`s). The binding-
/// side layout is a wire shape: it crosses the C-ABI between
/// generated thunks and the Gossamer runtime only at well-defined
/// transfer points. Pointers of this type MUST NOT be handed to
/// `gos_rt_map_free` - that helper `Box::from_raw`s the runtime
/// layout and would drop a `parking_lot::Mutex` over garbage. Use
/// `gossamer_runtime::c_abi::gos_rt_binding_map_free` or let the
/// GC arena reclaim the allocation.
#[repr(C)]
#[derive(Debug)]
pub struct BindingGosMap {
    /// Keys vec header.
    pub keys: *mut GosVec,
    /// Values vec header.
    pub values: *mut GosVec,
}

/// Compatibility alias for the pre-0.6 spelling of
/// [`BindingGosMap`]. Existing downstream binding crates referenced
/// the binding-side aggregate as `GosMap`; the alias keeps that
/// surface working for one release. Remove on the next ABI bump
/// once consumers have migrated.
#[deprecated(
    since = "0.6.0",
    note = "rename to BindingGosMap; the binding-side struct is not layout-compatible with the runtime's GosMap"
)]
pub use BindingGosMap as GosMap;

/// Aggregate matching the runtime's `Variant` ABI.
///
/// ABI 0.4. Carries an arm name (NUL-terminated, arena-allocated)
/// plus a payload list. The arm name is the only piece of
/// metadata runtime code needs to dispatch on; payload typing is
/// the binding signature's responsibility.
#[repr(C)]
#[derive(Debug)]
pub struct GosDynVariant {
    /// Arm name as a NUL-terminated C string (arena-allocated).
    pub name: *const c_char,
    /// Payload count.
    pub payload_len: i32,
    /// Padding to align `payload` to an 8-byte boundary.
    pub pad: i32,
    /// Payload pointer. Layout is `payload_len` `GosVariantValue`s.
    pub payload: *mut GosVariantValue,
}

/// Aggregate matching the runtime's `Callback` ABI.
///
/// ABI 0.4. Carries a Gossamer-side callable handle. The handle
/// is registered into a per-call dispatch table by the codegen at
/// the binding call site; the binding invokes through
/// `gos_rt_callback_invoke(handle, ...)`. The handle is INVALID
/// after the binding fn returns - bindings MUST NOT retain it
/// past the call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GosCallback {
    /// Opaque dispatch-table handle. Zero is the null sentinel.
    pub handle: u64,
}

// --- BindingAbi -----------------------------------------------------

/// Maps a Rust binding-signature type to its compiled-mode
/// C-ABI input / output shape.
///
/// The macro reads `Input` for parameter types and `Output` for
/// the return type; the codegen emits the call with the same
/// shapes determined from the binding's declared `Signature`.
///
/// `Output: Copy + Default` is required because:
/// - `Default` produces the panic-fallback value the
///   `register_module!` thunk returns when the binding body
///   unwinds inside `std::panic::catch_unwind`.
/// - `Copy` lets generic container impls (`Vec<T>`, `Option<T>`,
///   etc.) write `T::Output` cells into a buffer for the wire
///   shape without owning-move bookkeeping.
pub trait BindingAbi: Sized {
    /// C-ABI shape used in argument position.
    type Input: Copy;
    /// C-ABI shape used in return position.
    type Output: Copy + Default;

    /// Picks the [`Type`] variant the codegen sees in the
    /// binding's advertised signature; used by the codegen to
    /// pick the matching pack/unpack lowering.
    const TYPE: Type;

    /// Materialises the Rust value from its `Input` shape. Called
    /// at the start of the macro-generated thunk.
    ///
    /// # Safety
    /// Pointers received via `Input` must be valid for the
    /// duration of the call. The codegen guarantees this; binding
    /// authors should not call this manually.
    unsafe fn from_input(input: Self::Input) -> Self;

    /// Boxes the Rust value into its `Output` shape. Called at
    /// the end of the macro-generated thunk to hand the value
    /// back to the calling Gossamer code.
    fn to_output(self) -> Self::Output;
}

// --- Primitive impls ------------------------------------------------

impl BindingAbi for i64 {
    type Input = i64;
    type Output = i64;
    const TYPE: Type = Type::I64;

    unsafe fn from_input(input: i64) -> Self {
        input
    }
    fn to_output(self) -> i64 {
        self
    }
}

impl BindingAbi for u64 {
    type Input = u64;
    type Output = u64;
    // u64 maps to the same Gossamer-source type as i64 - both
    // are 64-bit integers; the difference is unsigned display
    // semantics, not wire shape.
    const TYPE: Type = Type::I64;

    unsafe fn from_input(input: u64) -> Self {
        input
    }
    fn to_output(self) -> u64 {
        self
    }
}

impl BindingAbi for f64 {
    type Input = f64;
    type Output = f64;
    const TYPE: Type = Type::F64;

    unsafe fn from_input(input: f64) -> Self {
        input
    }
    fn to_output(self) -> f64 {
        self
    }
}

impl BindingAbi for bool {
    type Input = bool;
    type Output = bool;
    const TYPE: Type = Type::Bool;

    unsafe fn from_input(input: bool) -> Self {
        input
    }
    fn to_output(self) -> bool {
        self
    }
}

impl BindingAbi for char {
    type Input = u32;
    type Output = u32;
    const TYPE: Type = Type::Char;

    unsafe fn from_input(input: u32) -> Self {
        char::from_u32(input).unwrap_or('\0')
    }
    fn to_output(self) -> u32 {
        self as u32
    }
}

impl BindingAbi for () {
    type Input = ();
    type Output = ();
    const TYPE: Type = Type::Unit;

    unsafe fn from_input(_input: ()) -> Self {}
    fn to_output(self) {}
}

// --- String ---------------------------------------------------------

impl BindingAbi for String {
    type Input = *const c_char;
    type Output = *mut c_char;
    const TYPE: Type = Type::String;

    unsafe fn from_input(input: *const c_char) -> Self {
        if input.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(input) }
            .to_string_lossy()
            .into_owned()
    }

    fn to_output(self) -> *mut c_char {
        // Strip interior NULs so the C-string view stops at our
        // explicit terminator. Allocate `len + 1` arena bytes,
        // copy payload, write trailing NUL.
        let bytes: Vec<u8> = self.into_bytes().into_iter().filter(|b| *b != 0).collect();
        let total = bytes.len() + 1;
        let p = arena_alloc(total);
        if p.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: arena allocation is `total` bytes; we write
        // exactly `total` bytes (`bytes.len()` payload + NUL).
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            *p.add(bytes.len()) = 0;
        }
        p.cast::<c_char>()
    }
}

// --- Vec ------------------------------------------------------------

/// Builds a runtime-owned `GosVec` from `elements`.
///
/// Binding outputs must go through the runtime allocator: the header carries
/// a versioned owner with its generation and destructor identity, so a later
/// compiled-tier drop never needs to infer provenance from the header address.
fn make_gos_vec<T: Copy>(elements: &[T]) -> *mut GosVec {
    let elem_bytes = u32::try_from(std::mem::size_of::<T>()).unwrap_or(0);
    let len = i64::try_from(elements.len()).unwrap_or(0);
    // SAFETY: runtime allocator returns a header valid for this ABI prefix;
    // copying `T: Copy` bytes into its reserved buffer preserves the element.
    let out = unsafe { gos_rt_vec_with_capacity(elem_bytes, len) };
    if out.is_null() || elements.is_empty() {
        return out;
    }
    let bytes = std::mem::size_of_val(elements);
    unsafe {
        std::ptr::copy_nonoverlapping(elements.as_ptr().cast::<u8>(), (*out).ptr, bytes);
        (*out).len = len;
    }
    out
}

/// Length and element stride of a vec header. The runtime emits
/// packed buffers for narrow element types (`elem_bytes` 1 / 2 / 4 -
/// e.g. `resp.raw_bytes` arrives at stride 1); a zero `elem_bytes`
/// from a legacy producer means word-width.
fn vec_len_stride(header: &GosVec) -> (usize, usize) {
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    let stride = match header.elem_bytes {
        0 => 8,
        n => n as usize,
    };
    (len, stride)
}

/// Reads element `idx` as a zero-extended word, honoring the
/// header's element stride. Mirrors the runtime's `vec_elem_load_i64`
/// so packed vecs cross the binding ABI byte-exact instead of being
/// read at a fixed 8-byte stride.
unsafe fn vec_elem_word(header: &GosVec, idx: usize, stride: usize) -> i64 {
    let p = unsafe { header.ptr.add(idx * stride) };
    match stride {
        1 => i64::from(unsafe { p.read() }),
        2 => i64::from(unsafe { p.cast::<u16>().read_unaligned() }),
        4 => i64::from(unsafe { p.cast::<u32>().read_unaligned() }),
        _ => unsafe { p.cast::<i64>().read_unaligned() },
    }
}

unsafe fn read_gos_vec_i64(p: *const GosVec) -> Vec<i64> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    (0..len)
        .map(|i| unsafe { vec_elem_word(header, i, stride) })
        .collect()
}

unsafe fn read_gos_vec_f64(p: *const GosVec) -> Vec<f64> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    (0..len)
        .map(|i| {
            let p = unsafe { header.ptr.add(i * stride) };
            match stride {
                4 => f64::from(unsafe { p.cast::<f32>().read_unaligned() }),
                _ => unsafe { p.cast::<f64>().read_unaligned() },
            }
        })
        .collect()
}

unsafe fn read_gos_vec_strings(p: *const GosVec) -> Vec<String> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // String slots are pointers - no real producer packs them below
    // word width. A sub-word stride is a corrupt header; reading the
    // low bytes as a pointer would be UB, so bail to an empty vec.
    if stride < 8 {
        return Vec::new();
    }
    (0..len)
        .map(|i| {
            let ptr = unsafe { vec_elem_word(header, i, stride) } as usize as *const c_char;
            unsafe { String::from_input(ptr) }
        })
        .collect()
}

unsafe fn read_gos_vec_bools(p: *const GosVec) -> Vec<bool> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    (0..len)
        .map(|i| unsafe { vec_elem_word(header, i, stride) } != 0)
        .collect()
}

unsafe fn read_gos_vec_vec_i64(p: *const GosVec) -> Vec<Vec<i64>> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // Inner-vec slots are pointers - same corrupt-header guard as
    // `read_gos_vec_strings`.
    if stride < 8 {
        return Vec::new();
    }
    (0..len)
        .map(|i| {
            let inner = unsafe { vec_elem_word(header, i, stride) } as usize as *const GosVec;
            unsafe { read_gos_vec_i64(inner) }
        })
        .collect()
}

impl BindingAbi for Vec<i64> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::I64);

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_i64(input) }
    }

    fn to_output(self) -> *mut GosVec {
        make_gos_vec(&self)
    }
}

impl BindingAbi for Vec<f64> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::F64);

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_f64(input) }
    }

    fn to_output(self) -> *mut GosVec {
        make_gos_vec(&self)
    }
}

impl BindingAbi for Vec<bool> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::Bool);

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_bools(input) }
    }

    fn to_output(self) -> *mut GosVec {
        let bytes: Vec<u8> = self.into_iter().map(u8::from).collect();
        make_gos_vec(&bytes)
    }
}

impl BindingAbi for Vec<String> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::String);

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_strings(input) }
    }

    fn to_output(self) -> *mut GosVec {
        let ptrs: Vec<*mut c_char> = self.into_iter().map(BindingAbi::to_output).collect();
        make_gos_vec(&ptrs)
    }
}

impl BindingAbi for Vec<Vec<i64>> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::Vec(&Type::I64));

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_vec_i64(input) }
    }

    fn to_output(self) -> *mut GosVec {
        let ptrs: Vec<*mut GosVec> = self.into_iter().map(BindingAbi::to_output).collect();
        make_gos_vec(&ptrs)
    }
}

// --- Option<i64>, Result<i64, String> -----------------------------

unsafe fn variant_value_i64(v: i64) -> GosVariantValue {
    GosVariantValue {
        tag: 0,
        data: GosVariantPayload { i64_: v },
    }
}

unsafe fn variant_value_string(s: String) -> GosVariantValue {
    GosVariantValue {
        tag: 4,
        data: GosVariantPayload {
            string: s.to_output(),
        },
    }
}

fn make_variant(tag: i32, payload: Vec<GosVariantValue>) -> *mut GosVariant {
    let payload_len = i32::try_from(payload.len()).unwrap_or(0);
    let payload_ptr: *mut GosVariantValue = if payload.is_empty() {
        std::ptr::null_mut()
    } else {
        let bytes = std::mem::size_of_val(payload.as_slice());
        let buf = arena_alloc(bytes).cast::<GosVariantValue>();
        if !buf.is_null() {
            // SAFETY: arena buffer is `bytes` long, fresh, and
            // exclusively ours; payload slice is non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len());
            }
        }
        buf
    };
    arena_box(GosVariant {
        tag,
        payload_len,
        payload: payload_ptr,
    })
}

unsafe fn read_option_i64(p: *const GosVariant) -> Option<i64> {
    if p.is_null() {
        return None;
    }
    let v = unsafe { &*p };
    if v.tag == 0 || v.payload_len == 0 || v.payload.is_null() {
        return None;
    }
    let payload = unsafe { &*v.payload };
    if payload.tag != 0 {
        return None;
    }
    Some(unsafe { payload.data.i64_ })
}

unsafe fn read_result_i64_string(p: *const GosVariant) -> Result<i64, String> {
    if p.is_null() {
        return Err(String::new());
    }
    let v = unsafe { &*p };
    if v.payload_len == 0 || v.payload.is_null() {
        return Err(String::new());
    }
    let payload = unsafe { &*v.payload };
    if v.tag == 1 {
        Ok(unsafe { payload.data.i64_ })
    } else {
        Err(unsafe { String::from_input(payload.data.string) })
    }
}

impl BindingAbi for Option<i64> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::I64);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        unsafe { read_option_i64(input) }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(v) => make_variant(1, vec![unsafe { variant_value_i64(v) }]),
        }
    }
}

impl BindingAbi for Result<i64, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::I64, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        unsafe { read_result_i64_string(input) }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => make_variant(1, vec![unsafe { variant_value_i64(v) }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string(e) }]),
        }
    }
}

// --- ABI 0.4: Bytes (Vec<u8> via the `Bytes` newtype) ----------------

/// Builds a `GosBytes` header on the heap; the data buffer is
/// `Vec::leak`'d so the runtime's `gos_rt_bytes_free` can reclaim
/// it with `Vec::from_raw_parts`. Arena allocation is not used
/// here because reclamation happens at GC-reset boundaries that
/// the binding may outlive (e.g. an HTTP body returned to a
/// long-running connection handler).
fn make_gos_bytes(bytes: Vec<u8>) -> *mut GosBytes {
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let cap = i64::try_from(bytes.capacity()).unwrap_or(len);
    let mut boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    Box::into_raw(Box::new(GosBytes { len, cap, ptr }))
}

unsafe fn read_gos_bytes(p: *const GosBytes) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(header.ptr, len) };
    slice.to_vec()
}

impl BindingAbi for Bytes {
    type Input = *const GosBytes;
    type Output = *mut GosBytes;
    const TYPE: Type = Type::Bytes;

    unsafe fn from_input(input: *const GosBytes) -> Self {
        Bytes::new(unsafe { read_gos_bytes(input) })
    }

    fn to_output(self) -> *mut GosBytes {
        make_gos_bytes(self.into_inner())
    }
}

// --- ABI 0.4: Map<K, V> (HashMap<K, V>) ------------------------------

/// Builds a `BindingGosMap` from parallel-vec halves. Each half is
/// a heap-owned `GosVec` produced through [`make_gos_vec`]; the
/// outer `BindingGosMap` is heap-allocated via `Box::into_raw`.
/// Reclamation is the consumer's responsibility - bindings MUST
/// NOT call `gos_rt_map_free` on this pointer (that helper
/// targets the runtime's incompatible `GosMap` layout); use
/// `gos_rt_binding_map_free` instead.
fn make_gos_map<K: Copy, V: Copy>(keys: &[K], values: &[V]) -> *mut BindingGosMap {
    let keys_ptr = make_gos_vec(keys);
    let values_ptr = make_gos_vec(values);
    Box::into_raw(Box::new(BindingGosMap {
        keys: keys_ptr,
        values: values_ptr,
    }))
}

unsafe fn read_gos_map_keys_values_i64(p: *const BindingGosMap) -> (Vec<i64>, Vec<i64>) {
    if p.is_null() {
        return (Vec::new(), Vec::new());
    }
    let m = unsafe { &*p };
    let keys = unsafe { read_gos_vec_i64(m.keys) };
    let values = unsafe { read_gos_vec_i64(m.values) };
    (keys, values)
}

unsafe fn read_gos_map_keys_values_str_str(p: *const BindingGosMap) -> (Vec<String>, Vec<String>) {
    if p.is_null() {
        return (Vec::new(), Vec::new());
    }
    let m = unsafe { &*p };
    let keys = unsafe { read_gos_vec_strings(m.keys) };
    let values = unsafe { read_gos_vec_strings(m.values) };
    (keys, values)
}

unsafe fn read_gos_map_keys_values_str_i64(p: *const BindingGosMap) -> (Vec<String>, Vec<i64>) {
    if p.is_null() {
        return (Vec::new(), Vec::new());
    }
    let m = unsafe { &*p };
    let keys = unsafe { read_gos_vec_strings(m.keys) };
    let values = unsafe { read_gos_vec_i64(m.values) };
    (keys, values)
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<i64, i64> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::I64, &Type::I64);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        let (keys, values) = unsafe { read_gos_map_keys_values_i64(input) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut keys: Vec<i64> = Vec::with_capacity(self.len());
        let mut values: Vec<i64> = Vec::with_capacity(self.len());
        for (k, v) in self {
            keys.push(k);
            values.push(v);
        }
        make_gos_map(&keys, &values)
    }
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<String, String> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::String, &Type::String);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        let (keys, values) = unsafe { read_gos_map_keys_values_str_str(input) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut key_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        let mut val_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        for (k, v) in self {
            key_ptrs.push(k.to_output());
            val_ptrs.push(v.to_output());
        }
        make_gos_map(&key_ptrs, &val_ptrs)
    }
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<String, i64> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::String, &Type::I64);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        let (keys, values) = unsafe { read_gos_map_keys_values_str_i64(input) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut key_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        let mut values: Vec<i64> = Vec::with_capacity(self.len());
        for (k, v) in self {
            key_ptrs.push(k.to_output());
            values.push(v);
        }
        make_gos_map(&key_ptrs, &values)
    }
}

// --- ABI 0.4: Variant (DynValue) -------------------------------------

fn arena_cstr(s: &str) -> *const c_char {
    let bytes = s.as_bytes();
    // Strip interior NULs for safe C-string round-trip.
    let clean: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect();
    let total = clean.len() + 1;
    let p = arena_alloc(total);
    if p.is_null() {
        return std::ptr::null();
    }
    // SAFETY: arena allocation is `total` bytes; we write exactly
    // `clean.len()` payload bytes plus one NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(clean.as_ptr(), p, clean.len());
        *p.add(clean.len()) = 0;
    }
    p.cast::<c_char>()
}

fn dyn_to_variant_value(d: &DynValue) -> GosVariantValue {
    match d {
        DynValue::Nil => GosVariantValue {
            tag: 0,
            data: GosVariantPayload { i64_: 0 },
        },
        DynValue::Bool(b) => GosVariantValue {
            tag: 2,
            data: GosVariantPayload { bool_: *b },
        },
        DynValue::Int(i) => GosVariantValue {
            tag: 0,
            data: GosVariantPayload { i64_: *i },
        },
        DynValue::Float(f) => GosVariantValue {
            tag: 1,
            data: GosVariantPayload { f64_: *f },
        },
        DynValue::Char(c) => GosVariantValue {
            tag: 3,
            data: GosVariantPayload { char_: *c as u32 },
        },
        DynValue::String(s) => GosVariantValue {
            tag: 4,
            data: GosVariantPayload {
                string: s.clone().to_output(),
            },
        },
        DynValue::Bytes(buf) => {
            // Bytes route through the i64-array vec for v0.4 wire
            // compatibility; the runtime sees them as a typed
            // packed `Vec<i64>` on the interp tier and as a
            // `GosBytes` on the compiled tier (via the Bytes
            // BindingAbi). Here, inside a DynValue payload, we
            // pack them as a nested vec.
            let widened: Vec<i64> = buf.iter().map(|b| i64::from(*b)).collect();
            let vec = make_gos_vec(&widened);
            GosVariantValue {
                tag: 5,
                data: GosVariantPayload { vec },
            }
        }
        DynValue::List(items) => {
            let payload: Vec<GosVariantValue> = items.iter().map(dyn_to_variant_value).collect();
            // For variant-of-variant nesting, we wrap the list
            // as a tuple-shape payload.
            let len = i32::try_from(payload.len()).unwrap_or(0);
            let fields_ptr: *mut GosVariantValue = if payload.is_empty() {
                std::ptr::null_mut()
            } else {
                let bytes = std::mem::size_of_val(payload.as_slice());
                let buf = arena_alloc(bytes).cast::<GosVariantValue>();
                if !buf.is_null() {
                    // SAFETY: fresh arena buffer, payload slice
                    // owned by this call; non-overlapping.
                    unsafe {
                        std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len());
                    }
                }
                buf
            };
            let tuple = arena_box(GosTuple {
                len,
                fields: fields_ptr,
            });
            GosVariantValue {
                tag: 7,
                data: GosVariantPayload { tuple },
            }
        }
        DynValue::Map(entries) => {
            // A map inside a DynValue payload is reified as a
            // tuple of (keys-tuple, values-tuple). The reader
            // unflattens it via the `value_to_dyn` walker.
            let keys: Vec<GosVariantValue> = entries
                .iter()
                .map(|(k, _)| dyn_to_variant_value(k))
                .collect();
            let values: Vec<GosVariantValue> = entries
                .iter()
                .map(|(_, v)| dyn_to_variant_value(v))
                .collect();
            let pair: Vec<GosVariantValue> = vec![
                tuple_to_variant_value(&keys),
                tuple_to_variant_value(&values),
            ];
            tuple_to_variant_value(&pair)
        }
        DynValue::Tagged { name, payload } => {
            let arm_payload: Vec<GosVariantValue> =
                payload.iter().map(dyn_to_variant_value).collect();
            let inner = make_dyn_variant(name, arm_payload);
            GosVariantValue {
                tag: 6,
                data: GosVariantPayload {
                    variant: inner.cast::<GosVariant>(),
                },
            }
        }
    }
}

fn tuple_to_variant_value(items: &[GosVariantValue]) -> GosVariantValue {
    let len = i32::try_from(items.len()).unwrap_or(0);
    let fields_ptr: *mut GosVariantValue = if items.is_empty() {
        std::ptr::null_mut()
    } else {
        let bytes = std::mem::size_of_val(items);
        let buf = arena_alloc(bytes).cast::<GosVariantValue>();
        if !buf.is_null() {
            // SAFETY: fresh arena buffer; items slice owned by
            // the caller; non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(items.as_ptr(), buf, items.len());
            }
        }
        buf
    };
    let tuple = arena_box(GosTuple {
        len,
        fields: fields_ptr,
    });
    GosVariantValue {
        tag: 7,
        data: GosVariantPayload { tuple },
    }
}

/// Builds a `GosDynVariant` (an arm-named variant) on the arena.
fn make_dyn_variant(name: &str, payload: Vec<GosVariantValue>) -> *mut GosDynVariant {
    let name_ptr = arena_cstr(name);
    let payload_len = i32::try_from(payload.len()).unwrap_or(0);
    let payload_ptr: *mut GosVariantValue = if payload.is_empty() {
        std::ptr::null_mut()
    } else {
        let bytes = std::mem::size_of_val(payload.as_slice());
        let buf = arena_alloc(bytes).cast::<GosVariantValue>();
        if !buf.is_null() {
            // SAFETY: fresh arena buffer, payload slice owned by
            // this call; non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len());
            }
        }
        buf
    };
    arena_box(GosDynVariant {
        name: name_ptr,
        payload_len,
        pad: 0,
        payload: payload_ptr,
    })
}

unsafe fn read_dyn_variant(p: *const GosDynVariant) -> DynValue {
    if p.is_null() {
        return DynValue::Nil;
    }
    let v = unsafe { &*p };
    let name = if v.name.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(v.name) }
            .to_string_lossy()
            .into_owned()
    };
    let payload_len = usize::try_from(v.payload_len.max(0)).unwrap_or(0);
    let payload: Vec<DynValue> = if payload_len == 0 || v.payload.is_null() {
        Vec::new()
    } else {
        let slice = unsafe { std::slice::from_raw_parts(v.payload, payload_len) };
        slice.iter().map(read_variant_value).collect()
    };
    unbare(name, payload)
}

/// Gives back the value a `$`-headed wire arm stands for, and every other arm
/// as the named arm it is. The two directions have to agree: `to_output`
/// wraps a bare value under one of these names, and nothing else may.
fn unbare(name: String, mut payload: Vec<DynValue>) -> DynValue {
    let single = |payload: &mut Vec<DynValue>| {
        if payload.is_empty() {
            DynValue::Nil
        } else {
            payload.remove(0)
        }
    };
    match name.as_str() {
        "$Nil" => DynValue::Nil,
        "$Bool" | "$Int" | "$Float" | "$Char" | "$String" | "$Bytes" | "$Map" => {
            single(&mut payload)
        }
        "$List" => DynValue::List(payload),
        _ => DynValue::Tagged { name, payload },
    }
}

fn read_variant_value(v: &GosVariantValue) -> DynValue {
    // SAFETY: we honour the `tag` discriminant to pick the live
    // union field; tags outside the documented range are coerced
    // to Nil.
    unsafe {
        match v.tag {
            0 => DynValue::Int(v.data.i64_),
            1 => DynValue::Float(v.data.f64_),
            2 => DynValue::Bool(v.data.bool_),
            3 => char::from_u32(v.data.char_).map_or(DynValue::Nil, DynValue::Char),
            4 => {
                if v.data.string.is_null() {
                    DynValue::String(String::new())
                } else {
                    DynValue::String(CStr::from_ptr(v.data.string).to_string_lossy().into_owned())
                }
            }
            5 => {
                let vec = v.data.vec;
                let items = read_gos_vec_i64(vec);
                // Heuristic: an i64-vec whose every element is in
                // u8 range is treated as Bytes - matching the
                // interp-tier policy in `conv.rs::value_to_dyn`.
                if items.iter().all(|x| (0..=255).contains(x)) {
                    DynValue::Bytes(items.iter().map(|x| *x as u8).collect())
                } else {
                    DynValue::List(items.into_iter().map(DynValue::Int).collect())
                }
            }
            6 => read_dyn_variant(v.data.variant.cast::<GosDynVariant>()),
            7 => {
                let t = v.data.tuple;
                if t.is_null() {
                    return DynValue::List(Vec::new());
                }
                let header = &*t;
                let len = usize::try_from(header.len.max(0)).unwrap_or(0);
                if header.fields.is_null() || len == 0 {
                    return DynValue::List(Vec::new());
                }
                let slice = std::slice::from_raw_parts(header.fields, len);
                DynValue::List(slice.iter().map(read_variant_value).collect())
            }
            _ => DynValue::Nil,
        }
    }
}

impl BindingAbi for DynValue {
    type Input = *const GosDynVariant;
    type Output = *mut GosDynVariant;
    const TYPE: Type = Type::Variant(&[]);

    unsafe fn from_input(input: *const GosDynVariant) -> Self {
        unsafe { read_dyn_variant(input) }
    }

    fn to_output(self) -> *mut GosDynVariant {
        // The DynValue is always emitted as a tagged variant on the wire; a
        // value that is not a named arm rides under a `$`-headed name the
        // reader gives back as the bare value it stands for.
        match self {
            DynValue::Tagged { name, payload } => {
                let arm_payload: Vec<GosVariantValue> =
                    payload.iter().map(dyn_to_variant_value).collect();
                make_dyn_variant(&name, arm_payload)
            }
            // A value that is not a named arm still crosses as one, under a
            // `$`-headed name no arm can carry - `$` is not an identifier
            // character - so the reader gives back the bare value rather than
            // a arm that happens to share its spelling.
            DynValue::Nil => make_dyn_variant("$Nil", Vec::new()),
            DynValue::Bool(b) => {
                make_dyn_variant("$Bool", vec![dyn_to_variant_value(&DynValue::Bool(b))])
            }
            DynValue::Int(i) => {
                make_dyn_variant("$Int", vec![dyn_to_variant_value(&DynValue::Int(i))])
            }
            DynValue::Float(f) => {
                make_dyn_variant("$Float", vec![dyn_to_variant_value(&DynValue::Float(f))])
            }
            DynValue::Char(c) => {
                make_dyn_variant("$Char", vec![dyn_to_variant_value(&DynValue::Char(c))])
            }
            DynValue::String(s) => {
                make_dyn_variant("$String", vec![dyn_to_variant_value(&DynValue::String(s))])
            }
            DynValue::Bytes(b) => {
                make_dyn_variant("$Bytes", vec![dyn_to_variant_value(&DynValue::Bytes(b))])
            }
            DynValue::List(items) => {
                make_dyn_variant("$List", items.iter().map(dyn_to_variant_value).collect())
            }
            DynValue::Map(entries) => {
                make_dyn_variant("$Map", vec![dyn_to_variant_value(&DynValue::Map(entries))])
            }
        }
    }
}

// --- ABI 0.4: Callback (compiled-tier) -------------------------------
//
// On the compiled tier, a Gossamer-side callable is represented
// as a u64 handle into a per-call dispatch table. The codegen
// emits the registration before the binding call and the cleanup
// after - bindings receive only the handle.
//
// For ABI 0.4, the compiled-tier invocation surface is the
// runtime helper `gos_rt_callback_invoke` declared below. Calling
// it from inside a binding requires the binding to use the
// returned handle to thread back into the Gossamer scheduler.
// The handle is INVALID after the binding fn returns; bindings
// MUST NOT retain it.

unsafe extern "C" {
    /// Compiled-tier callback dispatcher. Returns 0 on success,
    /// non-zero on error. Result is written into `result_out`
    /// (caller-allocated `GosVariantValue`).
    #[allow(dead_code)]
    fn gos_rt_callback_invoke(
        handle: u64,
        args: *const GosVariantValue,
        args_len: u32,
        result_out: *mut GosVariantValue,
    ) -> i32;
}

/// Compiled-tier handle to a Gossamer-side callback. ABI 0.4
/// passes this as a `u64` over the wire.
///
/// This is the compiled-tier counterpart to
/// [`crate::conv::BindingCallback`] (the interp-tier wrapper).
/// Binding code that needs to work in both tiers should accept
/// `BindingCallback` for the interp path and use this type via
/// the `BindingAbi` impl for the compiled path.
#[derive(Debug, Clone, Copy)]
pub struct NativeCallback {
    /// Opaque dispatch-table handle.
    pub handle: u64,
}

impl NativeCallback {
    /// Invokes the callback with `args` and returns the result.
    ///
    /// # Safety
    /// `self.handle` must be a valid handle handed to the
    /// binding by the codegen during the current binding call.
    /// Calling after the binding fn returns is undefined
    /// behaviour.
    pub unsafe fn invoke_raw(&self, args: &[GosVariantValue]) -> Result<GosVariantValue, i32> {
        let mut result = GosVariantValue {
            tag: 0,
            data: GosVariantPayload { i64_: 0 },
        };
        let args_ptr = if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        };
        let args_len = u32::try_from(args.len()).unwrap_or(0);
        // SAFETY: the contract above plus the codegen's guarantee
        // that gos_rt_callback_invoke is reachable in the runtime.
        let rc =
            unsafe { gos_rt_callback_invoke(self.handle, args_ptr, args_len, &raw mut result) };
        if rc == 0 { Ok(result) } else { Err(rc) }
    }
}

impl BindingAbi for NativeCallback {
    type Input = u64;
    type Output = u64;
    const TYPE: Type = Type::Callback(&[], &Type::Any);

    unsafe fn from_input(input: u64) -> Self {
        Self { handle: input }
    }

    fn to_output(self) -> u64 {
        self.handle
    }
}

// `NativeCallback` is a compiled-tier-only handle. The interp
// path materialises it from a `Value::Int(handle as i64)` and
// returns it as the same shape. Bindings that want a true
// interp-side callable should declare `BindingCallback` instead.
impl crate::conv::FromGos for NativeCallback {
    fn from_gos(
        value: &gossamer_interp::value::Value,
    ) -> gossamer_interp::value::RuntimeResult<Self> {
        match value {
            gossamer_interp::value::Value::Int(i) => Ok(Self { handle: *i as u64 }),
            gossamer_interp::value::Value::Uint(u) => Ok(Self { handle: *u }),
            other => Err(gossamer_interp::value::RuntimeError::Type(format!(
                "expected callback handle (u64), found {other:?}"
            ))),
        }
    }
}

impl crate::conv::ToGos for NativeCallback {
    fn to_gos(self) -> gossamer_interp::value::Value {
        gossamer_interp::value::Value::Int(self.handle as i64)
    }
}

impl crate::sig::SigType for NativeCallback {
    const TYPE: Type = Type::Callback(&[], &Type::Any);
}

// --- ABI 0.4: Vec<u8> as a plain byte vec (non-Bytes path) ---------

impl BindingAbi for Vec<u8> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::I64);

    unsafe fn from_input(input: *const GosVec) -> Self {
        // Reuse the existing byte-pack path (same as Vec<bool>);
        // bytes are stored 1-byte-per-element in the GosVec data
        // buffer.
        unsafe { read_gos_vec_u8(input) }
    }

    fn to_output(self) -> *mut GosVec {
        make_gos_vec(&self)
    }
}

unsafe fn read_gos_vec_u8(p: *const GosVec) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*p };
    let (len, stride) = vec_len_stride(header);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    if stride == 1 {
        let slice = unsafe { std::slice::from_raw_parts(header.ptr, len) };
        return slice.to_vec();
    }
    // Word-width byte vecs (the canonical one-i64-slot-per-byte
    // runtime shape) truncate each slot to its low byte.
    (0..len)
        .map(|i| unsafe { vec_elem_word(header, i, stride) } as u8)
        .collect()
}

// --- Default impls for Output types ---------------------------------
//
// The macro-generated thunk's panic-catch path needs a
// `Default::default()` for every `Output` type. Pointer outputs
// already have one (null ptr). The non-pointer outputs (i64, etc.)
// also already have one. The new shapes here all return pointers
// or `u64`, both `Default`. No additional code needed.

// ---------------------------------------------------------------------
// Phase 1 - expanded type vocabulary.
//
// Helpers + impls for the most-asked-for binding shapes the
// original allowlist did not cover. See `~/dev/contexts/gos/rustergo.md`
// §4.7 for the design and §6 for the file/line boundary.
// ---------------------------------------------------------------------

// --- variant payload tag constants -----------------------------------

const VAR_TAG_I64: i32 = 0;
const VAR_TAG_F64: i32 = 1;
const VAR_TAG_BOOL: i32 = 2;
const VAR_TAG_CHAR: i32 = 3;
const VAR_TAG_STRING: i32 = 4;
const VAR_TAG_VEC: i32 = 5;
#[allow(dead_code, reason = "documents the GosVariantValue tag namespace")]
const VAR_TAG_VARIANT: i32 = 6;
#[allow(dead_code, reason = "documents the GosVariantValue tag namespace")]
const VAR_TAG_TUPLE: i32 = 7;
#[allow(dead_code, reason = "documents the GosVariantValue tag namespace")]
const VAR_TAG_OPAQUE: i32 = 8;

// --- variant-value pack / unpack primitives --------------------------

unsafe fn variant_value_f64(v: f64) -> GosVariantValue {
    GosVariantValue {
        tag: VAR_TAG_F64,
        data: GosVariantPayload { f64_: v },
    }
}

unsafe fn variant_value_bool(v: bool) -> GosVariantValue {
    GosVariantValue {
        tag: VAR_TAG_BOOL,
        data: GosVariantPayload { bool_: v },
    }
}

unsafe fn variant_value_char(v: char) -> GosVariantValue {
    GosVariantValue {
        tag: VAR_TAG_CHAR,
        data: GosVariantPayload { char_: v as u32 },
    }
}

unsafe fn variant_value_string_owned(s: String) -> GosVariantValue {
    GosVariantValue {
        tag: VAR_TAG_STRING,
        data: GosVariantPayload {
            string: s.to_output(),
        },
    }
}

unsafe fn variant_value_vec(p: *mut GosVec) -> GosVariantValue {
    GosVariantValue {
        tag: VAR_TAG_VEC,
        data: GosVariantPayload { vec: p },
    }
}

unsafe fn variant_value_unit() -> GosVariantValue {
    // `()` packs as an i64 zero; the consumer is expected to ignore
    // the payload anyway (tag 0 with no semantic content).
    GosVariantValue {
        tag: VAR_TAG_I64,
        data: GosVariantPayload { i64_: 0 },
    }
}

// --- Option<T> impls (additions) -------------------------------------

/// Helper: read a single-payload variant header into `(tag, payload)`.
unsafe fn read_single_payload(p: *const GosVariant) -> Option<(i32, GosVariantValue)> {
    if p.is_null() {
        return None;
    }
    let v = unsafe { &*p };
    if v.payload_len == 0 || v.payload.is_null() {
        return None;
    }
    let payload = unsafe { *v.payload };
    Some((v.tag, payload))
}

impl BindingAbi for Option<String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        if payload.tag != VAR_TAG_STRING {
            return None;
        }
        Some(unsafe { String::from_input(payload.data.string) })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(s) => make_variant(1, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Option<bool> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::Bool);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        Some(unsafe { payload.data.bool_ })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(b) => make_variant(1, vec![unsafe { variant_value_bool(b) }]),
        }
    }
}

impl BindingAbi for Option<f64> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::F64);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        Some(unsafe { payload.data.f64_ })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(v) => make_variant(1, vec![unsafe { variant_value_f64(v) }]),
        }
    }
}

impl BindingAbi for Option<char> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::Char);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        char::from_u32(unsafe { payload.data.char_ })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(c) => make_variant(1, vec![unsafe { variant_value_char(c) }]),
        }
    }
}

impl BindingAbi for Option<Vec<i64>> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::Vec(&Type::I64));

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        Some(unsafe { read_gos_vec_i64(payload.data.vec) })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(v) => {
                let inner = make_gos_vec(&v);
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
        }
    }
}

impl BindingAbi for Option<Vec<String>> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Option(&Type::Vec(&Type::String));

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return None,
        };
        if tag == 0 {
            return None;
        }
        Some(unsafe { read_gos_vec_strings(payload.data.vec) })
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            None => make_variant(0, Vec::new()),
            Some(v) => {
                let ptrs: Vec<*mut c_char> = v.into_iter().map(BindingAbi::to_output).collect();
                let inner = make_gos_vec(&ptrs);
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
        }
    }
}

// --- Result<T, String> impls (additions) -----------------------------

impl BindingAbi for Result<String, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::String, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        let s = unsafe { String::from_input(payload.data.string) };
        if tag == 1 { Ok(s) } else { Err(s) }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(s) => make_variant(1, vec![unsafe { variant_value_string_owned(s) }]),
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Result<bool, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Bool, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.bool_ })
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(b) => make_variant(1, vec![unsafe { variant_value_bool(b) }]),
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Result<f64, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::F64, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.f64_ })
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => make_variant(1, vec![unsafe { variant_value_f64(v) }]),
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Result<(), String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Unit, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(())
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(()) => make_variant(1, vec![unsafe { variant_value_unit() }]),
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Result<Vec<i64>, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Vec(&Type::I64), &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(unsafe { read_gos_vec_i64(payload.data.vec) })
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => {
                let inner = make_gos_vec(&v);
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

impl BindingAbi for Result<Vec<String>, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Vec(&Type::String), &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(unsafe { read_gos_vec_strings(payload.data.vec) })
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => {
                let ptrs: Vec<*mut c_char> = v.into_iter().map(BindingAbi::to_output).collect();
                let inner = make_gos_vec(&ptrs);
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
            Err(s) => make_variant(0, vec![unsafe { variant_value_string_owned(s) }]),
        }
    }
}

/// Fallible binary payloads are the common wire contract for Redis,
/// protobuf/RPC, MessagePack, and CBOR adapters. Inside a `Result` variant,
/// bytes use the established vector payload tag: `GosVariantValue` predates
/// `GosBytes` and has no bytes pointer arm of its own.
impl BindingAbi for Result<Bytes, String> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Bytes, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(String::new()),
        };
        if tag == 1 {
            Ok(Bytes::new(unsafe { read_gos_vec_u8(payload.data.vec) }))
        } else {
            Err(unsafe { String::from_input(payload.data.string) })
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(bytes) => {
                let inner = make_gos_vec(&bytes.into_inner());
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
            Err(message) => make_variant(0, vec![unsafe { variant_value_string_owned(message) }]),
        }
    }
}

// --- HashMap impls (additions) ---------------------------------------

unsafe fn read_gos_map_keys_values_str_vec_i64(
    p: *const BindingGosMap,
) -> (Vec<String>, Vec<*const GosVec>) {
    if p.is_null() {
        return (Vec::new(), Vec::new());
    }
    let m = unsafe { &*p };
    let keys = unsafe { read_gos_vec_strings(m.keys) };
    // values is a GosVec of *const GosVec pointers
    if m.values.is_null() {
        return (keys, Vec::new());
    }
    let vh = unsafe { &*m.values };
    let len = usize::try_from(vh.len.max(0)).unwrap_or(0);
    if vh.ptr.is_null() || len == 0 {
        return (keys, Vec::new());
    }
    let slice = unsafe { std::slice::from_raw_parts(vh.ptr.cast::<*const GosVec>(), len) };
    (keys, slice.to_vec())
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<String, Vec<i64>> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::String, &Type::Vec(&Type::I64));

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        let (keys, value_ptrs) = unsafe { read_gos_map_keys_values_str_vec_i64(input) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, vptr) in keys.into_iter().zip(value_ptrs) {
            let v = unsafe { read_gos_vec_i64(vptr) };
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut key_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        let mut value_ptrs: Vec<*mut GosVec> = Vec::with_capacity(self.len());
        for (k, v) in self {
            key_ptrs.push(k.to_output());
            value_ptrs.push(make_gos_vec(&v));
        }
        make_gos_map(&key_ptrs, &value_ptrs)
    }
}

unsafe fn read_gos_map_keys_values_i64_str(p: *const BindingGosMap) -> (Vec<i64>, Vec<String>) {
    if p.is_null() {
        return (Vec::new(), Vec::new());
    }
    let m = unsafe { &*p };
    let keys = unsafe { read_gos_vec_i64(m.keys) };
    let values = unsafe { read_gos_vec_strings(m.values) };
    (keys, values)
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<i64, String> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::I64, &Type::String);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        let (keys, values) = unsafe { read_gos_map_keys_values_i64_str(input) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut keys: Vec<i64> = Vec::with_capacity(self.len());
        let mut val_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        for (k, v) in self {
            keys.push(k);
            val_ptrs.push(v.to_output());
        }
        make_gos_map(&keys, &val_ptrs)
    }
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<String, bool> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::String, &Type::Bool);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        if input.is_null() {
            return HashMap::new();
        }
        let m = unsafe { &*input };
        let keys = unsafe { read_gos_vec_strings(m.keys) };
        let values = unsafe { read_gos_vec_bools(m.values) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut key_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        let mut values: Vec<u8> = Vec::with_capacity(self.len());
        for (k, v) in self {
            key_ptrs.push(k.to_output());
            values.push(u8::from(v));
        }
        make_gos_map(&key_ptrs, &values)
    }
}

#[allow(
    clippy::implicit_hasher,
    reason = "ABI surface; binding receives a freshly built HashMap."
)]
impl BindingAbi for std::collections::HashMap<String, f64> {
    type Input = *const BindingGosMap;
    type Output = *mut BindingGosMap;
    const TYPE: Type = Type::Map(&Type::String, &Type::F64);

    unsafe fn from_input(input: *const BindingGosMap) -> Self {
        if input.is_null() {
            return HashMap::new();
        }
        let m = unsafe { &*input };
        let keys = unsafe { read_gos_vec_strings(m.keys) };
        let values = unsafe { read_gos_vec_f64(m.values) };
        let mut out = HashMap::with_capacity(keys.len());
        for (k, v) in keys.into_iter().zip(values) {
            out.entry(k).or_insert(v);
        }
        out
    }

    fn to_output(self) -> *mut BindingGosMap {
        let mut key_ptrs: Vec<*mut c_char> = Vec::with_capacity(self.len());
        let mut values: Vec<f64> = Vec::with_capacity(self.len());
        for (k, v) in self {
            key_ptrs.push(k.to_output());
            values.push(v);
        }
        make_gos_map(&key_ptrs, &values)
    }
}

// --- Tuple impls -----------------------------------------------------
//
// Tuples lower to `GosTuple { len, fields: *mut GosVariantValue }`.
// Each field's type tag picks the live `GosVariantPayload` member.
// Supported field types per element: i64, f64, bool, char, String.
// Nested aggregates inside a tuple field go via `Type::Tuple/Vec/etc.`
// - bindings author the explicit `BindingAbi` for the outer tuple
// shape.

fn make_gos_tuple(fields: Vec<GosVariantValue>) -> *mut GosTuple {
    let len = i32::try_from(fields.len()).unwrap_or(0);
    let fields_ptr: *mut GosVariantValue = if fields.is_empty() {
        std::ptr::null_mut()
    } else {
        let bytes = std::mem::size_of_val(fields.as_slice());
        let buf = arena_alloc(bytes).cast::<GosVariantValue>();
        if !buf.is_null() {
            // SAFETY: arena buffer is `bytes` long, fresh, and
            // exclusively ours; payload slice is non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(fields.as_ptr(), buf, fields.len());
            }
        }
        buf
    };
    arena_box(GosTuple {
        len,
        fields: fields_ptr,
    })
}

/// Reads the GosTuple's payload buffer into a Vec of variant values.
unsafe fn read_gos_tuple(p: *const GosTuple) -> Vec<GosVariantValue> {
    if p.is_null() {
        return Vec::new();
    }
    let t = unsafe { &*p };
    let len = usize::try_from(t.len.max(0)).unwrap_or(0);
    if t.fields.is_null() || len == 0 {
        return Vec::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(t.fields, len) };
    slice.to_vec()
}

/// Materialise a single tuple element from a `GosVariantValue`.
/// Dispatches on the value's tag; returns `Default::default()` if
/// the wire layout disagrees with the expected type.
unsafe fn unpack_i64(v: GosVariantValue) -> i64 {
    if v.tag == VAR_TAG_I64 {
        unsafe { v.data.i64_ }
    } else {
        0
    }
}
unsafe fn unpack_f64(v: GosVariantValue) -> f64 {
    if v.tag == VAR_TAG_F64 {
        unsafe { v.data.f64_ }
    } else {
        0.0
    }
}
unsafe fn unpack_bool(v: GosVariantValue) -> bool {
    if v.tag == VAR_TAG_BOOL {
        unsafe { v.data.bool_ }
    } else {
        false
    }
}
unsafe fn unpack_string(v: GosVariantValue) -> String {
    if v.tag == VAR_TAG_STRING {
        unsafe { String::from_input(v.data.string) }
    } else {
        String::new()
    }
}

impl BindingAbi for (i64, String) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::I64, Type::String]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter.next().map_or(0, |v| unsafe { unpack_i64(v) });
        let b = iter
            .next()
            .map_or_else(String::new, |v| unsafe { unpack_string(v) });
        (a, b)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b) = self;
        make_gos_tuple(vec![unsafe { variant_value_i64(a) }, unsafe {
            variant_value_string_owned(b)
        }])
    }
}

impl BindingAbi for (String, i64) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::String, Type::I64]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter
            .next()
            .map_or_else(String::new, |v| unsafe { unpack_string(v) });
        let b = iter.next().map_or(0, |v| unsafe { unpack_i64(v) });
        (a, b)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b) = self;
        make_gos_tuple(vec![unsafe { variant_value_string_owned(a) }, unsafe {
            variant_value_i64(b)
        }])
    }
}

impl BindingAbi for (i64, i64) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::I64, Type::I64]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter.next().map_or(0, |v| unsafe { unpack_i64(v) });
        let b = iter.next().map_or(0, |v| unsafe { unpack_i64(v) });
        (a, b)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b) = self;
        make_gos_tuple(vec![unsafe { variant_value_i64(a) }, unsafe {
            variant_value_i64(b)
        }])
    }
}

impl BindingAbi for (f64, f64) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::F64, Type::F64]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter.next().map_or(0.0, |v| unsafe { unpack_f64(v) });
        let b = iter.next().map_or(0.0, |v| unsafe { unpack_f64(v) });
        (a, b)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b) = self;
        make_gos_tuple(vec![unsafe { variant_value_f64(a) }, unsafe {
            variant_value_f64(b)
        }])
    }
}

impl BindingAbi for (String, String) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::String, Type::String]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter
            .next()
            .map_or_else(String::new, |v| unsafe { unpack_string(v) });
        let b = iter
            .next()
            .map_or_else(String::new, |v| unsafe { unpack_string(v) });
        (a, b)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b) = self;
        make_gos_tuple(vec![unsafe { variant_value_string_owned(a) }, unsafe {
            variant_value_string_owned(b)
        }])
    }
}

// --- Result<T, GosError> impls (Phase 2) -----------------------------
//
// Wire-equivalent to `Result<T, String>` - the rendered message
// is the Err payload. Cause chains are flattened at the boundary
// by `GosError::render()`. Bindings get `?`-propagation with rich
// causes (interp tier preserves the full chain via the Variant
// payload); the compiled tier sees the rendered string.

impl BindingAbi for Result<i64, crate::error::GosError> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::I64, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(crate::error::GosError::new("")),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.i64_ })
        } else {
            let msg = unsafe { String::from_input(payload.data.string) };
            Err(crate::error::GosError::new(msg))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => make_variant(1, vec![unsafe { variant_value_i64(v) }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string_owned(e.render()) }]),
        }
    }
}

impl BindingAbi for Result<String, crate::error::GosError> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::String, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(crate::error::GosError::new("")),
        };
        let s = unsafe { String::from_input(payload.data.string) };
        if tag == 1 {
            Ok(s)
        } else {
            Err(crate::error::GosError::new(s))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(s) => make_variant(1, vec![unsafe { variant_value_string_owned(s) }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string_owned(e.render()) }]),
        }
    }
}

impl BindingAbi for Result<(), crate::error::GosError> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Unit, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(crate::error::GosError::new("")),
        };
        if tag == 1 {
            Ok(())
        } else {
            let msg = unsafe { String::from_input(payload.data.string) };
            Err(crate::error::GosError::new(msg))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(()) => make_variant(1, vec![unsafe { variant_value_unit() }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string_owned(e.render()) }]),
        }
    }
}

impl BindingAbi for Result<bool, crate::error::GosError> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Bool, &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(crate::error::GosError::new("")),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.bool_ })
        } else {
            let msg = unsafe { String::from_input(payload.data.string) };
            Err(crate::error::GosError::new(msg))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(b) => make_variant(1, vec![unsafe { variant_value_bool(b) }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string_owned(e.render()) }]),
        }
    }
}

impl BindingAbi for Result<Vec<i64>, crate::error::GosError> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Vec(&Type::I64), &Type::String);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(crate::error::GosError::new("")),
        };
        if tag == 1 {
            Ok(unsafe { read_gos_vec_i64(payload.data.vec) })
        } else {
            let msg = unsafe { String::from_input(payload.data.string) };
            Err(crate::error::GosError::new(msg))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => {
                let inner = make_gos_vec(&v);
                make_variant(1, vec![unsafe { variant_value_vec(inner) }])
            }
            Err(e) => make_variant(0, vec![unsafe { variant_value_string_owned(e.render()) }]),
        }
    }
}

impl BindingAbi for (i64, String, bool) {
    type Input = *const GosTuple;
    type Output = *mut GosTuple;
    const TYPE: Type = Type::Tuple(&[Type::I64, Type::String, Type::Bool]);

    unsafe fn from_input(input: *const GosTuple) -> Self {
        let fields = unsafe { read_gos_tuple(input) };
        let mut iter = fields.into_iter();
        let a = iter.next().map_or(0, |v| unsafe { unpack_i64(v) });
        let b = iter
            .next()
            .map_or_else(String::new, |v| unsafe { unpack_string(v) });
        let c = iter.next().is_some_and(|v| unsafe { unpack_bool(v) });
        (a, b, c)
    }

    fn to_output(self) -> *mut GosTuple {
        let (a, b, c) = self;
        make_gos_tuple(vec![
            unsafe { variant_value_i64(a) },
            unsafe { variant_value_string_owned(b) },
            unsafe { variant_value_bool(c) },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_round_trip_through_abi() {
        unsafe {
            assert_eq!(<i64 as BindingAbi>::from_input(7), 7);
            assert!(<bool as BindingAbi>::from_input(true));
            assert_eq!(<f64 as BindingAbi>::from_input(1.5), 1.5);
        }
    }

    #[test]
    fn string_round_trip() {
        let s = String::from("hello");
        let raw = s.to_output();
        let back = unsafe { String::from_input(raw) };
        assert_eq!(back, "hello");
        // No explicit free: arena reclamation lives behind
        // `gos_rt_gc_reset`, called at the runtime's tick
        // boundary. The test exits before any reset, so the
        // arena holds the bytes for the duration of the test.
    }

    #[test]
    fn vec_i64_round_trip() {
        let v: Vec<i64> = vec![1, 2, 3];
        let raw = v.to_output();
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(raw) };
        assert_eq!(back, vec![1, 2, 3]);
    }

    /// Builds a leaked GosVec over a raw byte buffer with the given
    /// element stride, mimicking the runtime's packed vec shapes.
    fn packed_vec(bytes: &[u8], elem_bytes: u32, len: i64) -> *const GosVec {
        let mut buf = bytes.to_vec();
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf);
        Box::into_raw(Box::new(GosVec {
            len,
            cap: len,
            elem_bytes,
            ptr,
        }))
    }

    #[test]
    fn vec_i64_input_honors_packed_byte_stride() {
        // The `resp.raw_bytes` shape: elem_bytes=1, one byte per
        // element. A fixed 8-byte stride would read garbage here.
        let raw = packed_vec(&[0x68, 0xFF, 0x00, 0x69], 1, 4);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(raw) };
        assert_eq!(back, vec![0x68, 0xFF, 0x00, 0x69]);
    }

    #[test]
    fn vec_i64_input_honors_half_and_word_strides() {
        let half = packed_vec(&500u16.to_le_bytes(), 2, 1);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(half) };
        assert_eq!(back, vec![500]);

        let word = packed_vec(&70000u32.to_le_bytes(), 4, 1);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(word) };
        assert_eq!(back, vec![70000]);
    }

    #[test]
    fn vec_u8_input_truncates_word_width_slots() {
        // Canonical runtime byte-vec shape: one i64 slot per byte.
        let mut bytes = Vec::new();
        for v in [0x68i64, 0xFF, 0x00] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let raw = packed_vec(&bytes, 8, 3);
        let back = unsafe { read_gos_vec_u8(raw) };
        assert_eq!(back, vec![0x68, 0xFF, 0x00]);
    }

    #[test]
    fn vec_vec_i64_round_trip() {
        let v: Vec<Vec<i64>> = vec![vec![1, 2], vec![3, 4]];
        let raw = v.to_output();
        let back: Vec<Vec<i64>> = unsafe { <Vec<Vec<i64>> as BindingAbi>::from_input(raw) };
        assert_eq!(back, vec![vec![1, 2], vec![3, 4]]);
    }

    #[test]
    fn option_round_trip() {
        let some_raw = Some(42_i64).to_output();
        assert_eq!(
            unsafe { <Option<i64> as BindingAbi>::from_input(some_raw) },
            Some(42)
        );

        let none_raw = Option::<i64>::None.to_output();
        assert_eq!(
            unsafe { <Option<i64> as BindingAbi>::from_input(none_raw) },
            None
        );
    }

    #[test]
    fn result_round_trip() {
        let ok_raw = Ok::<i64, String>(7).to_output();
        let back = unsafe { <Result<i64, String> as BindingAbi>::from_input(ok_raw) };
        assert_eq!(back, Ok(7));

        let err_raw = Err::<i64, _>("nope".to_string()).to_output();
        let back = unsafe { <Result<i64, String> as BindingAbi>::from_input(err_raw) };
        assert_eq!(back, Err("nope".to_string()));
    }

    #[test]
    fn result_bytes_round_trip() {
        let payload = Bytes::new(vec![0, 1, 127, 255]);
        let raw = Ok::<Bytes, String>(payload.clone()).to_output();
        let back = unsafe { <Result<Bytes, String> as BindingAbi>::from_input(raw) };
        assert_eq!(back, Ok(payload));

        let raw = Err::<Bytes, _>("bad frame".to_string()).to_output();
        let back = unsafe { <Result<Bytes, String> as BindingAbi>::from_input(raw) };
        assert_eq!(back, Err("bad frame".to_string()));
    }

    // --- ABI 0.4 round-trip coverage -------------------------------

    #[test]
    fn bytes_round_trip() {
        let payload: Vec<u8> = (0..=255u8).collect();
        let bytes = Bytes::new(payload.clone());
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert_eq!(back.as_slice(), payload.as_slice());
    }

    #[test]
    fn bytes_empty_round_trip() {
        let bytes = Bytes::default();
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert!(back.is_empty());
    }

    #[test]
    fn bytes_large_round_trip() {
        let payload: Vec<u8> = (0..16 * 1024).map(|i| (i % 256) as u8).collect();
        let bytes = Bytes::new(payload.clone());
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert_eq!(back.len(), payload.len());
        assert_eq!(back.as_slice(), payload.as_slice());
    }

    #[test]
    fn vec_u8_round_trip() {
        let payload: Vec<u8> = vec![0, 1, 127, 200, 255];
        let raw = payload.clone().to_output();
        let back = unsafe { <Vec<u8> as BindingAbi>::from_input(raw) };
        assert_eq!(back, payload);
    }

    #[test]
    fn hash_map_i64_i64_round_trip() {
        let mut m: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        m.insert(1, 100);
        m.insert(2, 200);
        m.insert(3, 300);
        let raw = m.clone().to_output();
        let back = unsafe { <std::collections::HashMap<i64, i64> as BindingAbi>::from_input(raw) };
        assert_eq!(back, m);
    }

    #[test]
    fn hash_map_string_string_round_trip() {
        let mut m: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        m.insert("content-type".into(), "application/json".into());
        m.insert("x-request-id".into(), "abc123".into());
        let raw = m.clone().to_output();
        let back =
            unsafe { <std::collections::HashMap<String, String> as BindingAbi>::from_input(raw) };
        assert_eq!(back, m);
    }

    #[test]
    fn hash_map_empty_round_trip() {
        let m: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let raw = m.to_output();
        let back = unsafe { <std::collections::HashMap<i64, i64> as BindingAbi>::from_input(raw) };
        assert!(back.is_empty());
    }

    #[test]
    fn dyn_value_nil_round_trip() {
        let raw = DynValue::Nil.to_output();
        let back = unsafe { <DynValue as BindingAbi>::from_input(raw) };
        // A bare value crosses under a `$`-headed wire name no declared arm
        // can carry, and comes back as the value rather than as an arm.
        assert!(matches!(back, DynValue::Nil), "got {back:?}");
    }

    #[test]
    fn dyn_value_tagged_round_trip() {
        let v = DynValue::Tagged {
            name: "Integer".to_string(),
            payload: vec![DynValue::Int(42)],
        };
        let raw = v.clone().to_output();
        let back = unsafe { <DynValue as BindingAbi>::from_input(raw) };
        let DynValue::Tagged { name, payload } = back else {
            panic!("expected Tagged");
        };
        assert_eq!(name, "Integer");
        assert_eq!(payload, vec![DynValue::Int(42)]);
    }

    #[test]
    fn dyn_value_redis_resp_array_shape() {
        // Mirrors a Redis RESP array of mixed types: an integer
        // followed by a bulk-string-encoded byte payload.
        let v = DynValue::Tagged {
            name: "Array".to_string(),
            payload: vec![
                DynValue::Tagged {
                    name: "Integer".to_string(),
                    payload: vec![DynValue::Int(7)],
                },
                DynValue::Tagged {
                    name: "BulkString".to_string(),
                    payload: vec![DynValue::Bytes(b"hello".to_vec())],
                },
            ],
        };
        let raw = v.clone().to_output();
        let back = unsafe { <DynValue as BindingAbi>::from_input(raw) };
        let DynValue::Tagged { name, payload } = back else {
            panic!("expected Tagged");
        };
        assert_eq!(name, "Array");
        assert_eq!(payload.len(), 2);
    }

    #[test]
    fn native_callback_passes_handle_through() {
        let cb = NativeCallback { handle: 42 };
        let raw = cb.to_output();
        let back = unsafe { <NativeCallback as BindingAbi>::from_input(raw) };
        assert_eq!(back.handle, 42);
    }
}
