//! The counted words an aggregate's flat slots hold, and the comparator a
//! container reaches its key type's own `cmp` through.
//!
//! A container that keys by an aggregate's slots holds one share of every
//! `String`, `Vec`, and node those slots point at, and gives it back when the
//! entry dies. An ordered container whose key type writes its own `cmp` calls
//! that comparator on the same slots.

use std::cmp::Ordering;

/// How a counted word inside an aggregate's slots takes and gives back a
/// share.
#[derive(Clone, Copy)]
pub(crate) enum CountedWord {
    /// A `String` or a node: reference counted.
    Rc,
    /// A `Vec`: its own share count.
    Vec,
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
    desc.iter()
        .enumerate()
        .filter_map(|(index, kind)| match kind {
            b'S' => Some((word_at(slots, index), CountedWord::Rc)),
            b'V' => Some((word_at(slots, index), CountedWord::Vec)),
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
            CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_retain(word) },
            CountedWord::Vec => unsafe { crate::c_abi::gos_rt_vec_retain(word.cast()) },
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
            CountedWord::Rc => unsafe { crate::c_abi::rc::gos_rt_rc_release(word) },
            CountedWord::Vec => unsafe { crate::c_abi::map::gos_rt_vec_free(word.cast()) },
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
        type ByWord = unsafe extern "C" fn(i64, i64) -> i64;
        // SAFETY: the address is the comparator the program compiled for this
        // key type, which takes a one-word key by value.
        let cmp: ByWord = unsafe { std::mem::transmute(self.address) };
        unsafe { cmp(a, b) }.cmp(&0)
    }

    /// Orders two keys by their stored slots.
    ///
    /// # Safety
    /// The slots hold the key type the comparator was compiled for.
    pub(crate) unsafe fn order(self, a: &[u8], b: &[u8]) -> Ordering {
        type ByAddress = unsafe extern "C" fn(*const u8, *const u8) -> i64;
        type ByWord = unsafe extern "C" fn(i64, i64) -> i64;
        let verdict = if self.by_address {
            // SAFETY: the address is the comparator the program compiled for
            // this key type, which takes its two aggregates by address.
            let cmp: ByAddress = unsafe { std::mem::transmute(self.address) };
            unsafe { cmp(a.as_ptr(), b.as_ptr()) }
        } else {
            let word = |slots: &[u8]| -> i64 {
                slots
                    .get(0..8)
                    .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                    .map_or(0, i64::from_le_bytes)
            };
            // SAFETY: as above, for a key whose value is one word.
            let cmp: ByWord = unsafe { std::mem::transmute(self.address) };
            unsafe { cmp(word(a), word(b)) }
        };
        verdict.cmp(&0)
    }
}
