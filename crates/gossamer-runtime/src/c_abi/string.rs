#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(clippy::wildcard_imports)]

use std::alloc::{Layout, handle_alloc_error};
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use std::alloc::{alloc, dealloc};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use std::sync::{Mutex, OnceLock};

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
use rustc_hash::FxHashSet;

use super::*;

// ---------------------------------------------------------------
// String runtime
// ---------------------------------------------------------------
// Strings are represented as owning `CString`-shaped pointers
// allocated by Rust's `String::into_boxed_str`/`into_raw`. The
// pointer passed across the FFI is the first byte of the UTF-8
// payload; it is nul-terminated so C code can `%s`-print it. We
// track length separately by scanning for the nul byte in the C
// ABI; users that want O(1) length should use the GosStr header
// helpers (future). The ABI pointer remains the first content byte, but every
// runtime string now has an explicit carrier immediately before its legacy
// header.  The carrier is selected by the pointer's fixed low-bit shape before
// any backwards read, so public C-ABI helpers never probe a foreign C string.

/// ABI-versioned ownership carrier preceding every Gossamer string header.
/// The legacy `[rc, cap, len, tag]` suffix stays immediately before the C
/// string body, preserving all native-code offsets.
#[repr(C)]
struct StringOwner {
    abi_version: u16,
    kind: u16,
    destructor: u32,
    /// The body address this owner was written for, mixed with a salt: an
    /// untyped entry point accepts a pointer as a runtime string only when
    /// the owner it reads names that very body, so a foreign allocation that
    /// happens to sit in the heap cannot pass as one.
    check: u64,
}

const STRING_OWNER_VERSION: u16 = 1;
const STRING_OWNER_KIND: u16 = 2;
const STRING_DTOR_HEAP: u32 = 1;
const STRING_DTOR_REGION: u32 = 2;
const STRING_DTOR_STATIC: u32 = 3;
/// Heap bytes whose lifetime belongs to the region that was open when they
/// were allocated. A copy of region-backed bytes cannot be bump-allocated -
/// a recycled slab could land on its own source - so it goes to the heap,
/// but the region remains its owner and frees it at pop. Retain and release
/// therefore leave it alone, exactly as they do region-backed bytes.
const STRING_DTOR_REGION_HEAP: u32 = 4;
const STRING_OWNER_BYTES: usize = std::mem::size_of::<StringOwner>();
const STRING_LEGACY_HEADER_BYTES: usize = 13;
const STRING_BODY_OFFSET: usize = STRING_OWNER_BYTES + STRING_LEGACY_HEADER_BYTES;
const STRING_BODY_TAG: usize = STRING_BODY_OFFSET & 7;
const OWNER_CHECK_SALT: u64 = 0x5347_4F53_5452_4F57;

const _: () = assert!(STRING_OWNER_BYTES == 16);
const _: () = assert!(STRING_BODY_TAG as u64 == gossamer_abi::string_layout::BODY_ADDR_TAG);
// The back-ends read this header inline from the same constants, so a
// change to either side that the other does not follow stops the build.
const _: () =
    assert!(STRING_LEGACY_HEADER_BYTES as i64 + gossamer_abi::string_layout::CAP_OFFSET == 4);
const _: () = assert!(gossamer_abi::string_layout::LEN_OFFSET == -5);
const _: () = assert!(gossamer_abi::string_layout::TAG_OFFSET == -1);

#[inline]
fn owner_check(body: *const c_char) -> u64 {
    (body as usize as u64) ^ OWNER_CHECK_SALT
}

// Whether the sixteen bytes before `s` may be read as a string owner. A
// runtime string body lives in an allocation the global allocator handed
// out, and the owner sits at the front of that allocation, so an address
// mimalloc manages is one the read stays inside. A pointer into rodata, a
// stack, or another allocator's memory is a foreign C string, and the bytes
// before it belong to whoever placed it; they are never read.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
#[inline]
fn body_is_probeable(s: *const c_char) -> bool {
    // SAFETY: mimalloc answers from its segment map without touching `p`.
    unsafe {
        libmimalloc_sys::mi_is_in_heap_region(
            s.cast::<u8>().wrapping_sub(STRING_BODY_OFFSET).cast(),
        )
    }
}

#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
#[inline]
fn register_heap_string_body(_s: *const c_char) {}

#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
#[inline]
fn unregister_heap_string_body(_s: *const c_char) {}

// Builds whose global allocator is not mimalloc keep a registry of live heap
// bodies, so the untyped entry points can still tell a runtime string from a
// foreign pointer without reading in front of it.
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
#[inline]
fn body_is_probeable(s: *const c_char) -> bool {
    is_registered_heap_string_body(s)
}

/// Number of independent registry shards. A string body's address selects its
/// shard, so allocation and release on different goroutines contend only when
/// two live bodies hash to the same shard.
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
const STRING_REGISTRY_SHARDS: usize = 64;
#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
static HEAP_STRING_BODIES: OnceLock<Box<[Mutex<FxHashSet<usize>>]>> = OnceLock::new();

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
const _: () = assert!(STRING_REGISTRY_SHARDS.is_power_of_two());

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
fn heap_string_shard(s: *const c_char) -> &'static Mutex<FxHashSet<usize>> {
    let shards = HEAP_STRING_BODIES.get_or_init(|| {
        (0..STRING_REGISTRY_SHARDS)
            .map(|_| Mutex::new(FxHashSet::default()))
            .collect()
    });
    // Body addresses share a fixed low-bit shape, so the selector mixes the
    // allocation-varying high bits rather than reading the address directly.
    // The mix is done at a fixed 64-bit width so a 32-bit target selects
    // shards the same way instead of truncating the multiplier.
    let mixed = ((s as usize as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let index = (mixed >> (u64::BITS - STRING_REGISTRY_SHARDS.trailing_zeros())) as usize;
    &shards[index]
}

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
fn register_heap_string_body(s: *const c_char) {
    heap_string_shard(s)
        .lock()
        .expect("heap string registry poisoned")
        .insert(s as usize);
}

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
fn unregister_heap_string_body(s: *const c_char) {
    heap_string_shard(s)
        .lock()
        .expect("heap string registry poisoned")
        .remove(&(s as usize));
}

#[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
fn is_registered_heap_string_body(s: *const c_char) -> bool {
    heap_string_shard(s)
        .lock()
        .expect("heap string registry poisoned")
        .contains(&(s as usize))
}

/// Whether `s` carries the fixed low-bit shape of a Gossamer string body.
///
/// Every body sits `STRING_BODY_OFFSET` bytes into an 8-aligned allocation, so
/// its address is congruent to `STRING_BODY_TAG` modulo 8. A pointer failing
/// this test is a foreign C string, and the bytes before it belong to whoever
/// allocated it - reading them addresses memory outside the allocation, which
/// faults outright when the string begins an OS mapping.
#[inline]
fn has_body_shape(s: *const c_char) -> bool {
    (s as usize & 7) == STRING_BODY_TAG
}

#[inline]
fn str_owner(s: *const c_char) -> Option<&'static StringOwner> {
    if s.is_null() || !has_body_shape(s) {
        return None;
    }
    if !body_is_probeable(s) {
        return None;
    }
    // The read stays inside heap memory, and an owner naming this very body
    // proves the pointer was returned from `alloc_growable_with_fill`.
    // SAFETY: the probe above found `s` inside heap memory with a body's shape, so the owner
    // bytes before it are readable heap memory.
    let owner = unsafe { &*s.cast::<u8>().sub(STRING_BODY_OFFSET).cast::<StringOwner>() };
    (owner.abi_version == STRING_OWNER_VERSION
        && owner.kind == STRING_OWNER_KIND
        && owner.destructor == STRING_DTOR_HEAP
        && owner.check == owner_check(s))
    .then_some(owner)
}

#[inline]
unsafe fn typed_str_owner(s: *const c_char) -> Option<&'static StringOwner> {
    if s.is_null() || !has_body_shape(s) {
        return None;
    }
    // SAFETY: `s` is non-null (checked above), and this `unsafe fn`'s caller passes a live string
    // body.
    let owner = unsafe { &*s.cast::<u8>().sub(STRING_BODY_OFFSET).cast::<StringOwner>() };
    (owner.abi_version == STRING_OWNER_VERSION
        && owner.kind == STRING_OWNER_KIND
        && matches!(
            owner.destructor,
            STRING_DTOR_HEAP | STRING_DTOR_REGION | STRING_DTOR_REGION_HEAP | STRING_DTOR_STATIC
        ))
    .then_some(owner)
}

#[inline]
fn managed_string_owner(s: *const c_char) -> Option<&'static StringOwner> {
    str_owner(s).filter(|owner| owner.destructor == STRING_DTOR_HEAP)
}

#[inline]
unsafe fn typed_managed_string_owner(s: *const c_char) -> Option<&'static StringOwner> {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_owner` accepts.
    unsafe { typed_str_owner(s) }.filter(|owner| owner.destructor == STRING_DTOR_HEAP)
}

/// Byte length of a NUL-terminated buffer.
///
/// HOST-CSTRING: this is the `strlen` fallback that [`typed_str_len`] uses for
/// pointers with no Gossamer length header.
pub(crate) unsafe fn c_str_len(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: `s` is non-null (checked above) and, per this `unsafe fn`'s caller, a
    // NUL-terminated string.
    unsafe { CStr::from_ptr(s).to_bytes().len() }
}

/// Returns the byte length of a compiler-typed Gossamer string.
///
/// Heap builders, static literals, and region strings carry a length header;
/// reading it rather than scanning for a NUL is what lets a `String` hold
/// interior NUL bytes. The body shape selects the carrier before the header
/// read, so a foreign C string - one a runtime shim received from a host API -
/// takes the `strlen` fallback without any backwards probe. A host allocation
/// is at least 8-byte aligned, so its address never has a body's shape; a
/// foreign pointer that is not so aligned must not reach this reader.
#[inline]
unsafe fn typed_str_len(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }
    if !has_body_shape(s) {
        // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
        return unsafe { c_str_len(s) };
    }
    // SAFETY: `s` is non-null (checked above), and this `unsafe fn`'s caller passes a live string
    // body.
    let tag = unsafe { *s.cast::<u8>().sub(1) };
    if matches!(tag, STR_BUILDER_TAG | STR_STATIC_TAG | STR_REGION_TAG) {
        // SAFETY: a typed body (tag checked above) carries its length in the four bytes before
        // the tag.
        let p = unsafe { s.cast::<u8>().sub(5) };
        // SAFETY: `p` addresses those four length bytes.
        return u32::from_le_bytes(unsafe { [*p, *p.add(1), *p.add(2), *p.add(3)] }) as usize;
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
    unsafe { c_str_len(s) }
}

/// Borrows bytes from a compiler-typed Gossamer string. See
/// [`typed_str_len`] for the header contract.
#[inline]
pub(crate) unsafe fn typed_str_bytes<'a>(s: *const c_char) -> &'a [u8] {
    if s.is_null() {
        return &[];
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
    let len = unsafe { typed_str_len(s) };
    // SAFETY: `s` is a live string body whose first `len` bytes are its content.
    unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) }
}

/// Borrows the content bytes of a Gossamer `String` argument arriving over the
/// C ABI.
///
/// A Gossamer string carries an explicit length and may contain interior NUL
/// bytes, so every shim whose parameter is a language `String` reads it through
/// the length header. `CStr::from_ptr` is reserved for the few parameters that
/// are genuinely host C strings (an `environ` entry, an OS callback argument).
///
///
/// # Safety
///
/// `s` is null or points at a Gossamer string body, or at an 8-byte-aligned
/// NUL-terminated host buffer when it carries no length header. The returned slice
/// borrows `s`; the caller keeps `s` alive for the borrow.
#[inline]
pub(crate) unsafe fn gos_str_arg_bytes<'a>(s: *const c_char) -> &'a [u8] {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes` accepts.
    unsafe { typed_str_bytes(s) }
}

/// Borrows a Gossamer `String` argument as UTF-8 text, yielding the empty
/// string when the bytes are not valid UTF-8.
///
///
/// # Safety
///
/// See [`gos_str_arg_bytes`].
#[inline]
pub(crate) unsafe fn gos_str_arg_text<'a>(s: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_text` accepts.
    unsafe { typed_str_text(s) }
}

/// Borrows a Gossamer `String` argument as UTF-8 text, replacing invalid
/// sequences with `U+FFFD`.
///
///
/// # Safety
///
/// See [`gos_str_arg_bytes`].
#[inline]
pub(crate) unsafe fn gos_str_arg_lossy<'a>(s: *const c_char) -> std::borrow::Cow<'a, str> {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `gos_str_arg_bytes`
    // accepts.
    String::from_utf8_lossy(unsafe { gos_str_arg_bytes(s) })
}

/// Copies a Gossamer `String` argument into an owned `String`, replacing
/// invalid sequences with `U+FFFD`.
///
///
/// # Safety
///
/// See [`gos_str_arg_bytes`].
#[inline]
pub(crate) unsafe fn gos_str_arg_string(s: *const c_char) -> String {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `gos_str_arg_lossy`
    // accepts.
    unsafe { gos_str_arg_lossy(s) }.into_owned()
}

/// Byte length of a Gossamer `String` argument arriving over the C ABI.
///
///
/// # Safety
///
/// See [`gos_str_arg_bytes`].
#[inline]
pub(crate) unsafe fn gos_str_arg_len(s: *const c_char) -> usize {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_len` accepts.
    unsafe { typed_str_len(s) }
}

#[inline]
pub(crate) unsafe fn typed_str_text<'a>(s: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes` accepts.
    let bytes = unsafe { typed_str_bytes(s) };
    // SAFETY: this `unsafe fn`'s caller passes `s` null or live, which `typed_str_known_utf8`
    // accepts.
    if unsafe { typed_str_known_utf8(s) } {
        // SAFETY: an index footer other than `u32::MAX` is written only for
        // content that was validated, or built from validated pieces, as
        // UTF-8 (see `rebuild_str_index` and `extend_str_index`).
        return unsafe { std::str::from_utf8_unchecked(bytes) };
    }
    std::str::from_utf8(bytes).unwrap_or("")
}

/// Whether `s` carries a character index, which is written only for content
/// known to be UTF-8.
///
///
/// # Safety
///
/// `s` is null or a Gossamer string body.
#[inline]
unsafe fn typed_str_known_utf8(s: *const c_char) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_cap` accepts.
    let Some(cap) = (unsafe { typed_str_cap(s) }) else {
        return false;
    };
    // SAFETY: the string's capacity is `cap` (read above), so its index footer follows the `cap +
    // 1` content bytes inside the allocation.
    let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
    // SAFETY: `footer` addresses the footer's first word.
    (unsafe { footer.read_unaligned() }) != u32::MAX
}

/// Number of bytes in the UTF-8 scalar a leading byte begins.
///
/// A malformed leading byte answers one, so a walk over invalid content still
/// advances and terminates.
const fn utf8_encoded_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Decodes the UTF-8 scalar starting at `at`, reading no more than the four
/// bytes one scalar can occupy.
///
/// Validating a whole buffer to read one character makes any scan of it
/// quadratic, so the window is bounded by the longest encoding rather than by
/// the content's length. `at` is a character boundary, so the scalar it starts
/// is complete inside the window even when the window's own tail is not.
fn utf8_scalar_at(bytes: &[u8], at: usize) -> Option<char> {
    let end = at.saturating_add(4).min(bytes.len());
    let window = bytes.get(at..end)?;
    match std::str::from_utf8(window) {
        Ok(text) => text.chars().next(),
        Err(err) => std::str::from_utf8(window.get(..err.valid_up_to())?)
            .ok()?
            .chars()
            .next(),
    }
}

/// Byte offset of the next character boundary at or after `at`.
///
/// Reads only the byte at each candidate offset: a UTF-8 continuation byte is
/// `10xxxxxx`, and every other byte starts a scalar.
fn utf8_boundary_at_or_after(bytes: &[u8], mut at: usize) -> Option<usize> {
    if at > bytes.len() {
        return None;
    }
    while at < bytes.len() && (bytes[at] & 0xC0) == 0x80 {
        at += 1;
    }
    Some(at)
}

/// Byte offset of character `index`, walking scalars from `from_byte`.
///
/// Each step reads one leading byte, so a caller that starts from a nearby
/// index block pays that block's stride rather than the content's length.
fn utf8_offset_of_char(bytes: &[u8], from_byte: usize, steps: usize) -> Option<usize> {
    let mut at = from_byte;
    for _ in 0..steps {
        if at >= bytes.len() {
            return None;
        }
        at += utf8_encoded_len(bytes[at]);
    }
    (at <= bytes.len()).then_some(at)
}

#[inline]
unsafe fn typed_str_char_len(s: *const c_char) -> usize {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_cap` accepts.
    if let Some(cap) = unsafe { typed_str_cap(s) } {
        // SAFETY: the string's capacity is `cap` (read above), so its index footer follows the
        // `cap + 1` content bytes inside the allocation.
        let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
        // SAFETY: `footer` addresses the footer's first word.
        let char_len = unsafe { footer.read_unaligned() };
        if char_len == STR_INDEX_ASCII {
            // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes`
            // accepts.
            return unsafe { typed_str_bytes(s) }.len();
        }
        return if char_len == u32::MAX {
            0
        } else {
            char_len as usize
        };
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_text` accepts.
    unsafe { typed_str_text(s) }.chars().count()
}

#[inline]
unsafe fn typed_str_char_boundary(s: *const c_char, index: usize) -> Option<usize> {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_cap` accepts.
    if let Some(cap) = unsafe { typed_str_cap(s) } {
        // SAFETY: the string's capacity is `cap` (read above), so its index footer follows the
        // `cap + 1` content bytes inside the allocation.
        let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
        // SAFETY: `footer` addresses the footer's first word.
        let raw_char_len = unsafe { footer.read_unaligned() };
        if raw_char_len == STR_INDEX_ASCII {
            // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes`
            // accepts.
            let len = unsafe { typed_str_bytes(s) }.len();
            return (index <= len).then_some(index);
        }
        let char_len = raw_char_len as usize;
        if char_len == u32::MAX as usize {
            return None;
        }
        // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes`
        // accepts.
        let bytes = unsafe { typed_str_bytes(s) };
        if index > char_len {
            return None;
        }
        if index == char_len {
            return Some(bytes.len());
        }
        let block = index / STR_INDEX_STRIDE;
        let block_char = block * STR_INDEX_STRIDE;
        // SAFETY: a UTF-8 index holds one word per stride block after its first word, and `block`
        // is below the block count for `index`.
        let byte = unsafe { footer.add(1 + block).read_unaligned() } as usize;
        return utf8_offset_of_char(bytes, byte, index - block_char);
    }
    // No index to start from, so the walk is the content's own. A string
    // reaching here is a foreign C pointer, which no Gossamer value names.
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes` accepts.
    let bytes = unsafe { typed_str_bytes(s) };
    if index == 0 {
        return Some(0);
    }
    utf8_offset_of_char(bytes, 0, index)
}

#[inline]
unsafe fn typed_str_next_char_boundary(s: *const c_char, index: usize) -> Option<usize> {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_bytes` accepts.
    utf8_boundary_at_or_after(unsafe { typed_str_bytes(s) }, index)
}

/// Tests the private builder tag on a compiler-typed string.
///
///
/// # Safety
///
/// `s` comes from a slot the compiler typed as `String`, so it carries
/// the owner prefix every runtime string allocator writes. Region- and
/// static-backed strings fail the heap-destructor filter and route to the
/// copying path, exactly as the registry-backed probe does.
#[inline]
unsafe fn is_typed_builder(s: *const c_char) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which
    // `typed_managed_string_owner` accepts.
    unsafe { typed_managed_string_owner(s) }.is_some()
        && unsafe { *s.cast::<u8>().sub(1) == STR_BUILDER_TAG }
}

/// Tag for growable strings allocated by `alloc_growable`.
/// Layout: `[cap:u32 LE][len:u32 LE][tag=0xAB][content(cap bytes)][NUL]`
/// `ptr` is 9 bytes past the start of the allocation (at `content[0]`).
/// `ptr[-1]` = tag, `ptr[-5..-1]` = len (u32 LE), `ptr[-9..-5]` = cap (u32 LE).
/// Total allocation: cap + 10 bytes.
const STR_BUILDER_TAG: u8 = gossamer_abi::string_layout::TAG_BUILDER;

/// High bit of a `STR_BUILDER` string's `rc:u32` field, set once the string
/// has escaped to another goroutine (`gos_rt_rc_mark_shared`). When set,
/// `gos_rt_str_retain` / `gos_rt_str_free` adjust the count with atomic
/// read-modify-write instead of the non-atomic fast path, so concurrent
/// clone/drop across goroutines cannot tear the count (the same biased-RC
/// protocol `RcHeader` objects use via their `SHARED_BIT`). The live count is
/// the low 31 bits; a string never reaches 2^31 references, so the bit never
/// collides with a real count.
pub(crate) const STR_SHARED: u32 = 1 << 31;

/// Tag for static string literals emitted into compiler-owned rodata.
/// `is_gos_string` uses this only on values already known by typed runtime RC
/// metadata to be Gossamer values; public raw-string entry points never probe
/// this prefix.
const STR_STATIC_TAG: u8 = gossamer_abi::string_layout::TAG_STATIC;

/// Tag for growable strings whose backing bytes live in an arena region.
/// Same `[cap][len][tag][content][NUL]` layout as `STR_BUILDER_TAG` (so
/// length reads and in-place append work identically), but the bytes are
/// freed wholesale at `arena_pop`, so `gos_rt_str_free` skips them.
const STR_REGION_TAG: u8 = gossamer_abi::string_layout::TAG_REGION;
const STR_INDEX_STRIDE: usize = gossamer_abi::string_layout::INDEX_STRIDE;
/// Character-count sentinel meaning "every byte is one character", i.e. the
/// content is ASCII. A character index then equals its byte offset, so the
/// per-block offsets are the identity and are neither written nor read. This
/// keeps the common case off the O(len) `char_indices` walk that building the
/// index otherwise costs on every allocation and every append.
pub(crate) const STR_INDEX_ASCII: u32 = gossamer_abi::string_layout::INDEX_ASCII;

#[inline]
const fn str_index_slots(cap: usize) -> usize {
    cap / STR_INDEX_STRIDE + 2
}

#[inline]
const fn str_index_bytes(cap: usize) -> usize {
    str_index_slots(cap) * std::mem::size_of::<u32>()
}

unsafe fn rebuild_str_index(s: *mut c_char, len: usize, cap: usize) {
    // SAFETY: this `unsafe fn`'s caller passes `s` a string body of `len` content bytes and
    // capacity `cap`.
    let bytes = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) };
    // `is_ascii` is a vectorised scan, where validation walks sequences.
    if !bytes.is_ascii() && std::str::from_utf8(bytes).is_err() {
        // SAFETY: the index footer follows the `cap + 1` content bytes inside the allocation.
        let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
        // SAFETY: `footer` addresses the footer's first word.
        unsafe { footer.write_unaligned(u32::MAX) };
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `s` a string body of `len` content bytes and
    // capacity `cap`.
    unsafe { index_utf8_content(s, len, cap) };
}

/// Writes the character index of content known to be UTF-8.
///
/// Every character starts at a byte that is not a continuation byte, so the
/// index reads leading bytes rather than decoding each scalar.
///
///
/// # Safety
///
/// `s` is a string body with `cap` bytes of content capacity whose
/// first `len` bytes are UTF-8.
unsafe fn index_utf8_content(s: *mut c_char, len: usize, cap: usize) {
    // SAFETY: this `unsafe fn`'s caller passes `s` a string body of `len` content bytes and
    // capacity `cap`.
    let bytes = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) };
    // SAFETY: the index footer follows the `cap + 1` content bytes inside the allocation.
    let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
    if bytes.is_ascii() {
        // SAFETY: `footer` addresses the footer's first word.
        unsafe { footer.write_unaligned(STR_INDEX_ASCII) };
        return;
    }
    let mut chars = 0usize;
    let mut offset = 0usize;
    let words = bytes.chunks_exact(8);
    let rest_at = len - words.remainder().len();
    for word in words {
        let leads = 8 - utf8_continuation_bytes(word);
        // A word that reaches no block boundary only adds to the count; the
        // one that does is walked byte by byte for the boundary's offset.
        if chars % STR_INDEX_STRIDE + leads < STR_INDEX_STRIDE
            && !chars.is_multiple_of(STR_INDEX_STRIDE)
        {
            chars += leads;
        } else {
            for (at, &byte) in word.iter().enumerate() {
                // SAFETY: `footer` addresses an index sized for the string's capacity, which
                // covers every char start.
                chars = unsafe { index_char_start(footer, chars, offset + at, byte) };
            }
        }
        offset += 8;
    }
    for (at, &byte) in bytes[rest_at..].iter().enumerate() {
        // SAFETY: `footer` addresses an index sized for the string's capacity, which covers every
        // char start.
        chars = unsafe { index_char_start(footer, chars, rest_at + at, byte) };
    }
    // SAFETY: `footer` addresses the footer's first word.
    unsafe { footer.write_unaligned(chars as u32) };
}

/// How many of the eight bytes in `word` continue a character: those whose
/// top two bits are `10`.
#[inline]
fn utf8_continuation_bytes(word: &[u8]) -> usize {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(word);
    let x = u64::from_le_bytes(raw);
    // Bit 7 set and bit 6 clear, per byte: shifting left by one moves each
    // byte's bit 6 into its bit 7 position.
    ((x & !(x << 1)) & 0x8080_8080_8080_8080).count_ones() as usize
}

/// Counts `byte`, at `offset`, toward the character index: a character that
/// starts a block records the block's byte offset. Answers the new count.
///
///
/// # Safety
///
/// `footer` is the index footer of a string with room for the entry
/// of the block `chars` falls in.
#[inline]
unsafe fn index_char_start(footer: *mut u32, chars: usize, offset: usize, byte: u8) -> usize {
    if byte & 0xC0 == 0x80 {
        return chars;
    }
    if chars.is_multiple_of(STR_INDEX_STRIDE) {
        // SAFETY: this `unsafe fn`'s caller passes `footer` an index sized for the string's
        // capacity, whose block for `chars` exists.
        unsafe {
            footer
                .add(1 + chars / STR_INDEX_STRIDE)
                .write_unaligned(offset as u32);
        }
    }
    chars + 1
}

/// Extends the footer character index after an in-place append. The previous
/// implementation rebuilt it from byte zero on every append, making otherwise
/// amortized string builders quadratic.
#[inline]
unsafe fn extend_str_index(s: *mut c_char, old_len: usize, added: &[u8], cap: usize) {
    // SAFETY: this `unsafe fn`'s caller passes `s` a body of capacity `cap`, whose index footer
    // follows its content.
    let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
    // SAFETY: `footer` addresses the footer's first word.
    let old_chars = unsafe { footer.read_unaligned() };
    if old_chars == u32::MAX {
        // SAFETY: this `unsafe fn`'s caller passes `s` a body of capacity `cap` holding `old_len
        // + added.len()` content bytes.
        unsafe { rebuild_str_index(s, old_len + added.len(), cap) };
        return;
    }
    if old_chars == STR_INDEX_ASCII {
        // Appending ASCII to ASCII keeps the identity index; anything else
        // needs real offsets for the whole content.
        if added.is_ascii() {
            return;
        }
        // SAFETY: this `unsafe fn`'s caller passes `s` a body of capacity `cap` holding `old_len
        // + added.len()` content bytes.
        unsafe { rebuild_str_index(s, old_len + added.len(), cap) };
        return;
    }
    let Ok(text) = std::str::from_utf8(added) else {
        // SAFETY: `footer` addresses the footer's first word.
        unsafe { footer.write_unaligned(u32::MAX) };
        return;
    };
    let mut added_chars = 0usize;
    for (byte_offset, _) in text.char_indices() {
        let char_index = old_chars as usize + added_chars;
        if char_index.is_multiple_of(STR_INDEX_STRIDE) {
            // SAFETY: the index is sized for the capacity, so the block for `char_index` exists.
            unsafe {
                footer
                    .add(1 + char_index / STR_INDEX_STRIDE)
                    .write_unaligned((old_len + byte_offset) as u32);
            }
        }
        added_chars += 1;
    }
    // SAFETY: `footer` addresses the footer's first word.
    unsafe { footer.write_unaligned(old_chars.saturating_add(added_chars as u32)) };
}

#[inline]
unsafe fn typed_str_cap(s: *const c_char) -> Option<usize> {
    if s.is_null() || !has_body_shape(s) {
        return None;
    }
    // SAFETY: `s` is non-null (checked above), and this `unsafe fn`'s caller passes a live string
    // body.
    let tag = unsafe { *s.cast::<u8>().sub(1) };
    if !matches!(tag, STR_BUILDER_TAG | STR_STATIC_TAG | STR_REGION_TAG) {
        return None;
    }
    // SAFETY: a typed body (tag checked above) carries its capacity nine bytes before the body.
    let p = unsafe { s.cast::<u8>().sub(9) };
    // SAFETY: `p` addresses those four capacity bytes.
    Some(u32::from_le_bytes(unsafe { [*p, *p.add(1), *p.add(2), *p.add(3)] }) as usize)
}

/// Whether `s` carries a character index that records its content as ASCII.
///
///
/// # Safety
///
/// `s` is null or a Gossamer string body.
#[inline]
unsafe fn typed_str_is_ascii(s: *const c_char) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_cap` accepts.
    let Some(cap) = (unsafe { typed_str_cap(s) }) else {
        return false;
    };
    // SAFETY: the string's capacity is `cap` (read above), so its index footer follows the `cap +
    // 1` content bytes inside the allocation.
    let footer = unsafe { s.cast::<u8>().add(cap + 1).cast::<u32>() };
    // SAFETY: `footer` addresses the footer's first word.
    (unsafe { footer.read_unaligned() }) == STR_INDEX_ASCII
}

#[inline]
fn is_managed_string(s: *const c_char) -> bool {
    // SAFETY: a managed string (checked first) is a typed body whose tag byte precedes it.
    managed_string_owner(s).is_some() && unsafe { *s.cast::<u8>().sub(1) == STR_BUILDER_TAG }
}

/// Copies `n` non-overlapping bytes from `src` to `dst`, keeping short copies
/// inline with overlapping fixed-width loads/stores instead of calling the
/// platform `memcpy`. The static-musl release link resolves `memcpy` to musl's
/// scalar routine, whose per-call overhead dominates the short copies that k-mer
/// keys and small-string content produce (glibc hides this with a SIMD ifunc;
/// musl does not). Large copies fall through to `memcpy`, where throughput
/// dominates and the call is amortised. This mirrors how the Go runtime's
/// `memmove` and optimised libc `memcpy`s special-case small sizes.
///
///
/// # Safety
///
/// `src` is readable and `dst` writable for `n` bytes, and the two
/// ranges do not overlap.
#[inline]
pub(crate) unsafe fn copy_small_bytes(src: *const u8, dst: *mut u8, n: usize) {
    // SAFETY: this `unsafe fn`'s caller passes `src` and `dst` each addressing `n` bytes, not
    // overlapping.
    unsafe {
        if n >= 32 {
            std::ptr::copy_nonoverlapping(src, dst, n);
        } else if n >= 16 {
            let a0 = (src as *const u64).read_unaligned();
            let a1 = (src.add(8) as *const u64).read_unaligned();
            let b0 = (src.add(n - 16) as *const u64).read_unaligned();
            let b1 = (src.add(n - 8) as *const u64).read_unaligned();
            (dst as *mut u64).write_unaligned(a0);
            (dst.add(8) as *mut u64).write_unaligned(a1);
            (dst.add(n - 16) as *mut u64).write_unaligned(b0);
            (dst.add(n - 8) as *mut u64).write_unaligned(b1);
        } else if n >= 8 {
            let a = (src as *const u64).read_unaligned();
            let b = (src.add(n - 8) as *const u64).read_unaligned();
            (dst as *mut u64).write_unaligned(a);
            (dst.add(n - 8) as *mut u64).write_unaligned(b);
        } else if n >= 4 {
            let a = (src as *const u32).read_unaligned();
            let b = (src.add(n - 4) as *const u32).read_unaligned();
            (dst as *mut u32).write_unaligned(a);
            (dst.add(n - 4) as *mut u32).write_unaligned(b);
        } else if n >= 2 {
            let a = (src as *const u16).read_unaligned();
            let b = (src.add(n - 2) as *const u16).read_unaligned();
            (dst as *mut u16).write_unaligned(a);
            (dst.add(n - 2) as *mut u16).write_unaligned(b);
        } else if n == 1 {
            *dst = *src;
        }
    }
}

/// Compares two byte slices without a libc call for short inputs.
///
/// The static-musl release link resolves `memcmp` to musl's scalar byte loop,
/// and a hash table compares its candidate key on every probe that hits, so a
/// map of short keys pays that loop per lookup (glibc hides the same cost
/// behind a SIMD ifunc; musl does not). Short slices are compared here as a
/// few overlapping fixed-width loads, the same shape [`copy_small_bytes`]
/// uses for the copy side, and longer ones walk 32-byte blocks of the same.
/// The standard comparison is what a growing dictionary key reached, at about
/// six hundred instructions per compare.
#[inline]
pub(crate) fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    if n >= 32 {
        // SAFETY: every read is inside `0..n`, which both slices hold: the
        // loop stops with 32 bytes still ahead of `i`, and the final block
        // starts at `n - 32`, overlapping the one before it where the length
        // is not a whole number of blocks.
        unsafe {
            let mut i = 0usize;
            while i + 32 <= n {
                if load64(pa.add(i)) != load64(pb.add(i))
                    || load64(pa.add(i + 8)) != load64(pb.add(i + 8))
                    || load64(pa.add(i + 16)) != load64(pb.add(i + 16))
                    || load64(pa.add(i + 24)) != load64(pb.add(i + 24))
                {
                    return false;
                }
                i += 32;
            }
            let tail = n - 32;
            return load64(pa.add(tail)) == load64(pb.add(tail))
                && load64(pa.add(tail + 8)) == load64(pb.add(tail + 8))
                && load64(pa.add(tail + 16)) == load64(pb.add(tail + 16))
                && load64(pa.add(tail + 24)) == load64(pb.add(tail + 24));
        }
    }
    // SAFETY: every read below is bounded by `n`, which both slices hold, and
    // the trailing reads start at `n - width` so they stay inside the same
    // range. Unaligned reads are explicit.
    unsafe {
        if n >= 16 {
            load64(pa) == load64(pb)
                && load64(pa.add(8)) == load64(pb.add(8))
                && load64(pa.add(n - 16)) == load64(pb.add(n - 16))
                && load64(pa.add(n - 8)) == load64(pb.add(n - 8))
        } else if n >= 8 {
            load64(pa) == load64(pb) && load64(pa.add(n - 8)) == load64(pb.add(n - 8))
        } else if n >= 4 {
            load32(pa) == load32(pb) && load32(pa.add(n - 4)) == load32(pb.add(n - 4))
        } else if n >= 2 {
            load16(pa) == load16(pb) && load16(pa.add(n - 2)) == load16(pb.add(n - 2))
        } else if n == 1 {
            *pa == *pb
        } else {
            true
        }
    }
}

/// # Safety
///
/// `p` is readable for 8 bytes.
#[inline]
unsafe fn load64(p: *const u8) -> u64 {
    // SAFETY: this `unsafe fn`'s caller passes `p` addressing eight readable bytes.
    unsafe { p.cast::<u64>().read_unaligned() }
}

/// # Safety
///
/// `p` is readable for 4 bytes.
#[inline]
unsafe fn load32(p: *const u8) -> u32 {
    // SAFETY: this `unsafe fn`'s caller passes `p` addressing four readable bytes.
    unsafe { p.cast::<u32>().read_unaligned() }
}

/// # Safety
///
/// `p` is readable for 2 bytes.
#[inline]
unsafe fn load16(p: *const u8) -> u16 {
    // SAFETY: this `unsafe fn`'s caller passes `p` addressing two readable bytes.
    unsafe { p.cast::<u16>().read_unaligned() }
}

/// Copies a string part into a newly allocated builder, tolerating allocator
/// address reuse that places the destination over stale source storage.
#[inline]
unsafe fn copy_builder_part(src: *const u8, dst: *mut u8, n: usize) {
    let src_addr = src as usize;
    let dst_addr = dst as usize;
    let overlaps =
        n != 0 && src_addr < dst_addr.saturating_add(n) && dst_addr < src_addr.saturating_add(n);
    if overlaps {
        // SAFETY: the caller provides readable/writable ranges of `n` bytes;
        // `copy` explicitly permits overlap (memmove semantics).
        unsafe { std::ptr::copy(src, dst, n) };
    } else {
        // SAFETY: the range check above proves the caller's valid ranges do
        // not overlap, satisfying `copy_small_bytes`' stronger contract.
        unsafe { copy_small_bytes(src, dst, n) };
    }
}

/// Allocates an owned `Box<[u8]>` holding `src`'s bytes via the inline
/// small-copy path, so short keys avoid a libc `memcpy` call (see
/// [`copy_small_bytes`]). Used by the string-keyed map insert paths, where a
/// k-mer key is copied into the map's own storage on a miss.
#[inline]
pub(crate) fn boxed_bytes(src: &[u8]) -> Box<[u8]> {
    let mut b: Box<[std::mem::MaybeUninit<u8>]> = Box::new_uninit_slice(src.len());
    // SAFETY: `b` has `src.len()` writable bytes, all written by the copy below;
    // `src` and the fresh `b` are distinct allocations, so they do not overlap.
    unsafe {
        copy_small_bytes(src.as_ptr(), b.as_mut_ptr().cast::<u8>(), src.len());
        b.assume_init()
    }
}

/// Allocates a growable string with `cap` bytes of content capacity.
/// `parts` are concatenated into the initial content (total must be <= cap).
/// Returns a pointer to `content[0]`; the 9-byte header lives just before it.
fn alloc_growable(parts: &[&[u8]], cap: usize) -> *mut c_char {
    alloc_growable_forced(parts, cap, false)
}

/// Storage for a string body.
///
/// The Rust global-allocator facade routes a request through mimalloc's
/// aligned entry, which pads it by 8 to 16 bytes and takes the aligned free
/// path on the way back. Plain `mi_malloc` returns the bin the size asks for
/// and guarantees 16-byte alignment, which covers the 8 this layout wants.
/// The sanitizer and wasm builds keep the facade, where the global allocator
/// is the system one and mixing would free across allocators.
#[inline]
fn string_body_alloc(layout: Layout) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // SAFETY: `mi_malloc` accepts any size and answers null on failure.
        unsafe { libmimalloc_sys::mi_malloc(layout.size()).cast() }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        // SAFETY: `layout` has a non-zero size (a string's header and terminator at least).
        unsafe { alloc(layout) }
    }
}

/// Companion to [`string_body_alloc`].
#[inline]
unsafe fn string_body_free(base: *mut u8, layout: Layout) {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        let _ = layout;
        // SAFETY: this `unsafe fn`'s caller passes `base` a string allocation that nothing uses
        // afterwards.
        unsafe { libmimalloc_sys::mi_free(base.cast()) };
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        // SAFETY: this `unsafe fn`'s caller passes `base` as a block allocated with `layout`, not
        // freed before.
        unsafe { dealloc(base, layout) };
    }
}

/// Allocates a growable string, promoting it to the heap when `force_heap` is
/// set or any non-empty input slice points into region storage.
fn alloc_growable_forced(parts: &[&[u8]], cap: usize, force_heap: bool) -> *mut c_char {
    let content_len: usize = parts.iter().map(|p| p.len()).sum();
    // A region-backed source can escape the compiler-generated arena scope
    // that created it. If the destination were allocated in the next active
    // region, slab recycling could place it over its own source bytes. Promote
    // copies of region storage to the heap before allocating the destination.
    let force_heap = force_heap
        || parts
            .iter()
            .any(|part| crate::c_abi::rc::in_region_arena(part.as_ptr()));
    alloc_growable_with_fill(content_len, cap, force_heap, |out| {
        let mut off = 0;
        for p in parts {
            // SAFETY: `alloc_growable_with_fill` passes `cap` writable content
            // bytes and `cap >= content_len`; this loop writes each input part
            // exactly once into the first `content_len` bytes.
            unsafe {
                copy_builder_part(p.as_ptr(), out.add(off), p.len());
            }
            off += p.len();
        }
    })
}

/// What an allocation already knows about the content it is filled with,
/// which decides how much of it the character index has to read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KnownText {
    /// Every byte is ASCII.
    Ascii,
    /// The bytes are UTF-8.
    Utf8,
    /// Nothing is known; the bytes are validated.
    Unchecked,
}

/// Allocates a growable runtime string and lets `fill` initialise exactly the
/// first `content_len` bytes of the content region.
pub(crate) fn alloc_growable_with_fill<F>(
    content_len: usize,
    cap: usize,
    force_heap: bool,
    fill: F,
) -> *mut c_char
where
    F: FnOnce(*mut u8),
{
    alloc_growable_filled(content_len, cap, force_heap, KnownText::Unchecked, fill)
}

/// Copies `bytes`, which the caller has proven ASCII, into a fresh string.
/// Knowing the content is ASCII lets the character index be written as the
/// identity instead of rescanning the bytes just copied.
pub(crate) fn alloc_ascii_cstring(bytes: &[u8]) -> *mut c_char {
    debug_assert!(
        bytes.is_ascii(),
        "alloc_ascii_cstring: content is not ASCII"
    );
    let force_heap = crate::c_abi::rc::in_region_arena(bytes.as_ptr());
    alloc_growable_filled(
        bytes.len(),
        bytes.len(),
        force_heap,
        KnownText::Ascii,
        |out| unsafe {
            // SAFETY: the allocation passes `bytes.len()` writable content bytes.
            copy_builder_part(bytes.as_ptr(), out, bytes.len());
        },
    )
}

/// Copies `bytes`, a slice of the string `source`, into a fresh string. A slice
/// of ASCII text is ASCII, so when `source`'s index says it is the copy takes
/// that index instead of scanning the bytes it was just written.
///
/// # Safety
///
/// `source` is null or a Gossamer string body, and `bytes` holds text copied
/// from within its content.
#[inline]
pub(crate) unsafe fn alloc_slice_cstring(source: *const c_char, bytes: &[u8]) -> *mut c_char {
    // SAFETY: this `unsafe fn`'s caller passes `source` null or live, which the probe accepts.
    if unsafe { typed_str_is_ascii(source) } {
        return alloc_ascii_cstring(bytes);
    }
    // SAFETY: this `unsafe fn`'s caller passes `source` null or live, which the probe accepts.
    if unsafe { typed_str_known_utf8(source) } && utf8_slice_is_whole(bytes) {
        let force_heap = crate::c_abi::rc::in_region_arena(bytes.as_ptr());
        return alloc_growable_filled(
            bytes.len(),
            bytes.len(),
            force_heap,
            KnownText::Utf8,
            |out| unsafe {
                // SAFETY: the allocation passes `bytes.len()` writable content
                // bytes.
                copy_builder_part(bytes.as_ptr(), out, bytes.len());
            },
        );
    }
    alloc_cstring(bytes)
}

/// Whether a run of bytes cut out of UTF-8 text is itself UTF-8: it starts
/// on a character and its last character is complete. Everything between
/// was already UTF-8, so the two ends are all the cut can have broken.
fn utf8_slice_is_whole(bytes: &[u8]) -> bool {
    let Some(&first) = bytes.first() else {
        return true;
    };
    if first & 0xC0 == 0x80 {
        return false;
    }
    let tail = bytes.len().saturating_sub(4);
    let Some(lead) = (tail..bytes.len())
        .rev()
        .find(|&at| bytes[at] & 0xC0 != 0x80)
    else {
        return false;
    };
    lead + utf8_encoded_len(bytes[lead]) == bytes.len()
}

/// [`alloc_growable_with_fill`], told what is known about the filled content.
fn alloc_growable_filled<F>(
    content_len: usize,
    cap: usize,
    force_heap: bool,
    known: KnownText,
    fill: F,
) -> *mut c_char
where
    F: FnOnce(*mut u8),
{
    debug_assert!(
        cap >= content_len,
        "alloc_growable_with_fill: cap < content length"
    );
    // The builder header stores length and capacity as `u32` (offsets
    // `ptr[-5]` / `ptr[-9]`). A value past `u32::MAX` cannot be represented,
    // so refuse it here rather than truncate and later index the buffer with a
    // wrapped length. A single string this large is not a real workload; treat
    // it like the allocation failure it effectively is (aborting, matching the
    // OOM discipline of the `Box::new_uninit_slice` path below) instead of
    // corrupting the heap. This is on the string-append hot path, so the check
    // is two comparisons and no allocation.
    if cap > u32::MAX as usize || content_len > u32::MAX as usize {
        eprintln!(
            "gossamer: string length {content_len} exceeds the 4 GiB builder-header limit; aborting"
        );
        std::process::abort();
    }
    // owner(16) + rc(4) + cap(4) + len(4) + tag(1) + content(cap) + NUL(1).
    // Refcount at the FRONT keeps cap(-9)/len(-5)/tag(-1) offsets unchanged.
    let total = STRING_BODY_OFFSET + cap + 1 + str_index_bytes(cap);
    crate::c_abi::ledger::benchmark_allocation(total);
    // Inside an arena region, allocate fresh builders from the region. A copy
    // whose source is already region-backed must be promoted to the heap so a
    // recycled slab cannot place the destination over its own source bytes.
    let region_base = if force_heap {
        std::ptr::null_mut()
    } else {
        crate::c_abi::rc::region_alloc_bytes(total)
    };
    // A promotion is a heap allocation the open region still owns: the slab
    // sweep at pop cannot reclaim it, so the region records it and frees it
    // there instead.
    let promoted = force_heap && crate::c_abi::rc::region_is_active();
    let (base, tag, zero_tail) = if region_base.is_null() {
        let layout = Layout::from_size_align(total, 8).expect("string layout is valid");
        // `layout` has non-zero size and a power-of-two alignment. The
        // matching `dealloc` below reconstructs the exact same layout.
        let base = string_body_alloc(layout);
        if base.is_null() {
            handle_alloc_error(layout);
        }
        (base, STR_BUILDER_TAG, true)
    } else {
        (region_base, STR_REGION_TAG, false)
    };
    // SAFETY: `base` points to `total` writable bytes. Header fields and the
    // trailing zero region are initialised here; `fill` initialises the content
    // prefix promised by its caller.
    unsafe {
        let content = base.add(STRING_BODY_OFFSET);
        let owner = base.cast::<StringOwner>();
        owner.write(StringOwner {
            abi_version: STRING_OWNER_VERSION,
            kind: STRING_OWNER_KIND,
            destructor: if tag == STR_REGION_TAG {
                STRING_DTOR_REGION
            } else if promoted {
                STRING_DTOR_REGION_HEAP
            } else {
                STRING_DTOR_HEAP
            },
            check: owner_check(content.cast::<c_char>()),
        });
        let hdr = base.add(STRING_OWNER_BYTES);
        std::ptr::copy_nonoverlapping(1u32.to_le_bytes().as_ptr(), hdr, 4);
        std::ptr::copy_nonoverlapping((cap as u32).to_le_bytes().as_ptr(), hdr.add(4), 4);
        std::ptr::copy_nonoverlapping((content_len as u32).to_le_bytes().as_ptr(), hdr.add(8), 4);
        *hdr.add(12) = tag;
        fill(content);
        if zero_tail {
            // Region allocations arrive zeroed. A heap allocation needs only
            // its terminator: the header's length is what says how much of
            // the content is text, the index footer is written in full below,
            // and spare capacity is written by the append that claims it. A
            // builder reserved for a large document would otherwise be
            // cleared once at its full size and again as it fills.
            *content.add(content_len) = 0;
        }
        // Empty content is ASCII, which is the index a reserved builder starts
        // from before its first append.
        match known {
            KnownText::Ascii => content
                .add(cap + 1)
                .cast::<u32>()
                .write_unaligned(STR_INDEX_ASCII),
            // Empty content is ASCII, and `index_utf8_content` says so.
            KnownText::Utf8 => index_utf8_content(content.cast::<c_char>(), content_len, cap),
            KnownText::Unchecked if content_len == 0 => content
                .add(cap + 1)
                .cast::<u32>()
                .write_unaligned(STR_INDEX_ASCII),
            KnownText::Unchecked => {
                rebuild_str_index(content.cast::<c_char>(), content_len, cap);
            }
        }
        if tag != STR_REGION_TAG {
            register_heap_string_body(content.cast::<c_char>());
            crate::c_abi::ledger::str_inc();
            if promoted {
                crate::c_abi::rc::region_track_promoted(content.cast::<c_char>());
            }
        }
        content.cast::<c_char>()
    }
}

/// Frees a promoted heap string at the pop of the region that owns it.
///
/// Retain and release never reach a promoted string, so its reference count is
/// not consulted here: the region is its one owner, and pop is its one free.
///
///
/// # Safety
///
/// `body` was recorded by `region_track_promoted` while this region was
/// open, and is freed exactly once, here.
pub(crate) unsafe fn free_promoted_string(body: *mut c_char) {
    if body.is_null() {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `body` a promoted string, whose legacy header
    // precedes it.
    let hdr = unsafe { body.cast::<u8>().sub(STRING_LEGACY_HEADER_BYTES) };
    // SAFETY: `hdr` addresses the 13-byte legacy header.
    let cap = u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] })
        as usize;
    let total = STRING_BODY_OFFSET + cap + 1 + str_index_bytes(cap);
    let layout = Layout::from_size_align(total, 8).expect("string layout is valid");
    unregister_heap_string_body(body);
    // SAFETY: the allocation base is `STRING_BODY_OFFSET` below the body, and
    // `layout` reconstructs the one `alloc_growable_with_fill` used.
    unsafe { string_body_free(body.cast::<u8>().sub(STRING_BODY_OFFSET), layout) };
    crate::c_abi::ledger::str_dec();
}

/// Reclaims a live heap c-string previously returned by [`alloc_cstring`].
/// The cleanup pass emits a call to this helper at every
/// body return for a non-escaping String produced by a known
/// String allocator (e.g. `gos_rt_stream_read_to_string`); the
/// escape analyser's non-capturing-callee whitelist ensures only
/// owning bindings reach this path so the drop never observes an
/// aliased pointer.
///
///
/// # Safety
///
/// Caller guarantees that `s` remains a valid C string for this call
/// and that it owns one live runtime reference. Foreign, static, and
/// region-backed strings are ignored without probing a private prefix. As with
/// every raw-pointer ABI, a stale pointer whose address has been reused cannot
/// be distinguished without a generation-bearing carrier type.
unsafe fn str_free_impl(s: *mut c_char, typed: bool) {
    ffi_entry!({
        // Region storage is reclaimed by its region's pop, which may already
        // have run: the address alone decides, before any header is read.
        if s.is_null() || crate::c_abi::rc::in_region_arena(s.cast()) {
            return;
        }
        let is_managed = if typed {
            // SAFETY: this `unsafe fn`'s caller passes `s` live; non-null, checked above.
            unsafe { typed_managed_string_owner(s) }.is_some()
        } else {
            is_managed_string(s)
        };
        if !is_managed {
            return;
        }
        crate::c_abi::ledger::benchmark_arc_release();
        // Refcounted carrier: [owner][rc:u32][cap:u32][len:u32][tag][content][NUL].
        // Carrier validation above establishes that the legacy suffix belongs
        // to a live runtime allocation.
        // SAFETY: the carrier check above found a live runtime string, whose 13-byte header
        // precedes the body.
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        // SAFETY: `hdr` addresses the header's count bytes.
        let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
        if rc & STR_SHARED != 0 {
            // SAFETY: `hdr` is the allocation-aligned count word, only ever accessed atomically
            // once shared.
            let cell = unsafe { AtomicU32::from_ptr(hdr.cast::<u32>()) };
            let prev = cell.fetch_sub(1, Ordering::Release);
            if prev & !STR_SHARED != 1 {
                return;
            }
            std::sync::atomic::fence(Ordering::Acquire);
        } else if rc > 1 {
            // SAFETY: the string is thread-local, so no other thread writes its count.
            unsafe {
                std::ptr::copy_nonoverlapping((rc - 1).to_le_bytes().as_ptr(), hdr, 4);
            }
            return;
        }
        let cap =
            // SAFETY: `hdr` addresses the header's capacity bytes.
            u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] })
                as usize;
        let total = STRING_BODY_OFFSET + cap + 1 + str_index_bytes(cap);
        let layout = Layout::from_size_align(total, 8).expect("string layout is valid");
        unregister_heap_string_body(s);
        // SAFETY: builder allocation uses this exact layout, and this is the
        // last strong reference after the count logic above. The carrier owns
        // the allocation base; `hdr` is only its legacy suffix.
        unsafe { string_body_free(s.cast::<u8>().sub(STRING_BODY_OFFSET), layout) };
        crate::c_abi::ledger::str_dec();
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_free(s: *mut c_char) {
    // SAFETY: `s` is this shim's string argument, null or a share it gives back (C-ABI contract).
    unsafe { str_free_impl(s, false) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_free_typed(s: *mut c_char) {
    // SAFETY: `s` is this shim's string argument, null or a share it gives back (C-ABI contract).
    unsafe { str_free_impl(s, true) };
}

/// Gives up the one reference a consuming call was handed.
///
/// A container that copies a string key out of its argument owns exactly one
/// reference to it: a caller passing a temporary hands over the only one it
/// had, and a caller passing `k.clone()` retains one at the call site for
/// this. Either way the reference to drop is one, so this is a plain
/// release: the count carries the rest, and any other live holder keeps the
/// value alive.
unsafe fn consume_moved_string_impl(s: *mut c_char, typed: bool) {
    // SAFETY: this `unsafe fn`'s caller passes `s` null or a share it gives back.
    unsafe { str_free_impl(s, typed) };
}

pub(crate) unsafe fn consume_moved_string(s: *mut c_char) {
    // SAFETY: this `unsafe fn`'s caller passes `s` null or a moved share it gives back.
    unsafe { consume_moved_string_impl(s, false) };
}

pub(crate) unsafe fn consume_moved_string_typed(s: *mut c_char) {
    // SAFETY: this `unsafe fn`'s caller passes `s` null or a moved share it gives back.
    unsafe { consume_moved_string_impl(s, true) };
}

/// True when `s` is a string value inside a compiler-typed Gossamer object.
///
///
/// # Safety
///
/// Unlike public raw C-string entry points, this internal RC dispatch
/// helper may only receive a pointer whose surrounding typed metadata already
/// establishes it as a valid Gossamer value. Static and region strings have no
/// registry entry, so their compiler-owned tag is read here to route cleanup
/// away from the RC header path. Do not use this to validate a foreign pointer.
#[inline]
pub unsafe fn is_gos_string(s: *const c_char) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_owner` accepts.
    unsafe { typed_str_owner(s).is_some() }
}

unsafe fn str_retain_impl(s: *const c_char, typed: bool) {
    let is_managed = if typed {
        // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which
        // `typed_managed_string_owner` accepts.
        unsafe { typed_managed_string_owner(s) }.is_some()
    } else {
        is_managed_string(s)
    };
    if !is_managed {
        return;
    }
    crate::c_abi::ledger::benchmark_arc_retain();
    // SAFETY: `s` is a managed string (checked above), whose 13-byte header precedes the body.
    let hdr = unsafe { s.cast::<u8>().sub(13) };
    // SAFETY: `hdr` addresses the header's count bytes.
    let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
    if rc & STR_SHARED != 0 {
        // Goroutine-shared: atomic increment of the low-31-bit count. `hdr` is
        // the allocation base (allocator-aligned >= 4), so the cast is sound;
        // the count cannot reach the shared bit, so `fetch_add` preserves it.
        // SAFETY: `hdr` is the allocation-aligned count word, only ever accessed atomically once
        // shared.
        let cell = unsafe { AtomicU32::from_ptr(hdr.cast_mut().cast::<u32>()) };
        cell.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: the string is thread-local, so no other thread writes its count.
    unsafe {
        std::ptr::copy_nonoverlapping(
            rc.saturating_add(1).to_le_bytes().as_ptr(),
            hdr.cast_mut(),
            4,
        );
    }
}

/// Increment a heap (`STR_BUILDER_TAG`) string's refcount; no-op otherwise.
pub(crate) unsafe fn gos_rt_str_retain(s: *const c_char) {
    // SAFETY: this `unsafe fn`'s caller passes `s` null or live, which the retain accepts.
    unsafe { str_retain_impl(s, false) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_retain_typed(s: *const c_char) {
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    unsafe { str_retain_impl(s, true) };
}

/// `s.clone()` for a `String`: a second share of the same text.
///
/// A `String` is immutable in place - an append that has to grow builds a
/// new one - so a clone does not have to copy the bytes to behave like a
/// separate value. What it does have to do is take a share of its own:
/// answering the argument unchanged, as this once did by lowering to
/// nothing at all, hands a consuming callee the caller's only share, and
/// the caller's binding is freed while it is still holding it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_clone(s: *const c_char) -> *const c_char {
    ffi_entry!({
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
        unsafe { str_retain_impl(s, true) };
        s
    })
}

/// Marks a `STR_BUILDER` string as goroutine-shared so subsequent
/// retain/release use atomic counting. No-op for other string kinds (static /
/// region / fixed `STR_ALLOC` strings carry no refcount). Called from
/// `gos_rt_rc_mark_shared` when a string escapes to another goroutine.
pub(crate) unsafe fn gos_rt_str_mark_shared(s: *const c_char) {
    if !is_managed_string(s) {
        return;
    }
    // SAFETY: `s` is a managed string (checked above), whose 13-byte header precedes the body.
    let hdr = unsafe { s.cast::<u8>().sub(13) };
    // SAFETY: `hdr` is the allocation-aligned count word, accessed atomically from here on.
    let cell = unsafe { AtomicU32::from_ptr(hdr.cast_mut().cast::<u32>()) };
    cell.fetch_or(STR_SHARED, Ordering::Relaxed);
}

/// Allocate an owned, NUL-terminated heap string holding `s`'s bytes (the
/// growable runtime-string allocator shape).
/// Re-allocates `c` as a tagged Gossamer string for tests.
///
/// Same contract as [`test_gos_str`]: a `CString` built by a test has no
/// length header, so a shim probing for one would read before it.
#[cfg(test)]
pub(crate) fn test_gos_ptr(c: &std::ffi::CStr) -> *const c_char {
    alloc_cstring(c.to_bytes()).cast_const()
}

/// Allocates a tagged Gossamer string for tests.
///
/// The C ABI receives a pointer whose length header sits before it, so a bare
/// `c"..."` literal has no header and probing for one reads outside the
/// literal. Tests that feed a `gos_rt_*` string parameter build their input
/// here instead.
#[cfg(test)]
pub(crate) fn test_gos_str(text: &str) -> *const c_char {
    alloc_cstring(text.as_bytes()).cast_const()
}

pub fn alloc_cstring(s: &[u8]) -> *mut c_char {
    alloc_cstring_from_slices(&[s])
}

/// Allocates one c-string holding the byte-wise concatenation of
/// `parts`, with a single allocator round trip. Used by
/// `gos_rt_str_concat` (which previously allocated a transient
/// `Vec<u8>` and then re-allocated through `alloc_cstring`,
/// paying two malloc/free pairs per `+`).
///
/// Layout: one allocator-tag byte, then the joined content bytes,
/// then NUL. The returned pointer is 1 byte into the allocation
/// (the content head) so `CStr::from_ptr` and `strlen` see a
/// normal c-string. Runtime ownership is recorded when the allocation is
/// created, so release never needs to inspect memory before an arbitrary raw
/// pointer.
pub fn alloc_cstring_from_slices(parts: &[&[u8]]) -> *mut c_char {
    // Use the length-carrying builder layout (cap = content length) so the
    // result has its byte length stored at `ptr[-5]` for O(1)
    // `gos_rt_str_len` / `gos_rt_str_slice`. A later in-place `+=` finds no
    // spare capacity and reallocates with doubling - correctness and the
    // amortised growth analysis are unchanged. `gos_rt_str_free` and the
    // concat fast path already handle `STR_BUILDER_TAG`.
    let total: usize = parts.iter().map(|p| p.len()).sum();
    alloc_growable(parts, total)
}

/// Allocate one runtime string and fill it with ASCII-uppercase bytes from
/// `src`. The caller has already proven `src.is_ascii()`, so Unicode
/// expansion never applies and the output length equals the input length.
fn alloc_ascii_upper_cstring(src: &[u8]) -> *mut c_char {
    let len = src.len();
    let force_heap = crate::c_abi::rc::in_region_arena(src.as_ptr());
    alloc_growable_with_fill(len, len, force_heap, |out| {
        for (i, &b) in src.iter().enumerate() {
            let upper = if b.is_ascii_lowercase() {
                b - (b'a' - b'A')
            } else {
                b
            };
            // SAFETY: `alloc_growable_with_fill` passes `len` writable content
            // bytes and this loop writes each byte exactly once.
            unsafe {
                *out.add(i) = upper;
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_len(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    ffi_entry!({ unsafe { typed_str_char_len(s) as i64 } })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_byte_len(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    ffi_entry!({ unsafe { typed_str_len(s) as i64 } })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_is_empty(s: *const c_char) -> bool {
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    ffi_entry!({ unsafe { gos_rt_str_len(s) == 0 } })
}

/// The capacity and length of `s` when it is a heap builder this reference
/// holds alone, so its bytes may be rewritten in place.
///
///
/// # Safety
///
/// `s` is null or a Gossamer string body.
pub(crate) unsafe fn unique_builder_cap_len(s: *const c_char) -> Option<(usize, usize)> {
    // SAFETY: this `unsafe fn`'s caller passes `s` null or live, which the probe accepts.
    if !unsafe { is_typed_builder(s) } {
        return None;
    }
    // SAFETY: `s` is a typed builder (checked above), so its 13-byte header of count, capacity,
    // length, and tag precedes the body.
    let hdr = unsafe { s.cast::<u8>().sub(13) };
    // SAFETY: `hdr` addresses the header's count bytes.
    let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
    if rc != 1 {
        return None;
    }
    // SAFETY: `hdr` addresses the header's capacity bytes.
    let cap = u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] });
    // SAFETY: `hdr` addresses the header's length bytes.
    let len = u32::from_le_bytes(unsafe { [*hdr.add(8), *hdr.add(9), *hdr.add(10), *hdr.add(11)] });
    Some((cap as usize, len as usize))
}

/// Sets the length of the unique builder `s` to `len`, which must not exceed
/// its current length and must fall on a character boundary.
///
///
/// # Safety
///
/// `unique_builder_cap_len(s)` answered `Some((cap, _))`.
unsafe fn shorten_unique_builder(s: *mut c_char, len: usize, cap: usize) {
    // SAFETY: this `unsafe fn`'s caller passes `s` a uniquely held builder of capacity `cap` and
    // `len <= cap`.
    unsafe {
        *s.cast::<u8>().add(len) = 0;
        let hdr = s.cast::<u8>().sub(13);
        std::ptr::copy_nonoverlapping((len as u32).to_le_bytes().as_ptr(), hdr.add(8), 4);
        let footer = s.cast::<u8>().add(cap + 1).cast::<u32>();
        // A prefix of ASCII content is ASCII; anything else is re-indexed.
        if len == 0 || footer.read_unaligned() == STR_INDEX_ASCII {
            footer.write_unaligned(STR_INDEX_ASCII);
        } else {
            rebuild_str_index(s, len, cap);
        }
    }
}

/// Appends ASCII `parts` to `acc` in place when `acc` is a builder this
/// reference holds alone, its content is ASCII, and it has room: the copy and
/// the new length, with the character index left saying every byte is one
/// character. Answers whether it appended; `acc` is untouched otherwise.
///
///
/// # Safety
///
/// `acc` is null or a Gossamer string body, and every part is ASCII.
#[inline]
unsafe fn append_ascii_in_place(acc: *const c_char, parts: &[&[u8]]) -> bool {
    // SAFETY: this `unsafe fn`'s caller passes `acc` null or live, which the probe accepts.
    let Some((cap, len)) = (unsafe { unique_builder_cap_len(acc) }) else {
        return false;
    };
    let added: usize = parts.iter().map(|p| p.len()).sum();
    if len + added > cap {
        return false;
    }
    // SAFETY: `acc` is a uniquely held builder with room for `added` more bytes (checked above).
    unsafe {
        let footer = acc.cast::<u8>().add(cap + 1).cast::<u32>();
        if footer.read_unaligned() != STR_INDEX_ASCII {
            return false;
        }
        let dst = acc.cast_mut().cast::<u8>().add(len);
        let mut at = 0;
        for part in parts {
            copy_small_bytes(part.as_ptr(), dst.add(at), part.len());
            at += part.len();
        }
        *dst.add(added) = 0;
        let hdr = acc.cast_mut().cast::<u8>().sub(13);
        std::ptr::copy_nonoverlapping(((len + added) as u32).to_le_bytes().as_ptr(), hdr.add(8), 4);
    }
    true
}

/// The window `buf[start..end]` of a buffer whose slots are bytes, or `None`
/// for a wide buffer or a window that does not lie within it.
///
///
/// # Safety
///
/// `buf` is null or a live `GosVec` whose storage outlives the slice.
#[inline]
pub(crate) unsafe fn packed_byte_window<'a>(
    buf: *const crate::c_abi::vec::GosVec,
    start: i64,
    end: i64,
) -> Option<&'a [u8]> {
    if buf.is_null() || start < 0 || end < start {
        return None;
    }
    // SAFETY: `buf` is non-null (checked above), and this `unsafe fn`'s caller passes a live
    // `Vec`.
    let header = unsafe { &*buf };
    if header.elem_bytes != 1 || end > header.len || header.ptr.is_null() {
        return None;
    }
    let (lo, hi) = (start as usize, end as usize);
    // SAFETY: `start..end` lies inside the byte vec's `len` bytes at a non-null `ptr` (checked
    // above).
    Some(unsafe { std::slice::from_raw_parts(header.ptr.as_const_ptr().add(lo), hi - lo) })
}

/// `s.clear()` for compiled String method lowering. Consumes `s` and answers
/// the cleared string: `s` itself, emptied in place and keeping its capacity,
/// when this reference holds it alone; otherwise a fresh empty string with
/// the same capacity, and `s` is released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_clear(s: *const c_char) -> *mut c_char {
    // Bare (no `ffi_entry!`), as `gos_rt_str_concat_drop_a` is: a buffer
    // cleared per request sits on the hot path, and nothing here unwinds -
    // header reads and writes, and an allocation whose failure aborts.
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    if let Some((cap, _)) = unsafe { unique_builder_cap_len(s) } {
        // SAFETY: `s` is a uniquely held builder of capacity `cap`.
        unsafe { shorten_unique_builder(s.cast_mut(), 0, cap) };
        return s.cast_mut();
    }
    // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
    let cleared = if unsafe { is_typed_builder(s) } {
        // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `typed_str_cap` accepts.
        alloc_growable(&[], unsafe { typed_str_cap(s) }.unwrap_or(0))
    } else {
        alloc_cstring(b"")
    };
    if is_managed_string(s) {
        // SAFETY: `s` arrived as a consuming-call argument, so this call owns the share it
        // releases (C-ABI contract).
        unsafe { gos_rt_str_free(s.cast_mut()) };
    }
    cleared
}

/// Allocates an empty owned string with at least `capacity` writable bytes.
/// This is the compiled implementation of `String::with_capacity`; subsequent
/// unique `push_str` calls reuse the allocation until the reserved space is
/// exhausted.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_str_with_capacity(capacity: i64) -> *mut c_char {
    if capacity < 0 {
        crate::c_abi::panic::panic_text("String::with_capacity: capacity must be non-negative");
    }
    let capacity = usize::try_from(capacity).unwrap_or(u32::MAX as usize);
    alloc_growable(&[], capacity.min(u32::MAX as usize))
}

/// `s.truncate(n)` for compiled String method lowering. The public method takes
/// a byte length; if `n` lands inside a UTF-8 scalar, truncate to the preceding
/// valid boundary so the returned Gossamer String remains well-formed.
///
/// Consumes `s` and answers the truncated string: `s` itself, shortened in
/// place, when this reference holds it alone; otherwise a copy of the kept
/// prefix, and `s` is released.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_truncate(s: *const c_char, n: i64) -> *mut c_char {
    ffi_entry_passthrough!({
        if n < 0 {
            crate::c_abi::panic::panic_text("truncate: length must be non-negative");
        }
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract); non-null,
        // checked above.
        let len = unsafe { gos_str_arg_len(s) };
        let limit = usize::try_from(n).unwrap_or(0).min(len);
        // SAFETY: `s` is non-null (checked above) and its first `len` bytes are its content.
        let bytes = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) };
        let end = if limit == len {
            len
        } else {
            utf8_boundary_at_or_before(bytes, limit)
        };
        // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract); non-null,
        // checked above.
        if let Some((cap, _)) = unsafe { unique_builder_cap_len(s) } {
            if end < len {
                // SAFETY: `s` is a uniquely held builder of capacity `cap`, and `end < len <=
                // cap`.
                unsafe { shorten_unique_builder(s.cast_mut(), end, cap) };
            }
            return s.cast_mut();
        }
        let kept = alloc_cstring_from_slices(&[&bytes[..end]]);
        if is_managed_string(s) {
            // SAFETY: `s` arrived as a consuming-call argument, so this call owns the share it
            // releases (C-ABI contract).
            unsafe { gos_rt_str_free(s.cast_mut()) };
        }
        kept
    })
}

/// The largest character boundary of `bytes` at or before `limit`; `limit`
/// itself for bytes that are not UTF-8.
fn utf8_boundary_at_or_before(bytes: &[u8], limit: usize) -> usize {
    if std::str::from_utf8(bytes).is_err() {
        return limit;
    }
    // A continuation byte is `10xxxxxx`; a boundary is any other position.
    (0..=limit)
        .rev()
        .find(|&i| i == bytes.len() || (bytes[i] & 0xC0) != 0x80)
        .unwrap_or(0)
}

/// `String::from_utf8(bytes) -> Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_string_from_utf8(bytes: *const GosVec) -> i128 {
    ffi_entry!({
        if bytes.is_null() {
            return gos_rt_result_new(0, alloc_cstring(b"") as i64);
        }
        // SAFETY: `bytes` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*bytes };
        let mut out = Vec::with_capacity(vec.len.max(0) as usize);
        for idx in 0..vec.len.max(0) {
            // SAFETY: `idx` is below the vec's length.
            let b = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, idx) };
            out.push(b as u8);
        }
        match std::str::from_utf8(&out) {
            Ok(_) => gos_rt_result_new(0, alloc_cstring_from_slices(&[&out]) as i64),
            Err(e) => {
                let msg = format!("String::from_utf8: {e}");
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

/// Generic length-zero check used by `is_empty` for any
/// receiver whose length is reachable through `gos_rt_len`
/// (Vec / array / slice / hashmap …).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_len_is_zero(p: *const i64) -> bool {
    // SAFETY: `p` is this shim's argument, null or a live sized value (C-ABI contract).
    ffi_entry!({ unsafe { gos_rt_len(p) == 0 } })
}

/// Clones a `*mut GosVec` element-by-element. Used by
/// `xs.to_vec()` so the result is independent of the source -
/// without this, the previous identity lowering aliased the
/// source buffer and mutations like `out.swap(i, j)` clobbered
/// the caller's input.
///
/// **Allocator domain:** the header is `Box::into_raw` and the
/// data buffer is `Vec<u8>` (`Global`-allocated, then `forget`-ed),
/// so the buffer matches the layout `gos_rt_vec_push` reconstructs
/// via `Vec::from_raw_parts(...)` when the vec needs to grow. The
/// previous version allocated both from the bump arena
/// (`gos_rt_gc_alloc`); a subsequent push past `cap` would feed an
/// arena interior pointer to the global allocator's deallocator, a
/// cross-domain free that produced heisencrashes anywhere else in
/// the runtime malloc'd next. See
/// `~/dev/contexts/lang/fix_architecture_ownership.md` §3.1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_clone(src: *const GosVec) -> *mut GosVec {
    ffi_entry!({
        if src.is_null() {
            return gos_rt_vec_new(8);
        }
        // SAFETY: `src` is a handle from compiled code, checked non-null above and live for the whole call.
        let s = unsafe { &*src };
        let bytes = (s.len as usize) * (s.elem_bytes as usize);
        // Header + element buffer in one `Box<InlineVec>` (inline for a
        // small vec, else a separate buffer), then copy the source slots
        // into whichever data region `ptr` lands at. Ledger + strong count
        // are set by `alloc_box_vec`, symmetric with `gos_rt_vec_free`.
        let out = crate::c_abi::vec::alloc_box_vec(s.elem_bytes, s.elem_kind, s.len, s.len);
        // SAFETY: `out` is the vec `alloc_box_vec` just made.
        let data = unsafe { (*out).ptr.as_ptr() };
        if bytes > 0 && !s.ptr.is_null() && !data.is_null() {
            // SAFETY: `out`'s buffer holds `len` elements of the source's width, and the two do
            // not overlap.
            unsafe { std::ptr::copy_nonoverlapping(s.ptr.as_ptr(), data, bytes) };
        }
        // SAFETY: `src` is live and `out` holds raw copies of its elements.
        unsafe { crate::c_abi::vec::vec_adopt_element_shares(src, out) };
        out
    })
}

/// Materialises `s.as_bytes()` as a real `GosVec<u8>` so callees
/// receiving `&[u8]` can call `.len()` / `.iter()` / index it
/// the same way they would any other slice. The previous
/// identity lowering returned the raw c-string ptr - `.len()`
/// on it read the first 8 content bytes as a Vec length prefix,
/// and `.iter()` walked off into garbage. Backing buffer +
/// header are arena-allocated; the next `gos_rt_gc_reset`
/// reclaims them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_as_bytes(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let len = if s.is_null() {
            0
        } else {
            // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `gos_str_arg_len` accepts.
            unsafe { gos_str_arg_len(s) }
        };
        let bytes = if len == 0 || s.is_null() {
            &[][..]
        } else {
            // SAFETY: `s` is non-null (checked above) and its first `len` bytes are its content.
            unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len) }
        };
        super::encoding::bytes_to_gosvec(bytes)
    })
}

/// `s.chars()` - materialises the string's Unicode scalar values as a
/// fresh `*mut GosVec` of i64 codepoints (one 8-byte slot per char), so
/// `for ch in s.chars()` reads each scalar via `gos_rt_vec_get_i64` and
/// binds a `char`. Mirrors the interp builtin so `gos` and
/// `gos build` agree. The backing buffer + header are
/// `Box::from_raw`-compatible (via `gos_rt_vec_with_capacity`) so the
/// auto-emitted `gos_rt_vec_free` at scope-end reclaims them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_chars(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        // A UTF-8 string has at most one scalar per byte, so its byte length is
        // a safe capacity upper bound. Allocate once and discover the exact
        // scalar count while filling instead of scanning the string twice.
        let v = gos_rt_vec_with_capacity(8, st.len() as i64);
        if v.is_null() {
            return v;
        }
        // SAFETY: `v` is the fresh non-null vec with capacity for every char.
        unsafe {
            let header = &mut *v;
            let dst = header.ptr.cast::<i64>();
            let mut char_count = 0;
            for (i, ch) in st.chars().enumerate() {
                *dst.add(i) = i64::from(u32::from(ch));
                char_count = i + 1;
            }
            header.len = char_count as i64;
        }
        v
    })
}

/// Formats a signed integer directly as a fresh `Vec<char>`. Decimal integer
/// text is ASCII, so each formatted byte is also its Unicode scalar value.
/// This is the allocation-fused implementation of `n.to_string().chars()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_i64_chars(n: i64) -> *mut GosVec {
    ffi_entry!({
        let mut buffer = itoa::Buffer::new();
        let bytes = buffer.format(n).as_bytes();
        let v = gos_rt_vec_with_capacity(8, bytes.len() as i64);
        if v.is_null() {
            return v;
        }
        // SAFETY: `v` is the fresh non-null vec with capacity for every digit.
        unsafe {
            let header = &mut *v;
            let dst = header.ptr.cast::<i64>();
            for (i, byte) in bytes.iter().copied().enumerate() {
                *dst.add(i) = i64::from(byte);
            }
            header.len = bytes.len() as i64;
        }
        v
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_byte_at(s: *const c_char, i: i64) -> i64 {
    // Bare (no `ffi_entry!`): byte access is a generated-code primitive, is
    // panic-free, and commonly executes once per input byte. Wrapping each
    // read in `catch_unwind` and an allocation-registry lock made parsers pay
    // synchronization overhead in their innermost loop.
    if s.is_null() || i < 0 {
        return 0;
    }
    // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract); non-null, checked
    // above.
    let len = unsafe { typed_str_len(s) };
    if i as usize >= len {
        return 0;
    }
    // SAFETY: `i` lies in `[0, len)`, so the byte at offset `i` is within the
    // compiler-typed string's content bytes.
    let byte = unsafe { *s.cast::<u8>().add(i as usize) };
    i64::from(byte)
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_char_at(s: *const c_char, i: i64) -> i64 {
    // `s[i]` is an indexed read, and an index outside `[0, len)` panics, in the
    // wording every sequence access reports so a failure's text does not
    // depend on the tier that ran it.
    if s.is_null() {
        crate::c_abi::panic::panic_oob_text("vec index", i, 0);
    }
    // SAFETY: `s` is non-null (checked above) and this shim's live string argument (C-ABI
    // contract).
    let char_len = unsafe { typed_str_char_len(s) };
    if i < 0 || i as usize >= char_len {
        crate::c_abi::panic::panic_oob_text("vec index", i, char_len as i64);
    }
    // SAFETY: `s` is live, and `i` is below its char length (checked above).
    let Some(byte) = (unsafe { typed_str_char_boundary(s, i as usize) }) else {
        crate::c_abi::panic::panic_oob_text("vec index", i, char_len as i64);
    };
    // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `typed_str_bytes` accepts.
    let bytes = unsafe { typed_str_bytes(s) };
    utf8_scalar_at(bytes, byte).map_or(0, |ch| i64::from(u32::from(ch)))
}

/// `os::read_dir(path) -> Result<Vec<String>, errors::Error>` -
/// returns the entry names under `path` as a `*mut GosVec` of
/// `*const c_char`. Gossamer programs treat the call as
/// fallible, but the MIR pin keeps it as a plain `Vec<String>`
/// today (matching the interp's shape) - error cases land as an
/// empty vec rather than a Result-shaped Adt.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_read_dir(path: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let p = if path.is_null() {
            std::path::PathBuf::from(".")
        } else {
            // SAFETY: `path` is a String argument from compiled code, null or a live string body for the whole call.
            let encoded = unsafe { gos_str_arg_lossy(path) };
            super::args::decode_os_path(&encoded)
        };
        let entries: Vec<String> = match std::fs::read_dir(&p) {
            Ok(it) => {
                let mut names: Vec<String> = it
                    .flatten()
                    .map(|e| super::args::encode_os_path(std::path::Path::new(&e.file_name())))
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        };
        // STRING-typed: the vec owns the entry-name strings.
        let out = {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        for name in entries {
            let cs = alloc_cstring(name.as_bytes()) as i64;
            // SAFETY: `vec` is the live vec made above, and `cs` one 8-byte element.
            unsafe {
                gos_rt_vec_push_i64(out, cs);
            }
        }
        out
    })
}

/// `s.substring(start, end)` - byte-range slice. Clamps `start`
/// and `end` into `[0, byte_len(s)]` and returns the indicated byte
/// substring as a fresh `*mut c_char`. Bounds inside a multibyte scalar
/// advance to the next UTF-8 boundary. Mirrors the interp
/// builtin so user code that calls `s.substring(a, b)` runs the
/// same way under `gos` and `gos build` - without this
/// helper the compiled tier saw `s.substring(...)` as an
/// undispatched method, fell through to a free-fn lookup, and
/// resolved to a user-defined `pub fn substring` (askq's
/// `util::substring` calls `s.substring` recursively, which then
/// stack-overflowed instead of reaching the runtime slice).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_substring(
    s: *const c_char,
    start: i64,
    end: i64,
) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is non-null (checked above) and this shim's live string argument (C-ABI
        // contract).
        let bytes = unsafe { substring_bytes(s, start, end) };
        // SAFETY: `s` is live, and `bytes` is a window of its content.
        unsafe { alloc_slice_cstring(s, bytes) }
    })
}

/// The bytes `s.substring(start, end)` answers: the offsets clamped into the
/// string, the end no earlier than the start, each moved forward to the next
/// character boundary.
///
/// # Safety
///
/// `s` is a non-null Gossamer string body that stays alive while the slice is
/// in use.
#[allow(
    clippy::inline_always,
    reason = "shared by `substring` and `push_substring`, each called once per slice a program cuts: left to the heuristic, LLVM keeps it out of line and every substring pays a call"
)]
#[inline(always)]
unsafe fn substring_bytes<'a>(s: *const c_char, start: i64, end: i64) -> &'a [u8] {
    // O(1) length from the string's length header (every runtime-built
    // string carries one); an untagged rodata literal falls back to
    // strlen. Sizing the slice from the header keeps `substring`
    // proportional to the slice length, not the source length, so a
    // sliding-window scan over one string stays linear.
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `typed_str_len` accepts.
    let byte_len = unsafe { typed_str_len(s) };
    let len_i = byte_len as i64;
    let lo = start.clamp(0, len_i) as usize;
    let hi = end.clamp(0, len_i).max(start.clamp(0, len_i)) as usize;
    // SAFETY: this `unsafe fn`'s caller passes `s` a live string body.
    let lo_byte = unsafe { typed_str_next_char_boundary(s, lo) }.unwrap_or(byte_len);
    // SAFETY: this `unsafe fn`'s caller passes `s` a live string body.
    let hi_byte = unsafe { typed_str_next_char_boundary(s, hi) }.unwrap_or(byte_len);
    // SAFETY: `s` is a live string body whose first `byte_len` bytes are its content.
    let bytes = unsafe { std::slice::from_raw_parts(s.cast::<u8>(), byte_len) };
    &bytes[lo_byte..hi_byte]
}

/// `acc.push_str(src.substring(start, end))` without the intermediate
/// `String`: appends the characters `substring` would answer straight from
/// `src`, taking and answering the accumulator as
/// [`gos_rt_str_append_bytes`] does. `src` may be `acc` itself: an append in
/// place copies from below the current length, and a regrowth reads both
/// before the old buffer is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_push_substring(
    acc: *const c_char,
    src: *const c_char,
    start: i64,
    end: i64,
) -> *mut c_char {
    ffi_entry!({
        let bytes: &[u8] = if src.is_null() {
            &[]
        } else {
            // SAFETY: `src` is non-null (checked above) and this shim's live string argument
            // (C-ABI contract).
            unsafe { substring_bytes(src, start, end) }
        };
        // A slice of ASCII text is ASCII, so the accumulator's index is
        // extended without scanning the bytes appended.
        // SAFETY: `src` is this shim's string argument, null or live (C-ABI contract).
        let ascii = unsafe { typed_str_is_ascii(src) };
        // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
        // contract).
        unsafe { str_append_parts(acc, &[bytes], ascii) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_concat(a: *const c_char, b: *const c_char) -> *mut c_char {
    ffi_entry!({
        // Both operands are language `String` values, so their length comes
        // from the header rather than a NUL scan: a string may contain
        // interior NULs, and one that starts with a NUL is not empty.
        // Writing into the destination directly sizes the allocation from
        // the two lengths without an intermediate `Vec`.
        // SAFETY: `a` is a String argument from compiled code, null or a live string body for the whole call.
        let a_bytes: &[u8] = unsafe { gos_str_arg_bytes(a) };
        // SAFETY: `b` is a String argument from compiled code, null or a live string body for the whole call.
        let b_bytes: &[u8] = unsafe { gos_str_arg_bytes(b) };
        let force_heap = crate::c_abi::rc::in_region_arena(a.cast())
            || crate::c_abi::rc::in_region_arena(b.cast());
        // Two ASCII operands concatenate to ASCII, so the index is written
        // without reading the copied bytes again.
        // SAFETY: `a` and `b` are this shim's string arguments, each null or live (C-ABI
        // contract).
        if unsafe { typed_str_is_ascii(a) && typed_str_is_ascii(b) } {
            let len = a_bytes.len() + b_bytes.len();
            return alloc_growable_filled(len, len, force_heap, KnownText::Ascii, |out| unsafe {
                // SAFETY: the allocation passes `len` writable content bytes,
                // and the two parts fill them exactly once, in order.
                copy_builder_part(a_bytes.as_ptr(), out, a_bytes.len());
                copy_builder_part(b_bytes.as_ptr(), out.add(a_bytes.len()), b_bytes.len());
            });
        }
        alloc_growable_forced(
            &[a_bytes, b_bytes],
            a_bytes.len() + b_bytes.len(),
            force_heap,
        )
    })
}

/// Answers the concatenation of `a` with an empty right side: `a` itself when
/// it is already owned, and an owned copy otherwise.
///
///
/// # Safety
///
/// `a` is null or a Gossamer string body.
unsafe fn concat_with_empty(a: *const c_char) -> *mut c_char {
    if is_managed_string(a) {
        return a.cast_mut();
    }
    // SAFETY: this `unsafe fn`'s caller passes `a` live or null, which `gos_str_arg_bytes`
    // accepts.
    let a_bytes: &[u8] = unsafe { gos_str_arg_bytes(a) };
    let force_heap = crate::c_abi::rc::in_region_arena(a.cast());
    alloc_growable_forced(&[a_bytes], 64.max(a_bytes.len()), force_heap)
}

/// Concatenates `a + b`, frees `a`, and returns the result.
///
/// Implements amortized O(1) string accumulation: when `a` is already a
/// growable string (`STR_BUILDER_TAG`) with enough spare capacity, `b` is
/// appended in-place without any allocation. When capacity is exhausted the
/// buffer is reallocated with 2x the required size, giving O(n) total copy
/// work across n append operations (standard doubling analysis).
///
/// Safe when `a` is null or a rodata literal: those paths allocate a fresh
/// growable buffer rather than attempting to free an unowned pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_concat_drop_a(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    // Bare (no `ffi_entry!`): like the RC primitives, this is on the hot
    // string-accumulation path (one call per appended fragment) where the
    // per-call catch_unwind setup dominates, and it is panic-free across the
    // FFI boundary - pointer arithmetic, memcpy, and a stack `write!` never
    // unwind; the only failure path (`alloc_growable` OOM) aborts.
    {
        // Emptiness is a header length of zero. A `String` whose first byte
        // is a NUL still has content to append.
        // SAFETY: `b` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `typed_str_bytes` accepts.
        let b_bytes: &[u8] = unsafe { typed_str_bytes(b) };
        let len_b = b_bytes.len();

        if len_b == 0 {
            // SAFETY: `a` is this shim's accumulator argument, null or a share it hands on (C-ABI
            // contract).
            return unsafe { concat_with_empty(a) };
        }

        // Fast path: a is a known live heap builder - try in-place append.
        // Region- and static-backed pointers carry a non-heap destructor and
        // take the copying path below, which keeps their compiler-owned
        // storage immutable.
        // SAFETY: `a` is this shim's accumulator argument, null or live (C-ABI contract).
        if unsafe { is_typed_builder(a) } {
            // SAFETY: `a` is a typed builder (checked above), so its 13-byte header of count,
            // capacity, length, and tag precedes the body.
            let hdr = unsafe { a.cast::<u8>().sub(13) };
            // SAFETY: `hdr` addresses the header's count bytes.
            let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
            let cap =
                // SAFETY: `hdr` addresses the header's capacity bytes.
                u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] })
                    as usize;
            // SAFETY: `hdr` addresses the header's length bytes.
            let len_a = u32::from_le_bytes(unsafe {
                [*hdr.add(8), *hdr.add(9), *hdr.add(10), *hdr.add(11)]
            }) as usize;
            let new_len = len_a + len_b;
            // In-place only when sole owner (rc == 1): mutating a shared
            // buffer would corrupt other holders.
            if new_len <= cap && rc == 1 {
                // SAFETY: `a` is a uniquely held builder (rc 1) with room for the new length
                // (checked above).
                unsafe {
                    let dst = (a as *mut u8).add(len_a);
                    copy_small_bytes(b_bytes.as_ptr(), dst, len_b);
                    *dst.add(len_b) = 0;
                    let hdr_mut = hdr.cast_mut();
                    std::ptr::copy_nonoverlapping(
                        (new_len as u32).to_le_bytes().as_ptr(),
                        hdr_mut.add(8),
                        4,
                    );
                    extend_str_index(a.cast_mut(), len_a, b_bytes, cap);
                }
                return a.cast_mut();
            }
            // Shared or capacity exhausted: copy, allocate fresh, drop one ref.
            // SAFETY: `a`'s first `len_a` bytes are its content.
            let a_content = unsafe { std::slice::from_raw_parts(a.cast::<u8>(), len_a) };
            let new_cap = (new_len * 2).max(64);
            let result = alloc_growable(&[a_content, b_bytes], new_cap);
            // SAFETY: `a` arrived as a consuming accumulator, so this call owns the share it
            // releases (C-ABI contract).
            unsafe { gos_rt_str_free(a.cast_mut()) };
            return result;
        }

        // a is null, a literal, or a fixed heap string - allocate fresh growable.
        // SAFETY: `a` is a String argument from compiled code, null or a live string body for the whole call.
        let a_bytes: &[u8] = unsafe { gos_str_arg_bytes(a) };
        let new_len = a_bytes.len() + len_b;
        let new_cap = (new_len * 2).max(64);
        let force_heap = crate::c_abi::rc::in_region_arena(a.cast())
            || crate::c_abi::rc::in_region_arena(b.cast());
        let result = alloc_growable_forced(&[a_bytes, b_bytes], new_cap, force_heap);
        if is_managed_string(a) {
            // SAFETY: `a` arrived as a consuming accumulator, so this call owns the share it
            // releases (C-ABI contract).
            unsafe { gos_rt_str_free(a.cast_mut()) };
        }
        result
    }
}

/// Appends `len` bytes at `b` onto growable string `acc`, freeing/reusing
/// `acc`, and returns the result. The byte-counted counterpart of
/// [`gos_rt_str_concat_drop_a`]: the caller supplies the fragment length
/// (a compile-time constant for string-literal appends), so the hot path
/// skips even the header read that `concat_drop_a` pays per call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_append_bytes(
    acc: *const c_char,
    b: *const u8,
    len: i64,
) -> *mut c_char {
    let len_b = if len < 0 { 0 } else { len as usize };
    if len_b == 0 {
        // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
        // contract).
        return unsafe { concat_with_empty(acc) };
    }
    // SAFETY: `b` is this shim's byte argument, addressing `len` bytes (C-ABI contract).
    let b_bytes: &[u8] = unsafe { std::slice::from_raw_parts(b, len_b) };

    // Generated code supplies a typed String, so its private tag is directly
    // available without a global allocation-registry lookup.
    // SAFETY: `acc` is this shim's accumulator argument, null or live (C-ABI contract).
    if unsafe { is_typed_builder(acc) } {
        // SAFETY: `acc` is a typed builder (checked above), so its 13-byte header of count,
        // capacity, length, and tag precedes the body.
        let hdr = unsafe { acc.cast::<u8>().sub(13) };
        // SAFETY: `hdr` addresses the header's count bytes.
        let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
        let cap =
            // SAFETY: `hdr` addresses the header's capacity bytes.
            u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] })
                as usize;
        let len_a =
            // SAFETY: `hdr` addresses the header's length bytes.
            u32::from_le_bytes(unsafe { [*hdr.add(8), *hdr.add(9), *hdr.add(10), *hdr.add(11)] })
                as usize;
        let new_len = len_a + len_b;
        if new_len <= cap && rc == 1 {
            // SAFETY: `acc` is a uniquely held builder (rc 1) with room for the new length
            // (checked above).
            unsafe {
                let dst = (acc as *mut u8).add(len_a);
                copy_small_bytes(b_bytes.as_ptr(), dst, len_b);
                *dst.add(len_b) = 0;
                let hdr_mut = hdr.cast_mut();
                std::ptr::copy_nonoverlapping(
                    (new_len as u32).to_le_bytes().as_ptr(),
                    hdr_mut.add(8),
                    4,
                );
                extend_str_index(acc.cast_mut(), len_a, b_bytes, cap);
            }
            return acc.cast_mut();
        }
        // SAFETY: `acc`'s first `len_a` bytes are its content.
        let a_content = unsafe { std::slice::from_raw_parts(acc.cast::<u8>(), len_a) };
        let result = alloc_growable(&[a_content, b_bytes], (new_len * 2).max(64));
        // SAFETY: `acc` arrived as a consuming accumulator, so this call owns the share it
        // releases (C-ABI contract).
        unsafe { gos_rt_str_free(acc.cast_mut()) };
        return result;
    }

    // SAFETY: `acc` is a String argument from compiled code, null or a live string body for the whole call.
    let a_bytes: &[u8] = unsafe { gos_str_arg_bytes(acc) };
    let force_heap =
        crate::c_abi::rc::in_region_arena(acc.cast()) || crate::c_abi::rc::in_region_arena(b);
    let result = alloc_growable_forced(
        &[a_bytes, b_bytes],
        ((a_bytes.len() + len_b) * 2).max(64),
        force_heap,
    );
    if is_managed_string(acc) {
        // SAFETY: `acc` arrived as a consuming accumulator, so this call owns the share it
        // releases (C-ABI contract).
        unsafe { gos_rt_str_free(acc.cast_mut()) };
    }
    result
}

/// Writes `bytes` into an exclusively owned builder whose caller already
/// reserved enough capacity, then publishes the new length and terminator.
/// This is the internal bulk-writer path used by serializers that own the
/// builder for their entire lifetime. It avoids repeating ownership and
/// capacity checks for every small formatter fragment.
///
/// # Safety
///
/// `acc` must be a live, uniquely owned growable Gossamer string. `offset`
/// must equal its current length, and `offset + bytes.len()` must not exceed
/// its capacity.
pub(crate) unsafe fn str_builder_write_reserved(acc: *mut c_char, offset: usize, bytes: &[u8]) {
    let new_len = offset
        .checked_add(bytes.len())
        .expect("reserved string length overflow");
    // SAFETY: this `unsafe fn`'s caller passes `acc` a live string body.
    debug_assert!(unsafe { is_typed_builder(acc) });
    debug_assert!(u32::try_from(new_len).is_ok());
    // SAFETY: this `unsafe fn`'s caller passes `acc` a uniquely held builder whose reserved
    // capacity covers `offset + bytes.len()`.
    unsafe {
        let dst = acc.cast::<u8>().add(offset);
        copy_small_bytes(bytes.as_ptr(), dst, bytes.len());
        *dst.add(bytes.len()) = 0;
        let len_header = acc.cast::<u8>().sub(5);
        std::ptr::copy_nonoverlapping((new_len as u32).to_le_bytes().as_ptr(), len_header, 4);
        let cap_header = acc.cast::<u8>().sub(9);
        let cap = u32::from_le_bytes([
            *cap_header,
            *cap_header.add(1),
            *cap_header.add(2),
            *cap_header.add(3),
        ]) as usize;
        // `offset` is the builder's current length, so the index extends from
        // the fragment alone. Rescanning the whole buffer per fragment would
        // make a serializer quadratic in document size.
        extend_str_index(acc, offset, bytes, cap);
    }
}

/// Appends the decimal form of `n` straight onto growable string `acc`
/// and returns the (possibly reallocated) accumulator. The digits format
/// into a stack buffer, so the value reaches `acc` in a single copy - the
/// fused form of `acc += format!("{}", n)` that skips the concat buffer and
/// the throwaway result string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_append_i64(acc: *const c_char, n: i64) -> *mut c_char {
    // Bare + byte-counted: see gos_rt_str_concat_drop_a / gos_rt_str_append_bytes.
    // `itoa` formats into a stack buffer without the generic `fmt::Write`
    // machinery; this is the hot fused path for `s += format!("{}", i)`.
    let mut buf = itoa::Buffer::new();
    let digits = buf.format(n);
    // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
    // contract).
    unsafe { append_ascii_text(acc, digits.as_bytes()) }
}

/// Appends the ASCII `text` a scalar renders as onto `acc`: in place when an
/// exclusively held ASCII builder has room, through the general append
/// otherwise.
///
///
/// # Safety
///
/// As [`gos_rt_str_append_bytes`], with `text` ASCII.
#[inline]
unsafe fn append_ascii_text(acc: *const c_char, text: &[u8]) -> *mut c_char {
    // SAFETY: this `unsafe fn`'s caller passes `acc` null or a share it hands on.
    if unsafe { append_ascii_in_place(acc, &[text]) } {
        return acc.cast_mut();
    }
    // SAFETY: this `unsafe fn`'s caller passes `acc` null or a share it hands on, and `text` is a
    // live slice.
    unsafe { gos_rt_str_append_bytes(acc, text.as_ptr(), text.len() as i64) }
}

/// Appends the decimal form of the unsigned `n` onto growable string `acc`:
/// the `u64` counterpart of [`gos_rt_str_append_i64`], so a value at or above
/// 2^63 appends as its own magnitude.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_append_u64(acc: *const c_char, n: u64) -> *mut c_char {
    let mut buf = itoa::Buffer::new();
    let digits = buf.format(n);
    // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
    // contract).
    unsafe { append_ascii_text(acc, digits.as_bytes()) }
}

/// Appends `true` or `false` (`b` nonzero is `true`) onto growable string
/// `acc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_append_bool(acc: *const c_char, b: i32) -> *mut c_char {
    let text: &[u8] = if b == 0 { b"false" } else { b"true" };
    // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
    // contract).
    unsafe { append_ascii_text(acc, text) }
}

/// Appends the text `x.to_string()` answers for an `f64` straight onto
/// growable string `acc`, from [`crate::builtins::f64_display`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_append_f64(acc: *const c_char, x: f64) -> *mut c_char {
    let mut text = crate::builtins::FloatText::new();
    let digits = crate::builtins::f64_display(x, &mut text);
    // SAFETY: `acc` is this shim's accumulator argument, null or a share it hands on (C-ABI
    // contract).
    unsafe { append_ascii_text(acc, digits) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_trim(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract), and the
        // trimmed text is a window of it.
        unsafe { alloc_slice_cstring(s, st.trim().as_bytes()) }
    })
}

/// `s.trim_start() / strings::trim_start(s)` - strips leading
/// Unicode whitespace, mirroring Rust's `str::trim_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_trim_start(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract), and the
        // trimmed text is a window of it.
        unsafe { alloc_slice_cstring(s, st.trim_start().as_bytes()) }
    })
}

/// `s.trim_end() / strings::trim_end(s)` - strips trailing
/// Unicode whitespace, mirroring Rust's `str::trim_end`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_trim_end(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract), and the
        // trimmed text is a window of it.
        unsafe { alloc_slice_cstring(s, st.trim_end().as_bytes()) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_upper(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        let bytes = if s.is_null() {
            b"" as &[u8]
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_bytes(s) }
        };
        if bytes.is_ascii() {
            return alloc_ascii_upper_cstring(bytes);
        }
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        alloc_cstring(st.to_uppercase().as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_lower(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        let st = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        alloc_cstring(st.to_lowercase().as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_contains(s: *const c_char, needle: *const c_char) -> i32 {
    ffi_entry!({
        if s.is_null() || needle.is_null() {
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_bytes(s) };
        // SAFETY: `needle` is a String argument from compiled code, null or a live string body for the whole call.
        let n = unsafe { gos_str_arg_bytes(needle) };
        if n.is_empty() {
            return 1;
        }
        if s.len() < n.len() {
            return 0;
        }
        for i in 0..=(s.len() - n.len()) {
            if &s[i..i + n.len()] == n {
                return 1;
            }
        }
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_starts_with(s: *const c_char, prefix: *const c_char) -> i32 {
    ffi_entry!({
        if s.is_null() || prefix.is_null() {
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_bytes(s) };
        // SAFETY: `prefix` is a String argument from compiled code, null or a live string body for the whole call.
        let p = unsafe { gos_str_arg_bytes(prefix) };
        i32::from(s.starts_with(p))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_ends_with(s: *const c_char, suffix: *const c_char) -> i32 {
    ffi_entry!({
        if s.is_null() || suffix.is_null() {
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_bytes(s) };
        // SAFETY: `suffix` is a String argument from compiled code, null or a live string body for the whole call.
        let suf = unsafe { gos_str_arg_bytes(suffix) };
        i32::from(s.ends_with(suf))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_find(s: *const c_char, needle: *const c_char) -> i64 {
    ffi_entry!({
        if s.is_null() || needle.is_null() {
            return -1;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_bytes(s) };
        // SAFETY: `needle` is a String argument from compiled code, null or a live string body for the whole call.
        let n = unsafe { gos_str_arg_bytes(needle) };
        if n.is_empty() {
            return 0;
        }
        if s.len() < n.len() {
            return -1;
        }
        for i in 0..=(s.len() - n.len()) {
            if &s[i..i + n.len()] == n {
                // SAFETY: `s` is valid UTF-8 and `i` is where a match of the UTF-8 needle starts,
                // a char boundary.
                let prefix = unsafe { std::str::from_utf8_unchecked(&s[..i]) };
                return prefix.chars().count() as i64;
            }
        }
        -1
    })
}

/// `s.find(needle) -> Option<i64>` packed as a `*mut GosResult`
/// (`disc 0 = Some(idx)`, `disc 1 = None`). Wraps the raw i64
/// `gos_rt_str_find` return so cranelift's match-on-Option
/// lowering reads the right discriminant - the bare i64 form
/// produces a Value the SwitchInt path always treats as Some
/// because -1 doesn't correspond to either Some-disc (0) or
/// None-disc (1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_find_opt(s: *const c_char, needle: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `s`, `needle` are this shim's arguments, live for the call (C-ABI contract) or
        // null, which `gos_rt_str_find` accepts.
        let idx = unsafe { gos_rt_str_find(s, needle) };
        if idx < 0 {
            gos_rt_result_new(1, 0)
        } else {
            gos_rt_result_new(0, idx)
        }
    })
}

/// An `Option<i64>` carrier for a text position.
fn position_option(index: Option<usize>) -> i128 {
    match index.and_then(|i| i64::try_from(i).ok()) {
        Some(i) => gos_rt_result_new(0, i),
        None => gos_rt_result_new(1, 0),
    }
}

/// `strings::byte_find(text, needle) -> Option<i64>`: the byte offset of the
/// first match.
///
/// # Safety
/// `s` and `needle` are null or live string bodies for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_byte_find(s: *const c_char, needle: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's string arguments, null or live (C-ABI contract).
        let (text, needle) = unsafe { (gos_str_arg_text(s), gos_str_arg_text(needle)) };
        position_option(crate::codec::text::byte_find(text, needle))
    })
}

/// `strings::byte_rfind(text, needle) -> Option<i64>`: the byte offset of the
/// last match.
///
/// # Safety
/// `s` and `needle` are null or live string bodies for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_byte_rfind(s: *const c_char, needle: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: both are this shim's string arguments, null or live (C-ABI contract).
        let (text, needle) = unsafe { (gos_str_arg_text(s), gos_str_arg_text(needle)) };
        position_option(crate::codec::text::byte_rfind(text, needle))
    })
}

/// `strings::byte_offset(text, char_index) -> Option<i64>`.
///
/// # Safety
/// `s` is null or a live string body for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_byte_offset(s: *const c_char, char_index: i64) -> i128 {
    ffi_entry!({
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
        let text = unsafe { gos_str_arg_text(s) };
        position_option(
            usize::try_from(char_index)
                .ok()
                .and_then(|i| crate::codec::text::byte_offset(text, i)),
        )
    })
}

/// `strings::char_index(text, byte_offset) -> Option<i64>`.
///
/// # Safety
/// `s` is null or a live string body for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_char_index(s: *const c_char, byte_offset: i64) -> i128 {
    ffi_entry!({
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
        let text = unsafe { gos_str_arg_text(s) };
        position_option(
            usize::try_from(byte_offset)
                .ok()
                .and_then(|i| crate::codec::text::char_index(text, i)),
        )
    })
}

/// `s.to_i64() -> Option<i64>` packed as `{disc, payload}` (`disc 0 =
/// Some`, `disc 1 = None`). Strict full-string parse, no trimming.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_i64_opt(s: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        match parse_i64_bytes(unsafe { gos_str_arg_bytes(s) }) {
            Some(n) => gos_rt_result_new(0, n),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `str::parse::<i64>` over raw bytes: an optional `+` or `-`, then one or more
/// ASCII digits, with `None` on anything else or on overflow.
///
/// Every byte an integer accepts is ASCII, so the text needs no UTF-8 decoding
/// first: a byte outside ASCII fails the digit test exactly as its decoded
/// character fails `parse`.
fn parse_i64_bytes(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &b in digits {
        let d = i64::from(b.wrapping_sub(b'0'));
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?;
        n = if negative {
            n.checked_sub(d)?
        } else {
            n.checked_add(d)?
        };
    }
    Some(n)
}

/// `s.to_f64() -> Option<f64>`: the Some payload carries the value's
/// bits (`gos_rt_result_new_f64`), read back by the f64 payload path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_f64_opt(s: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_lossy(s) };
        match text.parse::<f64>() {
            Ok(f) => crate::c_abi::gos_rt_result_new_f64(0, f),
            Err(_) => gos_rt_result_new(1, 0),
        }
    })
}

/// `s.to_bool() -> Option<bool>`: accepts exactly `true` / `false`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_to_bool_opt(s: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_lossy(s) };
        match text.as_ref() {
            "true" => gos_rt_result_new(0, 1),
            "false" => gos_rt_result_new(0, 0),
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `s.rfind(needle) -> Option<i64>` packed as a `*mut GosResult`
/// (`disc 0 = Some(idx)`, `disc 1 = None`). UTF-8 bytes are searched from
/// the right, then the match is reported as a Unicode scalar offset.
/// `str::rfind` semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_rfind_opt(s: *const c_char, needle: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() || needle.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let hay = unsafe { gos_str_arg_bytes(s) };
        // SAFETY: `needle` is a String argument from compiled code, null or a live string body for the whole call.
        let n = unsafe { gos_str_arg_bytes(needle) };
        if n.is_empty() {
            // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
            return unsafe { gos_rt_result_new(0, typed_str_char_len(s) as i64) };
        }
        if hay.len() < n.len() {
            return gos_rt_result_new(1, 0);
        }
        let upper = hay.len() - n.len();
        for i in (0..=upper).rev() {
            if &hay[i..i + n.len()] == n {
                // SAFETY: `hay` is valid UTF-8 text and `i` is where a match of the UTF-8 needle
                // starts, a char boundary.
                let prefix = unsafe { std::str::from_utf8_unchecked(&hay[..i]) };
                return gos_rt_result_new(0, prefix.chars().count() as i64);
            }
        }
        gos_rt_result_new(1, 0)
    })
}

/// `s == t` for string operands. Compares byte-for-byte. NULL
/// pointers compare equal to empty strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_eq(a: *const c_char, b: *const c_char) -> bool {
    ffi_entry!({
        // SAFETY: `a` is a String argument from compiled code, null or a live string body for the whole call.
        unsafe { gos_str_arg_bytes(a) == gos_str_arg_bytes(b) }
    })
}

/// Lexicographic ordering of two C strings. Returns negative / zero /
/// positive like libc `strcmp`, but through Rust `Ord` so UTF-8 bytes
/// compare correctly. Used by the compiled tier for `a < b`, `a > b`,
/// etc. when both operands are `String` or `&String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_compare(a: *const c_char, b: *const c_char) -> i32 {
    ffi_entry!({
        let a = if a.is_null() {
            b""
        } else {
            // SAFETY: `a` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_bytes(a) }
        };
        let b = if b.is_null() {
            b""
        } else {
            // SAFETY: `b` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_bytes(b) }
        };
        match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_replace(
    s: *const c_char,
    from: *const c_char,
    to: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let f = if from.is_null() {
            ""
        } else {
            // SAFETY: `from` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(from) }
        };
        let t = if to.is_null() {
            ""
        } else {
            // SAFETY: `to` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(to) }
        };
        alloc_cstring(s.replace(f, t).as_bytes())
    })
}

/// `s.split_once(sep) -> Option<(String, String)>`. Returns a
/// `*mut GosResult` with `disc=0` holding a heap-allocated
/// `{a: *mut c_char, b: *mut c_char}` pair; `disc=1` for None
/// (separator not found, or null/empty input). Mirrors the
/// `find_opt` packing convention.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_split_once(s: *const c_char, sep: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() || sep.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let source = s;
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_text(s) };
        // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
        let sep = unsafe { gos_str_arg_text(sep) };
        match s.split_once(sep) {
            None => gos_rt_result_new(1, 0),
            Some((a, b)) => {
                #[repr(C)]
                struct Pair {
                    a: i64,
                    b: i64,
                }
                let pair = Box::into_raw(Box::new(Pair {
                    // SAFETY: `source` is this shim's live string argument, and `a` a window of
                    // it.
                    a: unsafe { alloc_slice_cstring(source, a.as_bytes()) } as i64,
                    // SAFETY: `source` is this shim's live string argument, and `b` a window of
                    // it.
                    b: unsafe { alloc_slice_cstring(source, b.as_bytes()) } as i64,
                }));
                gos_rt_result_new(0, pair as i64)
            }
        }
    })
}

/// `s.rsplit_once(sep) -> Option<(String, String)>`. Same shape as
/// `split_once` but anchored at the last occurrence of `sep`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_rsplit_once(s: *const c_char, sep: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() || sep.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let source = s;
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_text(s) };
        // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
        let sep = unsafe { gos_str_arg_text(sep) };
        match s.rsplit_once(sep) {
            None => gos_rt_result_new(1, 0),
            Some((a, b)) => {
                #[repr(C)]
                struct Pair {
                    a: i64,
                    b: i64,
                }
                let pair = Box::into_raw(Box::new(Pair {
                    // SAFETY: `source` is this shim's live string argument, and `a` a window of
                    // it.
                    a: unsafe { alloc_slice_cstring(source, a.as_bytes()) } as i64,
                    // SAFETY: `source` is this shim's live string argument, and `b` a window of
                    // it.
                    b: unsafe { alloc_slice_cstring(source, b.as_bytes()) } as i64,
                }));
                gos_rt_result_new(0, pair as i64)
            }
        }
    })
}

/// `s.count(needle) -> i64`. Counts non-overlapping occurrences.
/// Empty needle returns 0 (avoid the infinite "match between every
/// byte" that Rust's `matches("")` produces).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_count(s: *const c_char, needle: *const c_char) -> i64 {
    ffi_entry!({
        if s.is_null() || needle.is_null() {
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { gos_str_arg_text(s) };
        // SAFETY: `needle` is a String argument from compiled code, null or a live string body for the whole call.
        let n = unsafe { gos_str_arg_text(needle) };
        if n.is_empty() {
            return 0;
        }
        s.matches(n).count() as i64
    })
}

/// `s.strip_chars(cutset)` - trims any char in `cutset` from both
/// ends. Empty cutset is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_strip_chars(
    s: *const c_char,
    cutset: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let cutset = if cutset.is_null() {
            ""
        } else {
            // SAFETY: `cutset` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(cutset) }
        };
        if cutset.is_empty() {
            return alloc_cstring(s.as_bytes());
        }
        let pat: Vec<char> = cutset.chars().collect();
        alloc_cstring(s.trim_matches(pat.as_slice()).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_lstrip_chars(
    s: *const c_char,
    cutset: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let cutset = if cutset.is_null() {
            ""
        } else {
            // SAFETY: `cutset` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(cutset) }
        };
        if cutset.is_empty() {
            return alloc_cstring(s.as_bytes());
        }
        let pat: Vec<char> = cutset.chars().collect();
        alloc_cstring(s.trim_start_matches(pat.as_slice()).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_rstrip_chars(
    s: *const c_char,
    cutset: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let cutset = if cutset.is_null() {
            ""
        } else {
            // SAFETY: `cutset` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(cutset) }
        };
        if cutset.is_empty() {
            return alloc_cstring(s.as_bytes());
        }
        let pat: Vec<char> = cutset.chars().collect();
        alloc_cstring(s.trim_end_matches(pat.as_slice()).as_bytes())
    })
}

/// `s.zfill(width)` - pad with `'0'` on the left until at least
/// `width` characters wide.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_zfill(s: *const c_char, width: i64) -> *mut c_char {
    ffi_entry_passthrough!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        if width < 0 {
            crate::c_abi::panic::panic_text("strings::center: width must be non-negative");
        }
        if width == 0 {
            return alloc_cstring(s.as_bytes());
        }
        let cur = s.chars().count();
        let w = width as usize;
        if cur >= w {
            return alloc_cstring(s.as_bytes());
        }
        let mut out = String::with_capacity(w);
        for _ in 0..(w - cur) {
            out.push('0');
        }
        out.push_str(s);
        alloc_cstring(out.as_bytes())
    })
}

/// `s.center(width, pad_char)` - symmetric pad to `width`. Pads
/// with `' '` if `pad_char` is 0 (caller defaulted).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_center(
    s: *const c_char,
    width: i64,
    pad_char: i64,
) -> *mut c_char {
    ffi_entry!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        if width <= 0 {
            return alloc_cstring(s.as_bytes());
        }
        let cur = s.chars().count();
        let w = width as usize;
        if cur >= w {
            return alloc_cstring(s.as_bytes());
        }
        let pad = char::from_u32(pad_char as u32).unwrap_or(' ');
        let pad = if pad == '\0' { ' ' } else { pad };
        let total_pad = w - cur;
        let left_pad = total_pad / 2;
        let right_pad = total_pad - left_pad;
        let mut out = String::with_capacity(w * 4);
        for _ in 0..left_pad {
            out.push(pad);
        }
        out.push_str(s);
        for _ in 0..right_pad {
            out.push(pad);
        }
        alloc_cstring(out.as_bytes())
    })
}

/// `s.slice(start, end) -> Result<String, errors::Error>`. Byte offsets are
/// used, and mid-scalar bounds advance to the next UTF-8 boundary so a
/// successful result is always valid UTF-8. Result
/// payload pointers: `disc=0` → owned `*mut c_char`, `disc=1` →
/// `*mut GosError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_slice(s: *const c_char, start: i64, end: i64) -> i128 {
    ffi_entry!({
        let byte_len = if s.is_null() {
            0usize
        } else {
            // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `typed_str_len` accepts.
            unsafe { typed_str_len(s) }
        };
        let len_bytes = byte_len as i64;
        if start < 0 || end < 0 || start > end || end > len_bytes {
            // The bounds are byte offsets, so the length they are checked
            // against is the byte length.
            let msg = format!("slice: range [{start}, {end}) out of bounds for length {len_bytes}");
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            return gos_rt_result_new(1, err as i64);
        }
        let bytes: &[u8] = if s.is_null() {
            &[]
        } else {
            // SAFETY: `s` is non-null (checked above) and its first `byte_len` bytes are its
            // content.
            unsafe { std::slice::from_raw_parts(s.cast::<u8>(), byte_len) }
        };
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
        let lo = unsafe { typed_str_next_char_boundary(s, start as usize) }.unwrap_or(byte_len);
        // SAFETY: `s` is this shim's string argument, null or live (C-ABI contract).
        let hi = unsafe { typed_str_next_char_boundary(s, end as usize) }.unwrap_or(byte_len);
        gos_rt_result_new(0, alloc_cstring(&bytes[lo..hi]) as i64)
    })
}

/// Splits `s` on every occurrence of `sep` and returns a fresh
/// `*mut GosVec` of c-string pointers. Mirrors Rust's `str::split`
/// (and `gossamer_std::strings::split`): an empty separator yields
/// an empty leading field, one field per character, and an empty
/// trailing field. Each split slice gets its own heap-allocated
/// nul-terminated copy so the caller can hold them past the
/// underlying string's lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_split(s: *const c_char, sep: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let source = s;
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let sep = if sep.is_null() {
            ""
        } else {
            // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(sep) }
        };
        let parts: Vec<*mut c_char> = s
            .split(sep)
            // SAFETY: `source` is this shim's live string argument, and each piece a window of
            // it.
            .map(|p| unsafe { alloc_slice_cstring(source, p.as_bytes()) })
            .collect();
        // STRING-typed: the vec owns the pieces, so `gos_rt_vec_free`
        // reclaims them even when a consumer loop breaks early.
        let vec = {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                8,
                parts.len() as i64,
                crate::c_abi::vec::vec_elem_kind::STRING,
            )
        };
        for p in &parts {
            let pv = *p as i64;
            // SAFETY: `vec` is the live vec made above, and `pv` one 8-byte element.
            unsafe {
                gos_rt_vec_push(vec, std::ptr::addr_of!(pv).cast::<u8>());
            }
        }
        vec
    })
}

/// `strings::join(parts, sep) -> String`. Joins the c-string
/// pointers held in `parts` (a `*mut GosVec` of `*mut c_char`)
/// with `sep` between each pair. Empty Vec yields `""`. Null
/// element pointers contribute the empty string for that slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strings_join(
    parts: *const GosVec,
    sep: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        if parts.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `parts` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*parts };
        let sep_str = if sep.is_null() {
            ""
        } else {
            // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(sep) }
        };
        let len = vec.len.max(0) as usize;
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                out.push_str(sep_str);
            }
            // SAFETY: `i` is below the vec's length, so the element lies inside its buffer.
            let p = unsafe { vec.ptr.add(i * (vec.elem_bytes as usize)) };
            // Each element is a `*const c_char` stored as i64 in the
            // Vec slot (matches `gos_rt_str_split` / `gos_rt_str_lines`
            // packing).
            // SAFETY: `p` addresses one 8-byte element.
            let elem_ptr = unsafe { (p as *const i64).read_unaligned() } as *const c_char;
            if !elem_ptr.is_null() {
                // SAFETY: a non-null element of a `String` vec is a live string body.
                let s = unsafe { gos_str_arg_text(elem_ptr) };
                out.push_str(s);
            }
        }
        alloc_cstring(out.as_bytes())
    })
}

/// Reads element `i` of a scalar Vec at its declared stride: 1-byte
/// slots widen from `u8`, everything else reads the full 8-byte word.
unsafe fn vec_scalar_word(vec: &GosVec, i: usize) -> i64 {
    // SAFETY: this `unsafe fn`'s caller passes `i` below the vec's length.
    let p = unsafe { vec.ptr.add(i * (vec.elem_bytes as usize)) };
    if vec.elem_bytes == 1 {
        // SAFETY: `p` addresses one element.
        i64::from(unsafe { *p })
    } else {
        // SAFETY: `p` addresses one 8-byte element.
        unsafe { (p as *const i64).read_unaligned() }
    }
}

/// `xs.join(sep)` for an integer-element Vec: Display-render each
/// element, joined by `sep`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_join_i64(v: *const GosVec, sep: *const c_char) -> *mut c_char {
    ffi_entry!({
        if v.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let sep_str = if sep.is_null() {
            ""
        } else {
            // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(sep) }
        };
        let len = vec.len.max(0) as usize;
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                out.push_str(sep_str);
            }
            // SAFETY: `i` is below the vec's length.
            let n = unsafe { vec_scalar_word(vec, i) };
            out.push_str(&format!("{n}"));
        }
        alloc_cstring(out.as_bytes())
    })
}

/// `xs.join(sep)` for an f64-element Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_join_f64(v: *const GosVec, sep: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `v` and `sep` are this shim's arguments, each null or live (C-ABI contract).
        unsafe { join_float_elements(v, sep, |f, out| out.push_str(&format!("{f}"))) }
    })
}

/// `xs.join(sep)` for an f32-element Vec, each element in the digits of its
/// single-precision value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_join_f32(v: *const GosVec, sep: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `v` and `sep` are this shim's arguments, each null or live (C-ABI contract).
        unsafe {
            join_float_elements(v, sep, |f, out| {
                out.push_str(&crate::builtins::format_f32(f));
            })
        }
    })
}

/// Joins a float-element Vec's elements, each written by `render`.
unsafe fn join_float_elements(
    v: *const GosVec,
    sep: *const c_char,
    render: impl Fn(f64, &mut String),
) -> *mut c_char {
    if v.is_null() {
        return alloc_cstring(b"");
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live `Vec`.
    let vec = unsafe { &*v };
    let sep_str = if sep.is_null() {
        ""
    } else {
        // SAFETY: this `unsafe fn`'s caller passes `sep` live or null, which `gos_str_arg_text`
        // accepts.
        unsafe { gos_str_arg_text(sep) }
    };
    let len = vec.len.max(0) as usize;
    let mut out = String::new();
    for i in 0..len {
        if i > 0 {
            out.push_str(sep_str);
        }
        // SAFETY: `i` is below the vec's length.
        let bits = unsafe { vec_scalar_word(vec, i) };
        render(f64::from_bits(bits as u64), &mut out);
    }
    alloc_cstring(out.as_bytes())
}

/// `xs.join(sep)` for a bool-element Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_join_bool(v: *const GosVec, sep: *const c_char) -> *mut c_char {
    ffi_entry!({
        if v.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let sep_str = if sep.is_null() {
            ""
        } else {
            // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(sep) }
        };
        let len = vec.len.max(0) as usize;
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                out.push_str(sep_str);
            }
            // SAFETY: `i` is below the vec's length.
            let raw = unsafe { vec_scalar_word(vec, i) };
            out.push_str(if raw & 1 != 0 { "true" } else { "false" });
        }
        alloc_cstring(out.as_bytes())
    })
}

/// `xs.join(sep)` for a char-element Vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_join_char(v: *const GosVec, sep: *const c_char) -> *mut c_char {
    ffi_entry!({
        if v.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let sep_str = if sep.is_null() {
            ""
        } else {
            // SAFETY: `sep` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(sep) }
        };
        let len = vec.len.max(0) as usize;
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                out.push_str(sep_str);
            }
            // SAFETY: `i` is below the vec's length.
            let raw = unsafe { vec_scalar_word(vec, i) };
            let ch = char::from_u32(raw as u32).unwrap_or('\u{FFFD}');
            out.push(ch);
        }
        alloc_cstring(out.as_bytes())
    })
}

/// Splits `s` on `\n` and returns a fresh `*mut GosVec` of
/// c-string pointers, one per line. Trailing empty lines
/// (from `"a\nb\n"`) are dropped to mirror Rust's `lines()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_lines(s: *const c_char) -> *mut GosVec {
    ffi_entry!({
        let source = s;
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        let parts: Vec<*mut c_char> = s
            .lines()
            // SAFETY: `source` is this shim's live string argument, and each line a window of it.
            .map(|l| unsafe { alloc_slice_cstring(source, l.as_bytes()) })
            .collect();
        // STRING-typed - same ownership contract as `gos_rt_str_split`.
        let vec = {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                8,
                parts.len() as i64,
                crate::c_abi::vec::vec_elem_kind::STRING,
            )
        };
        for p in &parts {
            let pv = *p as i64;
            // SAFETY: `vec` is the live vec made above, and `pv` one 8-byte element.
            unsafe {
                gos_rt_vec_push(vec, std::ptr::addr_of!(pv).cast::<u8>());
            }
        }
        vec
    })
}

/// Append a Unicode codepoint to `s`, consuming the caller's reference and
/// returning the updated string. A uniquely owned growable string is mutated
/// in place when it has capacity; otherwise the shared or exhausted buffer is
/// replaced through the same copy-on-write growth path as `push_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_push_char(s: *const c_char, c: i32) -> *mut c_char {
    ffi_entry!({
        let ch = char::from_u32(c as u32).unwrap_or('\u{FFFD}');
        let mut encoded = [0u8; 4];
        let bytes = ch.encode_utf8(&mut encoded).as_bytes();
        // SAFETY: `s` is this shim's accumulator argument, null or a share it hands on (C-ABI
        // contract).
        unsafe { gos_rt_str_append_bytes(s, bytes.as_ptr(), bytes.len() as i64) }
    })
}

/// Append a byte as its Unicode codepoint, consuming the caller's reference
/// with the same in-place/copy-on-write contract as [`gos_rt_str_push_char`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_push_byte(s: *const c_char, b: i32) -> *mut c_char {
    ffi_entry!({
        let ch = char::from(b as u8);
        let mut encoded = [0u8; 2];
        let bytes = ch.encode_utf8(&mut encoded).as_bytes();
        // SAFETY: `s` is this shim's accumulator argument, null or a share it hands on (C-ABI
        // contract).
        unsafe { gos_rt_str_append_bytes(s, bytes.as_ptr(), bytes.len() as i64) }
    })
}

/// Returns `s` repeated `n` times. Rust's `String::repeat`
/// semantics: `n=0` returns the empty string, `n=1` returns a
/// fresh copy.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_repeat(s: *const c_char, n: i64) -> *mut c_char {
    ffi_entry_passthrough!({
        let s = if s.is_null() {
            ""
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { gos_str_arg_text(s) }
        };
        if n < 0 {
            crate::c_abi::panic::panic_text("strings::repeat: count must be non-negative");
        }
        let n = n as usize;
        if s.len().checked_mul(n).is_none() {
            crate::c_abi::panic::panic_text("string repeat capacity overflow");
        }
        alloc_cstring(s.repeat(n).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_parse_i64(s: *const c_char, ok_out: *mut i32) -> i64 {
    ffi_entry!({
        if s.is_null() {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 0 };
            }
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_text(s) }.trim();
        if let Ok(n) = text.parse::<i64>() {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 1 };
            }
            n
        } else {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 0 };
            }
            0
        }
    })
}

/// `text.parse::<i64>()` returning a `Result<i64, errors::Error>`.
/// Err payload is a `*mut GosError` so user code can call
/// `e.message()` directly without `map_err`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_parse_i64_result(s: *const c_char) -> i128 {
    ffi_entry!({
        if s.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes(b"parse: null input");
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_text(s) }.trim();
        if let Ok(n) = text.parse::<i64>() {
            gos_rt_result_new(0, n)
        } else {
            let msg = format!(
                "unexpected byte 0x{:x} at 1:1",
                text.as_bytes().first().copied().unwrap_or(0)
            );
            let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
            gos_rt_result_new(1, err as i64)
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_parse_f64(s: *const c_char, ok_out: *mut i32) -> f64 {
    ffi_entry!({
        if s.is_null() {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 0 };
            }
            return 0.0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_text(s) }.trim();
        if let Ok(x) = text.parse::<f64>() {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 1 };
            }
            x
        } else {
            if !ok_out.is_null() {
                // SAFETY: `ok_out` is non-null (checked above) and this shim's out-slot (C-ABI
                // contract).
                unsafe { *ok_out = 0 };
            }
            0.0
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_i64_to_str(n: i64) -> *mut c_char {
    ffi_entry!({
        let mut digits = [0u8; 20];
        alloc_cstring(i64_digits(n, &mut digits))
    })
}

/// The decimal text of `n`, written into `out` and answered as the filled
/// part. The widest `i64` is 20 bytes with its sign, so the caller's buffer
/// is always large enough and the number reaches its string in one
/// allocation rather than through a `String` that is then copied.
fn i64_digits(n: i64, out: &mut [u8; 20]) -> &[u8] {
    if n == 0 {
        out[0] = b'0';
        return &out[..1];
    }
    // Negating in the unsigned domain so `i64::MIN` has a magnitude.
    let negative = n < 0;
    let mut magnitude = n.unsigned_abs();
    let mut end = out.len();
    while magnitude > 0 {
        end -= 1;
        out[end] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
    }
    if negative {
        end -= 1;
        out[end] = b'-';
    }
    &out[end..]
}

/// Stringifies an *unsigned* 64-bit integer. Distinct from
/// `gos_rt_i64_to_str` so values `>= 2^63` print as their true
/// magnitude rather than a leading-`-` two's-complement view.
/// Used by the cranelift + LLVM lowerers when the source TyKind
/// resolves to `u8/u16/u32/u64/u128/usize`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_u64_to_str(n: u64) -> *mut c_char {
    ffi_entry!({ alloc_cstring(n.to_string().as_bytes()) })
}

/// `x.to_string()` for an `f64`: [`crate::builtins::f64_display`]'s text in
/// one allocation. Nothing here can unwind, so it carries no panic guard.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f64_to_str(x: f64) -> *mut c_char {
    let mut text = crate::builtins::FloatText::new();
    alloc_ascii_cstring(crate::builtins::f64_display(x, &mut text))
}

/// `x.to_string()` for an `f32`: the shortest digits that read back as the
/// single-precision value its double-width slot holds.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f32_to_str(x: f64) -> *mut c_char {
    ffi_entry!({ alloc_cstring(crate::builtins::format_f32(x).as_bytes()) })
}

/// `{:?}` of an `f32`: [`gos_rt_f32_to_str`]'s digits, keeping a fractional
/// part or an exponent so the text reads back as a float.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f32_debug_to_str(x: f64) -> *mut c_char {
    ffi_entry!({ alloc_cstring(crate::builtins::format_f32_debug(x).as_bytes()) })
}

/// Stringifies an `f64` with `prec` fractional digits - the runtime
/// side of `format!("{:.N}", x)`. Routes through the Rust standard
/// library's float formatter so rounding matches the interpreter's
/// `{:.N}` Display output bit-for-bit. Very large `prec` is clamped
/// to a sane upper bound to keep the allocation bounded.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_f64_prec_to_str(x: f64, prec: i64) -> *mut c_char {
    ffi_entry_passthrough!({
        if prec < 0 {
            crate::c_abi::panic::panic_text("__fmt_prec: precision must be non-negative");
        }
        let prec = prec.min(64) as usize;
        alloc_cstring(format!("{x:.prec$}").as_bytes())
    })
}

/// Truncates `s` to its first `prec` Unicode scalars, which is what a
/// `{:.N}` spec asks of text: precision bounds how much of a value is
/// shown, and a string's length is counted in scalars everywhere else.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_str_prec_to_str(s: *const c_char, prec: i64) -> *mut c_char {
    ffi_entry_passthrough!({
        if prec < 0 {
            crate::c_abi::panic::panic_text("__fmt_prec: precision must be non-negative");
        }
        // SAFETY: `s` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `typed_str_bytes` accepts.
        let bytes = unsafe { typed_str_bytes(s) };
        let text = String::from_utf8_lossy(bytes);
        let taken: String = text.chars().take(prec as usize).collect();
        alloc_cstring(taken.as_bytes())
    })
}

/// `s.push_utf8(buf, start, end) -> bool` - appends the `[start, end)` byte
/// window of `buf` to `s` when that window is valid UTF-8.
///
/// The window is appended in place through the growable-string path, so
/// rendering text out of a byte buffer costs neither an intermediate `Vec`
/// nor an intermediate `String`. An out-of-range or non-UTF-8 window appends
/// nothing.
///
/// Answers a two-word carrier: `Ok` when the window was appended, `Err` when
/// it was not, and the payload is the string pointer the receiver takes on
/// either way.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_str_push_utf8(
    s: *const c_char,
    buf: *const crate::c_abi::vec::GosVec,
    start: i64,
    end: i64,
) -> i128 {
    // An ASCII window is valid UTF-8 and keeps an ASCII builder's index, so
    // it is appended with one scan and one copy ahead of the general path.
    // SAFETY: `buf` is this shim's byte vec argument, null or live (C-ABI contract).
    if let Some(window) = unsafe { packed_byte_window(buf, start, end) }
        && window.is_ascii()
        // SAFETY: `s` is this shim's accumulator argument, null or live (C-ABI contract).
        && unsafe { append_ascii_in_place(s, &[window]) }
    {
        return crate::c_abi::result::gos_rt_result_new(0, s as i64);
    }
    ffi_entry!({
        let unchanged =
            |ok: bool| crate::c_abi::result::gos_rt_result_new(i64::from(!ok), s as i64);
        if buf.is_null() || start < 0 || end < start {
            return unchanged(false);
        }
        let (lo, hi) = (start as usize, end as usize);
        // A packed buffer is read where it lies; a buffer whose slots are
        // wider than a byte has its window gathered - the window, not the
        // buffer, so appending a record out of a large file costs the record.
        // SAFETY: `buf` is this shim's byte vec argument, null or live (C-ABI contract).
        let Some(bytes) = (unsafe { crate::c_abi::vec::vec_bytes_window(buf, lo, hi) }) else {
            return unchanged(false);
        };
        if lo == hi {
            return unchanged(true);
        }
        let window = &bytes[..];
        if std::str::from_utf8(window).is_err() {
            return unchanged(false);
        }
        // SAFETY: `s` is this shim's accumulator argument, null or a share it hands on (C-ABI
        // contract).
        let appended = unsafe { gos_rt_str_append_bytes(s, window.as_ptr(), (hi - lo) as i64) };
        crate::c_abi::result::gos_rt_result_new(0, appended as i64)
    })
}

/// Appends `parts`, in order, onto growable string `acc` in one reservation
/// and answers the (possibly reallocated) accumulator. `ascii` says the caller
/// has proven every part ASCII, which spares the character index a scan.
///
/// # Safety
/// `acc` is a live Gossamer string the caller owns and hands over, as for
/// [`gos_rt_str_append_bytes`].
pub(crate) unsafe fn str_append_parts(
    acc: *const c_char,
    parts: &[&[u8]],
    ascii: bool,
) -> *mut c_char {
    let added: usize = parts.iter().map(|p| p.len()).sum();
    if added == 0 {
        // SAFETY: this `unsafe fn`'s caller passes `acc` null or a share it hands on.
        return unsafe { concat_with_empty(acc) };
    }
    // SAFETY: this `unsafe fn`'s caller passes `acc` null or live, which the probe accepts.
    if unsafe { is_typed_builder(acc) } {
        // SAFETY: `acc` is a typed builder (checked above), so its 13-byte header of count,
        // capacity, length, and tag precedes the body.
        let hdr = unsafe { acc.cast::<u8>().sub(13) };
        // SAFETY: `hdr` addresses the header's count bytes.
        let rc = u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] });
        let cap =
            // SAFETY: `hdr` addresses the header's capacity bytes.
            u32::from_le_bytes(unsafe { [*hdr.add(4), *hdr.add(5), *hdr.add(6), *hdr.add(7)] })
                as usize;
        let len_a =
            // SAFETY: `hdr` addresses the header's length bytes.
            u32::from_le_bytes(unsafe { [*hdr.add(8), *hdr.add(9), *hdr.add(10), *hdr.add(11)] })
                as usize;
        if len_a + added <= cap && rc == 1 {
            // SAFETY: the builder is solely owned, and its content, terminator,
            // and index footer all lie within the `cap` checked above.
            unsafe {
                let dst = acc.cast_mut().cast::<u8>().add(len_a);
                let mut at = 0;
                for part in parts {
                    copy_small_bytes(part.as_ptr(), dst.add(at), part.len());
                    at += part.len();
                }
                *dst.add(added) = 0;
                std::ptr::copy_nonoverlapping(
                    ((len_a + added) as u32).to_le_bytes().as_ptr(),
                    hdr.cast_mut().add(8),
                    4,
                );
                let footer = acc.cast::<u8>().add(cap + 1).cast::<u32>();
                if !(ascii && footer.read_unaligned() == STR_INDEX_ASCII) {
                    let written = std::slice::from_raw_parts(dst, added);
                    extend_str_index(acc.cast_mut(), len_a, written, cap);
                }
            }
            return acc.cast_mut();
        }
        // SAFETY: `acc`'s first `len_a` bytes are its content.
        let a_content = unsafe { std::slice::from_raw_parts(acc.cast::<u8>(), len_a) };
        let mut all: Vec<&[u8]> = Vec::with_capacity(parts.len() + 1);
        all.push(a_content);
        all.extend_from_slice(parts);
        let result = alloc_growable(&all, ((len_a + added) * 2).max(64));
        // SAFETY: `acc` arrived as a consuming accumulator, so this call owns the share it
        // releases.
        unsafe { gos_rt_str_free(acc.cast_mut()) };
        return result;
    }
    // SAFETY: this `unsafe fn`'s caller passes `acc` live or null, which `gos_str_arg_bytes`
    // accepts.
    let a_bytes: &[u8] = unsafe { gos_str_arg_bytes(acc) };
    let force_heap = crate::c_abi::rc::in_region_arena(acc.cast())
        || parts
            .iter()
            .any(|p| crate::c_abi::rc::in_region_arena(p.as_ptr()));
    let mut all: Vec<&[u8]> = Vec::with_capacity(parts.len() + 1);
    all.push(a_bytes);
    all.extend_from_slice(parts);
    let result = alloc_growable_forced(&all, ((a_bytes.len() + added) * 2).max(64), force_heap);
    if is_managed_string(acc) {
        // SAFETY: `acc` arrived as a consuming accumulator, so this call owns the share it
        // releases.
        unsafe { gos_rt_str_free(acc.cast_mut()) };
    }
    result
}

/// Stringifies a bool (passed as i32: nonzero = true). Used by
/// codegen to assemble multi-arg panic / format-style messages.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bool_to_str(b: i32) -> *mut c_char {
    ffi_entry!({ alloc_cstring(if b == 0 { b"false" } else { b"true" }) })
}

/// Stringifies a char (passed as i32 Unicode scalar) into a freshly
/// heap-allocated UTF-8 c-string. Invalid scalars (surrogates,
/// > U+10FFFF) render as `\u{FFFD}` (REPLACEMENT CHARACTER).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_char_to_str(c: i32) -> *mut c_char {
    ffi_entry!({
        let scalar = u32::try_from(c)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}');
        let mut buf = [0u8; 4];
        let s = scalar.encode_utf8(&mut buf);
        alloc_cstring(s.as_bytes())
    })
}

#[cfg(test)]
mod byte_compare_tests {
    use super::bytes_eq;

    /// The short-slice comparison answers exactly what the standard one does,
    /// at every length either path can take and at every byte position.
    #[test]
    fn bytes_eq_agrees_with_slice_equality_at_every_length() {
        for len in 0..=200usize {
            let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert!(bytes_eq(&a, &a.clone()), "equal at len {len}");
            assert_eq!(bytes_eq(&a, &a.clone()), a == a.clone());

            for pos in 0..len {
                let mut b = a.clone();
                b[pos] ^= 0x80;
                assert!(!bytes_eq(&a, &b), "byte {pos} of {len} differs");
                assert_eq!(bytes_eq(&a, &b), a == b);
            }
            if len > 0 {
                let shorter = &a[..len - 1];
                assert!(!bytes_eq(&a, shorter), "length differs at {len}");
            }
        }
    }

    /// An empty slice equals an empty slice and nothing longer.
    #[test]
    fn bytes_eq_handles_the_empty_slice() {
        assert!(bytes_eq(b"", b""));
        assert!(!bytes_eq(b"", b"a"));
        assert!(!bytes_eq(b"a", b""));
    }
}

#[cfg(test)]
mod ascii_index_tests {
    use super::{
        alloc_cstring_from_slices, gos_rt_str_concat, gos_rt_str_free, gos_rt_str_substring,
        typed_str_char_len, typed_str_is_ascii,
    };

    /// Concatenation and slicing of ASCII strings record the result as ASCII,
    /// and any non-ASCII operand leaves an index that counts characters.
    #[test]
    fn ascii_operands_keep_an_ascii_index_and_others_count_characters() {
        let a = alloc_cstring_from_slices(&[b"abc"]);
        let b = alloc_cstring_from_slices(&[b"de"]);
        let wide = alloc_cstring_from_slices(&["\u{e9}t\u{e9}".as_bytes()]);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            assert!(typed_str_is_ascii(a) && typed_str_is_ascii(b));
            let ab = gos_rt_str_concat(a, b);
            assert!(typed_str_is_ascii(ab));
            assert_eq!(typed_str_char_len(ab), 5);
            let slice = gos_rt_str_substring(ab, 1, 4);
            assert!(typed_str_is_ascii(slice));
            assert_eq!(super::typed_str_bytes(slice), b"bcd");
            let mixed = gos_rt_str_concat(a, wide);
            assert!(!typed_str_is_ascii(mixed));
            assert_eq!(typed_str_char_len(mixed), 6);
            let wide_slice = gos_rt_str_substring(wide, 0, 3);
            assert!(!typed_str_is_ascii(wide_slice));
            assert_eq!(typed_str_char_len(wide_slice), 2);
            for s in [a, b, wide, ab, slice, mixed, wide_slice] {
                gos_rt_str_free(s);
            }
        }
    }
}

#[cfg(test)]
mod parse_i64_bytes_tests {
    use super::parse_i64_bytes;

    #[test]
    fn byte_parse_answers_what_str_parse_answers() {
        let cases = [
            "",
            "0",
            "7",
            "-0",
            "+0",
            "+",
            "-",
            "--1",
            "+-1",
            "12a",
            " 12",
            "12 ",
            "00042",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775808",
            "-9223372036854775809",
            "99999999999999999999",
            "1_000",
            "\u{0661}",
            "é1",
            "٣",
        ];
        for case in cases {
            assert_eq!(
                parse_i64_bytes(case.as_bytes()),
                case.parse::<i64>().ok(),
                "{case:?}"
            );
        }
        assert_eq!(parse_i64_bytes(&[b'1', 0xff]), None);
    }
}

#[cfg(test)]
mod char_index_tests {
    /// Text whose characters take one to four bytes, `len` of them, laid out
    /// so block boundaries fall on every width.
    fn mixed_text(len: usize) -> String {
        ['a', 'é', '€', '😀']
            .iter()
            .cycle()
            .skip(len % 4)
            .take(len)
            .collect()
    }

    #[test]
    fn a_slice_of_indexed_text_finds_every_character() {
        for len in [0, 1, 7, 8, 9, 31, 32, 33, 63, 64, 65, 200, 1000] {
            let text = mixed_text(len);
            let starts: Vec<usize> = text
                .char_indices()
                .map(|(at, _)| at)
                .chain(std::iter::once(text.len()))
                .collect();
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe {
                let source = super::alloc_cstring(text.as_bytes());
                for (from, to) in [(0, len), (1.min(len), len), (0, len / 2), (len / 3, len)] {
                    let piece = &text.as_bytes()[starts[from]..starts[to]];
                    let slice = super::alloc_slice_cstring(source, piece);
                    assert_eq!(super::typed_str_char_len(slice), to - from, "len {len}");
                    for (index, &at) in starts[from..=to].iter().enumerate() {
                        assert_eq!(
                            super::typed_str_char_boundary(slice, index),
                            Some(at - starts[from]),
                            "len {len} slice {from}..{to} char {index}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_cut_through_a_character_is_not_whole() {
        let text = "aé€😀";
        assert!(super::utf8_slice_is_whole(text.as_bytes()));
        assert!(super::utf8_slice_is_whole(&text.as_bytes()[1..3]));
        assert!(!super::utf8_slice_is_whole(&text.as_bytes()[2..]));
        assert!(!super::utf8_slice_is_whole(
            &text.as_bytes()[..text.len() - 1]
        ));
        assert!(super::utf8_slice_is_whole(b""));
    }
}

#[cfg(test)]
mod miri_core_tests {
    use super::*;

    /// The text of a runtime string, for asserting on a shim's answer.
    fn text_of(s: *const c_char) -> String {
        // SAFETY: the shims under test answer live string bodies.
        unsafe { crate::c_abi::gos_str_arg_string(s) }
    }

    #[test]
    fn appends_grow_a_builder_and_keep_its_character_index() {
        let mut acc = gos_rt_str_with_capacity(4);
        let mut expect = String::new();
        for (i, piece) in ["ab", "cé", "∑", "xyz", "日本"]
            .iter()
            .cycle()
            .take(40)
            .enumerate()
        {
            // SAFETY: `acc` is the builder this test holds, whose share the append consumes and
            // answers; `piece` is live for the call.
            acc = unsafe {
                gos_rt_str_append_bytes(acc, piece.as_ptr(), i64::try_from(piece.len()).unwrap())
            };
            expect.push_str(piece);
            if i % 7 == 0 {
                let n = expect.chars().count() as i64;
                // SAFETY: `acc` is the live builder.
                assert_eq!(unsafe { gos_rt_str_len(acc) }, n);
                // SAFETY: `acc` is the live builder and `n - 1` is its last character.
                let last = unsafe { gos_rt_str_char_at(acc, n - 1) };
                assert_eq!(char::from_u32(last as u32), expect.chars().last());
            }
        }
        assert_eq!(text_of(acc), expect);
        // SAFETY: `acc` is the builder this test holds, freed once.
        unsafe { gos_rt_str_free(acc) };
    }

    #[test]
    fn slices_and_concatenations_are_strings_of_their_own() {
        let whole = alloc_cstring("héllo wörld".as_bytes());
        let slice = |lo: i64, hi: i64| -> *mut c_char {
            // SAFETY: `whole` is the live string made above; the answer is a `Result` whose `Ok`
            // payload is a fresh string.
            let answer = unsafe { gos_rt_str_slice(whole, lo, hi) };
            assert_eq!(crate::c_abi::result::gos_rt_result_disc(answer), 0);
            crate::c_abi::result::gos_rt_result_payload(answer) as *mut c_char
        };
        let (left, right) = (slice(0, 6), slice(7, 13));
        // SAFETY: `whole` is the live string this test owns, freed once.
        unsafe { gos_rt_str_free(whole) };
        // SAFETY: `left` and `right` are live strings; the answer is a fresh one.
        let joined = unsafe { gos_rt_str_concat(left, right) };
        assert_eq!(text_of(joined), "héllowörld");
        // SAFETY: each is a live string this test owns, freed once.
        unsafe {
            gos_rt_str_free(left);
            gos_rt_str_free(right);
            gos_rt_str_free(joined);
        }
    }
}
