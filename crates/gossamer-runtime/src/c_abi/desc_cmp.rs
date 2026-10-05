#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
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
    // SAFETY: this `unsafe fn`'s caller passes `tags` a compiler-emitted descriptor and `cursor`
    // one of its entries.
    unsafe { desc_span_walk(tags, &mut c) }
}

unsafe fn desc_span_walk(tags: *const u8, cursor: &mut usize) -> usize {
    // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s caller), and
    // the read stays inside the entry at `cursor`.
    let tag = unsafe { *tags.add(*cursor) };
    *cursor += 1;
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            let mut total = 0usize;
            for _ in 0..arity {
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                total += unsafe { desc_span_walk(tags, cursor) };
            }
            total
        }
        gossamer_abi::DESC_ARRAY => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let count = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let span = (unsafe { *tags.add(*cursor) } as usize).max(1);
            *cursor += 1;
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            count * span
        }
        gossamer_abi::DESC_OPTION | gossamer_abi::DESC_RESULT => {
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            if tag == gossamer_abi::DESC_RESULT {
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                unsafe { skip_cmp_desc(tags, cursor) };
            }
            2
        }
        gossamer_abi::DESC_ENUM => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let inline = unsafe { *tags.add(*cursor) } != 0;
            *cursor -= 1;
            // SAFETY: `cursor` addresses the enum entry just stepped back to.
            unsafe { skip_cmp_desc(tags, cursor) };
            if inline { 2 } else { 1 }
        }
        gossamer_abi::DESC_VEC => {
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            1
        }
        gossamer_abi::DESC_SELF => 1,
        gossamer_abi::DESC_PACKED => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let words = unsafe { *tags.add(*cursor) } as usize;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let leaves = unsafe { *tags.add(*cursor + 1) } as usize;
            *cursor += 2 + leaves * 3;
            words
        }
        _ => 1,
    }
}

/// Advances `cursor` past one whole descriptor.
pub(crate) unsafe fn skip_cmp_desc(tags: *const u8, cursor: &mut usize) {
    // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s caller), and
    // the read stays inside the entry at `cursor`.
    let tag = unsafe { *tags.add(*cursor) };
    *cursor += 1;
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            for _ in 0..arity {
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                unsafe { skip_cmp_desc(tags, cursor) };
            }
        }
        gossamer_abi::DESC_ARRAY => {
            *cursor += 2;
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
        }
        // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor `tags`.
        gossamer_abi::DESC_VEC | gossamer_abi::DESC_OPTION => unsafe {
            skip_cmp_desc(tags, cursor);
        },
        // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor `tags`.
        gossamer_abi::DESC_RESULT => unsafe {
            skip_cmp_desc(tags, cursor);
            skip_cmp_desc(tags, cursor);
        },
        gossamer_abi::DESC_PACKED => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let leaves = unsafe { *tags.add(*cursor + 1) } as usize;
            *cursor += 2 + leaves * 3;
        }
        gossamer_abi::DESC_ENUM => {
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let variants = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            for _ in 0..variants {
                // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
                // caller), and the read stays inside the entry at `cursor`.
                let fields = unsafe { *tags.add(*cursor) } as usize;
                *cursor += 1;
                for _ in 0..fields {
                    // SAFETY: `cursor` addresses the next entry of the compiler-emitted
                    // descriptor `tags`.
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
    // SAFETY: this `unsafe fn`'s caller passes `a` and `b` one word each, of the kind `tag`
    // names.
    unsafe { compare_flat_in(CmpMode::Order, tag, a, b) }
}

/// [`compare_flat`] deciding what `mode` asks.
///
/// # Safety
/// As for [`compare_flat`].
pub(crate) unsafe fn compare_flat_in(mode: CmpMode, tag: u8, a: *const u8, b: *const u8) -> i64 {
    // SAFETY: this `unsafe fn`'s caller passes `a` addressing one word.
    let wa = unsafe { (a as *const i64).read_unaligned() };
    // SAFETY: this `unsafe fn`'s caller passes `b` addressing one word.
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
            // SAFETY: words of tag 5 are null or live string bodies, which the comparison
            // accepts.
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
/// [`crate::c_abi::desc_format::gos_rt_enum_struct_eq`] reads it: tagged into the
/// pointer's low bits for a small enum, in the header byte otherwise.
unsafe fn node_disc(raw: usize, base: *const u8) -> i64 {
    let tag = raw & 7;
    if tag != 0 {
        (tag >> 1) as i64
    } else if base.is_null() {
        0
    } else {
        // SAFETY: an untagged node carries its discriminant in the header byte three below the
        // payload.
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
    // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry at
    // `cursor` describes.
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
    // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s caller), and
    // the read stays inside the entry at `cursor`.
    let tag = unsafe { *tags.add(*cursor) };
    match tag {
        gossamer_abi::TUPLE_TAG_NESTED => {
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let arity = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // A tuple reached by word - a carrier's payload - keeps its slots
            // in the block that word addresses.
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (a, b) = unsafe { inline_bases(a, b, storage) };
            // Where every field is one byte of descriptor over one slot, the
            // field's descriptor is at a known offset and its span is one, so
            // the ordering is read straight off the slots. The general walk
            // below re-derives both per field, per comparison, which is what
            // an ordered container spends its time on.
            // SAFETY: `i` is below the tuple's arity, inside the entry.
            let flat = (0..arity).all(|i| desc_tag_is_flat(unsafe { *tags.add(*cursor + i) }));
            if flat {
                let mut result = 0i64;
                for i in 0..arity {
                    // SAFETY: `i` is below the tuple's arity, inside the entry.
                    let tag = unsafe { *tags.add(*cursor + i) };
                    // SAFETY: each flat field is one word at slot `i` of the tuple.
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
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                let span = unsafe { desc_slot_span(tags, *cursor) };
                // Field order decides the ordering, so once a field has
                // answered the rest are only walked past, not compared.
                if result == 0 {
                    let mut c = *cursor;
                    // SAFETY: each field lies at slot `slot` of the tuple, laid out as its entry
                    // describes.
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
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                unsafe { skip_cmp_desc(tags, cursor) };
                slot += span;
            }
            result
        }
        gossamer_abi::DESC_ARRAY => {
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let count = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let span = (unsafe { *tags.add(*cursor) } as usize).max(1);
            *cursor += 1;
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (base_a, base_b) = unsafe { inline_bases(a, b, storage) };
            let elem_desc = *cursor;
            let mut result = 0i64;
            for i in 0..count {
                let mut c = elem_desc;
                // SAFETY: element `i` lies at `i * span` slots, laid out as the element entry
                // describes.
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
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            result
        }
        gossamer_abi::DESC_VEC => {
            *cursor += 1;
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let va: *const GosVec = unsafe { word_ptr(a) };
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let vb: *const GosVec = unsafe { word_ptr(b) };
            let elem_desc = *cursor;
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            // SAFETY: `va` and `vb` are the vecs the values name, null or live.
            unsafe { compare_vec_in(mode, va, vb, tags, elem_desc, self_desc) }
        }
        gossamer_abi::DESC_OPTION | gossamer_abi::DESC_RESULT => {
            *cursor += 1;
            let is_option = tag == gossamer_abi::DESC_OPTION;
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (pa, pb) = unsafe { carrier_pairs(a, b, storage) };
            let first = *cursor;
            // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
            // `tags`.
            unsafe { skip_cmp_desc(tags, cursor) };
            let second = *cursor;
            if !is_option {
                // SAFETY: `cursor` addresses the next entry of the compiler-emitted descriptor
                // `tags`.
                unsafe { skip_cmp_desc(tags, cursor) };
            }
            // SAFETY: `pa` is a carrier pair or null, which `carrier_words` accepts.
            let (da, payload_a) = unsafe { carrier_words(pa) };
            // SAFETY: `pb` is a carrier pair or null, which `carrier_words` accepts.
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
            // SAFETY: both payloads have the arm's shape, laid out as its entry describes.
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
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let inline = unsafe { *tags.add(*cursor) } != 0;
            *cursor += 1;
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let variants = unsafe { *tags.add(*cursor) } as usize;
            *cursor += 1;
            // Variant descriptors are indexed by discriminant, so record
            // where each starts before comparing.
            let mut starts = Vec::with_capacity(variants);
            for _ in 0..variants {
                starts.push(*cursor);
                // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
                // caller), and the read stays inside the entry at `cursor`.
                let fields = unsafe { *tags.add(*cursor) } as usize;
                *cursor += 1;
                for _ in 0..fields {
                    // SAFETY: `cursor` addresses the next entry of the compiler-emitted
                    // descriptor `tags`.
                    unsafe { skip_cmp_desc(tags, cursor) };
                }
            }
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (da, fields_a) = unsafe { enum_parts(a, storage, inline) };
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (db, fields_b) = unsafe { enum_parts(b, storage, inline) };
            if da != db {
                return ord_code(da.cmp(&db));
            }
            let Some(&start) = starts.get(da.max(0) as usize) else {
                return 0;
            };
            let mut c = start;
            // SAFETY: `c` addresses the variant's entry, inside the descriptor.
            let count = unsafe { *tags.add(c) } as usize;
            c += 1;
            // An inline enum keeps a single-field variant's field in the
            // payload word itself; a variant with more fields keeps them in
            // a block the payload word addresses.
            let (fields_a, fields_b) = if inline && count > 1 {
                // SAFETY: a multi-field inline variant keeps its fields in the block the payload
                // word addresses.
                (unsafe { word_ptr::<u8>(fields_a) }, unsafe {
                    word_ptr::<u8>(fields_b)
                })
            } else {
                (fields_a, fields_b)
            };
            let mut result = 0i64;
            let mut slot = 0usize;
            for _ in 0..count {
                // SAFETY: `c` addresses the field's entry.
                let span = unsafe { desc_slot_span(tags, c) };
                let mut field_cursor = c;
                // SAFETY: each field lies at slot `slot` of the variant's fields.
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
                // SAFETY: `c` addresses the field's entry.
                unsafe { skip_cmp_desc(tags, &mut c) };
                slot += span;
                if result == 0 {
                    result = ord;
                }
            }
            result
        }
        gossamer_abi::DESC_PACKED => {
            // SAFETY: `tags` is a compiler-emitted ordering descriptor (this `unsafe fn`'s
            // caller), and the read stays inside the entry at `cursor`.
            let leaves = unsafe { *tags.add(*cursor + 2) } as usize;
            let first = *cursor + 3;
            *cursor = first + leaves * 3;
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            let (base_a, base_b) = unsafe { inline_bases(a, b, storage) };
            for leaf in 0..leaves {
                let at = first + leaf * 3;
                // SAFETY: each leaf entry is three bytes inside the descriptor.
                let offset = usize::from(u16::from_le_bytes([unsafe { *tags.add(at) }, unsafe {
                    *tags.add(at + 1)
                }]));
                // SAFETY: each leaf entry is three bytes inside the descriptor.
                let kind = unsafe { *tags.add(at + 2) };
                // SAFETY: each leaf lies at its offset inside the packed value.
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
            // SAFETY: a self-reference names the enclosing descriptor entry, and the values are
            // nodes of it.
            unsafe { compare_desc_in(mode, a, b, tags, &mut c, CmpStorage::ByWord, self_desc) }
        }
        _ => {
            *cursor += 1;
            // SAFETY: this `unsafe fn`'s caller passes `a` and `b` values laid out as the entry
            // at `cursor` describes.
            unsafe { compare_flat_in(mode, tag, a, b) }
        }
    }
}

/// The addresses a multi-slot value's own slots start at.
unsafe fn inline_bases(a: *const u8, b: *const u8, storage: CmpStorage) -> (*const u8, *const u8) {
    if storage == CmpStorage::Inline {
        (a, b)
    } else {
        // SAFETY: in by-word storage each value is one word addressing its block.
        (unsafe { word_ptr(a) }, unsafe { word_ptr(b) })
    }
}

/// The value a slot's word addresses.
unsafe fn word_ptr<T>(slot: *const u8) -> *const T {
    // SAFETY: this `unsafe fn`'s caller passes `slot` addressing one word.
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
        // SAFETY: in by-word storage each value is one word addressing its pair.
        (unsafe { word_ptr(a) }, unsafe { word_ptr(b) })
    }
}

unsafe fn carrier_words(pair: *const i64) -> (i64, i64) {
    if pair.is_null() {
        (0, 0)
    } else {
        // SAFETY: `pair` is non-null (checked above) and addresses two words.
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
            // SAFETY: in by-word storage the value is one word addressing its block.
            unsafe { word_ptr::<u8>(slot) }
        };
        if base.is_null() {
            return (0, base);
        }
        // SAFETY: `base` is non-null (checked above) and addresses the enum's discriminant word.
        let disc = unsafe { (base as *const i64).read_unaligned() };
        // SAFETY: the enum's fields follow its discriminant word.
        (disc, unsafe { base.add(8) })
    } else {
        // SAFETY: this `unsafe fn`'s caller passes `slot` addressing one word.
        let raw = unsafe { crate::c_abi::vec::slot_read_word(slot) }.expose_provenance();
        let base: *const u8 = std::ptr::with_exposed_provenance(raw & !7usize);
        // SAFETY: `base` is the node the word addresses, or null.
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
    // SAFETY: this `unsafe fn`'s caller passes `a` and `b` null or live vecs laid out as
    // `elem_desc` describes.
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
        // SAFETY: `a` is non-null (checked) and, per this `unsafe fn`'s caller, a live vec.
        if a.is_null() { 0 } else { unsafe { (*a).len } },
        // SAFETY: `b` is non-null (checked) and, per this `unsafe fn`'s caller, a live vec.
        if b.is_null() { 0 } else { unsafe { (*b).len } },
    );
    let shared = la.min(lb);
    // SAFETY: `elem_desc` addresses the element descriptor inside `tags`.
    let elem_tag = unsafe { *tags.add(elem_desc) };
    for i in 0..shared {
        // SAFETY: `i` is below `a`'s length.
        let ea = unsafe { elem_addr(a, i) };
        // SAFETY: `i` is below `b`'s length.
        let eb = unsafe { elem_addr(b, i) };
        // A vector of narrow scalars stores each at its own width, which a
        // comparison of whole words would read past.
        // SAFETY: `a` and `b` are live vecs with element `i` (checked above).
        let (wa, wb) = unsafe { ((*a).elem_bytes, (*b).elem_bytes) };
        if (wa < 8 || wb < 8) && matches!(elem_tag, 0 | 1 | 3 | 4) {
            let signed = elem_tag == 0;
            // SAFETY: `ea` and `eb` address one element of their vec's width.
            let (va, vb) =
                unsafe { (narrow_scalar(ea, wa, signed), narrow_scalar(eb, wb, signed)) };
            // SAFETY: `va` and `vb` are words on the stack.
            let ord = unsafe {
                compare_flat_in(
                    mode,
                    elem_tag,
                    (&raw const va).cast(),
                    (&raw const vb).cast(),
                )
            };
            if ord != 0 {
                return ord;
            }
            continue;
        }
        let mut c = elem_desc;
        let ord =
            // SAFETY: `ea` and `eb` are elements laid out as `elem_desc` describes.
            unsafe { compare_desc_in(mode, ea, eb, tags, &mut c, CmpStorage::Inline, self_desc) };
        if ord != 0 {
            return ord;
        }
    }
    ord_code(la.cmp(&lb))
}

/// The scalar of `width` bytes at `at`, widened to a word: sign-extended
/// when `signed`, zero-extended otherwise.
///
/// # Safety
/// `at` addresses `width` readable bytes.
unsafe fn narrow_scalar(at: *const u8, width: u32, signed: bool) -> i64 {
    // SAFETY: `at` addresses `width` bytes (contract).
    unsafe {
        match (width, signed) {
            (1, true) => i64::from(at.cast::<i8>().read_unaligned()),
            (1, false) => i64::from(at.read_unaligned()),
            (2, true) => i64::from(at.cast::<i16>().read_unaligned()),
            (2, false) => i64::from(at.cast::<u16>().read_unaligned()),
            (4, true) => i64::from(at.cast::<i32>().read_unaligned()),
            (4, false) => i64::from(at.cast::<u32>().read_unaligned()),
            _ => at.cast::<i64>().read_unaligned(),
        }
    }
}

unsafe fn elem_addr(v: *const GosVec, idx: i64) -> *const u8 {
    // SAFETY: this `unsafe fn`'s caller passes `v` a live vec.
    let vec = unsafe { &*v };
    // SAFETY: this `unsafe fn`'s caller passes `idx` below the vec's length.
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
    // SAFETY: this `unsafe fn`'s caller passes `a` addressing one word.
    let wa = unsafe { (a as *const i64).read_unaligned() };
    // SAFETY: this `unsafe fn`'s caller passes `b` addressing one word.
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
        // SAFETY: this `unsafe fn`'s caller passes `a` and `b` addressing `slots` words each.
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
    // SAFETY: this `unsafe fn`'s caller passes `a` and `b` laid out as `tags` describes.
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
    // SAFETY: `tags` is non-null (checked above) and, per this `unsafe fn`'s contract, a whole
    // ordering descriptor, whose first entry this read stays inside.
    let tag = unsafe { *tags };
    if tag == DESC_TAG_INT {
        return CmpPlan::IntWord;
    }
    if desc_tag_is_flat(tag) {
        return CmpPlan::Flat(tag);
    }
    if tag == gossamer_abi::TUPLE_TAG_NESTED {
        // SAFETY: `tags` is non-null (checked above) and, per this `unsafe fn`'s contract, a
        // whole ordering descriptor, whose first entry this read stays inside.
        let arity = unsafe { *tags.add(1) } as usize;
        // SAFETY: a nested tuple entry lists its `arity` field tags after the arity byte.
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
        // SAFETY: this `unsafe fn`'s caller passes `a` and `b` holding one word per field.
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
        // SAFETY: `a`, `b`, and `tags` are this shim's non-null arguments (checked above), laid
        // out as `tags` describes (C-ABI contract).
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
        // SAFETY: `a`, `b`, and `tags` are this shim's non-null arguments (checked above), laid
        // out as `tags` describes (C-ABI contract).
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
        // SAFETY: `a` and `b` are this shim's `Vec` arguments, null or live, with elements laid
        // out as `elem_tags` describes (C-ABI contract).
        unsafe { compare_vec(a, b, elem_tags, 0, None) }
    })
}
