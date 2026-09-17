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

use crate::c_abi::GosVec;
use indexmap::{IndexMap, IndexSet};
use rustc_hash::FxBuildHasher;
use std::os::raw::c_char;

/// A membership table keyed by content, in the order elements were added.
type SetTable<T> = IndexSet<T, FxBuildHasher>;
/// The aggregate family's canonical-bytes to stored-slots table, in the order
/// elements were added.
type AggregateTable = IndexMap<Box<[u8]>, Box<[u8]>, FxBuildHasher>;

// ---------------------------------------------------------------
// Sets - one heap table per element family, with the pointer to the
// table being the value user code sees. Each family stores its
// elements in their own representation: text keys as `String`,
// integer keys as `i64`, and struct/tuple keys by their canonical
// slot bytes.
//
// A `Set` traverses in no particular order: the tables keep the order
// elements were added, which every tier reproduces, and no program may
// rely on it. A `BTreeSet` sorts, and carries `ordered` to say so.
// ---------------------------------------------------------------

#[derive(Default)]
pub struct GosSet {
    inner: SetTable<String>,
    /// Integer elements keep their numeric representation: a decimal-text
    /// encoding would cost a formatting pass and an allocation per membership
    /// test, and store roughly twice the bytes per live element.
    i64_inner: SetTable<i64>,
    /// Aggregate elements are keyed by their canonical slot bytes and retain
    /// an owned copy of those slots for `iter()` / set algebra.
    struct_inner: AggregateTable,
    /// Each aggregate element is one enum node, held in its single slot.
    node_elements: bool,
    /// The slot descriptor the aggregate elements were keyed under, kept so a
    /// sorted read orders them by field value rather than by the bytes their
    /// canonical encoding happens to spell.
    skey_desc: Option<Box<[u8]>>,
    /// A `BTreeSet` reads its elements in sorted order; a `Set` reads them in
    /// the table's own order. Set algebra carries the flag onto its result, so
    /// `a.union(b)` reads the way `a` does.
    ordered: bool,
}

/// How a counted word inside an aggregate element's slots takes and gives back
/// a share.
#[derive(Clone, Copy)]
enum CountedWord {
    /// A `String` or an enum node: reference counted.
    Rc,
    /// A `Vec`: its own share count.
    Vec,
}

impl Clone for GosSet {
    fn clone(&self) -> Self {
        let copy = Self {
            inner: self.inner.clone(),
            i64_inner: self.i64_inner.clone(),
            struct_inner: self.struct_inner.clone(),
            node_elements: self.node_elements,
            skey_desc: self.skey_desc.clone(),
            ordered: self.ordered,
        };
        copy.retain_all_elements();
        copy
    }
}

impl Drop for GosSet {
    fn drop(&mut self) {
        for slots in self.struct_inner.values() {
            // SAFETY: every stored element holds a share of each counted word.
            unsafe { self.release_element(slots) };
        }
    }
}

impl GosSet {
    /// The counted words an aggregate element's slots hold: the node of an
    /// enum element, or each `String` and `Vec` field the descriptor names.
    fn counted_words(&self, slots: &[u8]) -> Vec<(*mut u8, CountedWord)> {
        let word_at = |index: usize| -> *mut u8 {
            let bytes = slots
                .get(index * 8..index * 8 + 8)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .unwrap_or_default();
            // A slot is eight bytes wide on every target, while a pointer is
            // the target's own width: read the word, then narrow it.
            let word = u64::from_le_bytes(bytes);
            std::ptr::with_exposed_provenance_mut(usize::try_from(word).unwrap_or_default())
        };
        if self.node_elements {
            return vec![(word_at(0), CountedWord::Rc)];
        }
        let Some(desc) = &self.skey_desc else {
            return Vec::new();
        };
        desc.iter()
            .enumerate()
            .filter_map(|(index, kind)| match kind {
                b'S' => Some((word_at(index), CountedWord::Rc)),
                b'V' => Some((word_at(index), CountedWord::Vec)),
                _ => None,
            })
            .filter(|(word, _)| !word.is_null())
            .collect()
    }

    /// Takes a share of every counted word `slots` holds.
    unsafe fn retain_element(&self, slots: &[u8]) {
        for (word, kind) in self.counted_words(slots) {
            match kind {
                CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_retain(word) },
                CountedWord::Vec => unsafe { crate::c_abi::gos_rt_vec_retain(word.cast()) },
            }
        }
    }

    /// Gives back a share of every counted word `slots` holds.
    unsafe fn release_element(&self, slots: &[u8]) {
        for (word, kind) in self.counted_words(slots) {
            match kind {
                CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_release(word) },
                CountedWord::Vec => unsafe { crate::c_abi::map::gos_rt_vec_free(word.cast()) },
            }
        }
    }

    /// Takes a share of every counted word of every aggregate element, for a
    /// table whose slots were copied from another set.
    fn retain_all_elements(&self) {
        for slots in self.struct_inner.values() {
            // SAFETY: the copied slots name words the source set keeps alive.
            unsafe { self.retain_element(slots) };
        }
    }

    /// The layout a vec of this set's aggregate elements owns.
    fn element_slot_children(&self) -> Box<[crate::c_abi::vec::VecSlotChild]> {
        use crate::c_abi::vec::{VecSlotChild, vec_elem_kind};
        let Some(desc) = &self.skey_desc else {
            return Box::new([]);
        };
        desc.iter()
            .enumerate()
            .filter_map(|(word, kind)| {
                let kind = match kind {
                    b'S' => vec_elem_kind::STRING,
                    b'V' => vec_elem_kind::VEC,
                    _ => return None,
                };
                Some(VecSlotChild {
                    gate: -1,
                    disc_word: 0,
                    word,
                    kind,
                })
            })
            .collect()
    }

    /// Orders the aggregate table's keys the way their fields compare, which
    /// is the order the interpreter reads the same elements in.
    fn sorted_aggregate_keys(&self) -> Vec<&[u8]> {
        let mut keys: Vec<&[u8]> = self.struct_inner.keys().map(AsRef::as_ref).collect();
        match &self.skey_desc {
            Some(desc) => {
                keys.sort_by_cached_key(|key| crate::c_abi::map::skey_order_key(key, desc));
            }
            None => keys.sort_unstable(),
        }
        keys
    }
}

/// Structural equality of two sets: equal when they hold the same elements,
/// whatever order those were added in. Each element family compares on its own
/// terms - text by its bytes, integers by value, and an aggregate by the
/// canonical slot bytes it is keyed under - so two sets built differently but
/// holding the same members compare equal, as they do in Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_eq(a: *const GosSet, b: *const GosSet) -> i64 {
    ffi_entry!(0, {
        if std::ptr::eq(a, b) {
            return 1;
        }
        if a.is_null() || b.is_null() {
            return 0;
        }
        let (x, y) = unsafe { (&*a, &*b) };
        let same = x.inner == y.inner
            && x.i64_inner == y.i64_inner
            && x.struct_inner.len() == y.struct_inner.len()
            && x.struct_inner
                .keys()
                .all(|key| y.struct_inner.contains_key(key));
        i64::from(same)
    })
}

/// Marks every counted word a set's aggregate elements hold as shared, before
/// the set is reached from another thread. Text and integer elements are owned
/// copies with no count of their own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_mark_shared(set: *mut GosSet) {
    ffi_entry!((), {
        if set.is_null() {
            return;
        }
        let set = unsafe { &*set };
        for slots in set.struct_inner.values() {
            for (word, kind) in set.counted_words(slots) {
                match kind {
                    CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_mark_shared(word) },
                    CountedWord::Vec => unsafe {
                        crate::c_abi::vec::gos_rt_vec_mark_shared(word.cast());
                    },
                }
            }
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_new() -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosSet::default()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_btree_set_new() -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        let mut set = GosSet::default();
        set.ordered = true;
        Box::into_raw(Box::new(set))
    })
}

/// Marks a set as reading in sorted order (the `BTreeSet` contract).
fn mark_ordered(set: *mut GosSet) -> *mut GosSet {
    if !set.is_null() {
        unsafe { &mut *set }.ordered = true;
    }
    set
}

/// `xs.clone()` for a `Set` / `BTreeSet` receiver, and the primitive a
/// `let` binding or by-value call argument uses to give the binding an
/// independent table instead of aliasing the source. Every element `GosSet`
/// stores is owned content (a `String`, or a struct's canonical slot bytes)
/// rather than an RC pointer, so a structural `Clone` deep-copies the whole
/// table with no child retain pass needed - unlike `gos_rt_map_clone`.
///
/// `GosSet` carries no refcount of its own (unlike `GosVec` / strings) - a
/// plain pointer copy at a `let` binding either double-frees the table once
/// both bindings' drop points run, or leaves both bindings mutating the
/// same live table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_clone(src: *const GosSet) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        if src.is_null() {
            return unsafe { gos_rt_set_new() };
        }
        Box::into_raw(Box::new(unsafe { &*src }.clone()))
    })
}

/// Replaces the set a by-value aggregate field holds with its own clone.
///
/// `slot` is the field's storage address. Emitted after a copy that would
/// otherwise leave two aggregates naming one table: a `GosSet` has no
/// reference count, so a copied field takes a value of its own the way a set
/// binding does. Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosSet` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_field_clone(slot: *mut *mut GosSet) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let s = unsafe { slot.read_unaligned() };
        if s.is_null() {
            return;
        }
        let cloned = unsafe { gos_rt_set_clone(s) };
        unsafe { slot.write_unaligned(cloned) };
    });
}

/// Frees the set a by-value aggregate field owns and nulls the slot.
///
/// Nulling makes the release idempotent, so the drop pass may book it at more
/// than one exit edge of the same field without the second booking touching a
/// freed table. Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosSet` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_field_release(slot: *mut *mut GosSet) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let s = unsafe { slot.read_unaligned() };
        if s.is_null() {
            return;
        }
        unsafe { slot.write_unaligned(std::ptr::null_mut()) };
        unsafe { crate::c_abi::gos_rt_set_free(s) };
    });
}

/// `*dst = src` through a `&mut Set`: the table every holder of the
/// reference names keeps its identity and takes a copy of `src`'s elements.
///
/// # Safety
/// `dst` and `src` are live `GosSet`s or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_assign(dst: *mut GosSet, src: *const GosSet) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() || std::ptr::addr_eq(dst.cast_const(), src) {
            return;
        }
        unsafe { *dst = (*src).clone() };
    });
}

/// Builds a set from a `GosVec` of scalar slots. `Set::from(values)` where
/// `values` is a runtime sequence rather than a literal list; the literal
/// form is unrolled into individual inserts at lowering time.
///
/// # Safety
/// `v` must be null or a live `GosVec` whose slots hold `i64` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_from_vec_i64(v: *const GosVec) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        let set = unsafe { gos_rt_set_new() };
        if v.is_null() || set.is_null() {
            return set;
        }
        let vec = unsafe { &*v };
        let ptr = vec.ptr.cast::<i64>();
        let out = unsafe { &mut *set };
        for i in 0..vec.len.max(0) as usize {
            out.i64_inner.insert(unsafe { *ptr.add(i) });
        }
        set
    })
}

/// Builds a set from a `GosVec` of string slots.
///
/// # Safety
/// `v` must be null or a live `GosVec` whose slots hold `*const c_char`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_from_vec_str(v: *const GosVec) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        let set = unsafe { gos_rt_set_new() };
        if v.is_null() || set.is_null() {
            return set;
        }
        let vec = unsafe { &*v };
        let ptr = vec.ptr.cast::<*const c_char>();
        let out = unsafe { &mut *set };
        for i in 0..vec.len.max(0) as usize {
            let entry = unsafe { *ptr.add(i) };
            if entry.is_null() {
                continue;
            }
            out.inner
                .insert(unsafe { crate::c_abi::gos_str_arg_string(entry) });
        }
        set
    })
}

/// `BTreeSet::from(values)`; the ordered set shares this representation and
/// sorts on iteration.
///
/// # Safety
/// Same contract as [`gos_rt_set_from_vec_i64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_btree_set_from_vec_i64(v: *const GosVec) -> *mut GosSet {
    mark_ordered(unsafe { gos_rt_set_from_vec_i64(v) })
}

/// # Safety
/// Same contract as [`gos_rt_set_from_vec_str`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_btree_set_from_vec_str(v: *const GosVec) -> *mut GosSet {
    mark_ordered(unsafe { gos_rt_set_from_vec_str(v) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_insert(s: *mut GosSet, key: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() || key.is_null() {
            return 0;
        }
        let k = unsafe { crate::c_abi::gos_str_arg_string(key) };
        let s = unsafe { &mut *s };
        i64::from(s.inner.insert(k))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_contains(s: *const GosSet, key: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() || key.is_null() {
            return 0;
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        // Gossamer strings are always valid UTF-8 at the source level.
        let k: &str = unsafe { std::str::from_utf8_unchecked(bytes) };
        let s = unsafe { &*s };
        i64::from(s.inner.contains(k))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_remove(s: *mut GosSet, key: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() || key.is_null() {
            return 0;
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(key) };
        // Gossamer strings are always valid UTF-8 at the source level.
        let k: &str = unsafe { std::str::from_utf8_unchecked(bytes) };
        let s = unsafe { &mut *s };
        i64::from(s.inner.shift_remove(k))
    })
}

// `HashSet<i64>` keeps its own numeric table. The MIR dispatch routes
// i64-element sets to these entry points and routes `to_vec` to the i64
// reader, which sorts numerically to match the VM's `MapKey::Int` ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_insert_i64(s: *mut GosSet, key: i64) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &mut *s };
        i64::from(s.i64_inner.insert(key))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_contains_i64(s: *const GosSet, key: i64) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &*s };
        i64::from(s.i64_inner.contains(&key))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_remove_i64(s: *mut GosSet, key: i64) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &mut *s };
        i64::from(s.i64_inner.shift_remove(&key))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_len(s: *const GosSet) -> i64 {
    ffi_entry!(-1, {
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &*s };
        (s.inner.len() + s.i64_inner.len() + s.struct_inner.len()) as i64
    })
}

/// Return 1 when the set holds no elements, 0 otherwise.
///
/// # Safety
/// `s` is null or a live `GosSet`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_is_empty(s: *const GosSet) -> i32 {
    ffi_entry!(1, { i32::from(unsafe { gos_rt_set_len(s) } <= 0) })
}

/// How a set opens when it is rendered: its own literal spelling, the
/// same one a program writes to build it. A `Set` and a `BTreeSet` are
/// both written `#{..}`, so both render that way.
fn set_format_open() -> &'static str {
    "#{"
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_i64(s: *const GosSet, _ordered: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() {
            let set = unsafe { &*s };
            let mut keys: Vec<i64> = set.i64_inner.iter().copied().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&crate::builtins::format_int(*key));
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}

/// Renders an integer-table set whose elements were declared `bool`, `char`,
/// or `f64`: the same slots as [`gos_rt_set_format_i64`], read through the
/// element's own tag so each reads as the value it holds.
///
/// # Safety
/// `s` is a live `GosSet` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_tagged(
    s: *const GosSet,
    _ordered: i32,
    tag: i32,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() {
            let set = unsafe { &*s };
            let mut keys: Vec<i64> = set.i64_inner.iter().copied().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                unsafe {
                    crate::c_abi::map::render_tagged_word(&mut out, *key, tag as u8);
                }
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}

/// Renders a set whose elements are aggregates, each stored as its canonical
/// slot bytes and rendered through the descriptor `tags` addresses.
///
/// # Safety
/// `s` is a live `GosSet` and `tags` addresses a descriptor for its elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_desc(
    s: *const GosSet,
    _ordered: i32,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() && !tags.is_null() {
            let tags = unsafe { crate::c_abi::map::DescStream::new(tags) };
            let set = unsafe { &*s };
            let entries: Vec<&Box<[u8]>> = set
                .sorted_aggregate_keys()
                .into_iter()
                .filter_map(|key| set.struct_inner.get(key))
                .collect();
            for (index, slots) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let mut cursor = 0usize;
                unsafe {
                    crate::c_abi::map::render_desc_value(
                        &mut out,
                        slots.as_ptr(),
                        tags,
                        &mut cursor,
                    );
                }
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}

/// Renders an integer set whose elements were declared `u64` / `usize`: the
/// same slots as [`gos_rt_set_format_i64`], read as unsigned so an element at
/// or above `i64::MAX` shows its own decimal.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_u64(s: *const GosSet, _ordered: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() {
            let set = unsafe { &*s };
            let mut keys: Vec<u64> = set.i64_inner.iter().map(|n| *n as u64).collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&crate::builtins::format_uint(*key));
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_string(s: *const GosSet, _ordered: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() {
            let set = unsafe { &*s };
            let mut keys: Vec<&str> = set.inner.iter().map(String::as_str).collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(key);
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}

/// Snapshots a string set's keys into a fresh `Vec<String>`: sorted for a
/// `BTreeSet`, and otherwise in the table's own order, which a `Set` gives no
/// meaning to beyond being the same on every tier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_to_vec(s: *const GosSet) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        if s.is_null() {
            return out;
        }
        let s = unsafe { &*s };
        let mut keys: Vec<&str> = s.inner.iter().map(String::as_str).collect();
        if s.ordered {
            keys.sort_unstable();
        }
        for k in keys {
            let cstr = crate::c_abi::string::alloc_cstring(k.as_bytes());
            let slot = (cstr as usize as i64).to_ne_bytes();
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, slot.as_ptr()) };
        }
        out
    })
}

/// Snapshots an i64 set's keys into a fresh `Vec<i64>`: sorted numerically for
/// a `BTreeSet`, and otherwise in the table's own order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_to_vec_i64(s: *const GosSet) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { set_to_vec_ordered(s, crate::c_abi::map::KeyOrder::Signed) }
    })
}

/// [`gos_rt_set_to_vec_i64`] for a set whose elements were declared `u64` /
/// `usize`: a `BTreeSet` of them reads in unsigned order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_to_vec_u64(s: *const GosSet) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { set_to_vec_ordered(s, crate::c_abi::map::KeyOrder::Unsigned) }
    })
}

unsafe fn set_to_vec_ordered(
    s: *const GosSet,
    order: crate::c_abi::map::KeyOrder,
) -> *mut crate::c_abi::vec::GosVec {
    {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::PRIMITIVE)
        };
        if s.is_null() {
            return out;
        }
        let s = unsafe { &*s };
        let mut keys: Vec<i64> = s.i64_inner.iter().copied().collect();
        if s.ordered {
            keys.sort_unstable_by_key(|key| order.rank(*key));
        }
        for k in keys {
            unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(out, k) };
        }
        out
    }
}

/// Snapshots the intersection of two string sets without first allocating a
/// temporary `HashSet`. This is the native lowering for
/// `left.intersection(right).iter()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_intersection_to_vec(
    a: *const GosSet,
    b: *const GosSet,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        let (a, b) = unsafe { set_refs(a, b) };
        let mut keys: Vec<&str> = a.inner.intersection(&b.inner).map(String::as_str).collect();
        if a.ordered {
            keys.sort_unstable();
        }
        for key in keys {
            let cstr = crate::c_abi::string::alloc_cstring(key.as_bytes());
            let slot = (cstr as usize as i64).to_ne_bytes();
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, slot.as_ptr()) };
        }
        out
    })
}

/// i64 counterpart of [`gos_rt_set_intersection_to_vec`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_intersection_to_vec_i64(
    a: *const GosSet,
    b: *const GosSet,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::PRIMITIVE)
        };
        let (a, b) = unsafe { set_refs(a, b) };
        let mut keys: Vec<i64> = a.i64_inner.intersection(&b.i64_inner).copied().collect();
        if a.ordered {
            keys.sort_unstable();
        }
        for key in keys {
            unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(out, key) };
        }
        out
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_clear(s: *mut GosSet) -> *mut GosSet {
    ffi_entry!(s, {
        if !s.is_null() {
            let s = unsafe { &mut *s };
            s.inner.clear();
            s.i64_inner.clear();
            for slots in s.struct_inner.values() {
                // SAFETY: every stored element holds a share of each counted word.
                unsafe { s.release_element(slots) };
            }
            s.struct_inner.clear();
        }
        s
    })
}

/// Borrows the two operand sets, or returns empty borrows for null
/// pointers so the algebra shims never deref a null handle.
unsafe fn set_refs<'a>(a: *const GosSet, b: *const GosSet) -> (&'a GosSet, &'a GosSet) {
    static EMPTY: std::sync::OnceLock<GosSet> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(GosSet::default);
    let a = if a.is_null() { empty } else { unsafe { &*a } };
    let b = if b.is_null() { empty } else { unsafe { &*b } };
    (a, b)
}

/// Combines two sets element-family by element-family. Each family is
/// independent, so a set only ever populates one of them and the other
/// combinations reduce to empty tables.
unsafe fn set_combine(
    a: *const GosSet,
    b: *const GosSet,
    text: impl Fn(&SetTable<String>, &SetTable<String>) -> SetTable<String>,
    ints: impl Fn(&SetTable<i64>, &SetTable<i64>) -> SetTable<i64>,
    aggregates: impl Fn(&AggregateTable, &AggregateTable) -> AggregateTable,
) -> *mut GosSet {
    let (a, b) = unsafe { set_refs(a, b) };
    let combined = GosSet {
        inner: text(&a.inner, &b.inner),
        i64_inner: ints(&a.i64_inner, &b.i64_inner),
        struct_inner: aggregates(&a.struct_inner, &b.struct_inner),
        node_elements: a.node_elements || b.node_elements,
        // Both operands hold one element type, so either side's descriptor
        // describes the result's elements.
        skey_desc: a.skey_desc.clone().or_else(|| b.skey_desc.clone()),
        // The result is read the way the receiver is: `a.union(b)` on a
        // `BTreeSet` answers a `BTreeSet`.
        ordered: a.ordered,
    };
    // The combined table copied its elements' slots out of the operands.
    combined.retain_all_elements();
    Box::into_raw(Box::new(combined))
}

/// True when `pred` holds for every element family of the two operands.
unsafe fn set_relation(
    a: *const GosSet,
    b: *const GosSet,
    text: impl Fn(&SetTable<String>, &SetTable<String>) -> bool,
    ints: impl Fn(&SetTable<i64>, &SetTable<i64>) -> bool,
    aggregates: impl Fn(&AggregateTable, &AggregateTable) -> bool,
) -> i64 {
    let (a, b) = unsafe { set_refs(a, b) };
    i64::from(
        text(&a.inner, &b.inner)
            && ints(&a.i64_inner, &b.i64_inner)
            && aggregates(&a.struct_inner, &b.struct_inner),
    )
}

fn aggregate_union(a: &AggregateTable, b: &AggregateTable) -> AggregateTable {
    let mut out = a.clone();
    for (key, slots) in b {
        out.entry(key.clone()).or_insert_with(|| slots.clone());
    }
    out
}

fn aggregate_intersection(a: &AggregateTable, b: &AggregateTable) -> AggregateTable {
    a.iter()
        .filter(|(key, _)| b.contains_key(key.as_ref()))
        .map(|(key, slots)| (key.clone(), slots.clone()))
        .collect()
}

fn aggregate_difference(a: &AggregateTable, b: &AggregateTable) -> AggregateTable {
    a.iter()
        .filter(|(key, _)| !b.contains_key(key.as_ref()))
        .map(|(key, slots)| (key.clone(), slots.clone()))
        .collect()
}

/// Inserts a struct or tuple by value. `desc` is the same slot descriptor as
/// the aggregate-keyed HashMap ABI, so equal values at different addresses
/// remain equal in the native runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_insert_skey(
    s: *mut GosSet,
    key: *const u8,
    desc: *const c_char,
) -> i64 {
    ffi_entry!(-1, {
        let Some(canonical) = (unsafe { crate::c_abi::map::build_skey_for_set(key, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        let width = unsafe { crate::c_abi::gos_str_arg_len(desc) } * 8;
        let slots = unsafe { std::slice::from_raw_parts(key, width) }
            .to_vec()
            .into_boxed_slice();
        let s = unsafe { &mut *s };
        if s.skey_desc.is_none() {
            s.skey_desc = Some(unsafe { crate::c_abi::gos_str_arg_bytes(desc) }.into());
        }
        // The inserted key arrives holding shares of its counted words. A new
        // element keeps them; an equal element already present stays, and the
        // moved key's shares go back.
        if s.struct_inner.contains_key(canonical.as_slice()) {
            unsafe { s.release_element(&slots) };
            return 0;
        }
        s.struct_inner.insert(canonical.into_boxed_slice(), slots);
        1
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_contains_skey(
    s: *const GosSet,
    key: *const u8,
    desc: *const c_char,
) -> i64 {
    ffi_entry!(-1, {
        let Some(canonical) = (unsafe { crate::c_abi::map::build_skey_for_set(key, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &*s };
        i64::from(s.struct_inner.contains_key(canonical.as_slice()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_remove_skey(
    s: *mut GosSet,
    key: *const u8,
    desc: *const c_char,
) -> i64 {
    ffi_entry!(-1, {
        let Some(canonical) = (unsafe { crate::c_abi::map::build_skey_for_set(key, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &mut *s };
        match s.struct_inner.shift_remove(canonical.as_slice()) {
            Some(slots) => {
                unsafe { s.release_element(&slots) };
                1
            }
            None => 0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_to_vec_skey(
    s: *const GosSet,
    desc: *const c_char,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if desc.is_null() {
            return std::ptr::null_mut();
        }
        let width = unsafe { crate::c_abi::gos_str_arg_len(desc) } * 8;
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(
                width as u32,
                crate::c_abi::vec::vec_elem_kind::PRIMITIVE,
            )
        };
        if s.is_null() {
            return out;
        }
        let s = unsafe { &*s };
        // Each pushed element takes the vec's own share of its counted words.
        crate::c_abi::vec::vec_own_slot_children(out, s.element_slot_children());
        let entries: Vec<&[u8]> = if s.ordered {
            s.sorted_aggregate_keys()
                .into_iter()
                .filter_map(|key| s.struct_inner.get(key).map(AsRef::as_ref))
                .collect()
        } else {
            s.struct_inner.values().map(AsRef::as_ref).collect()
        };
        for slots in entries {
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, slots.as_ptr()) };
        }
        out
    })
}

/// Aggregate-key counterpart of [`gos_rt_set_intersection_to_vec`]. It reads in
/// the receiver's own order while avoiding the cloned keys and slots of an
/// intermediate intersection set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_intersection_to_vec_skey(
    a: *const GosSet,
    b: *const GosSet,
    desc: *const c_char,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if desc.is_null() {
            return std::ptr::null_mut();
        }
        let width = unsafe { crate::c_abi::gos_str_arg_len(desc) } * 8;
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(
                width as u32,
                crate::c_abi::vec::vec_elem_kind::PRIMITIVE,
            )
        };
        if a.is_null() || b.is_null() {
            return out;
        }
        let (a, b) = unsafe { (&*a, &*b) };
        // Each pushed element takes the vec's own share of its counted words.
        crate::c_abi::vec::vec_own_slot_children(out, a.element_slot_children());
        let mut entries: Vec<_> = a
            .struct_inner
            .iter()
            .filter(|(key, _)| b.struct_inner.contains_key(key.as_ref()))
            .collect();
        if a.ordered {
            entries.sort_unstable_by_key(|(key, _)| *key);
        }
        for (_, slots) in entries {
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, slots.as_ptr()) };
        }
        out
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_union(a: *const GosSet, b: *const GosSet) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            set_combine(
                a,
                b,
                |x, y| x.union(y).cloned().collect(),
                |x, y| x.union(y).copied().collect(),
                aggregate_union,
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_intersection(
    a: *const GosSet,
    b: *const GosSet,
) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            set_combine(
                a,
                b,
                |x, y| x.intersection(y).cloned().collect(),
                |x, y| x.intersection(y).copied().collect(),
                aggregate_intersection,
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_intersection_skey(
    a: *const GosSet,
    b: *const GosSet,
) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            set_combine(
                a,
                b,
                |x, y| x.intersection(y).cloned().collect(),
                |x, y| x.intersection(y).copied().collect(),
                aggregate_intersection,
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_difference(a: *const GosSet, b: *const GosSet) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            set_combine(
                a,
                b,
                |x, y| x.difference(y).cloned().collect(),
                |x, y| x.difference(y).copied().collect(),
                aggregate_difference,
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_symmetric_difference(
    a: *const GosSet,
    b: *const GosSet,
) -> *mut GosSet {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            set_combine(
                a,
                b,
                |x, y| x.symmetric_difference(y).cloned().collect(),
                |x, y| x.symmetric_difference(y).copied().collect(),
                |x, y| {
                    let mut out = aggregate_difference(x, y);
                    out.extend(aggregate_difference(y, x));
                    out
                },
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_is_subset(a: *const GosSet, b: *const GosSet) -> i64 {
    ffi_entry!(-1, {
        unsafe {
            set_relation(a, b, SetTable::is_subset, SetTable::is_subset, |x, y| {
                x.keys().all(|key| y.contains_key(key.as_ref()))
            })
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_is_superset(a: *const GosSet, b: *const GosSet) -> i64 {
    ffi_entry!(-1, {
        unsafe {
            set_relation(
                a,
                b,
                SetTable::is_superset,
                SetTable::is_superset,
                |x, y| y.keys().all(|key| x.contains_key(key.as_ref())),
            )
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_is_disjoint(a: *const GosSet, b: *const GosSet) -> i64 {
    ffi_entry!(-1, {
        unsafe {
            set_relation(
                a,
                b,
                SetTable::is_disjoint,
                SetTable::is_disjoint,
                |x, y| !x.keys().any(|key| y.contains_key(key.as_ref())),
            )
        }
    })
}

// ---------------------------------------------------------------
// Enum elements. A user enum's value is a counted node, so a set of
// them keys by the same canonical discriminant-and-payload bytes an
// enum-keyed map uses, and stores the node itself as the element -
// two equal-valued nodes then share one slot, exactly as they do in
// the interpreter.
// ---------------------------------------------------------------

/// The canonical key bytes of an enum node, or `None` when the descriptor
/// does not describe it.
unsafe fn enum_element_key(node: *mut u8, desc: *const i64) -> Option<Vec<u8>> {
    unsafe { crate::c_abi::map::enum_canonical_key(node, desc) }
}

/// Inserts an enum element by value, answering 1 when it was not present.
///
/// # Safety
/// `node` is an enum node and `desc` its variant-layout descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_insert_ekey(
    s: *mut GosSet,
    node: *mut u8,
    desc: *const i64,
) -> i64 {
    ffi_entry!(-1, {
        let Some(key) = (unsafe { enum_element_key(node, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        let slots = (node as usize as i64).to_le_bytes().to_vec();
        let s = unsafe { &mut *s };
        s.node_elements = true;
        // The node arrives as a moved share: a new element keeps it, and an
        // equal element already present stays while the moved share goes back.
        if s.struct_inner.contains_key(key.as_slice()) {
            unsafe { s.release_element(&slots) };
            return 0;
        }
        s.struct_inner
            .insert(key.into_boxed_slice(), slots.into_boxed_slice());
        1
    })
}

/// Membership test for an enum element.
///
/// # Safety
/// `node` is an enum node and `desc` its variant-layout descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_contains_ekey(
    s: *const GosSet,
    node: *mut u8,
    desc: *const i64,
) -> i64 {
    ffi_entry!(-1, {
        let Some(key) = (unsafe { enum_element_key(node, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        i64::from(unsafe { &*s }.struct_inner.contains_key(key.as_slice()))
    })
}

/// Removes an enum element, answering 1 when it was present.
///
/// # Safety
/// `node` is an enum node and `desc` its variant-layout descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_remove_ekey(
    s: *mut GosSet,
    node: *mut u8,
    desc: *const i64,
) -> i64 {
    ffi_entry!(-1, {
        let Some(key) = (unsafe { enum_element_key(node, desc) }) else {
            return 0;
        };
        if s.is_null() {
            return 0;
        }
        let s = unsafe { &mut *s };
        match s.struct_inner.shift_remove(key.as_slice()) {
            Some(slots) => {
                unsafe { s.release_element(&slots) };
                1
            }
            None => 0,
        }
    })
}

/// The set's enum elements as a `Vec` of nodes, in the set's own order.
///
/// # Safety
/// `s` is a live `GosSet` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_to_vec_ekey(
    s: *const GosSet,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::RC_ENUM)
        };
        if s.is_null() {
            return out;
        }
        let s = unsafe { &*s };
        let mut entries: Vec<_> = s.struct_inner.iter().collect();
        if s.ordered {
            entries.sort_unstable_by_key(|(key, _)| *key);
        }
        for (_, slots) in entries {
            // The vec holds its own share of each node, given back at its free.
            unsafe { s.retain_element(slots) };
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, slots.as_ptr()) };
        }
        out
    })
}

/// Renders a set of enum elements: each stored slot addresses the node its
/// descriptor reads.
///
/// # Safety
/// `s` is a live `GosSet` and `tags` addresses a descriptor for its elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_set_format_ekey(
    s: *const GosSet,
    _ordered: i32,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::from(set_format_open());
        if !s.is_null() && !tags.is_null() {
            let tags = unsafe { crate::c_abi::map::DescStream::new(tags) };
            let set = unsafe { &*s };
            let mut entries: Vec<_> = set.struct_inner.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (_, slots)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let mut cursor = 0usize;
                unsafe {
                    crate::c_abi::map::render_desc_storage(
                        &mut out,
                        slots.as_ptr(),
                        tags,
                        &mut cursor,
                        crate::c_abi::map::Storage::ByWord,
                    );
                }
            }
        }
        out.push('}');
        crate::c_abi::string::alloc_cstring(out.as_bytes())
    })
}
