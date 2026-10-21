//! Storage of a `static mut` that the bytecode VM and every JIT artifact of a
//! program share: compiled code addresses its words directly, and the VM
//! reads and writes a scalar static through the same words.

use std::sync::atomic::{AtomicU64, Ordering};

/// How a scalar static lays out its value in its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticClass {
    /// Any integer, held as its canonical 64-bit word.
    Int,
    /// Any float, held as the bits of an `f64`.
    Float,
    /// One byte, 0 or 1.
    Bool,
    /// A Unicode scalar value in 32 bits.
    Char,
}

/// The words backing one static, at a stable address for the program's life.
#[derive(Debug)]
pub struct StaticStorage {
    words: Box<[AtomicU64]>,
}

impl StaticStorage {
    /// Storage of `words` 8-byte words, at least two, starting with `init`.
    #[must_use]
    pub fn new(words: usize, init: &[u8]) -> Self {
        let words: Box<[AtomicU64]> = (0..words.max(2).max(init.len().div_ceil(8)))
            .map(|i| {
                let mut word = [0u8; 8];
                let start = (i * 8).min(init.len());
                let end = (start + 8).min(init.len());
                word[..end - start].copy_from_slice(&init[start..end]);
                AtomicU64::new(u64::from_le_bytes(word))
            })
            .collect();
        Self { words }
    }

    /// Storage for a scalar static whose value is `bits`.
    #[must_use]
    pub fn scalar(class: StaticClass, bits: u64) -> Self {
        let storage = Self::new(2, &[]);
        storage.store(class, bits);
        storage
    }

    /// The address compiled code reads and writes.
    #[must_use]
    pub fn addr(&self) -> *const u8 {
        self.words.as_ptr().cast()
    }

    /// The value's bits, zero-extended to 64.
    #[must_use]
    pub fn load(&self, class: StaticClass) -> u64 {
        let (shift, mask) = Self::lane(class);
        (self.words[0].load(Ordering::Relaxed) >> shift) & mask
    }

    /// Writes `bits`, truncated to the class's width. Compiled code stores a
    /// narrow class at the word's address only, so the rest of the word is
    /// kept as it is.
    pub fn store(&self, class: StaticClass, bits: u64) {
        let (shift, mask) = Self::lane(class);
        let bits = match class {
            StaticClass::Bool => u64::from(bits != 0),
            _ => bits & mask,
        };
        let _ = self.words[0].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |word| {
            Some((word & !(mask << shift)) | (bits << shift))
        });
    }

    /// Where a class's bytes sit in the first word: the bytes at its address,
    /// which are the low bytes on a little-endian target and the high ones on
    /// a big-endian target.
    fn lane(class: StaticClass) -> (u32, u64) {
        let width = match class {
            StaticClass::Int | StaticClass::Float => return (0, u64::MAX),
            StaticClass::Bool => 8,
            StaticClass::Char => 32,
        };
        let shift = if cfg!(target_endian = "big") {
            64 - width
        } else {
            0
        };
        (shift, (1u64 << width) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{StaticClass, StaticStorage};

    #[test]
    fn scalar_storage_reads_back_each_class_at_its_width() {
        for (class, bits, expected) in [
            (StaticClass::Int, u64::MAX, u64::MAX),
            (StaticClass::Float, 2.5f64.to_bits(), 2.5f64.to_bits()),
            (StaticClass::Bool, 7, 1),
            (
                StaticClass::Char,
                u64::from(u32::from('z')) | (1 << 40),
                u64::from(u32::from('z')),
            ),
        ] {
            assert_eq!(StaticStorage::scalar(class, bits).load(class), expected);
        }
    }

    #[test]
    fn storage_starts_with_its_initial_bytes() {
        let storage = StaticStorage::new(1, &42i64.to_le_bytes());
        assert_eq!(storage.load(StaticClass::Int), 42);
    }
}
