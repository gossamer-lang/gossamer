//! Rendering and structural equality of tuples, vectors, and heap enums driven by the descriptor streams codegen lays out.

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

use std::os::raw::c_char;

use gossamer_abi::TUPLE_TAG_NESTED;

use super::*;

/// Appends `text` in the spelling that builds it, as a nested string renders:
/// quoted and escaped the way a map key is.
pub(crate) fn push_quoted_str(out: &mut String, text: &str) {
    use std::fmt::Write as _;
    let _ = write!(out, "{text:?}");
}

/// Appends the `char` whose code point is `word` in the spelling that builds
/// it - between single quotes, escaped as Rust's `Debug` escapes it.
pub(crate) fn push_quoted_char(out: &mut String, word: i64) {
    use std::fmt::Write as _;
    if let Some(c) = u32::try_from(word).ok().and_then(char::from_u32) {
        let _ = write!(out, "{c:?}");
    }
}

/// Renders one value word per the tuple tag encoding. Container tags read
/// the word as a handle; a tag with no handle shape renders as an integer.
pub(crate) unsafe fn render_tagged_word(out: &mut String, word: i64, tag: u8) {
    match tag {
        1 => out.push_str(&crate::builtins::format_uint(word as u64)),
        2 => out.push_str(&crate::builtins::format_float_debug(f64::from_bits(
            word as u64,
        ))),
        gossamer_abi::TUPLE_TAG_F32 => out.push_str(&crate::builtins::format_f32_debug(
            f64::from_bits(word as u64),
        )),
        gossamer_abi::TUPLE_TAG_UNIT => out.push_str("()"),
        3 => out.push_str(crate::builtins::format_bool(word & 1 != 0)),
        4 => push_quoted_char(out, word),
        5 => {
            let sp: *const c_char = std::ptr::with_exposed_provenance(word as usize);
            if !sp.is_null() {
                // SAFETY: a non-null word of tag 5 is a live string body.
                push_quoted_str(out, &unsafe { crate::c_abi::gos_str_arg_lossy(sp) });
            }
        }
        6 => {
            let vp = std::ptr::with_exposed_provenance(word as usize);
            // SAFETY: a word of tag 6 is a live `Vec<i64>` or null, which the formatter accepts.
            let rendered = unsafe { crate::c_abi::gos_rt_vec_format_i64(vp, 0) };
            if !rendered.is_null() {
                // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
            }
        }
        7 => {
            let mp = std::ptr::with_exposed_provenance(word as usize);
            // SAFETY: a word of tag 7 is a live `Map` or null, which the formatter accepts.
            let rendered = unsafe { gos_rt_map_format(mp) };
            if !rendered.is_null() {
                // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
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
    // SAFETY: `tags` is a compiler-emitted descriptor stream and `cursor` addresses an entry of
    // it (this `unsafe fn`'s caller).
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
                // SAFETY: `tags` is a compiler-emitted descriptor stream and `cursor` addresses
                // the tuple's next element entry.
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
            // SAFETY: `cursor` addresses the element descriptor that follows the array header.
            unsafe { skip_desc(tags, cursor) };
            count * span
        }
        gossamer_abi::DESC_ADT => {
            let slots = (tags.byte(*cursor + 2) as usize).max(1);
            *cursor += 3;
            slots
        }
        gossamer_abi::DESC_PACKED => {
            let words = tags.byte(*cursor) as usize;
            let leaves = tags.byte(*cursor + 1) as usize;
            *cursor += 2 + leaves * 3;
            words
        }
        gossamer_abi::DESC_OPTION => {
            // SAFETY: `cursor` addresses the payload descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            2
        }
        gossamer_abi::DESC_RESULT => {
            // SAFETY: `cursor` addresses the `Ok` payload descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            // SAFETY: `cursor` addresses the `Err` payload descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            2
        }
        _ => {
            *cursor -= 1;
            // SAFETY: `cursor` addresses the descriptor entry just stepped back to.
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
                // SAFETY: `cursor` addresses the tuple's next element entry.
                unsafe { skip_desc(tags, cursor) };
            }
        }
        // SAFETY: `cursor` addresses the element descriptor that follows.
        gossamer_abi::DESC_VEC => unsafe { skip_desc(tags, cursor) },
        // SAFETY: `cursor` addresses the key and value descriptors that follow.
        gossamer_abi::DESC_MAP => unsafe {
            skip_desc(tags, cursor);
            skip_desc(tags, cursor);
        },
        gossamer_abi::DESC_SET_I64 | gossamer_abi::DESC_SET_STR => *cursor += 1,
        gossamer_abi::DESC_CONTAINER => {
            // The byte naming the container, then one element descriptor.
            *cursor += 1;
            // SAFETY: `cursor` addresses the element descriptor that follows the container byte.
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ADT => *cursor += 3,
        gossamer_abi::DESC_PACKED => {
            let leaves = tags.byte(*cursor + 1) as usize;
            *cursor += 2 + leaves * 3;
        }
        // SAFETY: `cursor` addresses the payload descriptor that follows.
        gossamer_abi::DESC_OPTION => unsafe { skip_desc(tags, cursor) },
        gossamer_abi::DESC_ARRAY => {
            // Element count and per-element slot span, a `u16` each, then
            // one element descriptor.
            *cursor += 4;
            // SAFETY: `cursor` addresses the element descriptor that follows the array header.
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ERROR => {}
        // SAFETY: `cursor` addresses the `Ok` and `Err` payload descriptors that follow.
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
pub unsafe extern "C-unwind" fn gos_rt_tuple_format_desc(
    slots: *const i64,
    desc: *const u8,
) -> *mut c_char {
    ffi_entry_passthrough!({
        if slots.is_null() || desc.is_null() {
            return alloc_cstring(b"()");
        }
        // SAFETY: `desc` is this shim's argument, live for the call (C-ABI contract); non-null,
        // checked above.
        let tags = unsafe { DescStream::new(desc) };
        let mut out = String::new();
        let mut cursor = 0usize;
        // SAFETY: `slots` is this shim's value argument, laid out as `desc` describes (C-ABI
        // contract), and `tags` walks `desc`.
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
        // SAFETY: `base` is a compiler-emitted descriptor block, whose first word is its
        // formatter count (this `unsafe fn`'s caller).
        let fn_count = unsafe { base.cast::<i64>().read_unaligned() }.max(0) as usize;
        Self {
            // SAFETY: the formatter table follows the count word inside the block.
            fns: unsafe { base.add(8) }.cast(),
            // SAFETY: the descriptor bytes follow the `fn_count` formatter words inside the
            // block.
            bytes: unsafe { base.add(8 + fn_count * 8) },
            fn_count,
        }
    }

    pub(crate) fn byte(self, at: usize) -> u8 {
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
    // SAFETY: this `unsafe fn`'s caller passes `slot` laid out as the entry at `cursor` in `tags`
    // describes.
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
                // SAFETY: `slot` holds one word, the tuple's boxed address, in by-word storage.
                let word = unsafe { (slot as *const i64).read_unaligned() };
                std::ptr::with_exposed_provenance(word as usize)
            };
            if base.is_null() {
                out.push_str("()");
            } else {
                let mut slot_cursor = 0usize;
                // SAFETY: `base` is non-null (checked above) and holds the tuple's slots, laid
                // out as the descriptor describes.
                unsafe {
                    render_tuple_elements(out, base, tags, arity, &mut slot_cursor, cursor);
                }
            }
        }
        gossamer_abi::DESC_VEC => {
            *cursor += 1;
            // SAFETY: `slot` holds one word, the `Vec`'s handle.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let v: *const crate::c_abi::GosVec = std::ptr::with_exposed_provenance(word as usize);
            let elem_desc = *cursor;
            // A `Vec` renders in its own literal spelling; the bare
            // bracket belongs to the fixed array it shares a runtime
            // representation with, which `DESC_ARRAY` names.
            out.push_str("#[");
            if !v.is_null() {
                // SAFETY: `v` is non-null (checked above) and, as the descriptor names, a live
                // `Vec`.
                let vec = unsafe { &*v };
                for i in 0..vec.len {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    // SAFETY: `i` is below the vec's length, so the element lies inside its
                    // buffer.
                    let elem = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
                    let mut c = elem_desc;
                    // SAFETY: `elem` is one element of the vec, laid out as the element
                    // descriptor describes.
                    unsafe { render_desc_value(out, elem, tags, &mut c) };
                }
            }
            out.push(']');
            // SAFETY: `cursor` addresses the element descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_MAP => {
            *cursor += 1;
            // SAFETY: `slot` holds one word, the `Map`'s handle.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let m: *const GosMap = std::ptr::with_exposed_provenance(word as usize);
            let key_desc = *cursor;
            // SAFETY: `cursor` addresses the key descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            let val_desc = *cursor;
            // SAFETY: `cursor` addresses the value descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            // SAFETY: `m` is the map the descriptor names (null or live), and `key_desc` /
            // `val_desc` address its entries in `tags`.
            let rendered = unsafe { map_format_desc_stream(m, tags, key_desc, val_desc) };
            if rendered.is_null() {
                out.push_str("{}");
            } else {
                // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
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
                // SAFETY: `slot` holds one word, the array's boxed address, in by-word storage.
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
                // SAFETY: `i` is below the array's length, so the element lies inside its
                // storage.
                let elem = unsafe { base.add(i * elem_slots * 8) };
                // SAFETY: `elem` is one element of the array, laid out as the element descriptor
                // describes.
                unsafe { render_desc_value(out, elem, tags, &mut c) };
            }
            out.push(']');
            // SAFETY: `cursor` addresses the element descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
        }
        gossamer_abi::DESC_ERROR => {
            *cursor += 1;
            // SAFETY: `slot` holds one word, the error's handle.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            if word == 0 {
                return;
            }
            // SAFETY: a non-zero error word is a live error cell.
            let rendered = unsafe {
                crate::c_abi::gos_rt_error_display(std::ptr::with_exposed_provenance(word as usize))
            };
            if !rendered.is_null() {
                // SAFETY: `rendered` is the fresh non-null string the display answered.
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                // SAFETY: `rendered` is the fresh string the display answered, owned here.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
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
                // SAFETY: `slot` holds one word, the carrier's boxed address, in by-word storage.
                let word = unsafe { (slot as *const i64).read_unaligned() };
                std::ptr::with_exposed_provenance(word as usize)
            };
            let (disc, payload) = if pair.is_null() {
                (0i64, 0i64)
            } else {
                // SAFETY: `pair` is non-null (checked above) and addresses the carrier's two
                // words.
                unsafe { (pair.read_unaligned(), pair.add(1).read_unaligned()) }
            };
            let first_desc = *cursor;
            // SAFETY: `cursor` addresses the first arm's payload descriptor.
            unsafe { skip_desc(tags, cursor) };
            let second_desc = *cursor;
            if !is_option {
                // SAFETY: `cursor` addresses the second arm's payload descriptor.
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
            // SAFETY: `payload` is the live arm's value, laid out as `arm_desc` describes.
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
                // SAFETY: `slot` holds one word, the aggregate's address.
                let word = unsafe { crate::c_abi::vec::slot_read_word(slot) }.expose_provenance();
                std::ptr::with_exposed_provenance::<u8>(word)
            };
            // SAFETY: `arg` is the aggregate `fmt` formats (the descriptor names both).
            out.push_str(&unsafe { crate::c_abi::result::adt_fmt_string(arg, fmt) });
        }
        // A container whose elements live in the runtime: the slot holds
        // the handle, and the byte after the tag names which container.
        gossamer_abi::DESC_CONTAINER => {
            *cursor += 1;
            let which = tags.byte(*cursor);
            *cursor += 1;
            let elem_desc = *cursor;
            // SAFETY: `cursor` addresses the element descriptor that follows.
            unsafe { skip_desc(tags, cursor) };
            // SAFETY: `slot` holds one word, the container's handle.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let rendered = match which {
                28 | 30 => {
                    let owner = if which == 28 { "MaxHeap" } else { "MinHeap" };
                    let handle: *const crate::c_abi::GosVec =
                        std::ptr::with_exposed_provenance(word as usize);
                    // SAFETY: `handle` is the heap vec the descriptor names, null or live.
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
                    // SAFETY: `handle` is the deque the descriptor names, null or live, and
                    // `elem_desc` addresses its element entry in `tags`.
                    unsafe { crate::c_abi::deque::deque_format_at(handle, owner, tags, elem_desc) }
                }
            };
            out.push_str(&rendered);
        }
        gossamer_abi::DESC_SET_I64 | gossamer_abi::DESC_SET_STR => {
            *cursor += 1;
            // The byte after the tag carries the ordered flag in bit 0 and, for
            // an integer set, whether its elements were declared `u64` /
            // `usize` in bit 1.
            let flags = tags.byte(*cursor);
            let ordered = i32::from(flags & 1);
            *cursor += 1;
            // SAFETY: `slot` holds one word, the set's handle.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            let handle = std::ptr::with_exposed_provenance(word as usize);
            let rendered = if tag == gossamer_abi::DESC_SET_I64 && flags & 2 != 0 {
                // SAFETY: `handle` is the set the descriptor names, null or live, which the
                // formatter accepts.
                unsafe { crate::c_abi::gos_rt_set_format_u64(handle, ordered) }
            } else if tag == gossamer_abi::DESC_SET_I64 {
                // SAFETY: `handle` is the set the descriptor names, null or live, which the
                // formatter accepts.
                unsafe { crate::c_abi::gos_rt_set_format_i64(handle, ordered) }
            } else {
                // SAFETY: `handle` is the set the descriptor names, null or live, which the
                // formatter accepts.
                unsafe { crate::c_abi::gos_rt_set_format_string(handle, ordered) }
            };
            if rendered.is_null() {
                out.push_str("#{}");
            } else {
                // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
            }
        }
        _ => {
            *cursor += 1;
            // SAFETY: `slot` holds one word of the scalar kind `tag` names.
            let word = unsafe { (slot as *const i64).read_unaligned() };
            // SAFETY: `word` is a value of the kind `tag` names.
            unsafe { render_tagged_word(out, word, tag) };
        }
    }
}

/// Renders `count` elements starting at tag index `tag_cursor` and slot
/// index `slot_cursor`, advancing both past what it consumed.
///
/// A nested tuple's elements are flattened into the parent's slot
/// buffer, so slot and tag positions advance independently.
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
            // SAFETY: `p` holds the nested tuple's slots from `slot_cursor`, as the descriptor
            // describes.
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
            // SAFETY: `slot_cursor` is inside the tuple's slots, as the descriptor counts them.
            let field = unsafe { p.add(*slot_cursor) };
            *slot_cursor += slots;
            if let Some(fmt) = tags.fmt(index) {
                let arg = if by_slot_address {
                    field.cast::<u8>()
                } else {
                    // SAFETY: `field` is one slot inside the tuple.
                    let word = unsafe { field.read_unaligned() };
                    std::ptr::with_exposed_provenance::<u8>(word as usize)
                };
                // SAFETY: `arg` is the aggregate `fmt` formats (the descriptor names both).
                out.push_str(&unsafe { crate::c_abi::result::adt_fmt_string(arg, fmt) });
            }
            continue;
        }
        // A leaf tag names one slot and one of the shapes below. Anything
        // else is a whole descriptor - a `Vec`, a `Map`, a `Set`, an array,
        // an `Option` - which the descriptor walk renders and measures, so
        // both cursors stay on the element that follows.
        if !matches!(tag, 0..=7) {
            // SAFETY: `slot_cursor` is inside the tuple's slots, as the descriptor counts them.
            let element = unsafe { p.add(*slot_cursor) };
            *tag_cursor -= 1;
            // SAFETY: `tag_cursor` addresses a whole-value descriptor entry in `tags`.
            *slot_cursor += unsafe { desc_slot_span(tags, *tag_cursor) };
            // SAFETY: `element` is the field that entry describes.
            unsafe { render_desc_value(out, element.cast::<u8>(), tags, tag_cursor) };
            continue;
        }
        // SAFETY: `slot_cursor` is inside the tuple's slots, as the descriptor counts them.
        let word = unsafe { p.add(*slot_cursor).read_unaligned() };
        *slot_cursor += 1;
        match tag {
            0 => out.push_str(&crate::builtins::format_int(word)),
            1 => out.push_str(&crate::builtins::format_uint(word as u64)),
            2 => out.push_str(&crate::builtins::format_float_debug(f64::from_bits(
                word as u64,
            ))),
            3 => out.push_str(crate::builtins::format_bool(word & 1 != 0)),
            4 => push_quoted_char(out, word),
            5 => {
                let sp: *const c_char = std::ptr::with_exposed_provenance(word as usize);
                if !sp.is_null() {
                    // SAFETY: a non-null word of tag 5 is a live string body.
                    push_quoted_str(out, &unsafe { crate::c_abi::gos_str_arg_lossy(sp) });
                }
            }
            6 => {
                let vp = std::ptr::with_exposed_provenance(word as usize);
                // SAFETY: a word of tag 6 is a live `Vec<i64>` or null, which the formatter
                // accepts.
                let rendered = unsafe { crate::c_abi::gos_rt_vec_format_i64(vp, 0) };
                if !rendered.is_null() {
                    // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                    // The formatter answered a fresh rendering whose bytes are now copied.
                    // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                    unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
                }
            }
            7 => {
                let mp = std::ptr::with_exposed_provenance(word as usize);
                // SAFETY: a word of tag 7 is a live `Map` or null, which the formatter accepts.
                let rendered = unsafe { gos_rt_map_format(mp) };
                if !rendered.is_null() {
                    // SAFETY: `rendered` is the fresh non-null string the formatter answered.
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                    // The formatter answered a fresh rendering whose bytes are now copied.
                    // SAFETY: `rendered` is the fresh string the formatter answered, owned here.
                    unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
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
pub unsafe extern "C-unwind" fn gos_rt_tuple_format(
    p: *const i64,
    n: i64,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry_passthrough!({
        if p.is_null() || tags.is_null() || n <= 0 {
            return alloc_cstring(b"()");
        }
        let mut out = String::new();
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        // SAFETY: `p` is this shim's tuple argument of `n` elements, laid out as `tags` describes
        // (C-ABI contract).
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
/// [`gos_rt_tuple_format`]: `0` Int, `1` Uint, `2` Float, `3` Bool, `4`
/// Char, `5` Str). The first non-equal element decides; equal prefixes continue.
/// Routed to by the compiled tiers for tuple `== != < <= > >=`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_tuple_cmp(
    a: *const i64,
    b: *const i64,
    n: i64,
    tags: *const u8,
) -> i64 {
    ffi_entry_passthrough!({
        if a.is_null() || b.is_null() || tags.is_null() || n <= 0 {
            return 0;
        }
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        // SAFETY: `a` and `b` are this shim's tuple arguments of `n` elements, laid out as `tags`
        // describes (C-ABI contract).
        unsafe {
            compare_tuple_elements(
                crate::c_abi::desc_cmp::CmpMode::Order,
                a,
                b,
                tags,
                n as usize,
                &mut slot_cursor,
                &mut tag_cursor,
            )
        }
    })
}

/// Whether two tuples of `n` elements are equal under the `tags` stream,
/// answering `1` or `0`. A float element decides by IEEE `==`, so a NaN
/// equals nothing, as it does on the interpreter.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_tuple_eq(
    a: *const i64,
    b: *const i64,
    n: i64,
    tags: *const u8,
) -> i64 {
    ffi_entry_passthrough!({
        if a.is_null() || b.is_null() || tags.is_null() || n <= 0 {
            return i64::from(a == b || n <= 0);
        }
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 0usize;
        // SAFETY: `a` and `b` are this shim's tuple arguments of `n` elements, laid out as `tags`
        // describes (C-ABI contract).
        let code = unsafe {
            compare_tuple_elements(
                crate::c_abi::desc_cmp::CmpMode::Equal,
                a,
                b,
                tags,
                n as usize,
                &mut slot_cursor,
                &mut tag_cursor,
            )
        };
        i64::from(code == 0)
    })
}

/// Compares `count` elements starting at tag index `tag_cursor` and slot
/// index `slot_cursor`, advancing both past what it consumed. Returns
/// `-1` / `0` / `1`; the cursors are left past the compared elements
/// either way so a caller can keep walking.
unsafe fn compare_tuple_elements(
    mode: crate::c_abi::desc_cmp::CmpMode,
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
        // SAFETY: `tag_cursor` stays inside the `count` element entries of `tags` (this `unsafe
        // fn`'s caller).
        let tag = unsafe { *tags.add(*tag_cursor) };
        // A descriptor tag names a value whose slot word is not its own
        // order - an enum reached through its RC node, a nested sequence -
        // so it is compared through the ordering descriptor rather than as
        // the word the slot spells.
        if tag >= gossamer_abi::DESC_VEC {
            let field = *tag_cursor;
            // SAFETY: `field` addresses a whole-value descriptor entry in `tags`.
            let span = unsafe { crate::c_abi::desc_cmp::desc_slot_span(tags, field) };
            let mut walk = field;
            // SAFETY: `a` and `b` hold that element at `slot_cursor`, laid out as the entry
            // describes.
            let ord = unsafe {
                crate::c_abi::desc_cmp::compare_desc_in(
                    mode,
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
            // SAFETY: `tag_cursor` addresses the nested tuple's arity byte.
            let nested = unsafe { *tags.add(*tag_cursor) } as usize;
            *tag_cursor += 1;
            // SAFETY: `a` and `b` hold the nested tuple's slots from `slot_cursor`.
            let ord = unsafe {
                compare_tuple_elements(mode, a, b, tags, nested, slot_cursor, tag_cursor)
            };
            if result == 0 {
                result = ord;
            }
            continue;
        }
        // SAFETY: `slot_cursor` is inside `a`'s slots, as the descriptor counts them.
        let wa = unsafe { a.add(*slot_cursor).read_unaligned() };
        // SAFETY: `slot_cursor` is inside `b`'s slots, as the descriptor counts them.
        let wb = unsafe { b.add(*slot_cursor).read_unaligned() };
        *slot_cursor += 1;
        if result != 0 {
            continue;
        }
        let ord = match tag {
            1 => (wa as u64).cmp(&(wb as u64)),
            2 => {
                let (fa, fb) = (f64::from_bits(wa as u64), f64::from_bits(wb as u64));
                match mode {
                    crate::c_abi::desc_cmp::CmpMode::Equal if fa != fb => Ordering::Greater,
                    crate::c_abi::desc_cmp::CmpMode::Equal => Ordering::Equal,
                    crate::c_abi::desc_cmp::CmpMode::Order => {
                        crate::c_abi::sort::float_order(fa, fb)
                    }
                }
            }
            3 => (wa & 1).cmp(&(wb & 1)),
            4 => (wa as u32).cmp(&(wb as u32)),
            5 => {
                let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                // SAFETY: a word of tag 5 is null or a live string body, which the comparison
                // accepts.
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
        // SAFETY: `ai` is below `len`, so the element lies inside the buffer.
        let pa = unsafe { base.add(ai * stride).cast::<i64>() };
        // SAFETY: `bi` is below `len`, so the element lies inside the buffer.
        let pb = unsafe { base.add(bi * stride).cast::<i64>() };
        // SAFETY: `pa` and `pb` are elements of `n` slots, laid out as `tags` describes.
        unsafe { gos_rt_tuple_cmp(pa, pb, n, tags) }.cmp(&0)
    });
    let total = len.saturating_mul(stride);
    let mut tmp: Vec<u8> = vec![0u8; total];
    for (new_idx, &old_idx) in indices.iter().enumerate() {
        // SAFETY: each copy moves one `stride`-byte element between indices below `len`, inside
        // both buffers.
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(old_idx * stride),
                tmp.as_mut_ptr().add(new_idx * stride),
                stride,
            );
        }
    }
    // SAFETY: `tmp` and `base` each hold `len * stride` bytes, and they do not overlap.
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
    ffi_entry!({
        if v.is_null() || tags.is_null() || n <= 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &mut *v };
        if vec.len <= 1 || vec.ptr.is_null() {
            return;
        }
        let len = vec.len.max(0) as usize;
        let stride = i64::from(vec.elem_bytes).max(0) as usize;
        // SAFETY: `vec` is live and its buffer holds `len` elements of `stride` bytes, laid out
        // as `tags` describes (C-ABI contract).
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
    ffi_entry!({
        if p.is_null() || tags.is_null() || n <= 0 || len <= 1 || elem_bytes <= 0 {
            return;
        }
        // SAFETY: `p` is this shim's array argument of `len` elements of `elem_bytes` bytes
        // (non-null, checked above; C-ABI contract).
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
    ffi_entry!({
        if a.is_null() || b.is_null() {
            return std::ptr::eq(a, b);
        }
        // SAFETY: `a` is a handle from compiled code, checked non-null above and live for the whole call.
        let la = unsafe { (*a).len };
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        if la != unsafe { (*b).len } {
            return false;
        }
        for i in 0..la {
            // SAFETY: `i` is below both vecs' length, and `a` is non-null and live (checked
            // above; C-ABI contract).
            let wa = unsafe { gos_rt_vec_get_i64(a, i) };
            // SAFETY: `i` is below both vecs' length, and `b` is non-null and live (checked
            // above; C-ABI contract).
            let wb = unsafe { gos_rt_vec_get_i64(b, i) };
            let eq = match elem_tag {
                2 => f64::from_bits(wa as u64) == f64::from_bits(wb as u64),
                5 => {
                    let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                    let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                    // SAFETY: elements of tag 5 are null or live string bodies, which the
                    // comparison accepts.
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
    ffi_entry!({
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
                // SAFETY: an untagged node carries its discriminant in the header byte three
                // below the payload.
                unsafe { *base.sub(3) }
            }
        };
        let da = disc_of(raw_a, a);
        let db = disc_of(raw_b, b);
        if da != db {
            return 0;
        }
        // SAFETY: `desc` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let num_variants = unsafe { *desc };
        if i64::from(da) >= num_variants {
            return 0;
        }
        let mut idx = 1usize;
        for _ in 0..da {
            // SAFETY: `desc` is this shim's `i64` argument, non-null (checked above), live for
            // the call (C-ABI contract).
            let nf = unsafe { *desc.add(idx) }.max(0);
            idx += 1 + nf as usize;
        }
        // SAFETY: `desc` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let nf = unsafe { *desc.add(idx) }.max(0);
        idx += 1;
        for f in 0..nf {
            // SAFETY: `desc` is this shim's `i64` argument, non-null (checked above), live for
            // the call (C-ABI contract).
            let kind = unsafe { *desc.add(idx + f as usize) };
            // SAFETY: `f` is below the variant's field count, inside `a`'s payload.
            let wa = unsafe { *(a as *const i64).add(f as usize) };
            // SAFETY: `f` is below the variant's field count, inside `b`'s payload.
            let wb = unsafe { *(b as *const i64).add(f as usize) };
            let eq = match kind {
                1 => f64::from_bits(wa as u64) == f64::from_bits(wb as u64),
                2 => {
                    let sa: *const c_char = std::ptr::with_exposed_provenance(wa as usize);
                    let sb: *const c_char = std::ptr::with_exposed_provenance(wb as usize);
                    // SAFETY: string fields are null or live string bodies, which the comparison
                    // accepts.
                    unsafe { gos_rt_str_eq(sa, sb) }
                }
                // SAFETY: a field of kind 3 is a nested node of the same descriptor.
                3 => unsafe { gos_rt_enum_struct_eq(wa as *mut u8, wb as *mut u8, desc) != 0 },
                // SAFETY: a field of kind 4 is a `Vec` of nodes of the same descriptor.
                4 => unsafe { vec_self_enum_eq(wa, wb, desc) },
                // SAFETY: a field of kind 5 is a `Vec` of `(String, node)` pairs of the same
                // descriptor.
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
    // SAFETY: `va` is non-null (checked above) and a live `Vec` (this `unsafe fn`'s caller).
    let la = unsafe { (*va).len };
    // SAFETY: `vb` is non-null (checked above) and a live `Vec` (this `unsafe fn`'s caller).
    if la != unsafe { (*vb).len } {
        return false;
    }
    for i in 0..la {
        // SAFETY: `i` is below `va`'s length.
        let ea = unsafe { gos_rt_vec_get_i64(va, i) };
        // SAFETY: `i` is below `vb`'s length, which equals `va`'s.
        let eb = unsafe { gos_rt_vec_get_i64(vb, i) };
        // SAFETY: the elements are nodes of `desc`'s enum.
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
    // SAFETY: `va` is non-null (checked above) and a live `Vec` (this `unsafe fn`'s caller).
    let la = unsafe { (*va).len };
    // SAFETY: `vb` is non-null (checked above) and a live `Vec` (this `unsafe fn`'s caller).
    if la != unsafe { (*vb).len } {
        return false;
    }
    for i in 0..la {
        // SAFETY: `i` is below `va`'s length.
        let pa = unsafe { gos_rt_vec_get_ptr(va, i) };
        // SAFETY: `i` is below `vb`'s length, which equals `va`'s.
        let pb = unsafe { gos_rt_vec_get_ptr(vb, i) };
        if pa.is_null() || pb.is_null() {
            if pa != pb {
                return false;
            }
            continue;
        }
        // SAFETY: `pa` is a non-null 16-byte pair element.
        let ka = unsafe { pa.cast::<i64>().read_unaligned() };
        // SAFETY: `pb` is a non-null 16-byte pair element.
        let kb = unsafe { pb.cast::<i64>().read_unaligned() };
        let sa: *const c_char = std::ptr::with_exposed_provenance(ka as usize);
        let sb: *const c_char = std::ptr::with_exposed_provenance(kb as usize);
        // SAFETY: the pair's first words are null or live string bodies, which the comparison
        // accepts.
        if !unsafe { gos_rt_str_eq(sa, sb) } {
            return false;
        }
        // SAFETY: `pa` is a 16-byte pair element; its second word is at offset 8.
        let ea = unsafe { pa.add(8).cast::<i64>().read_unaligned() };
        // SAFETY: `pb` is a 16-byte pair element; its second word is at offset 8.
        let eb = unsafe { pb.add(8).cast::<i64>().read_unaligned() };
        // SAFETY: the pair's second words are nodes of `desc`'s enum.
        if unsafe { gos_rt_enum_struct_eq(ea as *mut u8, eb as *mut u8, desc) } == 0 {
            return false;
        }
    }
    true
}
