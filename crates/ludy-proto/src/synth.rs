//! Synthetic object content: the SplitMix64 output stream for `seed`, written
//! as little-endian 64-bit words. Any byte range can be produced from its
//! offset alone, so a 50 GB object costs no disk.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

#[inline]
pub fn word(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add(index.wrapping_add(1).wrapping_mul(GAMMA));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fill `buf` with the bytes of the synthetic object starting at `offset`.
pub fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    let mut pos = 0usize;
    let mut off = offset;
    while pos < buf.len() {
        let w = word(seed, off / 8).to_le_bytes();
        let start = (off % 8) as usize;
        let take = (8 - start).min(buf.len() - pos);
        buf[pos..pos + take].copy_from_slice(&w[start..start + take]);
        pos += take;
        off += take as u64;
    }
}

pub fn bytes(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    fill(seed, offset, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_splitmix64() {
        // First outputs of SplitMix64 seeded with 0 (reference implementation).
        assert_eq!(word(0, 0), 0xE220_A839_7B1D_CDAF);
        assert_eq!(word(0, 1), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(word(0, 2), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn any_offset_agrees_with_a_full_render() {
        let full = bytes(42, 0, 4096);
        for (off, len) in [(0usize, 1usize), (1, 7), (5, 20), (8, 8), (1023, 1030), (4000, 96)] {
            assert_eq!(bytes(42, off as u64, len), &full[off..off + len]);
        }
    }
}
