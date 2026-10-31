//! Port of `src/simd/vt.zig`.
//!
//! STATUS: ported
//!
//! The Zig SIMD build calls `vt.cpp` (Highway + simdutf). This port replaces
//! it with a Rust implementation of the same structure: an ASCII fast path
//! that fuses the ESC scan with the UTF-8 -> UTF-32 widening 16 bytes at a
//! time (SSE2 on x86_64, SWAR elsewhere), falling back on the first non-ASCII
//! byte to a `memchr` ESC search plus the scalar maximal-subpart decoder.
//! Where simdutf converts multi-byte UTF-8 with AVX2, the port does the
//! same on x86_64 CPUs with AVX2 (detected at runtime): blocks of 16
//! sequence starts that are entirely valid UTF-8 are validated and
//! converted with vector code, and anything else (ill-formed input,
//! incomplete sequences, the last 32 bytes) takes the scalar decoder.
//! Results are identical to `utf8_decode_until_control_seq_scalar` (checked
//! by the differential tests, as in Zig).

use super::index_of::index_of;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeResult {
    pub consumed: usize,
    pub decoded: usize,
}

/// Decode the UTF-8 text in input into output until an escape
/// character is found. Returns the number of bytes consumed from input
/// and the number of decoded codepoints written to output.
///
/// This may consume less than the full input even with no escape
/// character if the input ends with an incomplete (but valid so far) UTF-8
/// sequence; the caller should keep those bytes for the next call.
/// Ill-formed sequences are replaced with U+FFFD following the W3C/Unicode
/// "U+FFFD Substitution of Maximal Subparts" algorithm.
///
/// `output` must be at least as long as `input`.
#[inline]
pub fn utf8_decode_until_control_seq(input: &[u8], output: &mut [u32]) -> DecodeResult {
    debug_assert!(output.len() >= input.len());

    // ASCII fast path: terminal input is overwhelmingly ASCII, for which
    // UTF-8 decoding is a simple widening of each byte to 32 bits. We
    // fuse the ESC scan with the decode, one chunk at a time, and only
    // fall back to the full UTF-8 decoder when we encounter a non-ASCII
    // byte. This avoids a second pass over the input.
    let count = input.len();
    let mut i = 0;
    while i + CHUNK <= count {
        let chunk: &[u8; CHUNK] = input[i..i + CHUNK].try_into().unwrap();
        let out: &mut [u32; CHUNK] = (&mut output[i..i + CHUNK]).try_into().unwrap();

        // Widen the whole chunk unconditionally: output is guaranteed to
        // be at least as large as input, and if we stop mid-chunk only
        // the prefix is reported (the rest is scratch that the caller
        // never reads).
        let stop_mask = widen_ascii_chunk(chunk, out);
        if stop_mask == 0 {
            i += CHUNK;
            continue;
        }

        let stop_idx = i + stop_mask.trailing_zeros() as usize;
        if input[stop_idx] == 0x1B {
            // ESC: everything before it was ASCII, one codepoint per byte.
            return DecodeResult {
                consumed: stop_idx,
                decoded: stop_idx,
            };
        }

        // Non-ASCII: decode the rest (up to an ESC) with the full decoder.
        return decode_non_ascii_until_control_seq(input, stop_idx, output);
    }

    // Leftover input (< CHUNK bytes): process one byte at a time.
    while i < count {
        let b = input[i];
        if b == 0x1B {
            return DecodeResult {
                consumed: i,
                decoded: i,
            };
        }
        if b >= 0x80 {
            return decode_non_ascii_until_control_seq(input, i, output);
        }
        output[i] = b as u32;
        i += 1;
    }

    // The entire input was ASCII (no ESC, no partial sequences possible).
    DecodeResult {
        consumed: count,
        decoded: count,
    }
}

const CHUNK: usize = 16;

/// Widen 16 ASCII bytes into `out` and return a bitmask (bit k for byte k)
/// of the bytes that stop the ASCII fast path: ESC or any byte >= 0x80.
#[inline(always)]
fn widen_ascii_chunk(chunk: &[u8; CHUNK], out: &mut [u32; CHUNK]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        // SAFETY: SSE2 is part of the x86_64 baseline. All loads and stores
        // are unaligned and in bounds of the 16-byte input and the 16-element
        // (64-byte) output arrays.
        unsafe {
            let v = _mm_loadu_si128(chunk.as_ptr() as *const __m128i);
            let esc = _mm_cmpeq_epi8(v, _mm_set1_epi8(0x1B));
            let mask = (_mm_movemask_epi8(v) | _mm_movemask_epi8(esc)) as u32;

            let zero = _mm_setzero_si128();
            let lo = _mm_unpacklo_epi8(v, zero);
            let hi = _mm_unpackhi_epi8(v, zero);
            let p = out.as_mut_ptr() as *mut __m128i;
            _mm_storeu_si128(p, _mm_unpacklo_epi16(lo, zero));
            _mm_storeu_si128(p.add(1), _mm_unpackhi_epi16(lo, zero));
            _mm_storeu_si128(p.add(2), _mm_unpacklo_epi16(hi, zero));
            _mm_storeu_si128(p.add(3), _mm_unpackhi_epi16(hi, zero));
            mask
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        widen_ascii_chunk_portable(chunk, out)
    }
}

/// Portable (SWAR) version of [`widen_ascii_chunk`].
#[cfg(any(not(target_arch = "x86_64"), test))]
#[inline(always)]
fn widen_ascii_chunk_portable(chunk: &[u8; CHUNK], out: &mut [u32; CHUNK]) -> u32 {
    for (o, &b) in out.iter_mut().zip(chunk.iter()) {
        *o = b as u32;
    }
    let lo = u64::from_le_bytes(chunk[..8].try_into().unwrap());
    let hi = u64::from_le_bytes(chunk[8..].try_into().unwrap());
    (swar_stop_mask(lo) | (swar_stop_mask(hi) << 8)) as u32
}

/// For 8 little-endian bytes, a bitmask with bit k set for bytes that are
/// ESC or >= 0x80. Bits above the first set bit may be spurious (borrow
/// propagation), which is fine since only the lowest set bit is used.
#[cfg(any(not(target_arch = "x86_64"), test))]
#[inline(always)]
fn swar_stop_mask(w: u64) -> u64 {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let x = w ^ (LO * 0x1B);
    let esc = x.wrapping_sub(LO) & !x & HI;
    let stop = (w & HI) | esc;
    // Compress the high bit of each byte into one bit per byte.
    let mut mask = 0u64;
    for k in 0..8 {
        mask |= ((stop >> (8 * k + 7)) & 1) << k;
    }
    mask
}

/// The general (non-ASCII) portion of `utf8_decode_until_control_seq`.
/// Scans for ESC starting at byte offset `base` and decodes
/// input[base..stop). The caller must have already decoded input[0..base)
/// as ASCII into output[0..base) (one codepoint per byte).
#[inline(never)]
fn decode_non_ascii_until_control_seq(input: &[u8], base: usize, output: &mut [u32]) -> DecodeResult {
    let rest = &input[base..];
    let idx = index_of(rest, 0x1B).unwrap_or(rest.len());
    let r = decode_region(&rest[..idx], idx == rest.len(), &mut output[base..], avx2::available());
    DecodeResult {
        consumed: base + r.consumed,
        decoded: base + r.decoded,
    }
}

#[allow(dead_code)]
fn utf8_decode_until_control_seq_scalar(input: &[u8], output: &mut [u32]) -> DecodeResult {
    // Find our escape
    let idx = index_of(input, 0x1B).unwrap_or(input.len());
    let decode = &input[..idx];
    decode_region(decode, decode.len() == input.len(), output, false)
}

/// The decode loop of the Zig `utf8DecodeUntilControlSeqScalar`.
/// `at_input_end` is true when `decode` ends at the true end of the input
/// (`decode.len == input.len` in Zig), i.e. it was not bounded by an ESC.
/// `simd` enables the AVX2 block decoder, which the caller must have
/// checked with `avx2::available`.
fn decode_region(decode: &[u8], at_input_end: bool, output: &mut [u32], simd: bool) -> DecodeResult {
    debug_assert!(output.len() >= decode.len());

    // Go through and decode one item at a time, following the W3C/Unicode
    // "U+FFFD Substitution of Maximal Subparts" algorithm for ill-formed
    // subsequences.
    let mut decode_offset: usize = 0;
    let mut decode_count: usize = 0;
    while decode_offset < decode.len() {
        // Vectorized blocks of valid UTF-8. A block the vector decoder
        // rejects takes one scalar step below, which handles its first
        // item exactly as if the vector decoder did not exist.
        #[cfg(target_arch = "x86_64")]
        if simd && decode.len() - decode_offset >= avx2::WINDOW && output.len() - decode_count >= avx2::MAX_OUT {
            // SAFETY: AVX2 is available (the caller checked), WINDOW input
            // bytes are readable and MAX_OUT output elements writable.
            let r = unsafe {
                avx2::decode_block(
                    decode.as_ptr().add(decode_offset),
                    output.as_mut_ptr().add(decode_count),
                )
            };
            if let Some((consumed, decoded)) = r {
                decode_offset += consumed;
                decode_count += decoded;
                continue;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = simd;

        let b0 = decode[decode_offset];

        // ASCII fast path.
        if b0 < 0x80 {
            while decode_offset + 8 <= decode.len() {
                let w = u64::from_le_bytes(decode[decode_offset..decode_offset + 8].try_into().unwrap());
                if w & 0x8080_8080_8080_8080 != 0 {
                    break;
                }
                for k in 0..8 {
                    output[decode_count + k] = decode[decode_offset + k] as u32;
                }
                decode_count += 8;
                decode_offset += 8;
            }
            while decode_offset < decode.len() {
                let b = decode[decode_offset];
                if b >= 0x80 {
                    break;
                }
                output[decode_count] = b as u32;
                decode_count += 1;
                decode_offset += 1;
            }
            continue;
        }

        // Continuation byte (80-BF) or invalid byte (C0-C1, F5-FF)
        // as lead: each is its own maximal subpart → one FFFD per byte.
        if !(0xC2..=0xF4).contains(&b0) {
            output[decode_count] = 0xFFFD;
            decode_count += 1;
            decode_offset += 1;
            continue;
        }

        // Multi-byte sequence. Only the first continuation byte has a
        // lead-dependent valid range per Unicode Table 3-7; later
        // continuation bytes are always 80-BF. Range validity per
        // Table 3-7 excludes overlong, surrogate, and out-of-range
        // encodings, so a fully valid sequence can be decoded by
        // direct bit assembly with no further checks.
        let seq_len: usize = if b0 < 0xE0 {
            2
        } else if b0 < 0xF0 {
            3
        } else {
            4
        };
        let (cb1_lo, cb1_hi): (u8, u8) = match b0 {
            0xE0 => (0xA0, 0xBF),
            0xED => (0x80, 0x9F),
            0xF0 => (0x90, 0xBF),
            0xF4 => (0x80, 0x8F),
            _ => (0x80, 0xBF),
        };

        // Check how many continuation bytes form a valid prefix (the
        // maximal subpart), accumulating codepoint bits as we go. The
        // lead byte contributes its low 7-len bits.
        let mut cp: u32 = (b0 as u32) & (0x7F >> seq_len);
        let mut valid: usize = 1; // lead byte is valid
        while valid < seq_len {
            if decode_offset + valid >= decode.len() {
                // The sequence is cut off by the end of the decode
                // region. If the region ends at the true end of the
                // input then it may be completed by future input, so
                // stop without consuming these bytes. If the region
                // was bounded by an ESC then the sequence can never
                // be completed; the valid-so-far prefix is a maximal
                // subpart which maps to a single U+FFFD below.
                if at_input_end {
                    return DecodeResult {
                        consumed: decode_offset,
                        decoded: decode_count,
                    };
                }
                break;
            }
            let cb = decode[decode_offset + valid];
            let lo: u8 = if valid == 1 { cb1_lo } else { 0x80 };
            let hi: u8 = if valid == 1 { cb1_hi } else { 0xBF };
            if cb < lo || cb > hi {
                break;
            }
            cp = (cp << 6) | (cb as u32 & 0x3F);
            valid += 1;
        }

        if valid == seq_len {
            output[decode_count] = cp;
            decode_count += 1;
            decode_offset += seq_len;
        } else {
            // Incomplete/ill-formed: the maximal subpart (valid bytes)
            // maps to a single FFFD.
            output[decode_count] = 0xFFFD;
            decode_count += 1;
            decode_offset += valid;
        }
    }

    DecodeResult {
        consumed: decode_offset,
        decoded: decode_count,
    }
}

/// AVX2 conversion of valid multi-byte UTF-8 to UTF-32 (simdutf's role in
/// the Zig build).
///
/// A block looks at `WINDOW` input bytes and takes the sequences that
/// start in its first `STARTS` bytes. The block is accepted only when all
/// of them are well-formed (every lead byte is followed by exactly the
/// continuation bytes it needs, within the Table 3-7 ranges, and no other
/// byte is a continuation or invalid). A well-formed sequence has exactly
/// one decoding, so an accepted block produces what the scalar decoder
/// would. Each of the 16 start positions is decoded as if it were a lead
/// byte, then the real starts are left-packed into the output.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    /// Input bytes a block reads.
    pub const WINDOW: usize = 32;
    /// Positions a block decodes sequences from.
    const STARTS: u32 = 16;
    /// Output elements a block may write (decoded or scratch).
    pub const MAX_OUT: usize = 16;

    #[inline]
    pub fn available() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    /// For each 8-bit mask, the indices of its set bits in order: the
    /// `vpermd` permutation that left-packs those lanes.
    static PACK: [[u32; 8]; 256] = {
        let mut table = [[0u32; 8]; 256];
        let mut mask = 0;
        while mask < 256 {
            let mut n = 0;
            let mut lane = 0;
            while lane < 8 {
                if mask & (1 << lane) != 0 {
                    table[mask][n] = lane as u32;
                    n += 1;
                }
                lane += 1;
            }
            mask += 1;
        }
        table
    };

    /// Bytes `>= c` (unsigned) as a bitmask.
    #[inline(always)]
    unsafe fn ge(v: __m256i, c: u8) -> u32 {
        let c = _mm256_set1_epi8(c as i8);
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_max_epu8(v, c), v)) as u32
    }

    /// Bytes `== c` as a bitmask.
    #[inline(always)]
    unsafe fn eq(v: __m256i, c: u8) -> u32 {
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32
    }

    /// Validate and convert one block. Returns the bytes consumed (16 to
    /// 19, always ending on a sequence boundary) and codepoints written, or
    /// None (with `dst` possibly scribbled on) if the block is not
    /// entirely well-formed.
    ///
    /// # Safety
    ///
    /// AVX2 must be available, `src` must have `WINDOW` readable bytes and
    /// `dst` `MAX_OUT` writable elements.
    #[target_feature(enable = "avx2")]
    pub unsafe fn decode_block(src: *const u8, dst: *mut u32) -> Option<(usize, usize)> {
        const W: u32 = (1 << STARTS) - 1;

        // SAFETY: WINDOW bytes are readable.
        let v = unsafe { _mm256_loadu_si256(src.cast()) };
        let hi = _mm256_movemask_epi8(v) as u32;
        if hi & W == 0 {
            // Plain ASCII: the scalar loop's ASCII run handles it.
            return None;
        }
        let ge_90 = ge(v, 0x90);
        let ge_a0 = ge(v, 0xA0);
        let ge_c0 = ge(v, 0xC0);
        let ge_c2 = ge(v, 0xC2);
        let ge_e0 = ge(v, 0xE0);
        let ge_f0 = ge(v, 0xF0);
        let ge_f5 = ge(v, 0xF5);

        let ascii = !hi;
        let cont = hi & !ge_c0;
        let lead2 = ge_c2 & !ge_e0;
        let lead3 = ge_e0 & !ge_f0;
        let lead4 = ge_f0 & !ge_f5;
        let invalid = (ge_c0 & !ge_c2) | ge_f5;

        // The first continuation byte's lead-dependent range (Table 3-7).
        let bad_second = (eq(v, 0xE0) & !(ge_a0 >> 1))
            | (eq(v, 0xED) & (ge_a0 >> 1))
            | (eq(v, 0xF0) & !(ge_90 >> 1))
            | (eq(v, 0xF4) & (ge_90 >> 1));

        let (lead2, lead3, lead4) = (lead2 & W, lead3 & W, lead4 & W);
        let required = ((lead2 | lead3 | lead4) << 1) | ((lead3 | lead4) << 2) | (lead4 << 3);
        if (invalid | bad_second) & W != 0 || cont & W != required & W || required & !cont != 0 {
            return None;
        }
        let starts = (ascii | lead2 | lead3 | lead4) & W;
        let consumed = STARTS + (required >> STARTS).count_ones();

        // Decode the 16 positions as if each were a lead byte, 8 at a
        // time: lane k holds the four bytes from position k.
        let words = _mm256_setr_epi8(
            0, 1, 2, 3, 1, 2, 3, 4, 2, 3, 4, 5, 3, 4, 5, 6, //
            4, 5, 6, 7, 5, 6, 7, 8, 6, 7, 8, 9, 7, 8, 9, 10,
        );
        let low6 = _mm256_set1_epi32(0x3F);
        let byte = _mm256_set1_epi32(0xFF);
        let mut written = 0usize;
        for group in 0..2 {
            // SAFETY: bytes 8 * group .. 8 * group + 16 are in the window.
            let chunk = unsafe { _mm_loadu_si128(src.add(8 * group).cast()) };
            let w = _mm256_shuffle_epi8(_mm256_broadcastsi128_si256(chunk), words);
            let b0 = _mm256_and_si256(w, byte);
            let t1 = _mm256_and_si256(_mm256_srli_epi32(w, 8), low6);
            let t2 = _mm256_and_si256(_mm256_srli_epi32(w, 16), low6);
            let t3 = _mm256_and_si256(_mm256_srli_epi32(w, 24), low6);

            let cp2 = _mm256_or_si256(_mm256_slli_epi32(_mm256_and_si256(b0, _mm256_set1_epi32(0x1F)), 6), t1);
            let cp3 = _mm256_or_si256(
                _mm256_slli_epi32(_mm256_and_si256(b0, _mm256_set1_epi32(0x0F)), 12),
                _mm256_or_si256(_mm256_slli_epi32(t1, 6), t2),
            );
            let cp4 = _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_slli_epi32(_mm256_and_si256(b0, _mm256_set1_epi32(0x07)), 18),
                    _mm256_slli_epi32(t1, 12),
                ),
                _mm256_or_si256(_mm256_slli_epi32(t2, 6), t3),
            );

            let is_ascii = _mm256_cmpgt_epi32(_mm256_set1_epi32(0x80), b0);
            let is_2 = _mm256_cmpgt_epi32(_mm256_set1_epi32(0xE0), b0);
            let is_3 = _mm256_cmpgt_epi32(_mm256_set1_epi32(0xF0), b0);
            let cp = _mm256_blendv_epi8(cp4, cp3, is_3);
            let cp = _mm256_blendv_epi8(cp, cp2, is_2);
            let cp = _mm256_blendv_epi8(cp, b0, is_ascii);

            let mask = (starts >> (8 * group)) & 0xFF;
            // SAFETY: PACK rows are 8 u32s.
            let perm = unsafe { _mm256_loadu_si256(PACK[mask as usize].as_ptr().cast()) };
            let packed = _mm256_permutevar8x32_epi32(cp, perm);
            // SAFETY: written <= 8, so 8 lanes fit in MAX_OUT.
            unsafe { _mm256_storeu_si256(dst.add(written).cast(), packed) };
            written += mask.count_ones() as usize;
        }
        Some((consumed as usize, written))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic PRNG for the differential test (Zig uses
    /// `std.Random.DefaultPrng`; the exact sequence doesn't matter).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }

        fn range(&mut self, lo: u64, hi_inclusive: u64) -> u64 {
            lo + self.next() % (hi_inclusive - lo + 1)
        }

        fn byte(&mut self) -> u8 {
            self.next() as u8
        }
    }

    // Differential test: the SIMD implementation must agree with the
    // scalar implementation on any input. Exercises random mixtures of
    // ASCII, escapes, controls, valid and invalid UTF-8, at various
    // lengths (including chunk-boundary straddling cases).
    #[test]
    fn decode_simd_matches_scalar() {
        let mut rand = Rng(0xf00dface);

        let mut input = [0u8; 257];
        let mut out_simd = [0u32; 257];
        let mut out_scalar = [0u32; 257];

        for _ in 0..10_000 {
            let len = rand.range(0, input.len() as u64) as usize;
            let style = rand.range(0, 2);
            for b in input[..len].iter_mut() {
                *b = match style {
                    // Mostly ASCII with occasional specials.
                    0 => match rand.range(0, 20) {
                        0 => 0x1B,
                        1 => rand.range(0, 0x1F) as u8,
                        2 => rand.byte(),
                        _ => rand.range(0x20, 0x7E) as u8,
                    },
                    // Heavy multi-byte/invalid UTF-8.
                    1 => match rand.range(0, 3) {
                        0 => rand.range(0x80, 0xBF) as u8,
                        1 => rand.range(0xC0, 0xFF) as u8,
                        2 => 0x1B,
                        _ => rand.range(0x20, 0x7E) as u8,
                    },
                    // Fully random bytes.
                    _ => rand.byte(),
                };
            }

            let res_simd = utf8_decode_until_control_seq(&input[..len], &mut out_simd);
            let res_scalar = utf8_decode_until_control_seq_scalar(&input[..len], &mut out_scalar);
            assert_eq!(res_scalar.consumed, res_simd.consumed, "input={:x?}", &input[..len]);
            assert_eq!(res_scalar.decoded, res_simd.decoded, "input={:x?}", &input[..len]);
            assert_eq!(
                &out_scalar[..res_scalar.decoded],
                &out_simd[..res_simd.decoded],
                "input={:x?}",
                &input[..len]
            );
        }
    }

    /// A random codepoint whose UTF-8 encoding has `len` bytes.
    fn random_char(rand: &mut Rng, len: u64) -> char {
        loop {
            let cp = match len {
                1 => rand.range(0x20, 0x7E),
                2 => rand.range(0x80, 0x7FF),
                3 => rand.range(0x800, 0xFFFF),
                _ => rand.range(0x10000, 0x10FFFF),
            };
            if let Some(c) = char::from_u32(cp as u32) {
                return c;
            }
        }
    }

    // Differential test for the vectorized multi-byte decoder: mostly
    // valid mixed-length UTF-8 (so whole blocks take the vector path),
    // with occasional corruption, controls and escapes at random places.
    #[test]
    fn decode_multibyte_matches_scalar() {
        let mut rand = Rng(0x5eed_cafe);
        let mut out_simd = [0u32; 600];
        let mut out_scalar = [0u32; 600];

        for iter in 0..20_000 {
            let mut input: Vec<u8> = Vec::new();
            let chars = rand.range(0, 120);
            let max_len = rand.range(1, 4);
            for _ in 0..chars {
                let len = rand.range(1, max_len);
                let c = random_char(&mut rand, len);
                let mut buf = [0u8; 4];
                input.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            // Corrupt some inputs: replace bytes or truncate.
            if iter % 3 != 0 {
                for _ in 0..rand.range(1, 3) {
                    if input.is_empty() {
                        break;
                    }
                    let at = rand.range(0, input.len() as u64 - 1) as usize;
                    match rand.range(0, 4) {
                        0 => input[at] = rand.byte(),
                        1 => input[at] = 0x1B,
                        2 => input[at] = rand.range(0x80, 0xBF) as u8,
                        3 => input[at] = [0xE0, 0xED, 0xF0, 0xF4, 0xC0, 0xC1, 0xF5][rand.range(0, 6) as usize],
                        _ => input.truncate(at),
                    }
                }
            }
            let len = input.len().min(out_simd.len());

            let res_simd = utf8_decode_until_control_seq(&input[..len], &mut out_simd);
            let res_scalar = utf8_decode_until_control_seq_scalar(&input[..len], &mut out_scalar);
            assert_eq!(res_scalar.consumed, res_simd.consumed, "input={:x?}", &input[..len]);
            assert_eq!(res_scalar.decoded, res_simd.decoded, "input={:x?}", &input[..len]);
            assert_eq!(
                &out_scalar[..res_scalar.decoded],
                &out_simd[..res_simd.decoded],
                "input={:x?}",
                &input[..len]
            );
        }
    }

    // The vector decoder takes valid multi-byte blocks where AVX2 exists.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn decode_block_accepts_valid_utf8() {
        if !avx2::available() {
            return;
        }
        let s = "ünïcödé ✓ 日本語 🎉 ünïcödé ✓ 日本語 🎉 ünïcödé";
        let mut out = [0u32; avx2::MAX_OUT];
        // SAFETY: AVX2 is available, s is longer than WINDOW, out is MAX_OUT.
        let r = unsafe { avx2::decode_block(s.as_ptr(), out.as_mut_ptr()) };
        let (consumed, decoded) = r.expect("valid UTF-8 block is accepted");
        let expected: Vec<u32> = s[..consumed].chars().map(|c| c as u32).collect();
        assert_eq!(&expected[..], &out[..decoded]);
    }

    #[test]
    fn decode_matches_std_for_valid_utf8() {
        let s = "héllo wörld ✓ 日本語 🎉 abcdefghijklmnopqrstuvwxyz 0123456789 ünïcödé";
        let mut out = vec![0u32; s.len()];
        let r = utf8_decode_until_control_seq(s.as_bytes(), &mut out);
        let expected: Vec<u32> = s.chars().map(|c| c as u32).collect();
        assert_eq!(s.len(), r.consumed);
        assert_eq!(&expected[..], &out[..r.decoded]);
    }

    #[test]
    fn swar_stop_mask_test() {
        for pos in 0..8 {
            for &b in &[0x1Bu8, 0x80, 0xFF, 0xC3] {
                let mut bytes = [b'a'; 8];
                bytes[pos] = b;
                let m = swar_stop_mask(u64::from_le_bytes(bytes));
                assert_eq!(pos as u32, m.trailing_zeros());
            }
        }
        assert_eq!(0, swar_stop_mask(u64::from_le_bytes(*b"abcdefgh")));
    }

    #[test]
    fn widen_portable_matches_native() {
        let mut rand = Rng(1234);
        for _ in 0..10_000 {
            let mut chunk = [0u8; CHUNK];
            for b in chunk.iter_mut() {
                *b = match rand.range(0, 10) {
                    0 => 0x1B,
                    1 => rand.byte(),
                    _ => rand.range(0x20, 0x7E) as u8,
                };
            }
            let mut a = [0u32; CHUNK];
            let mut b = [0u32; CHUNK];
            let ma = widen_ascii_chunk(&chunk, &mut a);
            let mb = widen_ascii_chunk_portable(&chunk, &mut b);
            assert_eq!(a, b);
            assert_eq!(ma == 0, mb == 0);
            if ma != 0 {
                assert_eq!(ma.trailing_zeros(), mb.trailing_zeros());
            }
        }
    }

    #[test]
    fn decode_no_escape() {
        let mut output = [0u32; 1024];

        // TODO: many more test cases
        {
            let s = "hello".repeat(128);
            let str = s.as_bytes();
            assert_eq!(
                DecodeResult {
                    consumed: str.len(),
                    decoded: str.len(),
                },
                utf8_decode_until_control_seq(str, &mut output)
            );
        }
    }

    #[test]
    fn decode_ascii_to_escape() {
        let mut output = [0u32; 1024];

        // TODO: many more test cases
        {
            let prefix = "hello".repeat(64);
            let payload = "world".repeat(64);
            let s = format!("{prefix}\x1b{payload}");
            assert_eq!(
                DecodeResult {
                    consumed: prefix.len(),
                    decoded: prefix.len(),
                },
                utf8_decode_until_control_seq(s.as_bytes(), &mut output)
            );
        }
    }

    #[test]
    fn decode_immediate_esc_sequence() {
        let mut output = [0u32; 64];
        let str = b"\x1b[?5s";
        assert_eq!(
            DecodeResult {
                consumed: 0,
                decoded: 0,
            },
            utf8_decode_until_control_seq(str, &mut output)
        );
    }

    #[test]
    fn decode_incomplete_utf8() {
        let mut output = [0u32; 64];

        // 2-byte truncated at end of buffer
        {
            let str = b"hello\xc2";
            assert_eq!(
                DecodeResult {
                    consumed: 5,
                    decoded: 5,
                },
                utf8_decode_until_control_seq(str, &mut output)
            );
        }

        // 3-byte: \xe0 expects A0-BF next, but \x00 is not in range.
        // \xe0 is a maximal subpart of length 1 → FFFD, then \x00 is ASCII NUL.
        {
            let str = b"hello\xe0\x00";
            let result = utf8_decode_until_control_seq(str, &mut output);
            assert_eq!(7usize, result.consumed);
            assert_eq!(7usize, result.decoded);
            assert_eq!(0xFFFDu32, output[5]);
            assert_eq!(0x00u32, output[6]);
        }

        // 4-byte truncated at end of buffer (F0 90 is valid so far)
        {
            let str = b"hello\xf0\x90";
            assert_eq!(
                DecodeResult {
                    consumed: 5,
                    decoded: 5,
                },
                utf8_decode_until_control_seq(str, &mut output)
            );
        }
    }

    #[test]
    fn decode_invalid_utf8() {
        let mut output = [0u32; 64];

        // Invalid leading 2-byte sequence
        {
            let str = b"hello\xc2\x01";
            assert_eq!(
                DecodeResult {
                    consumed: 7,
                    decoded: 7,
                },
                utf8_decode_until_control_seq(str, &mut output)
            );
        }

        // Replacement will only replace the invalid leading byte.
        assert_eq!(0xFFFDu32, output[5]);
        assert_eq!(0x01u32, output[6]);
    }

    // Per the maximal subpart spec, bytes F5-FF are each replaced with FFFD.
    #[test]
    fn decode_invalid_leading_byte_is_replaced() {
        let mut output = [0u32; 64];
        {
            let str = b"hello\xFF";
            let result = utf8_decode_until_control_seq(str, &mut output);
            assert_eq!(6usize, result.consumed);
            assert_eq!(6usize, result.decoded);
            assert_eq!(0xFFFDu32, output[5]);
        }
    }

    #[test]
    fn decode_invalid_continuation_in_3_byte_sequence() {
        let mut output = [0u32; 64];

        // \xe2 expects two continuation bytes, \x28 is not one
        {
            let str = b"hello\xe2\x28world";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "hello" + replacement + "(" + "world" = 12 codepoints
            assert_eq!(12usize, result.decoded);
            assert_eq!(0xFFFDu32, output[5]);
            assert_eq!('(' as u32, output[6]);
            assert_eq!('w' as u32, output[7]);
        }
    }

    #[test]
    fn decode_invalid_continuation_in_4_byte_sequence() {
        let mut output = [0u32; 64];

        // \xf0\x90 is a valid prefix of a 4-byte sequence, but \x28 breaks it.
        // Maximal subpart is F0 90 (length 2) → single FFFD, then '(' proceeds.
        {
            let str = b"hello\xf0\x90\x28world";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "hello" + FFFD + "(" + "world" = 12 codepoints
            assert_eq!(12usize, result.decoded);
            assert_eq!(0xFFFDu32, output[5]);
            assert_eq!('(' as u32, output[6]);
            assert_eq!('w' as u32, output[7]);
        }
    }

    #[test]
    fn decode_multiple_consecutive_invalid_bytes() {
        let mut output = [0u32; 64];

        // Each lone continuation byte is its own maximal subpart → one FFFD each.
        {
            let str = b"a\x80\x80b";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "a" + FFFD + FFFD + "b" = 4 codepoints
            assert_eq!(4usize, result.decoded);
            assert_eq!('a' as u32, output[0]);
            assert_eq!(0xFFFDu32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
            assert_eq!('b' as u32, output[3]);
        }

        // C0 is an invalid lead byte (< C2), each byte gets its own FFFD.
        {
            let str = b"a\xc0\xc0b";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "a" + FFFD + FFFD + "b" = 4 codepoints
            assert_eq!(4usize, result.decoded);
            assert_eq!('a' as u32, output[0]);
            assert_eq!(0xFFFDu32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
            assert_eq!('b' as u32, output[3]);
        }
    }

    #[test]
    fn decode_unexpected_continuation_byte_as_lead() {
        let mut output = [0u32; 64];

        // 0x80 is a continuation byte appearing as a lead byte
        {
            let str = b"a\x80b";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "a" + replacement + "b" = 3 codepoints
            assert_eq!(3usize, result.decoded);
            assert_eq!('a' as u32, output[0]);
            assert_eq!(0xFFFDu32, output[1]);
            assert_eq!('b' as u32, output[2]);
        }
    }

    #[test]
    fn decode_overlong_2_byte_encoding() {
        let mut output = [0u32; 64];

        // \xc0\xaf: C0 is invalid lead (< C2) → FFFD, AF is lone continuation → FFFD
        // Per Table 3-8: C0 AF → FFFD FFFD
        {
            let str = b"a\xc0\xafb";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "a" + FFFD + FFFD + "b" = 4 codepoints
            assert_eq!(4usize, result.decoded);
            assert_eq!('a' as u32, output[0]);
            assert_eq!(0xFFFDu32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
            assert_eq!('b' as u32, output[3]);
        }
    }

    #[test]
    fn decode_surrogate_half() {
        let mut output = [0u32; 64];

        // \xed\xa0\x80 encodes U+D800 (a surrogate). Per Table 3-7, after ED
        // the next byte must be 80-9F. A0 is out of range, so ED is a maximal
        // subpart of length 1 → FFFD. Then A0 and 80 are lone continuations
        // → FFFD each. Per Table 3-9: ED A0 80 → FFFD FFFD FFFD
        {
            let str = b"a\xed\xa0\x80b";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // "a" + FFFD + FFFD + FFFD + "b" = 5 codepoints
            assert_eq!(5usize, result.decoded);
            assert_eq!('a' as u32, output[0]);
            assert_eq!(0xFFFDu32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
            assert_eq!(0xFFFDu32, output[3]);
            assert_eq!('b' as u32, output[4]);
        }
    }

    #[test]
    fn decode_valid_multibyte_surrounded_by_invalid() {
        let mut output = [0u32; 64];

        // \xc3\xa9 = é (U+00E9), surrounded by invalid continuation bytes
        {
            let str = b"\x80\xc3\xa9\x80";
            let result = utf8_decode_until_control_seq(str, &mut output);
            // replacement + é + replacement = 3 codepoints
            assert_eq!(3usize, result.decoded);
            assert_eq!(0xFFFDu32, output[0]);
            assert_eq!(0x00E9u32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
        }
    }

    #[test]
    fn decode_partial_utf8_before_escape() {
        // A valid-so-far but incomplete sequence cut off by an ESC can
        // never be completed, so it is consumed and replaced by a single
        // U+FFFD (maximal subpart) rather than left pending. Only
        // sequences cut off by the true end of input are left pending.
        let mut output = [0u32; 64];

        // 2-byte lead cut off by ESC.
        {
            let str = b"hi\xc2\x1b[0m";
            let result = utf8_decode_until_control_seq(str, &mut output);
            assert_eq!(3usize, result.consumed);
            assert_eq!(3usize, result.decoded);
            assert_eq!(0xFFFDu32, output[2]);
        }

        // 3-byte lead plus one valid continuation cut off by ESC:
        // the whole prefix is one maximal subpart, one U+FFFD.
        {
            let str = b"\xe0\xa0\x1bX";
            let result = utf8_decode_until_control_seq(str, &mut output);
            assert_eq!(2usize, result.consumed);
            assert_eq!(1usize, result.decoded);
            assert_eq!(0xFFFDu32, output[0]);
        }
    }

    #[test]
    fn decode_invalid_byte_before_escape() {
        let mut output = [0u32; 64];

        // Invalid byte followed by ESC - should replace then stop
        {
            let str = b"hi\x80\x1b[0m";
            let result = utf8_decode_until_control_seq(str, &mut output);
            assert_eq!(3usize, result.consumed);
            assert_eq!(3usize, result.decoded);
            assert_eq!('h' as u32, output[0]);
            assert_eq!('i' as u32, output[1]);
            assert_eq!(0xFFFDu32, output[2]);
        }
    }

    // Unicode Table 3-8: U+FFFD for Non-Shortest Form Sequences
    // Bytes:  C0  AF  E0  80  BF  F0  81  82  41
    // Output: FFFD FFFD FFFD FFFD FFFD FFFD FFFD FFFD 0041
    #[test]
    fn table_3_8_non_shortest_form_sequences() {
        let mut output = [0u32; 64];

        let str = b"\xC0\xAF\xE0\x80\xBF\xF0\x81\x82\x41";
        let result = utf8_decode_until_control_seq(str, &mut output);
        assert_eq!(9usize, result.consumed);
        assert_eq!(9usize, result.decoded);
        for o in &output[..8] {
            assert_eq!(0xFFFDu32, *o);
        }
        assert_eq!(0x41u32, output[8]);
    }

    // Unicode Table 3-9: U+FFFD for Ill-Formed Sequences for Surrogates
    // Bytes:  ED  A0  80  ED  BF  BF  ED  AF  41
    // Output: FFFD FFFD FFFD FFFD FFFD FFFD FFFD FFFD 0041
    #[test]
    fn table_3_9_surrogate_sequences() {
        let mut output = [0u32; 64];

        let str = b"\xED\xA0\x80\xED\xBF\xBF\xED\xAF\x41";
        let result = utf8_decode_until_control_seq(str, &mut output);
        assert_eq!(9usize, result.consumed);
        assert_eq!(9usize, result.decoded);
        for o in &output[..8] {
            assert_eq!(0xFFFDu32, *o);
        }
        assert_eq!(0x41u32, output[8]);
    }

    // Unicode Table 3-10: U+FFFD for Other Ill-Formed Sequences
    // Bytes:  F4  91  92  93  FF  41  80  BF  42
    // Output: FFFD FFFD FFFD FFFD FFFD 0041 FFFD FFFD 0042
    #[test]
    fn table_3_10_other_ill_formed_sequences() {
        let mut output = [0u32; 64];

        let str = b"\xF4\x91\x92\x93\xFF\x41\x80\xBF\x42";
        let result = utf8_decode_until_control_seq(str, &mut output);
        assert_eq!(9usize, result.consumed);
        assert_eq!(9usize, result.decoded);
        assert_eq!(0xFFFDu32, output[0]); // F4
        assert_eq!(0xFFFDu32, output[1]); // 91
        assert_eq!(0xFFFDu32, output[2]); // 92
        assert_eq!(0xFFFDu32, output[3]); // 93
        assert_eq!(0xFFFDu32, output[4]); // FF
        assert_eq!(0x0041u32, output[5]); // 41
        assert_eq!(0xFFFDu32, output[6]); // 80
        assert_eq!(0xFFFDu32, output[7]); // BF
        assert_eq!(0x0042u32, output[8]); // 42
    }

    // Unicode Table 3-11: U+FFFD for Truncated Sequences
    // Bytes:  E1  80  E2  F0  91  92  F1  BF  41
    // Output: FFFD     FFFD    FFFD         FFFD     0041
    #[test]
    fn table_3_11_truncated_sequences() {
        let mut output = [0u32; 64];

        let str = b"\xE1\x80\xE2\xF0\x91\x92\xF1\xBF\x41";
        let result = utf8_decode_until_control_seq(str, &mut output);
        assert_eq!(9usize, result.consumed);
        assert_eq!(5usize, result.decoded);
        assert_eq!(0xFFFDu32, output[0]); // E1 80 (truncated 3-byte)
        assert_eq!(0xFFFDu32, output[1]); // E2 (truncated 3-byte, next byte F0 not continuation)
        assert_eq!(0xFFFDu32, output[2]); // F0 91 92 (truncated 4-byte)
        assert_eq!(0xFFFDu32, output[3]); // F1 BF (truncated 4-byte, next byte 41 not continuation)
        assert_eq!(0x0041u32, output[4]); // 41
    }
}
