//! SHA-256 for content fingerprints: fetched source trees, cache keys, and
//! the language's `crypto::sha256`. Native builds hash through `sha2`, which
//! selects the CPU's SHA instructions at run time; the wasm playground, which
//! does not link `sha2`, hashes with the portable rounds below.

#![forbid(unsafe_code)]
#![allow(clippy::many_single_char_names)]

#[cfg(target_arch = "wasm32")]
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[cfg(target_arch = "wasm32")]
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Incremental SHA-256 state.
///
/// This keeps at most one partial 64-byte block in memory, so callers that
/// hash source trees or network streams do not need a second contiguous copy
/// of their input merely to produce a digest.
/// Incremental SHA-256.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
pub struct Hasher(sha2::Sha256);

#[cfg(not(target_arch = "wasm32"))]
impl Hasher {
    /// A hasher over no bytes yet.
    #[must_use]
    pub fn new() -> Self {
        Self(sha2::Sha256::default())
    }

    /// Appends `bytes` to the message.
    pub fn update(&mut self, bytes: &[u8]) {
        sha2::Digest::update(&mut self.0, bytes);
    }

    /// The digest of the bytes appended so far.
    #[must_use]
    pub fn finalize(&self) -> [u8; 32] {
        sha2::Digest::finalize(self.0.clone()).into()
    }

    /// [`Self::finalize`] in lowercase hexadecimal.
    #[must_use]
    pub fn finalize_hex(&self) -> String {
        hex_digest(self.finalize())
    }
}

/// Incremental SHA-256.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub struct Hasher {
    state: [u32; 8],
    pending: [u8; 64],
    pending_len: usize,
    total_bytes: u64,
}

#[cfg(target_arch = "wasm32")]
impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "wasm32")]
impl Hasher {
    /// Creates a new incremental SHA-256 hasher.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: H0,
            pending: [0; 64],
            pending_len: 0,
            total_bytes: 0,
        }
    }

    /// Adds `bytes` to the hash input.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(bytes.len() as u64);

        if self.pending_len != 0 {
            let needed = 64 - self.pending_len;
            let take = needed.min(bytes.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&bytes[..take]);
            self.pending_len += take;
            bytes = &bytes[take..];
            if self.pending_len == 64 {
                compress(&mut self.state, &self.pending);
                self.pending_len = 0;
            }
        }

        while bytes.len() >= 64 {
            compress(&mut self.state, &bytes[..64]);
            bytes = &bytes[64..];
        }
        if !bytes.is_empty() {
            self.pending[..bytes.len()].copy_from_slice(bytes);
            self.pending_len = bytes.len();
        }
    }

    /// Finalizes this hash without consuming it, allowing callers to retain a
    /// checkpoint and continue hashing afterwards.
    #[must_use]
    pub fn finalize(&self) -> [u8; 32] {
        let mut state = self.state;
        let mut tail = [0u8; 128];
        tail[..self.pending_len].copy_from_slice(&self.pending[..self.pending_len]);
        tail[self.pending_len] = 0x80;
        let tail_len = if self.pending_len < 56 { 64 } else { 128 };
        let length_offset = tail_len - 8;
        tail[length_offset..tail_len]
            .copy_from_slice(&self.total_bytes.wrapping_mul(8).to_be_bytes());
        for chunk in tail[..tail_len].chunks_exact(64) {
            compress(&mut state, chunk);
        }
        digest_from_state(state)
    }

    /// Finalizes the hash as lowercase hexadecimal.
    #[must_use]
    pub fn finalize_hex(&self) -> String {
        hex_digest(self.finalize())
    }
}

/// Computes the SHA-256 digest of `bytes` and returns its 64-character
/// lowercase hex form - the canonical representation used by the
/// fetcher and the lockfile.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let digest = digest(bytes);
    hex_digest(digest)
}

/// Formats a SHA-256 digest in the canonical lowercase hexadecimal form.
#[must_use]
pub fn hex_digest(digest: [u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(hex_nibble(b >> 4));
        out.push(hex_nibble(b & 0xf));
    }
    out
}

/// Returns the 32-byte SHA-256 digest of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(target_arch = "wasm32")]
fn digest_from_state(state: [u32; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(target_arch = "wasm32")]
fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (i, word) in w.iter_mut().enumerate().take(16) {
        let j = i * 4;
        *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];

    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

const fn hex_nibble(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + value - 10) as char,
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::{Hasher, hex};

    #[test]
    fn incremental_hash_matches_one_shot_at_block_boundaries() {
        let input: Vec<u8> = (0..513).map(|i| (i % 251) as u8).collect();
        let expected = hex(&input);
        for width in [1, 2, 3, 7, 31, 63, 64, 65, 127, 256] {
            let mut hasher = Hasher::new();
            for chunk in input.chunks(width) {
                hasher.update(chunk);
            }
            assert_eq!(hasher.finalize_hex(), expected, "chunk width {width}");
        }
    }

    #[test]
    fn finalize_does_not_consume_the_checkpoint() {
        let mut hasher = Hasher::new();
        hasher.update(b"first");
        assert_eq!(hasher.finalize_hex(), hex(b"first"));
        hasher.update(b" second");
        assert_eq!(hasher.finalize_hex(), hex(b"first second"));
    }
}
