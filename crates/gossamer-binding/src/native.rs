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
use crate::value::{
    CallbackOrigin, CallbackRef, NativeDispatch, RuntimeError, RuntimeResult, Value,
};
use crate::wire_json::{Nested, WireJson};

/// Moves `items` into a field array the runtime's allocator made, laid
/// out as the `Box<[T]>` the runtime frees it as; null when empty.
fn host_slice<T: Copy>(items: &[T]) -> *mut T {
    if items.is_empty() {
        return std::ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::array::<T>(items.len()) else {
        return std::ptr::null_mut();
    };
    let p = crate::host::alloc(layout.size(), layout.align()).cast::<T>();
    if !p.is_null() {
        // SAFETY: `p` is a fresh allocation for `items.len()` values of `T`.
        unsafe { std::ptr::copy_nonoverlapping(items.as_ptr(), p, items.len()) };
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
    /// String payload: a runtime string the receiver owns.
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
/// The header and its buffer come from the runtime's allocator, laid out
/// as the `Box<GosBytes>` and `Vec<u8>` (cap = len) the runtime frees.
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
/// `gossamer_runtime::c_abi::gos_rt_binding_map_free`, which frees both
/// halves and what their elements own.
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
/// ABI 0.4. Carries an arm name (a NUL-terminated runtime string)
/// plus a payload list. The arm name is the only piece of
/// metadata runtime code needs to dispatch on; payload typing is
/// the binding signature's responsibility.
#[repr(C)]
#[derive(Debug)]
pub struct GosDynVariant {
    /// Arm name as a NUL-terminated runtime string.
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

    /// Whether the value crosses as JSON text ([`crate::wire_json`]), which
    /// the caller's generated walk writes or reads on every tier.
    const JSON: bool = false;

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

    /// The value a [`Self::JSON`] type's text spells, for a VM call, which
    /// passes the same text the compiled tiers do.
    ///
    /// # Errors
    /// The text does not spell a value of this type, or the type does not
    /// cross as text.
    fn read_wire_text(text: &str) -> Result<Self, String> {
        let _ = text;
        Err("this type does not cross as text".to_string())
    }

    /// The text a [`Self::JSON`] type crosses as; empty for any other type.
    fn write_wire_text(&self) -> String {
        String::new()
    }
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
        // A NUL ends the text where a reader of the C string stops, so
        // interior ones are left out.
        if self.contains('\0') {
            return crate::host::str_new(&self.replace('\0', ""));
        }
        crate::host::str_new(&self)
    }
}

// --- Vec ------------------------------------------------------------

/// Builds a runtime-owned `GosVec` from `elements`.
///
/// Binding outputs must go through the runtime allocator: the header carries
/// a versioned owner with its generation and destructor identity, so a later
/// compiled-tier drop never needs to infer provenance from the header address.
fn make_gos_vec<T: Copy>(elements: &[T], elem_kind: u8) -> *mut GosVec {
    let elem_bytes = u32::try_from(std::mem::size_of::<T>()).unwrap_or(0);
    let len = i64::try_from(elements.len()).unwrap_or(0);
    // SAFETY: runtime allocator returns a header valid for this ABI prefix;
    // copying `T: Copy` bytes into its reserved buffer preserves the element.
    let out = crate::host::vec_with_capacity(elem_bytes, len, elem_kind).cast::<GosVec>();
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

impl BindingAbi for Vec<i64> {
    type Input = *const GosVec;
    type Output = *mut GosVec;
    const TYPE: Type = Type::Vec(&Type::I64);

    unsafe fn from_input(input: *const GosVec) -> Self {
        unsafe { read_gos_vec_i64(input) }
    }

    fn to_output(self) -> *mut GosVec {
        make_gos_vec(&self, gossamer_binding_abi::vec_kind::PRIMITIVE)
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
        make_gos_vec(&self, gossamer_binding_abi::vec_kind::PRIMITIVE)
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
        make_gos_vec(&bytes, gossamer_binding_abi::vec_kind::PRIMITIVE)
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
        make_gos_vec(&ptrs, gossamer_binding_abi::vec_kind::STRING)
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
    let payload_ptr: *mut GosVariantValue = host_slice(&payload);
    host_box(GosVariant {
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

impl<E: WireError> BindingAbi for Result<i64, E> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::I64, &E::TYPE);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        unsafe { read_result_i64_string(input) }.map_err(E::from_wire)
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => make_variant(1, vec![unsafe { variant_value_i64(v) }]),
            Err(e) => make_variant(0, vec![unsafe { variant_value_string(e.into_wire()) }]),
        }
    }
}

// --- ABI 0.4: Bytes (Vec<u8> via the `Bytes` newtype) ----------------

/// Builds a `GosBytes` header and its buffer with the runtime's allocator,
/// laid out as the `Box<GosBytes>` and `Vec<u8>` the runtime frees them as.
fn make_gos_bytes(bytes: Vec<u8>) -> *mut GosBytes {
    let len = bytes.len();
    let ptr = if len == 0 {
        std::ptr::NonNull::<u8>::dangling().as_ptr()
    } else {
        let buf = crate::host::alloc(len, 1);
        if buf.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `buf` holds `len` fresh bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, len) };
        buf
    };
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    host_box(GosBytes { len, cap: len, ptr })
}

/// Moves `value` into memory the runtime's allocator made, laid out as the
/// `Box<T>` the runtime frees it as.
fn host_box<T>(value: T) -> *mut T {
    let p = crate::host::alloc(std::mem::size_of::<T>(), std::mem::align_of::<T>()).cast::<T>();
    if !p.is_null() {
        // SAFETY: `p` is a fresh allocation sized and aligned for `T`.
        unsafe { std::ptr::write(p, value) };
    }
    p
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
/// outer `BindingGosMap` comes from the runtime's allocator.
/// Reclamation is the consumer's responsibility - bindings MUST
/// NOT call `gos_rt_map_free` on this pointer (that helper
/// targets the runtime's incompatible `GosMap` layout); use
/// `gos_rt_binding_map_free` instead.
fn make_gos_map<K: Copy, V: Copy>(
    keys: &[K],
    values: &[V],
    key_kind: u8,
    value_kind: u8,
) -> *mut BindingGosMap {
    let keys_ptr = make_gos_vec(keys, key_kind);
    let values_ptr = make_gos_vec(values, value_kind);
    host_box(BindingGosMap {
        keys: keys_ptr,
        values: values_ptr,
    })
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
        make_gos_map(
            &keys,
            &values,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
        )
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
        make_gos_map(
            &key_ptrs,
            &val_ptrs,
            gossamer_binding_abi::vec_kind::STRING,
            gossamer_binding_abi::vec_kind::STRING,
        )
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
        make_gos_map(
            &key_ptrs,
            &values,
            gossamer_binding_abi::vec_kind::STRING,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
        )
    }
}

// --- ABI 0.4: Variant (DynValue) -------------------------------------

fn host_cstr(s: &str) -> *const c_char {
    String::from(s).to_output().cast_const()
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
            // compatibility; a `Bytes` value of its own crosses as a
            // `GosBytes` (via the Bytes BindingAbi). Here, inside a
            // DynValue payload, we
            // pack them as a nested vec.
            let widened: Vec<i64> = buf.iter().map(|b| i64::from(*b)).collect();
            let vec = make_gos_vec(&widened, gossamer_binding_abi::vec_kind::PRIMITIVE);
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
            let fields_ptr: *mut GosVariantValue = host_slice(&payload);
            let tuple = host_box(GosTuple {
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
    let fields_ptr: *mut GosVariantValue = host_slice(items);
    let tuple = host_box(GosTuple {
        len,
        fields: fields_ptr,
    });
    GosVariantValue {
        tag: 7,
        data: GosVariantPayload { tuple },
    }
}

/// Builds a `GosDynVariant` (an arm-named variant) the receiver owns.
fn make_dyn_variant(name: &str, payload: Vec<GosVariantValue>) -> *mut GosDynVariant {
    let name_ptr = host_cstr(name);
    let payload_len = i32::try_from(payload.len()).unwrap_or(0);
    let payload_ptr: *mut GosVariantValue = host_slice(&payload);
    host_box(GosDynVariant {
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
                // An i64-vec whose every element is in u8 range is
                // treated as Bytes, as `conv.rs::value_to_dyn` reads the
                // same vector from a VM call.
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

// --- Callbacks on the compiled tiers -----------------------------------
//
// A compiled caller registers each callable argument with the runtime's
// callback registry and passes the binding its handle; the handle lives
// while the call runs, and while a `PersistentCallback` holds a share.

/// Compiled-tier handle to a Gossamer-side callback, passed as a `u64`.
#[derive(Debug, Clone, Copy)]
pub struct NativeCallback {
    /// Opaque dispatch-table handle.
    pub handle: u64,
}

impl NativeCallback {
    /// Invokes the callback with `args` and returns the result.
    ///
    /// # Safety
    /// `self.handle` must be a handle the runtime still holds: one handed to
    /// the binding call running now, or one a `PersistentCallback` keeps.
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
        // SAFETY: the contract above; `args` and `result` outlive the call.
        let rc = unsafe {
            crate::host::callback_invoke(
                self.handle,
                args_ptr.cast(),
                args_len,
                (&raw mut result).cast(),
            )
        };
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

impl crate::conv::FromGos for NativeCallback {
    fn from_gos(value: &Value) -> RuntimeResult<Self> {
        match value {
            Value::Int(i) => Ok(Self { handle: *i as u64 }),
            Value::Callback(callback) => Ok(Self {
                handle: callback.handle,
            }),
            other => crate::conv::type_err("callback handle (u64)", other),
        }
    }
}

impl crate::conv::ToGos for NativeCallback {
    fn to_gos(self) -> Value {
        Value::Int(self.handle as i64)
    }
}

impl crate::sig::SigType for NativeCallback {
    const TYPE: Type = Type::Callback(&[], &Type::Any);
}

// A callable argument crosses the compiled boundary as the handle the caller
// registered it under.
impl BindingAbi for crate::conv::BindingCallback {
    type Input = u64;
    type Output = u64;
    const TYPE: Type = Type::Callback(&[], &Type::Any);

    unsafe fn from_input(input: u64) -> Self {
        Self::from_handle(input)
    }

    fn to_output(self) -> u64 {
        self.handle().unwrap_or(0)
    }
}

impl BindingAbi for crate::conv::PersistentCallback {
    type Input = u64;
    type Output = u64;
    const TYPE: Type = Type::Callback(&[], &Type::Any);

    unsafe fn from_input(input: u64) -> Self {
        Self::from_handle(input)
    }

    fn to_output(self) -> u64 {
        self.handle().unwrap_or(0)
    }
}

/// Marks the running thread as inside a wire thunk while the guard lives.
#[doc(hidden)]
#[must_use]
pub fn enter_wire_call() -> impl Drop {
    crate::host::InCall::enter()
}

/// Records the panic a wire thunk caught, for its compiled caller to raise
/// once the thunk returns.
#[doc(hidden)]
pub fn raise_wire_panic(payload: &(dyn std::any::Any + Send)) {
    crate::host::raise_panic(&crate::abi::panic_message(payload));
}

/// Wire tag for a callback that answers the unit value.
const WIRE_TAG_UNIT: i32 = 9;

/// The dispatch a `cb_fn` binding receives on the compiled tiers: callables
/// arrive as runtime handles, which it calls through the runtime's registry,
/// and a callable the VM registered runs on the VM active on the thread, the
/// one whose call entered the JIT-compiled caller.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompiledDispatch;

impl NativeDispatch for CompiledDispatch {
    fn invoke_callback(&mut self, callback: CallbackRef, args: Vec<Value>) -> RuntimeResult<Value> {
        match callback.origin {
            CallbackOrigin::Compiled => invoke_compiled_callback(callback.handle, &args),
            CallbackOrigin::Vm => {
                crate::abi::VmDispatch::for_compiled_call().invoke_callback(callback, args)
            }
        }
    }
}

/// Calls the compiled callback registered under `handle` with `args` and
/// answers its result.
///
/// A handle the runtime no longer holds reports an error rather than
/// calling anything.
pub(crate) fn invoke_compiled_callback(handle: u64, args: &[Value]) -> RuntimeResult<Value> {
    let mut strings: Vec<std::ffi::CString> = Vec::new();
    let mut wire: Vec<GosVariantValue> = Vec::with_capacity(args.len());
    for arg in args {
        let (tag, data) = match arg {
            Value::Int(n) => (0, GosVariantPayload { i64_: *n }),
            Value::Float(f) => (1, GosVariantPayload { f64_: *f }),
            Value::Bool(b) => (
                2,
                GosVariantPayload {
                    i64_: i64::from(*b),
                },
            ),
            Value::Char(c) => (
                3,
                GosVariantPayload {
                    i64_: i64::from(u32::from(*c)),
                },
            ),
            Value::String(s) => {
                let text = std::ffi::CString::new(s.as_str()).map_err(|_| {
                    RuntimeError::Type("a callback string argument holds a NUL byte".to_string())
                })?;
                let ptr = text.as_ptr().cast_mut();
                strings.push(text);
                (4, GosVariantPayload { string: ptr })
            }
            other => {
                return Err(RuntimeError::Type(format!(
                    "a compiled callback takes integers, floats, bools, chars, and strings; \
                     found {}",
                    other.kind()
                )));
            }
        };
        wire.push(GosVariantValue { tag, data });
    }
    let callback = NativeCallback { handle };
    // SAFETY: the runtime checks the handle, and the string arguments stay
    // alive in `strings` until the call returns.
    let result = unsafe { callback.invoke_raw(&wire) }.map_err(|code| {
        RuntimeError::Type(format!(
            "callback {handle} could not be called (status {code}); a compiled callback lives \
             while its binding call runs or a PersistentCallback holds it"
        ))
    })?;
    drop(strings);
    // SAFETY: the runtime wrote the member the tag names.
    unsafe {
        Ok(match result.tag {
            0 => Value::Int(result.data.i64_),
            1 => Value::Float(result.data.f64_),
            2 => Value::Bool(result.data.i64_ != 0),
            3 => Value::Char(char::from_u32(result.data.char_).unwrap_or('\u{FFFD}')),
            4 => {
                let ptr = result.data.string;
                if ptr.is_null() {
                    Value::String("".into())
                } else {
                    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                    // The callback answered an owned runtime string.
                    crate::host::str_free(ptr);
                    Value::String(text.into())
                }
            }
            WIRE_TAG_UNIT => Value::Unit,
            other => {
                return Err(RuntimeError::Type(format!(
                    "a compiled callback answered an unknown wire tag {other}"
                )));
            }
        })
    }
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
        make_gos_vec(&self, gossamer_binding_abi::vec_kind::PRIMITIVE)
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

// --- Result<T, String> impls (additions) -----------------------------

impl<E: WireError> BindingAbi for Result<String, E> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::String, &E::TYPE);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(E::from_wire(String::new())),
        };
        let s = unsafe { String::from_input(payload.data.string) };
        if tag == 1 {
            Ok(s)
        } else {
            Err(E::from_wire(s))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(s) => make_variant(1, vec![unsafe { variant_value_string_owned(s) }]),
            Err(s) => make_variant(
                0,
                vec![unsafe { variant_value_string_owned(s.into_wire()) }],
            ),
        }
    }
}

impl<E: WireError> BindingAbi for Result<bool, E> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Bool, &E::TYPE);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(E::from_wire(String::new())),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.bool_ })
        } else {
            Err(E::from_wire(unsafe {
                String::from_input(payload.data.string)
            }))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(b) => make_variant(1, vec![unsafe { variant_value_bool(b) }]),
            Err(s) => make_variant(
                0,
                vec![unsafe { variant_value_string_owned(s.into_wire()) }],
            ),
        }
    }
}

impl<E: WireError> BindingAbi for Result<f64, E> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::F64, &E::TYPE);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        let (tag, payload) = match unsafe { read_single_payload(input) } {
            Some(x) => x,
            None => return Err(E::from_wire(String::new())),
        };
        if tag == 1 {
            Ok(unsafe { payload.data.f64_ })
        } else {
            Err(E::from_wire(unsafe {
                String::from_input(payload.data.string)
            }))
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            Ok(v) => make_variant(1, vec![unsafe { variant_value_f64(v) }]),
            Err(s) => make_variant(
                0,
                vec![unsafe { variant_value_string_owned(s.into_wire()) }],
            ),
        }
    }
}

impl<E: WireError> BindingAbi for Result<(), E> {
    type Input = *const GosVariant;
    type Output = *mut GosVariant;
    const TYPE: Type = Type::Result(&Type::Unit, &E::TYPE);

    unsafe fn from_input(input: *const GosVariant) -> Self {
        // `Ok(())` carries nothing, so its tag alone says it, with or
        // without a unit field after it.
        if !input.is_null() && unsafe { (*input).tag } == 1 {
            return Ok(());
        }
        match unsafe { read_single_payload(input) } {
            Some((_, payload)) => Err(E::from_wire(unsafe {
                String::from_input(payload.data.string)
            })),
            None => Err(E::from_wire(String::new())),
        }
    }

    fn to_output(self) -> *mut GosVariant {
        match self {
            // No fields: the runtime reads an absent payload as the unit.
            Ok(()) => make_variant(1, Vec::new()),
            Err(s) => make_variant(
                0,
                vec![unsafe { variant_value_string_owned(s.into_wire()) }],
            ),
        }
    }
}

// --- HashMap impls (additions) ---------------------------------------

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
        make_gos_map(
            &keys,
            &val_ptrs,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
            gossamer_binding_abi::vec_kind::STRING,
        )
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
        make_gos_map(
            &key_ptrs,
            &values,
            gossamer_binding_abi::vec_kind::STRING,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
        )
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
        make_gos_map(
            &key_ptrs,
            &values,
            gossamer_binding_abi::vec_kind::STRING,
            gossamer_binding_abi::vec_kind::PRIMITIVE,
        )
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
    let fields_ptr: *mut GosVariantValue = host_slice(&fields);
    host_box(GosTuple {
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
/// An error type a `Result` crosses the wire with: its message, as a string.
///
/// `GosError` folds its cause chain into the message (`GosError::render`),
/// so a binding answering `Result<T, GosError>` and one answering
/// `Result<T, String>` reach the caller alike.
pub trait WireError: Sized {
    /// The error's type in a signature.
    const TYPE: Type = Type::String;
    /// The message that crosses the wire.
    fn into_wire(self) -> String;
    /// The error a message read off the wire stands for.
    fn from_wire(message: String) -> Self;
}

impl WireError for String {
    fn into_wire(self) -> String {
        self
    }

    fn from_wire(message: String) -> Self {
        message
    }
}

impl WireError for crate::error::GosError {
    fn into_wire(self) -> String {
        self.render()
    }

    fn from_wire(message: String) -> Self {
        Self::new(message)
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

// --- Composites that cross as JSON text ------------------------------

/// The shared halves of a JSON-wired impl: the text crosses as a runtime
/// string, read and written by [`crate::wire_json`].
macro_rules! json_wire_impl {
    ([$($gen:tt)*] $ty:ty) => {
        impl<$($gen)*> BindingAbi for $ty {
            type Input = *const c_char;
            type Output = *mut c_char;
            const TYPE: Type = <$ty as crate::SigType>::TYPE;
            const JSON: bool = true;

            unsafe fn from_input(input: *const c_char) -> Self {
                // SAFETY: the caller's `from_input` contract.
                let text = unsafe { String::from_input(input) };
                crate::wire_json::from_text(&text)
                    .unwrap_or_else(|e| panic!("invalid binding argument text: {e}"))
            }

            fn to_output(self) -> *mut c_char {
                crate::wire_json::to_text(&self).to_output()
            }

            fn read_wire_text(text: &str) -> Result<Self, String> {
                crate::wire_json::from_text(text)
            }

            fn write_wire_text(&self) -> String {
                crate::wire_json::to_text(self)
            }
        }
    };
}

json_wire_impl!([T: Nested + crate::SigType] Option<T>);
json_wire_impl!([T: Nested + crate::SigType] Vec<T>);
json_wire_impl!([T: Nested + crate::SigType, E: WireJson + crate::SigType] Result<T, E>);
json_wire_impl!([V: Nested + crate::SigType] std::collections::HashMap<String, V>);

#[cfg(test)]
mod tests {
    use super::*;

    fn host() {
        crate::testing::install_host();
    }

    #[test]
    fn primitive_round_trip_through_abi() {
        host();
        unsafe {
            assert_eq!(<i64 as BindingAbi>::from_input(7), 7);
            assert!(<bool as BindingAbi>::from_input(true));
            assert_eq!(<f64 as BindingAbi>::from_input(1.5), 1.5);
        }
    }

    #[test]
    fn string_round_trip() {
        host();
        let s = String::from("hello");
        let raw = s.to_output();
        let back = unsafe { String::from_input(raw) };
        assert_eq!(back, "hello");
        // The testing host leaks the string, as it frees nothing.
    }

    #[test]
    fn vec_i64_round_trip() {
        host();
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
        host();
        // The `resp.raw_bytes` shape: elem_bytes=1, one byte per
        // element. A fixed 8-byte stride would read garbage here.
        let raw = packed_vec(&[0x68, 0xFF, 0x00, 0x69], 1, 4);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(raw) };
        assert_eq!(back, vec![0x68, 0xFF, 0x00, 0x69]);
    }

    #[test]
    fn vec_i64_input_honors_half_and_word_strides() {
        host();
        let half = packed_vec(&500u16.to_le_bytes(), 2, 1);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(half) };
        assert_eq!(back, vec![500]);

        let word = packed_vec(&70000u32.to_le_bytes(), 4, 1);
        let back: Vec<i64> = unsafe { <Vec<i64> as BindingAbi>::from_input(word) };
        assert_eq!(back, vec![70000]);
    }

    #[test]
    fn vec_u8_input_truncates_word_width_slots() {
        host();
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
        host();
        let v: Vec<Vec<i64>> = vec![vec![1, 2], vec![3, 4]];
        let raw = v.to_output();
        let back: Vec<Vec<i64>> = unsafe { <Vec<Vec<i64>> as BindingAbi>::from_input(raw) };
        assert_eq!(back, vec![vec![1, 2], vec![3, 4]]);
    }

    #[test]
    fn option_round_trip() {
        host();
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
        host();
        let ok_raw = Ok::<i64, String>(7).to_output();
        let back = unsafe { <Result<i64, String> as BindingAbi>::from_input(ok_raw) };
        assert_eq!(back, Ok(7));

        let err_raw = Err::<i64, _>("nope".to_string()).to_output();
        let back = unsafe { <Result<i64, String> as BindingAbi>::from_input(err_raw) };
        assert_eq!(back, Err("nope".to_string()));
    }

    #[test]
    fn result_bytes_round_trip() {
        host();
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
        host();
        let payload: Vec<u8> = (0..=255u8).collect();
        let bytes = Bytes::new(payload.clone());
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert_eq!(back.as_slice(), payload.as_slice());
    }

    #[test]
    fn bytes_empty_round_trip() {
        host();
        let bytes = Bytes::default();
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert!(back.is_empty());
    }

    #[test]
    fn bytes_large_round_trip() {
        host();
        let payload: Vec<u8> = (0..16 * 1024).map(|i| (i % 256) as u8).collect();
        let bytes = Bytes::new(payload.clone());
        let raw = bytes.to_output();
        let back = unsafe { <Bytes as BindingAbi>::from_input(raw) };
        assert_eq!(back.len(), payload.len());
        assert_eq!(back.as_slice(), payload.as_slice());
    }

    #[test]
    fn vec_u8_round_trip() {
        host();
        let payload: Vec<u8> = vec![0, 1, 127, 200, 255];
        let raw = payload.clone().to_output();
        let back = unsafe { <Vec<u8> as BindingAbi>::from_input(raw) };
        assert_eq!(back, payload);
    }

    #[test]
    fn hash_map_i64_i64_round_trip() {
        host();
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
        host();
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
        host();
        let m: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let raw = m.to_output();
        let back = unsafe { <std::collections::HashMap<i64, i64> as BindingAbi>::from_input(raw) };
        assert!(back.is_empty());
    }

    #[test]
    fn dyn_value_nil_round_trip() {
        host();
        let raw = DynValue::Nil.to_output();
        let back = unsafe { <DynValue as BindingAbi>::from_input(raw) };
        // A bare value crosses under a `$`-headed wire name no declared arm
        // can carry, and comes back as the value rather than as an arm.
        assert!(matches!(back, DynValue::Nil), "got {back:?}");
    }

    #[test]
    fn dyn_value_tagged_round_trip() {
        host();
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
        host();
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
        host();
        let cb = NativeCallback { handle: 42 };
        let raw = cb.to_output();
        let back = unsafe { <NativeCallback as BindingAbi>::from_input(raw) };
        assert_eq!(back.handle, 42);
    }
}
