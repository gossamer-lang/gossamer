//! Advisory locks on an open `fs::File`, the same on every tier.
//!
//! A lock belongs to the `fs::File` that took it, as Rust's `File::lock` and
//! Go's `flock`-based lockers treat it: a second `fs::File` of the same path in
//! the same process conflicts with it as one in another process does, so two
//! goroutines exclude each other, and closing one `fs::File` leaves the locks
//! of the others in place.
//!
//! Whole-file locks are `flock` on Unix, which the system keeps per open file
//! description. Range locks are open-file-description record locks
//! (`F_OFD_SETLK`) on Linux and Android. Other Unix systems only have POSIX
//! record locks, which belong to the whole process and fall away when the
//! process closes any descriptor of the file, so there a process-wide table
//! refuses a range another `fs::File` of the process holds, and the descriptor
//! of a closed `fs::File` stays open until no range of that file is held. On
//! Windows `LockFileEx` already belongs to the handle.

use parking_lot::Mutex;

/// What a lock covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// The whole file, as `try_lock_shared` / `try_lock_exclusive` take it.
    Whole,
    /// `len` bytes from `start`; a zero `len` runs to the end of the file,
    /// however the file later grows.
    Range {
        /// The first byte covered.
        start: u64,
        /// How many bytes are covered; zero runs to the end of the file.
        len: u64,
    },
}

/// The locks one open file holds, so `unlock` can release each and a close
/// can release them before the descriptor goes.
#[derive(Debug, Default)]
pub struct HeldLocks {
    /// Each span this file holds, with whether it is held exclusively.
    spans: Mutex<Vec<(Span, bool)>>,
}

impl HeldLocks {
    /// Takes the lock over `span` without waiting. `Ok(false)` reports that
    /// another holder has a conflicting lock.
    ///
    /// # Errors
    /// The system refused the lock for a reason other than a conflict, or
    /// the target has no advisory locks.
    pub fn try_lock(
        &self,
        file: &std::fs::File,
        span: Span,
        exclusive: bool,
    ) -> std::io::Result<bool> {
        let mut spans = self.spans.lock();
        let owner = self.owner();
        let held = spans.iter().position(|(held, _)| *held == span);
        // Win32 stacks a second lock over a span instead of converting the
        // first, and refuses an exclusive one over the handle's own lock, so
        // a span this file holds is released before it is taken in a new
        // mode, as `flock` and record locks convert it on Unix.
        if cfg!(windows)
            && let Some(at) = held
        {
            if spans[at].1 == exclusive {
                return Ok(true);
            }
            release(owner, file, span)?;
            spans.swap_remove(at);
        }
        let acquired = match span {
            Span::Whole => sys::try_lock_whole(file, exclusive)?,
            Span::Range { start, len } => sys::try_lock_range(owner, file, start, len, exclusive)?,
        };
        if acquired {
            spans.retain(|(held, _)| *held != span);
            spans.push((span, exclusive));
        }
        Ok(acquired)
    }

    /// Releases the lock over `span`.
    ///
    /// # Errors
    /// The system refused the release, or the target has no advisory locks.
    pub fn unlock(&self, file: &std::fs::File, span: Span) -> std::io::Result<()> {
        let mut spans = self.spans.lock();
        release(self.owner(), file, span)?;
        spans.retain(|(held, _)| *held != span);
        Ok(())
    }

    /// Releases every lock this file holds.
    ///
    /// # Errors
    /// The first release the system refused; the rest are still attempted.
    pub fn unlock_all(&self, file: &std::fs::File) -> std::io::Result<()> {
        let held = std::mem::take(&mut *self.spans.lock());
        let owner = self.owner();
        let mut first_error = None;
        for (span, _) in held {
            if let Err(error) = release(owner, file, span) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Releases every lock this file holds and closes `file`, keeping the
    /// descriptor open where closing it would drop another file's locks.
    pub fn close(&self, file: std::fs::File) {
        let _ = self.unlock_all(&file);
        sys::close(self.owner(), file);
    }

    /// Identifies this open file among the process's open files while it
    /// lives: the address of its lock record, which moves with nothing.
    fn owner(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }
}

fn release(owner: usize, file: &std::fs::File, span: Span) -> std::io::Result<()> {
    match span {
        Span::Whole => sys::unlock_whole(file),
        Span::Range { start, len } => sys::unlock_range(owner, file, start, len),
    }
}

/// The end of a range, exclusive; a zero `len` runs to the end of the file.
#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
fn range_end(start: u64, len: u64) -> u64 {
    if len == 0 {
        u64::MAX
    } else {
        start.saturating_add(len)
    }
}

/// Interval arithmetic for the process-wide range table, over half-open
/// `[start, end)` byte ranges.
#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
mod spans {
    /// One range an open file of the process holds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Hold {
        pub(super) owner: usize,
        pub(super) start: u64,
        pub(super) end: u64,
        pub(super) exclusive: bool,
    }

    /// Whether another open file holds a range that a lock of `owner` over
    /// `[start, end)` would conflict with.
    pub(super) fn conflicts(
        holds: &[Hold],
        owner: usize,
        start: u64,
        end: u64,
        exclusive: bool,
    ) -> bool {
        holds.iter().any(|hold| {
            hold.owner != owner
                && hold.start < end
                && start < hold.end
                && (exclusive || hold.exclusive)
        })
    }

    /// Removes `[start, end)` from the ranges `owner` holds, splitting a hold
    /// that covers it on both sides, as the system's record locks do.
    pub(super) fn remove(holds: &mut Vec<Hold>, owner: usize, start: u64, end: u64) {
        let mut kept = Vec::with_capacity(holds.len() + 1);
        for hold in holds.drain(..) {
            if hold.owner != owner || hold.end <= start || end <= hold.start {
                kept.push(hold);
                continue;
            }
            if hold.start < start {
                kept.push(Hold { end: start, ..hold });
            }
            if end < hold.end {
                kept.push(Hold { start: end, ..hold });
            }
        }
        *holds = kept;
    }

    /// The parts of `[start, end)` no open file other than `owner` holds,
    /// in order: the parts whose system lock can go when `owner` releases.
    pub(super) fn uncovered(holds: &[Hold], owner: usize, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut others: Vec<(u64, u64)> = holds
            .iter()
            .filter(|hold| hold.owner != owner && hold.start < end && start < hold.end)
            .map(|hold| (hold.start.max(start), hold.end.min(end)))
            .collect();
        others.sort_unstable();
        let mut free = Vec::new();
        let mut at = start;
        for (from, to) in others {
            if at < from {
                free.push((at, from));
            }
            at = at.max(to);
        }
        if at < end {
            free.push((at, end));
        }
        free
    }
}

#[cfg(unix)]
mod sys {
    pub(super) fn try_lock_whole(file: &std::fs::File, exclusive: bool) -> std::io::Result<bool> {
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        flock(file, operation | libc::LOCK_NB)
    }

    pub(super) fn unlock_whole(file: &std::fs::File) -> std::io::Result<()> {
        flock(file, libc::LOCK_UN).map(|_| ())
    }

    fn flock(file: &std::fs::File, operation: libc::c_int) -> std::io::Result<bool> {
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
        loop {
            // SAFETY: `fd` is owned by the borrowed `File` for the whole call.
            if unsafe { libc::flock(fd, operation) } == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(code) if code == libc::EWOULDBLOCK => return Ok(false),
                _ => return Err(error),
            }
        }
    }

    /// One `fcntl` record-lock call over `len` bytes from `start` (zero runs
    /// to the end of the file). `Ok(false)` is a conflicting holder. `kind`
    /// is `F_RDLCK`, `F_WRLCK`, or `F_UNLCK`, which some systems declare as
    /// `c_short` and others as `c_int`.
    pub(super) fn record_lock(
        file: &std::fs::File,
        command: libc::c_int,
        kind: impl Into<libc::c_int>,
        start: u64,
        len: u64,
    ) -> std::io::Result<bool> {
        let (Ok(start), Ok(len)) = (libc::off_t::try_from(start), libc::off_t::try_from(len))
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lock range lies past the largest file offset",
            ));
        };
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
        // SAFETY: `flock` is a plain C struct whose all-zero value is valid,
        // and a zero `l_pid` is what open-file-description locks require.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = kind.into() as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        lock.l_start = start;
        lock.l_len = len;
        loop {
            // SAFETY: `lock` is a fully initialised `flock` and `fd` is owned
            // by the borrowed `File` for the duration of the call.
            if unsafe { libc::fcntl(fd, command, &raw const lock) } == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::EACCES | libc::EAGAIN) => return Ok(false),
                _ => return Err(error),
            }
        }
    }

    pub(super) use super::sys_ranges::{close, try_lock_range, unlock_range};
}

/// Open-file-description record locks: the system keeps each with the open
/// file that took it, so nothing in the process needs tracking.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys_ranges {
    use super::sys::record_lock;

    pub(super) fn try_lock_range(
        _owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
        exclusive: bool,
    ) -> std::io::Result<bool> {
        let kind = if exclusive {
            libc::F_WRLCK
        } else {
            libc::F_RDLCK
        };
        record_lock(file, libc::F_OFD_SETLK, kind, start, len)
    }

    pub(super) fn unlock_range(
        _owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
    ) -> std::io::Result<()> {
        record_lock(file, libc::F_OFD_SETLK, libc::F_UNLCK, start, len).map(|_| ())
    }

    pub(super) fn close(_owner: usize, file: std::fs::File) {
        drop(file);
    }
}

/// POSIX record locks belong to the process, so the table below holds which
/// open file holds which range: a conflicting range of another open file is
/// refused here, a release gives back only the bytes no other open file
/// still holds, and a closed file's descriptor waits in `parked` until its
/// file has no held range, since closing it would drop them all.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
mod sys_ranges {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;

    use parking_lot::Mutex;

    use super::range_end;
    use super::spans::{self, Hold};
    use super::sys::record_lock;

    #[derive(Default)]
    struct FileRanges {
        holds: Vec<Hold>,
        parked: Vec<std::fs::File>,
    }

    /// Keyed by device and inode, which name the file whatever path opened it.
    static RANGES: Mutex<Option<HashMap<(u64, u64), FileRanges>>> = Mutex::new(None);

    fn file_key(file: &std::fs::File) -> std::io::Result<(u64, u64)> {
        let meta = file.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }

    pub(super) fn try_lock_range(
        owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
        exclusive: bool,
    ) -> std::io::Result<bool> {
        let key = file_key(file)?;
        let end = range_end(start, len);
        let mut table = RANGES.lock();
        let ranges = table
            .get_or_insert_with(HashMap::new)
            .entry(key)
            .or_default();
        if spans::conflicts(&ranges.holds, owner, start, end, exclusive) {
            return Ok(false);
        }
        let kind = if exclusive {
            libc::F_WRLCK
        } else {
            libc::F_RDLCK
        };
        if !record_lock(file, libc::F_SETLK, kind, start, len)? {
            return Ok(false);
        }
        spans::remove(&mut ranges.holds, owner, start, end);
        ranges.holds.push(Hold {
            owner,
            start,
            end,
            exclusive,
        });
        Ok(true)
    }

    pub(super) fn unlock_range(
        owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
    ) -> std::io::Result<()> {
        let key = file_key(file)?;
        let end = range_end(start, len);
        let mut table = RANGES.lock();
        let Some(ranges) = table.as_mut().and_then(|t| t.get_mut(&key)) else {
            return record_lock(file, libc::F_SETLK, libc::F_UNLCK, start, len).map(|_| ());
        };
        for (from, to) in spans::uncovered(&ranges.holds, owner, start, end) {
            let span = if to == u64::MAX { 0 } else { to - from };
            record_lock(file, libc::F_SETLK, libc::F_UNLCK, from, span)?;
        }
        spans::remove(&mut ranges.holds, owner, start, end);
        if ranges.holds.is_empty()
            && let Some(done) = table.as_mut().and_then(|t| t.remove(&key))
        {
            drop(table);
            drop(done.parked);
        }
        Ok(())
    }

    pub(super) fn close(owner: usize, file: std::fs::File) {
        let Ok(key) = file_key(&file) else {
            return;
        };
        let mut table = RANGES.lock();
        if let Some(ranges) = table.as_mut().and_then(|t| t.get_mut(&key))
            && ranges.holds.iter().any(|hold| hold.owner != owner)
        {
            ranges.parked.push(file);
            return;
        }
        drop(table);
        drop(file);
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    pub(super) fn try_lock_whole(file: &std::fs::File, exclusive: bool) -> std::io::Result<bool> {
        try_lock_range(0, file, 0, 0, exclusive)
    }

    pub(super) fn unlock_whole(file: &std::fs::File) -> std::io::Result<()> {
        unlock_range(0, file, 0, 0)
    }

    fn overlapped_at(start: u64) -> OVERLAPPED {
        // SAFETY: `OVERLAPPED` is a plain C struct whose all-zero value is valid.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = start as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (start >> 32) as u32;
        overlapped
    }

    /// `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY`, so a held lock answers
    /// `Ok(false)` instead of blocking the scheduler thread. A zero `len`
    /// covers the whole 64-bit range, the POSIX "to end of file".
    pub(super) fn try_lock_range(
        _owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
        exclusive: bool,
    ) -> std::io::Result<bool> {
        let handle = std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE;
        let span = if len == 0 { u64::MAX } else { len };
        let mut overlapped = overlapped_at(start);
        let mut flags = LOCKFILE_FAIL_IMMEDIATELY;
        if exclusive {
            flags |= LOCKFILE_EXCLUSIVE_LOCK;
        }
        // SAFETY: `handle` is owned by the borrowed `File` and `overlapped` is
        // a fully initialised structure that outlives the call.
        let ok = unsafe {
            LockFileEx(
                handle,
                flags,
                0,
                span as u32,
                (span >> 32) as u32,
                &raw mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    /// Win32 releases a lock only through an `UnlockFileEx` naming the span
    /// `LockFileEx` took, which the caller's held list supplies.
    pub(super) fn unlock_range(
        _owner: usize,
        file: &std::fs::File,
        start: u64,
        len: u64,
    ) -> std::io::Result<()> {
        let handle = std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE;
        let span = if len == 0 { u64::MAX } else { len };
        let mut overlapped = overlapped_at(start);
        // SAFETY: as in `try_lock_range`.
        let ok = unsafe {
            UnlockFileEx(
                handle,
                0,
                span as u32,
                (span >> 32) as u32,
                &raw mut overlapped,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn close(_owner: usize, file: std::fs::File) {
        drop(file);
    }
}

/// A target with neither the POSIX nor the Win32 calls has no advisory locks.
/// Answering `Ok(false)` would claim another holder owns the range, so the
/// absence is reported instead.
#[cfg(not(any(unix, windows)))]
mod sys {
    fn unsupported() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "advisory file locks are unavailable on this target",
        )
    }

    pub(super) fn try_lock_whole(_file: &std::fs::File, _exclusive: bool) -> std::io::Result<bool> {
        Err(unsupported())
    }

    pub(super) fn unlock_whole(_file: &std::fs::File) -> std::io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn try_lock_range(
        _owner: usize,
        _file: &std::fs::File,
        _start: u64,
        _len: u64,
        _exclusive: bool,
    ) -> std::io::Result<bool> {
        Err(unsupported())
    }

    pub(super) fn unlock_range(
        _owner: usize,
        _file: &std::fs::File,
        _start: u64,
        _len: u64,
    ) -> std::io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn close(_owner: usize, file: std::fs::File) {
        drop(file);
    }
}

#[cfg(test)]
mod tests {
    use super::spans::{Hold, conflicts, remove, uncovered};
    use super::*;

    fn hold(owner: usize, start: u64, end: u64, exclusive: bool) -> Hold {
        Hold {
            owner,
            start,
            end,
            exclusive,
        }
    }

    #[test]
    fn ranges_conflict_only_across_open_files_and_only_with_an_exclusive_side() {
        let holds = [hold(1, 0, 10, false), hold(2, 20, 30, true)];
        assert!(!conflicts(&holds, 3, 0, 10, false));
        assert!(conflicts(&holds, 3, 5, 15, true));
        assert!(conflicts(&holds, 3, 25, 26, false));
        assert!(!conflicts(&holds, 2, 25, 26, false));
        assert!(!conflicts(&holds, 3, 10, 20, true));
        assert!(conflicts(&holds, 3, 0, range_end(0, 0), true));
    }

    #[test]
    fn removing_a_middle_range_splits_the_hold_around_it() {
        let mut holds = vec![hold(1, 0, 100, true), hold(2, 0, 100, false)];
        remove(&mut holds, 1, 40, 60);
        assert_eq!(
            holds,
            vec![
                hold(1, 0, 40, true),
                hold(1, 60, 100, true),
                hold(2, 0, 100, false)
            ]
        );
    }

    #[test]
    fn a_release_gives_back_only_bytes_no_other_open_file_holds() {
        let holds = [
            hold(1, 0, 100, false),
            hold(2, 10, 20, false),
            hold(3, 15, 40, false),
        ];
        assert_eq!(uncovered(&holds, 1, 0, 100), vec![(0, 10), (40, 100)]);
        assert_eq!(uncovered(&holds, 2, 10, 20), Vec::<(u64, u64)>::new());
    }

    fn scratch_file(name: &str) -> (std::path::PathBuf, std::fs::File, std::fs::File) {
        let path =
            std::env::temp_dir().join(format!("gossamer-file-lock-{name}-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        (path.clone(), open(), open())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn two_open_files_of_one_path_in_one_process_exclude_each_other() {
        let (path, a, b) = scratch_file("whole");
        let (held_a, held_b) = (HeldLocks::default(), HeldLocks::default());
        assert!(held_a.try_lock(&a, Span::Whole, true).unwrap());
        assert!(!held_b.try_lock(&b, Span::Whole, true).unwrap());
        assert!(!held_b.try_lock(&b, Span::Whole, false).unwrap());
        held_a.unlock_all(&a).unwrap();
        assert!(held_b.try_lock(&b, Span::Whole, true).unwrap());
        held_b.close(b);
        held_a.close(a);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn ranges_exclude_another_open_file_and_survive_a_third_closing() {
        let (path, a, b) = scratch_file("range");
        let (held_a, held_b) = (HeldLocks::default(), HeldLocks::default());
        let first = Span::Range { start: 0, len: 4 };
        assert!(held_a.try_lock(&a, first, true).unwrap());
        assert!(
            !held_b
                .try_lock(&b, Span::Range { start: 2, len: 4 }, false)
                .unwrap()
        );
        assert!(
            held_b
                .try_lock(&b, Span::Range { start: 4, len: 4 }, true)
                .unwrap()
        );
        let third = HeldLocks::default();
        third.close(std::fs::File::open(&path).unwrap());
        assert!(
            !held_b
                .try_lock(&b, Span::Range { start: 0, len: 1 }, false)
                .unwrap()
        );
        held_a.unlock(&a, first).unwrap();
        assert!(
            held_b
                .try_lock(&b, Span::Range { start: 0, len: 1 }, false)
                .unwrap()
        );
        held_a.close(a);
        held_b.close(b);
        std::fs::remove_file(path).unwrap();
    }
}
