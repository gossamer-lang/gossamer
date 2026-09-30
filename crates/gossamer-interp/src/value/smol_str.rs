//! `SmolStr`: the VM's string, inline up to seven bytes and shared beyond.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Tagged-pointer string with 7-byte inline storage (B2).
///
/// **Encoding.** A single 8-byte word `raw`. The high bit
/// distinguishes inline from heap:
/// - `raw >> 63 == 0`: inline. The low 7 bytes hold UTF-8 content
///   (little-endian); the eighth byte (byte index 7, the high
///   byte) holds the length in `0..=7`.
/// - `raw >> 63 == 1`: heap. The low 63 bits hold a pointer
///   produced by the thin RC byte-buffer allocator. On
///   `x86_64` / aarch64, user-space pointers fit in 48 bits, so
///   masking the high bit is lossless.
///
/// **Why this matters.** Without SSO, every `Value::String(SmolStr::from("Ok"))`
/// allocates a `String` on the heap *and* an `Arc` header (~32
/// bytes total). Variant names like `"Ok"` / `"Err"` / `"Some"`
/// / `"None"`, single-char field names, and most stack tags fit
/// in 7 bytes - so a steady-state hot loop now does zero heap
/// allocation for those values.
///
/// **Safety.** All pointer arithmetic is contained in this type.
/// `Drop` and `Clone` decrement / increment the underlying heap string
/// only when the heap tag is set; inline values are pure `u64`
/// values that don't own anything. The unsafe block in
/// `as_str` casts the storage to `&[u8]`; the bytes are
/// guaranteed UTF-8 because `from_str` only stores valid UTF-8
/// inline.
pub struct SmolStr {
    raw: u64,
}

const SMOL_HEAP_TAG: u64 = 1u64 << 63;
const SMOL_PTR_MASK: u64 = !SMOL_HEAP_TAG;
const SMOL_INLINE_MAX: usize = 7;

#[repr(C)]
struct HeapSmolStr {
    strong: AtomicU32,
    len: u32,
    cap: u32,
    char_len: u32,
    char_index: Vec<u32>,
}

const SMOL_CHAR_INDEX_STRIDE: usize = 32;

fn smol_char_index(s: &str) -> Vec<u32> {
    s.char_indices()
        .enumerate()
        .filter_map(|(char_index, (byte_index, _))| {
            (char_index % SMOL_CHAR_INDEX_STRIDE == 0).then_some(byte_index as u32)
        })
        .collect()
}

impl HeapSmolStr {
    fn layout(cap: usize) -> Layout {
        let header = Layout::new::<Self>();
        let bytes = Layout::array::<u8>(cap).expect("SmolStr heap layout overflow");
        header
            .extend(bytes)
            .expect("SmolStr heap layout overflow")
            .0
            .pad_to_align()
    }

    fn alloc_with_capacity(bytes: &[u8], cap: usize) -> *const Self {
        debug_assert!(cap >= bytes.len());
        Self::alloc_with_fill(bytes.len(), cap, |dst| {
            // SAFETY: `alloc_with_fill` passes `bytes.len()` writable payload
            // bytes; the fresh allocation cannot overlap the source slice.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            }
        })
    }

    fn alloc_ascii_upper(bytes: &[u8]) -> *const Self {
        Self::alloc_with_fill(bytes.len(), bytes.len(), |dst| {
            for (i, &b) in bytes.iter().enumerate() {
                let upper = if b.is_ascii_lowercase() {
                    b - (b'a' - b'A')
                } else {
                    b
                };
                // SAFETY: `alloc_with_fill` passes `bytes.len()` writable
                // payload bytes and this loop writes each byte exactly once.
                unsafe {
                    *dst.add(i) = upper;
                }
            }
        })
    }

    #[allow(
        clippy::cast_ptr_alignment,
        reason = "alloc uses HeapSmolStr::layout, whose alignment is at least HeapSmolStr's alignment"
    )]
    fn alloc_with_fill<F>(len: usize, cap: usize, fill: F) -> *const Self
    where
        F: FnOnce(*mut u8),
    {
        let len_u32 = u32::try_from(len).expect("SmolStr heap string too large");
        let cap_u32 = u32::try_from(cap).expect("SmolStr heap string too large");
        let layout = Self::layout(cap);
        // SAFETY: `layout` is non-zero and was computed for the header +
        // payload.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        let header = ptr.cast::<Self>();
        // SAFETY: `header` points to a fresh allocation large enough for the
        // header plus `len` payload bytes.
        unsafe {
            header.write(Self {
                strong: AtomicU32::new(1),
                len: len_u32,
                cap: cap_u32,
                char_len: 0,
                char_index: Vec::new(),
            });
            fill(Self::bytes_mut(header));
            let text = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                Self::bytes_ptr(header),
                len,
            ));
            (*header).char_len = text.chars().count() as u32;
            (*header).char_index = smol_char_index(text);
        }
        header
    }

    unsafe fn bytes_ptr(header: *const Self) -> *const u8 {
        // SAFETY: caller guarantees `header` points to a valid `HeapSmolStr`.
        unsafe { header.cast::<u8>().add(std::mem::size_of::<Self>()) }
    }

    unsafe fn bytes_mut(header: *mut Self) -> *mut u8 {
        // SAFETY: caller guarantees `header` points to a valid mutable allocation.
        unsafe { header.cast::<u8>().add(std::mem::size_of::<Self>()) }
    }

    unsafe fn as_str<'a>(header: *const Self) -> &'a str {
        // SAFETY: caller guarantees `header` is live. Payload bytes came
        // from a `str`/`String`, so they are valid UTF-8.
        let len = unsafe { (*header).len as usize };
        let bytes = unsafe { std::slice::from_raw_parts(Self::bytes_ptr(header), len) };
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    unsafe fn inc(header: *const Self) {
        // SAFETY: caller owns a live strong reference to `header`.
        let prev = unsafe { (*header).strong.fetch_add(1, Ordering::Relaxed) };
        assert!(prev != u32::MAX, "SmolStr refcount overflow");
    }

    unsafe fn is_unique(header: *const Self) -> bool {
        // SAFETY: caller owns a live strong reference to `header`.
        unsafe { (*header).strong.load(Ordering::Acquire) == 1 }
    }

    unsafe fn append_unique(header: *mut Self, bytes: &[u8]) {
        // SAFETY: caller guarantees unique ownership and enough capacity.
        let len = unsafe { (*header).len as usize };
        let cap = unsafe { (*header).cap as usize };
        debug_assert!(len + bytes.len() <= cap);
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                Self::bytes_mut(header).add(len),
                bytes.len(),
            );
            (*header).len =
                u32::try_from(len + bytes.len()).expect("SmolStr heap string too large");
            let suffix = std::str::from_utf8_unchecked(bytes);
            let old_char_len = (*header).char_len as usize;
            let mut added_chars = 0usize;
            for (byte_offset, _) in suffix.char_indices() {
                let char_index = old_char_len + added_chars;
                if char_index.is_multiple_of(SMOL_CHAR_INDEX_STRIDE) {
                    (*header)
                        .char_index
                        .push(u32::try_from(len + byte_offset).expect("string too large"));
                }
                added_chars += 1;
            }
            (*header).char_len =
                u32::try_from(old_char_len + added_chars).expect("SmolStr heap string too large");
        }
    }

    unsafe fn dec(header: *const Self) {
        // SAFETY: caller owns one strong reference to `header`.
        if unsafe { (*header).strong.fetch_sub(1, Ordering::Release) } == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            let cap = unsafe { (*header).cap as usize };
            let layout = Self::layout(cap);
            // SAFETY: this is the final strong reference, so no other
            // thread can access the allocation after the release/acquire pair.
            unsafe {
                std::ptr::drop_in_place(header.cast_mut());
                dealloc(header.cast::<u8>().cast_mut(), layout);
            }
        }
    }
}

impl SmolStr {
    /// Empty string (inline, len 0).
    #[must_use]
    pub const fn new() -> Self {
        Self { raw: 0 }
    }

    /// Constructs an empty string with space reserved for at least `capacity`
    /// UTF-8 bytes.  Small hints stay inline; larger hints allocate the same
    /// thin, copy-on-write buffer used by appended strings so a mutable VM
    /// `String` can consume the reservation without reallocating.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity <= SMOL_INLINE_MAX {
            Self::new()
        } else {
            Self::new_heap_with_capacity(&[], capacity)
        }
    }

    /// Constructs a [`SmolStr`] from a borrowed `&str`. Strings
    /// up to 7 bytes are stored inline; longer strings allocate
    /// a fresh thin RC byte buffer.
    ///
    /// Intentionally not the [`std::str::FromStr`] trait method -
    /// `FromStr` returns `Result` to model fallible parsing and
    /// this conversion is infallible. Implementing the trait
    /// would force callers to `.unwrap()` an `Ok`-only path.
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "infallible conversion; FromStr would force callers to .unwrap()"
    )]
    pub fn from_str(s: &str) -> Self {
        if s.len() <= SMOL_INLINE_MAX {
            Self::new_inline(s.as_bytes())
        } else {
            Self::new_heap(s.as_bytes())
        }
    }

    /// Constructs a [`SmolStr`] from an owned [`String`]. Avoids
    /// re-allocating for inline-fitting strings; heap-bound strings
    /// move their bytes into the thin RC buffer.
    #[must_use]
    pub fn from_string(s: String) -> Self {
        if s.len() <= SMOL_INLINE_MAX {
            Self::new_inline(s.as_bytes())
        } else {
            Self::new_heap(s.as_bytes())
        }
    }

    /// Constructs an uppercase string, using a byte-wise fast path for ASCII
    /// and Rust's Unicode expansion for non-ASCII.
    #[must_use]
    pub fn to_uppercase_from(s: &str) -> Self {
        if !s.is_ascii() {
            return Self::from_string(s.to_uppercase());
        }
        let bytes = s.as_bytes();
        if bytes.len() <= SMOL_INLINE_MAX {
            let mut buf = [0u8; 8];
            for (i, &b) in bytes.iter().enumerate() {
                buf[i] = if b.is_ascii_lowercase() {
                    b - (b'a' - b'A')
                } else {
                    b
                };
            }
            buf[7] = bytes.len() as u8;
            Self {
                raw: u64::from_le_bytes(buf),
            }
        } else {
            let ptr = HeapSmolStr::alloc_ascii_upper(bytes) as usize as u64;
            debug_assert!(
                ptr & SMOL_HEAP_TAG == 0,
                "HeapSmolStr pointer must have high bit clear"
            );
            Self {
                raw: ptr | SMOL_HEAP_TAG,
            }
        }
    }

    /// Constructs from an existing `Arc<String>` - used by value
    /// registry paths that still expose strings that way.
    #[must_use]
    pub fn from_arc(arc: Arc<String>) -> Self {
        Self::from_str(arc.as_str())
    }

    fn new_inline(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= SMOL_INLINE_MAX);
        let mut buf = [0u8; 8];
        buf[..bytes.len()].copy_from_slice(bytes);
        // Length in the high byte (offset 7). High bit is 0,
        // so the heap tag is implicitly clear.
        buf[7] = bytes.len() as u8;
        Self {
            raw: u64::from_le_bytes(buf),
        }
    }

    fn new_heap(bytes: &[u8]) -> Self {
        Self::new_heap_with_capacity(bytes, bytes.len())
    }

    fn new_heap_with_capacity(bytes: &[u8], cap: usize) -> Self {
        debug_assert!(cap >= bytes.len());
        // SAFETY: `HeapSmolStr::alloc` returns an aligned allocation
        // obtained from the global allocator; user-space pointers on
        // supported targets fit in the low 63 bits, so OR-ing the tag
        // bit is information-preserving.
        let ptr = HeapSmolStr::alloc_with_capacity(bytes, cap) as usize as u64;
        debug_assert!(
            ptr & SMOL_HEAP_TAG == 0,
            "HeapSmolStr pointer must have high bit clear"
        );
        Self {
            raw: ptr | SMOL_HEAP_TAG,
        }
    }

    fn grown_capacity(current: usize, needed: usize) -> usize {
        debug_assert!(needed > current);
        let doubled = current.saturating_mul(2).max(16);
        doubled.max(needed)
    }

    /// Returns the borrowed string contents. Inline storage
    /// uses bytes from `self`; heap storage dereferences the
    /// underlying `String`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        if self.raw & SMOL_HEAP_TAG == 0 {
            // Inline: read length, return the prefix.
            // SAFETY: `new_inline` only writes valid UTF-8
            // bytes (since the input was a `&str`), so the
            // resulting prefix is valid UTF-8. The reference
            // ties its lifetime to `self`.
            let bytes: [u8; 8] = self.raw.to_le_bytes();
            let len = bytes[7] as usize;
            unsafe {
                let ptr = (&raw const self.raw).cast::<u8>();
                let slice = std::slice::from_raw_parts(ptr, len);
                std::str::from_utf8_unchecked(slice)
            }
        } else {
            // Heap: dereference the thin RC byte buffer.
            // SAFETY: only constructed via `HeapSmolStr::alloc`;
            // the strong count is at least 1 for the lifetime
            // of `self` (we hold one reference). We never give
            // out the raw pointer outside `Drop` / `Clone`.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::as_str(ptr) }
        }
    }

    /// Appends `s` in place. The heap variant keeps spare capacity and grows
    /// it when uniquely owned, so repeated appends to a `mut String` cost
    /// O(total length) instead of O(n^2). A shared heap string is copied once
    /// on the next append (copy-on-write). Inline storage appends in place
    /// until it exceeds the 7-byte window, then promotes to a heap string
    /// sized for both halves.
    pub fn push_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.raw & SMOL_HEAP_TAG == 0 {
            let len = self.raw.to_le_bytes()[7] as usize;
            if len + s.len() <= SMOL_INLINE_MAX {
                let mut buf = self.raw.to_le_bytes();
                buf[len..len + s.len()].copy_from_slice(s.as_bytes());
                buf[7] = (len + s.len()) as u8;
                self.raw = u64::from_le_bytes(buf);
            } else {
                let mut owned = String::with_capacity(len + s.len());
                owned.push_str(self.as_str());
                owned.push_str(s);
                *self = Self::new_heap_with_capacity(
                    owned.as_bytes(),
                    Self::grown_capacity(SMOL_INLINE_MAX, owned.len()),
                );
            }
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *mut HeapSmolStr;
            // SAFETY: `ptr` comes from a live heap SmolStr owned by `self`.
            let (len, cap, unique) = unsafe {
                (
                    (*ptr).len as usize,
                    (*ptr).cap as usize,
                    HeapSmolStr::is_unique(ptr),
                )
            };
            let needed = len + s.len();
            if unique && needed <= cap {
                // SAFETY: uniqueness and capacity are checked immediately above.
                unsafe { HeapSmolStr::append_unique(ptr, s.as_bytes()) };
                return;
            }
            // A VM builtin receives a cloned receiver before its write-back.
            // Copy-on-write therefore commonly takes this branch even when a
            // prior `String::with_capacity` reservation is large enough. Keep
            // that reservation on the replacement buffer; only grow when the
            // appended bytes exceed it.
            let new_cap = if needed <= cap {
                cap
            } else {
                Self::grown_capacity(cap, needed)
            };
            let mut owned = String::with_capacity(needed);
            owned.push_str(self.as_str());
            owned.push_str(s);
            let old = std::mem::replace(
                self,
                Self::new_heap_with_capacity(owned.as_bytes(), new_cap),
            );
            drop(old);
        }
    }

    /// Appends one Unicode scalar while retaining any reserved capacity.
    pub fn push(&mut self, ch: char) {
        let mut encoded = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut encoded));
    }

    /// Returns the number of Unicode scalar values.
    #[must_use]
    pub fn len(&self) -> usize {
        if self.raw & SMOL_HEAP_TAG == 0 {
            self.as_str().chars().count()
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
            unsafe { (*ptr).char_len as usize }
        }
    }

    /// Returns the UTF-8 byte length.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.as_str().len()
    }

    /// Returns the Unicode scalar at `index`.
    #[must_use]
    pub fn char_at(&self, index: usize) -> Option<char> {
        if self.raw & SMOL_HEAP_TAG == 0 {
            return self.as_str().chars().nth(index);
        }
        let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
        // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
        let header = unsafe { &*ptr };
        if index >= header.char_len as usize {
            return None;
        }
        let block = index / SMOL_CHAR_INDEX_STRIDE;
        let block_char = block * SMOL_CHAR_INDEX_STRIDE;
        let byte = header.char_index[block] as usize;
        self.as_str()[byte..].chars().nth(index - block_char)
    }

    /// Maps a Unicode scalar position to its UTF-8 byte boundary.
    #[must_use]
    pub fn char_boundary(&self, index: usize) -> Option<usize> {
        if index > self.len() {
            return None;
        }
        if index == self.len() {
            return Some(self.byte_len());
        }
        if self.raw & SMOL_HEAP_TAG == 0 {
            return self.as_str().char_indices().nth(index).map(|(i, _)| i);
        }
        let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
        // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
        let header = unsafe { &*ptr };
        let block = index / SMOL_CHAR_INDEX_STRIDE;
        let block_char = block * SMOL_CHAR_INDEX_STRIDE;
        let byte = header.char_index[block] as usize;
        self.as_str()[byte..]
            .char_indices()
            .nth(index - block_char)
            .map(|(offset, _)| byte + offset)
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        if self.raw & SMOL_HEAP_TAG == 0 {
            SMOL_INLINE_MAX
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
            unsafe { (*ptr).cap as usize }
        }
    }

    /// Returns `true` iff the string has zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SmolStr {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SmolStr {
    fn clone(&self) -> Self {
        if self.raw & SMOL_HEAP_TAG != 0 {
            // SAFETY: we own a strong reference; reconstruct an
            // Arc to bump the count, then forget so we don't
            // drop our copy. The original raw stays valid.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::inc(ptr) };
        }
        Self { raw: self.raw }
    }
}

impl Drop for SmolStr {
    fn drop(&mut self) {
        if self.raw & SMOL_HEAP_TAG != 0 {
            // SAFETY: we own one strong reference produced by
            // `HeapSmolStr::alloc`. Decrementing releases it exactly once.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::dec(ptr) };
        }
    }
}

impl PartialEq for SmolStr {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: both inline with same raw bits → equal.
        if self.raw == other.raw {
            return true;
        }
        self.as_str() == other.as_str()
    }
}

impl Eq for SmolStr {}

impl std::hash::Hash for SmolStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for SmolStr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmolStr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Debug for SmolStr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), out)
    }
}

impl fmt::Display for SmolStr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.as_str())
    }
}

impl AsRef<str> for SmolStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for SmolStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for SmolStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SmolStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<String> for SmolStr {
    fn from(s: String) -> Self {
        Self::from_string(s)
    }
}

impl From<&str> for SmolStr {
    fn from(s: &str) -> Self {
        Self::from_str(s)
    }
}

impl From<Arc<String>> for SmolStr {
    fn from(arc: Arc<String>) -> Self {
        Self::from_arc(arc)
    }
}

// SAFETY: heap storage is a thin atomic-refcounted immutable byte buffer.
// Inline storage is plain bytes copyable across threads.
unsafe impl Send for SmolStr {}
unsafe impl Sync for SmolStr {}

#[cfg(test)]
mod smolstr_tests {
    use super::SmolStr;

    #[test]
    fn repeated_appends_preserve_contents() {
        let mut s = SmolStr::new();
        for _ in 0..10_000 {
            s.push_str("aç");
        }
        assert_eq!(s.len(), 20_000);
        assert_eq!(s.char_at(19_999), Some('ç'));
        assert!(s.as_str().starts_with("açaç"));
        assert!(s.as_str().ends_with("aç"));
    }

    #[test]
    fn append_to_shared_heap_string_is_copy_on_write() {
        let mut left = SmolStr::from("abcdefgh");
        let right = left.clone();

        left.push_str("-mutated");

        assert_eq!(right.as_str(), "abcdefgh");
        assert_eq!(left.as_str(), "abcdefgh-mutated");
    }

    #[test]
    fn reserved_empty_string_reuses_vm_builder_capacity() {
        let mut text = SmolStr::with_capacity(64);
        assert_eq!(text.capacity(), 64);
        text.push_str("reserved text");
        assert_eq!(text.as_str(), "reserved text");
        assert_eq!(text.capacity(), 64);
    }
}
