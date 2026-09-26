#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::cmp::Ordering;
use std::ffi::c_char;

use super::*;

// ---------------------------------------------------------------
// Ordering over a value read through a descriptor stream: the one
// comparison the ordered containers and the sequence sorts share, so
// two values order the same wherever the language compares them.
//
// The stream is a flat byte sequence, walked alongside the value's
// slots:
//   0..=5            one slot - int, uint, float, bool, char, String
//   TUPLE_TAG_NESTED arity, then that many descriptors, laid out inline
//   DESC_ARRAY       count, per-element slot span, then one descriptor
//   DESC_VEC         one descriptor, over the elements behind the handle
//   DESC_OPTION      one descriptor (the Some arm)
//   DESC_RESULT      two descriptors (the Ok arm, then the Err arm)
//   DESC_ENUM        inline flag, variant count, then per variant its
//                    field count followed by that many descriptors
//   DESC_SELF        the enclosing enum's own descriptor, for a field
//                    whose type is that enum
// ---------------------------------------------------------------

/// What a walk decides: an order, or only whether two values are equal.
///
/// The two differ for a float, whose IEEE `==` says a NaN equals nothing
/// while an order has to place it somewhere.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpMode {
    /// `-1` / `0` / `1`.
    Order,
    /// `0` for equal, nonzero otherwise.
    Equal,
}

/// Where a descriptor's value sits relative to the slot it is reached from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpStorage {
    /// The value's own slots begin here.
    Inline,
    /// This slot holds a word addressing the value.
    ByWord,
}

/// How many slots the descriptor at `cursor` spans where it is stored
/// inline, leaving the cursor untouched.
pub(crate) unsafe fn desc_slot_span(tags: *const u8, cursor: usize) -> usize {
    let mut c = cursor;
    unsafe { desc_span_walk(tags, &mut c) }
}

unsafe fn desc_span_walk(tags: *const u8, cursor: &mut usize) -> usize {
    let tag = unsafe { *tags.add(*cursor) };
    *cursor += 1;
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            let mut total = 0usize;
            for _ in 0..arity {
                total += unsafe { desc_span_walk(tags, cursor) };
            }
            total
        }
        gossamer_abi::DESC_ARRAY => {
            let count = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            let span = (unsafe { *tags.add(*cursor) } as usize).max(1);
            *cursor += 1;
            unsafe { skip_cmp_desc(tags, cursor) };
            count * span
        }
        gossamer_abi::DESC_OPTION | gossamer_abi::DESC_RESULT => {
            unsafe { skip_cmp_desc(tags, cursor) };
            if tag == gossamer_abi::DESC_RESULT {
                unsafe { skip_cmp_desc(tags, cursor) };
            }
            2
        }
        gossamer_abi::DESC_ENUM => {
            let inline = unsafe { *tags.add(*cursor) } != 0;
            *cursor -= 1;
            unsafe { skip_cmp_desc(tags, cursor) };
            if inline { 2 } else { 1 }
        }
        gossamer_abi::DESC_VEC => {
            unsafe { skip_cmp_desc(tags, cursor) };
            1
        }
        gossamer_abi::DESC_SELF => 1,
        gossamer_abi::DESC_PACKED => {
            let words = unsafe { *tags.add(*cursor) } as usize;
            let leaves = unsafe { *tags.add(*cursor + 1) } as usize;
            *cursor += 2 + leaves * 3;
            words
        }
        _ => 1,
    }
}

/// Advances `cursor` past one whole descriptor.
pub(crate) unsafe fn skip_cmp_desc(tags: *const u8, cursor: &mut usize) {
    let tag = unsafe { *tags.add(*cursor) };
    *cursor += 1;
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            for _ in 0..arity {
                unsafe { skip_cmp_desc(tags, cursor) };
            }
        }
        gossamer_abi::DESC_ARRAY => {
            *cursor += 2;
            unsafe { skip_cmp_desc(tags, cursor) };
        }
        gossamer_abi::DESC_VEC | gossamer_abi::DESC_OPTION => unsafe {
            skip_cmp_desc(tags, cursor);
        },
        gossamer_abi::DESC_RESULT => unsafe {
            skip_cmp_desc(tags, cursor);
            skip_cmp_desc(tags, cursor);
        },
        gossamer_abi::DESC_PACKED => {
            let leaves = unsafe { *tags.add(*cursor + 1) } as usize;
            *cursor += 2 + leaves * 3;
        }
        gossamer_abi::DESC_ENUM => {
            *cursor += 1;
            let variants = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            for _ in 0..variants {
                let fields = unsafe { *tags.add(*cursor) } as usize;
                *cursor += 1;
                for _ in 0..fields {
                    unsafe { skip_cmp_desc(tags, cursor) };
                }
            }
        }
        _ => {}
    }
}

/// `true` when a descriptor is one byte long and covers one slot, so a walk
/// over it is a read of that byte and nothing more.
const fn desc_tag_is_flat(tag: u8) -> bool {
    !matches!(
        tag,
        gossamer_abi::TUPLE_TAG_NESTED
            | gossamer_abi::DESC_ARRAY
            | gossamer_abi::DESC_OPTION
            | gossamer_abi::DESC_RESULT
            | gossamer_abi::DESC_ENUM
            | gossamer_abi::DESC_VEC
            | gossamer_abi::DESC_SELF
            | gossamer_abi::DESC_PACKED
    )
}

/// Orders the one-slot values at `a` and `b` under a flat descriptor tag.
/// Orders two one-slot values under their descriptor tag.
///
/// # Safety
/// `a` and `b` address one slot each, holding a value of the tag's kind.
pub(crate) unsafe fn compare_flat(tag: u8, a: *const u8, b: *const u8) -> i64 {
    unsafe { compare_flat_in(CmpMode::Order, tag, a, b) }
}

/// [`compare_flat`] deciding what `mode` asks.
///
/// # Safety
/// As for [`compare_flat`].
pub(crate) unsafe fn compare_flat_in(mode: CmpMode, tag: u8, a: *const u8, b: *const u8) -> i64 {
    let wa = unsafe { (a as *const i64).read_unaligned() };
    let wb = unsafe { (b as *const i64).read_unaligned() };
    match tag {
        1 => ord_code((wa as u64).cmp(&(wb as u64))),
        2 => float_code(mode, f64::from_bits(wa as u64), f64::from_bits(wb as u64)),
        3 => ord_code((wa & 1).cmp(&(wb & 1))),
        4 => ord_code((wa as u32).cmp(&(wb as u32))),
        // Every unit value is the same value.
        gossamer_abi::TUPLE_TAG_UNIT => 0,
        5 => {
            let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
            let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
            ord_code(unsafe { crate::c_abi::gos_rt_str_compare(sa, sb) }.cmp(&0))
        }
        _ => ord_code(wa.cmp(&wb)),
    }
}

/// Two floats under `mode`: IEEE equality, or an order that leaves an
/// unordered pair equal.
fn float_code(mode: CmpMode, a: f64, b: f64) -> i64 {
    match mode {
        CmpMode::Equal => i64::from(a != b),
        CmpMode::Order => ord_code(crate::c_abi::sort::float_order(a, b)),
    }
}

/// Orders two leaves of a packed struct, each stored as `kind` names.
///
/// # Safety
/// `a` and `b` address a leaf of that kind's width.
unsafe fn compare_packed_leaf(mode: CmpMode, kind: u8, a: *const u8, b: *const u8) -> i64 {
    use gossamer_abi::packed_leaf;
    // SAFETY: the caller hands two addresses of a leaf of this kind's width.
    unsafe {
        match kind {
            packed_leaf::I8 => ord_code(
                a.cast::<i8>()
                    .read_unaligned()
                    .cmp(&b.cast::<i8>().read_unaligned()),
            ),
            packed_leaf::U8 | packed_leaf::BOOL => {
                ord_code(a.read_unaligned().cmp(&b.read_unaligned()))
            }
            packed_leaf::I16 => ord_code(
                a.cast::<i16>()
                    .read_unaligned()
                    .cmp(&b.cast::<i16>().read_unaligned()),
            ),
            packed_leaf::U16 => ord_code(
                a.cast::<u16>()
                    .read_unaligned()
                    .cmp(&b.cast::<u16>().read_unaligned()),
            ),
            packed_leaf::I32 => ord_code(
                a.cast::<i32>()
                    .read_unaligned()
                    .cmp(&b.cast::<i32>().read_unaligned()),
            ),
            packed_leaf::U32 | packed_leaf::CHAR => ord_code(
                a.cast::<u32>()
                    .read_unaligned()
                    .cmp(&b.cast::<u32>().read_unaligned()),
            ),
            packed_leaf::U64 => ord_code(
                a.cast::<u64>()
                    .read_unaligned()
                    .cmp(&b.cast::<u64>().read_unaligned()),
            ),
            packed_leaf::FLOAT => float_code(
                mode,
                a.cast::<f64>().read_unaligned(),
                b.cast::<f64>().read_unaligned(),
            ),
            _ => ord_code(
                a.cast::<i64>()
                    .read_unaligned()
                    .cmp(&b.cast::<i64>().read_unaligned()),
            ),
        }
    }
}

fn ord_code(ordering: Ordering) -> i64 {
    match ordering {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// The discriminant of an enum node reached by word, laid out the way
/// [`crate::c_abi::map::gos_rt_enum_struct_eq`] reads it: tagged into the
/// pointer's low bits for a small enum, in the header byte otherwise.
unsafe fn node_disc(raw: usize, base: *const u8) -> i64 {
    let tag = raw & 7;
    if tag != 0 {
        (tag >> 1) as i64
    } else if base.is_null() {
        0
    } else {
        i64::from(unsafe { *base.sub(3) })
    }
}

/// Compares the values at `a` and `b` through the descriptor at `cursor`,
/// leaving the cursor past that descriptor. `self_desc` is the descriptor a
/// `DESC_SELF` field reads, when one is in scope.
pub(crate) unsafe fn compare_desc(
    a: *const u8,
    b: *const u8,
    tags: *const u8,
    cursor: &mut usize,
    storage: CmpStorage,
    self_desc: Option<usize>,
) -> i64 {
    unsafe { compare_desc_in(CmpMode::Order, a, b, tags, cursor, storage, self_desc) }
}

/// [`compare_desc`] deciding what `mode` asks.
///
/// # Safety
/// As for [`compare_desc`].
pub(crate) unsafe fn compare_desc_in(
    mode: CmpMode,
    a: *const u8,
    b: *const u8,
    tags: *const u8,
    cursor: &mut usize,
    storage: CmpStorage,
    self_desc: Option<usize>,
) -> i64 {
    let tag = unsafe { *tags.add(*cursor) };
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            *cursor += 1;
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // A tuple reached by word - a carrier's payload - keeps its slots
            // in the block that word addresses.
            let (a, b) = unsafe { inline_bases(a, b, storage) };
            // Where every field is one byte of descriptor over one slot, the
            // field's descriptor is at a known offset and its span is one, so
            // the ordering is read straight off the slots. The general walk
            // below re-derives both per field, per comparison, which is what
            // an ordered container spends its time on.
            let flat = (0..arity).all(|i| desc_tag_is_flat(unsafe { *tags.add(*cursor + i) }));
            if flat {
                let mut result = 0i64;
                for i in 0..arity {
                    let tag = unsafe { *tags.add(*cursor + i) };
                    let ord = unsafe { compare_flat_in(mode, tag, a.add(i * 8), b.add(i * 8)) };
                    if result == 0 {
                        result = ord;
                    }
                }
                *cursor += arity;
                return result;
            }
            let mut result = 0i64;
            let mut slot = 0usize;
            for _ in 0..arity {
                let span = unsafe { desc_slot_span(tags, *cursor) };
                // Field order decides the ordering, so once a field has
                // answered the rest are only walked past, not compared.
                if result == 0 {
                    let mut c = *cursor;
                    result = unsafe {
                        compare_desc_in(
                            mode,
                            a.add(slot * 8),
                            b.add(slot * 8),
                            tags,
                            &mut c,
                            CmpStorage::Inline,
                            self_desc,
                        )
                    };
                }
                unsafe { skip_cmp_desc(tags, cursor) };
                slot += span;
            }
            result
        }
        gossamer_abi::DESC_ARRAY => {
            *cursor += 1;
            let count = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            let span = (unsafe { *tags.add(*cursor) } as usize).max(1);
            *cursor += 1;
            let (base_a, base_b) = unsafe { inline_bases(a, b, storage) };
            let elem_desc = *cursor;
            let mut result = 0i64;
            for i in 0..count {
                let mut c = elem_desc;
                let ord = unsafe {
                    compare_desc_in(
                        mode,
                        base_a.add(i * span * 8),
                        base_b.add(i * span * 8),
                        tags,
                        &mut c,
                        CmpStorage::Inline,
                        self_desc,
                    )
                };
                if result == 0 {
                    result = ord;
                }
            }
            unsafe { skip_cmp_desc(tags, cursor) };
            result
        }
        gossamer_abi::DESC_VEC => {
            *cursor += 1;
            let va: *const GosVec = unsafe { word_ptr(a) };
            let vb: *const GosVec = unsafe { word_ptr(b) };
            let elem_desc = *cursor;
            unsafe { skip_cmp_desc(tags, cursor) };
            unsafe { compare_vec_in(mode, va, vb, tags, elem_desc, self_desc) }
        }
        gossamer_abi::DESC_OPTION | gossamer_abi::DESC_RESULT => {
            *cursor += 1;
            let is_option = tag == gossamer_abi::DESC_OPTION;
            let (pa, pb) = unsafe { carrier_pairs(a, b, storage) };
            let first = *cursor;
            unsafe { skip_cmp_desc(tags, cursor) };
            let second = *cursor;
            if !is_option {
                unsafe { skip_cmp_desc(tags, cursor) };
            }
            let (da, payload_a) = unsafe { carrier_words(pa) };
            let (db, payload_b) = unsafe { carrier_words(pb) };
            if da != db {
                // `None` ranks after `Some`, and `Err` after `Ok`, which is
                // the declaration order of both.
                return ord_code(da.cmp(&db));
            }
            if is_option && da != 0 {
                return 0;
            }
            let arm = if da == 0 { first } else { second };
            let mut c = arm;
            unsafe {
                compare_desc_in(
                    mode,
                    std::ptr::addr_of!(payload_a).cast::<u8>(),
                    std::ptr::addr_of!(payload_b).cast::<u8>(),
                    tags,
                    &mut c,
                    CmpStorage::ByWord,
                    self_desc,
                )
            }
        }
        gossamer_abi::DESC_ENUM => {
            let own = *cursor;
            *cursor += 1;
            let inline = unsafe { *tags.add(*cursor) } != 0;
            *cursor += 1;
            let variants = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // Variant descriptors are indexed by discriminant, so record
            // where each starts before comparing.
            let mut starts = Vec::with_capacity(variants);
            for _ in 0..variants {
                starts.push(*cursor);
                let fields = unsafe { *tags.add(*cursor) } as usize;
                *cursor += 1;
                for _ in 0..fields {
                    unsafe { skip_cmp_desc(tags, cursor) };
                }
            }
            let (da, fields_a) = unsafe { enum_parts(a, storage, inline) };
            let (db, fields_b) = unsafe { enum_parts(b, storage, inline) };
            if da != db {
                return ord_code(da.cmp(&db));
            }
            let Some(&start) = starts.get(da.max(0) as usize) else {
                return 0;
            };
            let mut c = start;
            let count = unsafe { *tags.add(c) } as usize;
            c += 1;
            // An inline enum keeps a single-field variant's field in the
            // payload word itself; a variant with more fields keeps them in
            // a block the payload word addresses.
            let (fields_a, fields_b) = if inline && count > 1 {
                (unsafe { word_ptr::<u8>(fields_a) }, unsafe {
                    word_ptr::<u8>(fields_b)
                })
            } else {
                (fields_a, fields_b)
            };
            let mut result = 0i64;
            let mut slot = 0usize;
            for _ in 0..count {
                let span = unsafe { desc_slot_span(tags, c) };
                let mut field_cursor = c;
                let ord = unsafe {
                    compare_desc_in(
                        mode,
                        fields_a.add(slot * 8),
                        fields_b.add(slot * 8),
                        tags,
                        &mut field_cursor,
                        CmpStorage::Inline,
                        Some(own),
                    )
                };
                unsafe { skip_cmp_desc(tags, &mut c) };
                slot += span;
                if result == 0 {
                    result = ord;
                }
            }
            result
        }
        gossamer_abi::DESC_PACKED => {
            let leaves = unsafe { *tags.add(*cursor + 2) } as usize;
            let first = *cursor + 3;
            *cursor = first + leaves * 3;
            let (base_a, base_b) = unsafe { inline_bases(a, b, storage) };
            for leaf in 0..leaves {
                let at = first + leaf * 3;
                let offset = usize::from(u16::from_le_bytes([unsafe { *tags.add(at) }, unsafe {
                    *tags.add(at + 1)
                }]));
                let kind = unsafe { *tags.add(at + 2) };
                let ord = unsafe {
                    compare_packed_leaf(mode, kind, base_a.add(offset), base_b.add(offset))
                };
                if ord != 0 {
                    return ord;
                }
            }
            0
        }
        gossamer_abi::DESC_SELF => {
            *cursor += 1;
            let Some(start) = self_desc else {
                return 0;
            };
            let mut c = start;
            unsafe { compare_desc_in(mode, a, b, tags, &mut c, CmpStorage::ByWord, self_desc) }
        }
        _ => {
            *cursor += 1;
            unsafe { compare_flat_in(mode, tag, a, b) }
        }
    }
}

/// The addresses a multi-slot value's own slots start at.
unsafe fn inline_bases(a: *const u8, b: *const u8, storage: CmpStorage) -> (*const u8, *const u8) {
    if storage == CmpStorage::Inline {
        (a, b)
    } else {
        (unsafe { word_ptr(a) }, unsafe { word_ptr(b) })
    }
}

/// The value a slot's word addresses.
unsafe fn word_ptr<T>(slot: *const u8) -> *const T {
    let word = unsafe { (slot as *const i64).read_unaligned() };
    std::ptr::with_exposed_provenance(word as usize)
}

/// The `[disc, payload]` pairs of two `Option` / `Result` carriers.
unsafe fn carrier_pairs(
    a: *const u8,
    b: *const u8,
    storage: CmpStorage,
) -> (*const i64, *const i64) {
    if storage == CmpStorage::Inline {
        (a.cast::<i64>(), b.cast::<i64>())
    } else {
        (unsafe { word_ptr(a) }, unsafe { word_ptr(b) })
    }
}

unsafe fn carrier_words(pair: *const i64) -> (i64, i64) {
    if pair.is_null() {
        (0, 0)
    } else {
        unsafe { (pair.read_unaligned(), pair.add(1).read_unaligned()) }
    }
}

/// An enum value's discriminant and the address of the selected variant's
/// fields: the second slot for an inline enum whose variant carries one
/// field, the node's own slots otherwise.
unsafe fn enum_parts(slot: *const u8, storage: CmpStorage, inline: bool) -> (i64, *const u8) {
    if inline {
        let base = if storage == CmpStorage::Inline {
            slot
        } else {
            unsafe { word_ptr::<u8>(slot) }
        };
        if base.is_null() {
            return (0, base);
        }
        let disc = unsafe { (base as *const i64).read_unaligned() };
        (disc, unsafe { base.add(8) })
    } else {
        let raw = unsafe { crate::c_abi::vec::slot_read_word(slot) }.expose_provenance();
        let base: *const u8 = std::ptr::with_exposed_provenance(raw & !7usize);
        (unsafe { node_disc(raw, base) }, base)
    }
}

/// Lexicographic ordering of two sequences, element by element; on a shared
/// prefix the shorter one is less.
unsafe fn compare_vec(
    a: *const GosVec,
    b: *const GosVec,
    tags: *const u8,
    elem_desc: usize,
    self_desc: Option<usize>,
) -> i64 {
    unsafe { compare_vec_in(CmpMode::Order, a, b, tags, elem_desc, self_desc) }
}

unsafe fn compare_vec_in(
    mode: CmpMode,
    a: *const GosVec,
    b: *const GosVec,
    tags: *const u8,
    elem_desc: usize,
    self_desc: Option<usize>,
) -> i64 {
    let (la, lb) = (
        if a.is_null() { 0 } else { unsafe { (*a).len } },
        if b.is_null() { 0 } else { unsafe { (*b).len } },
    );
    let shared = la.min(lb);
    for i in 0..shared {
        let ea = unsafe { elem_addr(a, i) };
        let eb = unsafe { elem_addr(b, i) };
        let mut c = elem_desc;
        let ord =
            unsafe { compare_desc_in(mode, ea, eb, tags, &mut c, CmpStorage::Inline, self_desc) };
        if ord != 0 {
            return ord;
        }
    }
    ord_code(la.cmp(&lb))
}

unsafe fn elem_addr(v: *const GosVec, idx: i64) -> *const u8 {
    let vec = unsafe { &*v };
    unsafe { vec.ptr.add((idx as usize) * (vec.elem_bytes as usize)) }
}

/// A decoded ordering descriptor.
///
/// A container that orders its elements reads the same descriptor for every
/// comparison it makes. Deciding its shape once per operation leaves the
/// comparison itself a read of the slots.
pub(crate) enum CmpPlan<'a> {
    /// One slot holding a signed machine word.
    IntWord,
    /// That many slots, each a signed machine word, compared in order.
    IntTuple(usize),
    /// One slot under one tag.
    Flat(u8),
    /// One tag per slot, compared in order until one decides.
    FlatTuple(&'a [u8]),
    /// Any other shape, read from the descriptor at each comparison.
    Walk,
}

/// The descriptor tag of a signed machine word - the tag
/// [`compare_flat`] reaches through its default arm, and the one the
/// overwhelming majority of ordered elements carry.
const DESC_TAG_INT: u8 = 0;

/// Orders two signed machine words.
///
/// # Safety
/// `a` and `b` address one slot each.
#[inline]
pub(crate) unsafe fn compare_int_word(a: *const u8, b: *const u8) -> i64 {
    let wa = unsafe { (a as *const i64).read_unaligned() };
    let wb = unsafe { (b as *const i64).read_unaligned() };
    ord_code(wa.cmp(&wb))
}

/// Orders two runs of `slots` signed machine words, lexicographically.
///
/// # Safety
/// `a` and `b` each address `slots` slots.
#[inline]
pub(crate) unsafe fn compare_int_slots(slots: usize, a: *const u8, b: *const u8) -> i64 {
    for i in 0..slots {
        let ord = unsafe { compare_int_word(a.add(i * 8), b.add(i * 8)) };
        if ord != 0 {
            return ord;
        }
    }
    0
}

/// Orders two values by walking their whole descriptor.
///
/// # Safety
/// `a` and `b` address values `tags` describes.
#[inline]
pub(crate) unsafe fn compare_whole(a: *const u8, b: *const u8, tags: *const u8) -> i64 {
    let mut cursor = 0usize;
    unsafe { compare_desc(a, b, tags, &mut cursor, CmpStorage::Inline, None) }
}

/// Decodes `tags` into the plan its comparisons follow.
///
/// # Safety
/// `tags` is null or a whole ordering descriptor.
pub(crate) unsafe fn plan_cmp<'a>(tags: *const u8) -> CmpPlan<'a> {
    if tags.is_null() {
        return CmpPlan::Walk;
    }
    let tag = unsafe { *tags };
    if tag == DESC_TAG_INT {
        return CmpPlan::IntWord;
    }
    if desc_tag_is_flat(tag) {
        return CmpPlan::Flat(tag);
    }
    if tag == gossamer_abi::TUPLE_TAG_NESTED {
        let arity = unsafe { *tags.add(1) } as usize;
        let fields = unsafe { std::slice::from_raw_parts(tags.add(2), arity) };
        if fields.iter().all(|&t| t == DESC_TAG_INT) {
            return CmpPlan::IntTuple(arity);
        }
        if fields.iter().all(|&t| desc_tag_is_flat(t)) {
            return CmpPlan::FlatTuple(fields);
        }
    }
    CmpPlan::Walk
}

/// Orders two runs of one-slot values, one tag per slot, lexicographically.
///
/// # Safety
/// `a` and `b` each address `fields.len()` slots.
#[inline]
pub(crate) unsafe fn compare_flat_slots(fields: &[u8], a: *const u8, b: *const u8) -> i64 {
    for (i, &tag) in fields.iter().enumerate() {
        let ord = unsafe { compare_flat(tag, a.add(i * 8), b.add(i * 8)) };
        if ord != 0 {
            return ord;
        }
    }
    0
}

/// Compares two values of one type through their ordering descriptor,
/// answering `-1` / `0` / `1`.
///
/// # Safety
/// `a` and `b` address values `tags` describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_desc_cmp(a: *const u8, b: *const u8, tags: *const u8) -> i64 {
    ffi_entry!(0, {
        if a.is_null() || b.is_null() || tags.is_null() {
            return 0;
        }
        let mut cursor = 0usize;
        unsafe { compare_desc(a, b, tags, &mut cursor, CmpStorage::Inline, None) }
    })
}

/// Whether two values of one type are equal through their ordering
/// descriptor, answering `1` or `0`. A float decides by IEEE `==`, so a NaN
/// equals nothing, as it does on the interpreter.
///
/// # Safety
/// `a` and `b` address values `tags` describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_desc_eq(a: *const u8, b: *const u8, tags: *const u8) -> i64 {
    ffi_entry!(0, {
        if a.is_null() || b.is_null() || tags.is_null() {
            return i64::from(a == b);
        }
        let mut cursor = 0usize;
        let code = unsafe {
            compare_desc_in(
                CmpMode::Equal,
                a,
                b,
                tags,
                &mut cursor,
                CmpStorage::Inline,
                None,
            )
        };
        i64::from(code == 0)
    })
}

/// Orders two sequences lexicographically, each element through
/// `elem_tags`, answering `-1` / `0` / `1`. A null handle is an empty
/// sequence.
///
/// # Safety
/// `a` and `b` are null or `GosVec` handles whose elements `elem_tags`
/// describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_desc_cmp(
    a: *const GosVec,
    b: *const GosVec,
    elem_tags: *const u8,
) -> i64 {
    ffi_entry!(0, {
        if elem_tags.is_null() {
            return 0;
        }
        unsafe { compare_vec(a, b, elem_tags, 0, None) }
    })
}
