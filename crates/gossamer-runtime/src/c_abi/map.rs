#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
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
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use gossamer_abi::TUPLE_TAG_NESTED;

use super::*;

// ---------------------------------------------------------------
// HashMap runtime - typed-storage variants over rustc-hash's
// FxHashMap. Auto-promotes Empty → I64I64 / StrI64 / StrStr /
// Bytes on first typed call. The i64-keyed/i64-valued shape
// (counter / scoreboard hot paths) avoids per-op `Vec<u8>`
// allocation and uses FxHash directly on the
// 8-byte key.
// ---------------------------------------------------------------

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use rustc_hash::FxHashMap;

/// A mutex whose lock is *biased* to the goroutine that owns the map:
/// while the map has not escaped to another goroutine it is accessed
/// with no locking at all, and only an escaped (shared) map pays the
/// `parking_lot` acquire/release on each operation.
///
/// Gossamer's model is "share by communicating": an ordinary
/// `HashMap` lives on one goroutine and is never touched concurrently,
/// so the per-operation lock the unconditional mutex imposed was pure
/// overhead on the common path (the k-mer-counter hot loop pays it tens
/// of millions of times). A map only becomes reachable from a second
/// goroutine through an explicit escape point - captured by a `go` /
/// `spawn` closure, or sent on a channel - and the codegen marks it
/// `shared` *on the owning goroutine, before the value is published*
/// (the same protocol `RcHeader` / string values use via
/// `gos_rt_rc_mark_shared`). After that flip every operation locks, so
/// concurrent access to a genuinely shared map is fully synchronized -
/// strictly safer than Go's unsynchronized maps, with zero cost when no
/// sharing exists.
struct BiasedLock<T> {
    shared: AtomicBool,
    inner: parking_lot::Mutex<T>,
}

impl<T> BiasedLock<T> {
    fn new(value: T) -> Self {
        BiasedLock {
            shared: AtomicBool::new(false),
            inner: parking_lot::Mutex::new(value),
        }
    }

    /// Acquires access to the protected value. Lock-free for a
    /// goroutine-local map; takes the real lock once the map is shared.
    #[inline]
    fn lock(&self) -> BiasedGuard<'_, T> {
        if self.shared.load(Ordering::Acquire) {
            BiasedGuard::Locked(self.inner.lock())
        } else {
            // The map is owned by a single goroutine, so no other thread
            // can touch `inner` for the duration of this borrow. The flip
            // to `shared` happens on this goroutine before the map is
            // published to any other (see the type doc), so the load
            // above cannot miss an escape that another thread could race.
            BiasedGuard::Local(unsafe { &mut *self.inner.data_ptr() })
        }
    }

    /// Marks the map shared so every subsequent operation synchronizes.
    /// Called on the owning goroutine before the map escapes; idempotent.
    #[inline]
    fn mark_shared(&self) {
        self.shared.store(true, Ordering::Release);
    }
}

/// Access handle from [`BiasedLock::lock`]. Derefs to the protected
/// value in both the lock-free and the locked case; the `Locked`
/// variant releases the mutex on drop.
enum BiasedGuard<'a, T> {
    Local(&'a mut T),
    Locked(parking_lot::MutexGuard<'a, T>),
}

impl<T> Deref for BiasedGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        match self {
            BiasedGuard::Local(r) => r,
            BiasedGuard::Locked(g) => g,
        }
    }
}

impl<T> DerefMut for BiasedGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        match self {
            BiasedGuard::Local(r) => r,
            BiasedGuard::Locked(g) => g,
        }
    }
}

/// Layout-sensitive: the first 8 bytes hold the current element
/// count so the generic `gos_rt_arr_len` returns the right value
/// without needing a HashMap-specific dispatch.
#[repr(C)]
pub struct GosMap {
    len_cache: i64,
    storage: BiasedLock<MapStorage>,
    /// Ownership destructor for pointer-valued entries. The map keeps exactly
    /// one share per entry, releases overwritten/removed/remaining entries,
    /// and retains before handing an optional value to the caller. It is set
    /// immediately after construction from the checked map value type.
    value_owner: AtomicU8,
}

const MAP_VALUE_NONE: u8 = 0;
const MAP_VALUE_RC: u8 = 1;
const MAP_VALUE_VEC: u8 = 2;

/// A map key of raw bytes.
///
/// Equality is [`bytes_eq`] rather than the standard slice comparison: a hash
/// table confirms the key on every probe that hits, and under the static-musl
/// release link the standard comparison is a libc byte loop. Hashing is the
/// slice's own, so a key hashes identically however it was built.
#[derive(Clone, Debug)]
pub(crate) struct ByteKey(Box<[u8]>);

impl ByteKey {
    /// The key's bytes.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Box<[u8]>> for ByteKey {
    #[inline]
    fn from(bytes: Box<[u8]>) -> Self {
        Self(bytes)
    }
}

impl From<Vec<u8>> for ByteKey {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes.into_boxed_slice())
    }
}

impl From<&[u8]> for ByteKey {
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }
}

impl PartialEq for ByteKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        crate::c_abi::string::bytes_eq(&self.0, &other.0)
    }
}

impl Eq for ByteKey {}

impl PartialOrd for ByteKey {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByteKey {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl std::hash::Hash for ByteKey {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&*self.0, state);
    }
}

/// The borrowed form of [`ByteKey`], so a lookup by slice hashes and compares
/// exactly as the owned key it is matched against.
#[repr(transparent)]
pub(crate) struct ByteKeyRef([u8]);

impl ByteKeyRef {
    /// Views `bytes` as a lookup key.
    #[inline]
    pub(crate) fn new(bytes: &[u8]) -> &Self {
        // SAFETY: `repr(transparent)` over `[u8]`, so the two have identical
        // layout and metadata and the reference is valid for the same region.
        unsafe { &*(std::ptr::from_ref::<[u8]>(bytes) as *const ByteKeyRef) }
    }
}

impl PartialEq for ByteKeyRef {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        crate::c_abi::string::bytes_eq(&self.0, &other.0)
    }
}

impl Eq for ByteKeyRef {}

impl std::hash::Hash for ByteKeyRef {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.0, state);
    }
}

impl std::borrow::Borrow<ByteKeyRef> for ByteKey {
    #[inline]
    fn borrow(&self) -> &ByteKeyRef {
        ByteKeyRef::new(&self.0)
    }
}

enum MapStorage {
    Empty,
    I64I64(FxHashMap<i64, i64>),
    /// String-keyed maps store keys as `Box<[u8]>` (16 B header)
    /// rather than `Vec<u8>` (24 B header) - for the k-mer-counter
    /// hot shape (HashMap<String, i64> with millions of short
    /// keys), the saved 8 B per entry compounds visibly: ~8 MB
    /// off a 1 M-entry table. Same applies to `StrStr` keys and
    /// the `Bytes` byte-erased fallback.
    StrI64(FxHashMap<ByteKey, i64>),
    StrStr(FxHashMap<ByteKey, Box<[u8]>>),
    StrBytes(StrBytesStorage),
    I64Bytes(I64BytesStorage),
    I64Str(FxHashMap<i64, Box<[u8]>>),
    Bytes(FxHashMap<ByteKey, Box<[u8]>>),
    /// Struct / aggregate keys: the key is the flat content bytes of the
    /// aggregate (so two distinct allocations of an equal value hash and
    /// compare equal, matching the VM), the value is an 8-byte word - an
    /// `i64`, or a heap pointer for `String` / struct values.
    ///
    /// `desc` is the slot descriptor the keys were encoded with (one byte per
    /// slot, `s` for a scalar word and `S` for a string). Keeping it beside
    /// the entries is what lets a snapshot turn the stored bytes back into
    /// the aggregate the program wrote.
    SkeyVal {
        entries: FxHashMap<ByteKey, i64>,
        desc: Box<[u8]>,
    },
    /// Enum keys: the key is the canonical encoding of the enum node's
    /// discriminant and payload (so two equal-valued nodes at distinct
    /// allocations share a slot, matching the VM). An enum's layout varies
    /// per variant, so instead of rebuilding a key from its bytes the entry
    /// keeps the representative node the map retained, which a snapshot hands
    /// back as the value the program wrote.
    EkeyVal {
        entries: FxHashMap<ByteKey, EnumEntry>,
    },
}

/// One entry of an enum-keyed map: the stored value word and the retained
/// key node the map hands back from `keys()` / `iter()`.
struct EnumEntry {
    value: i64,
    key_node: *mut u8,
}

// SAFETY: `key_node` is an RC-managed node the map owns a share of for as
// long as the entry lives; the map's own lock serialises every access to it,
// exactly as it does for the string and blob pointers the other storage
// variants hold.
unsafe impl Send for EnumEntry {}
unsafe impl Sync for EnumEntry {}

#[derive(Clone)]
struct I64BytesStorage {
    entries: FxHashMap<i64, u64>,
    data: Vec<u8>,
    live_bytes: usize,
    reserve_entries: usize,
}

impl I64BytesStorage {
    fn with_capacity(cap: usize) -> Self {
        Self {
            entries: FxHashMap::with_capacity_and_hasher(cap, rustc_hash::FxBuildHasher),
            data: Vec::new(),
            live_bytes: 0,
            reserve_entries: cap,
        }
    }

    fn span(start: usize, len: usize) -> u64 {
        StrBytesStorage::span(start, len)
    }

    fn bounds(span: u64) -> (usize, usize) {
        StrBytesStorage::bounds(span)
    }

    fn get(&self, key: i64) -> Option<&[u8]> {
        let (start, len) = Self::bounds(*self.entries.get(&key)?);
        self.data.get(start..start + len)
    }

    fn contains_key(&self, key: i64) -> bool {
        self.entries.contains_key(&key)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn entries_vec(&self) -> Vec<(i64, &[u8])> {
        self.entries
            .iter()
            .map(|(key, span)| {
                let (start, len) = Self::bounds(*span);
                (*key, &self.data[start..start + len])
            })
            .collect()
    }

    fn insert(&mut self, key: i64, value: &[u8]) -> bool {
        if self.data.capacity() == 0 && !value.is_empty() {
            let requested = self.reserve_entries.saturating_mul(value.len());
            let _ = self.data.try_reserve_exact(requested);
        }
        let start = self.data.len();
        self.data.extend_from_slice(value);
        let span = Self::span(start, value.len());
        if let Some(old) = self.entries.get_mut(&key) {
            self.live_bytes -= Self::bounds(*old).1;
            *old = span;
            self.live_bytes += value.len();
            self.compact_if_needed();
            return false;
        }
        self.entries.insert(key, span);
        self.live_bytes += value.len();
        self.compact_if_needed();
        true
    }

    fn remove(&mut self, key: i64) -> Option<Vec<u8>> {
        let span = self.entries.remove(&key)?;
        let (start, len) = Self::bounds(span);
        let value = self.data[start..start + len].to_vec();
        self.live_bytes -= len;
        self.compact_if_needed();
        Some(value)
    }

    fn compact_if_needed(&mut self) {
        if self.data.len() <= self.live_bytes.saturating_mul(2).max(4096) {
            return;
        }
        let mut compact = Vec::with_capacity(self.live_bytes);
        for span in self.entries.values_mut() {
            let (start, len) = Self::bounds(*span);
            let next = compact.len();
            compact.extend_from_slice(&self.data[start..start + len]);
            *span = Self::span(next, len);
        }
        self.data = compact;
    }
}

#[derive(Clone)]
struct StrBytesStorage {
    entries: FxHashMap<ByteKey, u64>,
    data: Vec<u8>,
    live_bytes: usize,
    reserve_entries: usize,
}

impl StrBytesStorage {
    fn with_capacity(cap: usize) -> Self {
        Self {
            entries: FxHashMap::with_capacity_and_hasher(cap, rustc_hash::FxBuildHasher),
            data: Vec::new(),
            live_bytes: 0,
            reserve_entries: cap,
        }
    }

    fn span(start: usize, len: usize) -> u64 {
        ((start as u64) << 32) | len as u64
    }

    fn bounds(span: u64) -> (usize, usize) {
        ((span >> 32) as usize, span as u32 as usize)
    }

    fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let (start, len) = Self::bounds(*self.entries.get(ByteKeyRef::new(key))?);
        self.data.get(start..start + len)
    }

    fn contains_key(&self, key: &[u8]) -> bool {
        self.entries.contains_key(ByteKeyRef::new(key))
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.entries.iter().map(|(key, span)| {
            let (start, len) = Self::bounds(*span);
            (key.as_slice(), &self.data[start..start + len])
        })
    }

    fn insert(&mut self, key: &[u8], value: &[u8]) -> bool {
        // `HashMap::with_capacity` promises that the requested entry count
        // can be populated without repeated table growth. Apply the same
        // principle to compact byte values once their actual width is known.
        // This avoids repeatedly copying the entire contiguous value arena.
        if self.data.capacity() == 0 && !value.is_empty() {
            let requested = self.reserve_entries.saturating_mul(value.len());
            let _ = self.data.try_reserve_exact(requested);
        }
        let start = self.data.len();
        self.data.extend_from_slice(value);
        let span = Self::span(start, value.len());
        if let Some(old) = self.entries.get_mut(ByteKeyRef::new(key)) {
            self.live_bytes -= Self::bounds(*old).1;
            *old = span;
            self.live_bytes += value.len();
            self.compact_if_needed();
            return false;
        }
        self.entries
            .insert(crate::c_abi::string::boxed_bytes(key).into(), span);
        self.live_bytes += value.len();
        self.compact_if_needed();
        true
    }

    fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let span = self.entries.remove(ByteKeyRef::new(key))?;
        let (start, len) = Self::bounds(span);
        let value = self.data[start..start + len].to_vec();
        self.live_bytes -= len;
        self.compact_if_needed();
        Some(value)
    }

    fn compact_if_needed(&mut self) {
        if self.data.len() <= self.live_bytes.saturating_mul(2).max(4096) {
            return;
        }
        let mut compact = Vec::with_capacity(self.live_bytes);
        for span in self.entries.values_mut() {
            let (start, len) = Self::bounds(*span);
            let next = compact.len();
            compact.extend_from_slice(&self.data[start..start + len]);
            *span = Self::span(next, len);
        }
        self.data = compact;
    }
}

unsafe fn byte_vec_from_slice(bytes: &[u8]) -> *mut GosVec {
    let out = unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity(1, bytes.len() as i64) };
    if !bytes.is_empty() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), (*out).ptr.as_ptr(), bytes.len());
            (*out).len = bytes.len() as i64;
        }
    }
    out
}

/// One entry of a map, in the shape a comparison reads it: the key and value
/// each as the word they are stored as, or as their bytes.
#[derive(PartialEq, Eq, Hash)]
enum EntryPart {
    Word(i64),
    Bytes(Box<[u8]>),
}

/// How a map's value word is read when it is a bare word rather than stored
/// bytes. The compiled tiers pass the kind the value's declared type names,
/// because the storage keeps no type of its own.
mod map_value_kind {
    /// An `f64`, compared as the float it spells rather than as its bits.
    pub(super) const FLOAT: i64 = 1;
    /// A runtime `String`, compared by the bytes it holds.
    pub(super) const STRING: i64 = 2;
    /// A single-slot field the descriptor describes - a sequence, a string -
    /// canonicalised from the slot the value word occupies.
    pub(super) const DESC_SLOT: i64 = 3;
    /// A block of slots the descriptor describes, addressed by the value word.
    pub(super) const DESC_BLOCK: i64 = 4;
}

/// Reads a value word as the part a comparison uses, per the declared kind.
/// A value the runtime keeps as one word may still stand for a whole value -
/// a string, a sequence, an aggregate - so `desc` names the shape to fold it
/// into content bytes by, the same encoding a content key is built with.
unsafe fn value_part(word: i64, kind: i64, desc: *const c_char) -> EntryPart {
    if kind == map_value_kind::STRING {
        if word == 0 {
            return EntryPart::Bytes(Box::default());
        }
        let text = unsafe { crate::c_abi::gos_str_arg_bytes(word as *const std::ffi::c_char) };
        return EntryPart::Bytes(text.to_vec().into_boxed_slice());
    }
    if matches!(kind, map_value_kind::DESC_SLOT | map_value_kind::DESC_BLOCK) && !desc.is_null() {
        let block: *const u8 = if kind == map_value_kind::DESC_SLOT {
            std::ptr::from_ref(&word).cast()
        } else {
            std::ptr::with_exposed_provenance(word as usize)
        };
        if block.is_null() {
            return EntryPart::Bytes(Box::default());
        }
        if let Some(bytes) = unsafe { build_skey_for_set(block, desc) } {
            return EntryPart::Bytes(bytes.into_boxed_slice());
        }
    }
    EntryPart::Word(word)
}

/// Snapshots a map's entries in a shape that is the same for every storage
/// the same static type can settle into.
unsafe fn map_entry_parts(
    m: &GosMap,
    value_kind: i64,
    value_desc: *const c_char,
) -> Vec<(EntryPart, EntryPart)> {
    let storage = m.storage.lock();
    let bytes = |b: &[u8]| EntryPart::Bytes(b.to_vec().into_boxed_slice());
    match &*storage {
        MapStorage::Empty => Vec::new(),
        MapStorage::I64I64(inner) => inner
            .iter()
            .map(|(k, v)| {
                (EntryPart::Word(*k), unsafe {
                    value_part(*v, value_kind, value_desc)
                })
            })
            .collect(),
        MapStorage::StrI64(inner) => inner
            .iter()
            .map(|(k, v)| {
                (bytes(k.as_slice()), unsafe {
                    value_part(*v, value_kind, value_desc)
                })
            })
            .collect(),
        MapStorage::StrStr(inner) | MapStorage::Bytes(inner) => inner
            .iter()
            .map(|(k, v)| (bytes(k.as_slice()), bytes(v)))
            .collect(),
        MapStorage::I64Str(inner) => inner
            .iter()
            .map(|(k, v)| (EntryPart::Word(*k), bytes(v)))
            .collect(),
        MapStorage::StrBytes(inner) => inner.iter().map(|(k, v)| (bytes(k), bytes(v))).collect(),
        MapStorage::I64Bytes(inner) => inner
            .entries
            .keys()
            .filter_map(|k| inner.get(*k).map(|v| (EntryPart::Word(*k), bytes(v))))
            .collect(),
        MapStorage::SkeyVal { entries, .. } => entries
            .iter()
            .map(|(k, v)| {
                (bytes(k.as_slice()), unsafe {
                    value_part(*v, value_kind, value_desc)
                })
            })
            .collect(),
        MapStorage::EkeyVal { entries } => entries
            .iter()
            .map(|(k, entry)| {
                (bytes(k.as_slice()), unsafe {
                    value_part(entry.value, value_kind, value_desc)
                })
            })
            .collect(),
    }
}

/// Structural equality of two maps: equal when they hold the same entries,
/// whatever order those went in and whichever storage each side settled into.
/// `value_kind` names how a bare value word is read - see `map_value_kind`.
///
/// A `f64` value compares as the float it spells, so a map holding a NaN is
/// unequal to itself, exactly as the scalar is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_eq(
    a: *const GosMap,
    b: *const GosMap,
    value_kind: i64,
    value_desc: *const c_char,
) -> i64 {
    ffi_entry!(0, {
        if std::ptr::eq(a, b) {
            return 1;
        }
        if a.is_null() || b.is_null() {
            return 0;
        }
        let xa = unsafe { map_entry_parts(&*a, value_kind, value_desc) };
        let xb = unsafe { map_entry_parts(&*b, value_kind, value_desc) };
        if xa.len() != xb.len() {
            return 0;
        }
        let rhs: std::collections::HashMap<&EntryPart, &EntryPart> =
            xb.iter().map(|(k, v)| (k, v)).collect();
        for (key, value) in &xa {
            let Some(other) = rhs.get(key) else {
                return 0;
            };
            let same = if value_kind == map_value_kind::FLOAT {
                match (value, other) {
                    (EntryPart::Word(x), EntryPart::Word(y)) => {
                        f64::from_bits(*x as u64) == f64::from_bits(*y as u64)
                    }
                    _ => value == *other,
                }
            } else {
                value == *other
            };
            if !same {
                return 0;
            }
        }
        1
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_new(_key_bytes: u32, _val_bytes: u32) -> *mut GosMap {
    ffi_entry!(std::ptr::null_mut(), {
        crate::c_abi::ledger::map_inc();
        Box::into_raw(Box::new(GosMap {
            len_cache: 0,
            storage: BiasedLock::new(MapStorage::Empty),
            value_owner: AtomicU8::new(MAP_VALUE_NONE),
        }))
    })
}

/// Pre-sized constructor: avoids the doubling chain (~22 reallocs
/// for ~5M inserts) when the caller has an upper bound. Picks the
/// initial typed shape from the byte sizes - both 8 → I64I64,
/// otherwise the byte-erased generic shape that promotes lazily.
/// Pre-sizing avoids the doubling chain on counter-style hot
/// loops where the caller knows the total entry count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_new_with_capacity(
    key_bytes: u32,
    val_bytes: u32,
    cap: i64,
) -> *mut GosMap {
    ffi_entry!(std::ptr::null_mut(), {
        // An inferred loop bound may ultimately come from untrusted input.
        // Keep preallocation an optimisation rather than an allocation-DoS;
        // subsequent inserts retain ordinary map growth semantics.
        const MAX_PREALLOCATED_CAPACITY: usize = 1 << 24;
        if cap < 0 {
            unsafe {
                crate::c_abi::panic::panic_text(
                    "HashMap::with_capacity: capacity must be non-negative",
                );
            };
        }
        let cap = (cap as usize).min(MAX_PREALLOCATED_CAPACITY);
        let storage = if key_bytes == 8 && val_bytes == 8 {
            MapStorage::I64I64(FxHashMap::with_capacity_and_hasher(
                cap,
                rustc_hash::FxBuildHasher,
            ))
        } else {
            MapStorage::Empty
        };
        crate::c_abi::ledger::map_inc();
        Box::into_raw(Box::new(GosMap {
            len_cache: 0,
            storage: BiasedLock::new(storage),
            value_owner: AtomicU8::new(MAP_VALUE_NONE),
        }))
    })
}

/// Typed pre-sized constructor for the source-visible map layouts. The older
/// width-only constructor cannot distinguish scalar, string, and byte-vector
/// values. Unknown kinds retain lazy generic storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_new_with_capacity_typed(
    key_kind: u32,
    val_kind: u32,
    cap: i64,
) -> *mut GosMap {
    ffi_entry!(std::ptr::null_mut(), {
        const MAX_PREALLOCATED_CAPACITY: usize = 1 << 24;
        if cap < 0 {
            unsafe {
                crate::c_abi::panic::panic_text(
                    "HashMap::with_capacity: capacity must be non-negative",
                );
            };
        }
        let cap = (cap as usize).min(MAX_PREALLOCATED_CAPACITY);
        let storage = match (key_kind, val_kind) {
            (0, 0) => MapStorage::I64I64(FxHashMap::with_capacity_and_hasher(
                cap,
                rustc_hash::FxBuildHasher,
            )),
            (1, 0) => MapStorage::StrI64(FxHashMap::with_capacity_and_hasher(
                cap,
                rustc_hash::FxBuildHasher,
            )),
            (0, 1) => MapStorage::I64Str(FxHashMap::with_capacity_and_hasher(
                cap,
                rustc_hash::FxBuildHasher,
            )),
            (1, 1) => MapStorage::StrStr(FxHashMap::with_capacity_and_hasher(
                cap,
                rustc_hash::FxBuildHasher,
            )),
            (0, 2) => MapStorage::I64Bytes(I64BytesStorage::with_capacity(cap)),
            (1, 2) => MapStorage::StrBytes(StrBytesStorage::with_capacity(cap)),
            _ => MapStorage::Empty,
        };
        crate::c_abi::ledger::map_inc();
        Box::into_raw(Box::new(GosMap {
            len_cache: 0,
            storage: BiasedLock::new(storage),
            value_owner: AtomicU8::new(MAP_VALUE_NONE),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_len(m: *const GosMap) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() {
            return 0;
        }
        unsafe { (*m).len_cache }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert(m: *mut GosMap, key: *const u8, val: *const u8) {
    ffi_entry!((), {
        if m.is_null() || key.is_null() || val.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        let k = unsafe { std::slice::from_raw_parts(key, 8) }.to_vec();
        let v = unsafe { std::slice::from_raw_parts(val, 8) }.to_vec();
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::Bytes(FxHashMap::default());
        }
        let MapStorage::Bytes(inner) = &mut *storage else {
            return;
        };
        if inner.insert(k.into(), v.into_boxed_slice()).is_none() {
            map.len_cache += 1;
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get(m: *const GosMap, key: *const u8, val_out: *mut u8) -> i32 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() || val_out.is_null() {
            return 0;
        }
        let map = unsafe { &*m };
        let k = unsafe { std::slice::from_raw_parts(key, 8) };
        let storage = map.storage.lock();
        let MapStorage::Bytes(inner) = &*storage else {
            // Answering 0 here would read as "no such key", which is a
            // different fact: this map holds its entries in a shape this
            // reader does not know, so the caller was compiled against a
            // storage the map never took.
            drop(storage);
            crate::c_abi::panic::panic_text(
                "map read reached a storage shape this accessor does not handle",
            );
            return 0;
        };
        if let Some(v) = inner.get(ByteKeyRef::new(k)) {
            unsafe {
                std::ptr::copy_nonoverlapping(v.as_ptr(), val_out, v.len());
            }
            1
        } else {
            0
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_i64(m: *const GosMap, key: i64, default: i64) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() {
            return default;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::I64I64(inner) => inner.get(&key).copied().unwrap_or(default),
            _ => default,
        }
    })
}

/// `get_or` for string-keyed, i64-valued maps. Mirrors
/// `gos_rt_map_get_or_i64` but hashes the key via the same UTF-8
/// byte slice the `_str_i64` insert path uses, so an `insert(k, v)`
/// followed by `get_or(k, d)` round-trips.
unsafe fn map_get_or_str_i64_impl(m: *const GosMap, key: *const c_char, default: i64) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() {
            return default;
        }
        let map = unsafe { &*m };
        crate::c_abi::ledger::map_str_probe();
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::StrI64(inner) => inner
                .get(ByteKeyRef::new(key_bytes))
                .copied()
                .unwrap_or(default),
            _ => default,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_str_i64(
    m: *const GosMap,
    key: *const c_char,
    default: i64,
) -> i64 {
    unsafe { map_get_or_str_i64_impl(m, key, default) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_typed_str_i64(
    m: *const GosMap,
    key: *const c_char,
    default: i64,
) -> i64 {
    unsafe { map_get_or_str_i64_impl(m, key, default) }
}

/// `get_or` for string-keyed, string-valued maps. Returns a fresh
/// GC-allocated `*mut c_char` for the stored value, or a copy of
/// `default`'s bytes when the key is absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_str_str(
    m: *const GosMap,
    key: *const c_char,
    default: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let default_bytes: &[u8] = if default.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(default) }
        };
        if m.is_null() || key.is_null() {
            return alloc_cstring(default_bytes);
        }
        let map = unsafe { &*m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        let MapStorage::StrStr(inner) = &*storage else {
            return alloc_cstring(default_bytes);
        };
        match inner.get(ByteKeyRef::new(key_bytes)) {
            Some(v) => alloc_cstring(v),
            None => alloc_cstring(default_bytes),
        }
    })
}

/// `get_or` for i64-keyed, string-valued maps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_i64_str(
    m: *const GosMap,
    key: i64,
    default: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let default_bytes: &[u8] = if default.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(default) }
        };
        if m.is_null() {
            return alloc_cstring(default_bytes);
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let MapStorage::I64Str(inner) = &*storage else {
            return alloc_cstring(default_bytes);
        };
        match inner.get(&key) {
            Some(v) => alloc_cstring(v),
            None => alloc_cstring(default_bytes),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_i64_i64(m: *mut GosMap, key: i64, val: i64) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if let MapStorage::I64Bytes(inner) = &mut *storage {
            let vec = val as usize as *mut GosVec;
            if vec.is_null() {
                return;
            }
            if unsafe { crate::c_abi::vec::consume_byte_vec(vec, |bytes| inner.insert(key, bytes)) }
            {
                map.len_cache += 1;
            }
            return;
        }
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::I64I64(FxHashMap::default());
        }
        let MapStorage::I64I64(inner) = &mut *storage else {
            return;
        };
        let prev = inner.insert(key, val);
        if prev.is_none() {
            map.len_cache += 1;
        }
        let owns_values = map_has_owned_values(map);
        let replaced = prev.filter(|old| *old != val);
        // The entry keeps this word, so the map takes a share of it here -
        // the same exchange the string-keyed insert makes. Minting it in the
        // runtime rather than at the call site covers every spelling that
        // reaches a map: a literal, a method call, and the free form.
        if owns_values && (replaced.is_some() || prev.is_none()) {
            unsafe { retain_owned_value(map, val) };
        }
        if owns_values && let Some(old) = replaced {
            unsafe { release_owned_value(map, old) };
        }
    });
}

/// Builds a canonical by-value key for an aggregate from its flat slot buffer,
/// driven by a per-slot layout descriptor: `'s'` = an 8-byte scalar (read
/// inline), `'S'` = a `String` pointer (dereferenced; its length-prefixed
/// content is folded in). Nested all-scalar structs inline their slots, so
/// they appear as runs of `'s'`. The result is identical for two equal values
/// at distinct allocations, matching the VM's value-keying.
pub(crate) unsafe fn build_skey_for_set(key: *const u8, desc: *const c_char) -> Option<Vec<u8>> {
    if key.is_null() || desc.is_null() {
        return None;
    }
    let desc = unsafe { crate::c_abi::gos_str_arg_bytes(desc) };
    let mut out = Vec::with_capacity(desc.len() * 8);
    let mut off = 0usize;
    for &c in desc {
        let slot = unsafe { key.add(off) };
        match c {
            b's' => out.extend_from_slice(unsafe { std::slice::from_raw_parts(slot, 8) }),
            b'S' => {
                // The string field holds a cstring pointer exposed as an
                // integer by the flat-slot ABI; recover its provenance.
                let raw = unsafe { (slot as *const usize).read_unaligned() };
                let sptr: *const c_char = std::ptr::with_exposed_provenance(raw);
                if sptr.is_null() {
                    out.extend_from_slice(&0u64.to_le_bytes());
                } else {
                    let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(sptr) };
                    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
            }
            // A sequence field folds by content: its length, then the bytes
            // its elements occupy, so two equal sequences at distinct
            // allocations key one slot exactly as the interpreter's
            // by-value keying does.
            b'V' => {
                let raw = unsafe { (slot as *const usize).read_unaligned() };
                let vec: *const crate::c_abi::GosVec = std::ptr::with_exposed_provenance(raw);
                if vec.is_null() {
                    out.extend_from_slice(&0u64.to_le_bytes());
                    out.extend_from_slice(&8u64.to_le_bytes());
                } else {
                    let v = unsafe { &*vec };
                    let len = v.len.max(0) as usize;
                    let stride = (v.elem_bytes as usize).max(1);
                    out.extend_from_slice(&(len as u64).to_le_bytes());
                    out.extend_from_slice(&(stride as u64).to_le_bytes());
                    if !v.ptr.is_null() {
                        out.extend_from_slice(unsafe {
                            std::slice::from_raw_parts(v.ptr.as_ptr(), len * stride)
                        });
                    }
                }
            }
            _ => return None,
        }
        off += 8;
    }
    Some(out)
}

#[allow(dead_code)]
unsafe fn build_skey(key: *const u8, desc: *const c_char) -> Option<Vec<u8>> {
    unsafe { build_skey_for_set(key, desc) }
}

/// Struct-keyed insert: keys by the aggregate's content (per `desc`) rather
/// than its pointer, so two equal values share a slot, matching the VM. `val`
/// is the 8-byte value word (an `i64`, or a pointer for `String` / struct
/// values).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_skey(
    m: *mut GosMap,
    key: *const u8,
    desc: *const c_char,
    val: i64,
) {
    ffi_entry!((), {
        let Some(k) = (unsafe { build_skey(key, desc) }) else {
            return;
        };
        if m.is_null() {
            return;
        }
        // SAFETY: the caller supplies a live descriptor c-string.
        let desc_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(desc) };
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::SkeyVal {
                entries: FxHashMap::default(),
                desc: desc_bytes.into(),
            };
        }
        let MapStorage::SkeyVal { entries, .. } = &mut *storage else {
            return;
        };
        let prev = entries.insert(k.into(), val);
        if prev.is_none() {
            map.len_cache += 1;
        }
        // The entry keeps this word, so the map takes a share of it here -
        // the same exchange the scalar- and string-keyed inserts make. The
        // inserting frame gives its own share back at the call site, and the
        // entry's death (an overwrite, a removal, or the map's own free)
        // gives back the one taken here.
        let owns_values = map_has_owned_values(map);
        let replaced = prev.filter(|old| *old != val);
        if owns_values && (replaced.is_some() || prev.is_none()) {
            unsafe { retain_owned_value(map, val) };
        }
        if owns_values && let Some(old) = replaced {
            unsafe { release_owned_value(map, old) };
        }
    });
}

/// Struct-keyed lookup. Returns `Option<i64>` in the `gos_rt_result_new`
/// i128 layout (0 = Some, 1 = None), matching [`gos_rt_map_get_i64_opt`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_skey_opt(
    m: *const GosMap,
    key: *const u8,
    desc: *const c_char,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let none = unsafe { gos_rt_result_new(1, 0) };
        let Some(k) = (unsafe { build_skey(key, desc) }) else {
            return none;
        };
        if m.is_null() {
            return none;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let payload: Option<i64> = match &*storage {
            MapStorage::SkeyVal { entries, .. } => {
                entries.get(ByteKeyRef::new(k.as_slice())).copied()
            }
            _ => None,
        };
        if let Some(v) = payload
            && map_has_owned_values(map)
        {
            unsafe { retain_owned_value(map, v) };
        }
        match payload {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => none,
        }
    })
}

/// Struct-keyed membership test (for `HashSet` of structs).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_contains_skey(
    m: *const GosMap,
    key: *const u8,
    desc: *const c_char,
) -> bool {
    ffi_entry!(false, {
        let Some(k) = (unsafe { build_skey(key, desc) }) else {
            return false;
        };
        if m.is_null() {
            return false;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::SkeyVal { entries, .. } => {
                entries.contains_key(ByteKeyRef::new(k.as_slice()))
            }
            _ => false,
        }
    })
}

/// Fused increment: `m[k] = m.get_or(k, 0) + by`. Single lock,
/// single hash, single bucket walk. Replaces the
/// `m.insert(k, m.get_or(k, 0) + 1)` pattern that costs 2× the
/// hash work on hot counter loops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_i64(m: *mut GosMap, key: i64, by: i64) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() {
            return 0;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::I64I64(FxHashMap::default());
        }
        let MapStorage::I64I64(inner) = &mut *storage else {
            return 0;
        };
        let entry = inner.entry(key).or_insert_with(|| {
            map.len_cache += 1;
            0
        });
        *entry += by;
        *entry
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_i64(m: *const GosMap, key: i64) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() {
            return 0;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::I64I64(inner) => inner.get(&key).copied().unwrap_or(0),
            _ => 0,
        }
    })
}

/// `m.get(k) -> Option<V>` for an i64-keyed map. Returns `*mut GosResult`
/// with `disc=0, payload=value-as-i64` when present, `disc=1, payload=0`
/// when absent. Treats every 8-byte value payload (i64, c-string ptr,
/// struct heap pointer) uniformly: the MIR pin recovers the proper V
/// from the call expression's `Option<V>` Adt substs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_i64_opt(m: *const GosMap, key: i64) -> i128 {
    ffi_entry!(0i128, {
        if m.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let payload: Option<i64> = match &*storage {
            MapStorage::I64I64(inner) => inner.get(&key).copied(),
            MapStorage::I64Bytes(inner) => inner
                .get(key)
                .map(|bs| unsafe { byte_vec_from_slice(bs) } as i64),
            MapStorage::I64Str(inner) => inner.get(&key).map(|bs| alloc_cstring(bs) as i64),
            _ => None,
        };
        match payload {
            Some(v) => {
                // Blob values: the caller's option holder receives (and
                // later releases) its own share; the map keeps its own.
                if map_has_owned_values(map) {
                    unsafe { retain_owned_value(map, v) };
                }
                unsafe { gos_rt_result_new(0, v) }
            }
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_contains_key_i64(m: *const GosMap, key: i64) -> bool {
    ffi_entry!(false, {
        if m.is_null() {
            return false;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::I64I64(inner) => inner.contains_key(&key),
            MapStorage::I64Bytes(inner) => inner.contains_key(key),
            MapStorage::I64Str(inner) => inner.contains_key(&key),
            _ => false,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_remove_i64(m: *mut GosMap, key: i64) -> bool {
    ffi_entry!(false, {
        if m.is_null() {
            return false;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        let owned_values = map_has_owned_values(map);
        let removed = match &mut *storage {
            MapStorage::I64I64(inner) => match inner.remove(&key) {
                Some(old) => {
                    if owned_values {
                        unsafe { release_owned_value(map, old) };
                    }
                    true
                }
                None => false,
            },
            MapStorage::I64Bytes(inner) => inner.remove(key).is_some(),
            MapStorage::I64Str(inner) => inner.remove(&key).is_some(),
            _ => false,
        };
        if removed {
            map.len_cache -= 1;
        }
        removed
    })
}

unsafe fn map_insert_str_i64_impl(m: *mut GosMap, key: *const c_char, val: i64, typed_key: bool) {
    ffi_entry!((), {
        if m.is_null() || key.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        crate::c_abi::ledger::map_str_probe();
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let mut storage = map.storage.lock();
        if let MapStorage::StrBytes(inner) = &mut *storage {
            let vec = val as usize as *mut GosVec;
            if vec.is_null() {
                return;
            }
            let inserted = unsafe {
                crate::c_abi::vec::consume_byte_vec_preserving_source(vec, |bytes| {
                    inner.insert(key_bytes, bytes)
                })
            };
            if inserted {
                crate::c_abi::ledger::map_str_key_copy(key_bytes.len());
                map.len_cache += 1;
            }
            drop(storage);
            if typed_key {
                unsafe { crate::c_abi::string::consume_moved_string_typed(key.cast_mut()) };
            } else {
                unsafe { crate::c_abi::string::consume_moved_string(key.cast_mut()) };
            }
            return;
        }
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::StrI64(FxHashMap::default());
        }
        let MapStorage::StrI64(inner) = &mut *storage else {
            return;
        };
        let prev = if let Some(slot) = inner.get_mut(ByteKeyRef::new(key_bytes)) {
            Some(std::mem::replace(slot, val))
        } else {
            crate::c_abi::ledger::map_str_key_copy(key_bytes.len());
            inner.insert(crate::c_abi::string::boxed_bytes(key_bytes).into(), val);
            map.len_cache += 1;
            None
        };
        // Overwriting a copy-blob value (e.g. a `Vec<i64>` handle in a
        // `HashMap<String, Vec<i64>>`): release the map's share of the
        // old word, mirroring the i64/i64 insert path. Gated on the
        // blob-values flag so scalar-valued maps stay untouched.
        let owns_values = map_has_owned_values(map);
        let release_old = if owns_values && prev != Some(val) {
            prev
        } else {
            None
        };
        drop(storage);
        // The entry keeps this word, and the `get` path hands out a share of
        // it beside the map's own, so the entry needs one: without it the
        // first read to be dropped takes the stored object with it.
        if owns_values && prev != Some(val) {
            unsafe { retain_owned_value(map, val) };
        }
        if let Some(old) = release_old {
            unsafe { release_owned_value(map, old) };
        }
        // Consuming insert copied the key bytes; release the moved-in gos-string
        // (rc-aware + tag-checked - safe for temps, shared, and literals).
        if typed_key {
            unsafe { crate::c_abi::string::consume_moved_string_typed(key.cast_mut()) };
        } else {
            unsafe { crate::c_abi::string::consume_moved_string(key.cast_mut()) };
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_str_i64(m: *mut GosMap, key: *const c_char, val: i64) {
    unsafe { map_insert_str_i64_impl(m, key, val, false) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_typed_str_i64(
    m: *mut GosMap,
    key: *const c_char,
    val: i64,
) {
    unsafe { map_insert_str_i64_impl(m, key, val, true) };
}

unsafe fn map_get_str_i64_impl(m: *const GosMap, key: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() {
            return 0;
        }
        let map = unsafe { &*m };
        crate::c_abi::ledger::map_str_probe();
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::StrI64(inner) => {
                inner.get(ByteKeyRef::new(key_bytes)).copied().unwrap_or(0)
            }
            _ => 0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_str_i64(m: *const GosMap, key: *const c_char) -> i64 {
    unsafe { map_get_str_i64_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_typed_str_i64(m: *const GosMap, key: *const c_char) -> i64 {
    unsafe { map_get_str_i64_impl(m, key) }
}

/// `m.get(k) -> Option<V>` for a string-keyed map. Same `*mut GosResult`
/// layout as [`gos_rt_map_get_i64_opt`]: 8-byte payload, MIR pin
/// recovers V from the call's `Option<V>` substs.
unsafe fn map_get_str_opt_impl(m: *const GosMap, key: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if m.is_null() || key.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let map = unsafe { &*m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        let payload: Option<i64> = match &*storage {
            MapStorage::StrI64(inner) => inner.get(ByteKeyRef::new(key_bytes)).copied(),
            MapStorage::StrStr(inner) | MapStorage::Bytes(inner) => inner
                .get(ByteKeyRef::new(key_bytes))
                .map(|bs| alloc_cstring(bs) as i64),
            MapStorage::StrBytes(inner) => inner
                .get(key_bytes)
                .map(|bs| unsafe { byte_vec_from_slice(bs) } as i64),
            _ => None,
        };
        // Handing out a copy-blob value (Vec / struct handle) shares
        // ownership with the caller: retain so the map's later drop
        // and the caller's drop are balanced. Gated like the i64/i64
        // get path; the StrStr/Bytes arms allocate a fresh c-string
        // and are not blob-values.
        if let Some(v) = payload
            && matches!(&*storage, MapStorage::StrI64(_))
            && map_has_owned_values(map)
        {
            unsafe { retain_owned_value(map, v) };
        }
        match payload {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_str_opt(m: *const GosMap, key: *const c_char) -> i128 {
    unsafe { map_get_str_opt_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_typed_str_opt(
    m: *const GosMap,
    key: *const c_char,
) -> i128 {
    unsafe { map_get_str_opt_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_str_str(
    m: *mut GosMap,
    key: *const c_char,
    val: *const c_char,
) {
    ffi_entry!((), {
        if m.is_null() || key.is_null() || val.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let val_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(val) };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::StrStr(FxHashMap::default());
        }
        let MapStorage::StrStr(inner) = &mut *storage else {
            return;
        };
        if let Some(slot) = inner.get_mut(ByteKeyRef::new(key_bytes)) {
            *slot = crate::c_abi::string::boxed_bytes(val_bytes);
        } else {
            inner.insert(
                crate::c_abi::string::boxed_bytes(key_bytes).into(),
                crate::c_abi::string::boxed_bytes(val_bytes),
            );
            map.len_cache += 1;
        }
        drop(storage);
        // The map copied the key/val bytes into its own storage, so it does not
        // retain the inbound gos-strings. `map_insert` is a consuming call (the
        // drop pass moves its arguments in), so release the originals here -
        // `gos_rt_str_free` is rc-aware and tag-checked: a moved temp is freed,
        // a still-shared string only has its count decremented, and a `.rodata`
        // literal / region string is skipped. Without this the inbound
        // `format!(...)` temporaries leaked once per insert.
        unsafe {
            crate::c_abi::string::consume_moved_string(key.cast_mut());
            crate::c_abi::string::consume_moved_string(val.cast_mut());
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_str_str(
    m: *const GosMap,
    key: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() || key.is_null() {
            return empty_cstring();
        }
        let map = unsafe { &*m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        let MapStorage::StrStr(inner) = &*storage else {
            return empty_cstring();
        };
        match inner.get(ByteKeyRef::new(key_bytes)) {
            Some(v) => alloc_cstring(v),
            None => empty_cstring(),
        }
    })
}

unsafe fn map_contains_key_str_impl(m: *const GosMap, key: *const c_char) -> bool {
    ffi_entry!(false, {
        if m.is_null() || key.is_null() {
            return false;
        }
        let map = unsafe { &*m };
        crate::c_abi::ledger::map_str_probe();
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::StrI64(inner) => inner.contains_key(ByteKeyRef::new(key_bytes)),
            MapStorage::StrStr(inner) => inner.contains_key(ByteKeyRef::new(key_bytes)),
            MapStorage::StrBytes(inner) => inner.contains_key(key_bytes),
            _ => false,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_contains_key_str(m: *const GosMap, key: *const c_char) -> bool {
    unsafe { map_contains_key_str_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_contains_key_typed_str(
    m: *const GosMap,
    key: *const c_char,
) -> bool {
    unsafe { map_contains_key_str_impl(m, key) }
}

unsafe fn map_remove_str_impl(m: *mut GosMap, key: *const c_char) -> bool {
    ffi_entry!(false, {
        if m.is_null() || key.is_null() {
            return false;
        }
        let map = unsafe { &mut *m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let mut storage = map.storage.lock();
        let owned_values = map_has_owned_values(map);
        let removed = match &mut *storage {
            MapStorage::StrI64(inner) => match inner.remove(ByteKeyRef::new(key_bytes)) {
                Some(old) => {
                    if owned_values {
                        unsafe { release_owned_value(map, old) };
                    }
                    true
                }
                None => false,
            },
            MapStorage::StrStr(inner) => inner.remove(ByteKeyRef::new(key_bytes)).is_some(),
            MapStorage::StrBytes(inner) => inner.remove(key_bytes).is_some(),
            _ => false,
        };
        if removed {
            map.len_cache -= 1;
        }
        removed
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_remove_str(m: *mut GosMap, key: *const c_char) -> bool {
    unsafe { map_remove_str_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_remove_typed_str(m: *mut GosMap, key: *const c_char) -> bool {
    unsafe { map_remove_str_impl(m, key) }
}

/// `m.inc_at(seq, start, len, by)` for `HashMap<String, i64>` -
/// the zero-allocation analogue of
/// `m.insert(k, m.get_or(k, 0) + by)` where `k = seq[start..start+len]`.
///
/// Mirrors `*m.entry(&seq[i..i+k]).or_insert(0) += by`: the
/// slice is borrowed (zero-copy), the hash table is consulted
/// exactly once, and a `Vec<u8>` is allocated only on the first
/// occurrence of each unique key. Halves the hash work per
/// iteration vs the get_or + insert pair, and avoids any
/// per-iteration scratch allocation for the key.
///
/// Returns the new value at `seq[start..start+len]` (or `by` if
/// the entry is fresh).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_at_str_i64(
    m: *mut GosMap,
    seq: *const c_char,
    start: i64,
    len: i64,
    by: i64,
) -> i64 {
    ffi_entry!(-1, {
        if start < 0 {
            crate::c_abi::panic::panic_text("HashMap::inc_at: start must be non-negative");
        }
        if len < 0 {
            crate::c_abi::panic::panic_text("HashMap::inc_at: length must be non-negative");
        }
        if m.is_null() || seq.is_null() || len == 0 {
            return 0;
        }
        // Generated code only calls this helper with a compiler-typed String,
        // so borrow its bytes through the typed fast path. That keeps the
        // defensive registry check on general C ABI helpers while avoiding a
        // global lock for each k-mer window.
        crate::c_abi::ledger::map_str_probe();
        let seq_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(seq) };
        let (start_u, len_u) = (start as usize, len as usize);
        let end_u = match start_u.checked_add(len_u) {
            Some(end) if end <= seq_bytes.len() => end,
            _ => return 0,
        };
        let key_slice = &seq_bytes[start_u..end_u];
        if std::str::from_utf8(key_slice).is_err() {
            return 0;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::StrI64(FxHashMap::default());
        }
        let MapStorage::StrI64(inner) = &mut *storage else {
            return 0;
        };
        // Lookup is by `&[u8]` - `Vec<u8>: Borrow<[u8]>` lets the
        // hashbrown table hash the slice without first allocating an
        // owned key. Only the first occurrence of each unique k-mer
        // pays the `to_vec()` cost.
        if let Some(v) = inner.get_mut(ByteKeyRef::new(key_slice)) {
            *v += by;
            return *v;
        }
        crate::c_abi::ledger::map_str_key_copy(key_slice.len());
        inner.insert(crate::c_abi::string::boxed_bytes(key_slice).into(), by);
        map.len_cache += 1;
        by
    })
}

/// `m.inc(key, by)` for `HashMap<String, i64>` - adds `by`
/// (default 1 in user code) to the value at `key`, inserting
/// the entry if absent. Halves the lock + hash work compared to
/// `m.insert(k, m.get_or(k, 0) + by)` and avoids the
/// double-borrow that pattern triggers in compiled mode.
unsafe fn map_inc_str_i64_impl(m: *mut GosMap, key: *const c_char, by: i64) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() {
            return 0;
        }
        crate::c_abi::ledger::map_str_probe();
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::StrI64(FxHashMap::default());
        }
        let MapStorage::StrI64(inner) = &mut *storage else {
            return 0;
        };
        if let Some(v) = inner.get_mut(ByteKeyRef::new(key_bytes)) {
            *v += by;
            return *v;
        }
        crate::c_abi::ledger::map_str_key_copy(key_bytes.len());
        inner.insert(crate::c_abi::string::boxed_bytes(key_bytes).into(), by);
        map.len_cache += 1;
        by
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_str_i64(
    m: *mut GosMap,
    key: *const c_char,
    by: i64,
) -> i64 {
    unsafe { map_inc_str_i64_impl(m, key, by) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_typed_str_i64(
    m: *mut GosMap,
    key: *const c_char,
    by: i64,
) -> i64 {
    unsafe { map_inc_str_i64_impl(m, key, by) }
}

/// `m.or_insert(key, default)` - inserts `default` for `key` only when
/// the key is absent; returns the current (possibly just-inserted) value.
/// `HashMap<String, i64>` variant.
unsafe fn map_or_insert_str_i64_impl(
    m: *mut GosMap,
    key: *const c_char,
    default: i64,
    typed_key: bool,
) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() {
            return default;
        }
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::StrI64(FxHashMap::default());
        }
        // A string-valued map stores its values as bytes, not as the word
        // every other value kind is stored as, so it answers through its own
        // entry: the caller's `default` is a `String` pointer whose bytes the
        // map copies, and the result is a fresh `String` holding the stored
        // text - the same shape `get_or` hands back.
        if let MapStorage::StrStr(inner) = &mut *storage {
            let default_bytes: &[u8] = if default == 0 {
                b""
            } else {
                unsafe { crate::c_abi::gos_str_arg_bytes(default as usize as *const c_char) }
            };
            let stored = if let Some(v) = inner.get(ByteKeyRef::new(key_bytes)) {
                alloc_cstring(v)
            } else {
                let value = crate::c_abi::string::boxed_bytes(default_bytes);
                inner.insert(crate::c_abi::string::boxed_bytes(key_bytes).into(), value);
                map.len_cache += 1;
                alloc_cstring(default_bytes)
            };
            drop(storage);
            if typed_key {
                unsafe { crate::c_abi::string::consume_moved_string_typed(key.cast_mut()) };
            } else {
                unsafe { crate::c_abi::string::consume_moved_string(key.cast_mut()) };
            }
            if default != 0 {
                unsafe {
                    crate::c_abi::string::consume_moved_string_typed(
                        default as usize as *mut c_char,
                    );
                }
            }
            return stored as usize as i64;
        }
        let MapStorage::StrI64(inner) = &mut *storage else {
            return default;
        };
        if let Some(v) = inner.get(ByteKeyRef::new(key_bytes)).copied() {
            // Key present: hand back the stored value. For a copy-blob
            // value (Vec / struct handle) the result remains an interior
            // borrow, just like Rust's `&mut V`; the caller must not own or
            // release it. The unused `default` value did receive the normal
            // container-call retain before this runtime helper ran, so drop
            // that prospective map share and leave its source owner for its
            // ordinary scope cleanup.
            if map_has_owned_values(map) {
                if default != v {
                    unsafe { release_owned_value(map, default) };
                }
            }
            // The key was retained as a consuming-call argument and copied
            // into the map's owned byte key, so release its source share.
            if typed_key {
                unsafe { crate::c_abi::string::consume_moved_string_typed(key.cast_mut()) };
            } else {
                unsafe { crate::c_abi::string::consume_moved_string(key.cast_mut()) };
            }
            return v;
        }
        // Key absent: the compiler's consuming-call retain supplies the
        // map's independent value share. The return below is a borrow of
        // that stored value and therefore does not create another share.
        inner.insert(crate::c_abi::string::boxed_bytes(key_bytes).into(), default);
        map.len_cache += 1;
        if typed_key {
            unsafe { crate::c_abi::string::consume_moved_string_typed(key.cast_mut()) };
        } else {
            unsafe { crate::c_abi::string::consume_moved_string(key.cast_mut()) };
        }
        default
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_or_insert_str_i64(
    m: *mut GosMap,
    key: *const c_char,
    default: i64,
) -> i64 {
    unsafe { map_or_insert_str_i64_impl(m, key, default, false) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_or_insert_typed_str_i64(
    m: *mut GosMap,
    key: *const c_char,
    default: i64,
) -> i64 {
    unsafe { map_or_insert_str_i64_impl(m, key, default, true) }
}

/// `m.or_insert(key, default)` - `HashMap<i64, i64>` variant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_or_insert_i64_i64(
    m: *mut GosMap,
    key: i64,
    default: i64,
) -> i64 {
    ffi_entry!(-1, {
        if m.is_null() {
            return default;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::I64I64(FxHashMap::default());
        }
        // See `map_or_insert_str_i64_impl`: a string-valued map stores bytes
        // rather than the value word, so it answers through its own entry.
        if let MapStorage::I64Str(inner) = &mut *storage {
            let default_bytes: &[u8] = if default == 0 {
                b""
            } else {
                unsafe { crate::c_abi::gos_str_arg_bytes(default as usize as *const c_char) }
            };
            let stored = if let Some(v) = inner.get(&key) {
                alloc_cstring(v)
            } else {
                inner.insert(key, crate::c_abi::string::boxed_bytes(default_bytes));
                map.len_cache += 1;
                alloc_cstring(default_bytes)
            };
            drop(storage);
            if default != 0 {
                unsafe {
                    crate::c_abi::string::consume_moved_string_typed(
                        default as usize as *mut c_char,
                    );
                }
            }
            return stored as usize as i64;
        }
        let MapStorage::I64I64(inner) = &mut *storage else {
            return default;
        };
        if let Some(v) = inner.get(&key) {
            return *v;
        }
        inner.insert(key, default);
        map.len_cache += 1;
        default
    })
}

/// `m.insert(k: i64, v: String)` - `HashMap<i64, String>` insert.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_i64_str(m: *mut GosMap, key: i64, val: *const c_char) {
    ffi_entry!((), {
        if m.is_null() || val.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        let val_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(val) };
        let mut storage = map.storage.lock();
        if matches!(*storage, MapStorage::Empty) {
            *storage = MapStorage::I64Str(FxHashMap::default());
        }
        let MapStorage::I64Str(inner) = &mut *storage else {
            return;
        };
        if inner
            .insert(key, crate::c_abi::string::boxed_bytes(val_bytes))
            .is_none()
        {
            map.len_cache += 1;
        }
        drop(storage);
        // Consuming insert copied the value bytes; release the moved-in gos-string.
        unsafe { crate::c_abi::string::consume_moved_string(val.cast_mut()) };
    });
}

/// `m.get(k: i64) -> String` - returns an empty string when absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_i64_str(m: *const GosMap, key: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return empty_cstring();
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let MapStorage::I64Str(inner) = &*storage else {
            return empty_cstring();
        };
        match inner.get(&key) {
            Some(v) => alloc_cstring(v),
            None => empty_cstring(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_clear(m: *mut GosMap) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        if map_has_owned_values(map) {
            match &*storage {
                MapStorage::I64I64(inner) => {
                    for &v in inner.values() {
                        unsafe { release_owned_value(map, v) };
                    }
                }
                MapStorage::StrI64(inner) => {
                    for &v in inner.values() {
                        unsafe { release_owned_value(map, v) };
                    }
                }
                MapStorage::SkeyVal { entries, .. } => {
                    for &v in entries.values() {
                        unsafe { release_owned_value(map, v) };
                    }
                }
                MapStorage::EkeyVal { entries } => {
                    for entry in entries.values() {
                        unsafe { release_owned_value(map, entry.value) };
                    }
                }
                _ => {}
            }
        }
        // An enum-keyed map owns a share of every key node it stored,
        // independently of whether its values are owned.
        if let MapStorage::EkeyVal { entries } = &*storage {
            for entry in entries.values() {
                unsafe { crate::c_abi::rc::gos_rt_rc_release(entry.key_node) };
            }
        }
        *storage = MapStorage::Empty;
        map.len_cache = 0;
    });
}

/// Renders `count` elements starting at tag index `tag_cursor` and slot
/// index `slot_cursor`, advancing both past what it consumed.
///
/// A nested tuple's elements are flattened into the parent's slot
/// buffer, so slot and tag positions advance independently.
/// Renders one value word per the tuple tag encoding. Container tags read
/// the word as a handle; a tag with no handle shape renders as an integer.
pub(crate) unsafe fn render_tagged_word(out: &mut String, word: i64, tag: u8) {
    match tag {
        1 => out.push_str(&crate::builtins::format_uint(word as u64)),
        2 => out.push_str(&crate::builtins::format_float_debug(f64::from_bits(
            word as u64,
        ))),
        3 => out.push_str(crate::builtins::format_bool(word & 1 != 0)),
        4 => {
            if let Some(c) = char::from_u32(word as u32) {
                out.push(c);
            }
        }
        5 => {
            let sp: *const c_char = std::ptr::with_exposed_provenance(word as usize);
            if !sp.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(sp) });
            }
        }
        6 => {
            let vp = std::ptr::with_exposed_provenance(word as usize);
            let rendered = unsafe { crate::c_abi::gos_rt_vec_format_i64(vp, 0) };
            if !rendered.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
            }
        }
        7 => {
            let mp = std::ptr::with_exposed_provenance(word as usize);
            let rendered = unsafe { gos_rt_map_format(mp) };
            if !rendered.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
            }
        }
        _ => out.push_str(&crate::builtins::format_int(word)),
    }
}

/// How many slots the descriptor at `cursor` occupies where it is stored
/// inline, leaving the cursor untouched. A handle - a `Vec`, a `Map`, a
/// `Set`, an error - is one word wherever it is reached from; a fixed array
/// is its element span repeated, and a nested tuple is its elements'.
unsafe fn desc_slot_span(tags: DescStream, cursor: usize) -> usize {
    let mut c = cursor;
    unsafe { desc_slot_span_walk(tags, &mut c) }
}

unsafe fn desc_slot_span_walk(tags: DescStream, cursor: &mut usize) -> usize {
    let tag = tags.byte(*cursor);
    *cursor += 1;
    match tag {
        TUPLE_TAG_NESTED => {
            let arity = tags.byte(*cursor) as usize;
            *cursor += 1;
            let mut total = 0usize;
            for _ in 0..arity {
                total += unsafe { desc_slot_span_walk(tags, cursor) };
            }
            total
        }
        gossamer_abi::DESC_ARRAY => {
            let count = u16::from_le_bytes([tags.byte(*cursor), tags.byte(*cursor + 1)]) as usize;
            let span = (u16::from_le_bytes([tags.byte(*cursor + 2), tags.byte(*cursor + 3)])
                as usize)
                .max(1);
            *cursor += 4;
            unsafe { skip_desc(tags, cursor) };
            count * span
        }
        gossamer_abi::DESC_ADT => {
            let slots = (tags.byte(*cursor + 2) as usize).max(1);
            *cursor += 3;
            slots
        }
        gossamer_abi::DESC_OPTION => {
            unsafe { skip_desc(tags, cursor) };
            2
        }
        gossamer_abi::DESC_RESULT => {
            unsafe { skip_desc(tags, cursor) };
            unsafe { skip_desc(tags, cursor) };
            2
        }
        _ => {
            *cursor -= 1;
            unsafe { skip_desc(tags, cursor) };
            1
        }
    }
}

/// Advances `cursor` past one descriptor without rendering it.
unsafe fn skip_desc(tags: DescStream, cursor: &mut usize) {
    let tag = tags.byte(*cursor);
    *cursor += 1;
    match tag {
        TUPLE_TAG_NESTED => {
            let arity = tags.byte(*cursor) as usize;
            *cursor += 1;
            for _ in 0..arity {
                unsafe { skip_desc(tags, cursor) };
            }
        }
        gossamer_abi::DESC_VEC => unsafe { skip_desc(tags, cursor) },
        gossamer_abi::DESC_MAP => unsafe {
            skip_desc(tags, cursor);
            skip_desc(tags, cursor);
        },
        gossamer_abi::DESC_SET_I64 | gossamer_abi::DESC_SET_STR => *cursor += 1,
        gossamer_abi::DESC_CONTAINER => {
            // The byte naming the container, then one element descriptor.
            *cursor += 1;
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ADT => *cursor += 3,
        gossamer_abi::DESC_OPTION => unsafe { skip_desc(tags, cursor) },
        gossamer_abi::DESC_ARRAY => {
            // Element count and per-element slot span, a `u16` each, then
            // one element descriptor.
            *cursor += 4;
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ERROR => {}
        gossamer_abi::DESC_RESULT => unsafe {
            skip_desc(tags, cursor);
            skip_desc(tags, cursor);
        },
        _ => {}
    }
}

/// Renders a tuple whose fields are described by a descriptor stream, for a
/// field that no flat tag names - a struct, an enum, or a container of them.
/// The stream opens with the nested-tuple marker and the arity, so the slot
/// buffer needs no separate count.
///
/// # Safety
/// `slots` addresses the tuple's slot buffer and `desc` a descriptor global.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tuple_format_desc(
    slots: *const i64,
    desc: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if slots.is_null() || desc.is_null() {
            return alloc_cstring(b"()");
        }
        let tags = unsafe { DescStream::new(desc) };
        let mut out = String::new();
        let mut cursor = 0usize;
        unsafe { render_desc_value(&mut out, slots.cast::<u8>(), tags, &mut cursor) };
        alloc_cstring(out.as_bytes())
    })
}

/// A descriptor stream as the codegen lays it out: an 8-byte count of the
/// per-type `fmt` pointers that follow, those pointers, then the descriptor
/// bytes. A `DESC_ADT` byte names one of the pointers by index, so a user
/// struct or enum nested anywhere in a shape renders through the same
/// derived formatter a bare `{:?}` on it calls.
#[derive(Clone, Copy)]
pub(crate) struct DescStream {
    bytes: *const u8,
    fns: *const *const std::ffi::c_void,
    fn_count: usize,
}

impl DescStream {
    /// A stream of tag bytes with no formatter table, which is what a plain
    /// tuple tag stream is.
    pub(crate) fn bare(bytes: *const u8) -> Self {
        Self {
            bytes,
            fns: std::ptr::null(),
            fn_count: 0,
        }
    }

    /// # Safety
    /// `base` addresses a descriptor global emitted by the native backend.
    pub(crate) unsafe fn new(base: *const u8) -> Self {
        let fn_count = unsafe { base.cast::<i64>().read_unaligned() }.max(0) as usize;
        Self {
            fns: unsafe { base.add(8) }.cast(),
            bytes: unsafe { base.add(8 + fn_count * 8) },
            fn_count,
        }
    }

    fn byte(self, at: usize) -> u8 {
        // SAFETY: cursors only ever address bytes of the stream this view
        // was built over.
        unsafe { *self.bytes.add(at) }
    }

    /// The little-endian `u16` two bytes of the stream carry, for a
    /// descriptor field that outgrows one byte.
    fn u16(self, at: usize) -> u16 {
        u16::from_le_bytes([self.byte(at), self.byte(at + 1)])
    }

    fn fmt(self, index: usize) -> Option<*const std::ffi::c_void> {
        // SAFETY: the index is bounds-checked against the emitted count.
        (index < self.fn_count).then(|| unsafe { *self.fns.add(index) })
    }
}

/// Renders the value at `slot` per the descriptor at `cursor`, advancing the
/// cursor past that descriptor. A container descriptor reads the slot as a
/// handle and renders its elements through the descriptor that follows, so a
/// nested shape needs no formatter of its own.
pub(crate) unsafe fn render_desc_value(
    out: &mut String,
    slot: *const u8,
    tags: DescStream,
    cursor: &mut usize,
) {
    unsafe { render_desc_storage(out, slot, tags, cursor, Storage::Inline) };
}

/// Where a descriptor's value lives relative to the slot it is reached from.
/// A single-word value reads the same either way; the distinction matters for
/// a multi-word one - an `Option` / `Result` pair, or a struct's flat slots.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Storage {
    /// The value's own bytes begin at the slot.
    Inline,
    /// The slot holds a word addressing the value.
    ByWord,
}

pub(crate) unsafe fn render_desc_storage(
    out: &mut String,
    slot: *const u8,
    tags: DescStream,
    cursor: &mut usize,
    storage: Storage,
) {
    let tag = tags.byte(*cursor);
    match tag {
        TUPLE_TAG_NESTED => {
            *cursor += 1;
            let arity = tags.byte(*cursor) as usize;
            *cursor += 1;
            // A tuple reached as another value's payload - an `Option` /
            // `Result` arm - is boxed, so the slot holds a pointer to its
            // flat words; an inline tuple begins at the slot itself.
            let base: *const i64 = if storage == Storage::Inline {
                slot.cast::<i64>()
            } else {
                let word = unsafe { (slot as *const i64).read_unaligned() };
                std::ptr::with_exposed_provenance(word as usize)
            };
            if base.is_null() {
                out.push_str("()");
            } else {
                let mut slot_cursor = 0usize;
                unsafe {
                    render_tuple_elements(out, base, tags, arity, &mut slot_cursor, cursor);
                }
            }
        }
        gossamer_abi::DESC_VEC => {
            *cursor += 1;
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let v: *const crate::c_abi::GosVec = std::ptr::with_exposed_provenance(word as usize);
            let elem_desc = *cursor;
            // A `Vec` renders in its own literal spelling; the bare
            // bracket belongs to the fixed array it shares a runtime
            // representation with, which `DESC_ARRAY` names.
            out.push_str("#[");
            if !v.is_null() {
                let vec = unsafe { &*v };
                for i in 0..vec.len {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let elem = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
                    let mut c = elem_desc;
                    unsafe { render_desc_value(out, elem, tags, &mut c) };
                }
            }
            out.push(']');
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_MAP => {
            *cursor += 1;
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let m: *const GosMap = std::ptr::with_exposed_provenance(word as usize);
            let key_desc = *cursor;
            unsafe { skip_desc(tags, cursor) };
            let val_desc = *cursor;
            unsafe { skip_desc(tags, cursor) };
            let rendered = unsafe { map_format_desc_stream(m, tags, key_desc, val_desc) };
            if rendered.is_null() {
                out.push_str("{}");
            } else {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
            }
        }
        gossamer_abi::DESC_ARRAY => {
            *cursor += 1;
            let len = tags.u16(*cursor) as usize;
            *cursor += 2;
            let elem_slots = (tags.u16(*cursor) as usize).max(1);
            *cursor += 2;
            // The elements sit inline from the slot, so each reads through
            // the one descriptor that follows, at its own offset.
            let base = if storage == Storage::Inline {
                slot
            } else {
                let word = unsafe { (slot as *const i64).read_unaligned() };
                std::ptr::with_exposed_provenance::<u8>(word as usize)
            };
            let elem_desc = *cursor;
            out.push('[');
            for i in 0..len {
                if i > 0 {
                    out.push_str(", ");
                }
                let mut c = elem_desc;
                let elem = unsafe { base.add(i * elem_slots * 8) };
                unsafe { render_desc_value(out, elem, tags, &mut c) };
            }
            out.push(']');
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ERROR => {
            *cursor += 1;
            let word = unsafe { (slot as *const i64).read_unaligned() };
            if word == 0 {
                return;
            }
            let rendered = unsafe {
                crate::c_abi::gos_rt_error_display(std::ptr::with_exposed_provenance(word as usize))
            };
            if !rendered.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
            }
        }
        gossamer_abi::DESC_RESULT | gossamer_abi::DESC_OPTION => {
            *cursor += 1;
            let is_option = tag == gossamer_abi::DESC_OPTION;
            // The two words are the discriminant then the selected arm's
            // payload: laid out at the slot where the value is stored inline
            // (a `Vec` element, a set member), or behind the slot's word
            // where it is another value's payload.
            let pair: *const i64 = if storage == Storage::Inline {
                slot.cast::<i64>()
            } else {
                let word = unsafe { (slot as *const i64).read_unaligned() };
                std::ptr::with_exposed_provenance(word as usize)
            };
            let (disc, payload) = if pair.is_null() {
                (0i64, 0i64)
            } else {
                unsafe { (pair.read_unaligned(), pair.add(1).read_unaligned()) }
            };
            let first_desc = *cursor;
            unsafe { skip_desc(tags, cursor) };
            let second_desc = *cursor;
            if !is_option {
                unsafe { skip_desc(tags, cursor) };
            }
            let arm_desc = if disc == 0 { first_desc } else { second_desc };
            out.push_str(match (is_option, disc) {
                (true, 0) => "Some(",
                (true, _) => "None",
                (false, 0) => "Ok(",
                (false, _) => "Err(",
            });
            if is_option && disc != 0 {
                return;
            }
            let mut arm_cursor = arm_desc;
            unsafe {
                render_desc_storage(
                    out,
                    std::ptr::addr_of!(payload).cast::<u8>(),
                    tags,
                    &mut arm_cursor,
                    Storage::ByWord,
                );
            }
            out.push(')');
        }
        gossamer_abi::DESC_ADT => {
            *cursor += 1;
            let index = tags.byte(*cursor) as usize;
            *cursor += 1;
            let by_slot_address = tags.byte(*cursor) != 0;
            *cursor += 2;
            let Some(fmt) = tags.fmt(index) else {
                return;
            };
            let arg = if by_slot_address && storage == Storage::Inline {
                slot
            } else {
                let word = unsafe { (slot as *const usize).read_unaligned() };
                std::ptr::with_exposed_provenance::<u8>(word)
            };
            out.push_str(&unsafe { crate::c_abi::vec::adt_fmt_string(arg, fmt) });
        }
        // A container whose elements live in the runtime: the slot holds
        // the handle, and the byte after the tag names which container.
        gossamer_abi::DESC_CONTAINER => {
            *cursor += 1;
            let which = tags.byte(*cursor);
            *cursor += 1;
            let elem_desc = *cursor;
            unsafe { skip_desc(tags, cursor) };
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let rendered = match which {
                28 | 30 => {
                    let owner = if which == 28 { "MaxHeap" } else { "MinHeap" };
                    let handle: *const crate::c_abi::GosVec =
                        std::ptr::with_exposed_provenance(word as usize);
                    unsafe {
                        crate::c_abi::container_heap::bheap_format_at(
                            handle, owner, tags, elem_desc,
                        )
                    }
                }
                _ => {
                    let owner = match which {
                        31 => "Queue",
                        32 => "Stack",
                        _ => "Deque",
                    };
                    let handle: *mut crate::c_abi::deque::GosDeque =
                        std::ptr::with_exposed_provenance_mut(word as usize);
                    unsafe { crate::c_abi::deque::deque_format_at(handle, owner, tags, elem_desc) }
                }
            };
            out.push_str(&rendered);
        }
        gossamer_abi::DESC_SET_I64 | gossamer_abi::DESC_SET_STR => {
            *cursor += 1;
            let ordered = i32::from(tags.byte(*cursor));
            *cursor += 1;
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let handle = std::ptr::with_exposed_provenance(word as usize);
            let rendered = if tag == gossamer_abi::DESC_SET_I64 {
                unsafe { crate::c_abi::gos_rt_set_format_i64(handle, ordered) }
            } else {
                unsafe { crate::c_abi::gos_rt_set_format_string(handle, ordered) }
            };
            if rendered.is_null() {
                out.push_str("#{}");
            } else {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
            }
        }
        _ => {
            *cursor += 1;
            let word = unsafe { (slot as *const i64).read_unaligned() };
            unsafe { render_tagged_word(out, word, tag) };
        }
    }
}

pub(crate) unsafe fn render_tuple_elements(
    out: &mut String,
    p: *const i64,
    tags: DescStream,
    count: usize,
    slot_cursor: &mut usize,
    tag_cursor: &mut usize,
) {
    out.push('(');
    for i in 0..count {
        if i > 0 {
            out.push_str(", ");
        }
        let tag = tags.byte(*tag_cursor);
        *tag_cursor += 1;
        if tag == TUPLE_TAG_NESTED {
            let nested = tags.byte(*tag_cursor) as usize;
            *tag_cursor += 1;
            unsafe { render_tuple_elements(out, p, tags, nested, slot_cursor, tag_cursor) };
            continue;
        }
        // A struct or enum field is stored inline across its own slots, so it
        // renders from the address of the first and advances the cursor by
        // however many the descriptor says it spans.
        if tag == gossamer_abi::DESC_ADT {
            let index = tags.byte(*tag_cursor) as usize;
            let by_slot_address = tags.byte(*tag_cursor + 1) != 0;
            let slots = (tags.byte(*tag_cursor + 2) as usize).max(1);
            *tag_cursor += 3;
            let field = unsafe { p.add(*slot_cursor) };
            *slot_cursor += slots;
            if let Some(fmt) = tags.fmt(index) {
                let arg = if by_slot_address {
                    field.cast::<u8>()
                } else {
                    let word = unsafe { field.read_unaligned() };
                    std::ptr::with_exposed_provenance::<u8>(word as usize)
                };
                out.push_str(&unsafe { crate::c_abi::vec::adt_fmt_string(arg, fmt) });
            }
            continue;
        }
        // A leaf tag names one slot and one of the shapes below. Anything
        // else is a whole descriptor - a `Vec`, a `Map`, a `Set`, an array,
        // an `Option` - which the descriptor walk renders and measures, so
        // both cursors stay on the element that follows.
        if !matches!(tag, 0..=7) {
            let element = unsafe { p.add(*slot_cursor) };
            *tag_cursor -= 1;
            *slot_cursor += unsafe { desc_slot_span(tags, *tag_cursor) };
            unsafe { render_desc_value(out, element.cast::<u8>(), tags, tag_cursor) };
            continue;
        }
        let word = unsafe { p.add(*slot_cursor).read_unaligned() };
        *slot_cursor += 1;
        match tag {
            0 => out.push_str(&crate::builtins::format_int(word)),
            1 => out.push_str(&crate::builtins::format_uint(word as u64)),
            2 => out.push_str(&crate::builtins::format_float_debug(f64::from_bits(
                word as u64,
            ))),
            3 => out.push_str(crate::builtins::format_bool(word & 1 != 0)),
            4 => {
                if let Some(c) = char::from_u32(word as u32) {
                    out.push(c);
                }
            }
            5 => {
                let sp: *const c_char = std::ptr::with_exposed_provenance(word as usize);
                if !sp.is_null() {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(sp) });
                }
            }
            6 => {
                let vp = std::ptr::with_exposed_provenance(word as usize);
                let rendered = unsafe { crate::c_abi::gos_rt_vec_format_i64(vp, 0) };
                if !rendered.is_null() {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                }
            }
            7 => {
                let mp = std::ptr::with_exposed_provenance(word as usize);
                let rendered = unsafe { gos_rt_map_format(mp) };
                if !rendered.is_null() {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                }
            }
            _ => {}
        }
    }
    if count == 1 {
        out.push(',');
    }
    out.push(')');
}

/// Renders a tuple's flat slot buffer to `(a, b, …)` (a 1-tuple
/// gets a trailing comma, `(a,)`), matching the VM's `Display`.
/// `p` points at the tuple's contiguous 8-byte slots and `n` is its
/// element count; the `tags` stream selects how each element is
/// interpreted: `0` = Int, `2` = Float (the slot's bits are an `f64`),
/// `3` = Bool (low bit), `4` = Char (low 32 bits as a code point),
/// `5` = Str (the slot is a c-string pointer), `6` = `Vec<i64>`,
/// `7` = HashMap, `8` = a nested tuple whose element count is the next
/// tag byte and whose own tags follow it. A nested tuple's slots are
/// flattened into the parent buffer, so the stream is walked with
/// separate tag and slot cursors. Integers and floats route through
/// `crate::builtins::format_int` / `format_float` so the rendering is
/// byte-identical to the VM.
///
/// The tag stream is emitted by the compiler alongside `n` and is
/// self-describing given `n`; a caller-supplied stream must match the
/// tuple's shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tuple_format(
    p: *const i64,
    n: i64,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || tags.is_null() || n <= 0 {
            return alloc_cstring(b"()");
        }
        let mut out = String::new();
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        unsafe {
            render_tuple_elements(
                &mut out,
                p,
                DescStream::bare(tags),
                n as usize,
                &mut slot_cursor,
                &mut tag_cursor,
            );
        }
        alloc_cstring(out.as_bytes())
    })
}

/// Lexicographically compares two tuples' flat slot buffers, returning
/// `-1` / `0` / `1`. `a` and `b` point at `n` contiguous 8-byte slots;
/// `tags[i]` selects each slot's kind (same encoding as
/// [`gos_rt_tuple_format`]: `0` Int, `2` Float, `3` Bool, `4` Char, `5`
/// Str). The first non-equal element decides; equal prefixes continue.
/// Routed to by the compiled tiers for tuple `== != < <= > >=`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tuple_cmp(
    a: *const i64,
    b: *const i64,
    n: i64,
    tags: *const u8,
) -> i64 {
    ffi_entry!(0, {
        if a.is_null() || b.is_null() || tags.is_null() || n <= 0 {
            return 0;
        }
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        unsafe { compare_tuple_elements(a, b, tags, n as usize, &mut slot_cursor, &mut tag_cursor) }
    })
}

/// Compares `count` elements starting at tag index `tag_cursor` and slot
/// index `slot_cursor`, advancing both past what it consumed. Returns
/// `-1` / `0` / `1`; the cursors are left past the compared elements
/// either way so a caller can keep walking.
unsafe fn compare_tuple_elements(
    a: *const i64,
    b: *const i64,
    tags: *const u8,
    count: usize,
    slot_cursor: &mut usize,
    tag_cursor: &mut usize,
) -> i64 {
    use std::cmp::Ordering;
    let mut result = 0i64;
    for _ in 0..count {
        let tag = unsafe { *tags.add(*tag_cursor) };
        // A descriptor tag names a value whose slot word is not its own
        // order - an enum reached through its RC node, a nested sequence -
        // so it is compared through the ordering descriptor rather than as
        // the word the slot spells.
        if tag >= gossamer_abi::DESC_VEC {
            let field = *tag_cursor;
            let span = unsafe { crate::c_abi::desc_cmp::desc_slot_span(tags, field) };
            let mut walk = field;
            let ord = unsafe {
                crate::c_abi::desc_cmp::compare_desc(
                    a.add(*slot_cursor).cast::<u8>(),
                    b.add(*slot_cursor).cast::<u8>(),
                    tags,
                    &mut walk,
                    crate::c_abi::desc_cmp::CmpStorage::Inline,
                    None,
                )
            };
            *tag_cursor = walk;
            *slot_cursor += span;
            if result == 0 {
                result = ord;
            }
            continue;
        }
        *tag_cursor += 1;
        if tag == TUPLE_TAG_NESTED {
            let nested = unsafe { *tags.add(*tag_cursor) } as usize;
            *tag_cursor += 1;
            let ord =
                unsafe { compare_tuple_elements(a, b, tags, nested, slot_cursor, tag_cursor) };
            if result == 0 {
                result = ord;
            }
            continue;
        }
        let wa = unsafe { a.add(*slot_cursor).read_unaligned() };
        let wb = unsafe { b.add(*slot_cursor).read_unaligned() };
        *slot_cursor += 1;
        if result != 0 {
            continue;
        }
        let ord = match tag {
            2 => f64::from_bits(wa as u64)
                .partial_cmp(&f64::from_bits(wb as u64))
                .unwrap_or(Ordering::Equal),
            3 => (wa & 1).cmp(&(wb & 1)),
            4 => (wa as u32).cmp(&(wb as u32)),
            5 => {
                let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                unsafe { gos_rt_str_compare(sa, sb) }.cmp(&0)
            }
            _ => wa.cmp(&wb),
        };
        result = match ord {
            Ordering::Less => -1,
            Ordering::Greater => 1,
            Ordering::Equal => 0,
        };
    }
    result
}

/// Sorts `len` tuple elements of `stride` bytes each, in place and
/// ascending, comparing with [`gos_rt_tuple_cmp`] under the `n`-element
/// `tags` stream.
unsafe fn sort_tuple_buffer(base: *mut u8, len: usize, stride: usize, n: i64, tags: *const u8) {
    if len <= 1 || stride == 0 {
        return;
    }
    // Rank indices, then permute through a temp buffer: the same shape
    // as `gos_rt_arr_sort_by_aggr`, and it keeps the comparator's
    // operand pointers stable across swaps.
    let mut indices: Vec<usize> = (0..len).collect();
    indices.sort_by(|&ai, &bi| {
        let pa = unsafe { base.add(ai * stride).cast::<i64>() };
        let pb = unsafe { base.add(bi * stride).cast::<i64>() };
        unsafe { gos_rt_tuple_cmp(pa, pb, n, tags) }.cmp(&0)
    });
    let total = len.saturating_mul(stride);
    let mut tmp: Vec<u8> = vec![0u8; total];
    for (new_idx, &old_idx) in indices.iter().enumerate() {
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(old_idx * stride),
                tmp.as_mut_ptr().add(new_idx * stride),
                stride,
            );
        }
    }
    unsafe {
        std::ptr::copy_nonoverlapping(tmp.as_ptr(), base, total);
    }
}

/// Sorts a `Vec` of tuple elements in place, ascending, per the
/// `n`-element `tags` stream. Element stride comes from the vec's
/// `elem_bytes` header field. Routed to by `xs.sort()` when the element
/// type is a tuple, where a plain slot-wise i64 sort would reorder the
/// flattened slots rather than the tuples they belong to.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_sort_tuple(v: *mut GosVec, n: i64, tags: *const u8) {
    ffi_entry!((), {
        if v.is_null() || tags.is_null() || n <= 0 {
            return;
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 1 || vec.ptr.is_null() {
            return;
        }
        let len = vec.len.max(0) as usize;
        let stride = i64::from(vec.elem_bytes).max(0) as usize;
        unsafe { sort_tuple_buffer(vec.ptr.as_ptr(), len, stride, n, tags) };
    });
}

/// Sorts a fixed-size array of tuple elements in place, ascending, per
/// the `n`-element `tags` stream. The flat-buffer sibling of
/// [`gos_rt_vec_sort_tuple`]: `p` points straight at the elements, so
/// `len` and `elem_bytes` are passed rather than read from a header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_sort_tuple(
    p: *mut u8,
    len: i64,
    elem_bytes: i64,
    n: i64,
    tags: *const u8,
) {
    ffi_entry!((), {
        if p.is_null() || tags.is_null() || n <= 0 || len <= 1 || elem_bytes <= 0 {
            return;
        }
        unsafe { sort_tuple_buffer(p, len as usize, elem_bytes as usize, n, tags) };
    });
}

/// Structural equality of two Vec/array values. `elem_tag` (same encoding
/// as [`gos_rt_tuple_cmp`]) selects how each element slot is interpreted:
/// `2` Float (bit-equal would mishandle NaN), `5` Str (per-element
/// `gos_rt_str_eq`), anything else a plain word compare. Routed to by the
/// compiled tiers for `[T] == [T]` / `!=`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_eq(a: *const GosVec, b: *const GosVec, elem_tag: u8) -> bool {
    ffi_entry!(false, {
        if a.is_null() || b.is_null() {
            return std::ptr::eq(a, b);
        }
        let la = unsafe { (*a).len };
        if la != unsafe { (*b).len } {
            return false;
        }
        for i in 0..la {
            let wa = unsafe { gos_rt_vec_get_i64(a, i) };
            let wb = unsafe { gos_rt_vec_get_i64(b, i) };
            let eq = match elem_tag {
                2 => f64::from_bits(wa as u64) == f64::from_bits(wb as u64),
                5 => {
                    let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                    let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                    unsafe { gos_rt_str_eq(sa, sb) }
                }
                _ => wa == wb,
            };
            if !eq {
                return false;
            }
        }
        true
    })
}

/// Structural equality of two heap (recursive / `Box`) enum nodes, driven by
/// a per-enum descriptor blob so equal-but-distinct allocations compare true
/// (matching the VM's `values_equal`) instead of by pointer identity. The
/// compiled tiers route heap-enum `==` / `!=` here.
///
/// `desc` (pure `i64`): `[num_variants]` then, per variant in discriminant
/// order, `[num_fields, kind_0, .., kind_{n-1}]`. Field kinds: `0` word
/// (int / bool / char), `1` `f64`, `2` `String`, `3` nested self-enum
/// (recurse with the same `desc`), `4` `Vec<self-enum>`, `5`
/// `Vec<(String, self-enum)>`. The codegen emits a descriptor - and routes
/// here - only when every nested enum field is the same type, so a mismatched
/// sub-shape never reaches this walk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_enum_struct_eq(a: *mut u8, b: *mut u8, desc: *const i64) -> i64 {
    ffi_entry!(0, {
        let raw_a = a as usize;
        let raw_b = b as usize;
        let a = crate::c_abi::rc::untag_rc(a);
        let b = crate::c_abi::rc::untag_rc(b);
        if std::ptr::eq(a, b) {
            return 1; // same node, or both null
        }
        if a.is_null() || b.is_null() || desc.is_null() {
            return 0;
        }
        // Discriminant: a small heap enum tags it into the pointer's low bits
        // (`base | (disc << 1)`); a larger one stores it in the RcHeader byte at
        // payload-3. A zero tag means disc 0 or the header form - the header
        // then holds the value (0 for a tagged disc-0 node too, so both agree).
        let disc_of = |raw: usize, base: *mut u8| -> u8 {
            let tag = raw & 7;
            if tag != 0 {
                (tag >> 1) as u8
            } else {
                unsafe { *base.sub(3) }
            }
        };
        let da = disc_of(raw_a, a);
        let db = disc_of(raw_b, b);
        if da != db {
            return 0;
        }
        let num_variants = unsafe { *desc };
        if i64::from(da) >= num_variants {
            return 0;
        }
        let mut idx = 1usize;
        for _ in 0..da {
            let nf = unsafe { *desc.add(idx) }.max(0);
            idx += 1 + nf as usize;
        }
        let nf = unsafe { *desc.add(idx) }.max(0);
        idx += 1;
        for f in 0..nf {
            let kind = unsafe { *desc.add(idx + f as usize) };
            let wa = unsafe { *(a as *const i64).add(f as usize) };
            let wb = unsafe { *(b as *const i64).add(f as usize) };
            let eq = match kind {
                1 => f64::from_bits(wa as u64) == f64::from_bits(wb as u64),
                2 => {
                    let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                    let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                    unsafe { gos_rt_str_eq(sa, sb) }
                }
                3 => unsafe { gos_rt_enum_struct_eq(wa as *mut u8, wb as *mut u8, desc) != 0 },
                4 => unsafe { vec_self_enum_eq(wa, wb, desc) },
                5 => unsafe { vec_str_self_enum_eq(wa, wb, desc) },
                _ => wa == wb,
            };
            if !eq {
                return 0;
            }
        }
        1
    })
}

/// Element-wise structural equality of two `Vec<self-enum>` field words (each
/// a `*mut GosVec` of 8-byte enum-pointer slots), recursing per element.
unsafe fn vec_self_enum_eq(a_word: i64, b_word: i64, desc: *const i64) -> bool {
    let va = a_word as *const GosVec;
    let vb = b_word as *const GosVec;
    if std::ptr::eq(va, vb) {
        return true;
    }
    if va.is_null() || vb.is_null() {
        return false;
    }
    let la = unsafe { (*va).len };
    if la != unsafe { (*vb).len } {
        return false;
    }
    for i in 0..la {
        let ea = unsafe { gos_rt_vec_get_i64(va, i) };
        let eb = unsafe { gos_rt_vec_get_i64(vb, i) };
        if unsafe { gos_rt_enum_struct_eq(ea as *mut u8, eb as *mut u8, desc) } == 0 {
            return false;
        }
    }
    true
}

/// Element-wise structural equality of two `Vec<(String, self-enum)>` field
/// words (each a `*mut GosVec` of 16-byte `[cstr @ +0][enum ptr @ +8]` slots).
unsafe fn vec_str_self_enum_eq(a_word: i64, b_word: i64, desc: *const i64) -> bool {
    let va = a_word as *const GosVec;
    let vb = b_word as *const GosVec;
    if std::ptr::eq(va, vb) {
        return true;
    }
    if va.is_null() || vb.is_null() {
        return false;
    }
    let la = unsafe { (*va).len };
    if la != unsafe { (*vb).len } {
        return false;
    }
    for i in 0..la {
        let pa = unsafe { gos_rt_vec_get_ptr(va, i) };
        let pb = unsafe { gos_rt_vec_get_ptr(vb, i) };
        if pa.is_null() || pb.is_null() {
            if pa != pb {
                return false;
            }
            continue;
        }
        let ka = unsafe { pa.cast::<i64>().read_unaligned() };
        let kb = unsafe { pb.cast::<i64>().read_unaligned() };
        let sa: *const c_char = std::ptr::with_exposed_provenance(ka as usize);
        let sb: *const c_char = std::ptr::with_exposed_provenance(kb as usize);
        if !unsafe { gos_rt_str_eq(sa, sb) } {
            return false;
        }
        let ea = unsafe { pa.add(8).cast::<i64>().read_unaligned() };
        let eb = unsafe { pb.add(8).cast::<i64>().read_unaligned() };
        if unsafe { gos_rt_enum_struct_eq(ea as *mut u8, eb as *mut u8, desc) } == 0 {
            return false;
        }
    }
    true
}

/// Renders a `HashMap` to `{k: v, k2: v2}`, sorting entries by key so
/// the output is deterministic and byte-identical across tiers (an
/// `FxHashMap`'s bucket order is neither stable nor the same as the
/// VM's). Keys and values render the way the VM's `Display` does -
/// integers via `format_int`; string keys are quoted while string values
/// remain bare. Empty maps and storage
/// shapes whose values aren't scalar (struct-keyed / byte-erased)
/// render as `{}`; the codegen only routes scalar-keyed, scalar- or
/// string-valued maps here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_format(m: *const GosMap) -> *mut c_char {
    unsafe { gos_rt_map_format_tagged(m, 0, 0, std::ptr::null(), 0) }
}

/// Renders one map value word. An aggregate tag reads the word as the address
/// of the value's slot buffer: `9` renders it through the derived `fmt` in
/// `aux`, and `8` through the `aux_n` tuple tags `aux` addresses.
unsafe fn render_map_value(out: &mut String, word: i64, val_tag: i64, aux: *const u8, aux_n: i64) {
    if aux.is_null() {
        unsafe { render_tagged_word(out, word, val_tag as u8) };
        return;
    }
    if val_tag == i64::from(gossamer_abi::DEBUG_PAYLOAD_ADT) {
        let slots: *const u8 = std::ptr::with_exposed_provenance(word as usize);
        out.push_str(&unsafe { crate::c_abi::vec::adt_fmt_string(slots, aux.cast()) });
        return;
    }
    if val_tag == i64::from(TUPLE_TAG_NESTED) {
        let slots: *const i64 = std::ptr::with_exposed_provenance(word as usize);
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        unsafe {
            render_tuple_elements(
                out,
                slots,
                DescStream::bare(aux),
                aux_n as usize,
                &mut slot_cursor,
                &mut tag_cursor,
            );
        }
        return;
    }
    unsafe { render_tagged_word(out, word, val_tag as u8) };
}

/// Renders a `GosMap` whose keys and values are described by the descriptors
/// at `key_desc` and `val_desc` inside `tags`, so a nested container value
/// renders through the same walk rather than needing its own entry point.
///
/// # Safety
/// `m` is a live `GosMap`, and `tags` addresses descriptors at both offsets.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_format_desc(
    m: *const GosMap,
    tags: *const u8,
    key_desc: i64,
    val_desc: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() || tags.is_null() {
            return alloc_cstring(b"{}");
        }
        let tags = unsafe { DescStream::new(tags) };
        unsafe { map_format_desc_stream(m, tags, key_desc as usize, val_desc as usize) }
    })
}

/// [`gos_rt_map_format_desc`] over an already-parsed stream, so the recursive
/// walk reaches a nested map without re-reading the stream header.
unsafe fn map_format_desc_stream(
    m: *const GosMap,
    tags: DescStream,
    key_desc: usize,
    val_desc: usize,
) -> *mut c_char {
    {
        let aggregate = unsafe { map_aggregate_entries(m) };
        if !aggregate.is_empty() {
            let mut out = String::from("{");
            for (index, entry) in aggregate.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let mut c = key_desc;
                let storage = if entry.key_by_word {
                    Storage::ByWord
                } else {
                    Storage::Inline
                };
                unsafe {
                    render_desc_storage(
                        &mut out,
                        entry.key_slots.as_ptr().cast::<u8>(),
                        tags,
                        &mut c,
                        storage,
                    );
                }
                out.push_str(": ");
                let value = entry.value;
                let mut c = val_desc;
                unsafe {
                    render_desc_storage(
                        &mut out,
                        std::ptr::from_ref(&value).cast::<u8>(),
                        tags,
                        &mut c,
                        Storage::ByWord,
                    );
                }
                entry.release();
            }
            out.push('}');
            return alloc_cstring(out.as_bytes());
        }
        let entries = unsafe { map_word_entries(m) };
        let mut out = String::from("{");
        let mut first = true;
        for (string_key, key, value) in entries {
            if first {
                first = false;
            } else {
                out.push_str(", ");
            }
            if let Some(bytes) = string_key {
                out.push_str(&format!("{:?}", String::from_utf8_lossy(&bytes)));
            } else {
                let mut c = key_desc;
                let slot = std::ptr::from_ref(&key).cast::<u8>();
                unsafe { render_desc_value(&mut out, slot, tags, &mut c) };
            }
            out.push_str(": ");
            let mut c = val_desc;
            let slot = std::ptr::from_ref(&value).cast::<u8>();
            unsafe { render_desc_storage(&mut out, slot, tags, &mut c, Storage::ByWord) };
        }
        out.push('}');
        alloc_cstring(out.as_bytes())
    }
}

/// Key/value words of a map in deterministic key order. A string key travels
/// as its own bytes; shapes whose values are not single words yield nothing.
unsafe fn map_word_entries(m: *const GosMap) -> Vec<(Option<Vec<u8>>, i64, i64)> {
    let map = unsafe { &*m };
    let storage = map.storage.lock();
    match &*storage {
        MapStorage::I64I64(inner) => {
            let mut out: Vec<(Option<Vec<u8>>, i64, i64)> =
                inner.iter().map(|(k, v)| (None, *k, *v)).collect();
            out.sort_unstable_by_key(|(_, k, _)| *k);
            out
        }
        MapStorage::StrI64(inner) => {
            let mut out: Vec<(Option<Vec<u8>>, i64, i64)> = inner
                .iter()
                .map(|(k, v)| (Some(k.as_slice().to_vec()), 0, *v))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        _ => Vec::new(),
    }
}

/// One entry of a map keyed by an aggregate, for the descriptor walk: the
/// key's own slot buffer and the value word. A struct, tuple, or fixed array
/// key is stored content-encoded, so its slots are rebuilt from that encoding
/// exactly as `keys()` rebuilds them; an enum key is a single node word.
struct DescEntry {
    key_slots: Vec<i64>,
    /// True when the single key slot holds a word addressing the key - an
    /// enum node - rather than the key's own slots.
    key_by_word: bool,
    /// Slot indices holding a c-string this entry allocated while decoding,
    /// which the renderer releases once the entry is rendered.
    owned_strings: Vec<usize>,
    /// Slot indices holding a sequence this entry rebuilt while decoding,
    /// released the same way.
    owned_vecs: Vec<usize>,
    value: i64,
}

impl DescEntry {
    fn release(&self) {
        for &index in &self.owned_vecs {
            let ptr: *mut GosVec =
                std::ptr::with_exposed_provenance_mut(self.key_slots[index] as usize);
            if !ptr.is_null() {
                unsafe { gos_rt_vec_free(ptr) };
            }
        }
        for &index in &self.owned_strings {
            let ptr: *mut c_char =
                std::ptr::with_exposed_provenance_mut(self.key_slots[index] as usize);
            if !ptr.is_null() {
                unsafe { crate::c_abi::string::gos_rt_str_free(ptr) };
            }
        }
    }
}

/// Every entry of an aggregate-keyed map, ordered by the stored key bytes so
/// rendering is stable across runs the way the bytecode tier's is.
unsafe fn map_aggregate_entries(m: *const GosMap) -> Vec<DescEntry> {
    let map = unsafe { &*m };
    let storage = map.storage.lock();
    match &*storage {
        MapStorage::SkeyVal { entries, desc } => {
            let slots = desc.len();
            let mut keys: Vec<&ByteKey> = entries.keys().collect();
            keys.sort_by_cached_key(|key| skey_order(key.as_slice(), desc));
            keys.into_iter()
                .filter_map(|k| {
                    // A field-less key owns no slots, yet the renderer reads
                    // through the buffer's address, so it always points at
                    // storage of its own.
                    let mut key_slots = vec![0i64; slots.max(1)];
                    if !decode_skey_into(k.as_slice(), desc, &mut key_slots) {
                        return None;
                    }
                    let owned_strings = desc
                        .iter()
                        .enumerate()
                        .filter(|&(_, &code)| code == b'S')
                        .map(|(index, _)| index)
                        .collect();
                    let owned_vecs = desc
                        .iter()
                        .enumerate()
                        .filter(|&(_, &code)| code == b'V')
                        .map(|(index, _)| index)
                        .collect();
                    Some(DescEntry {
                        key_slots,
                        key_by_word: false,
                        owned_strings,
                        owned_vecs,
                        value: entries[k],
                    })
                })
                .collect()
        }
        MapStorage::EkeyVal { entries } => {
            let mut keys: Vec<&ByteKey> = entries.keys().collect();
            keys.sort_unstable();
            keys.into_iter()
                .map(|k| {
                    let entry = &entries[k];
                    DescEntry {
                        key_slots: vec![entry.key_node as usize as i64],
                        key_by_word: true,
                        owned_strings: Vec::new(),
                        owned_vecs: Vec::new(),
                        value: entry.value,
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Renders a `GosMap` whose values carry `val_tag`, the tuple tag encoding:
/// a container tag reads the stored word as a handle rather than as the
/// integer it would otherwise print as. Tag `0` is the scalar rendering
/// [`gos_rt_map_format`] performs.
///
/// # Safety
/// `m` is a live `GosMap` whose value words match `val_tag`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_format_tagged(
    m: *const GosMap,
    key_tag: i64,
    val_tag: i64,
    aux: *const u8,
    aux_n: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return alloc_cstring(b"{}");
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let entries = match &*storage {
            MapStorage::Empty => 0,
            MapStorage::I64I64(inner) => inner.len(),
            MapStorage::I64Bytes(inner) => inner.len(),
            MapStorage::StrI64(inner) => inner.len(),
            MapStorage::StrStr(inner) => inner.len(),
            MapStorage::StrBytes(inner) => inner.len(),
            MapStorage::I64Str(inner) => inner.len(),
            MapStorage::Bytes(inner) => inner.len(),
            MapStorage::SkeyVal { entries, .. } => entries.len(),
            MapStorage::EkeyVal { entries } => entries.len(),
        };
        crate::c_abi::ledger::map_format(entries);
        let mut out = String::from("{");
        let push_entry = |out: &mut String, first: &mut bool, k: &str, v: &str| {
            if *first {
                *first = false;
            } else {
                out.push_str(", ");
            }
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
        };
        let quote_key = |key: &[u8]| format!("{:?}", String::from_utf8_lossy(key));
        // An integer key follows the width its declaration named: `1` is the
        // unsigned tag, so a key at or above `i64::MAX` reads as its own
        // decimal rather than the negative the same bits spell.
        let format_key = |k: i64| {
            // The key's own tag decides how its word reads: an unsigned
            // decimal, a float's value rather than the bits' integer, a
            // `bool`, or a `char`.
            let mut out = String::new();
            unsafe { render_tagged_word(&mut out, k, key_tag as u8) };
            out
        };
        let mut first = true;
        match &*storage {
            MapStorage::I64I64(inner) => {
                let mut entries: Vec<(i64, i64)> = inner.iter().map(|(k, v)| (*k, *v)).collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (k, v) in entries {
                    let mut value = String::new();
                    unsafe { render_map_value(&mut value, v, val_tag, aux, aux_n) };
                    push_entry(&mut out, &mut first, &format_key(k), &value);
                }
            }
            MapStorage::StrI64(inner) => {
                let mut entries: Vec<(&[u8], i64)> =
                    inner.iter().map(|(k, v)| (k.as_slice(), *v)).collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (k, v) in entries {
                    let key = quote_key(k);
                    let mut value = String::new();
                    unsafe { render_map_value(&mut value, v, val_tag, aux, aux_n) };
                    push_entry(&mut out, &mut first, &key, &value);
                }
            }
            MapStorage::StrStr(inner) => {
                let mut entries: Vec<(&[u8], &[u8])> = inner
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_ref()))
                    .collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (k, v) in entries {
                    let key = quote_key(k);
                    push_entry(&mut out, &mut first, &key, &String::from_utf8_lossy(v));
                }
            }
            MapStorage::I64Str(inner) => {
                let mut entries: Vec<(i64, &[u8])> =
                    inner.iter().map(|(k, v)| (*k, v.as_ref())).collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (k, v) in entries {
                    push_entry(
                        &mut out,
                        &mut first,
                        &format_key(k),
                        &String::from_utf8_lossy(v),
                    );
                }
            }
            MapStorage::I64Bytes(inner) => {
                let mut entries = inner.entries_vec();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (k, v) in entries {
                    let value = format!(
                        "[{}]",
                        v.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")
                    );
                    push_entry(&mut out, &mut first, &format_key(k), &value);
                }
            }
            MapStorage::StrBytes(inner) => {
                let mut entries: Vec<(&[u8], &[u8])> = inner.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (k, v) in entries {
                    let key = quote_key(k);
                    let value = format!(
                        "[{}]",
                        v.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")
                    );
                    push_entry(&mut out, &mut first, &key, &value);
                }
            }
            MapStorage::Empty
            | MapStorage::Bytes(_)
            | MapStorage::SkeyVal { .. }
            | MapStorage::EkeyVal { .. } => {}
        }
        out.push('}');
        alloc_cstring(out.as_bytes())
    })
}

/// Drops a `HashMap` allocated by [`gos_rt_map_new`] /
/// [`gos_rt_map_new_with_capacity`]. The MIR's drop-insertion pass
/// emits a call to this at every function return for any local
/// that owns a freshly-constructed map and isn't moved into the
/// return slot. Idempotent on null.
///
/// SAFETY: only call this on a pointer returned by one of the
/// runtime's `gos_rt_map_new*` constructors - the runtime's
/// [`GosMap`] layout includes a `parking_lot::Mutex<...>` and
/// dropping a binding-side `BindingGosMap` (two parallel `GosVec`
/// pointers) here would `Box::from_raw` the wrong shape and run
/// `Mutex::drop` over garbage. Use [`gos_rt_binding_map_free`] for
/// the binding-shaped aggregate instead.
/// Marks `m` as holding RC copy-blob values. Emitted by the MIR lowering
/// immediately after construction. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_set_blob_values(m: *mut GosMap) {
    if m.is_null() {
        return;
    }
    unsafe { &*m }
        .value_owner
        .store(MAP_VALUE_RC, Ordering::Release);
}

/// Marks `m` as holding `Vec`/slice values. The map owns one Vec reference
/// per entry; this is distinct from a copy blob because Vecs have their own
/// refcount and destructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_set_vec_values(m: *mut GosMap) {
    if m.is_null() {
        return;
    }
    unsafe { &*m }
        .value_owner
        .store(MAP_VALUE_VEC, Ordering::Release);
}

fn map_value_owner(m: &GosMap) -> u8 {
    m.value_owner.load(Ordering::Acquire)
}

fn map_has_owned_values(m: &GosMap) -> bool {
    map_value_owner(m) != MAP_VALUE_NONE
}

/// Release one stored blob value word (set-gated inside the RC layer
/// via the copy blob's explicit owner carrier).
unsafe fn release_blob_value(word: i64) {
    if word != 0 {
        unsafe { crate::c_abi::rc::gos_rt_rc_release(word as usize as *mut u8) };
    }
}

/// Retain one stored blob value word before handing it out.
unsafe fn retain_blob_value(word: i64) {
    if word != 0 {
        unsafe { crate::c_abi::rc::gos_rt_rc_retain(word as usize as *mut u8) };
    }
}

unsafe fn release_owned_value(m: &GosMap, word: i64) {
    unsafe { release_owned_value_tag(map_value_owner(m), word) };
}

/// Same as [`release_owned_value`], keyed directly by a `value_owner` tag -
/// for storage that no longer sits under the `GosMap` whose tag it was
/// built with.
unsafe fn release_owned_value_tag(owner: u8, word: i64) {
    if word == 0 {
        return;
    }
    match owner {
        MAP_VALUE_RC => unsafe { release_blob_value(word) },
        MAP_VALUE_VEC => unsafe { gos_rt_vec_free(word as usize as *mut GosVec) },
        _ => {}
    }
}

/// Releases every share `storage` holds: each value under `owner`, and the
/// key node of every enum-keyed entry, which the map owns whether or not
/// its values are owned.
unsafe fn release_storage_entries(owner: u8, storage: &MapStorage) {
    let release = |word: i64| unsafe { release_owned_value_tag(owner, word) };
    match storage {
        MapStorage::I64I64(inner) => inner.values().for_each(|&v| release(v)),
        MapStorage::StrI64(inner) => inner.values().for_each(|&v| release(v)),
        MapStorage::SkeyVal { entries, .. } => entries.values().for_each(|&v| release(v)),
        MapStorage::EkeyVal { entries } => {
            for entry in entries.values() {
                release(entry.value);
                unsafe { crate::c_abi::rc::gos_rt_rc_release(entry.key_node) };
            }
        }
        _ => {}
    }
}

unsafe fn retain_owned_value(m: &GosMap, word: i64) {
    unsafe { retain_owned_value_tag(map_value_owner(m), word) };
}

/// Same as [`retain_owned_value`], keyed directly by a `value_owner` tag
/// instead of a live `GosMap` - used while building a cloned map's storage,
/// before a `GosMap` wrapping it exists to read the tag from.
unsafe fn retain_owned_value_tag(owner: u8, word: i64) {
    if word == 0 {
        return;
    }
    match owner {
        MAP_VALUE_RC => unsafe { retain_blob_value(word) },
        MAP_VALUE_VEC => unsafe { crate::c_abi::gos_rt_vec_retain(word as usize as *mut GosVec) },
        _ => {}
    }
}

/// Deep-clones `storage`'s entries into a fresh `MapStorage` of the same
/// shape, retaining any RC-managed value or key node the clone now shares
/// with the source (a byte-blob deep copy for everything else). Mirrors
/// [`gos_rt_map_mark_shared`]'s per-variant walk, but builds a new table
/// instead of flipping the source's atomics.
fn clone_map_storage(storage: &MapStorage, value_owner: u8) -> MapStorage {
    match storage {
        MapStorage::Empty => MapStorage::Empty,
        MapStorage::I64I64(m) => {
            let cloned = m.clone();
            if value_owner != MAP_VALUE_NONE {
                for &v in cloned.values() {
                    unsafe { retain_owned_value_tag(value_owner, v) };
                }
            }
            MapStorage::I64I64(cloned)
        }
        MapStorage::StrI64(m) => {
            let cloned = m.clone();
            if value_owner != MAP_VALUE_NONE {
                for &v in cloned.values() {
                    unsafe { retain_owned_value_tag(value_owner, v) };
                }
            }
            MapStorage::StrI64(cloned)
        }
        MapStorage::SkeyVal { entries, desc } => {
            let cloned = entries.clone();
            if value_owner != MAP_VALUE_NONE {
                for &v in cloned.values() {
                    unsafe { retain_owned_value_tag(value_owner, v) };
                }
            }
            MapStorage::SkeyVal {
                entries: cloned,
                desc: desc.clone(),
            }
        }
        MapStorage::StrStr(m) => MapStorage::StrStr(m.clone()),
        MapStorage::StrBytes(s) => MapStorage::StrBytes(s.clone()),
        MapStorage::I64Bytes(s) => MapStorage::I64Bytes(s.clone()),
        MapStorage::I64Str(m) => MapStorage::I64Str(m.clone()),
        MapStorage::Bytes(m) => MapStorage::Bytes(m.clone()),
        MapStorage::EkeyVal { entries } => {
            let cloned: FxHashMap<ByteKey, EnumEntry> = entries
                .iter()
                .map(|(k, e)| {
                    if !e.key_node.is_null() {
                        unsafe { crate::c_abi::rc::gos_rt_rc_retain(e.key_node) };
                    }
                    (
                        k.clone(),
                        EnumEntry {
                            value: e.value,
                            key_node: e.key_node,
                        },
                    )
                })
                .collect();
            MapStorage::EkeyVal { entries: cloned }
        }
    }
}

/// `xs.clone()` for a `Map` / `Set` receiver, and the primitive a `let`
/// binding or by-value call argument uses to give the binding an
/// independent table instead of aliasing the source. Allocates a fresh
/// `GosMap` with a deep copy of every entry, retaining any RC-managed
/// value or key node the copy now shares with the source.
///
/// `GosMap` carries no refcount of its own (unlike `GosVec` / strings,
/// which have an atomic strong count in their header) - every `HashMap` /
/// `Set` binding owns its table uniquely, so a plain pointer copy at a
/// `let` binding either double-frees the table once both bindings' drop
/// points run, or - if the drop pass elides one as a mere alias - leaves
/// both bindings mutating the same live table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_clone(src: *const GosMap) -> *mut GosMap {
    ffi_entry!(std::ptr::null_mut(), {
        if src.is_null() {
            return unsafe { gos_rt_map_new(8, 8) };
        }
        let source = unsafe { &*src };
        let owner = map_value_owner(source);
        let guard = source.storage.lock();
        let cloned_storage = clone_map_storage(&guard, owner);
        drop(guard);
        crate::c_abi::ledger::map_inc();
        Box::into_raw(Box::new(GosMap {
            len_cache: source.len_cache,
            storage: BiasedLock::new(cloned_storage),
            value_owner: AtomicU8::new(owner),
        }))
    })
}

/// `*dst = src` through a `&mut Map`: the table every holder of the
/// reference names keeps its identity and takes a copy of `src`'s entries,
/// releasing the ones it held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_assign(dst: *mut GosMap, src: *const GosMap) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() || std::ptr::addr_eq(dst.cast_const(), src) {
            return;
        }
        let source = unsafe { &*src };
        let owner = map_value_owner(source);
        let cloned = {
            let guard = source.storage.lock();
            clone_map_storage(&guard, owner)
        };
        let target = unsafe { &mut *dst };
        let old_owner = map_value_owner(target);
        let old = {
            let mut guard = target.storage.lock();
            std::mem::replace(&mut *guard, cloned)
        };
        target.value_owner.store(owner, Ordering::Release);
        target.len_cache = source.len_cache;
        unsafe { release_storage_entries(old_owner, &old) };
    });
}

/// Marks `m` shared across goroutines so every subsequent operation
/// takes the real lock instead of the goroutine-local fast path.
/// Codegen emits this at goroutine-spawn / channel-send escape points
/// for `HashMap`-typed values, on the owning goroutine *before* the map
/// is published - the same ordering `gos_rt_rc_mark_shared` relies on,
/// so the flip races with no concurrent reader. Aggregate (RC copy-blob)
/// values are marked shared too, since they become reachable from the
/// other goroutine through the now-shared map. Idempotent; null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_mark_shared(m: *mut GosMap) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        let map = unsafe { &*m };
        if map_has_owned_values(map) {
            let storage = map.storage.lock();
            match &*storage {
                MapStorage::I64I64(inner) => {
                    for &v in inner.values() {
                        if map_value_owner(map) == MAP_VALUE_VEC {
                            unsafe {
                                crate::c_abi::vec::gos_rt_vec_mark_shared(
                                    v as usize as *mut GosVec,
                                );
                            };
                        } else {
                            unsafe {
                                crate::c_abi::rc::gos_rt_rc_mark_shared(v as usize as *mut u8);
                            };
                        }
                    }
                }
                MapStorage::SkeyVal { entries, .. } => {
                    for &v in entries.values() {
                        if map_value_owner(map) == MAP_VALUE_VEC {
                            unsafe {
                                crate::c_abi::vec::gos_rt_vec_mark_shared(
                                    v as usize as *mut GosVec,
                                );
                            };
                        } else {
                            unsafe {
                                crate::c_abi::rc::gos_rt_rc_mark_shared(v as usize as *mut u8);
                            };
                        }
                    }
                }
                MapStorage::StrI64(inner) => {
                    for &v in inner.values() {
                        if map_value_owner(map) == MAP_VALUE_VEC {
                            unsafe {
                                crate::c_abi::vec::gos_rt_vec_mark_shared(
                                    v as usize as *mut GosVec,
                                );
                            };
                        } else {
                            unsafe {
                                crate::c_abi::rc::gos_rt_rc_mark_shared(v as usize as *mut u8);
                            };
                        }
                    }
                }
                _ => {}
            }
        }
        map.storage.mark_shared();
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_free(m: *mut GosMap) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        crate::c_abi::ledger::map_dec();
        let boxed = unsafe { Box::from_raw(m) };
        {
            let storage = boxed.storage.lock();
            unsafe { release_storage_entries(map_value_owner(&boxed), &storage) };
        }
        drop(boxed);
    });
}

/// Frees the map a by-value aggregate field owns and nulls the slot.
///
/// `slot` is the field's storage address. Nulling makes the release
/// idempotent, so the drop pass may book it at more than one exit edge of the
/// same field without the second booking touching a freed map. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_field_release(slot: *mut *mut GosMap) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let m = unsafe { slot.read_unaligned() };
        if m.is_null() {
            return;
        }
        unsafe { slot.write_unaligned(std::ptr::null_mut()) };
        unsafe { gos_rt_map_free(m) };
    });
}

/// Replaces the map a by-value aggregate field holds with its own clone.
///
/// `slot` is the field's storage address. Emitted after a copy that would
/// otherwise leave two aggregates naming one map: a `GosMap` has no reference
/// count, so a copied field takes a value of its own the way a map binding
/// does. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_field_clone(slot: *mut *mut GosMap) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let m = unsafe { slot.read_unaligned() };
        if m.is_null() {
            return;
        }
        let cloned = unsafe { crate::c_abi::gos_rt_map_clone(m) };
        unsafe { slot.write_unaligned(cloned) };
    });
}

/// Wire shape of `gossamer_binding::native::BindingGosMap`. Defined
/// here (as a private type) so the dedicated free helper can box it
/// back without the runtime depending on the binding crate. The two
/// fields are pointers to `GosVec`-headed parallel arrays; the
/// binding crate's `make_gos_map` constructs both with
/// `Box::into_raw(Box::new(...))` so the matching free path walks
/// the same `Box`-shaped allocation.
#[repr(C)]
struct BindingGosMapLayout {
    keys: *mut GosVec,
    values: *mut GosVec,
}

/// Drops a binding-side map (a `BindingGosMap` from
/// `gossamer-binding`'s `native.rs`). Walks the two inner `GosVec`
/// pointers, freeing each via [`gos_rt_vec_free`], then drops the
/// outer `Box<BindingGosMapLayout>` allocation. Idempotent on null.
///
/// This is intentionally a separate symbol from [`gos_rt_map_free`]
/// because the two structs share a name across crates but have
/// incompatible layouts (one wraps a `parking_lot::Mutex<...>`, the
/// other is two raw pointers). Sending a binding-side pointer
/// through `gos_rt_map_free` would drop a `Mutex` over uninitialised
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_map_free(m: *mut u8) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(m.cast::<BindingGosMapLayout>()) };
        unsafe {
            gos_rt_vec_free(boxed.keys);
            gos_rt_vec_free(boxed.values);
        }
        drop(boxed);
    });
}

/// Drops a `Vec` allocated by [`gos_rt_vec_new`] /
/// [`gos_rt_vec_with_capacity`] / [`gos_rt_vec_new_typed`]. Frees
/// the `GosVec` header, the backing element buffer, and - when
/// `elem_kind != PRIMITIVE` - every pointer-bearing element
/// payload (cstring, nested Vec, Map, Error). Null is a no-op.
///
/// # Ownership contract
///
/// `v` must be a live owning reference returned by this runtime. In
/// particular, callers must not pass a borrowed/region Vec, invoke this twice
/// for the same owning reference, or retain and use the pointer after this
/// function consumes its final reference. A raw pointer has no generation in
/// the stable ABI, so an address-only global live set cannot make stale-pointer
/// release sound: allocator reuse would let an old pointer release a new Vec.
/// The compiler's ownership lowering supplies this invariant; foreign callers
/// must model the same retain/release discipline.
///
/// The default `elem_kind = PRIMITIVE` path matches pre-0.6
/// behaviour: shallow free of the byte buffer. Typed vecs created
/// via `gos_rt_vec_new_typed` opt in to deep free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_free(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        // Region-allocated vecs (header + buffer in arena slabs) are freed
        // wholesale at `arena_pop` - never individually. Touching them here
        // via `Box::from_raw` / `Vec::from_raw_parts` would corrupt the
        // global allocator (the memory isn't its).
        if crate::c_abi::vec::vec_is_region(unsafe { &*v }) {
            return;
        }
        // RC: atomic decrement. Reclaim only when this thread held the last
        // reference (old count was 1). An aliased Vec still has live holders.
        // `Release` on the decrement publishes this thread's prior writes to
        // the element payloads; the `Acquire` fence on the final drop then
        // observes every other holder's writes before teardown - the standard
        // Arc drop discipline, without which a weakly-ordered target (aarch64,
        // a shipped tier) could reclaim a buffer while a peer's store is still
        // in flight.
        let old_rc = crate::c_abi::vec::vec_rc_atomic(unsafe { &*v })
            .fetch_sub(1, std::sync::atomic::Ordering::Release);
        if crate::c_abi::vec::rc_trace_enabled() {
            eprintln!("VFREE   v={v:p} old_rc={old_rc}");
        }
        // A count that was already zero means this reference was handed back
        // twice. Reclaiming again would return the block to the allocator a
        // second time, so the release stops here; `GOS_RC_DEBUG` names the
        // site so the ownership lowering that produced it can be found.
        if old_rc == 0 {
            if std::env::var_os("GOS_RC_DEBUG").is_some() {
                eprintln!(
                    "gos_rt_vec_free: released a Vec whose count was already zero ({v:p})\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
            return;
        }
        if old_rc > 1 {
            return;
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        crate::c_abi::ledger::vec_dec();
        // Non-region headers are a single `Box<InlineVec>` (header + inline
        // element buffer). The header's `ptr` for an inline vec aliases this
        // same allocation's buffer, so the deep-free walk below reads through
        // a pointer into the block. Drive that walk through the raw pointer
        // and reconstruct the owning `Box` only afterwards (its drop reclaims
        // the header block, including any inline buffer); a separately
        // allocated (split) buffer is reclaimed explicitly via
        // `free_vec_buffer`.
        let compact_header = crate::c_abi::vec::vec_has_compact_header(unsafe { &*v });
        let inline_ptr = v.cast::<crate::c_abi::vec::InlineVec>();
        let boxed = unsafe { &*v };
        if boxed.elem_kind == vec_elem_kind::PACKED_ROWS {
            unsafe { crate::c_abi::vec::free_packed_rows(boxed) };
            if compact_header {
                drop(unsafe { Box::from_raw(v) });
            } else {
                drop(unsafe { Box::from_raw(inline_ptr) });
            }
            return;
        }
        if !boxed.ptr.is_null() && boxed.cap > 0 {
            // Deep-free pointer-bearing element payloads BEFORE
            // reclaiming the backing buffer. Each branch walks the
            // first `len` slots - slots between `len` and `cap` were
            // never written and contain the zero-init produced by
            // `vec![0u8; bytes]` at construction time.
            // Guarded aggregate elements: release each element's
            // copy-blob children (set-gated in the walk) before the
            // buffer goes away.
            if boxed.elem_kind == vec_elem_kind::AGGR_GUARDED {
                unsafe { crate::c_abi::vec::vec_release_guarded_elements(boxed) };
            }
            // Owned-slot-children elements (materializer shims): free
            // each live embedded string / nested vec, including slots a
            // consumer loop never reached (the early-`break` path).
            if boxed.elem_kind == vec_elem_kind::AGGR_OWNED {
                unsafe { crate::c_abi::vec::vec_release_owned_children(boxed) };
            }
            if boxed.elem_kind != vec_elem_kind::PRIMITIVE && boxed.elem_bytes as usize == 8 {
                let count = boxed.len.max(0) as usize;
                // SAFETY: ptr is non-null + cap > 0 (checked above);
                // we only read `count <= len <= cap` slots of 8 bytes
                // each, all initialised by construction.
                let base = boxed.ptr;
                for i in 0..count {
                    // Slots hold child pointers exposed as integers by the
                    // flat-slot ABI in a byte buffer with no 8-byte
                    // alignment guarantee; read unaligned and recover
                    // provenance before the dereferencing free.
                    let raw = unsafe { base.add(i * 8).cast::<usize>().read_unaligned() };
                    if raw == 0 {
                        continue;
                    }
                    let slot: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
                    match boxed.elem_kind {
                        vec_elem_kind::STRING => {
                            // SAFETY: each slot in a STRING-typed vec was
                            // populated via gos_rt_str_clone / alloc_cstring
                            // and therefore carries the allocator tag.
                            unsafe { gos_rt_str_free(slot.cast::<c_char>()) };
                        }
                        vec_elem_kind::VEC => {
                            unsafe { gos_rt_vec_free(slot.cast::<GosVec>()) };
                        }
                        vec_elem_kind::MAP => {
                            unsafe { gos_rt_map_free(slot.cast::<GosMap>()) };
                        }
                        vec_elem_kind::ERROR => {
                            // No dedicated free helper yet; drop the
                            // raw Box (allocated via `Box::into_raw`
                            // elsewhere in the file). Safe because
                            // `GosError`'s own drop chains through the
                            // message + cause heap allocations.
                            let _ = unsafe { Box::from_raw(slot.cast::<GosError>()) };
                        }
                        vec_elem_kind::RC_ENUM => {
                            // The vec owns each enum-node element (the push
                            // moved the frame's share in); release cascades
                            // through the node's own child meta.
                            unsafe { crate::c_abi::rc::gos_rt_rc_release(slot) };
                        }
                        vec_elem_kind::JSON => {
                            // Each element is a handle holding a share of the
                            // document's tree; the tree dies with its last one.
                            unsafe { crate::c_abi::json::gos_rt_json_free(slot.cast()) };
                        }
                        _ => {}
                    }
                }
            }
            // Reclaim the element buffer only when it is a standalone
            // (split) allocation; an inline buffer is part of the header
            // block and goes away with `drop(inline_box)` below.
            if crate::c_abi::vec::vec_is_split(boxed) {
                let bytes = (boxed.cap as usize) * (boxed.elem_bytes as usize);
                // SAFETY: a split vec's buffer came from `alloc_vec_buffer(bytes)`.
                unsafe {
                    crate::c_abi::vec::free_vec_buffer(boxed.ptr.as_ptr(), bytes);
                }
            }
        }
        // Drop any lazily allocated aggregate-owned slot metadata after the
        // deep-free walk and before the header block. Metadata is owned by
        // the header itself, never an address-keyed side table. Pass the
        // `Box`'s own borrow, not the raw `v`, so the read of `elem_kind`
        // stays under the Box's exclusive ownership.
        unsafe { crate::c_abi::vec::drop_vec_owner(&mut *v) };
        // Reconstruct the owning box now that the self-referential walk is
        // done, so its drop reclaims the header block (and any inline buffer).
        if compact_header {
            drop(unsafe { Box::from_raw(v) });
        } else {
            drop(unsafe { Box::from_raw(inline_ptr) });
        }
    });
}

/// Drops a `HashSet` allocated by [`gos_rt_set_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_free(s: *mut GosSet) {
    ffi_entry!((), {
        if s.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(s) });
    });
}

/// Snapshots the i64 keys of an i64-keyed `HashMap` into a fresh
/// `GosVec<i64>` for the for-loop lowerer to drive with the
/// regular `gos_rt_vec_*` helpers. Iteration order matches the
/// underlying `FxHashMap`'s order - undefined-but-stable per
/// process. Returns an empty vec for any other storage shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_keys_i64(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if m.is_null() {
            return out;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let push_key = |k: &i64| {
            let bytes = k.to_ne_bytes();
            unsafe { gos_rt_vec_push(out, bytes.as_ptr()) };
        };
        // Sort by key for deterministic order that matches `values()`,
        // `iter()`, and the VM (a `FxHashMap`'s bucket order is neither
        // stable nor the same across tiers).
        let mut keys: Vec<i64> = match &*storage {
            MapStorage::I64I64(inner) => inner.keys().copied().collect(),
            MapStorage::I64Str(inner) => inner.keys().copied().collect(),
            _ => Vec::new(),
        };
        keys.sort_unstable();
        keys.iter().for_each(push_key);
        out
    })
}

/// Snapshots the i64 values of an i64-valued `HashMap` into a
/// fresh `GosVec<i64>`. Pairs with `gos_rt_map_keys_i64` for
/// `for v in m.values()` lowering. Empty vec for non-i64-valued
/// storage shapes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_values_i64(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if m.is_null() {
            return out;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        // Emit values in key-sorted order so `keys()` / `values()` /
        // `iter()` agree and the order is deterministic across tiers.
        let push_val = |v: i64| {
            let bytes = v.to_ne_bytes();
            unsafe { gos_rt_vec_push(out, bytes.as_ptr()) };
        };
        match &*storage {
            MapStorage::I64I64(inner) => {
                let mut entries: Vec<(i64, i64)> = inner.iter().map(|(k, v)| (*k, *v)).collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (_, v) in entries {
                    push_val(v);
                }
            }
            MapStorage::StrI64(inner) => {
                let mut rows: Vec<(&[u8], i64)> =
                    inner.iter().map(|(k, v)| (k.as_slice(), *v)).collect();
                rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (_, v) in rows {
                    push_val(v);
                }
            }
            MapStorage::SkeyVal { entries, desc } => {
                let mut rows: Vec<(&[u8], i64)> =
                    entries.iter().map(|(k, v)| (k.as_slice(), *v)).collect();
                rows.sort_by_cached_key(|(key, _)| skey_order(key, desc));
                for (_, v) in rows {
                    push_val(v);
                }
            }
            _ => {}
        }
        out
    })
}

/// Snapshots the string keys of a string-keyed `HashMap` into a
/// fresh `GosVec<*mut c_char>`. Each key is freshly allocated in
/// the GC arena so the slot value is the same `*mut c_char`
/// representation Gossamer's `String` type uses elsewhere.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_keys_str(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // STRING-typed: the snapshot owns its key strings, so
        // `gos_rt_vec_free` reclaims them even on early `break`.
        let out = unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8, vec_elem_kind::STRING) };
        if m.is_null() {
            return out;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let push_key = |k: &[u8]| {
            let cstr = alloc_cstring(k);
            let slot = (cstr as usize as i64).to_ne_bytes();
            unsafe { gos_rt_vec_push(out, slot.as_ptr()) };
        };
        // Sort by key (lexicographic byte order, matching the VM's
        // `SmolStr` ordering) for deterministic, cross-tier order.
        let mut keys: Vec<&[u8]> = match &*storage {
            MapStorage::StrI64(inner) => inner.keys().map(ByteKey::as_slice).collect(),
            MapStorage::StrStr(inner) => inner.keys().map(ByteKey::as_slice).collect(),
            _ => Vec::new(),
        };
        keys.sort_unstable();
        for k in keys {
            push_key(k);
        }
        out
    })
}

/// Snapshots the string values of a string-valued `HashMap` into
/// a fresh `GosVec<*mut c_char>`. Mirrors `gos_rt_map_keys_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_values_str(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // STRING-typed - same ownership contract as `gos_rt_map_keys_str`.
        let out = unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8, vec_elem_kind::STRING) };
        if m.is_null() {
            return out;
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let push_val = |v: &[u8]| {
            let cstr = alloc_cstring(v);
            let slot = (cstr as usize as i64).to_ne_bytes();
            unsafe { gos_rt_vec_push(out, slot.as_ptr()) };
        };
        // Values in key-sorted order so `keys()` / `values()` / `iter()`
        // agree and the order is deterministic across tiers.
        match &*storage {
            MapStorage::StrStr(inner) => {
                let mut entries: Vec<(&[u8], &[u8])> =
                    inner.iter().map(|(k, v)| (k.as_slice(), &**v)).collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (_, v) in entries {
                    push_val(v);
                }
            }
            MapStorage::I64Str(inner) => {
                let mut entries: Vec<(i64, &[u8])> =
                    inner.iter().map(|(k, v)| (*k, &**v)).collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (_, v) in entries {
                    push_val(v);
                }
            }
            _ => {}
        }
        out
    })
}

fn empty_cstring() -> *mut c_char {
    alloc_cstring(b"")
}

/// Snapshots the aggregate keys of a struct- or tuple-keyed `HashMap` into a
/// fresh `GosVec` of flat element slots, in key-byte order so `keys()`,
/// `values()`, and `iter()` agree across tiers.
///
/// Each stored key is the encoding `build_skey_for_set` produced under the
/// map's own slot descriptor, so decoding it slot by slot rebuilds exactly the
/// aggregate the program inserted: a scalar slot is copied back verbatim and a
/// string slot is reallocated as a c-string the snapshot owns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_keys_skey(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let MapStorage::SkeyVal { entries, desc } = &*storage else {
            return unsafe { gos_rt_vec_new(8) };
        };
        let slots = desc.len();
        // A field-less key occupies the one slot every inline aggregate is
        // sized at, and decodes no content into it.
        let elem_bytes = (slots.max(1) * 8) as u32;
        let mut keys: Vec<&[u8]> = entries.keys().map(ByteKey::as_slice).collect();
        keys.sort_by_cached_key(|key| skey_order(key, desc));
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                elem_bytes,
                keys.len() as i64,
                vec_elem_kind::PRIMITIVE,
            )
        };
        let mut slot_buf = vec![0i64; slots.max(1)];
        for key in keys {
            if !decode_skey_into(key, desc, &mut slot_buf) {
                continue;
            }
            unsafe { gos_rt_vec_push(out, slot_buf.as_ptr().cast::<u8>()) };
        }
        // String slots hold freshly allocated c-strings the snapshot owns, so
        // record where they sit for `gos_rt_vec_free` to release them.
        let string_slots: Vec<i64> = desc
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == b'S')
            .flat_map(|(i, _)| [-1, 0, i as i64, i64::from(vec_elem_kind::STRING)])
            .collect();
        if !string_slots.is_empty() {
            let mut meta = Vec::with_capacity(string_slots.len() + 1);
            meta.push((string_slots.len() / 4) as i64);
            meta.extend_from_slice(&string_slots);
            unsafe { crate::c_abi::vec::gos_rt_vec_set_slot_children(out, meta.as_ptr()) };
        }
        out
    })
}

/// One decoded field of an aggregate key, ordered the way the VM orders the
/// matching `MapKey` field: numerically for a scalar word (integers, `char`
/// code points, `bool`, and float bit patterns alike) and byte-lexicographic
/// for a string.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SkeyField<'a> {
    Word(i64),
    Text(&'a [u8]),
    /// A sequence's elements, widened to whole words so they order by value
    /// the way the interpreter orders the same elements.
    Seq(Vec<i64>),
}

/// Field-wise ordering key for one stored aggregate key.
///
/// Sorting by the raw encoding would order scalar fields by their
/// little-endian bytes; the snapshot orders by field value so a struct-keyed
/// `keys()`, `values()`, and `iter()` all follow the same sequence the VM
/// yields.
pub(crate) fn skey_order_key(key: &[u8], desc: &[u8]) -> Vec<u8> {
    // The comparable form is the field sequence rendered back into bytes that
    // compare in the same order: a word as its big-endian two's-complement
    // encoding with the sign bit flipped, text and sequences by content.
    let mut out = Vec::with_capacity(key.len());
    for field in skey_order(key, desc) {
        match field {
            SkeyField::Word(word) => {
                out.push(0);
                out.extend_from_slice(&(word as u64 ^ (1u64 << 63)).to_be_bytes());
            }
            SkeyField::Text(text) => {
                out.push(1);
                out.extend_from_slice(text);
                out.push(0);
            }
            SkeyField::Seq(words) => {
                out.push(2);
                for word in words {
                    out.extend_from_slice(&(word as u64 ^ (1u64 << 63)).to_be_bytes());
                }
                out.push(0);
            }
        }
    }
    out
}

fn skey_order<'a>(key: &'a [u8], desc: &[u8]) -> Vec<SkeyField<'a>> {
    let mut fields = Vec::with_capacity(desc.len());
    let mut cursor = 0usize;
    for &code in desc {
        match code {
            b's' => {
                let Some(word) = key.get(cursor..cursor + 8) else {
                    break;
                };
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(word);
                fields.push(SkeyField::Word(i64::from_ne_bytes(bytes)));
                cursor += 8;
            }
            b'S' => {
                let Some(len_bytes) = key.get(cursor..cursor + 8) else {
                    break;
                };
                let mut raw = [0u8; 8];
                raw.copy_from_slice(len_bytes);
                let len = u64::from_le_bytes(raw) as usize;
                cursor += 8;
                let Some(text) = key.get(cursor..cursor + len) else {
                    break;
                };
                cursor += len;
                fields.push(SkeyField::Text(text));
            }
            b'V' => {
                let Some(header) = key.get(cursor..cursor + 16) else {
                    break;
                };
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&header[..8]);
                let len = u64::from_le_bytes(raw) as usize;
                raw.copy_from_slice(&header[8..]);
                let stride = (u64::from_le_bytes(raw) as usize).max(1);
                cursor += 16;
                let Some(bytes) = key.get(cursor..cursor + len * stride) else {
                    break;
                };
                cursor += len * stride;
                fields.push(SkeyField::Seq(seq_words(bytes, stride)));
            }
            _ => break,
        }
    }
    fields
}

/// Reads a sequence's raw element bytes back as whole words, so two keys
/// order element by element rather than by their little-endian encoding.
fn seq_words(bytes: &[u8], stride: usize) -> Vec<i64> {
    bytes
        .chunks_exact(stride.clamp(1, 8))
        .map(|chunk| {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            i64::from_le_bytes(word)
        })
        .collect()
}

/// Decodes one stored aggregate key back into `slots`, one word per descriptor
/// entry. Returns false for a key whose bytes do not match the descriptor,
/// which cannot happen for a key this map encoded.
fn decode_skey_into(key: &[u8], desc: &[u8], slots: &mut [i64]) -> bool {
    let mut cursor = 0usize;
    for (index, &code) in desc.iter().enumerate() {
        match code {
            b's' => {
                let Some(word) = key.get(cursor..cursor + 8) else {
                    return false;
                };
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(word);
                slots[index] = i64::from_ne_bytes(bytes);
                cursor += 8;
            }
            b'S' => {
                let Some(len_bytes) = key.get(cursor..cursor + 8) else {
                    return false;
                };
                let mut raw = [0u8; 8];
                raw.copy_from_slice(len_bytes);
                let len = u64::from_le_bytes(raw) as usize;
                cursor += 8;
                // A zero length covers both an empty string and an absent
                // one; both rebuild as an owned empty c-string, which is the
                // representation a `String` slot always holds.
                let Some(text) = key.get(cursor..cursor + len) else {
                    return false;
                };
                cursor += len;
                slots[index] = alloc_cstring(text) as usize as i64;
            }
            // A sequence key rebuilds as a fresh vec over the bytes the key
            // folded, which the renderer reads and the entry then releases.
            b'V' => {
                let Some(header) = key.get(cursor..cursor + 16) else {
                    return false;
                };
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&header[..8]);
                let len = u64::from_le_bytes(raw) as usize;
                raw.copy_from_slice(&header[8..]);
                let stride = (u64::from_le_bytes(raw) as usize).max(1);
                cursor += 16;
                let Some(bytes) = key.get(cursor..cursor + len * stride) else {
                    return false;
                };
                cursor += len * stride;
                let vec = unsafe {
                    crate::c_abi::vec::gos_rt_vec_new_typed(
                        stride as u32,
                        crate::c_abi::vec::vec_elem_kind::PRIMITIVE,
                    )
                };
                for chunk in bytes.chunks_exact(stride) {
                    unsafe { crate::c_abi::vec::gos_rt_vec_push(vec, chunk.as_ptr()) };
                }
                slots[index] = vec as usize as i64;
            }
            _ => return false,
        }
    }
    true
}

/// Auto-dispatch `m.keys() -> Vec<K>` based on the live map storage.
/// I64-keyed maps return a `Vec<i64>`; string-keyed maps return a
/// `Vec<*mut c_char>`. Empty Vec for empty / unknown storage shapes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_keys_vec(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::I64I64(_) | MapStorage::I64Bytes(_) | MapStorage::I64Str(_) => {
                drop(storage);
                unsafe { gos_rt_map_keys_i64(m) }
            }
            MapStorage::StrI64(_)
            | MapStorage::StrStr(_)
            | MapStorage::StrBytes(_)
            | MapStorage::Bytes(_) => {
                drop(storage);
                unsafe { gos_rt_map_keys_str(m) }
            }
            MapStorage::SkeyVal { .. } => {
                drop(storage);
                unsafe { gos_rt_map_keys_skey(m) }
            }
            MapStorage::EkeyVal { .. } => {
                drop(storage);
                unsafe { gos_rt_map_keys_ekey(m) }
            }
            MapStorage::Empty => unsafe { gos_rt_vec_new(8) },
        }
    })
}

/// Auto-dispatch `m.values() -> Vec<V>` based on the live map
/// storage. Mirrors [`gos_rt_map_keys_vec`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_values_vec(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        match &*storage {
            MapStorage::I64I64(_) | MapStorage::StrI64(_) => {
                drop(storage);
                unsafe { gos_rt_map_values_i64(m) }
            }
            MapStorage::I64Bytes(inner) => {
                let mut entries = inner.entries_vec();
                entries.sort_unstable_by_key(|(key, _)| *key);
                let values: Vec<*mut GosVec> = entries
                    .into_iter()
                    .map(|(_, value)| unsafe { byte_vec_from_slice(value) })
                    .collect();
                drop(storage);
                let out = unsafe {
                    crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                        8,
                        values.len() as i64,
                        vec_elem_kind::VEC,
                    )
                };
                for value in values {
                    unsafe { gos_rt_vec_push(out, (&raw const value).cast()) };
                }
                out
            }
            MapStorage::StrStr(_) | MapStorage::I64Str(_) | MapStorage::Bytes(_) => {
                drop(storage);
                unsafe { gos_rt_map_values_str(m) }
            }
            MapStorage::StrBytes(inner) => {
                let mut entries: Vec<(&[u8], &[u8])> = inner.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let values: Vec<*mut GosVec> = entries
                    .into_iter()
                    .map(|(_, value)| unsafe { byte_vec_from_slice(value) })
                    .collect();
                drop(storage);
                let out = unsafe {
                    crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                        8,
                        values.len() as i64,
                        vec_elem_kind::VEC,
                    )
                };
                for value in values {
                    unsafe { gos_rt_vec_push(out, (&raw const value).cast()) };
                }
                out
            }
            // Struct/tuple-keyed maps store i64 values just like `I64I64`;
            // route them through the i64 snapshot so `m.values()` / `for v in
            // m.values()` see the real values instead of an empty Vec.
            MapStorage::SkeyVal { .. } | MapStorage::EkeyVal { .. } => {
                drop(storage);
                unsafe { gos_rt_map_values_i64(m) }
            }
            MapStorage::Empty => unsafe { gos_rt_vec_new(8) },
        }
    })
}

/// `m.pop(k) -> Option<V>` for an i64-keyed map. Removes the entry
/// at `k` if present and returns the previous value as Some;
/// returns None otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_pop_i64(m: *mut GosMap, key: i64) -> i128 {
    ffi_entry!(0i128, {
        if m.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        let popped: Option<i64> = match &mut *storage {
            MapStorage::I64I64(inner) => inner.remove(&key),
            MapStorage::I64Bytes(inner) => inner
                .remove(key)
                .map(|bs| unsafe { byte_vec_from_slice(bs.as_slice()) } as i64),
            MapStorage::I64Str(inner) => inner.remove(&key).map(|bs| {
                let cstr = alloc_cstring(&bs);
                cstr as i64
            }),
            _ => None,
        };
        if popped.is_some() {
            map.len_cache = map.len_cache.saturating_sub(1);
        }
        match popped {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `m.pop(k) -> Option<V>` for a string-keyed map. The key is a
/// c-string pointer; the returned Option payload is the
/// raw 8-byte previous value (i64 directly for `StrI64`,
/// `*mut c_char` cast to i64 for `StrStr`).
unsafe fn map_pop_str_impl(m: *mut GosMap, key: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if m.is_null() || key.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let map = unsafe { &mut *m };
        let key_bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        let mut storage = map.storage.lock();
        let popped: Option<i64> = match &mut *storage {
            MapStorage::StrI64(inner) => inner.remove(ByteKeyRef::new(key_bytes)),
            MapStorage::StrStr(inner) | MapStorage::Bytes(inner) => {
                inner.remove(ByteKeyRef::new(key_bytes)).map(|bs| {
                    let cstr = alloc_cstring(&bs);
                    cstr as i64
                })
            }
            MapStorage::StrBytes(inner) => inner
                .remove(key_bytes)
                .map(|bs| unsafe { byte_vec_from_slice(bs.as_slice()) } as i64),
            _ => None,
        };
        if popped.is_some() {
            map.len_cache = map.len_cache.saturating_sub(1);
        }
        match popped {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_pop_str(m: *mut GosMap, key: *const c_char) -> i128 {
    unsafe { map_pop_str_impl(m, key) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_pop_typed_str(m: *mut GosMap, key: *const c_char) -> i128 {
    unsafe { map_pop_str_impl(m, key) }
}

/// `m.pop(k) -> Option<V>` for a struct / tuple-keyed map. Content-hashes
/// the key via `build_skey`, removes the slot, and returns the previous
/// value in the `gos_rt_result_new` i128 layout (0 = Some, 1 = None),
/// matching [`gos_rt_map_pop_i64`]. The popped value's share transfers to
/// the caller, so no blob release fires here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_pop_skey(
    m: *mut GosMap,
    key: *const u8,
    desc: *const c_char,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let none = unsafe { gos_rt_result_new(1, 0) };
        let Some(k) = (unsafe { build_skey(key, desc) }) else {
            return none;
        };
        if m.is_null() {
            return none;
        }
        let map = unsafe { &mut *m };
        let mut storage = map.storage.lock();
        let popped: Option<i64> = match &mut *storage {
            MapStorage::SkeyVal { entries, .. } => entries.remove(ByteKeyRef::new(k.as_slice())),
            _ => None,
        };
        if popped.is_some() {
            map.len_cache = map.len_cache.saturating_sub(1);
        }
        match popped {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => none,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_remove(m: *mut GosMap, key: *const u8) -> i32 {
    ffi_entry!(-1, {
        if m.is_null() || key.is_null() {
            return 0;
        }
        let map = unsafe { &mut *m };
        let k = unsafe { std::slice::from_raw_parts(key, 8) };
        let mut storage = map.storage.lock();
        let removed = match &mut *storage {
            MapStorage::Bytes(inner) => inner.remove(ByteKeyRef::new(k)).is_some(),
            _ => false,
        };
        if removed {
            map.len_cache -= 1;
            1
        } else {
            0
        }
    })
}

/// Inserts into an i64-keyed, word-valued map and returns the previous value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_i64_i64_opt(m: *mut GosMap, key: i64, val: i64) -> i128 {
    let previous = unsafe { gos_rt_map_get_i64_opt(m, key) };
    unsafe { gos_rt_map_insert_i64_i64(m, key, val) };
    previous
}

/// Inserts into a string-keyed, word-valued map and returns the previous value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_str_i64_opt(
    m: *mut GosMap,
    key: *const c_char,
    val: i64,
) -> i128 {
    let previous = unsafe { gos_rt_map_get_str_opt(m, key) };
    unsafe { gos_rt_map_insert_str_i64(m, key, val) };
    previous
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_typed_str_i64_opt(
    m: *mut GosMap,
    key: *const c_char,
    val: i64,
) -> i128 {
    let previous = unsafe { map_get_str_opt_impl(m, key) };
    unsafe { map_insert_str_i64_impl(m, key, val, true) };
    previous
}

/// Inserts into an i64-keyed string map and returns the previous value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_i64_str_opt(
    m: *mut GosMap,
    key: i64,
    val: *const c_char,
) -> i128 {
    let previous = unsafe { gos_rt_map_get_i64_opt(m, key) };
    unsafe { gos_rt_map_insert_i64_str(m, key, val) };
    previous
}

/// Inserts into a string-keyed string map and returns the previous value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_str_str_opt(
    m: *mut GosMap,
    key: *const c_char,
    val: *const c_char,
) -> i128 {
    let previous = unsafe { gos_rt_map_get_str_opt(m, key) };
    unsafe { gos_rt_map_insert_str_str(m, key, val) };
    previous
}

/// Inserts into an aggregate-keyed map and returns the previous value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_skey_opt(
    m: *mut GosMap,
    key: *const u8,
    desc: *const c_char,
    val: i64,
) -> i128 {
    let previous = unsafe { gos_rt_map_get_skey_opt(m, key, desc) };
    unsafe { gos_rt_map_insert_skey(m, key, desc, val) };
    previous
}

/// The value stored under an aggregate key, or `default` when the slot is
/// absent. Read-only: an absent key stays absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_skey(
    m: *const GosMap,
    key: *const u8,
    desc: *const c_char,
    default: i64,
) -> i64 {
    ffi_entry!(default, {
        unsafe { skey_lookup(m, key, desc) }.unwrap_or(default)
    })
}

/// The value stored under an aggregate key, inserting `default` first when
/// the slot is absent, so the caller always sees a value that is in the map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_or_insert_skey(
    m: *mut GosMap,
    key: *const u8,
    desc: *const c_char,
    default: i64,
) -> i64 {
    ffi_entry!(default, {
        if let Some(found) = unsafe { skey_lookup(m, key, desc) } {
            return found;
        }
        unsafe { gos_rt_map_insert_skey(m, key, desc, default) };
        default
    })
}

/// Adds `by` to the counter stored under an aggregate key, treating an absent
/// slot as zero, and returns the new total.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_skey(
    m: *mut GosMap,
    key: *const u8,
    desc: *const c_char,
    by: i64,
) -> i64 {
    ffi_entry!(0, {
        let next = unsafe { skey_lookup(m, key, desc) }
            .unwrap_or(0)
            .wrapping_add(by);
        unsafe { gos_rt_map_insert_skey(m, key, desc, next) };
        next
    })
}

/// Canonical bytes of an enum node: its discriminant followed by each payload
/// field in declaration order, recursing into nested nodes of the same enum.
/// Two equal-valued nodes at distinct allocations encode identically, which is
/// what makes an enum key hash by value the way the VM does.
///
/// `desc` is the same blob [`gos_rt_enum_struct_eq`] walks: `[num_variants]`
/// then, per variant, `[num_fields, kind_0, ..]`. Returns `None` for a shape
/// the walk cannot encode, which leaves the caller on pointer identity.
/// The canonical by-value key of an enum node: its discriminant and payload,
/// so two equal-valued nodes at distinct allocations key one slot. Shared with
/// the set family, which keys its enum elements the same way.
///
/// # Safety
/// `node` is an enum node and `desc` its variant-layout descriptor.
pub(crate) unsafe fn enum_canonical_key(node: *mut u8, desc: *const i64) -> Option<Vec<u8>> {
    unsafe { enum_canonical_bytes(node, desc) }
}

unsafe fn enum_canonical_bytes(node: *mut u8, desc: *const i64) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(16);
    unsafe { append_enum_canonical(node, desc, &mut out) }.then_some(out)
}

unsafe fn append_enum_canonical(node: *mut u8, desc: *const i64, out: &mut Vec<u8>) -> bool {
    let raw = node as usize;
    let base = crate::c_abi::rc::untag_rc(node);
    if desc.is_null() {
        return false;
    }
    if base.is_null() {
        // A payload-less variant is a tagged null pointer: the discriminant
        // lives in the tag bits and is the whole key, so two such variants
        // encode distinctly and sort by their own order.
        out.push(((raw & 7) >> 1) as u8);
        return true;
    }
    // Discriminant: a small heap enum tags it into the pointer's low bits
    // (`base | (disc << 1)`); a larger one keeps it in the RcHeader byte at
    // payload-3. Mirrors `gos_rt_enum_struct_eq`.
    let tag = raw & 7;
    let disc = if tag != 0 {
        (tag >> 1) as u8
    } else {
        unsafe { *base.sub(3) }
    };
    let num_variants = unsafe { *desc };
    if i64::from(disc) >= num_variants {
        return false;
    }
    out.push(disc);
    let mut idx = 1usize;
    for _ in 0..disc {
        let nf = unsafe { *desc.add(idx) }.max(0);
        idx += 1 + nf as usize;
    }
    let nf = unsafe { *desc.add(idx) }.max(0);
    idx += 1;
    for f in 0..nf {
        let kind = unsafe { *desc.add(idx + f as usize) };
        let word = unsafe { *(base as *const i64).add(f as usize) };
        match kind {
            // A `String` field folds by content, like the `'S'` slot of an
            // aggregate key.
            2 => {
                let sptr: *const c_char = std::ptr::with_exposed_provenance(word as usize);
                if sptr.is_null() {
                    out.extend_from_slice(&0u64.to_le_bytes());
                } else {
                    let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(sptr) };
                    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
            }
            3 => {
                if !unsafe { append_enum_canonical(word as *mut u8, desc, out) } {
                    return false;
                }
            }
            // `Vec`-shaped payloads carry no fixed slot count; they stay on
            // pointer identity rather than encoding a partial key.
            4 | 5 => return false,
            _ => out.extend_from_slice(&word.to_le_bytes()),
        }
    }
    true
}

/// Runs `f` with the enum-key entry table, installing it when the map is
/// still empty. Returns `None` when the map holds some other storage shape.
unsafe fn with_ekey_entries<R>(
    m: *mut GosMap,
    install: bool,
    f: impl FnOnce(&mut FxHashMap<ByteKey, EnumEntry>, &mut i64) -> R,
) -> Option<R> {
    if m.is_null() {
        return None;
    }
    let map = unsafe { &mut *m };
    let mut storage = map.storage.lock();
    if install && matches!(*storage, MapStorage::Empty) {
        *storage = MapStorage::EkeyVal {
            entries: FxHashMap::default(),
        };
    }
    let MapStorage::EkeyVal { entries } = &mut *storage else {
        return None;
    };
    let mut len = map.len_cache;
    let out = f(entries, &mut len);
    map.len_cache = len;
    Some(out)
}

/// Inserts under an enum key, retaining the node so the map can hand the same
/// value back from a snapshot. Returns the previous value word, if the key was
/// already present.
unsafe fn ekey_insert(m: *mut GosMap, key: *mut u8, desc: *const i64, val: i64) -> Option<i64> {
    let bytes = unsafe { enum_canonical_bytes(key, desc) }?;
    unsafe {
        with_ekey_entries(m, true, |entries, len| {
            unsafe { crate::c_abi::rc::gos_rt_rc_retain(key) };
            let replaced = entries.insert(
                bytes.into(),
                EnumEntry {
                    value: val,
                    key_node: key,
                },
            );
            // The replaced entry's own share of its key node is done.
            if let Some(prev) = &replaced {
                unsafe { crate::c_abi::rc::gos_rt_rc_release(prev.key_node) };
            } else {
                *len += 1;
            }
            replaced.map(|prev| prev.value)
        })
        .flatten()
    }
}

/// The value word stored under an enum key, or `None` when absent.
unsafe fn ekey_lookup(m: *const GosMap, key: *mut u8, desc: *const i64) -> Option<i64> {
    let bytes = unsafe { enum_canonical_bytes(key, desc) }?;
    if m.is_null() {
        return None;
    }
    let map = unsafe { &*m };
    let storage = map.storage.lock();
    let MapStorage::EkeyVal { entries } = &*storage else {
        return None;
    };
    entries
        .get(ByteKeyRef::new(bytes.as_slice()))
        .map(|e| e.value)
}

/// `m.insert(k, v)` for an enum-keyed map, returning `Option<V>` in the
/// `gos_rt_result_new` layout (0 = Some, 1 = None).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_insert_ekey_opt(
    m: *mut GosMap,
    key: *mut u8,
    desc: *const i64,
    val: i64,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        match unsafe { ekey_insert(m, key, desc, val) } {
            Some(prev) => unsafe { gos_rt_result_new(0, prev) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `m.get(k) -> Option<V>` for an enum-keyed map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_ekey_opt(
    m: *const GosMap,
    key: *mut u8,
    desc: *const i64,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        match unsafe { ekey_lookup(m, key, desc) } {
            Some(v) => unsafe { gos_rt_result_new(0, v) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `m.contains_key(k)` for an enum-keyed map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_contains_ekey(
    m: *const GosMap,
    key: *mut u8,
    desc: *const i64,
) -> bool {
    ffi_entry!(false, { unsafe { ekey_lookup(m, key, desc) }.is_some() })
}

/// `m.pop(k)` / `m.remove(k)` for an enum-keyed map, returning `Option<V>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_pop_ekey(
    m: *mut GosMap,
    key: *mut u8,
    desc: *const i64,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let none = unsafe { gos_rt_result_new(1, 0) };
        let Some(bytes) = (unsafe { enum_canonical_bytes(key, desc) }) else {
            return none;
        };
        let popped = unsafe {
            with_ekey_entries(m, false, |entries, len| {
                entries
                    .remove(ByteKeyRef::new(bytes.as_slice()))
                    .inspect(|entry| {
                        *len = len.saturating_sub(1);
                        unsafe { crate::c_abi::rc::gos_rt_rc_release(entry.key_node) };
                    })
            })
        };
        match popped.flatten() {
            Some(entry) => unsafe { gos_rt_result_new(0, entry.value) },
            None => none,
        }
    })
}

/// `m.get_or(k, default)` for an enum-keyed map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_get_or_ekey(
    m: *const GosMap,
    key: *mut u8,
    desc: *const i64,
    default: i64,
) -> i64 {
    ffi_entry!(default, {
        unsafe { ekey_lookup(m, key, desc) }.unwrap_or(default)
    })
}

/// `m.or_insert(k, default)` for an enum-keyed map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_or_insert_ekey(
    m: *mut GosMap,
    key: *mut u8,
    desc: *const i64,
    default: i64,
) -> i64 {
    ffi_entry!(default, {
        if let Some(found) = unsafe { ekey_lookup(m, key, desc) } {
            return found;
        }
        unsafe { ekey_insert(m, key, desc, default) };
        default
    })
}

/// `m.inc(k, by)` for an enum-keyed map; an absent slot counts as zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_inc_ekey(
    m: *mut GosMap,
    key: *mut u8,
    desc: *const i64,
    by: i64,
) -> i64 {
    ffi_entry!(0, {
        let next = unsafe { ekey_lookup(m, key, desc) }
            .unwrap_or(0)
            .wrapping_add(by);
        unsafe { ekey_insert(m, key, desc, next) };
        next
    })
}

/// Snapshots an enum-keyed map's keys as a `GosVec` of node pointers, in
/// canonical-key order so `keys()`, `values()`, and `iter()` agree.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_map_keys_ekey(m: *const GosMap) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if m.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let map = unsafe { &*m };
        let storage = map.storage.lock();
        let MapStorage::EkeyVal { entries } = &*storage else {
            return unsafe { gos_rt_vec_new(8) };
        };
        let mut rows: Vec<(&[u8], *mut u8)> = entries
            .iter()
            .map(|(k, e)| (k.as_slice(), e.key_node))
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                8,
                rows.len() as i64,
                vec_elem_kind::RC_ENUM,
            )
        };
        for (_, node) in rows {
            // The snapshot hands out its own share of each node.
            unsafe { crate::c_abi::rc::gos_rt_rc_retain(node) };
            let word = node as i64;
            unsafe { gos_rt_vec_push(out, std::ptr::addr_of!(word).cast::<u8>()) };
        }
        out
    })
}

/// The raw value word stored under an aggregate key, or `None` when absent.
unsafe fn skey_lookup(m: *const GosMap, key: *const u8, desc: *const c_char) -> Option<i64> {
    let k = unsafe { build_skey(key, desc) }?;
    if m.is_null() {
        return None;
    }
    let map = unsafe { &*m };
    let storage = map.storage.lock();
    match &*storage {
        MapStorage::SkeyVal { entries, .. } => entries.get(ByteKeyRef::new(k.as_slice())).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod map_iter_tests {
    use super::*;
    use std::ffi::CStr;

    unsafe fn formatted_map(map: *const GosMap) -> String {
        let rendered = unsafe { gos_rt_map_format(map) };
        assert!(!rendered.is_null());
        let text = unsafe { CStr::from_ptr(rendered) }
            .to_string_lossy()
            .into_owned();
        unsafe { crate::c_abi::gos_rt_str_free(rendered) };
        text
    }

    #[test]
    fn map_format_quotes_and_sorts_string_keys() {
        unsafe {
            let map = gos_rt_map_new(8, 8);
            gos_rt_map_insert_str_i64(map, crate::c_abi::string::test_gos_str("zebra"), 1);
            gos_rt_map_insert_str_i64(map, crate::c_abi::string::test_gos_str("apple"), 2);
            gos_rt_map_insert_str_i64(map, crate::c_abi::string::test_gos_str("mango"), 3);

            assert_eq!(
                formatted_map(map),
                r#"{"apple": 2, "mango": 3, "zebra": 1}"#
            );
            gos_rt_map_free(map);
        }
    }

    #[test]
    fn map_keys_i64_snapshots_inserted_keys() {
        unsafe {
            let m = gos_rt_map_new(8, 8);
            gos_rt_map_insert_i64_i64(m, 1, 100);
            gos_rt_map_insert_i64_i64(m, 2, 200);
            gos_rt_map_insert_i64_i64(m, 3, 50);
            assert_eq!(gos_rt_map_len(m), 3);
            let v = gos_rt_map_keys_i64(m);
            assert_eq!(gos_rt_vec_len(v), 3);
            let mut keys: Vec<i64> = (0..gos_rt_vec_len(v))
                .map(|i| {
                    let p = gos_rt_vec_get_ptr(v, i);
                    (p as *const i64).read_unaligned()
                })
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, vec![1, 2, 3]);
        }
    }

    #[test]
    fn typed_capacity_constructor_preserves_string_key_layout() {
        unsafe {
            let m = gos_rt_map_new_with_capacity_typed(1, 0, 8);
            // `insert` takes ownership of the key, so the lookup needs its own.
            gos_rt_map_insert_str_i64(m, crate::c_abi::string::test_gos_str("alpha"), 7);
            assert_eq!(gos_rt_map_len(m), 1);
            let probe = crate::c_abi::string::test_gos_str("alpha");
            assert_eq!(gos_rt_map_get_or_str_i64(m, probe, -1), 7);
            gos_rt_map_free(m);
        }
    }

    #[test]
    fn typed_byte_values_use_compact_storage_across_map_operations() {
        unsafe {
            let m = gos_rt_map_new_with_capacity_typed(1, 2, 4);
            let first = byte_vec_from_slice(&[1, 2, 3]);
            gos_rt_map_insert_str_i64(m, crate::c_abi::string::test_gos_str("alpha"), first as i64);
            let replacement = byte_vec_from_slice(&[4, 5]);
            gos_rt_map_insert_str_i64(
                m,
                crate::c_abi::string::test_gos_str("alpha"),
                replacement as i64,
            );

            assert_eq!(gos_rt_map_len(m), 1);
            assert!(gos_rt_map_contains_key_str(
                m,
                crate::c_abi::string::test_gos_str("alpha")
            ));
            {
                let storage = (*m).storage.lock();
                let MapStorage::StrBytes(inner) = &*storage else {
                    panic!("expected compact byte-vector map storage");
                };
                assert_eq!(inner.get(b"alpha".as_slice()), Some(&[4, 5][..]));
            }

            let values = gos_rt_map_values_vec(m);
            assert_eq!(gos_rt_vec_len(values), 1);
            let value = *(gos_rt_vec_get_ptr(values, 0) as *const *mut GosVec);
            assert_eq!(gos_rt_vec_len(value), 2);
            assert_eq!(
                std::slice::from_raw_parts((*value).ptr.as_ptr(), 2),
                &[4, 5]
            );
            gos_rt_vec_free(values);

            assert!(gos_rt_map_remove_str(
                m,
                crate::c_abi::string::test_gos_str("alpha")
            ));
            assert_eq!(gos_rt_map_len(m), 0);
            gos_rt_map_free(m);
        }
    }
}
