//! The C ABI between `gos` and the Rust binding libraries it loads.
//!
//! A binding library exports two symbols: [`ABI_VERSION_SYMBOL`], answering
//! the [`ABI_VERSION`] it was built against, and [`INIT_SYMBOL`], which takes
//! the host's [`HostTable`] and answers the library's [`ModuleTable`]. Every
//! type here is `repr(C)`, so the two sides may be built by different
//! compilers; nothing crosses in a Rust layout.
//!
//! Two calling conventions cross the boundary:
//!
//! - **VM calls** go through an item's [`VmThunk`]: arguments are a borrowed
//!   array of self-describing [`AbiValue`]s, and the answer is written into a
//!   [`CallOut`] whose storage the library owns until the host hands it back
//!   through [`ModuleTable::free_out`].
//! - **Compiled calls** go through an item's wire thunk, a typed `extern "C"`
//!   function compiled code calls directly. The memory it answers is
//!   allocated through the [`HostTable`], so the runtime frees it.

use std::ffi::c_void;
use std::os::raw::c_char;

/// Major and minor version of this ABI, as `major << 16 | minor`.
///
/// A host refuses a library whose major differs from its own, and one whose
/// minor is newer than its own: a minor bump only adds to the tables.
pub const ABI_VERSION: u32 = (3 << 16) | 1;

/// The word a compiled call's wire thunk answers for `Ok(())` in place of a
/// result variant's address, so a successful unit call allocates nothing. A
/// variant is pointer-aligned and never sits at this address. Minor 1 of
/// [`ABI_VERSION`] adds it.
pub const UNIT_OK_WORD: usize = 1;

/// The exported `extern "C" fn() -> u32` answering a library's [`ABI_VERSION`].
pub const ABI_VERSION_SYMBOL: &str = "gos_binding_abi_version";

/// The exported `extern "C" fn(*const HostTable) -> *const ModuleTable`.
pub const INIT_SYMBOL: &str = "gos_binding_init";

/// The major half of an [`ABI_VERSION`] value.
#[must_use]
pub const fn abi_major(version: u32) -> u32 {
    version >> 16
}

/// The minor half of an [`ABI_VERSION`] value.
#[must_use]
pub const fn abi_minor(version: u32) -> u32 {
    version & 0xffff
}

/// Whether a host speaking `host` can load a library built for `library`.
#[must_use]
pub const fn abi_compatible(host: u32, library: u32) -> bool {
    abi_major(host) == abi_major(library) && abi_minor(library) <= abi_minor(host)
}

/// A UTF-8 string borrowed across the boundary.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StrRef {
    /// First byte, or null for the empty string.
    pub ptr: *const u8,
    /// Length in bytes.
    pub len: usize,
}

impl StrRef {
    /// The empty string.
    pub const EMPTY: Self = Self {
        ptr: std::ptr::null(),
        len: 0,
    };

    /// Borrows `text` for as long as it lives.
    #[must_use]
    pub const fn new(text: &str) -> Self {
        Self {
            ptr: text.as_ptr(),
            len: text.len(),
        }
    }

    /// The bytes this names.
    ///
    /// # Safety
    /// `ptr` is null with `len == 0`, or names `len` readable bytes that stay
    /// alive and unchanged for `'a`.
    #[allow(unsafe_code)]
    #[must_use]
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.ptr.is_null() || self.len == 0 {
            return &[];
        }
        // SAFETY: the caller's contract above.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The text this names, with any invalid UTF-8 replaced.
    ///
    /// # Safety
    /// As for [`Self::as_bytes`].
    #[allow(unsafe_code)]
    #[must_use]
    pub unsafe fn to_string_lossy(self) -> String {
        // SAFETY: the caller's contract, forwarded.
        String::from_utf8_lossy(unsafe { self.as_bytes() }).into_owned()
    }
}

/// Tags of an [`AbiValue`], with what its `a` and `b` words hold.
pub mod tag {
    /// `()`; no words.
    pub const UNIT: u32 = 0;
    /// `bool`; `a` is 0 or 1.
    pub const BOOL: u32 = 1;
    /// An integer; `a` holds its 64 bits.
    pub const INT: u32 = 2;
    /// An `f64`; `a` holds its bits.
    pub const FLOAT: u32 = 3;
    /// A `char`; `a` holds its scalar value.
    pub const CHAR: u32 = 4;
    /// UTF-8 text; `a` is the first byte, `b` the length in bytes.
    pub const STRING: u32 = 5;
    /// A byte buffer; `a` is the first byte, `b` the length.
    pub const BYTES: u32 = 6;
    /// A sequence; `a` points at `b` elements.
    pub const LIST: u32 = 7;
    /// A tuple; `a` points at `b` elements.
    pub const TUPLE: u32 = 8;
    /// A map; `a` points at `2 * b` values, key then value per entry.
    pub const MAP: u32 = 9;
    /// An enum variant; `a` points at `b` values: the variant's name as a
    /// [`STRING`], then its payload.
    pub const VARIANT: u32 = 10;
    /// A struct; `a` points at `b` values: the struct's name as a
    /// [`STRING`], then a name and a value per field.
    pub const STRUCT: u32 = 11;
    /// A callable the library may call through the host while its handle
    /// is held; `a` is the handle.
    pub const CALLBACK: u32 = 12;
}

/// A self-describing value of a VM call. Aggregates point at arrays of
/// further values; see [`tag`] for what each tag's words hold.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiValue {
    /// One of the [`tag`] constants.
    pub tag: u32,
    /// Zero; room for a flag a later minor version defines.
    pub reserved: u32,
    /// First payload word.
    pub a: u64,
    /// Second payload word.
    pub b: u64,
}

impl AbiValue {
    /// The unit value.
    pub const UNIT: Self = Self::words(tag::UNIT, 0, 0);

    /// A value of `tag` with payload words `a` and `b`.
    #[must_use]
    pub const fn words(tag: u32, a: u64, b: u64) -> Self {
        Self {
            tag,
            reserved: 0,
            a,
            b,
        }
    }

    /// A value naming `len` items at `ptr`.
    #[must_use]
    pub fn pointing(tag: u32, ptr: *const u8, len: usize) -> Self {
        Self::words(tag, ptr as u64, len as u64)
    }

    /// The bytes a [`tag::STRING`] or [`tag::BYTES`] value names.
    ///
    /// # Safety
    /// `self` was built by the side whose storage it names, and that storage
    /// is alive for `'a`.
    #[allow(unsafe_code)]
    #[must_use]
    pub unsafe fn bytes<'a>(&self) -> &'a [u8] {
        let text = StrRef {
            ptr: self.a as *const u8,
            len: usize::try_from(self.b).unwrap_or(0),
        };
        // SAFETY: the caller's contract, forwarded.
        unsafe { text.as_bytes() }
    }

    /// The elements a [`tag::LIST`], [`tag::TUPLE`], [`tag::MAP`],
    /// [`tag::VARIANT`], or [`tag::STRUCT`] value points at.
    ///
    /// # Safety
    /// As for [`Self::bytes`]; for a map the slice holds `2 * b` values.
    #[allow(unsafe_code)]
    #[must_use]
    pub unsafe fn elements<'a>(&self) -> &'a [AbiValue] {
        let count = usize::try_from(self.b).unwrap_or(0);
        let count = if self.tag == tag::MAP {
            count * 2
        } else {
            count
        };
        let ptr = self.a as *const AbiValue;
        if ptr.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: the caller's contract, forwarded.
        unsafe { std::slice::from_raw_parts(ptr, count) }
    }
}

/// An answer, and the storage holding it until it is handed back.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CallOut {
    /// The answered value, or the message of a failed call.
    pub value: AbiValue,
    /// What the side that wrote `value` frees it by; null when nothing.
    pub owner: *mut c_void,
}

impl CallOut {
    /// An empty answer.
    pub const EMPTY: Self = Self {
        value: AbiValue::UNIT,
        owner: std::ptr::null_mut(),
    };
}

/// A call answered its value.
pub const STATUS_OK: u32 = 0;
/// A call failed with a typed error; the answer is its message as a
/// [`tag::STRING`].
pub const STATUS_ERROR: u32 = 1;
/// A call panicked; the answer is the panic's message as a [`tag::STRING`].
pub const STATUS_PANIC: u32 = 2;

/// The host's state for one VM call, opaque to the library.
#[repr(C)]
pub struct CallCtx {
    _private: [u8; 0],
}

/// A binding item's VM entry: reads `argc` arguments at `args`, writes the
/// answer into `out`, and answers one of the `STATUS_*` codes.
pub type VmThunk = unsafe extern "C" fn(
    ctx: *mut CallCtx,
    args: *const AbiValue,
    argc: usize,
    out: *mut CallOut,
) -> u32;

/// Calls through a VM callback handle while `ctx` is the running call; the
/// same shape as [`VmThunk`], with the handle first.
pub type VmInvoke = unsafe extern "C" fn(
    ctx: *mut CallCtx,
    handle: u64,
    args: *const AbiValue,
    argc: usize,
    out: *mut CallOut,
) -> u32;

/// The runtime functions a binding library reaches the process's one runtime
/// through. Built by the host; the library keeps the pointer for the life of
/// the process.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HostTable {
    /// The [`ABI_VERSION`] the host speaks.
    pub abi_version: u32,
    /// `size_of::<HostTable>()` on the host, so a library reads only the
    /// fields the host has.
    pub size: u32,
    /// Allocates memory the runtime frees: the buffers a wire thunk answers.
    pub alloc: unsafe extern "C" fn(size: usize, align: usize) -> *mut u8,
    /// Frees memory [`Self::alloc`] made.
    pub dealloc: unsafe extern "C" fn(ptr: *mut u8, size: usize, align: usize),
    /// Builds an empty runtime vector with room for `cap` elements of
    /// `elem_bytes` each, whose elements are of `elem_kind` (see
    /// [`vec_kind`]), so freeing it frees what its elements own.
    pub vec_with_capacity:
        unsafe extern "C" fn(elem_bytes: u32, cap: i64, elem_kind: u8) -> *mut c_void,
    /// Builds a runtime string holding `text`, which the compiled caller
    /// owns as its `String`.
    pub str_new: unsafe extern "C" fn(text: StrRef) -> *mut c_char,
    /// Frees a runtime string a compiled callback answered.
    pub str_free: unsafe extern "C" fn(s: *mut c_char),
    /// Calls a compiled-tier callback handle; answers 0 on success.
    pub callback_invoke:
        unsafe extern "C" fn(handle: u64, args: *const c_void, argc: u32, out: *mut c_void) -> i32,
    /// Keeps a compiled-tier callback handle alive past its call.
    pub callback_retain: unsafe extern "C" fn(handle: u64),
    /// Gives back a share [`Self::callback_retain`] took.
    pub callback_release: unsafe extern "C" fn(handle: u64),
    /// Records the panic of a wire thunk, which the compiled caller raises
    /// once the thunk returns.
    pub raise_panic: unsafe extern "C" fn(message: StrRef),
    /// Marks the start of a blocking native call, so the scheduler runs other
    /// goroutines meanwhile; answers a token for [`Self::blocking_leave`].
    pub blocking_enter: unsafe extern "C" fn() -> u64,
    /// Marks the end of the blocking call [`Self::blocking_enter`] began.
    pub blocking_leave: unsafe extern "C" fn(token: u64),
    /// Calls a VM callback handle; `None` in a compiled program.
    pub vm_invoke: Option<VmInvoke>,
    /// Frees the answer [`Self::vm_invoke`] wrote.
    pub vm_free_out: Option<unsafe extern "C" fn(owner: *mut c_void)>,
    /// Keeps a VM callback handle alive past its call.
    pub vm_callback_retain: Option<unsafe extern "C" fn(handle: u64)>,
    /// Gives back a share [`Self::vm_callback_retain`] took.
    pub vm_callback_release: Option<unsafe extern "C" fn(handle: u64)>,
}

/// What the elements of a runtime vector own, for
/// [`HostTable::vec_with_capacity`].
pub mod vec_kind {
    /// Plain words owning nothing.
    pub const PRIMITIVE: u8 = 0;
    /// Runtime strings.
    pub const STRING: u8 = 1;
    /// Runtime vectors.
    pub const VEC: u8 = 2;
}

/// One binding item.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ItemEntry {
    /// The item's name within its module.
    pub name: StrRef,
    /// Its documentation.
    pub doc: StrRef,
    /// Its signature as JSON: `{"params":[<type>..],"ret":<type>}`.
    pub signature: StrRef,
    /// The VM entry.
    pub vm_thunk: VmThunk,
    /// The wire thunk's symbol, empty when the item has none.
    pub wire_symbol: StrRef,
    /// The wire thunk, null when the item has none.
    pub wire_thunk: *const c_void,
    /// [`ITEM_CALLS_BACK`] when the item calls Gossamer callables back.
    pub flags: u32,
}

/// An [`ItemEntry::flags`] bit: the item takes a dispatch and calls the
/// callables it holds back through it, so its caller must be able to run
/// them.
pub const ITEM_CALLS_BACK: u32 = 1;

/// One module of a library.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ModuleEntry {
    /// The module's path as Gossamer code names it.
    pub path: StrRef,
    /// Its documentation.
    pub doc: StrRef,
    /// Its items.
    pub items: *const ItemEntry,
    /// How many items `items` points at.
    pub item_count: usize,
}

/// What a library answers from [`INIT_SYMBOL`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ModuleTable {
    /// The [`ABI_VERSION`] the library was built against.
    pub abi_version: u32,
    /// Zero; keeps the layout the same on every target.
    pub reserved: u32,
    /// The library's modules.
    pub modules: *const ModuleEntry,
    /// How many modules `modules` points at.
    pub module_count: usize,
    /// [`signature_hash`] of every module path, item name, and signature.
    pub signature_hash: u64,
    /// Frees the answer of a [`VmThunk`] by its [`CallOut::owner`].
    pub free_out: unsafe extern "C" fn(owner: *mut c_void),
}

/// The seed [`signature_hash`] starts from.
pub const SIGNATURE_HASH_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Folds `bytes` into `hash` (64-bit FNV-1a), followed by a separator byte so
/// adjacent fields cannot run together.
#[must_use]
pub fn signature_hash(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes.iter().chain(&[0xff]) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_loads_older_minors_of_its_own_major_only() {
        let host = (3 << 16) | 2;
        assert!(abi_compatible(host, 3 << 16));
        assert!(abi_compatible(host, (3 << 16) | 2));
        assert!(!abi_compatible(host, (3 << 16) | 3));
        assert!(!abi_compatible(host, 4 << 16));
        assert!(!abi_compatible(host, 2 << 16));
    }

    #[test]
    fn signature_hash_separates_adjacent_fields() {
        let ab_c = signature_hash(signature_hash(SIGNATURE_HASH_SEED, b"ab"), b"c");
        let a_bc = signature_hash(signature_hash(SIGNATURE_HASH_SEED, b"a"), b"bc");
        assert_ne!(ab_c, a_bc);
    }

    #[test]
    fn map_elements_count_both_halves_of_each_entry() {
        let pairs = [AbiValue::UNIT; 4];
        let map = AbiValue::pointing(tag::MAP, pairs.as_ptr().cast(), 2);
        #[allow(unsafe_code)]
        // SAFETY: `pairs` outlives the read.
        let elements = unsafe { map.elements() };
        assert_eq!(elements.len(), 4);
    }
}
