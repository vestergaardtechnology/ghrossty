//! Port of `src/simd/base64.zig`.
//!
//! STATUS: ported
//!
//! Deviation: the Zig SIMD build calls simdutf (`base64.cpp`) for `maxLen`
//! and `decode`. This port decodes with the Zig scalar path
//! (`base64_scalar.rs`) and emulates simdutf's whitespace skipping on top of
//! it. On x86-64 CPUs with AVX2 (detected at runtime), inputs made only of
//! alphabet characters decode 32 characters per step with a vectorized
//! decoder instead; its results are identical to the scalar path, and any
//! other input takes the scalar path.

use std::fmt;

use super::base64_scalar;
use super::base64_scalar::SCALAR_DECODER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Base64Invalid,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Base64Invalid")
    }
}

impl std::error::Error for DecodeError {}

pub fn max_len(input: &[u8]) -> usize {
    // The Ghostty application builds with `options.simd`, which uses
    // simdutf's `maximal_binary_length_from_base64`. Mirror that formula:
    // it ignores trailing padding and does not account for whitespace,
    // so it is an upper bound.
    let mut padding = 0;
    if input.last() == Some(&b'=') {
        padding += 1;
        if input.len() >= 2 && input[input.len() - 2] == b'=' {
            padding += 1;
        }
    }
    let actual = input.len() - padding;
    if actual % 4 <= 1 {
        actual / 4 * 3
    } else {
        actual / 4 * 3 + actual % 4 - 1
    }
}

fn max_len_scalar(input: &[u8]) -> usize {
    match SCALAR_DECODER.calc_size_for_slice(scalar_input(input)) {
        Ok(v) => v,
        Err(err) => {
            log::warn!(target: "simd_base64", "failed to calculate base64 size for payload: {}", err);
            0
        }
    }
}

pub fn decode<'a>(input: &[u8], output: &'a mut [u8]) -> Result<&'a [u8], DecodeError> {
    // Fast path: input that is only alphabet characters plus trailing
    // padding (the common case) decodes vectorized. It returns None for
    // anything else, which then takes the path below unchanged.
    if let Some(len) = decode_vectorized(input, output) {
        return Ok(&output[..len]);
    }

    // The SIMD build uses simdutf's `base64_to_binary` (default options),
    // which skips ASCII whitespace and accepts a partial final chunk.
    // Emulate that on top of the scalar decoder.
    const fn is_ws(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
    }
    if !input.iter().any(|&c| is_ws(c)) {
        return decode_scalar(input, output);
    }
    let filtered: Vec<u8> = input.iter().copied().filter(|&c| !is_ws(c)).collect();
    let len = decode_scalar(&filtered, output)?.len();
    Ok(&output[..len])
}

fn decode_scalar<'a>(input_raw: &[u8], output: &'a mut [u8]) -> Result<&'a [u8], DecodeError> {
    let input = scalar_input(input_raw);
    let size = max_len_scalar(input);
    if size == 0 {
        return Ok(&output[..0]);
    }
    debug_assert!(output.len() >= size);
    SCALAR_DECODER
        .decode(output, scalar_input(input))
        .map_err(|_| DecodeError::Base64Invalid)?;
    Ok(&output[..size])
}

/// Decode `input` like `decode_scalar` would when the input, with its
/// trailing padding removed, is made only of alphabet characters. Returns
/// the decoded length, or None (having possibly written to `output`) when
/// the vectorized decoder is unavailable or the input is anything else:
/// whitespace, invalid characters, a length the scalar path rejects or
/// treats specially, or a too-short output.
///
/// When this returns Some, `decode_scalar(input)` returns the same bytes:
/// it sees no whitespace (`decode` would not filter), and the scalar
/// decoder fails only on non-alphabet characters, which this rejects.
fn decode_vectorized(input: &[u8], output: &mut [u8]) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        // Short inputs are not worth the setup; leave them to the scalar
        // path.
        if input.len() < 64 || !std::arch::is_x86_feature_detected!("avx2") {
            return None;
        }
        let src = scalar_input(input);
        // A single leftover character makes the scalar path log and return
        // an empty result; leave that to it.
        if src.len() % 4 == 1 {
            return None;
        }
        let size = src.len() / 4 * 3 + src.len() % 4 * 3 / 4;
        if output.len() < size {
            return None;
        }
        let dest = &mut output[..size];
        // SAFETY: AVX2 support was checked above.
        let (read, written) = unsafe { avx2::decode(src, dest)? };
        // The remainder (fewer than 32 characters, starting at a group
        // boundary) takes the scalar decoder, which writes exactly the
        // rest of `dest`.
        SCALAR_DECODER.decode(&mut dest[written..], &src[read..]).ok()?;
        Some(size)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (input, output);
        None
    }
}

/// AVX2 base64 decoding of 32 characters per step: the nibble-lookup
/// validation and translation from Wojciech Muła and Daniel Lemire,
/// "Faster Base64 Encoding and Decoding using AVX2 Instructions" (2018),
/// the same algorithm simdutf's AVX2 kernel uses.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    /// Decode whole 32 character blocks of `src` into `dest`. Returns how
    /// many characters were read and bytes written (a multiple of 32 and
    /// 24), or None if a block has a non-alphabet character.
    #[target_feature(enable = "avx2")]
    pub unsafe fn decode(src: &[u8], dest: &mut [u8]) -> Option<(usize, usize)> {
        // Per low nibble: the high nibbles that are invalid with it, as
        // bits of `lut_hi`. Bit 0x10 covers the high nibbles that are
        // never valid.
        let lut_lo = _mm256_setr_epi8(
            0x15, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x13, 0x1A, 0x1B, 0x1B, 0x1B, 0x1A, //
            0x15, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x13, 0x1A, 0x1B, 0x1B, 0x1B, 0x1A,
        );
        // Per high nibble: its bit for `lut_lo`.
        let lut_hi = _mm256_setr_epi8(
            0x10, 0x10, 0x01, 0x02, 0x04, 0x08, 0x04, 0x08, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, //
            0x10, 0x10, 0x01, 0x02, 0x04, 0x08, 0x04, 0x08, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        );
        // Per high nibble (minus one for '/'): the offset from the
        // character to its 6-bit value.
        let lut_roll = _mm256_setr_epi8(
            0, 16, 19, 4, -65, -65, -71, -71, 0, 0, 0, 0, 0, 0, 0, 0, //
            0, 16, 19, 4, -65, -65, -71, -71, 0, 0, 0, 0, 0, 0, 0, 0,
        );
        let nibble = _mm256_set1_epi8(0x0F);
        let slash = _mm256_set1_epi8(b'/' as i8);
        // Pack the four 6-bit values of each 32-bit lane into 24 bits,
        // then the 24-bit groups into the low 12 bytes of each 128-bit
        // half, then both halves into the low 24 bytes.
        let merge_ab_bc = _mm256_set1_epi32(0x0140_0140);
        let merge_abc = _mm256_set1_epi32(0x0001_1000);
        let pack_lanes = _mm256_setr_epi8(
            2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -1, -1, -1, -1, //
            2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -1, -1, -1, -1,
        );
        let pack_halves = _mm256_setr_epi32(0, 1, 2, 4, 5, 6, 0, 0);

        let mut read = 0;
        let mut written = 0;
        while src.len() - read >= 32 && dest.len() - written >= 24 {
            // SAFETY: 32 bytes are in bounds (checked above).
            let input = unsafe { _mm256_loadu_si256(src.as_ptr().add(read).cast()) };
            let hi_nibbles = _mm256_and_si256(_mm256_srli_epi32(input, 4), nibble);
            let lo_nibbles = _mm256_and_si256(input, nibble);
            let lo = _mm256_shuffle_epi8(lut_lo, lo_nibbles);
            let hi = _mm256_shuffle_epi8(lut_hi, hi_nibbles);
            if _mm256_testz_si256(lo, hi) == 0 {
                return None;
            }
            let eq_slash = _mm256_cmpeq_epi8(input, slash);
            let roll = _mm256_shuffle_epi8(lut_roll, _mm256_add_epi8(eq_slash, hi_nibbles));
            let values = _mm256_add_epi8(input, roll);

            let merged = _mm256_maddubs_epi16(values, merge_ab_bc);
            let merged = _mm256_madd_epi16(merged, merge_abc);
            let packed = _mm256_shuffle_epi8(merged, pack_lanes);
            let packed = _mm256_permutevar8x32_epi32(packed, pack_halves);

            if dest.len() - written >= 32 {
                // SAFETY: 32 bytes are in bounds; the last 8 are
                // overwritten by the next block or the remainder.
                unsafe { _mm256_storeu_si256(dest.as_mut_ptr().add(written).cast(), packed) };
            } else {
                let mut tmp = [0u8; 32];
                // SAFETY: `tmp` is 32 bytes.
                unsafe { _mm256_storeu_si256(tmp.as_mut_ptr().cast(), packed) };
                dest[written..written + 24].copy_from_slice(&tmp[..24]);
            }
            read += 32;
            written += 24;
        }
        Some((read, written))
    }
}

/// For non-SIMD enabled builds, we trim the padding from the end of the
/// base64 input in order to get identical output with the SIMD version.
fn scalar_input(input: &[u8]) -> &[u8] {
    let mut end = input.len();
    while end > 0 && input[end - 1] == b'=' {
        end -= 1;
    }
    &input[..end]
}

/// Whether strict decoding requires the input to be padded to a
/// multiple of four bytes (RFC 4648 section 3.2). The Kitty clipboard
/// protocol requires padding; the legacy OSC 52 protocol tolerates a
/// missing-padding tail because it has no way to report errors to the
/// client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Padding {
    Required,
    Optional,
}

/// Decode strict RFC 4648 standard-alphabet base64: characters outside
/// the alphabet (including whitespace) and misplaced padding are
/// errors rather than being skipped, and padding is validated per the
/// given requirement. This is the decoding the Kitty clipboard
/// protocol specifies:
/// https://sw.kovidgoyal.net/kitty/clipboard/#encoding-of-payloads
///
/// The output must be at least max_len(input) bytes.
pub fn decode_strict<'a>(
    input: &[u8],
    output: &'a mut [u8],
    padding: Padding,
) -> Result<&'a [u8], DecodeError> {
    // Padding can only be a suffix of at most two bytes; any '='
    // elsewhere is rejected by the underlying decode.
    let mut pad: usize = 0;
    if !input.is_empty() && input[input.len() - 1] == b'=' {
        pad += 1;
    }
    if input.len() > 1 && input[input.len() - 2] == b'=' {
        pad += 1;
    }

    match padding {
        Padding::Required => {
            if input.len() % 4 != 0 {
                return Err(DecodeError::Base64Invalid);
            }
        }
        // Present padding must still complete a four byte group; only
        // fully absent padding is tolerated. A single leftover byte
        // can never carry a decodable value.
        Padding::Optional => {
            if pad > 0 {
                if input.len() % 4 != 0 {
                    return Err(DecodeError::Base64Invalid);
                }
            } else if input.len() % 4 == 1 {
                return Err(DecodeError::Base64Invalid);
            }
        }
    }

    // The permissive decode already rejects every invalid character
    // except whitespace, which simdutf silently skips. Skipped
    // characters make the decoded length fall short of the exact
    // length the input length implies, so comparing the two rejects
    // whitespace without a separate validation pass over the input.
    let decoded = decode(input, output).map_err(|_| DecodeError::Base64Invalid)?;
    let expected = match input.len() % 4 {
        0 => input.len() / 4 * 3 - pad,
        2 => input.len() / 4 * 3 + 1,
        3 => input.len() / 4 * 3 + 2,
        _ => unreachable!(), // rejected above
    };
    if decoded.len() != expected {
        return Err(DecodeError::Base64Invalid);
    }
    Ok(decoded)
}

/// A streaming strict base64 decoder for one logical payload split
/// across multiple chunks at arbitrary byte boundaries (e.g. the Kitty
/// clipboard protocol's wdata packets): the concatenation of the fed
/// chunks must be valid RFC 4648 standard-alphabet base64.
///
/// Padding is terminal within a feed: a padded group followed by more
/// data in the same feed is an error. A feed that ends exactly at
/// terminal padding resets the decoder so the next feed starts a
/// fresh stream, exactly like the kitty reference implementation
/// (which resets its aklomp streaming decoder on EOF); this keeps
/// clients that pad each chunk independently working.
#[derive(Debug, Clone, Copy, Default)]
pub struct Streaming {
    /// Partial group carried between feeds; a group only decodes once
    /// all four of its characters have arrived.
    carry: [u8; 4],
    /// Zig: u3
    carry_len: u8,
}

impl Streaming {
    /// Maximum decoded bytes one feed of input can produce.
    pub fn max_len(&self, input: &[u8]) -> usize {
        (self.carry_len as usize + input.len()) / 4 * 3
    }

    /// Decode the complete groups of input (with any carried bytes
    /// prepended) into output, which must be at least max_len(input)
    /// bytes, and carry the remainder for the next feed.
    pub fn feed<'a>(&mut self, input: &[u8], output: &'a mut [u8]) -> Result<&'a [u8], DecodeError> {
        debug_assert!(output.len() >= self.max_len(input));
        if input.is_empty() {
            return Ok(&output[..0]);
        }

        let mut rem = input;
        let mut written: usize = 0;

        // Complete a carried partial group first.
        if self.carry_len > 0 {
            let carry_len = self.carry_len as usize;
            let take = (4 - carry_len).min(rem.len());
            for (pos, &c) in (carry_len..).zip(rem[..take].iter()) {
                if !valid_partial_char(pos, c) {
                    return Err(DecodeError::Base64Invalid);
                }
            }
            self.carry[carry_len..carry_len + take].copy_from_slice(&rem[..take]);
            self.carry_len += take as u8;
            rem = &rem[take..];
            if self.carry_len < 4 {
                return Ok(&output[..0]);
            }
            self.carry_len = 0;
            let (decoded, padded) = group(self.carry, output)?;
            written += decoded;
            if padded {
                if !rem.is_empty() {
                    return Err(DecodeError::Base64Invalid);
                }
                return Ok(&output[..written]);
            }
        }

        // Bulk-decode all complete groups with the strict single-shot
        // decode, which also enforces that padding only appears as a
        // terminal suffix. Terminal padding must then end the feed.
        let bulk = &rem[..rem.len() - (rem.len() % 4)];
        written += decode_strict(bulk, &mut output[written..], Padding::Required)?.len();
        if !bulk.is_empty() && bulk[bulk.len() - 1] == b'=' {
            if bulk.len() != rem.len() {
                return Err(DecodeError::Base64Invalid);
            }
            return Ok(&output[..written]);
        }

        // Carry the trailing partial group, validated eagerly so
        // garbage is reported on the feed that contains it.
        let tail = &rem[bulk.len()..];
        for (pos, &c) in tail.iter().enumerate() {
            if !valid_partial_char(pos, c) {
                return Err(DecodeError::Base64Invalid);
            }
        }
        self.carry[..tail.len()].copy_from_slice(tail);
        self.carry_len = tail.len() as u8;
        Ok(&output[..written])
    }

    /// Finish the stream: the concatenation of the fed chunks must
    /// have formed complete groups, so a carried partial group is an
    /// error (the stream was not correctly padded). The decoder is
    /// ready for a fresh stream afterwards either way.
    pub fn finish(&mut self) -> Result<(), DecodeError> {
        let carry_len = self.carry_len;
        *self = Streaming::default();
        if carry_len != 0 {
            return Err(DecodeError::Base64Invalid);
        }
        Ok(())
    }
}

/// Decode one complete four character group, the only place
/// padding is legal: the last two characters may be '=' ('=' in
/// the second-to-last position requires it in the last). Returns
/// the decoded length and whether the group was padded.
fn group(g: [u8; 4], output: &mut [u8]) -> Result<(usize, bool), DecodeError> {
    let padded = g[3] == b'=';
    let decoded = decode_strict(&g, output, Padding::Required)?;
    Ok((decoded.len(), padded))
}

/// Whether c is valid at the given position of a partial group
/// still waiting for its remaining characters: the first two
/// positions must be alphabet characters and only the last two
/// may open the padding suffix.
fn valid_partial_char(pos: usize, c: u8) -> bool {
    base64_scalar::is_alphabet_char(c) || (pos >= 2 && c == b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_skips_whitespace_like_simdutf() {
        let input = b"aGVs\nbG8g\r\nd29y bGQ=";
        let mut out = vec![0u8; max_len(input)];
        assert_eq!(decode(input, &mut out).unwrap(), b"hello world");
        assert_eq!(max_len(b"aGVsbG8="), 5);
        assert_eq!(max_len(b"aGVsbG8"), 5);
    }

    /// A small deterministic PRNG for the randomized tests.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// The scalar-only `decode`, as the port did before the vectorized
    /// fast path: the reference the fast path must match exactly.
    fn decode_reference(input: &[u8], output: &mut [u8]) -> Result<usize, DecodeError> {
        const fn is_ws(c: u8) -> bool {
            matches!(c, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
        }
        if !input.iter().any(|&c| is_ws(c)) {
            return decode_scalar(input, output).map(|v| v.len());
        }
        let filtered: Vec<u8> = input.iter().copied().filter(|&c| !is_ws(c)).collect();
        decode_scalar(&filtered, output).map(|v| v.len())
    }

    #[test]
    fn decode_vectorized_matches_scalar() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for iter in 0..20_000 {
            let len = (rng.next() % 300) as usize;
            let mut input: Vec<u8> = (0..len).map(|_| ALPHABET[(rng.next() % 64) as usize]).collect();
            // Padding, whitespace, any other byte, or several of them.
            match iter % 8 {
                0..=2 => {}
                3 => input.extend(std::iter::repeat_n(b'=', (rng.next() % 3) as usize)),
                _ => {
                    for _ in 0..1 + rng.next() % 3 {
                        if input.is_empty() {
                            break;
                        }
                        let at = (rng.next() as usize) % input.len();
                        input[at] = match rng.next() % 4 {
                            0 => b"\n\r\t \x0c"[(rng.next() % 5) as usize],
                            1 => b'=',
                            _ => rng.next() as u8,
                        };
                    }
                }
            }

            let mut expect = vec![0xAAu8; max_len(&input) + 64];
            let mut got = expect.clone();
            let want = decode_reference(&input, &mut expect);
            let have = decode(&input, &mut got).map(|v| v.len());
            assert_eq!(want, have, "input {:?}", String::from_utf8_lossy(&input));
            if let Ok(n) = want {
                assert_eq!(expect[..n], got[..n], "input {:?}", String::from_utf8_lossy(&input));
            }
        }
    }

    #[test]
    fn decode_vectorized_exact_output() {
        // Every output length up to 200 bytes, decoded into a buffer of
        // exactly max_len bytes: the vectorized stores must stay in bounds
        // and the result must be the encoded data.
        let mut rng = XorShift(42);
        for bytes in 0..200usize {
            let data: Vec<u8> = (0..bytes).map(|_| rng.next() as u8).collect();
            let mut encoded = Vec::new();
            for chunk in data.chunks(3) {
                let mut b = [0u8; 3];
                b[..chunk.len()].copy_from_slice(chunk);
                let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
                for i in 0..chunk.len() + 1 {
                    encoded.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]);
                }
                encoded.extend(std::iter::repeat_n(b'=', 3 - chunk.len()));
            }
            let mut out = vec![0u8; max_len(&encoded)];
            assert_eq!(&data[..], decode(&encoded, &mut out).unwrap());

            // The vectorized path really ran where it is available.
            #[cfg(target_arch = "x86_64")]
            if encoded.len() >= 64 && std::arch::is_x86_feature_detected!("avx2") {
                let mut out = vec![0u8; max_len(&encoded)];
                assert_eq!(Some(data.len()), decode_vectorized(&encoded, &mut out));
                assert_eq!(&data[..], &out[..]);
            }
        }
    }

    #[test]
    fn base64_max_len() {
        let len = max_len(b"aGVsbG8gd29ybGQ=");
        assert_eq!(11, len);
    }

    #[test]
    fn base64_empty_input() {
        let mut output: [u8; 0] = [];
        assert_eq!(0, max_len(b""));
        assert_eq!(b"", decode(b"", &mut output).unwrap());
    }

    #[test]
    fn base64_decode() {
        let input = b"aGVsbG8gd29ybGQ=";
        let len = max_len(input);
        let mut output = vec![0u8; len];
        let s = decode(input, &mut output).unwrap();
        assert_eq!(b"hello world", s);
    }

    #[test]
    fn base64_strict_decode_valid() {
        let mut output = [0u8; 128];
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"aGVsbG8gd29ybGQ=", b"hello world"),
            (b"bGlnaHQgdw==", b"light w"),
            (b"Zm9vYmFy", b"foobar"),
            // Every alphabet character in one input.
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
                b"\x00\x10\x83\x10\x51\x87\x20\x92\x8b\x30\xd3\x8f\x41\x14\x93\x51\x55\x97\x61\x96\x9b\x71\xd7\x9f\x82\x18\xa3\x92\x59\xa7\xa2\x9a\xab\xb2\xdb\xaf\xc3\x1c\xb3\xd3\x5d\xb7\xe3\x9e\xbb\xf3\xdf\xbf",
            ),
        ];
        for (input, expect) in cases {
            assert_eq!(*expect, decode_strict(input, &mut output, Padding::Required).unwrap());
            assert_eq!(*expect, decode_strict(input, &mut output, Padding::Optional).unwrap());
        }
    }

    #[test]
    fn base64_strict_decode_invalid() {
        let mut output = [0u8; 128];

        // The invalid inputs from kitty's own strict decoding tests plus
        // some extra padding-placement cases. All of these are invalid for
        // both padding requirements.
        let cases: &[&[u8]] = &[
            b"bGlnaHQgdw=",    // missing one padding byte
            b"bGln!!Qgdw==",   // invalid characters
            b"bGlnaHQgdw==\n", // trailing whitespace
            b"\nbGlnaHQgdw==", // leading whitespace
            b"bGlnaHQg dw==",  // interior whitespace
            b"!!!!",
            b"=",
            b"==",
            b"A===",
            b"AB=C", // padding must be the suffix
            b"Zm9v YmFy",
        ];
        for case in cases {
            assert_eq!(
                Err(DecodeError::Base64Invalid),
                decode_strict(case, &mut output, Padding::Required)
            );
            assert_eq!(
                Err(DecodeError::Base64Invalid),
                decode_strict(case, &mut output, Padding::Optional)
            );
        }

        // Unpadded input is only tolerated when padding is optional. A
        // single leftover character can never be decoded.
        assert_eq!(
            Err(DecodeError::Base64Invalid),
            decode_strict(b"bGlnaHQgdw", &mut output, Padding::Required)
        );
        assert_eq!(
            b"light w",
            decode_strict(b"bGlnaHQgdw", &mut output, Padding::Optional).unwrap()
        );
        assert_eq!(
            Err(DecodeError::Base64Invalid),
            decode_strict(b"bGl", &mut output, Padding::Required)
        );
        assert_eq!(b"li", decode_strict(b"bGl", &mut output, Padding::Optional).unwrap());
        assert_eq!(
            Err(DecodeError::Base64Invalid),
            decode_strict(b"bGlna", &mut output, Padding::Optional)
        );
    }

    #[test]
    fn base64_streaming_decode_chunk_boundaries() {
        // Decoding a stream split at every possible boundary, including
        // one byte at a time, matches the single-shot decode.
        let input = b"c29tZSBsb25nZXIgZGF0YSB3aXRoIHBhZGRpbmc+Pz8=";
        let expect = b"some longer data with padding>??";
        for split in 0..input.len() + 1 {
            let mut s = Streaming::default();
            let mut result: Vec<u8> = Vec::new();
            let mut output = [0u8; 64];
            result.extend_from_slice(s.feed(&input[..split], &mut output).unwrap());
            result.extend_from_slice(s.feed(&input[split..], &mut output).unwrap());
            s.finish().unwrap();
            assert_eq!(&expect[..], &result[..]);
        }
        {
            let mut s = Streaming::default();
            let mut result: Vec<u8> = Vec::new();
            let mut output = [0u8; 4];
            for i in 0..input.len() {
                result.extend_from_slice(s.feed(&input[i..i + 1], &mut output).unwrap());
            }
            s.finish().unwrap();
            assert_eq!(&expect[..], &result[..]);
        }
    }

    #[test]
    fn base64_streaming_decode_invalid() {
        let mut output = [0u8; 64];
        let invalid = Err(DecodeError::Base64Invalid);

        // Invalid characters are rejected wherever they appear.
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"!!!!", &mut output).map(|v| v.to_vec()));
        }
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"SGVs!!!bG8=", &mut output).map(|v| v.to_vec()));
        }
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"\nc29tZSBkYXRh", &mut output).map(|v| v.to_vec()));
        }

        // Data after terminal padding within one feed is rejected, even
        // when the padded group only completes in that feed.
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"Z29vZA==SGVsbG8=", &mut output).map(|v| v.to_vec()));
        }
        {
            let mut s = Streaming::default();
            s.feed(b"Z29vZA=", &mut output).unwrap();
            assert_eq!(invalid, s.feed(b"=SGVs", &mut output).map(|v| v.to_vec()));
        }

        // A feed that ends exactly at terminal padding resets the stream:
        // the next feed starts fresh, so clients that pad every chunk
        // independently keep working (matching the kitty implementation,
        // which resets its streaming decoder on EOF).
        {
            let mut s = Streaming::default();
            assert_eq!(b"good", s.feed(b"Z29vZA==", &mut output).unwrap());
            assert_eq!(b"Hello", s.feed(b"SGVsbG8=", &mut output).unwrap());
            s.finish().unwrap();
        }
        {
            // Padding split across feeds resets too.
            let mut s = Streaming::default();
            assert_eq!(b"goo", s.feed(b"Z29vZA=", &mut output).unwrap());
            assert_eq!(b"d", s.feed(b"=", &mut output).unwrap());
            assert_eq!(b"more", s.feed(b"bW9yZQ==", &mut output).unwrap());
            s.finish().unwrap();
        }

        // Misplaced padding within a group.
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"YQ=X", &mut output).map(|v| v.to_vec()));
        }
        {
            let mut s = Streaming::default();
            assert_eq!(invalid, s.feed(b"=AAA", &mut output).map(|v| v.to_vec()));
        }

        // A stream that ends in a partial group is missing its padding.
        // The failed finish resets the decoder for the next stream.
        {
            let mut s = Streaming::default();
            assert_eq!(b"Hel", s.feed(b"SGVsbG8", &mut output).unwrap());
            assert_eq!(Err(DecodeError::Base64Invalid), s.finish());
            assert_eq!(b"Hel", s.feed(b"SGVsbG8", &mut output).unwrap());
            assert_eq!(b"lo", s.feed(b"=", &mut output).unwrap());
            s.finish().unwrap();
        }
    }
}
