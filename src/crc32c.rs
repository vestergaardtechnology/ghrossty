//! Port of `src/crc32c.zig`.
//!
//! STATUS: ported
//!
//! CRC32C with hardware acceleration.
//!
//! The Zig standard library implementation processes one byte per table
//! lookup (as of Zig 0.16), which is more than an order of magnitude slower
//! than the dedicated CRC32C instructions available on aarch64 (CRC
//! extension) and x86_64 (SSE4.2). This module selects the best backend at
//! compile time.
//!
//! Deviation: an x86_64 build without SSE4.2 enabled at compile time (the
//! default `x86-64` baseline) also checks for SSE4.2 at runtime and uses
//! the CRC32C instruction when the CPU has it. Zig builds for the native
//! CPU by default, so its default build gets the instruction at compile
//! time. The result is identical either way.
//!
//! Targets without a dedicated instruction, such as WebAssembly, use a
//! custom implementation that is faster than a byte-at-a-time table.
//!
//! The resulting value is identical across all backends: this is the
//! iSCSI CRC32C parameter set (reflected, initial and final XOR
//! `0xFFFFFFFF`), matching `std.hash.crc.@"CRC-32/ISCSI"`.

/// The available backends.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Aarch64Crc,
    X86_64Sse42,
    Software,
}

/// The backend selected at compile time from the target features.
#[allow(dead_code)]
const BACKEND: Backend = if cfg!(all(target_arch = "aarch64", target_feature = "crc")) {
    Backend::Aarch64Crc
} else if cfg!(all(target_arch = "x86_64", target_feature = "sse4.2")) {
    Backend::X86_64Sse42
} else {
    Backend::Software
};

/// Streaming CRC32C with the same interface shape as `std.hash.crc` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32c {
    pub crc: u32,
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::init()
    }
}

impl Crc32c {
    pub const fn init() -> Crc32c {
        Crc32c { crc: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.crc = update_backend(self.crc, bytes);
    }

    pub fn final_(&self) -> u32 {
        self.crc ^ 0xFFFF_FFFF
    }

    pub fn hash(bytes: &[u8]) -> u32 {
        let mut c = Crc32c::init();
        c.update(bytes);
        c.final_()
    }
}

#[cfg(any(
    all(target_arch = "aarch64", target_feature = "crc"),
    all(target_arch = "x86_64", target_feature = "sse4.2")
))]
#[inline(always)]
fn update_backend(crc: u32, bytes: &[u8]) -> u32 {
    update_hardware(crc, bytes)
}

#[cfg(all(target_arch = "x86_64", not(target_feature = "sse4.2")))]
#[inline(always)]
fn update_backend(crc: u32, bytes: &[u8]) -> u32 {
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU supports SSE4.2 (checked above).
        return unsafe { runtime_sse42::update(crc, bytes) };
    }
    software::update(crc, bytes)
}

#[cfg(not(any(
    all(target_arch = "aarch64", target_feature = "crc"),
    target_arch = "x86_64"
)))]
#[inline(always)]
fn update_backend(crc: u32, bytes: &[u8]) -> u32 {
    software::update(crc, bytes)
}

/// `update_hardware` for x86_64 builds without SSE4.2 at compile time,
/// for CPUs that have it at runtime.
#[cfg(all(target_arch = "x86_64", not(target_feature = "sse4.2")))]
mod runtime_sse42 {
    use std::arch::x86_64::{_mm_crc32_u32, _mm_crc32_u64, _mm_crc32_u8};

    /// # Safety
    ///
    /// The CPU must support SSE4.2.
    #[target_feature(enable = "sse4.2")]
    pub unsafe fn update(initial: u32, bytes: &[u8]) -> u32 {
        let mut crc = initial;
        let mut remaining = bytes;

        while remaining.len() >= 8 {
            crc = _mm_crc32_u64(crc as u64, u64::from_le_bytes(remaining[0..8].try_into().unwrap())) as u32;
            remaining = &remaining[8..];
        }
        if remaining.len() >= 4 {
            crc = _mm_crc32_u32(crc, u32::from_le_bytes(remaining[0..4].try_into().unwrap()));
            remaining = &remaining[4..];
        }
        for &byte in remaining {
            crc = _mm_crc32_u8(crc, byte);
        }
        crc
    }
}

/// One update pass using the dedicated CRC32C instructions. Both supported
/// architectures handle unaligned loads efficiently, so the loop reads
/// little-endian words directly from the input.
#[cfg(any(
    all(target_arch = "aarch64", target_feature = "crc"),
    all(target_arch = "x86_64", target_feature = "sse4.2")
))]
fn update_hardware(initial: u32, bytes: &[u8]) -> u32 {
    let mut crc = initial;
    let mut remaining = bytes;

    while remaining.len() >= 8 {
        crc = step::u64(crc, u64::from_le_bytes(remaining[0..8].try_into().unwrap()));
        remaining = &remaining[8..];
    }
    if remaining.len() >= 4 {
        crc = step::u32(crc, u32::from_le_bytes(remaining[0..4].try_into().unwrap()));
        remaining = &remaining[4..];
    }
    for &byte in remaining {
        crc = step::u8(crc, byte);
    }
    crc
}

/// One CRC32C instruction folding `value` into the running CRC.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
mod step {
    use std::arch::x86_64::{_mm_crc32_u32, _mm_crc32_u64, _mm_crc32_u8};

    #[inline(always)]
    pub fn u8(crc: u32, value: u8) -> u32 {
        // SAFETY: sse4.2 is enabled at compile time.
        unsafe { _mm_crc32_u8(crc, value) }
    }

    #[inline(always)]
    pub fn u32(crc: u32, value: u32) -> u32 {
        // SAFETY: sse4.2 is enabled at compile time.
        unsafe { _mm_crc32_u32(crc, value) }
    }

    #[inline(always)]
    pub fn u64(crc: u32, value: u64) -> u32 {
        // SAFETY: sse4.2 is enabled at compile time.
        unsafe { _mm_crc32_u64(crc as u64, value) as u32 }
    }
}

/// One CRC32C instruction folding `value` into the running CRC.
#[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
mod step {
    use std::arch::aarch64::{__crc32cb, __crc32cd, __crc32cw};

    #[inline(always)]
    pub fn u8(crc: u32, value: u8) -> u32 {
        // SAFETY: the crc feature is enabled at compile time.
        unsafe { __crc32cb(crc, value) }
    }

    #[inline(always)]
    pub fn u32(crc: u32, value: u32) -> u32 {
        // SAFETY: the crc feature is enabled at compile time.
        unsafe { __crc32cw(crc, value) }
    }

    #[inline(always)]
    pub fn u64(crc: u32, value: u64) -> u32 {
        // SAFETY: the crc feature is enabled at compile time.
        unsafe { __crc32cd(crc, value) }
    }
}

/// The portable software backend, used by targets without a dedicated
/// CRC32C instruction.
#[allow(dead_code)]
mod software {
    /// The reflected CRC32C (Castagnoli) polynomial.
    pub const REFLECTED_POLY: u32 = 0x82F63B78;

    /// Number of slicing tables, which is also the bytes folded per
    /// iteration.
    pub const SLICES: usize = 16;

    /// Inputs below this length use the single-stream pass: the
    /// stream-combine matrix work would not pay for itself.
    pub const MULTI_STREAM_THRESHOLD: usize = 4096;

    /// Slicing tables: `tables[i][b]` is the CRC of byte `b` followed by
    /// `i` zero bytes. Table zero is the classic one-byte-per-step table;
    /// the higher tables let one iteration fold a whole block with
    /// independent lookups instead of a byte-by-byte dependency chain.
    pub static TABLES: [[u32; 256]; SLICES] = make_tables();

    const fn make_tables() -> [[u32; 256]; SLICES] {
        let mut result = [[0u32; 256]; SLICES];
        let mut n = 0;
        while n < 256 {
            let mut crc: u32 = n as u32;
            let mut k = 0;
            while k < 8 {
                crc = (crc >> 1) ^ (REFLECTED_POLY.wrapping_mul(crc & 1));
                k += 1;
            }
            result[0][n] = crc;
            n += 1;
        }
        let mut i = 1;
        while i < SLICES {
            let mut n = 0;
            while n < 256 {
                let prev = result[i - 1][n];
                result[i][n] = (prev >> 8) ^ result[0][(prev & 0xFF) as usize];
                n += 1;
            }
            i += 1;
        }
        result
    }

    pub fn update(initial: u32, bytes: &[u8]) -> u32 {
        if bytes.len() >= MULTI_STREAM_THRESHOLD {
            return update_multi(initial, bytes);
        }

        update_single(initial, bytes)
    }

    /// One single-stream update pass using slicing.
    pub fn update_single(initial: u32, bytes: &[u8]) -> u32 {
        let t = &TABLES;
        let mut crc = initial;
        let mut i: usize = 0;
        while i + SLICES <= bytes.len() {
            crc = fold_chunk(bytes, i, crc);
            i += SLICES;
        }
        for &byte in &bytes[i..] {
            crc = (crc >> 8) ^ t[0][((crc ^ byte as u32) & 0xFF) as usize];
        }
        crc
    }

    /// One update pass as three independent interleaved streams. Faster
    /// for large enough inputs.
    pub fn update_multi(initial: u32, bytes: &[u8]) -> u32 {
        // Both leading parts are block multiples so the interleaved loop
        // needs no tail handling; the third part absorbs the remainder.
        let part = (bytes.len() / 3) & !(SLICES - 1);
        let p0 = &bytes[0..part];
        let p1 = &bytes[part..][..part];
        let p2 = &bytes[2 * part..];

        let mut s0 = initial;
        let mut s1: u32 = 0;
        let mut s2: u32 = 0;
        let mut i: usize = 0;
        while i + SLICES <= part {
            s0 = fold_chunk(p0, i, s0);
            s1 = fold_chunk(p1, i, s1);
            s2 = fold_chunk(p2, i, s2);
            i += SLICES;
        }
        s2 = update_single(s2, &p2[part..]);

        let s01 = s1 ^ zero_shift(s0, p1.len());
        s2 ^ zero_shift(s01, p2.len())
    }

    /// Fold one aligned block through the per-position slicing tables.
    /// The running CRC must already be XORed into the block's first word.
    #[inline(always)]
    fn fold_block(words: &[u32; SLICES / 4]) -> u32 {
        let t = &TABLES;
        let len = SLICES;
        let mut crc: u32 = 0;
        for (w, &word) in words.iter().enumerate() {
            let base = len - 1 - w * 4;
            crc ^= t[base][(word & 0xFF) as usize]
                ^ t[base - 1][((word >> 8) & 0xFF) as usize]
                ^ t[base - 2][((word >> 16) & 0xFF) as usize]
                ^ t[base - 3][(word >> 24) as usize];
        }
        crc
    }

    /// Fold the block starting at `offset`, chaining the running CRC state.
    #[inline(always)]
    fn fold_chunk(bytes: &[u8], offset: usize, crc: u32) -> u32 {
        let mut words = [0u32; SLICES / 4];
        for (w, word) in words.iter_mut().enumerate() {
            let start = offset + w * 4;
            *word = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
        }
        words[0] ^= crc;
        fold_block(&words)
    }

    /// Advance a CRC state as if `len` zero bytes had been processed.
    fn zero_shift(state: u32, len: usize) -> u32 {
        let mut s = state;
        let mut remaining = len;
        let mut k: usize = 0;
        while remaining != 0 {
            if remaining & 1 != 0 {
                let mat = &ZERO_SHIFT_MATRICES[k / 2];
                s = mat_times_vec(mat, s);
                if k % 2 != 0 {
                    s = mat_times_vec(mat, s);
                }
            }
            remaining >>= 1;
            k += 1;
        }
        s
    }

    /// Multiply the GF(2) matrix by a CRC state column vector.
    #[inline(always)]
    const fn mat_times_vec(mat: &[u32; 32], vec: u32) -> u32 {
        let mut sum: u32 = 0;
        let mut v = vec;
        let mut i: usize = 0;
        while v != 0 {
            if v & 1 != 0 {
                sum ^= mat[i];
            }
            v >>= 1;
            i += 1;
        }
        sum
    }

    static ZERO_SHIFT_MATRICES: [[u32; 32]; 32] = make_zero_shift_matrices();

    const fn make_zero_shift_matrices() -> [[u32; 32]; 32] {
        let tables = make_tables();
        let mut matrices = [[0u32; 32]; 32];
        let mut previous = [0u32; 32];
        let mut i = 0;
        while i < 32 {
            let unit: u32 = 1 << i;
            previous[i] = (unit >> 8) ^ tables[0][(unit & 0xFF) as usize];
            i += 1;
        }
        matrices[0] = previous;
        let mut k = 1;
        while k < 64 {
            let mut squared = [0u32; 32];
            let mut i = 0;
            while i < 32 {
                squared[i] = mat_times_vec(&previous, previous[i]);
                i += 1;
            }
            previous = squared;
            if k % 2 == 0 {
                matrices[k / 2] = squared;
            }
            k += 1;
        }
        matrices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bit-at-a-time generic implementation of the same parameter set.
    /// This is the reference the tests compare against (Zig uses
    /// `std.hash.crc.Generic` for this). `crc` holds the running state.
    struct Reference {
        crc: u32,
    }

    impl Reference {
        fn init() -> Self {
            Self { crc: 0xFFFF_FFFF }
        }

        fn update(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.crc ^= b as u32;
                for _ in 0..8 {
                    self.crc = (self.crc >> 1) ^ (software::REFLECTED_POLY * (self.crc & 1));
                }
            }
        }

        fn final_(&self) -> u32 {
            self.crc ^ 0xFFFF_FFFF
        }

        fn hash(bytes: &[u8]) -> u32 {
            let mut r = Self::init();
            r.update(bytes);
            r.final_()
        }
    }

    /// Deterministic PRNG (splitmix64) standing in for `std.Random.DefaultPrng`.
    fn random_bytes(seed: u64, out: &mut [u8]) {
        let mut state = seed;
        for chunk in out.chunks_mut(8) {
            state = state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    #[test]
    fn software_slicing_matches_the_standard_library() {
        // The selected backend may be hardware, so cover the sliced software
        // path directly: every length around the sixteen-byte boundary, several
        // alignments, and continuation across arbitrary split points.
        let mut bytes = [0u8; 512 + 19];
        random_bytes(0x511C, &mut bytes);

        for len in 0..=64 {
            for offset in 0..4 {
                let input = &bytes[offset..][..len];
                let mut reference = Reference::init();
                reference.update(input);
                assert_eq!(reference.crc, software::update(0xFFFF_FFFF, input));
            }
        }

        let long = &bytes[0..512];
        let mut reference = Reference::init();
        reference.update(long);
        for split in [0usize, 1, 15, 16, 17, 100, 511, 512] {
            let first = software::update(0xFFFF_FFFF, &long[0..split]);
            assert_eq!(reference.crc, software::update(first, &long[split..]));
        }
    }

    #[test]
    fn software_multi_stream_matches_the_standard_library() {
        // Lengths around and far above the interleaving threshold, plus odd
        // remainders, so all three streams and both combine steps are covered.
        let mut bytes = vec![0u8; 96 * 1024];
        random_bytes(0x3517_1A3B, &mut bytes);

        let t = software::MULTI_STREAM_THRESHOLD;
        for len in [
            t - 1,
            t,
            t + 1,
            t + 97,
            12 * 1024,
            64 * 1024 + 31,
            bytes.len(),
        ] {
            let input = &bytes[0..len];
            let mut reference = Reference::init();
            reference.update(input);
            assert_eq!(reference.crc, software::update(0xFFFF_FFFF, input));

            // Continuation across a split inside the multi-stream range.
            let first = software::update(0xFFFF_FFFF, &input[0..len / 2]);
            assert_eq!(reference.crc, software::update(first, &input[len / 2..]));
        }
    }

    #[test]
    fn matches_the_check_value() {
        // The catalog check value for CRC-32/ISCSI.
        assert_eq!(0xE3069283u32, Crc32c::hash(b"123456789"));
    }

    #[test]
    fn matches_the_standard_library_at_every_length_and_split() {
        let mut bytes = [0u8; 259];
        random_bytes(0xC5C32C, &mut bytes);

        for len in 0..=bytes.len() {
            let input = &bytes[0..len];
            assert_eq!(Reference::hash(input), Crc32c::hash(input));

            // Streaming across arbitrary split points must not change the
            // result: word batching may not leak state between updates.
            let mut split = Crc32c::init();
            split.update(&input[0..len / 3]);
            split.update(&input[len / 3..len - len / 3]);
            split.update(&input[len - len / 3..]);
            assert_eq!(Reference::hash(input), split.final_());
        }
    }

    #[test]
    fn matches_the_standard_library_at_every_alignment() {
        let mut bytes = [0u8; 64 + 16];
        random_bytes(0xA11C, &mut bytes);

        for offset in 0..16 {
            let input = &bytes[offset..][..64];
            assert_eq!(Reference::hash(input), Crc32c::hash(input));
        }
    }
}
