//! CRC-32C (Castagnoli), the checksum iSCSI, ext4, and most storage formats
//! use, computed with the CPU's CRC instruction where there is one.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

const fn tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                POLY ^ (crc >> 1)
            } else {
                crc >> 1
            };
            j += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut i = 0usize;
    while i < 256 {
        let mut crc = tables[0][i];
        let mut k = 1;
        while k < 8 {
            crc = tables[0][(crc & 0xFF) as usize] ^ (crc >> 8);
            tables[k][i] = crc;
            k += 1;
        }
        i += 1;
    }
    tables
}

static TABLES: [[u32; 256]; 8] = tables();

/// Slicing-by-eight over the reflected register, for a CPU with no CRC-32C
/// instruction.
fn update_tables(mut state: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let lo = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ state;
        let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        state = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xFF) as usize]
            ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    for &byte in chunks.remainder() {
        state = TABLES[0][((state ^ u32::from(byte)) & 0xFF) as usize] ^ (state >> 8);
    }
    state
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
fn update_hw(mut state: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        state = _mm_crc32_u64(u64::from(state), u64::from_le_bytes(word)) as u32;
    }
    for &byte in chunks.remainder() {
        state = _mm_crc32_u8(state, byte);
    }
    state
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
fn update_hw(mut state: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        state = __crc32cd(state, u64::from_le_bytes(word));
    }
    for &byte in chunks.remainder() {
        state = __crc32cb(state, byte);
    }
    state
}

/// Whether this CPU has the CRC-32C instruction, read once.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn has_hw() -> bool {
    use std::sync::OnceLock;
    static HW: OnceLock<bool> = OnceLock::new();
    *HW.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            std::arch::is_x86_feature_detected!("sse4.2")
        }
        #[cfg(target_arch = "aarch64")]
        {
            std::arch::is_aarch64_feature_detected!("crc")
        }
    })
}

/// Continues a CRC-32C from the finished value `crc` over `data`; `update(0,
/// data)` is the checksum of `data`.
#[must_use]
#[allow(
    unsafe_code,
    reason = "the CPU's CRC instruction is reached through a target-feature function"
)]
pub fn update(crc: u32, data: &[u8]) -> u32 {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    if has_hw() {
        // SAFETY: the CPU reported the instruction `update_hw` is compiled for.
        return !unsafe { update_hw(!crc, data) };
    }
    !update_tables(!crc, data)
}

#[cfg(test)]
mod tests {
    use super::{update, update_tables};

    #[test]
    fn the_check_value_matches_the_castagnoli_standard() {
        assert_eq!(update(0, b"123456789"), 0xE306_9283);
        assert_eq!(update(0, b""), 0);
    }

    #[test]
    fn the_instruction_and_the_tables_agree_on_every_length() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 37 + 11) as u8).collect();
        for len in 0..data.len() {
            let want = !update_tables(!0, &data[..len]);
            assert_eq!(update(0, &data[..len]), want, "len {len}");
        }
    }

    #[test]
    fn continuing_from_a_prefix_equals_the_whole() {
        let data = b"the quick brown fox jumps over the lazy dog";
        for split in 0..=data.len() {
            let head = update(0, &data[..split]);
            assert_eq!(
                update(head, &data[split..]),
                update(0, data),
                "split {split}"
            );
        }
    }
}
