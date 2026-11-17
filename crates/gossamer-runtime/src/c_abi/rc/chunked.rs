use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Words per chunk of a [`ChunkedWords`] table.
const CHUNK_WORDS: usize = 256;
/// Chunks a table addresses: one word for every `u16`.
const CHUNKS: usize = 256;

/// The words a [`ChunkedWords`] table holds.
pub(super) const CHUNKED_WORDS: usize = CHUNK_WORDS * CHUNKS;

type Chunk = [AtomicUsize; CHUNK_WORDS];

/// The chunk every table entry names until a word in its range is stored.
/// Never written, so a word no one stored reads 0.
static EMPTY_CHUNK: Chunk = [const { AtomicUsize::new(0) }; CHUNK_WORDS];

/// A table of [`CHUNKED_WORDS`] atomic words, all 0 at first, whose storage
/// is allocated a chunk at a time as words are first stored. A table that
/// sees a handful of indices costs one chunk instead of half a megabyte of
/// `.bss`, which Windows and strict-overcommit hosts charge whether or not it
/// is touched. Every entry names a chunk, so a load is two loads and no
/// branch.
pub(super) struct ChunkedWords {
    chunks: [AtomicPtr<Chunk>; CHUNKS],
}

impl ChunkedWords {
    pub(super) const fn new() -> Self {
        Self {
            chunks: [const { AtomicPtr::new((&raw const EMPTY_CHUNK).cast_mut()) }; CHUNKS],
        }
    }

    /// The word at `index`, which must be below [`CHUNKED_WORDS`].
    #[inline]
    pub(super) fn load(&self, index: usize, order: Ordering) -> usize {
        let chunk = self.chunks[index / CHUNK_WORDS].load(Ordering::Acquire);
        // SAFETY: every entry names `EMPTY_CHUNK` or a chunk `store` published,
        // which lives until the table drops.
        unsafe { (*chunk)[index % CHUNK_WORDS].load(order) }
    }

    /// Stores `value` at `index`, which must be below [`CHUNKED_WORDS`],
    /// allocating the chunk that holds it on first use.
    pub(super) fn store(&self, index: usize, value: usize, order: Ordering) {
        let entry = &self.chunks[index / CHUNK_WORDS];
        let mut chunk = entry.load(Ordering::Acquire);
        if std::ptr::eq(chunk, &raw const EMPTY_CHUNK) {
            let fresh = Box::into_raw(Box::new([const { AtomicUsize::new(0) }; CHUNK_WORDS]));
            chunk = match entry.compare_exchange(chunk, fresh, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => fresh,
                Err(published) => {
                    // SAFETY: `fresh` came from `Box::into_raw` above and no
                    // one else saw it, since the exchange failed.
                    drop(unsafe { Box::from_raw(fresh) });
                    published
                }
            };
        }
        // SAFETY: `chunk` is a chunk this table published, which lives until
        // the table drops.
        unsafe { (*chunk)[index % CHUNK_WORDS].store(value, order) };
    }
}

impl Drop for ChunkedWords {
    fn drop(&mut self) {
        for entry in &mut self.chunks {
            let chunk = *entry.get_mut();
            if !std::ptr::eq(chunk, &raw const EMPTY_CHUNK) {
                // SAFETY: a chunk other than the shared empty one came from
                // `Box::into_raw` in `store`, and the table is its only owner.
                drop(unsafe { Box::from_raw(chunk) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstored_words_read_zero_and_stored_words_read_back() {
        let table = ChunkedWords::new();
        assert_eq!(table.load(0, Ordering::Relaxed), 0);
        assert_eq!(table.load(CHUNKED_WORDS - 1, Ordering::Relaxed), 0);
        table.store(3, 30, Ordering::Relaxed);
        table.store(CHUNKED_WORDS - 1, 99, Ordering::Relaxed);
        assert_eq!(table.load(3, Ordering::Relaxed), 30);
        assert_eq!(table.load(CHUNKED_WORDS - 1, Ordering::Relaxed), 99);
        assert_eq!(table.load(4, Ordering::Relaxed), 0);
        assert_eq!(
            EMPTY_CHUNK[3].load(Ordering::Relaxed),
            0,
            "the shared chunk stays empty"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "thread contention is covered natively")]
    fn concurrent_first_stores_into_one_chunk_all_land() {
        let table = std::sync::Arc::new(ChunkedWords::new());
        let writers: Vec<_> = (0..8)
            .map(|t| {
                let table = table.clone();
                std::thread::spawn(move || {
                    for i in 0..32 {
                        table.store(t * 32 + i, t * 32 + i + 1, Ordering::Release);
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        for i in 0..256 {
            assert_eq!(table.load(i, Ordering::Acquire), i + 1);
        }
    }
}
