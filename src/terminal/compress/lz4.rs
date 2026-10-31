//! Port of `src/terminal/compress/lz4.zig`.
//!
//! STATUS: ported
//!
//! An allocation-free implementation of the raw LZ4 block format.
//!
//! LZ4 has two relevant layers: the block format describes the compressed
//! bytes, while the frame format adds headers, sizes, checksums, and support
//! for a stream of blocks. Terminal pages already have their own ownership
//! and metadata, so this implements only blocks. In particular, an encoded
//! block does not contain its decompressed size. The caller must store that
//! separately and provide an exactly sized buffer when decoding.
//!
//! A block is a series of sequences. Each non-final sequence has this shape:
//!
//!     token | literal length extensions | literals | offset | match length extensions
//!
//! The token's high nibble contains the literal length and its low nibble
//! contains the match length minus four. A nibble value of 15 means that the
//! length continues in extension bytes at the corresponding point in the
//! sequence. Each extension byte adds to the length; a value of 255 means
//! another byte follows. The literal bytes are copied directly. The two-byte
//! little-endian offset then points backwards in the already decompressed
//! output to the match bytes.
//!
//! The last sequence is special: it contains literals only and ends directly
//! after them. The reference format also requires the last five input bytes to
//! be literals and the final match to begin at least twelve bytes before the
//! end of the input. The compressor observes these restrictions so its output
//! can be consumed by optimized LZ4 decoders which copy in larger units.
//!
//! Compression uses the fast LZ4 strategy: hash each four-byte input sequence
//! and test recent positions as match candidates. Two 16-bit positions fit in
//! each hash-table entry because the format cannot refer further than 64 KiB
//! backwards. Retaining the displaced position recovers useful matches after
//! hash collisions without increasing the fixed 16 KiB workspace. Long runs
//! without matches gradually skip input positions, while matching runs are
//! extended a machine word at a time. The implementation allocates nothing;
//! all input, output, and scratch memory is supplied by the caller.
//!
//! Format reference:
//! https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md

use core::fmt;

/// Maximum input accepted by the reference LZ4 block API. Keeping the same
/// limit means `compressBound` fits in the integer sizes used by LZ4 callers
/// and gives us the same compatibility boundary as other implementations.
pub const MAX_INPUT_SIZE: usize = 0x7E00_0000;

/// Every LZ4 match represents at least four bytes. The token stores the number
/// of bytes beyond this minimum rather than the full match length.
const MIN_MATCH: usize = 4;

/// Number of bytes at the end of a conforming block which must remain literals.
const LAST_LITERALS: usize = 5;

/// A match may not begin in the final 12 bytes. This leaves enough room for the
/// minimum match and the required five trailing literals.
const MATCH_FIND_LIMIT: usize = 12;

/// We retain one input position for each 12-bit hash. LZ4 refers to this as
/// memory usage 14 because the 4096 entries are four bytes each (16 KiB).
const HASH_LOG: u32 = 12;

/// Multiplicative hash used by the reference LZ4 fast compressor. The high
/// `hash_log` bits provide the table index.
const HASH_MULTIPLIER: u32 = 2_654_435_761;

/// Scratch memory used while compressing one block. Each entry packs the low
/// 16 bits of the two most recent input positions for its hash. All-ones marks
/// an empty half; LZ4's 16-bit offset is enough to reconstruct the only useful
/// preceding address from the current position.
/// The table is reset by every call to `compress` and can be reused afterwards.
pub type HashTable = [u32; 1 << HASH_LOG];

/// Errors which can occur while encoding a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressError {
    /// The input exceeds the maximum size supported by the block compressor.
    InputTooLarge,

    /// The provided output buffer cannot hold the encoded block.
    OutputTooSmall,
}

/// Errors which can occur while decoding a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompressError {
    /// The encoded block ended in the middle of a sequence.
    TruncatedInput,

    /// A match offset was zero or pointed before the produced output.
    InvalidOffset,

    /// A sequence would write beyond the provided output buffer.
    OutputTooSmall,

    /// The block ended before filling the exact-size output buffer.
    OutputSizeMismatch,
}

impl fmt::Display for CompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
impl std::error::Error for CompressError {}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
impl std::error::Error for DecompressError {}

/// Return the maximum number of bytes needed to encode `input_len` bytes.
///
/// Incompressible input is represented as one literal run. Every 255 literal
/// bytes can require one extension byte. The additional 16-byte margin covers
/// the token and the format's fixed overhead. Callers can allocate this amount
/// once and reuse it for any block no larger than `input_len`.
pub fn compress_bound(input_len: usize) -> Result<usize, CompressError> {
    if input_len > MAX_INPUT_SIZE {
        return Err(CompressError::InputTooLarge);
    }
    Ok(input_len + input_len / 255 + 16)
}

/// Compress `input` into a raw LZ4 block in `output`.
///
/// Returns the initialized length of `output`. The input and output buffers
/// must not overlap. `table` is scratch space and does not need to be
/// initialized by the caller; it is reset before use.
pub fn compress(input: &[u8], output: &mut [u8], table: &mut HashTable) -> Result<usize, CompressError> {
    if input.len() > MAX_INPUT_SIZE {
        return Err(CompressError::InputTooLarge);
    }

    // All-ones in either packed half means "no previous position".
    table.fill(u32::MAX);

    // `ip` is the current input position, `anchor` is the first literal not yet
    // emitted, and `op` is the next output position. A successful match emits
    // input[anchor..ip] as literals followed by the match, then moves both input
    // positions to the end of that match.
    let mut op: usize = 0;
    let mut anchor: usize = 0;

    // LZ4's format leaves the final five input bytes as literals and starts
    // the final match at least twelve bytes before the end. This is not
    // required by our safe decoder, but makes blocks compatible with fast
    // decoders that rely on the standard format restrictions. Inputs too
    // short for any match are emitted below as one literal-only sequence.
    if input.len() >= MATCH_FIND_LIMIT {
        let search_end = input.len() - MATCH_FIND_LIMIT;
        let match_end_limit = input.len() - LAST_LITERALS;
        let mut ip: usize = 0;
        let mut search_attempts: usize = 0;

        'search: while ip <= search_end {
            // Hash the next four bytes and replace the table entry
            // immediately. Hash collisions are expected, so equality is
            // checked below before accepting a saved position as a match.
            // SAFETY: ip <= search_end, so ip + 4 <= input.len().
            let sequence = unsafe { read_u32_unchecked(input, ip) };
            let hash = hash_sequence(sequence);
            let candidates = table[hash];
            remember_position(table, hash, ip);

            let mut match_pos = match candidate_position(ip, candidates as u16)
                .or_else(|| candidate_position(ip, (candidates >> 16) as u16))
            {
                Some(v) => v,
                None => {
                    advance_search(&mut ip, anchor, &mut search_attempts);
                    continue 'search;
                }
            };

            // SAFETY: candidates precede ip (see candidate_position).
            if unsafe { read_u32_unchecked(input, match_pos) } != sequence {
                // The nearest candidate collided. Fall back to the older
                // one, which must additionally match one byte beyond the
                // minimum so collision-prone minimum-length matches from
                // the stale half are not emitted.
                let older = candidate_position(ip, (candidates >> 16) as u16).filter(|&older| {
                    // SAFETY: older < ip <= search_end, so both reads end
                    // at least MATCH_FIND_LIMIT - MIN_MATCH - 1 bytes
                    // before the end of the input.
                    unsafe {
                        read_u32_unchecked(input, older) == sequence
                            && *input.get_unchecked(older + MIN_MATCH) == *input.get_unchecked(ip + MIN_MATCH)
                    }
                });
                match older {
                    Some(older) => match_pos = older,
                    None => {
                        advance_search(&mut ip, anchor, &mut search_attempts);
                        continue 'search;
                    }
                }
            }

            // Pull the match backwards into the current literal run. This is
            // particularly helpful around aligned cell records. As with
            // forward extension, compare words before locating the first
            // differing byte.
            let (position, candidate) = match_begin(input, ip, match_pos, anchor);
            ip = position;
            match_pos = candidate;

            // We already compared the first four bytes. Continue up to the
            // point where the required last five literals begin. `matchEnd`
            // compares a machine word at a time before locating the first
            // differing byte.
            let match_end = match_end(input, ip + MIN_MATCH, match_pos + MIN_MATCH, match_end_limit);

            emit_sequence(
                output,
                &mut op,
                &input[anchor..ip],
                (ip - match_pos) as u16,
                match_end - ip,
            )?;

            // The main loop jumps over the matched bytes rather than hashing
            // every position within them. Seed one position near the end so
            // an adjacent repeated record can still refer back into this
            // match. The next loop iteration will then seed `match_end`
            // normally.
            if match_end >= 2 && match_end - 2 + MIN_MATCH <= input.len() {
                let seed = match_end - 2;
                remember_position(table, hash_sequence(read_u32(input, seed)), seed);
            }

            ip = match_end;
            anchor = ip;
            search_attempts = 0;
        }
    }

    // Whatever remains after the last match is the terminal literal-only
    // sequence. For short inputs this is also the only sequence in the block.
    emit_last_literals(output, &mut op, &input[anchor..])?;
    Ok(op)
}

/// Decompress a raw LZ4 block into an exact-size output buffer.
///
/// Returns `output.len` on success. Both consuming all input and filling all
/// output are required. Raw LZ4 blocks do not carry their decoded size, so this
/// exact-size contract validates the size metadata maintained by the caller.
/// The input and output buffers must not overlap.
pub fn decompress(input: &[u8], output: &mut [u8]) -> Result<usize, DecompressError> {
    // `ip` and `op` always identify the next unread input byte and the next
    // unwritten output byte respectively.
    //
    // The decoder is written around one observation: almost every sequence
    // in real blocks has a short literal run and a short match. Both fast
    // paths below copy a fixed number of bytes blindly and let the length
    // arithmetic sort out how many of them were meaningful. Writing past a
    // run's logical end is safe within the output buffer because decoding
    // is strictly in order: every byte past `op` is either rewritten by a
    // later copy before anything can read it, or lies beyond the block's
    // final length and is never part of the result. The margin conditions
    // on the fast paths also subsume the exact bounds checks they replace,
    // which keeps decoding of malformed blocks memory-safe.
    let mut ip: usize = 0;
    let mut op: usize = 0;

    loop {
        // A normal block ends after the literal bytes of its final sequence.
        // This also accepts the empty block produced by our compressor, which
        // consists of a zero token and no literals.
        if ip == input.len() {
            if op != output.len() {
                return Err(DecompressError::OutputSizeMismatch);
            }
            return Ok(op);
        }

        let token = input[ip];
        ip += 1;

        // The high nibble and any extension bytes describe the literal run.
        // The literals are copied as a side effect of computing the length.
        let literal_len: usize = {
            let nibble: usize = (token >> 4) as usize;
            if nibble != 15 && (input.len() - ip).min(output.len() - op) >= 16 {
                // A run below the extension threshold is at most 14 bytes,
                // so with a 16-byte margin on both buffers one wide copy
                // covers it.
                write_u128(output, op, read_u128(input, ip));
                nibble
            } else {
                // Extended or margin-poor runs take the checked path. Bounds are
                // verified before copying so malformed blocks never cause a
                // partial read or write.
                let len = decode_length(input, &mut ip, nibble)?;
                if len > input.len() - ip {
                    return Err(DecompressError::TruncatedInput);
                }
                if len > output.len() - op {
                    return Err(DecompressError::OutputTooSmall);
                }
                output[op..op + len].copy_from_slice(&input[ip..ip + len]);
                len
            }
        };
        ip += literal_len;
        op += literal_len;

        // Ending immediately after the literals marks the final sequence. Any
        // non-final sequence must continue with an offset and match length.
        if ip == input.len() {
            if op != output.len() {
                return Err(DecompressError::OutputSizeMismatch);
            }
            return Ok(op);
        }

        if input.len() - ip < 2 {
            return Err(DecompressError::TruncatedInput);
        }
        let offset = read_u16(input, ip) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(DecompressError::InvalidOffset);
        }

        // The token stores the match length minus the four-byte minimum. As
        // with literals, a low nibble of 15 is extended by following bytes.
        let match_nibble: usize = (token & 0x0F) as usize;

        // A match whose length fits its nibble spans at most 18 bytes, so
        // three blind copies always cover it. They are overlap-safe when the
        // offset is at least a word: each load lies a full word behind the
        // store which could observe it, so repeating patterns propagate
        // correctly.
        if match_nibble != 15 && offset >= 8 && output.len() - op >= 18 {
            let m = op - offset;
            let v = read_u64(output, m);
            write_u64(output, op, v);
            let v = read_u64(output, m + 8);
            write_u64(output, op + 8, v);
            let v = read_u16(output, m + 16);
            write_u16(output, op + 16, v);
            op += match_nibble + MIN_MATCH;
            continue;
        }

        let encoded_match_len = decode_length(input, &mut ip, match_nibble)?;
        let match_len = match encoded_match_len.checked_add(MIN_MATCH) {
            Some(v) => v,
            None => return Err(DecompressError::OutputTooSmall),
        };
        if match_len > output.len() - op {
            return Err(DecompressError::OutputTooSmall);
        }

        // Match copies may overlap, so this cannot always be one memcpy.
        // `copyMatch` expands the common small repeating periods into wide
        // stores and uses word copies for larger offsets.
        copy_match(output, op, offset, match_len);
        op += match_len;
    }
}

/// Emit one non-final sequence.
///
/// A sequence starts with a token, followed by optional literal length bytes,
/// the literals themselves, the two-byte offset, and optional match length
/// bytes. This function computes the complete size first so `OutputTooSmall`
/// is reported without partially writing a sequence.
fn emit_sequence(
    output: &mut [u8],
    op: &mut usize,
    literals: &[u8],
    offset: u16,
    match_len: usize,
) -> Result<(), CompressError> {
    debug_assert!(match_len >= MIN_MATCH);
    debug_assert!(offset > 0);

    let encoded_match_len = match_len - MIN_MATCH;

    // One byte is always needed for the token and two for the offset. Each
    // length may additionally need extension bytes after its token nibble.
    let required = 1 + encoded_length_bytes(literals.len()) + literals.len() + 2 + encoded_length_bytes(encoded_match_len);
    if required > output.len() - *op {
        return Err(CompressError::OutputTooSmall);
    }

    let token_pos = *op;
    *op += 1;

    // Lengths below 15 fit directly in their nibble. Larger values put 15 in
    // the nibble and encode the remainder immediately after the token.
    output[token_pos] = ((literals.len().min(15) as u8) << 4) | (encoded_match_len.min(15) as u8);

    // Literal length extensions precede the literals they describe.
    if literals.len() >= 15 {
        write_length(output, op, literals.len() - 15);
    }
    output[*op..*op + literals.len()].copy_from_slice(literals);
    *op += literals.len();

    // Match length extensions follow the offset because this is where the
    // decoder expects them in an LZ4 sequence.
    write_u16(output, *op, offset);
    *op += 2;
    if encoded_match_len >= 15 {
        write_length(output, op, encoded_match_len - 15);
    }
    Ok(())
}

/// Emit the literal-only sequence which terminates every block.
///
/// There is no offset or match length after these bytes. As with
/// `emitSequence`, capacity is checked before modifying the output.
fn emit_last_literals(output: &mut [u8], op: &mut usize, literals: &[u8]) -> Result<(), CompressError> {
    let required = 1 + encoded_length_bytes(literals.len()) + literals.len();
    if required > output.len() - *op {
        return Err(CompressError::OutputTooSmall);
    }

    output[*op] = (literals.len().min(15) as u8) << 4;
    *op += 1;
    if literals.len() >= 15 {
        write_length(output, op, literals.len() - 15);
    }
    output[*op..*op + literals.len()].copy_from_slice(literals);
    *op += literals.len();
    Ok(())
}

/// Return the number of extension bytes needed when a length is represented by
/// a token nibble plus zero or more bytes. An extended length always ends with
/// a byte below 255, so an exact multiple of 255 requires a final zero byte.
fn encoded_length_bytes(encoded_len: usize) -> usize {
    if encoded_len < 15 {
        return 0;
    }
    (encoded_len - 15) / 255 + 1
}

/// Write the portion of a length which did not fit in the token nibble.
///
/// Each 255 byte means "add 255 and continue". The final byte is always less
/// than 255 and may be zero.
fn write_length(output: &mut [u8], op: &mut usize, length: usize) {
    let mut length = length;
    while length >= 255 {
        output[*op] = 255;
        *op += 1;
        length -= 255;
    }
    output[*op] = length as u8;
    *op += 1;
}

/// Decode a length from its token nibble and any following extension bytes.
/// `ip` is advanced past every consumed extension byte.
fn decode_length(input: &[u8], ip: &mut usize, nibble: usize) -> Result<usize, DecompressError> {
    let mut length: usize = nibble;
    if nibble != 15 {
        return Ok(length);
    }

    loop {
        if *ip >= input.len() {
            return Err(DecompressError::TruncatedInput);
        }
        let value = input[*ip];
        *ip += 1;
        length = match length.checked_add(value as usize) {
            Some(v) => v,
            None => return Err(DecompressError::TruncatedInput),
        };
        if value != 255 {
            return Ok(length);
        }
    }
}

// Unaligned little-endian integer reads/writes (Zig: `readIntAt`,
// `writeIntAt`, `copyIntAt`).

#[inline(always)]
fn read_u16(input: &[u8], position: usize) -> u16 {
    u16::from_le_bytes(input[position..position + 2].try_into().unwrap())
}

#[inline(always)]
fn read_u32(input: &[u8], position: usize) -> u32 {
    u32::from_le_bytes(input[position..position + 4].try_into().unwrap())
}

/// `read_u32` without the bounds check, for the per-byte match search
/// whose positions the loop bounds already prove in range.
///
/// # Safety
///
/// `position + 4 <= input.len()`.
#[inline(always)]
unsafe fn read_u32_unchecked(input: &[u8], position: usize) -> u32 {
    debug_assert!(position + 4 <= input.len());
    // SAFETY: in bounds per the caller; unaligned reads are allowed.
    unsafe { input.as_ptr().add(position).cast::<u32>().read_unaligned() }.to_le()
}

#[inline(always)]
fn read_u64(input: &[u8], position: usize) -> u64 {
    u64::from_le_bytes(input[position..position + 8].try_into().unwrap())
}

#[inline(always)]
fn read_u128(input: &[u8], position: usize) -> u128 {
    u128::from_le_bytes(input[position..position + 16].try_into().unwrap())
}

#[inline(always)]
fn write_u16(output: &mut [u8], position: usize, value: u16) {
    output[position..position + 2].copy_from_slice(&value.to_le_bytes());
}

#[inline(always)]
fn write_u64(output: &mut [u8], position: usize, value: u64) {
    output[position..position + 8].copy_from_slice(&value.to_le_bytes());
}

#[inline(always)]
fn write_u128(output: &mut [u8], position: usize, value: u128) {
    output[position..position + 16].copy_from_slice(&value.to_le_bytes());
}

/// Add a position to one hash slot and shift the previous newest position into
/// the fallback half. A position whose low bits are 0xFFFF conflicts with the
/// sentinel and is simply not stored.
#[inline(always)]
fn remember_position(table: &mut HashTable, hash: usize, position: usize) {
    let low: u16 = position as u16;
    if low == u16::MAX {
        return;
    }
    table[hash] = ((table[hash] as u16 as u32) << 16) | low as u32;
}

/// Recover the nearest preceding position from a stored low half. Modular
/// subtraction directly produces the LZ4 offset within the current window.
#[inline(always)]
fn candidate_position(ip: usize, stored: u16) -> Option<usize> {
    if stored == u16::MAX {
        return None;
    }
    let distance: u16 = (ip as u16).wrapping_sub(stored);
    if distance == 0 {
        return None;
    }
    Some(ip - distance as usize)
}

/// Advance through a literal run. The first KiB inspects every byte without
/// maintaining a counter, which keeps ordinary terminal-page searches cheap.
/// Longer runs enable gradually increasing steps for incompressible data.
#[inline(always)]
fn advance_search(ip: &mut usize, anchor: usize, attempts: &mut usize) {
    if *attempts == 0 {
        *ip += 1;
        if *ip - anchor == 1024 {
            *attempts = 1;
        }
        return;
    }

    *attempts += 1;
    *ip += 1 + *attempts / 64;
}

/// Extend a match backwards without crossing the current literal anchor or the
/// beginning of the candidate. The returned positions preserve their offset.
fn match_begin(input: &[u8], position: usize, candidate: usize, anchor: usize) -> (usize, usize) {
    let mut position = position;
    let mut candidate = candidate;

    while (position - anchor).min(candidate) >= 8 {
        let position_word = position - 8;
        let candidate_word = candidate - 8;
        let difference = read_u64(input, position_word) ^ read_u64(input, candidate_word);
        if difference != 0 {
            let equal_bytes = (difference.leading_zeros() / 8) as usize;
            position -= equal_bytes;
            candidate -= equal_bytes;
            return (position, candidate);
        }

        position = position_word;
        candidate = candidate_word;
    }

    while position > anchor && candidate > 0 && input[position - 1] == input[candidate - 1] {
        position -= 1;
        candidate -= 1;
    }
    (position, candidate)
}

/// Return the first input position where two matching runs differ, or `limit`
/// when they remain equal. Both positions are known to have matched through
/// `min_match` before this is called.
fn match_end(input: &[u8], position: usize, candidate: usize, limit: usize) -> usize {
    let mut position = position;
    let mut candidate = candidate;

    // Reading as little endian makes the least-significant differing bit map
    // to the earliest byte in memory on every target.
    while limit - position >= 8 {
        let difference = read_u64(input, position) ^ read_u64(input, candidate);
        if difference != 0 {
            let equal_bytes = (difference.trailing_zeros() / 8) as usize;
            return position + equal_bytes;
        }

        position += 8;
        candidate += 8;
    }

    while position < limit && input[position] == input[candidate] {
        position += 1;
        candidate += 1;
    }
    position
}

/// Copy one decoded match from `offset` bytes behind `op`.
///
/// The caller has validated that the match fits: `op + match_len` never
/// exceeds `output.len`. Wide copies may write a few scratch bytes past the
/// match's logical end; as described in `decompress`, that is safe anywhere
/// the write stays inside the output buffer. Every path therefore bounds its
/// wide stores by both the match end and the buffer end, and the bytewise
/// loop at the bottom finishes whatever remains.
fn copy_match(output: &mut [u8], op: usize, offset: usize, match_len: usize) {
    let mut op = op;
    let end = op + match_len;

    // A source which ends behind the copy can never overlap it. Long
    // distant matches are common in structured pages (repeated rows and
    // whole blank regions), and one exact memcpy moves them in cache-line
    // units. Shorter matches are not worth the call overhead.
    if offset >= match_len && match_len >= 64 {
        output.copy_within(op - offset..op - offset + match_len, op);
        return;
    }

    match offset {
        // A period which divides the word size expands into one repeated
        // pattern word. Long runs (blank lines, repeated cells) then become
        // independent stores, with no load waiting on a preceding store.
        // Stores advance by whole words from `op`, which preserves the
        // pattern's phase.
        1 | 2 | 4 | 8 => {
            let pattern: u64 = match offset {
                1 => (output[op - 1] as u64).wrapping_mul(0x0101_0101_0101_0101),
                2 => (read_u16(output, op - 2) as u64).wrapping_mul(0x0001_0001_0001_0001),
                4 => (read_u32(output, op - 4) as u64).wrapping_mul(0x0000_0001_0000_0001),
                8 => read_u64(output, op - 8),
                _ => unreachable!(),
            };

            let limit = end.min(output.len().saturating_sub(7));
            while op < limit {
                write_u64(output, op, pattern);
                op += 8;
            }
        }

        // Wide copies are overlap-safe for the remaining offsets of at
        // least a copy unit: each load lies a full unit behind the store
        // which could observe it. Offsets 3, 5, 6, and 7 fall through to
        // the bytewise loop; they are rare in real data and word tricks
        // for them cost more in complexity than they return.
        _ => {
            if offset >= 16 {
                let limit = end.min(output.len().saturating_sub(15));
                while op < limit {
                    let v = read_u128(output, op - offset);
                    write_u128(output, op, v);
                    op += 16;
                }
            } else if offset >= 8 {
                let limit = end.min(output.len().saturating_sub(7));
                while op < limit {
                    let v = read_u64(output, op - offset);
                    write_u64(output, op, v);
                    op += 8;
                }
            }
        }
    }

    while op < end {
        output[op] = output[op - offset];
        op += 1;
    }
}

/// Map a four-byte input sequence to its scratch-table slot.
#[inline(always)]
fn hash_sequence(sequence: u32) -> usize {
    (sequence.wrapping_mul(HASH_MULTIPLIER) >> (32 - HASH_LOG)) as usize
}

#[cfg(test)]
pub(crate) mod test_rng {
    //! Deterministic stand-in for Zig's `std.Random.DefaultPrng`
    //! (xoshiro256++ seeded through splitmix64). The tests only need a
    //! deterministic stream, not Zig's exact sequence.

    pub struct Prng {
        s: [u64; 4],
    }

    impl Prng {
        pub fn init(seed: u64) -> Prng {
            let mut x = seed;
            let mut next = || {
                x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = x;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            };
            Prng {
                s: [next(), next(), next(), next()],
            }
        }

        pub fn next_u64(&mut self) -> u64 {
            let s = &mut self.s;
            let result = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
            let t = s[1] << 17;
            s[2] ^= s[0];
            s[3] ^= s[1];
            s[1] ^= s[2];
            s[0] ^= s[3];
            s[2] ^= t;
            s[3] = s[3].rotate_left(45);
            result
        }

        pub fn bytes(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let v = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
        }

        pub fn int_u8(&mut self) -> u8 {
            self.next_u64() as u8
        }

        /// Uniform in `[0, less_than)`.
        pub fn uint_less_than(&mut self, less_than: usize) -> usize {
            assert!(less_than > 0);
            (self.next_u64() % less_than as u64) as usize
        }

        /// Uniform in `[0, at_most]`.
        pub fn uint_at_most(&mut self, at_most: usize) -> usize {
            if at_most == usize::MAX {
                return self.next_u64() as usize;
            }
            self.uint_less_than(at_most + 1)
        }

        /// Uniform in `[at_least, at_most]`.
        pub fn int_range_at_most(&mut self, at_least: usize, at_most: usize) -> usize {
            at_least + self.uint_at_most(at_most - at_least)
        }

        pub fn boolean(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }

        /// Uniform float in `[0, 1)`.
        pub fn float(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_rng::Prng;
    use super::*;

    /// Shared round-trip assertion used by the corpus-style tests below.
    fn expect_round_trip(input: &[u8]) {
        let bound = compress_bound(input.len()).unwrap();
        let mut encoded = vec![0u8; bound];
        let mut decoded = vec![0u8; input.len()];

        let mut table: HashTable = [0; 1 << HASH_LOG];
        let encoded_len = compress(input, &mut encoded, &mut table).unwrap();
        assert_eq!(input.len(), decompress(&encoded[..encoded_len], &mut decoded).unwrap());
        assert_eq!(input, &decoded[..]);
    }

    #[test]
    fn compress_bound_test() {
        assert_eq!(16, compress_bound(0).unwrap());
        assert_eq!(272, compress_bound(255).unwrap());
        assert_eq!(Err(CompressError::InputTooLarge), compress_bound(MAX_INPUT_SIZE + 1));
    }

    #[test]
    fn literal_only_compatibility_vectors() {
        let mut empty: [u8; 0] = [];
        assert_eq!(0, decompress(&[0], &mut empty).unwrap());

        let mut hello = [0u8; 5];
        assert_eq!(5, decompress(&[0x50, b'h', b'e', b'l', b'l', b'o'], &mut hello).unwrap());
        assert_eq!(b"hello", &hello);

        let mut fifteen = [0u8; 15];
        let mut encoded = [0u8; 17];
        encoded[0] = 0xF0;
        encoded[1] = 0;
        encoded[2..].fill(b'x');
        decompress(&encoded, &mut fifteen).unwrap();
        assert!(fifteen.iter().all(|&b| b == b'x'));
    }

    #[test]
    fn overlapping_match_compatibility_vector() {
        // One literal 'a', followed by a four-byte match at distance one.
        let mut output = [0u8; 5];
        assert_eq!(5, decompress(&[0x10, b'a', 0x01, 0x00], &mut output).unwrap());
        assert_eq!(b"aaaaa", &output);
    }

    #[test]
    fn extended_overlapping_match_compatibility_vector() {
        // One literal followed by a 274-byte match. The match extension is
        // encoded as 255 + 0 after the low token nibble's initial 15 bytes.
        let mut output = [0u8; 275];
        assert_eq!(
            output.len(),
            decompress(&[0x1F, b'a', 0x01, 0x00, 0xFF, 0x00], &mut output).unwrap()
        );
        assert!(output.iter().all(|&b| b == b'a'));
    }

    #[test]
    fn short_offset_compatibility_vectors() {
        // These blocks end immediately after their match. Besides covering the
        // repeating-pattern paths, they verify that the decoder uses exact copies
        // when the block does not provide the standard trailing-literal margin.
        let mut offset_two = [0u8; 6];
        decompress(&[0x20, b'a', b'b', 0x02, 0x00], &mut offset_two).unwrap();
        assert_eq!(b"ababab", &offset_two);

        let mut offset_three = [0u8; 9];
        decompress(&[0x32, b'a', b'b', b'c', 0x03, 0x00], &mut offset_three).unwrap();
        assert_eq!(b"abcabcabc", &offset_three);

        let mut offset_four = [0u8; 8];
        decompress(&[0x40, b'a', b'b', b'c', b'd', 0x04, 0x00], &mut offset_four).unwrap();
        assert_eq!(b"abcdabcd", &offset_four);
    }

    #[test]
    fn bounded_wild_copies_are_overwritten_by_final_literals() {
        // The first sequence's nine-byte match leaves the five final literals
        // required by the LZ4 block format. Its logical one-byte tail is copied as
        // a word and the following literal sequence overwrites the extra bytes.
        let mut repeated_byte = [0u8; 15];
        decompress(
            &[0x15, b'a', 0x01, 0x00, 0x50, b'1', b'2', b'3', b'4', b'5'],
            &mut repeated_byte,
        )
        .unwrap();
        assert_eq!(b"aaaaaaaaaa12345", &repeated_byte);

        // Exercise the same bounded tail copy with a non-overlapping offset.
        let mut word_offset = [0u8; 22];
        decompress(
            &[
                0x85, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', 0x08, 0x00, 0x50, b'1', b'2', b'3', b'4', b'5',
            ],
            &mut word_offset,
        )
        .unwrap();
        assert_eq!(b"abcdefghabcdefgha12345", &word_offset);
    }

    #[test]
    fn maximum_match_offset_compatibility_vector() {
        let literal_len = u16::MAX as usize;
        let extension_len = (literal_len - 15) / 255 + 1;
        let mut encoded = vec![0u8; 1 + extension_len + literal_len + 2];
        let mut output = vec![0u8; literal_len + MIN_MATCH];

        let mut op: usize = 0;
        encoded[op] = 0xF0;
        op += 1;
        write_length(&mut encoded, &mut op, literal_len - 15);
        for (i, byte) in encoded[op..op + literal_len].iter_mut().enumerate() {
            *byte = i as u8;
        }
        op += literal_len;
        encoded[op..op + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        op += 2;

        assert_eq!(encoded.len(), op);
        assert_eq!(output.len(), decompress(&encoded, &mut output).unwrap());
        assert_eq!(&encoded[1 + extension_len..1 + extension_len + 4], &output[literal_len..]);
    }

    #[test]
    fn round_trips_boundary_sized_inputs() {
        let lengths = [
            0usize, 1, 3, 4, 5, 12, 15, 16, 19, 20, 254, 255, 256, 269, 270, 271, 510, 511, 512, 65_535, 65_536,
            65_537,
        ];

        for len in lengths {
            let buf: Vec<u8> = (0..len).map(|i| i.wrapping_mul(31) as u8).collect();
            expect_round_trip(&buf);
        }
    }

    #[test]
    fn round_trips_compressible_page_sized_inputs() {
        let page_len = 400 * 1024;

        let zeros = vec![0u8; page_len];
        expect_round_trip(&zeros);

        let mut structured = vec![0u8; page_len];
        for i in 0..page_len / 8 {
            structured[i * 8] = (b' ' as usize + i % 95) as u8;
            structured[i * 8 + 4] = ((i / 80) % 16) as u8;
        }
        expect_round_trip(&structured);
    }

    #[test]
    fn round_trips_deterministic_random_inputs() {
        let mut prng = Prng::init(0x4C5A_3401);

        for _ in 0..256 {
            let len = prng.uint_less_than(32 * 1024);
            let mut input = vec![0u8; len];
            prng.bytes(&mut input);
            expect_round_trip(&input);
        }
    }

    #[test]
    fn compress_reports_short_output() {
        let input = b"a terminal page needs enough output space";
        let mut table: HashTable = [0; 1 << HASH_LOG];
        let mut output = [0u8; 4];
        assert_eq!(Err(CompressError::OutputTooSmall), compress(input, &mut output, &mut table));
    }

    #[test]
    fn decompress_rejects_malformed_blocks() {
        let mut output = [0u8; 32];

        assert_eq!(Err(DecompressError::TruncatedInput), decompress(&[0xF0], &mut output));
        assert_eq!(Err(DecompressError::TruncatedInput), decompress(&[0x10, b'a', 1], &mut output[..5]));
        assert_eq!(Err(DecompressError::InvalidOffset), decompress(&[0x10, b'a', 0, 0], &mut output[..5]));
        assert_eq!(Err(DecompressError::InvalidOffset), decompress(&[0x10, b'a', 2, 0], &mut output[..5]));
        assert_eq!(Err(DecompressError::OutputTooSmall), decompress(&[0x10, b'a', 1, 0], &mut output[..4]));
        assert_eq!(Err(DecompressError::OutputSizeMismatch), decompress(&[0], &mut output[..1]));
    }
}
