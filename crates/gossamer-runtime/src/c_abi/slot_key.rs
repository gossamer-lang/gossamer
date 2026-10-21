//! The counted words an aggregate's flat slots hold, and the comparator a
//! container reaches its key type's own `cmp` through.
//!
//! A container that keys by an aggregate's slots holds one share of every
//! `String`, `Vec`, and node those slots point at, and gives it back when the
//! entry dies. An ordered container whose key type writes its own `cmp` calls
//! that comparator on the same slots.

use std::cmp::Ordering;

use parking_lot::Mutex;

/// How a counted word inside an aggregate's slots takes and gives back a
/// share.
#[derive(Clone, Copy)]
pub(crate) enum CountedWord {
    /// A `String` or a node: reference counted.
    Rc,
    /// A `Vec`: its own share count.
    Vec,
    /// The copy blob a carrier's payload word points at: reference counted
    /// when the word names one.
    Blob,
}

/// The per-slot part of a key descriptor, one character per slot of the key:
/// `s` a scalar word, `S` a `String`, `V` a `Vec`, `B` the word of a payload
/// boxed in a copy blob. The layouts the `B` slots point at follow a `|`,
/// one parenthesised descriptor per `B` in slot order.
pub(crate) fn desc_slots(desc: &[u8]) -> &[u8] {
    desc.split(|&c| c == b'|').next().unwrap_or(desc)
}

/// The bytes the slots a key descriptor string describes occupy.
///
/// # Safety
/// `desc` is null or a live descriptor string.
pub(crate) unsafe fn desc_width(desc: *const std::ffi::c_char) -> usize {
    if desc.is_null() {
        return 0;
    }
    // SAFETY: `desc` is non-null (checked above) and a live string (this `unsafe fn`'s contract).
    desc_slots(unsafe { crate::c_abi::gos_str_arg_bytes(desc) }).len() * 8
}

/// The descriptors of the payloads the `B` slots of `desc` point at, in slot
/// order.
pub(crate) fn desc_boxed(desc: &[u8]) -> Vec<&[u8]> {
    let Some(bar) = desc.iter().position(|&c| c == b'|') else {
        return Vec::new();
    };
    let rest = &desc[bar + 1..];
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, &c) in rest.iter().enumerate() {
        match c {
            b'(' => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(&rest[start..i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// A boxed payload's descriptor and the blob layout built for it.
type BoxedMeta = (Box<[u8]>, &'static [i64]);

/// The layout of a blob rebuilt from a stored key for the boxed payload
/// `desc`: the strings, sequences, and boxed payloads its slots own. A blob
/// holds its layout for its whole life, so each one is built once and kept.
pub(crate) fn boxed_blob_meta(desc: &[u8]) -> *const i64 {
    // A program spells a handful of boxed key layouts, so a list serves.
    static METAS: Mutex<Vec<BoxedMeta>> = Mutex::new(Vec::new());
    let mut metas = METAS.lock();
    if let Some((_, meta)) = metas.iter().find(|(key, _)| **key == *desc) {
        return meta.as_ptr();
    }
    use crate::c_abi::vec::vec_elem_kind;
    let children: Vec<[i64; 4]> = desc_slots(desc)
        .iter()
        .enumerate()
        .filter_map(|(word, kind)| {
            let child = match kind {
                b'S' => vec_elem_kind::STRING,
                b'V' => vec_elem_kind::VEC,
                b'B' => vec_elem_kind::RC_NODE,
                _ => return None,
            };
            Some([-1, 0, word as i64, i64::from(child)])
        })
        .collect();
    let mut meta = vec![
        gossamer_abi::rc::RC_KIND_SLOT_CHILDREN,
        children.len() as i64,
    ];
    meta.extend(children.into_iter().flatten());
    let meta: &'static [i64] = Box::leak(meta.into_boxed_slice());
    metas.push((desc.into(), meta));
    meta.as_ptr()
}

/// The word at `index` of a slot buffer, as a pointer.
fn word_at(slots: &[u8], index: usize) -> *mut u8 {
    let bytes = slots
        .get(index * 8..index * 8 + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .unwrap_or_default();
    // A slot is eight bytes wide on every target, while a pointer is the
    // target's own width: read the word, then narrow it.
    let word = u64::from_le_bytes(bytes);
    std::ptr::with_exposed_provenance_mut(usize::try_from(word).unwrap_or_default())
}

/// The counted words `slots` holds: the node of an enum value, or each
/// `String` and `Vec` field the descriptor names.
pub(crate) fn counted_words(
    slots: &[u8],
    desc: Option<&[u8]>,
    node_value: bool,
) -> Vec<(*mut u8, CountedWord)> {
    if node_value {
        let node = word_at(slots, 0);
        return if node.is_null() {
            Vec::new()
        } else {
            vec![(node, CountedWord::Rc)]
        };
    }
    let Some(desc) = desc else {
        return Vec::new();
    };
    desc_slots(desc)
        .iter()
        .enumerate()
        .filter_map(|(index, kind)| match kind {
            b'S' => Some((word_at(slots, index), CountedWord::Rc)),
            b'V' => Some((word_at(slots, index), CountedWord::Vec)),
            b'B' => Some((word_at(slots, index), CountedWord::Blob)),
            _ => None,
        })
        .filter(|(word, _)| !word.is_null())
        .collect()
}

/// Takes a share of every counted word `slots` holds.
///
/// # Safety
/// `slots` holds live words of the shapes `desc` names.
pub(crate) unsafe fn retain_slots(slots: &[u8], desc: Option<&[u8]>, node_value: bool) {
    for (word, kind) in counted_words(slots, desc, node_value) {
        match kind {
            // SAFETY: `word` is a counted node the slots hold live (this `unsafe fn`'s contract),
            // and the container takes a share of it.
            CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_retain(word) },
            // SAFETY: `word` is a counted `Vec` the slots hold live (this `unsafe fn`'s
            // contract), and the container takes a share of it.
            CountedWord::Vec => unsafe { crate::c_abi::gos_rt_vec_retain(word.cast()) },
            // SAFETY: `word` is a live payload word (this `unsafe fn`'s contract); a copy blob it
            // names gains the container's share.
            CountedWord::Blob => unsafe {
                if crate::c_abi::rc::copy_blob::is_copy_blob(word) {
                    crate::c_abi::rc::gos_rt_rc_retain(word);
                }
            },
        }
    }
}

/// Gives back a share of every counted word `slots` holds.
///
/// # Safety
/// As [`retain_slots`], for a share this container took.
pub(crate) unsafe fn release_slots(slots: &[u8], desc: Option<&[u8]>, node_value: bool) {
    for (word, kind) in counted_words(slots, desc, node_value) {
        match kind {
            // SAFETY: `word` is a counted node holding a share this container took (this `unsafe
            // fn`'s contract).
            CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_release(word) },
            // SAFETY: `word` is a counted `Vec` holding a share this container took (this `unsafe
            // fn`'s contract).
            CountedWord::Vec => unsafe { crate::c_abi::map::gos_rt_vec_free(word.cast()) },
            // SAFETY: `word` is a payload word whose copy blob holds a share this container took
            // (this `unsafe fn`'s contract).
            CountedWord::Blob => unsafe {
                if crate::c_abi::rc::copy_blob::is_copy_blob(word) {
                    crate::c_abi::rc::gos_rt_rc_release(word);
                }
            },
        }
    }
}

/// A key type's own `cmp`, as the compiled body the program declares it in.
///
/// An aggregate crosses the call boundary by the address of its slots and a
/// node by the word that names it, which is what `by_address` says.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UserCmp {
    address: usize,
    by_address: bool,
}

impl UserCmp {
    /// The comparator at `address`, or `None` for a null one.
    pub(crate) fn new(address: usize, by_address: bool) -> Option<Self> {
        (address != 0).then_some(Self {
            address,
            by_address,
        })
    }

    /// Orders two keys whose value is one word.
    ///
    /// # Safety
    /// As [`UserCmp::order`], for a key the comparator takes by value.
    pub(crate) unsafe fn order_word(self, a: i64, b: i64) -> Ordering {
        type ByWord = unsafe extern "C-unwind" fn(i64, i64) -> i64;
        // SAFETY: the address is the comparator the program compiled for this
        // key type, which takes a one-word key by value.
        let cmp: ByWord = unsafe { std::mem::transmute(crate::c_abi::code_address(self.address)) };
        // SAFETY: `cmp` is the program's comparator for this key type, taking its one-word keys
        // by value (this `unsafe fn`'s contract).
        unsafe { cmp(a, b) }.cmp(&0)
    }

    /// Orders two keys by their stored slots.
    ///
    /// # Safety
    /// The slots hold the key type the comparator was compiled for.
    pub(crate) unsafe fn order(self, a: &[u8], b: &[u8]) -> Ordering {
        type ByAddress = unsafe extern "C-unwind" fn(*const u8, *const u8) -> i64;
        type ByWord = unsafe extern "C-unwind" fn(i64, i64) -> i64;
        let verdict = if self.by_address {
            // SAFETY: the address is the comparator the program compiled for
            // this key type, which takes its two aggregates by address.
            let cmp: ByAddress =
                unsafe { std::mem::transmute(crate::c_abi::code_address(self.address)) };
            // SAFETY: `cmp` is the program's comparator for this key type, and `a` and `b` are
            // its keys' live slots.
            unsafe { cmp(a.as_ptr(), b.as_ptr()) }
        } else {
            let word = |slots: &[u8]| -> i64 {
                slots
                    .get(0..8)
                    .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                    .map_or(0, i64::from_le_bytes)
            };
            // SAFETY: as above, for a key whose value is one word.
            let cmp: ByWord =
                unsafe { std::mem::transmute(crate::c_abi::code_address(self.address)) };
            // SAFETY: `cmp` is the program's comparator for this key type, taking its one-word
            // keys by value.
            unsafe { cmp(word(a), word(b)) }
        };
        verdict.cmp(&0)
    }
}
