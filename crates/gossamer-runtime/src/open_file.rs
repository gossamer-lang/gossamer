//! An open file as `fs::File` holds it on every tier, with the write buffer
//! a `bufio::Writer` turns on.
//!
//! Buffered bytes reach the file before any other call on it reads or moves
//! the cursor, when the last handle to the file goes, and when the program
//! ends, so a write the program made is never dropped unwritten.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

/// An open file and the bytes written to it that have not reached it yet.
#[derive(Debug)]
pub struct OpenFile {
    file: std::fs::File,
    /// Held by every call that moves or depends on the cursor.
    cursor: Mutex<Pending>,
    /// Whether `cursor` holds bytes, so positional calls skip its lock
    /// when nothing is buffered.
    pending: AtomicBool,
}

#[derive(Debug, Default)]
struct Pending {
    bytes: Vec<u8>,
    /// Zero writes straight through.
    capacity: usize,
}

impl Pending {
    fn write_out(&mut self, file: &std::fs::File) -> std::io::Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let mut file = file;
        let result = file.write_all(&self.bytes);
        self.bytes.clear();
        result
    }
}

/// Every file a `bufio::Writer` buffers, written out when the program ends.
static BUFFERED: Mutex<Vec<Weak<OpenFile>>> = Mutex::new(Vec::new());

impl OpenFile {
    /// Wraps `file`, writing straight through until [`Self::buffer_writes`].
    #[must_use]
    pub fn new(file: std::fs::File) -> Arc<Self> {
        Arc::new(Self {
            file,
            cursor: Mutex::new(Pending::default()),
            pending: AtomicBool::new(false),
        })
    }

    /// The file itself, for calls that read only its metadata or descriptor.
    #[must_use]
    pub fn file(&self) -> &std::fs::File {
        &self.file
    }

    /// Runs `op` holding the cursor lock, once the buffered bytes are written.
    ///
    /// # Errors
    /// The error writing out the buffered bytes; `op` does not run then.
    pub fn with_cursor<T>(&self, op: impl FnOnce(&mut &std::fs::File) -> T) -> std::io::Result<T> {
        let mut pending = self.cursor.lock();
        self.drain(&mut pending)?;
        Ok(op(&mut &self.file))
    }

    /// Runs `op`, which reads or writes at an offset, once the buffered
    /// bytes are written. Only Windows moves the cursor for such a call, so
    /// only there does it hold the cursor lock while it runs.
    ///
    /// # Errors
    /// The error writing out the buffered bytes; `op` does not run then.
    pub fn positional<T>(&self, op: impl FnOnce(&mut &std::fs::File) -> T) -> std::io::Result<T> {
        if cfg!(windows) || self.pending.load(Ordering::Acquire) {
            return self.with_cursor(op);
        }
        Ok(op(&mut &self.file))
    }

    /// Buffers later writes up to `capacity` bytes; zero writes through.
    ///
    /// # Errors
    /// The error writing out bytes a smaller capacity no longer holds.
    pub fn buffer_writes(self: &Arc<Self>, capacity: usize) -> std::io::Result<()> {
        let mut pending = self.cursor.lock();
        if pending.bytes.len() > capacity {
            self.drain(&mut pending)?;
        }
        let first = pending.capacity == 0 && capacity > 0;
        pending.capacity = capacity;
        drop(pending);
        if first {
            let mut buffered = BUFFERED.lock();
            buffered.retain(|file| file.strong_count() > 0);
            buffered.push(Arc::downgrade(self));
        }
        Ok(())
    }

    /// Writes all of `bytes`, into the buffer when one is on and they fit.
    ///
    /// # Errors
    /// The error the write, or writing out the buffer, met.
    pub fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut pending = self.cursor.lock();
        if pending.capacity == 0 {
            return (&self.file).write_all(bytes);
        }
        if pending.bytes.len() + bytes.len() > pending.capacity {
            self.drain(&mut pending)?;
        }
        if bytes.len() >= pending.capacity {
            return (&self.file).write_all(bytes);
        }
        pending.bytes.extend_from_slice(bytes);
        self.pending.store(true, Ordering::Release);
        Ok(())
    }

    /// One write of `bytes`, answering how many were taken: all of them
    /// when a buffer is on, else what the one system call moved.
    ///
    /// # Errors
    /// The error the write met.
    pub fn write_some(&self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cursor.lock().capacity > 0 {
            self.write_all(bytes)?;
            return Ok(bytes.len());
        }
        self.with_cursor(|file| file.write(bytes))?
    }

    /// Writes out the buffered bytes, then flushes the file.
    ///
    /// # Errors
    /// The error either step met.
    pub fn flush(&self) -> std::io::Result<()> {
        self.with_cursor(|file| file.flush())?
    }

    fn drain(&self, pending: &mut Pending) -> std::io::Result<()> {
        let result = pending.write_out(&self.file);
        self.pending.store(false, Ordering::Release);
        result
    }

    /// Writes out what is buffered, reporting a failure on stderr: the
    /// handle that could have answered it is gone.
    fn write_out_last(&self) {
        let mut pending = self.cursor.lock();
        if let Err(err) = self.drain(&mut pending) {
            eprintln!("bufio::Writer: buffered bytes could not be written: {err}");
        }
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        self.write_out_last();
    }
}

/// Held by the tests that write out every buffered file, and by those that
/// watch one file's writes land, which the former would otherwise reach.
#[cfg(test)]
pub(crate) static BUFFERED_FILES_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Writes out every buffered file still open. Runs as the program ends.
pub fn write_out_buffered() {
    let files = std::mem::take(&mut *BUFFERED.lock());
    for file in files.iter().filter_map(Weak::upgrade) {
        file.write_out_last();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (std::path::PathBuf, Arc<OpenFile>) {
        let path =
            std::env::temp_dir().join(format!("gos-open-file-{name}-{}", std::process::id()));
        let file = std::fs::File::create(&path).expect("create scratch file");
        (path, OpenFile::new(file))
    }

    #[test]
    fn buffered_writes_reach_the_file_on_flush_and_on_drop() {
        let _serial = BUFFERED_FILES_TEST_LOCK.lock();
        let (path, file) = scratch("flush");
        file.buffer_writes(64).expect("buffer");
        file.write_all(b"one ").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"");
        file.flush().expect("flush");
        assert_eq!(std::fs::read(&path).expect("read"), b"one ");
        file.write_all(b"two").expect("write");
        drop(file);
        assert_eq!(std::fs::read(&path).expect("read"), b"one two");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_cursor_call_sees_every_buffered_write_first() {
        let _serial = BUFFERED_FILES_TEST_LOCK.lock();
        let (path, file) = scratch("cursor");
        file.buffer_writes(64).expect("buffer");
        file.write_all(b"abc").expect("write");
        let len = file
            .with_cursor(|f| f.metadata().map(|m| m.len()))
            .expect("drain");
        assert_eq!(len.expect("metadata"), 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_write_past_the_capacity_goes_straight_through() {
        let _serial = BUFFERED_FILES_TEST_LOCK.lock();
        let (path, file) = scratch("large");
        file.buffer_writes(4).expect("buffer");
        file.write_all(b"ab").expect("write");
        file.write_all(b"cdefgh").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"abcdefgh");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_program_end_writes_out_files_still_open() {
        let _serial = BUFFERED_FILES_TEST_LOCK.lock();
        let (path, file) = scratch("exit");
        file.buffer_writes(64).expect("buffer");
        file.write_all(b"kept").expect("write");
        write_out_buffered();
        assert_eq!(std::fs::read(&path).expect("read"), b"kept");
        let _ = std::fs::remove_file(path);
    }
}
