//! `memcpy`, `memmove`, `memset`, `memcmp`, and `bcmp` for the static-musl
//! runtime on `x86_64`.
//!
//! musl's routines start every copy with `rep movsq` (and every fill with
//! `rep stosq`), whose microcode startup dominates the short copies a
//! program makes most: a string append, a struct move, a small key. Here a
//! length up to 64 bytes is a handful of unaligned loads and stores, and a
//! longer one is `rep movsb` / `rep stosb`, which the fast-string microcode
//! of every current `x86_64` core runs at full width. musl compares one byte
//! per step; here sixteen bytes compare per SSE2 instruction.
//!
//! The copy and fill bodies hold no loops, and the compare loop is made of
//! vector intrinsics, so the optimiser has nothing to turn back into a call
//! to the routine being defined.

use std::ptr::{read_unaligned, write_unaligned};

/// Copies `n <= 64` bytes. Every load happens before the first store, so the
/// two ranges may overlap in either direction.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes.
#[inline]
unsafe fn copy_upto_64(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: every access lies in `[0, n)` of the ranges the caller vouches
    // for, and the unaligned forms carry no alignment requirement.
    unsafe {
        if n >= 32 {
            let head = read_unaligned(src.cast::<u128>());
            let second = read_unaligned(src.add(16).cast::<u128>());
            let third = read_unaligned(src.add(n - 32).cast::<u128>());
            let tail = read_unaligned(src.add(n - 16).cast::<u128>());
            write_unaligned(dst.cast::<u128>(), head);
            write_unaligned(dst.add(16).cast::<u128>(), second);
            write_unaligned(dst.add(n - 32).cast::<u128>(), third);
            write_unaligned(dst.add(n - 16).cast::<u128>(), tail);
        } else if n >= 16 {
            let head = read_unaligned(src.cast::<u128>());
            let second = read_unaligned(src.add(n - 16).cast::<u128>());
            write_unaligned(dst.cast::<u128>(), head);
            write_unaligned(dst.add(n - 16).cast::<u128>(), second);
        } else if n >= 8 {
            let head = read_unaligned(src.cast::<u64>());
            let second = read_unaligned(src.add(n - 8).cast::<u64>());
            write_unaligned(dst.cast::<u64>(), head);
            write_unaligned(dst.add(n - 8).cast::<u64>(), second);
        } else if n >= 4 {
            let head = read_unaligned(src.cast::<u32>());
            let second = read_unaligned(src.add(n - 4).cast::<u32>());
            write_unaligned(dst.cast::<u32>(), head);
            write_unaligned(dst.add(n - 4).cast::<u32>(), second);
        } else if n >= 2 {
            let head = read_unaligned(src.cast::<u16>());
            let second = read_unaligned(src.add(n - 2).cast::<u16>());
            write_unaligned(dst.cast::<u16>(), head);
            write_unaligned(dst.add(n - 2).cast::<u16>(), second);
        } else if n == 1 {
            *dst = *src;
        }
    }
}

/// Copies `n` bytes front to back with `rep movsb`.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes, and `dst` does not
/// start inside `(src, src + n)`.
#[inline]
unsafe fn rep_movsb_forward(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: the caller guarantees both ranges; the direction flag is clear
    // on entry to every function under the System V ABI.
    unsafe {
        std::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rdi") dst => _,
            inout("rsi") src => _,
            options(nostack, preserves_flags)
        );
    }
}

/// Copies `n` bytes back to front with `rep movsb` under the direction flag.
///
/// # Safety
/// `src` is readable and `dst` writable for `n >= 1` bytes.
#[inline]
unsafe fn rep_movsb_backward(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: the caller guarantees both ranges; the direction flag is set for
    // the copy alone and cleared again, as the ABI requires at every call.
    unsafe {
        std::arch::asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rcx") n => _,
            inout("rdi") dst.add(n - 1) => _,
            inout("rsi") src.add(n - 1) => _,
            options(nostack)
        );
    }
}

/// `memcpy` over non-overlapping ranges.
///
/// # Safety
/// As C's `memcpy`.
#[inline]
pub(crate) unsafe fn copy(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: `memcpy`'s contract - disjoint ranges of `n` bytes - is each
    // helper's precondition.
    unsafe {
        if n <= 64 {
            copy_upto_64(dst, src, n);
        } else {
            rep_movsb_forward(dst, src, n);
        }
    }
}

/// `memmove` over ranges that may overlap.
///
/// # Safety
/// As C's `memmove`.
#[inline]
pub(crate) unsafe fn copy_overlapping(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: both ranges hold `n` bytes; the direction chosen below never reads
    // a byte after writing over it.
    unsafe {
        if n <= 64 {
            copy_upto_64(dst, src, n);
        } else if (dst as usize).wrapping_sub(src as usize) >= n {
            // `dst` sits below `src` or past its end, so a forward copy never
            // reads a byte it has already overwritten.
            rep_movsb_forward(dst, src, n);
        } else {
            rep_movsb_backward(dst, src, n);
        }
    }
}

/// `memset` of `n` bytes to `byte`.
///
/// # Safety
/// As C's `memset`.
#[inline]
pub(crate) unsafe fn fill(dst: *mut u8, byte: u8, n: usize) {
    let v8 = u64::from(byte) * 0x0101_0101_0101_0101;
    let v16 = u128::from(v8) | (u128::from(v8) << 64);
    // SAFETY: every store lies in `[0, n)` of the range the caller vouches for,
    // and `rep stosb` runs with the direction flag clear, as the ABI leaves it.
    unsafe {
        if n > 64 {
            std::arch::asm!(
                "rep stosb",
                inout("rcx") n => _,
                inout("rdi") dst => _,
                in("al") byte,
                options(nostack, preserves_flags)
            );
        } else if n >= 32 {
            write_unaligned(dst.cast::<u128>(), v16);
            write_unaligned(dst.add(16).cast::<u128>(), v16);
            write_unaligned(dst.add(n - 32).cast::<u128>(), v16);
            write_unaligned(dst.add(n - 16).cast::<u128>(), v16);
        } else if n >= 16 {
            write_unaligned(dst.cast::<u128>(), v16);
            write_unaligned(dst.add(n - 16).cast::<u128>(), v16);
        } else if n >= 8 {
            write_unaligned(dst.cast::<u64>(), v8);
            write_unaligned(dst.add(n - 8).cast::<u64>(), v8);
        } else if n >= 4 {
            write_unaligned(dst.cast::<u32>(), v8 as u32);
            write_unaligned(dst.add(n - 4).cast::<u32>(), v8 as u32);
        } else if n >= 2 {
            write_unaligned(dst.cast::<u16>(), v8 as u16);
            write_unaligned(dst.add(n - 2).cast::<u16>(), v8 as u16);
        } else if n == 1 {
            *dst = byte;
        }
    }
}

/// The sign of the first differing byte, as C's `memcmp` answers it.
#[inline]
fn byte_order(a: u8, b: u8) -> i32 {
    i32::from(a) - i32::from(b)
}

/// The order of two equal-width big-endian loads: the first differing byte
/// decides it, as in `memcmp`.
#[inline]
fn word_order(a: u64, b: u64) -> i32 {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// The index of the first differing byte of the sixteen at `a` and `b`, if
/// any differ.
///
/// # Safety
/// `a` and `b` are readable for 16 bytes.
#[inline]
unsafe fn first_difference_16(a: *const u8, b: *const u8) -> Option<usize> {
    use std::arch::x86_64::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
    // SAFETY: SSE2 is part of the `x86_64` baseline, and both loads lie in the
    // sixteen bytes the caller vouches for.
    let equal = unsafe {
        _mm_movemask_epi8(_mm_cmpeq_epi8(
            _mm_loadu_si128(a.cast()),
            _mm_loadu_si128(b.cast()),
        ))
    } as u32;
    let differing = !equal & 0xFFFF;
    (differing != 0).then(|| differing.trailing_zeros() as usize)
}

/// `memcmp` of `n` bytes.
///
/// # Safety
/// As C's `memcmp`.
#[inline]
pub(crate) unsafe fn compare(lhs: *const u8, rhs: *const u8, len: usize) -> i32 {
    // SAFETY: every access lies in `[0, len)` of the ranges the caller vouches
    // for, and the unaligned forms carry no alignment requirement.
    unsafe {
        if len >= 16 {
            let mut at = 0;
            while at + 16 <= len {
                if let Some(first) = first_difference_16(lhs.add(at), rhs.add(at)) {
                    return byte_order(*lhs.add(at + first), *rhs.add(at + first));
                }
                at += 16;
            }
            if at < len {
                let tail = len - 16;
                if let Some(first) = first_difference_16(lhs.add(tail), rhs.add(tail)) {
                    return byte_order(*lhs.add(tail + first), *rhs.add(tail + first));
                }
            }
            0
        } else if len >= 8 {
            let head = word_order(
                u64::from_be(read_unaligned(lhs.cast::<u64>())),
                u64::from_be(read_unaligned(rhs.cast::<u64>())),
            );
            if head != 0 {
                return head;
            }
            word_order(
                u64::from_be(read_unaligned(lhs.add(len - 8).cast::<u64>())),
                u64::from_be(read_unaligned(rhs.add(len - 8).cast::<u64>())),
            )
        } else if len >= 4 {
            let head = word_order(
                u64::from(u32::from_be(read_unaligned(lhs.cast::<u32>()))),
                u64::from(u32::from_be(read_unaligned(rhs.cast::<u32>()))),
            );
            if head != 0 {
                return head;
            }
            word_order(
                u64::from(u32::from_be(read_unaligned(lhs.add(len - 4).cast::<u32>()))),
                u64::from(u32::from_be(read_unaligned(rhs.add(len - 4).cast::<u32>()))),
            )
        } else {
            if len >= 1 && *lhs != *rhs {
                return byte_order(*lhs, *rhs);
            }
            if len >= 2 && *lhs.add(1) != *rhs.add(1) {
                return byte_order(*lhs.add(1), *rhs.add(1));
            }
            if len == 3 {
                return byte_order(*lhs.add(2), *rhs.add(2));
            }
            0
        }
    }
}

// Exported only where they replace musl's own: every other target links a
// libc whose routines already pick a width-appropriate path at run time.
#[cfg(target_env = "musl")]
mod exports {
    use std::os::raw::{c_int, c_void};

    /// C `memcpy`.
    ///
    /// # Safety
    /// As C's `memcpy`.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn memcpy(
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
    ) -> *mut c_void {
        // SAFETY: the C caller holds `memcpy`'s contract.
        unsafe { super::copy(dst.cast(), src.cast(), n) };
        dst
    }

    /// C `memmove`.
    ///
    /// # Safety
    /// As C's `memmove`.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn memmove(
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
    ) -> *mut c_void {
        // SAFETY: the C caller holds `memmove`'s contract.
        unsafe { super::copy_overlapping(dst.cast(), src.cast(), n) };
        dst
    }

    /// C `memset`.
    ///
    /// # Safety
    /// As C's `memset`.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
        // C converts the fill value to `unsigned char`.
        // SAFETY: the C caller holds `memset`'s contract.
        unsafe { super::fill(dst.cast(), c as u8, n) };
        dst
    }

    /// C `memcmp`.
    ///
    /// # Safety
    /// As C's `memcmp`.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        // SAFETY: the C caller holds `memcmp`'s contract.
        unsafe { super::compare(a.cast(), b.cast(), n) }
    }

    /// C `bcmp`: zero exactly when the ranges are equal.
    ///
    /// # Safety
    /// As C's `memcmp`.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        // SAFETY: the C caller holds `memcmp`'s contract.
        unsafe { super::compare(a.cast(), b.cast(), n) }
    }
}

#[cfg(test)]
mod tests {
    const SPAN: usize = 320;

    #[test]
    fn compare_orders_like_a_byte_compare_at_every_length_and_position() {
        let base = pattern();
        for n in 0..SPAN {
            // SAFETY: both buffers hold at least `n` bytes.
            let same = unsafe { super::compare(base.as_ptr(), base.as_ptr(), n) };
            assert_eq!(same, 0, "n={n}");
            for at in 0..n {
                for delta in [1u8, 0x80, 0xFF] {
                    let mut other = base.clone();
                    other[at] = other[at].wrapping_add(delta);
                    let want = i32::from(base[at]) - i32::from(other[at]);
                    // SAFETY: both buffers hold at least `n` bytes.
                    let got = unsafe { super::compare(base.as_ptr(), other.as_ptr(), n) };
                    assert_eq!(got.signum(), want.signum(), "n={n} at={at} delta={delta}");
                }
            }
        }
    }

    fn pattern() -> Vec<u8> {
        (0..SPAN * 2).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn copy_matches_a_byte_copy_at_every_length_and_offset() {
        let src = pattern();
        for n in 0..SPAN {
            for offset in 0..8 {
                let mut got = vec![0xAAu8; SPAN + 16];
                let mut want = got.clone();
                // SAFETY: `got` holds `offset + n` bytes and `src` holds `3 + n`.
                unsafe { super::copy(got.as_mut_ptr().add(offset), src.as_ptr().add(3), n) };
                want[offset..offset + n].copy_from_slice(&src[3..3 + n]);
                assert_eq!(got, want, "n={n} offset={offset}");
            }
        }
    }

    #[test]
    fn copy_overlapping_matches_copy_within_in_both_directions() {
        for n in 0..SPAN {
            for shift in [1usize, 3, 8, 17, 40, 63, 64, 65, 200] {
                let mut got = pattern();
                let mut want = got.clone();
                let base = got.as_mut_ptr();
                // SAFETY: `got` holds `shift + n` bytes.
                unsafe { super::copy_overlapping(base.add(shift), base, n) };
                want.copy_within(0..n, shift);
                assert_eq!(got, want, "forward overlap n={n} shift={shift}");

                let mut got = pattern();
                let mut want = got.clone();
                let base = got.as_mut_ptr();
                // SAFETY: `got` holds `shift + n` bytes.
                unsafe { super::copy_overlapping(base, base.add(shift), n) };
                want.copy_within(shift..shift + n, 0);
                assert_eq!(got, want, "backward overlap n={n} shift={shift}");
            }
        }
    }

    #[test]
    fn fill_matches_a_byte_fill_at_every_length_and_offset() {
        for n in 0..SPAN {
            for offset in 0..8 {
                let mut got = vec![0x55u8; SPAN + 16];
                let mut want = got.clone();
                // SAFETY: `got` holds `offset + n` bytes.
                unsafe { super::fill(got.as_mut_ptr().add(offset), 0xC3, n) };
                want[offset..offset + n].fill(0xC3);
                assert_eq!(got, want, "n={n} offset={offset}");
            }
        }
    }
}
