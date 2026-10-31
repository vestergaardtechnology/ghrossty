//! Port of `src/terminal/snapshot/grid.zig`.
//!
//! STATUS: ported
//!
//! Grid (rows, cells, and grapheme suffixes) encoding.
//!
//! A grid contains the rows and cells of a terminal page. Its dimensions are
//! supplied by the containing record rather than repeated here. The encoder
//! writes exactly `rows` row records followed by one grapheme suffix section.
//!
//! All records are tightly packed with no padding between them. All integers
//! are unsigned and little-endian.
//!
//! The layout is designed so the common case decodes with bulk copies: cells
//! are fixed-size words, trailing default cells are elided per row, and the
//! variable-length grapheme suffixes live outside the row data.
//!
//! ```text
//! +---------------------------+
//! | Row 0                     |
//! +---------------------------+
//! | ...                       |
//! +---------------------------+
//! | Row (rows - 1)            |
//! +---------------------------+
//! | Grapheme suffix section   |
//! +---------------------------+
//! ```
//!
//! ## Row
//!
//! Each row has the following format:
//!
//! | Offset | Size              | Field                      |
//! | -----: | ----------------: | :------------------------- |
//! |      0 |                 1 | Row flags                  |
//! |      1 |                 2 | Encoded cell count (`u16`) |
//! |      3 | `width` * `count` | Encoded cells              |
//!
//! The row flag byte has the following format:
//!
//! | Bits | Field                     |
//! | ---: | :------------------------ |
//! |    0 | Wrap                      |
//! |    1 | Wrap continuation         |
//! |  2-3 | Semantic prompt           |
//! |  4-5 | Encoded cell width        |
//! |  6-7 | Reserved, zero            |
//!
//! Semantic prompt values are:
//!
//! | Value | Meaning             |
//! | ----: | :------------------ |
//! |     0 | None                |
//! |     1 | Prompt              |
//! |     2 | Prompt continuation |
//!
//! Value 3 is not emitted in snapshot version 1. Decoders treat it as none.
//!
//! The encoded cell count must not exceed the grid's column count; it is a
//! structural field and decoders reject larger values. Cells at and beyond
//! the count are the default cell: all sixty-four bits zero, meaning an
//! empty narrow codepoint cell with the default style and no hyperlink.
//! Canonical encoders emit exactly through the row's last non-default cell,
//! so a fully default row has a zero count and no cell words.
//!
//! ## Encoded cell width
//!
//! The two width bits select how many bytes encode each of the row's
//! cells: `1 << width` is the size, so zero through three select one, two,
//! four, or eight bytes. Narrower widths are truncated transports of the
//! same cell word: a cell qualifies for a width when all of its higher
//! word bits are zero.
//!
//! | Width | Bytes | Encoded value and admitted cells                     |
//! | ----: | ----: | :--------------------------------------------------- |
//! |     0 |     1 | Codepoint at or below U+00FF; all other bits zero    |
//! |     1 |     2 | Codepoint at or below U+FFFF; all other bits zero    |
//! |     2 |     4 | Word bits 0-31: any content kind and codepoint, style |
//! |       |       | IDs 1-63, narrow, no flags, no hyperlink             |
//! |     3 |     8 | The complete word                                    |
//!
//! Widths zero and one store the codepoint value itself, which is word
//! bits 2-25 shifted down; the reconstructed word is the codepoint shifted
//! left by two. Width two stores the word's low half unshifted. This makes
//! every width a zero-extension on decode and a truncation on encode.
//!
//! Canonical encoders choose each row's smallest admissible width, which
//! follows directly from the bitwise OR of the row's cell words. Decoders
//! use the declared width for framing and accept rows encoded wider than
//! necessary. The width of a row with a zero cell count is canonically
//! zero and carries no meaning.
//!
//! Native row cache flags are not encoded. In particular, the Kitty virtual
//! placeholder hint is derived while decoding cells containing U+10EEEE.
//!
//! ## Cell
//!
//! Each cell is one 64-bit little-endian word, transported at the row's
//! encoded cell width as described above:
//!
//! ```text
//!  bit 0 +-------------------------------+
//!        | Content kind                  |
//!        | 2 bits                        |
//!  bit 2 +-------------------------------+
//!        | Content                       |
//!        | 24 bits                       |
//! bit 26 +-------------------------------+
//!        | Style ID                      |
//!        | 16 bits                       |
//! bit 42 +-------------------------------+
//!        | Width kind                    |
//!        | 2 bits                        |
//! bit 44 +-------------------------------+
//!        | Protected                     |
//! bit 45 +-------------------------------+
//!        | Hyperlink flag                |
//! bit 46 +-------------------------------+
//!        | Semantic content              |
//!        | 2 bits                        |
//! bit 48 +-------------------------------+
//!        | Hyperlink ID                  |
//!        | 16 bits                       |
//! bit 64 +-------------------------------+
//! ```
//!
//! Content kinds are:
//!
//! | Value | Meaning                          |
//! | ----: | :------------------------------- |
//! |     0 | Codepoint                        |
//! |     1 | Codepoint with grapheme suffix   |
//! |     2 | Palette background               |
//! |     3 | RGB background                   |
//!
//! The content kind selects the layout of the 24-bit content field. All
//! content bit positions below are relative to the start of the content
//! field, so content bit 0 is cell word bit 2.
//!
//! For codepoint content (kinds 0 and 1), the content field is a Unicode
//! scalar value. Values above U+10FFFF and surrogates decode as U+FFFD.
//! Kind 1 additionally declares that the grapheme suffix section contains
//! one entry for this cell; the cell is otherwise identical to kind 0.
//!
//! For a palette background (kind 2), the low byte is the palette index.
//! The remaining content bits are reserved, canonically zero, and ignored
//! by decoders.
//!
//! For an RGB background (kind 3), the content field is one byte per
//! channel: red (bits 0-7), green (bits 8-15), blue (bits 16-23).
//!
//! Width kinds are:
//!
//! | Value | Meaning     |
//! | ----: | :---------- |
//! |     0 | Narrow      |
//! |     1 | Wide        |
//! |     2 | Spacer tail |
//! |     3 | Spacer head |
//!
//! Semantic content values are:
//!
//! | Value | Meaning |
//! | ----: | :------ |
//! |     0 | Output  |
//! |     1 | Input   |
//! |     2 | Prompt  |
//!
//! Value 3 is not emitted in snapshot version 1. Decoders treat it as output.
//!
//! Style and hyperlink ID zero mean no style and no hyperlink. Other IDs
//! refer to entries in the containing record's separate style and hyperlink
//! tables. The hyperlink flag is set exactly when the hyperlink ID is
//! nonzero; decoders derive cell linkage from the remapped ID and ignore the
//! flag itself.
//!
//! ## Grapheme suffix section
//!
//! The section begins with an entry count followed by that many entries:
//!
//! | Offset | Size    | Field                |
//! | -----: | ------: | :------------------- |
//! |      0 |       4 | Entry count (`u32`)  |
//! |      4 | varies  | Entries              |
//!
//! Each entry:
//!
//! | Offset | Size        | Field                       |
//! | -----: | ----------: | :-------------------------- |
//! |      0 |           2 | Row (`u16`)                 |
//! |      2 |           2 | Column (`u16`)              |
//! |      4 |           2 | Codepoint count (`u16`)     |
//! |      6 | 4 * `count` | Codepoints (`u32` each)     |
//!
//! Each codepoint is a Unicode scalar; invalid scalars are individually
//! ignored by decoders. Canonical encoders emit exactly one entry, with at
//! least one codepoint, for every kind 1 cell, in ascending row-then-column
//! order. Decoders consume every declared entry and ignore entries whose
//! target is out of range, is not a codepoint cell, has a zero codepoint, or
//! already received an entry. A kind 1 cell that receives no suffix
//! codepoints decodes as a plain codepoint cell.
//!
//! Notes on the port:
//! - The Zig bulk codec uses explicit `@Vector` shuffles. The port keeps the
//!   same bulk/portable split but writes the bulk loops as plain scalar
//!   loops over cell words (which LLVM auto-vectorizes); the wire bytes are
//!   identical.
//! - Packed wire structs (`Row`, `Cell`) are plain structs with explicit
//!   `to_bits`/`from_bits` conversions.

use std::sync::OnceLock;

use super::io::{self, Error};
use crate::terminal::hyperlink as terminal_hyperlink;
use crate::terminal::page::{self as terminal_page, ContentTag, Page, SemanticContent, SemanticPrompt, Wide};
use crate::terminal::style as terminal_style;

/// The Kitty virtual-placement placeholder remains wire-relevant even when
/// runtime Kitty graphics support is compiled out.
const KITTY_VIRTUAL_PLACEHOLDER: u32 = 0x10EEEE;

type TerminalCell = terminal_page::Cell;
type TerminalRow = terminal_page::Row;
type TerminalHyperlinkId = terminal_hyperlink::Id;
type TerminalStyleId = terminal_style::Id;

/// The header before every row's encoded cells (Zig: `packed struct(u8)`).
///
/// The semantic prompt is a raw integer for the same reason as the wire
/// cell fields: decoders must accept its reserved value without
/// instantiating an invalid native enum. The width enum is exhaustive over
/// its two bits, so every header byte converts to a valid value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Row {
    pub wrap: bool,
    pub wrap_continuation: bool,
    /// u2
    pub semantic_prompt: u8,
    pub cell_width: EncodedWidth,
    /// u2
    pub _padding: u8,
}

impl Row {
    pub const fn to_bits(self) -> u8 {
        (self.wrap as u8)
            | (self.wrap_continuation as u8) << 1
            | (self.semantic_prompt & 0x3) << 2
            | (self.cell_width as u8) << 4
            | (self._padding & 0x3) << 6
    }

    pub const fn from_bits(bits: u8) -> Row {
        Row {
            wrap: bits & 1 != 0,
            wrap_continuation: bits & 2 != 0,
            semantic_prompt: (bits >> 2) & 0x3,
            cell_width: EncodedWidth::from_int((bits >> 4) & 0x3),
            _padding: (bits >> 6) & 0x3,
        }
    }
}

/// Bit offsets of the wire cell fields (Zig: `@bitOffsetOf(Cell, ...)`).
pub mod cell_bits {
    pub const KIND: u32 = 0;
    pub const CONTENT: u32 = 2;
    pub const STYLE_ID: u32 = 26;
    pub const WIDTH: u32 = 42;
    pub const PROTECTED: u32 = 44;
    pub const HYPERLINK: u32 = 45;
    pub const SEMANTIC_CONTENT: u32 = 46;
    pub const HYPERLINK_ID: u32 = 48;
}

/// The wire layout of one encoded cell (Zig: `packed struct(u64)`). This is
/// its own registry: the bit positions and field meanings are part of the
/// snapshot format and are documented above independently of the native
/// cell.
///
/// Enum-like fields are raw integers because decoders must accept reserved
/// values (for example semantic content 3) without instantiating an invalid
/// native enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    /// u2, a `CellKind`.
    pub kind: u8,
    /// u24
    pub content: u32,
    pub style_id: u16,
    /// u2
    pub width: u8,
    pub protected: bool,
    pub hyperlink: bool,
    /// u2
    pub semantic_content: u8,
    pub hyperlink_id: u16,
}

impl Cell {
    pub const fn to_bits(self) -> u64 {
        ((self.kind as u64) & 0x3) << cell_bits::KIND
            | ((self.content as u64) & 0xFF_FFFF) << cell_bits::CONTENT
            | (self.style_id as u64) << cell_bits::STYLE_ID
            | ((self.width as u64) & 0x3) << cell_bits::WIDTH
            | (self.protected as u64) << cell_bits::PROTECTED
            | (self.hyperlink as u64) << cell_bits::HYPERLINK
            | ((self.semantic_content as u64) & 0x3) << cell_bits::SEMANTIC_CONTENT
            | (self.hyperlink_id as u64) << cell_bits::HYPERLINK_ID
    }

    pub const fn from_bits(bits: u64) -> Cell {
        Cell {
            kind: ((bits >> cell_bits::KIND) & 0x3) as u8,
            content: ((bits >> cell_bits::CONTENT) & 0xFF_FFFF) as u32,
            style_id: (bits >> cell_bits::STYLE_ID) as u16,
            width: ((bits >> cell_bits::WIDTH) & 0x3) as u8,
            protected: (bits >> cell_bits::PROTECTED) & 1 != 0,
            hyperlink: (bits >> cell_bits::HYPERLINK) & 1 != 0,
            semantic_content: ((bits >> cell_bits::SEMANTIC_CONTENT) & 0x3) as u8,
            hyperlink_id: (bits >> cell_bits::HYPERLINK_ID) as u16,
        }
    }
}

/// Determines how `Cell::content` is interpreted (Zig: `Cell.Kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CellKind {
    Codepoint = 0,
    CodepointGrapheme = 1,
    BgColorPalette = 2,
    BgColorRgb = 3,
}

impl CellKind {
    pub const fn from_int(v: u8) -> CellKind {
        match v & 0x3 {
            0 => CellKind::Codepoint,
            1 => CellKind::CodepointGrapheme,
            2 => CellKind::BgColorPalette,
            _ => CellKind::BgColorRgb,
        }
    }
}

/// The encoded cell width declared by a row header: how many bytes
/// transport each of the row's cell words (Zig: `Cell.EncodedWidth`). See
/// the format documentation above for the value each width transports and
/// the cells it admits.
///
/// `truncate` and `extend` are the transport transform itself, so the
/// admission masks derive from them rather than being maintained by
/// hand: a cell word is admitted exactly when it round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum EncodedWidth {
    #[default]
    One = 0,
    Two = 1,
    Four = 2,
    Eight = 3,
}

impl EncodedWidth {
    pub const fn from_int(v: u8) -> EncodedWidth {
        match v & 0x3 {
            0 => EncodedWidth::One,
            1 => EncodedWidth::Two,
            2 => EncodedWidth::Four,
            _ => EncodedWidth::Eight,
        }
    }

    /// The number of bytes transporting one cell word.
    pub const fn size(self) -> usize {
        1usize << (self as u8)
    }

    /// The maximum value of the integer transporting one cell word
    /// (Zig: `std.math.maxInt(self.Int())`).
    const fn int_max(self) -> u64 {
        match self {
            EncodedWidth::One => u8::MAX as u64,
            EncodedWidth::Two => u16::MAX as u64,
            EncodedWidth::Four => u32::MAX as u64,
            EncodedWidth::Eight => u64::MAX,
        }
    }

    /// Truncate one cell word to its transported value. Only words
    /// admitted by this width round-trip; `select` proves that for
    /// every cell in a row before an encoder may use it.
    pub const fn truncate(self, word: u64) -> u64 {
        match self {
            // The bare codepoint, shifted down from the content field.
            EncodedWidth::One | EncodedWidth::Two => Cell::from_bits(word).content as u64 & self.int_max(),

            // The word itself.
            EncodedWidth::Four | EncodedWidth::Eight => word & self.int_max(),
        }
    }

    /// Widen one transported value back to its cell word.
    pub const fn extend(self, value: u64) -> u64 {
        match self {
            EncodedWidth::One | EncodedWidth::Two => Cell {
                kind: 0,
                content: value as u32,
                style_id: 0,
                width: 0,
                protected: false,
                hyperlink: false,
                semantic_content: 0,
                hyperlink_id: 0,
            }
            .to_bits(),
            EncodedWidth::Four | EncodedWidth::Eight => value,
        }
    }

    /// The word bits a cell may use and still round-trip through this
    /// width.
    pub const fn mask(self) -> u64 {
        self.extend(self.int_max())
    }

    /// The smallest width admitting the word, typically the bitwise
    /// OR of every cell word in a row.
    pub const fn select(word: u64) -> EncodedWidth {
        if word & !EncodedWidth::One.mask() == 0 {
            return EncodedWidth::One;
        }
        if word & !EncodedWidth::Two.mask() == 0 {
            return EncodedWidth::Two;
        }
        if word & !EncodedWidth::Four.mask() == 0 {
            return EncodedWidth::Four;
        }
        EncodedWidth::Eight
    }

    /// Write one transported value little-endian into `out`.
    #[inline(always)]
    fn write_value(self, out: &mut [u8], value: u64) {
        // One fixed-size store per width (Zig: `std.mem.writeInt` with a
        // comptime integer type). A runtime-length copy would become a
        // libc `memcpy` call per cell.
        let bytes = value.to_le_bytes();
        match self {
            EncodedWidth::One => out[0] = bytes[0],
            EncodedWidth::Two => out[..2].copy_from_slice(&bytes[..2]),
            EncodedWidth::Four => out[..4].copy_from_slice(&bytes[..4]),
            EncodedWidth::Eight => out[..8].copy_from_slice(&bytes),
        }
    }

    /// Read one transported value little-endian from `bytes`.
    #[inline(always)]
    fn read_value(self, bytes: &[u8]) -> u64 {
        // One fixed-size load per width, see `write_value`.
        match self {
            EncodedWidth::One => bytes[0] as u64,
            EncodedWidth::Two => u16::from_le_bytes([bytes[0], bytes[1]]) as u64,
            EncodedWidth::Four => u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64,
            EncodedWidth::Eight => u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        }
    }
}

/// The wire bits of the width field (Zig: `@bitCast(Cell{ .width = 3 })`).
const WIDE_MASK: u64 = Cell {
    kind: 0,
    content: 0,
    style_id: 0,
    width: 3,
    protected: false,
    hyperlink: false,
    semantic_content: 0,
    hyperlink_id: 0,
}
.to_bits();

/// Whether the native cell's in-memory layout matches the wire cell layout
/// bit for bit, with the wire hyperlink ID occupying the native padding.
///
/// The wire format does not require this: it is an optimization. When it
/// holds, whole rows encode and decode as bulk copies. If the native layout
/// ever diverges, the portable field-by-field codec below remains correct
/// and this simply becomes false.
fn native_matches_wire() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        if std::mem::size_of::<TerminalCell>() != 8 {
            return false;
        }

        // Instead of pinning every native field offset and enum value with
        // assertions that must be maintained by hand, each probe below pairs
        // a native cell with the wire cell that must share its exact bit
        // pattern; any difference means some field moved or some enum
        // member was renumbered. Three probes, one per content payload,
        // cover the whole cell.
        let mut probes: Vec<(TerminalCell, Cell)> = Vec::new();

        {
            let mut native = TerminalCell::init(0);
            native.set_content_tag(ContentTag::CodepointGrapheme);
            native.set_content_codepoint(0x10FFFF);
            native.set_style_id(0xBEEF);
            native.set_wide(Wide::SpacerTail);
            native.set_protected(true);
            // The native padding position, canonical wire or not.
            native = TerminalCell::from_bits(native.to_bits() | (0x1D2Cu64 << 48));
            probes.push((
                native,
                Cell {
                    kind: 1,
                    content: 0x10FFFF,
                    style_id: 0xBEEF,
                    width: 2,
                    protected: true,
                    hyperlink_id: 0x1D2C,
                    ..Default::default()
                },
            ));
        }
        {
            let mut native = TerminalCell::init(0);
            native.set_content_tag(ContentTag::BgColorPalette);
            native.set_content_color_palette(0xAB);
            native.set_wide(Wide::SpacerHead);
            native.set_hyperlink(true);
            native.set_semantic_content(SemanticContent::Input);
            probes.push((
                native,
                Cell {
                    kind: 2,
                    content: 0xAB,
                    width: 3,
                    hyperlink: true,
                    semantic_content: 1,
                    ..Default::default()
                },
            ));
        }
        {
            let mut native = TerminalCell::init(0);
            native.set_content_tag(ContentTag::BgColorRgb);
            native.set_content_color_rgb(terminal_page::Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            });
            native.set_wide(Wide::Wide);
            native.set_semantic_content(SemanticContent::Prompt);
            probes.push((
                native,
                Cell {
                    kind: 3,
                    content: 0x563412,
                    width: 1,
                    semantic_content: 2,
                    ..Default::default()
                },
            ));
        }

        probes.iter().all(|(native, wire)| native.to_bits() == wire.to_bits())
    })
}

/// Whether rows of cells can be copied between native and wire storage
/// without per-cell transformation.
#[inline(always)]
fn bulk_codec() -> bool {
    cfg!(target_endian = "little") && native_matches_wire()
}

/// View native cells as their 64-bit words.
#[inline(always)]
fn cell_words(cells: &[TerminalCell]) -> &[u64] {
    // SAFETY: `Cell` is `repr(transparent)` over `u64`.
    unsafe { std::slice::from_raw_parts(cells.as_ptr().cast::<u64>(), cells.len()) }
}

/// Errors from `encode`: `WriteFailed`, `InvalidWideCell` (wide and spacer
/// cells do not form a valid row), `TooManyGraphemes` (one cell's grapheme
/// suffix exceeds the entry's u16 codepoint count).
pub type EncodeError = Error;

/// Encode every row, cell, and grapheme suffix directly from a page.
///
/// If an error is returned, partial data may have been written. If you
/// want transactional writing, the caller is responsible for using something
/// like a seekable stream and rolling back.
pub fn encode(page: &Page, writer: &mut dyn io::Writer) -> Result<(), EncodeError> {
    let result = encode_inner(page, writer);
    page.assert_integrity();
    result
}

fn encode_inner(page: &Page, writer: &mut dyn io::Writer) -> Result<(), EncodeError> {
    for y in 0..page.size.rows as usize {
        // SAFETY: `y` is within the page and its row and cells are live.
        let row = unsafe { &*page.get_row(y) };
        let cells: &[TerminalCell] = unsafe { &*page.get_cells(row) };

        // Trailing default cells decode implicitly: wide/spacer pairs and
        // hyperlinked or styled cells are always nonzero, so eliding the
        // zero suffix never drops encoded state. The scan also accumulates
        // the OR of the row's cell words to select its encoded cell width.
        let (count, word_or) = scan_row(cells);

        // Validate the wide state of every encoded cell so we don't encode
        // corrupt data. The width bits of the OR word witness whether any
        // encoded cell is non-narrow at all; rows without them, the common
        // case, satisfy every pair rule vacuously. Trailing default cells
        // are narrow, so checking the encoded prefix against the full row
        // width covers every pair.
        if word_or & WIDE_MASK != 0 {
            for (x, cell) in cells[..count].iter().enumerate() {
                match cell.wide() {
                    Wide::Narrow => {}
                    Wide::Wide => {
                        if x + 1 == cells.len() || cells[x + 1].wide() != Wide::SpacerTail {
                            return Err(Error::InvalidWideCell);
                        }
                    }
                    Wide::SpacerTail => {
                        if x == 0 || cells[x - 1].wide() != Wide::Wide {
                            return Err(Error::InvalidWideCell);
                        }
                    }
                    Wide::SpacerHead => {
                        if x + 1 != cells.len() || !row.wrap() {
                            return Err(Error::InvalidWideCell);
                        }
                    }
                }
            }
        }

        // Canonical rows use the smallest admissible width.
        let cell_width = EncodedWidth::select(word_or);

        let row_header = Row {
            wrap: row.wrap(),
            wrap_continuation: row.wrap_continuation(),
            semantic_prompt: row.semantic_prompt() as u8,
            cell_width,
            _padding: 0,
        };

        // The bulk codec emits the row header and its encoded cells directly
        // into the destination's spare buffer capacity, so a row costs no
        // writer call at all instead of one for the header and one per cell
        // chunk.
        if bulk_codec() {
            let words = cell_words(cells);
            match cell_width {
                EncodedWidth::One | EncodedWidth::Two | EncodedWidth::Four => {
                    let size = cell_width.size();
                    let needed = 3 + count * size;
                    if let Some(out) = writer.reserve_slice(needed) {
                        out[0] = row_header.to_bits();
                        out[1..3].copy_from_slice(&(count as u16).to_le_bytes());
                        encode_narrow_into(cell_width, words, count, &mut out[3..]);
                        continue;
                    }
                }
                EncodedWidth::Eight => {
                    // Hyperlink IDs live in a native side table, but we
                    // embed them in ours, so if we have any hyperlinks we
                    // need to fall back to the loop below.
                    let witness = Cell::from_bits(word_or);
                    if !witness.hyperlink && witness.hyperlink_id == 0 {
                        let needed = 3 + count * 8;
                        if let Some(out) = writer.reserve_slice(needed) {
                            out[0] = row_header.to_bits();
                            out[1..3].copy_from_slice(&(count as u16).to_le_bytes());
                            for (chunk, word) in out[3..].chunks_exact_mut(8).zip(&words[..count]) {
                                chunk.copy_from_slice(&word.to_le_bytes());
                            }
                            continue;
                        }
                    }
                }
            }
        }

        // Row header: flags then the encoded cell count.
        {
            let mut header_bytes = [0u8; 3];
            header_bytes[0] = row_header.to_bits();
            header_bytes[1..3].copy_from_slice(&(count as u16).to_le_bytes());
            writer.write_all(&header_bytes)?;
        }

        match cell_width {
            EncodedWidth::One | EncodedWidth::Two | EncodedWidth::Four => {
                encode_narrow_cells(cell_width, &cells[..count], writer)?
            }

            EncodedWidth::Eight => {
                // Hyperlink IDs live in a native side table, but we embed
                // them in ours, so if we have any hyperlinks we need
                // to fallback to the loop below.
                if bulk_codec() {
                    let witness = Cell::from_bits(word_or);
                    if !witness.hyperlink && witness.hyperlink_id == 0 {
                        let words = cell_words(&cells[..count]);
                        // SAFETY: little-endian words are their LE bytes.
                        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), count * 8) };
                        writer.write_all(bytes)?;
                        continue;
                    }
                }

                for cell in &cells[..count] {
                    let link_id: TerminalHyperlinkId = if cell.hyperlink() {
                        page.lookup_hyperlink(cell).expect("hyperlinked cell has an ID")
                    } else {
                        0
                    };
                    io::write_int::<u64>(writer, cell_bits_of(*cell, link_id))?;
                }
            }
        }
    }

    encode_graphemes(page, writer)
}

/// The encoded cell count (through the last nonzero cell) and the bitwise
/// OR of every encoded cell word for one row.
fn scan_row(cells: &[TerminalCell]) -> (usize, u64) {
    if bulk_codec() {
        let words = cell_words(cells);

        // Count the trailing zero cells, four words at a time first.
        let mut count = cells.len();
        while count >= 4 {
            let tail = &words[count - 4..count];
            if (tail[0] | tail[1] | tail[2] | tail[3]) != 0 {
                break;
            }
            count -= 4;
        }
        while count > 0 && words[count - 1] == 0 {
            count -= 1;
        }

        // Accumulate the OR of the classification.
        let word_or = words[..count].iter().fold(0u64, |acc, w| acc | w);
        return (count, word_or);
    }

    // Scalar path, count backwards
    let mut count = cells.len();
    while count > 0 && cells[count - 1].is_zero() {
        count -= 1;
    }
    let mut word_or = 0u64;
    for cell in &cells[..count] {
        word_or |= classify_word(cell);
    }
    (count, word_or)
}

/// The word used to select a row's encoded cell width. This is the cell's
/// wire word with the hyperlink flag reflecting the native cell, so linked
/// cells and nonzero native padding disqualify every narrow width.
#[inline(always)]
fn classify_word(cell: &TerminalCell) -> u64 {
    if native_matches_wire() {
        return cell.to_bits();
    }

    let mut wire = Cell::from_bits(cell_bits_of(*cell, 0));
    wire.hyperlink = cell.hyperlink();
    wire.to_bits()
}

/// Write one row's cells truncated to the given encoded width.
///
/// Every admitted cell round-trips exactly because the row's width
/// selection proved every cell word survives `truncate` then `extend`.
fn encode_narrow_cells(width: EncodedWidth, cells: &[TerminalCell], writer: &mut dyn io::Writer) -> io::Result<()> {
    match width {
        EncodedWidth::One => encode_narrow_cells_w::<0>(cells, writer),
        EncodedWidth::Two => encode_narrow_cells_w::<1>(cells, writer),
        EncodedWidth::Four => encode_narrow_cells_w::<2>(cells, writer),
        EncodedWidth::Eight => unreachable!(),
    }
}

/// `encode_narrow_cells` for one width, given as `EncodedWidth as u8`
/// (Zig: `comptime width`), so every store has a fixed size.
fn encode_narrow_cells_w<const W: u8>(cells: &[TerminalCell], writer: &mut dyn io::Writer) -> io::Result<()> {
    let width = EncodedWidth::from_int(W);
    let size = width.size();
    let mut chunk = [0u8; 1024];
    let mut i = 0;
    while i < cells.len() {
        let n = (cells.len() - i).min(chunk.len() / size);
        for (out, cell) in chunk[..n * size].chunks_exact_mut(size).zip(&cells[i..i + n]) {
            width.write_value(out, width.truncate(classify_word(cell)));
        }
        writer.write_all(&chunk[..n * size])?;
        i += n;
    }
    Ok(())
}

/// Truncate one row's cell words into `out` at the given encoded width.
fn encode_narrow_into(width: EncodedWidth, words: &[u64], count: usize, out: &mut [u8]) {
    debug_assert!(bulk_codec());
    match width {
        EncodedWidth::One => encode_narrow_into_w::<0>(words, count, out),
        EncodedWidth::Two => encode_narrow_into_w::<1>(words, count, out),
        EncodedWidth::Four => encode_narrow_into_w::<2>(words, count, out),
        EncodedWidth::Eight => unreachable!(),
    }
}

/// `encode_narrow_into` for one width, given as `EncodedWidth as u8`
/// (Zig: `comptime width`), so every store has a fixed size.
#[inline(always)]
fn encode_narrow_into_w<const W: u8>(words: &[u64], count: usize, out: &mut [u8]) {
    let width = EncodedWidth::from_int(W);
    let size = width.size();
    for (out, &word) in out[..count * size].chunks_exact_mut(size).zip(&words[..count]) {
        width.write_value(out, width.truncate(word));
    }
}

/// Encode the grapheme suffix section for every kind 1 cell in the grid.
fn encode_graphemes(page: &Page, writer: &mut dyn io::Writer) -> Result<(), EncodeError> {
    // Every grapheme cell owns exactly one entry in the page's grapheme
    // map, so the section header comes straight from the page without a
    // counting pass over the grid. Rows without the native grapheme hint
    // contain no grapheme cells in any intact page.
    let entries = page.grapheme_count();
    io::write_int::<u32>(writer, entries as u32)?;
    if entries == 0 {
        return Ok(());
    }

    // Entries are batched into a local buffer so hot pages perform one
    // writer call per flush instead of several per entry.
    let mut buffer = [0u8; 4096];
    let mut used = 0usize;
    let mut emitted = 0usize;
    for y in 0..page.size.rows as usize {
        // SAFETY: `y` is within the page.
        let row = unsafe { &*page.get_row(y) };
        if !row.grapheme() {
            continue;
        }
        // SAFETY: the row's cells are live.
        let cells: &[TerminalCell] = unsafe { &*page.get_cells(row) };
        for (x, cell) in cells.iter().enumerate() {
            if !cell.has_grapheme() {
                continue;
            }
            let cps = page.lookup_grapheme(cell).expect("grapheme cell has data");
            if cps.len() > u16::MAX as usize {
                return Err(Error::TooManyGraphemes);
            }
            emitted += 1;

            let needed = 6 + cps.len() * 4;
            if buffer.len() - used < needed {
                writer.write_all(&buffer[..used])?;
                used = 0;
            }
            if needed > buffer.len() {
                // An entry larger than the whole buffer streams directly.
                io::write_int::<u16>(writer, y as u16)?;
                io::write_int::<u16>(writer, x as u16)?;
                io::write_int::<u16>(writer, cps.len() as u16)?;
                for &cp in cps {
                    io::write_int::<u32>(writer, cp)?;
                }
                continue;
            }

            buffer[used..used + 2].copy_from_slice(&(y as u16).to_le_bytes());
            buffer[used + 2..used + 4].copy_from_slice(&(x as u16).to_le_bytes());
            buffer[used + 4..used + 6].copy_from_slice(&(cps.len() as u16).to_le_bytes());
            used += 6;
            for &cp in cps {
                buffer[used..used + 4].copy_from_slice(&cp.to_le_bytes());
                used += 4;
            }
        }
    }
    writer.write_all(&buffer[..used])?;

    // The declared count is trusted by decoders for framing, so the grid
    // must have produced exactly that many entries.
    debug_assert_eq!(emitted, entries);
    Ok(())
}

/// Errors from `decode`: read errors, or `InvalidRowCellCount` when a row
/// declares more encoded cells than the grid has columns.
pub type DecodeError = Error;

/// Maps encoded style table IDs to page-assigned style IDs.
pub type StyleRemap = Remap<TerminalStyleId>;

/// Maps encoded hyperlink table IDs to page-assigned hyperlink IDs.
pub type HyperlinkRemap = Remap<TerminalHyperlinkId>;

/// Decode every row, cell, and grapheme suffix directly into an
/// initialized, empty page.
///
/// The grid does not encode dimensions, so `page` must already have the
/// exact row and column count expected by the containing record. Capacity
/// hints are advisory. Graphemes and cell hyperlink references that do not
/// fit are discarded without affecting the rest of the grid.
///
/// Style and hyperlink table entries must be inserted into `page` before
/// calling this function, with their encoded and page-assigned IDs recorded
/// in `style_remap` and `hyperlink_remap`. ID zero always means the default
/// style or no hyperlink. A nonzero ID missing from its remap is also
/// treated as zero so unknown table references do not prevent the rest of
/// the grid from decoding.
///
/// Invalid semantic data is normalized into a degraded form while
/// preserving the declared byte boundaries. Unknown semantic values use
/// their neutral variants, invalid Unicode becomes U+FFFD, invalid optional
/// data is ignored, and malformed wide-cell relationships become narrow
/// cells. The encoded cell count is the only structural field.
pub fn decode(
    page: &mut Page,
    reader: &mut dyn io::Reader,
    style_remap: &StyleRemap,
    hyperlink_remap: &HyperlinkRemap,
) -> Result<(), DecodeError> {
    for y in 0..page.size.rows as usize {
        // Read the row header and cell count.
        let (row_header_byte, count) = {
            // The staged payload path has every header buffered.
            let mut row_header_bytes = [0u8; 3];
            if reader.buffered_len() >= 3 {
                row_header_bytes.copy_from_slice(&reader.buffered()[..3]);
                reader.toss(3);
            } else {
                reader.read_slice_all(&mut row_header_bytes)?;
            }
            (
                row_header_bytes[0],
                u16::from_le_bytes([row_header_bytes[1], row_header_bytes[2]]) as usize,
            )
        };

        // Every bit pattern is a valid header: booleans and the exhaustive
        // width enum decode directly, and the raw semantic value gets a
        // default below. Reserved bits do not change the known fields.
        let row_header = Row::from_bits(row_header_byte);

        // A fully default row needs no work at all: decoded pages start
        // zeroed, which is exactly the default row and cell state.
        if row_header_byte == 0 && count == 0 {
            continue;
        }

        // Update the row fields through one load and store instead of a
        // read-modify-write per packed field.
        let row_ptr = page.get_row(y);
        // SAFETY: `y` is within the page; the row lives in page memory.
        let row = unsafe { &mut *row_ptr };
        let mut row_value = *row;
        row_value.set_wrap(row_header.wrap);
        row_value.set_wrap_continuation(row_header.wrap_continuation);
        row_value.set_semantic_prompt(match row_header.semantic_prompt {
            1 => SemanticPrompt::Prompt,
            2 => SemanticPrompt::PromptContinuation,
            _ => SemanticPrompt::None,
        });
        *row = row_value;

        let cells_ptr = page.get_cells(row);
        // SAFETY: the row's cells are live page memory.
        let cells: &mut [TerminalCell] = unsafe { &mut *cells_ptr };
        if count > cells.len() {
            return Err(Error::InvalidRowCellCount);
        }
        if count == 0 {
            continue;
        }

        // The encoded cell width is framing: it determines exactly how many
        // bytes this row occupies.
        match row_header.cell_width {
            EncodedWidth::One | EncodedWidth::Two => {
                decode_narrow_cells(row_header.cell_width, reader, &mut cells[..count])?
            }
            EncodedWidth::Four => {
                decode_word_cells(
                    EncodedWidth::Four,
                    page,
                    row,
                    cells,
                    count,
                    reader,
                    style_remap,
                    hyperlink_remap,
                )?;
            }
            EncodedWidth::Eight => {
                if bulk_codec() {
                    // Read the encoded words directly into page storage,
                    // then normalize them in place. The raw words are only
                    // ever touched as integers until normalization makes
                    // them valid cells.
                    {
                        // SAFETY: `Cell` is a transparent u64; the bytes are
                        // the cells' storage.
                        let bytes =
                            unsafe { std::slice::from_raw_parts_mut(cells.as_mut_ptr().cast::<u8>(), count * 8) };
                        reader.read_slice_all(bytes)?;
                    }
                    let mut row_or = 0u64;
                    for x in 0..count {
                        let word = cells[x].to_bits();
                        row_or |= word;
                        apply_cell(page, row, cells, x, word, style_remap, hyperlink_remap);
                    }
                    normalize_wide_row(EncodedWidth::Eight, row, cells, count, row_or);
                } else {
                    decode_word_cells(
                        EncodedWidth::Eight,
                        page,
                        row,
                        cells,
                        count,
                        reader,
                        style_remap,
                        hyperlink_remap,
                    )?;
                }

                // The implicit cell after a short row is narrow, which
                // resolves a trailing wide marker exactly like an explicit
                // narrow neighbor. Only full-width rows can encode wide
                // markers.
                if count < cells.len() && cells[count - 1].wide() == Wide::Wide {
                    cells[count - 1].set_wide(Wide::Narrow);
                }
            }
        }
    }

    decode_graphemes(page, reader)
}

/// Decode one row of width-one or width-two cells.
///
/// These widths admit only bare codepoints, so cells store directly with at
/// most Unicode scalar validation: no styles, hyperlinks, wide pairs, or
/// row hints are reachable and no normalization state is needed.
fn decode_narrow_cells(width: EncodedWidth, reader: &mut dyn io::Reader, cells: &mut [TerminalCell]) -> io::Result<()> {
    let size = width.size();

    // The staged payload path has the complete row buffered, making this
    // one bounds check followed by a widening loop.
    let total = cells.len() * size;
    if reader.buffered_len() >= total {
        widen_cells(width, &reader.buffered()[..total], cells);
        reader.toss(total);
        return Ok(());
    }

    // Streaming sources fall back to bounded chunks.
    let mut chunk = [0u8; 1024];
    let mut i = 0;
    while i < cells.len() {
        let n = (cells.len() - i).min(chunk.len() / size);
        reader.read_slice_all(&mut chunk[..n * size])?;
        widen_cells(width, &chunk[..n * size], &mut cells[i..i + n]);
        i += n;
    }
    Ok(())
}

/// Store codepoint-valued encoded cells of the given width.
fn widen_cells(width: EncodedWidth, bytes: &[u8], cells: &mut [TerminalCell]) {
    match width {
        EncodedWidth::One => widen_cells_w::<0>(bytes, cells),
        EncodedWidth::Two => widen_cells_w::<1>(bytes, cells),
        EncodedWidth::Four => widen_cells_w::<2>(bytes, cells),
        EncodedWidth::Eight => widen_cells_w::<3>(bytes, cells),
    }
}

/// `widen_cells` for one width, given as `EncodedWidth as u8` (Zig:
/// `comptime width`), so every load has a fixed size.
#[inline(always)]
fn widen_cells_w<const W: u8>(bytes: &[u8], cells: &mut [TerminalCell]) {
    let width = EncodedWidth::from_int(W);
    let size = width.size();
    let bytes = &bytes[..cells.len() * size];

    // When the native cell matches the wire word, this is a pure widening
    // loop over integers.
    if native_matches_wire() {
        for (cell, value) in cells.iter_mut().zip(bytes.chunks_exact(size)) {
            *cell = TerminalCell::from_bits(width.extend(widen_value(width, value)));
        }
        return;
    }

    for (cell, value) in cells.iter_mut().zip(bytes.chunks_exact(size)) {
        store_cell(cell, width.extend(widen_value(width, value)));
    }
}

/// Read and validate one narrow transported value.
#[inline(always)]
fn widen_value(width: EncodedWidth, bytes: &[u8]) -> u64 {
    let value = width.read_value(bytes);

    // Width one cannot encode an invalid scalar. Width two admits
    // surrogates, which degrade exactly like their full-width form.
    if width != EncodedWidth::One && !valid_scalar(value as u32) {
        return 0xFFFD;
    }
    value
}

/// Decode one row of width-four or fallback full-width cells through the
/// complete per-cell normalization path.
///
/// Returns the bitwise OR of every decoded wire word so callers can gate
/// the trailing wide-pair resolution pass without a second scan.
#[allow(clippy::too_many_arguments)]
fn decode_word_cells(
    width: EncodedWidth,
    page: &mut Page,
    row: &mut TerminalRow,
    cells: &mut [TerminalCell],
    count: usize,
    reader: &mut dyn io::Reader,
    style_remap: &StyleRemap,
    hyperlink_remap: &HyperlinkRemap,
) -> io::Result<u64> {
    let size = width.size();
    let mut row_or = 0u64;

    // The staged payload path has the complete row buffered.
    let total = count * size;
    if reader.buffered_len() >= total {
        let buffered = &reader.buffered()[..total];
        for x in 0..count {
            let bits = width.extend(width.read_value(&buffered[x * size..]));
            row_or |= bits;
            apply_cell(page, row, cells, x, bits, style_remap, hyperlink_remap);
        }
        reader.toss(total);
        normalize_wide_row(width, row, cells, count, row_or);
        return Ok(row_or);
    }

    // Streaming sources fall back to per-cell reads.
    for x in 0..count {
        let mut buf = [0u8; 8];
        reader.read_slice_all(&mut buf[..size])?;
        let bits = width.extend(width.read_value(&buf));
        row_or |= bits;
        apply_cell(page, row, cells, x, bits, style_remap, hyperlink_remap);
    }
    normalize_wide_row(width, row, cells, count, row_or);
    Ok(row_or)
}

/// Resolve wide-pair relationships for one decoded row.
///
/// Cell decoding stores every cell unresolved, so this pass applies
/// `normalize_wide` in order, which is equivalent to interleaving it with
/// the stores. Rows whose word OR carries no wide bits are already
/// normalized: every cell is narrow. Width four and narrower transports
/// cannot encode wide bits at all, so those rows skip the check entirely.
#[inline(always)]
fn normalize_wide_row(width: EncodedWidth, row: &TerminalRow, cells: &mut [TerminalCell], count: usize, row_or: u64) {
    if width != EncodedWidth::Eight {
        return;
    }
    if row_or & WIDE_MASK == 0 {
        return;
    }
    for x in 0..count {
        normalize_wide(row, cells, x);
    }
}

/// Whether the value is a valid Unicode scalar value.
#[inline(always)]
fn valid_scalar(cp: u32) -> bool {
    cp <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&cp)
}

/// Normalize one encoded cell word and store it at `cells[x]`.
///
/// This owns every per-cell decode rule except grapheme suffixes and
/// wide-pair resolution: content validation, reserved-value degradation,
/// and style and hyperlink remapping with reference counting. Callers run
/// `normalize_wide_row` over the stored row afterward.
fn apply_cell(
    page: &mut Page,
    row: &mut TerminalRow,
    cells: &mut [TerminalCell],
    x: usize,
    bits_wire: u64,
    style_remap: &StyleRemap,
    hyperlink_remap: &HyperlinkRemap,
) {
    let cell = &mut cells[x];

    // The default cell is the common case and needs no normalization,
    // reference counting, or table lookups.
    if bits_wire == 0 {
        store_cell(cell, 0);
        return;
    }

    // Any word is a valid wire cell because its fields are raw integers,
    // so all normalization below is plain field access.
    let mut wire = Cell::from_bits(bits_wire);

    // Hyperlink linkage is derived from the remapped ID below. The stored
    // native cell starts unlinked either way.
    let link_encoded = wire.hyperlink_id;
    wire.hyperlink_id = 0;
    wire.hyperlink = false;

    match CellKind::from_int(wire.kind) {
        // Kind 1 differs from 0 only by declaring a grapheme suffix section
        // entry, which reattaches through the native grapheme APIs later.
        CellKind::Codepoint | CellKind::CodepointGrapheme => {
            wire.kind = CellKind::Codepoint as u8;
            if !valid_scalar(wire.content) {
                wire.content = 0xFFFD;
            }

            // Kitty image and placement state is not part of this snapshot
            // version, but the placeholder is still a valid Unicode scalar.
            // Preserve it and derive the native row hint so later row
            // operations remain correct.
            if wire.content == KITTY_VIRTUAL_PLACEHOLDER {
                row.set_kitty_virtual_placeholder(true);
            }
        }

        // Only the palette index is meaningful; the remaining content bits
        // are reserved and must not obscure it.
        CellKind::BgColorPalette => wire.content &= 0xFF,

        CellKind::BgColorRgb => {}
    }

    // Reserved semantic content degrades to plain output.
    if wire.semantic_content > SemanticContent::Prompt as u8 {
        wire.semantic_content = SemanticContent::Output as u8;
    }

    // IDs belong to the encoded page. Translate them to IDs assigned by the
    // destination page before storing them on cells. The table owns one
    // reference and each decoded cell owns one additional reference.
    if wire.style_id != 0 {
        wire.style_id = style_remap.get(wire.style_id);
        if wire.style_id != 0 {
            let mem = page.memory;
            // SAFETY: `mem` is the page base and the ID is live in its set.
            unsafe { page.styles.use_(mem, wire.style_id) };
            row.set_styled(true);
        }
    }

    store_cell(cell, wire.to_bits());

    if link_encoded != 0 {
        let link_native = hyperlink_remap.get(link_encoded);
        if link_native != 0 {
            // setHyperlink records the cell mapping but intentionally does not
            // increment the set's reference count. If its map is full, undo our
            // reference and leave this cell unlinked.
            let mem = page.memory;
            // SAFETY: `mem` is the page base and the ID is live in its set.
            unsafe { page.hyperlink_set.use_(mem, link_native) };
            if page.set_hyperlink(row, cell, link_native).is_err() {
                // SAFETY: as above.
                unsafe { page.hyperlink_set.release(mem, link_native) };
            }
        }
    }
}

/// Resolve wide-pair relationships for the cell at `x` against its already
/// normalized predecessors.
fn normalize_wide(row: &TerminalRow, cells: &mut [TerminalCell], x: usize) {
    // A following cell resolves whether the previous wide marker owns a
    // tail. Normalize the current marker immediately when possible,
    // including a wide marker at the row end.
    if x > 0 && cells[x - 1].wide() == Wide::Wide && cells[x].wide() != Wide::SpacerTail {
        cells[x - 1].set_wide(Wide::Narrow);
    }
    match cells[x].wide() {
        Wide::Narrow => {}

        // A non-final wide marker remains pending until the next cell.
        Wide::Wide => {
            if x + 1 == cells.len() {
                cells[x].set_wide(Wide::Narrow);
            }
        }

        Wide::SpacerTail => {
            if x == 0 || cells[x - 1].wide() != Wide::Wide {
                cells[x].set_wide(Wide::Narrow);
            }
        }

        Wide::SpacerHead => {
            if x + 1 != cells.len() || !row.wrap() {
                cells[x].set_wide(Wide::Narrow);
            }
        }
    }
}

/// Decode the grapheme suffix section into already decoded cells.
fn decode_graphemes(page: &mut Page, reader: &mut dyn io::Reader) -> Result<(), DecodeError> {
    let entries = io::read_int::<u32>(reader)?;
    for _ in 0..entries {
        // The staged and borrowed payload paths have every header buffered.
        let (y, x, cp_count) = if reader.buffered_len() >= 6 {
            let bytes = &reader.buffered()[..6];
            let header = (
                u16::from_le_bytes([bytes[0], bytes[1]]),
                u16::from_le_bytes([bytes[2], bytes[3]]),
                u16::from_le_bytes([bytes[4], bytes[5]]),
            );
            reader.toss(6);
            header
        } else {
            (
                io::read_int::<u16>(reader)?,
                io::read_int::<u16>(reader)?,
                io::read_int::<u16>(reader)?,
            )
        };

        // Resolve the target cell. Entries whose target cannot carry a
        // suffix are optional detail: their codepoints are consumed to
        // preserve framing and then dropped.
        let target: Option<(*mut TerminalRow, *mut TerminalCell)> = 'target: {
            if y >= page.size.rows || x >= page.size.cols {
                break 'target None;
            }
            let row = page.get_row(y as usize);
            // SAFETY: `y`/`x` are within the page.
            let cell = unsafe { (*page.get_cells(&*row)).as_mut_ptr().add(x as usize) };
            // SAFETY: as above.
            let cell_ref = unsafe { &*cell };

            // Cell decoding stores every kind 1 cell as a plain codepoint,
            // so this also gives duplicate entries first-wins semantics.
            if cell_ref.content_tag() != ContentTag::Codepoint {
                break 'target None;
            }
            if cell_ref.content_codepoint() == 0 {
                break 'target None;
            }
            Some((row, cell))
        };

        // Always consume every declared codepoint. Invalid scalars and NUL
        // are not meaningful grapheme suffix components and are ignored. A
        // dropped entry's codepoints are discarded in bulk.
        let Some((row, cell)) = target else {
            reader.discard_all(cp_count as usize * 4)?;
            continue;
        };
        // SAFETY: the row and cell live in page memory, which outlives this
        // loop iteration; page methods do not move it.
        let (row, cell) = unsafe { (&mut *row, &mut *cell) };

        let mut accept = true;
        let mut index = 0usize;
        while index < cp_count as usize {
            let buffered_len = reader.buffered_len();
            if buffered_len >= 4 {
                let n = (cp_count as usize - index).min(buffered_len / 4);
                for i in 0..n {
                    let b = &reader.buffered()[i * 4..i * 4 + 4];
                    let cp = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    apply_grapheme_suffix(page, row, cell, &mut accept, cp);
                }
                reader.toss(n * 4);
                index += n;
            } else {
                let cp = io::read_int::<u32>(reader)?;
                apply_grapheme_suffix(page, row, cell, &mut accept, cp);
                index += 1;
            }
        }
    }
    Ok(())
}

/// Attach one decoded suffix codepoint to its resolved target cell.
///
/// If native capacity is exhausted, any prefix already attached is removed
/// so the cell never exposes a truncated cluster, and `accept` latches
/// false so the entry's remaining codepoints are consumed but dropped.
fn apply_grapheme_suffix(page: &mut Page, row: &mut TerminalRow, cell: &mut TerminalCell, accept: &mut bool, cp: u32) {
    if !*accept {
        return;
    }
    if cp == 0 || !valid_scalar(cp) {
        return;
    }

    if page.append_grapheme(row, cell, cp).is_err() {
        if cell.has_grapheme() {
            page.clear_grapheme(cell);
            page.update_row_grapheme_flag(row);
        }
        *accept = false;
    }
}

/// The encoded word for one native cell and its hyperlink ID.
fn cell_bits_of(cell: TerminalCell, link_id: TerminalHyperlinkId) -> u64 {
    if native_matches_wire() {
        let mut wire = Cell::from_bits(cell.to_bits());
        wire.hyperlink = link_id != 0;
        wire.hyperlink_id = link_id;
        return wire.to_bits();
    }

    let wire = Cell {
        kind: cell.content_tag() as u8,
        content: match cell.content_tag() {
            ContentTag::Codepoint | ContentTag::CodepointGrapheme => cell.content_codepoint(),
            ContentTag::BgColorPalette => cell.content_color_palette() as u32,
            ContentTag::BgColorRgb => {
                let rgb = cell.content_color_rgb();
                rgb.r as u32 | (rgb.g as u32) << 8 | (rgb.b as u32) << 16
            }
        },
        style_id: cell.style_id(),
        width: match cell.wide() {
            Wide::Narrow => 0,
            Wide::Wide => 1,
            Wide::SpacerTail => 2,
            Wide::SpacerHead => 3,
        },
        protected: cell.protected(),
        hyperlink: link_id != 0,
        semantic_content: match cell.semantic_content() {
            SemanticContent::Output => 0,
            SemanticContent::Input => 1,
            SemanticContent::Prompt => 2,
        },
        hyperlink_id: link_id,
    };
    wire.to_bits()
}

/// Store one normalized, hyperlink-free wire word as a native cell.
fn store_cell(cell: &mut TerminalCell, bits: u64) {
    if native_matches_wire() {
        *cell = TerminalCell::from_bits(bits);
        return;
    }

    let wire = Cell::from_bits(bits);
    let mut native = TerminalCell::init(0);
    match CellKind::from_int(wire.kind) {
        CellKind::Codepoint | CellKind::CodepointGrapheme => native.set_content_codepoint(wire.content),
        CellKind::BgColorPalette => {
            native.set_content_tag(ContentTag::BgColorPalette);
            native.set_content_color_palette(wire.content as u8);
        }
        CellKind::BgColorRgb => {
            native.set_content_tag(ContentTag::BgColorRgb);
            native.set_content_color_rgb(terminal_page::Rgb {
                r: wire.content as u8,
                g: (wire.content >> 8) as u8,
                b: (wire.content >> 16) as u8,
            });
        }
    }
    native.set_style_id(wire.style_id);
    native.set_wide(match wire.width {
        0 => Wide::Narrow,
        1 => Wide::Wide,
        2 => Wide::SpacerTail,
        _ => Wide::SpacerHead,
    });
    native.set_protected(wire.protected);
    native.set_semantic_content(match wire.semantic_content {
        1 => SemanticContent::Input,
        2 => SemanticContent::Prompt,
        _ => SemanticContent::Output,
    });
    *cell = native;
}

/// IDs usable with [`Remap`].
pub trait RemapId: Copy + Eq + Default {
    /// One slot for every possible encoded ID.
    const CAPACITY: usize;
    fn index(self) -> usize;
}

impl RemapId for u16 {
    const CAPACITY: usize = u16::MAX as usize + 1;
    #[inline(always)]
    fn index(self) -> usize {
        self as usize
    }
}

/// Maps encoded table IDs to IDs assigned by the destination page.
///
/// Build this by inserting each decoded table entry into the page, then
/// recording the encoded ID and the ID returned by the page's set. ID zero
/// is implicit and does not need an entry. Lookup must be cheap because the
/// grid decoder consults it for every styled or linked cell, so this is a
/// direct-indexed table rather than a hash map.
#[derive(Debug, Clone)]
pub struct Remap<Id: RemapId> {
    /// Indexed by encoded ID; zero means unmapped or default.
    pub entries: Vec<Id>,

    /// Tracks IDs that received an entry, including ones mapped to the
    /// default, so callers can give duplicate table entries first-wins
    /// semantics (Zig: `std.bit_set.Dynamic`).
    seen: Vec<u64>,
}

impl<Id: RemapId> Remap<Id> {
    /// One slot for every possible encoded ID.
    pub const CAPACITY: usize = Id::CAPACITY;

    /// A remap with no entries at all: every lookup is unmapped. Use
    /// this instead of `init` when the encoded table is empty so pages
    /// without styles or hyperlinks allocate nothing.
    pub const EMPTY: Remap<Id> = Remap {
        entries: Vec::new(),
        seen: Vec::new(),
    };

    pub fn init() -> Result<Remap<Id>, Error> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(Id::CAPACITY).map_err(|_| Error::OutOfMemory)?;
        entries.resize(Id::CAPACITY, Id::default());
        let words = Id::CAPACITY.div_ceil(64);
        let mut seen = Vec::new();
        seen.try_reserve_exact(words).map_err(|_| Error::OutOfMemory)?;
        seen.resize(words, 0);
        Ok(Remap { entries, seen })
    }

    /// Record one encoded-to-native mapping. Illegal on `EMPTY`.
    pub fn put(&mut self, encoded: Id, native: Id) {
        debug_assert!(!self.contains(encoded));
        let i = encoded.index();
        self.entries[i] = native;
        self.seen[i / 64] |= 1u64 << (i % 64);
    }

    /// Whether the encoded ID already has an entry, even a default
    /// one. Illegal on `EMPTY`.
    pub fn contains(&self, encoded: Id) -> bool {
        let i = encoded.index();
        self.seen[i / 64] & (1u64 << (i % 64)) != 0
    }

    /// The native ID for an encoded ID, or zero when unmapped.
    #[inline(always)]
    pub fn get(&self, encoded: Id) -> Id {
        if self.entries.is_empty() {
            return Id::default();
        }
        self.entries[encoded.index()]
    }

    /// Iterate the encoded IDs that received an entry, in ascending order
    /// (Zig: `seen.iterator(.{})`).
    pub fn seen_iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.seen.iter().enumerate().flat_map(|(word_index, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(word_index * 64 + bit)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::page::Capacity;
    use crate::terminal::snapshot::fixture;
    use crate::terminal::snapshot::io::{FixedReader, FixedWriter, Limited, Writer as _};

    fn rac(page: &Page, x: usize, y: usize) -> (&'static mut TerminalRow, &'static mut TerminalCell) {
        let rac = page.get_row_and_cell(x, y);
        // SAFETY: test pages outlive every use of these references.
        unsafe { (&mut *rac.row, &mut *rac.cell) }
    }

    fn cell_at(page: &Page, x: usize, y: usize) -> TerminalCell {
        *rac(page, x, y).1
    }

    fn cap(cols: u16, rows: u16) -> Capacity {
        Capacity {
            cols,
            rows,
            ..Default::default()
        }
    }

    fn remaps() -> (StyleRemap, HyperlinkRemap) {
        (StyleRemap::init().unwrap(), HyperlinkRemap::init().unwrap())
    }

    /// The fixed-width narrow codec must agree with the per-cell
    /// `truncate` / `extend` rules for every row length, including empty
    /// and short rows, and it must not write past the row.
    #[test]
    fn narrow_codec_matches_per_cell_truncate_and_extend() {
        let patterns = [
            0u64,
            0x41 << cell_bits::CONTENT,
            0xFF << cell_bits::CONTENT,
            0x100 << cell_bits::CONTENT,
            0xD7FF << cell_bits::CONTENT,
            0xD800 << cell_bits::CONTENT,
            0xDFFF << cell_bits::CONTENT,
            0xE000 << cell_bits::CONTENT,
            0xFFFF << cell_bits::CONTENT,
            0x1F_FFFF << cell_bits::CONTENT,
            u64::MAX,
            0x3,
            (0xBEEFu64 << cell_bits::STYLE_ID) | (0x41 << cell_bits::CONTENT),
        ];
        for width in [EncodedWidth::One, EncodedWidth::Two, EncodedWidth::Four] {
            for count in [0usize, 1, 3, 4, 5, 7, 8, 17] {
                let words: Vec<u64> = (0..count)
                    .map(|i| patterns[i % patterns.len()].wrapping_mul(i as u64 + 1))
                    .collect();
                let size = width.size();
                let mut fast = vec![0u8; count * size + 8];
                encode_narrow_into(width, &words, count, &mut fast);
                assert_eq!(&fast[count * size..], &[0u8; 8], "{width:?} count={count} wrote past the row");

                let mut slow = vec![0u8; count * size];
                for (j, &word) in words.iter().enumerate() {
                    width.write_value(&mut slow[j * size..], width.truncate(word));
                }
                assert_eq!(&fast[..count * size], slow.as_slice(), "{width:?} count={count}");

                if native_matches_wire() && matches!(width, EncodedWidth::One | EncodedWidth::Two) {
                    let mut cells = vec![TerminalCell::init(0); count];
                    widen_cells(width, &fast[..count * size], &mut cells);
                    for (j, cell) in cells.iter().enumerate() {
                        let value = width.read_value(&slow[j * size..]);
                        let value = if width != EncodedWidth::One && !valid_scalar(value as u32) {
                            0xFFFD
                        } else {
                            value
                        };
                        assert_eq!(
                            cell.to_bits(),
                            width.extend(value),
                            "{width:?} count={count} cell={j}"
                        );
                    }
                }
            }
        }

        // The streaming fallback truncates through `classify_word`, which is
        // the identity when the native cell matches the wire word.
        let cells: Vec<TerminalCell> = (0..17u32).map(TerminalCell::init).collect();
        for width in [EncodedWidth::One, EncodedWidth::Two, EncodedWidth::Four] {
            let mut got = Vec::new();
            encode_narrow_cells(width, &cells, &mut got).unwrap();
            let size = width.size();
            let mut expect = vec![0u8; cells.len() * size];
            for (j, cell) in cells.iter().enumerate() {
                width.write_value(&mut expect[j * size..], width.truncate(classify_word(cell)));
            }
            assert_eq!(got, expect, "{width:?}");
        }
    }

    #[test]
    fn cell_wire_layout_registry() {
        // The wire bit positions are format constants. The shifts and masks
        // derive from the packed struct, so pin the struct itself to the
        // documented registry so an edit cannot silently move wire bits.
        assert_eq!(0, cell_bits::KIND);
        assert_eq!(2, cell_bits::CONTENT);
        assert_eq!(26, cell_bits::STYLE_ID);
        assert_eq!(42, cell_bits::WIDTH);
        assert_eq!(44, cell_bits::PROTECTED);
        assert_eq!(45, cell_bits::HYPERLINK);
        assert_eq!(46, cell_bits::SEMANTIC_CONTENT);
        assert_eq!(48, cell_bits::HYPERLINK_ID);
        assert_eq!(1u64 << 0, Cell { kind: 1, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 2, Cell { content: 1, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 26, Cell { style_id: 1, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 42, Cell { width: 1, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 44, Cell { protected: true, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 45, Cell { hyperlink: true, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 46, Cell { semantic_content: 1, ..Default::default() }.to_bits());
        assert_eq!(1u64 << 48, Cell { hyperlink_id: 1, ..Default::default() }.to_bits());

        // The native cell currently matches the wire registry, which enables
        // the bulk row codec. If this fails, the native layout diverged: either
        // restore it or accept the portable codec and update this expectation.
        assert!(native_matches_wire());
    }

    #[test]
    fn encoded_cell_width_transport_registry() {
        // Pin the transported value and admission mask of every width against
        // the documented format: widths one and two carry the bare codepoint,
        // width four the low word half, width eight the complete word.
        let word = Cell {
            kind: 1,
            content: 0xABCDEF,
            style_id: 0x1234,
            hyperlink_id: 0x5678,
            ..Default::default()
        }
        .to_bits();
        assert_eq!(0xEF, EncodedWidth::One.truncate(word));
        assert_eq!(0xCDEF, EncodedWidth::Two.truncate(word));
        assert_eq!(word as u32 as u64, EncodedWidth::Four.truncate(word));
        assert_eq!(word, EncodedWidth::Eight.truncate(word));

        assert_eq!(0x0000_0000_0000_03FCu64, EncodedWidth::One.mask());
        assert_eq!(0x0000_0000_0003_FFFCu64, EncodedWidth::Two.mask());
        assert_eq!(0x0000_0000_FFFF_FFFFu64, EncodedWidth::Four.mask());
        assert_eq!(0xFFFF_FFFF_FFFF_FFFFu64, EncodedWidth::Eight.mask());

        // Selection returns the smallest admissible width, and every admitted
        // word round-trips through its transport.
        let d = Cell::default();
        let cases: [(Cell, EncodedWidth); 12] = [
            (d, EncodedWidth::One),
            (Cell { content: 0xFF, ..d }, EncodedWidth::One),
            (Cell { content: 0x100, ..d }, EncodedWidth::Two),
            (Cell { content: 0xFFFF, ..d }, EncodedWidth::Two),
            (Cell { content: 0x10000, ..d }, EncodedWidth::Four),
            (Cell { kind: 2, content: 7, ..d }, EncodedWidth::Four),
            (
                Cell {
                    content: 'a' as u32,
                    style_id: 63,
                    ..d
                },
                EncodedWidth::Four,
            ),
            (
                Cell {
                    content: 'a' as u32,
                    style_id: 64,
                    ..d
                },
                EncodedWidth::Eight,
            ),
            (
                Cell {
                    width: 1,
                    content: 'a' as u32,
                    ..d
                },
                EncodedWidth::Eight,
            ),
            (Cell { protected: true, ..d }, EncodedWidth::Eight),
            (Cell { semantic_content: 1, ..d }, EncodedWidth::Eight),
            (
                Cell {
                    hyperlink: true,
                    hyperlink_id: 1,
                    ..d
                },
                EncodedWidth::Eight,
            ),
        ];
        for (cell, width) in cases {
            let case_word = cell.to_bits();
            assert_eq!(width, EncodedWidth::select(case_word));
            assert_eq!(case_word, width.extend(width.truncate(case_word)));
        }
    }

    #[test]
    fn grid_golden_encoding_and_decoding() {
        let capacity = Capacity {
            cols: 3,
            rows: 4,
            styles: 0,
            hyperlink_bytes: 0,
            grapheme_bytes: 64,
            string_bytes: 0,
        };
        let mut source = Page::init(capacity).unwrap();

        // The first row covers semantic metadata, a wide-cell pair, and a
        // multi-codepoint grapheme.
        let (row, cell) = rac(&source, 0, 0);
        row.set_semantic_prompt(SemanticPrompt::Prompt);
        *cell = TerminalCell::init('A' as u32);
        cell.set_wide(Wide::Wide);
        cell.set_protected(true);
        cell.set_semantic_content(SemanticContent::Prompt);

        let (_, tail) = rac(&source, 1, 0);
        tail.set_wide(Wide::SpacerTail);
        tail.set_semantic_content(SemanticContent::Input);

        let (grow, gcell) = rac(&source, 2, 0);
        *gcell = TerminalCell::init('x' as u32);
        source.set_graphemes(grow, gcell, &[0x0301, 0x0302]).unwrap();

        // The second row covers both background-color kinds and a wrapped spacer
        // head with the remaining row semantic value.
        let (_, palette) = rac(&source, 0, 1);
        palette.set_content_tag(ContentTag::BgColorPalette);
        palette.set_content_color_palette(7);

        let (_, rgb) = rac(&source, 1, 1);
        rgb.set_content_tag(ContentTag::BgColorRgb);
        rgb.set_content_color_rgb(terminal_page::Rgb {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        });
        rgb.set_protected(true);

        let (hrow, head) = rac(&source, 2, 1);
        head.set_wide(Wide::SpacerHead);
        hrow.set_wrap(true);
        hrow.set_wrap_continuation(true);
        hrow.set_semantic_prompt(SemanticPrompt::PromptContinuation);

        // The third row is bare ASCII text with an interior blank, which uses
        // the one-byte encoded cell width.
        *rac(&source, 0, 2).1 = TerminalCell::init('h' as u32);
        *rac(&source, 2, 2).1 = TerminalCell::init('i' as u32);

        // The fourth row needs the two-byte width for a codepoint above U+00FF.
        *rac(&source, 0, 3).1 = TerminalCell::init(0x0416); // Ж
        *rac(&source, 1, 3).1 = TerminalCell::init('!' as u32);

        let mut encoded = [0u8; 160];
        let mut writer = FixedWriter::new(&mut encoded);
        encode(&source, &mut writer).unwrap();
        let reference = fixture::parse(include_str!("testdata/grid-v1.hex"));
        fixture::expect_equal(
            fixture::Kind::Bytes,
            "src/terminal/snapshot/testdata/grid-v1.hex",
            "snapshot_fixture-grid-v1.hex",
            &reference,
            writer.buffered(),
        );

        let mut destination = Page::init(capacity).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        // Decode the checked-in reference through a one-byte reader buffer.
        let mut limited = Limited::new(FixedReader::new(&reference), None, 1);
        decode(&mut destination, &mut limited, &style_remap, &hyperlink_remap).unwrap();
        destination.verify_integrity().unwrap();

        let (wrow, wcell) = rac(&destination, 0, 0);
        assert_eq!('A' as u32, wcell.codepoint());
        assert_eq!(Wide::Wide, wcell.wide());
        assert!(wcell.protected());
        assert_eq!(SemanticContent::Prompt, wcell.semantic_content());
        assert_eq!(SemanticPrompt::Prompt, wrow.semantic_prompt());

        let tail = cell_at(&destination, 1, 0);
        assert_eq!(Wide::SpacerTail, tail.wide());
        assert_eq!(SemanticContent::Input, tail.semantic_content());

        let gcell = cell_at(&destination, 2, 0);
        let gref = rac(&destination, 2, 0).1;
        assert!(gcell.has_grapheme());
        assert_eq!(&[0x0301u32, 0x0302][..], destination.lookup_grapheme(gref).unwrap());

        let palette = cell_at(&destination, 0, 1);
        assert_eq!(ContentTag::BgColorPalette, palette.content_tag());
        assert_eq!(7, palette.content_color_palette());

        let rgb = cell_at(&destination, 1, 1);
        assert_eq!(ContentTag::BgColorRgb, rgb.content_tag());
        assert_eq!(
            terminal_page::Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            },
            rgb.content_color_rgb()
        );
        assert!(rgb.protected());

        let (hrow, head) = rac(&destination, 2, 1);
        assert_eq!(Wide::SpacerHead, head.wide());
        assert!(hrow.wrap());
        assert!(hrow.wrap_continuation());
        assert_eq!(SemanticPrompt::PromptContinuation, hrow.semantic_prompt());

        assert_eq!('h' as u32, cell_at(&destination, 0, 2).codepoint());
        assert!(cell_at(&destination, 1, 2).is_zero());
        assert_eq!('i' as u32, cell_at(&destination, 2, 2).codepoint());
        assert_eq!(0x0416, cell_at(&destination, 0, 3).codepoint());
        assert_eq!('!' as u32, cell_at(&destination, 1, 3).codepoint());

        // A re-encode proves the decoded native page retains every wire field.
        let mut reencoded = [0u8; 160];
        let mut rewriter = FixedWriter::new(&mut reencoded);
        encode(&destination, &mut rewriter).unwrap();
        assert_eq!(&reference[..], rewriter.buffered());
    }

    #[test]
    fn grid_elides_trailing_default_cells() {
        let mut page = Page::init(cap(80, 3)).unwrap();

        // Row 0 is fully default. Row 1 has content in columns zero and two.
        // Row 2 has one protected-only cell at column four.
        *rac(&page, 2, 1).1 = TerminalCell::init('b' as u32);
        *rac(&page, 0, 1).1 = TerminalCell::init('a' as u32);
        rac(&page, 4, 2).1.set_protected(true);

        let mut encoded = [0u8; 256];
        let mut writer = FixedWriter::new(&mut encoded);
        encode(&page, &mut writer).unwrap();

        // 3 bytes per row header and cells only through the last non-default
        // cell, followed by an empty grapheme section. The text row uses the
        // one-byte width while the protected flag forces the full width.
        assert_eq!(3 + (3 + 3) + (3 + 5 * 8) + 4, writer.buffered().len());

        let mut destination = Page::init(cap(80, 3)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut destination, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        destination.verify_integrity().unwrap();

        assert_eq!('a' as u32, cell_at(&destination, 0, 1).codepoint());
        assert_eq!('b' as u32, cell_at(&destination, 2, 1).codepoint());
        assert!(cell_at(&destination, 4, 2).protected());
        assert!(cell_at(&destination, 79, 1).is_zero());
        let _ = &mut page;
    }

    #[test]
    fn grid_rejects_a_row_cell_count_above_the_column_count() {
        let mut page = Page::init(cap(2, 1)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 64];
        let mut writer = FixedWriter::new(&mut payload);
        writer.write_byte(Row::default().to_bits()).unwrap();
        io::write_int::<u16>(&mut writer, 3).unwrap();
        for _ in 0..3 {
            io::write_int::<u64>(&mut writer, 0).unwrap();
        }
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        assert_eq!(
            Err(Error::InvalidRowCellCount),
            decode(&mut page, &mut reader, &style_remap, &hyperlink_remap)
        );
    }

    fn eight_row() -> u8 {
        Row {
            cell_width: EncodedWidth::Eight,
            ..Default::default()
        }
        .to_bits()
    }

    #[test]
    fn grid_normalizes_incomplete_wide_cells() {
        // One column puts the wide cell at the row end. Two columns put an
        // ordinary narrow cell after it. Neither case supplies a spacer tail.
        // Both rely on the implicit narrow cell rule when the tail is elided.
        for columns in [1u16, 2] {
            // Craft the malformed row directly on the wire. Decoding keeps its
            // content but makes the incomplete wide cell narrow.
            let mut payload = [0u8; 64];
            let mut payload_writer = FixedWriter::new(&mut payload);
            payload_writer.write_byte(eight_row()).unwrap();
            io::write_int::<u16>(&mut payload_writer, columns).unwrap();
            io::write_int::<u64>(
                &mut payload_writer,
                Cell {
                    width: 1, // wide
                    content: 'A' as u32,
                    ..Default::default()
                }
                .to_bits(),
            )
            .unwrap();
            if columns > 1 {
                io::write_int::<u64>(
                    &mut payload_writer,
                    Cell {
                        content: 'B' as u32,
                        ..Default::default()
                    }
                    .to_bits(),
                )
                .unwrap();
            }
            io::write_int::<u32>(&mut payload_writer, 0).unwrap();

            let mut destination = Page::init(cap(columns, 1)).unwrap();
            let (style_remap, hyperlink_remap) = remaps();

            let mut payload_reader = FixedReader::new(payload_writer.buffered());
            decode(&mut destination, &mut payload_reader, &style_remap, &hyperlink_remap).unwrap();
            destination.verify_integrity().unwrap();

            assert_eq!('A' as u32, cell_at(&destination, 0, 0).codepoint());
            assert_eq!(Wide::Narrow, cell_at(&destination, 0, 0).wide());
            if columns > 1 {
                let second = cell_at(&destination, 1, 0);
                assert_eq!('B' as u32, second.codepoint());
                assert_eq!(Wide::Narrow, second.wide());
            }
        }

        // A wide marker whose spacer tail was elided by a short cell count is
        // also normalized, and the trailing cells stay default.
        let mut page = Page::init(cap(4, 1)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 64];
        let mut writer = FixedWriter::new(&mut payload);
        writer.write_byte(eight_row()).unwrap();
        io::write_int::<u16>(&mut writer, 1).unwrap();
        io::write_int::<u64>(
            &mut writer,
            Cell {
                width: 1, // wide
                content: 'W' as u32,
                ..Default::default()
            }
            .to_bits(),
        )
        .unwrap();
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();
        assert_eq!(Wide::Narrow, cell_at(&page, 0, 0).wide());
        assert!(cell_at(&page, 1, 0).is_zero());
    }

    #[test]
    fn grid_normalizes_reserved_cell_values() {
        let mut page = Page::init(cap(3, 1)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 64];
        let mut writer = FixedWriter::new(&mut payload);

        // Reserved row flag bits six and seven are ignored while the unknown
        // semantic-prompt value degrades to none and wrap survives. Bits four
        // and five select the full encoded cell width.
        writer.write_byte(0xFD).unwrap();
        io::write_int::<u16>(&mut writer, 3).unwrap();

        // A surrogate codepoint with reserved semantic content 3.
        io::write_int::<u64>(
            &mut writer,
            Cell {
                content: 0xD800,
                semantic_content: 3,
                ..Default::default()
            }
            .to_bits(),
        )
        .unwrap();

        // Reserved palette content bits do not obscure the palette index.
        io::write_int::<u64>(
            &mut writer,
            Cell {
                kind: 2,
                content: 0xFFFF07,
                ..Default::default()
            }
            .to_bits(),
        )
        .unwrap();

        // An unknown style reference degrades to the default style, and an
        // unknown hyperlink reference leaves the cell unlinked.
        io::write_int::<u64>(
            &mut writer,
            Cell {
                content: 'x' as u32,
                style_id: 5,
                hyperlink: true,
                hyperlink_id: 9,
                ..Default::default()
            }
            .to_bits(),
        )
        .unwrap();
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();

        let (first_row, first) = rac(&page, 0, 0);
        assert!(first_row.wrap());
        assert_eq!(SemanticPrompt::None, first_row.semantic_prompt());
        assert_eq!(0xFFFD, first.codepoint());
        assert_eq!(SemanticContent::Output, first.semantic_content());

        let second = cell_at(&page, 1, 0);
        assert_eq!(ContentTag::BgColorPalette, second.content_tag());
        assert_eq!(7, second.content_color_palette());

        let third = rac(&page, 2, 0).1;
        assert_eq!('x' as u32, third.codepoint());
        assert_eq!(0, third.style_id());
        assert!(!third.hyperlink());
        assert_eq!(None, page.lookup_hyperlink(third));
    }

    #[test]
    fn grid_drops_undeliverable_grapheme_entries() {
        let mut page = Page::init(Capacity {
            cols: 4,
            rows: 1,
            grapheme_bytes: 64,
            ..Default::default()
        })
        .unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 128];
        let mut writer = FixedWriter::new(&mut payload);
        writer.write_byte(eight_row()).unwrap();
        io::write_int::<u16>(&mut writer, 4).unwrap();
        let d = Cell::default();
        // A kind 1 cell that receives a valid entry.
        io::write_int::<u64>(&mut writer, Cell { kind: 1, content: 'x' as u32, ..d }.to_bits()).unwrap();
        // A kind 1 cell without an entry decodes as a plain codepoint.
        io::write_int::<u64>(&mut writer, Cell { kind: 1, content: 'y' as u32, ..d }.to_bits()).unwrap();
        // A background cell cannot carry a suffix.
        io::write_int::<u64>(&mut writer, Cell { kind: 2, content: 7, ..d }.to_bits()).unwrap();
        // An empty codepoint cannot carry a suffix.
        io::write_int::<u64>(&mut writer, d.to_bits()).unwrap();

        io::write_int::<u32>(&mut writer, 5).unwrap();
        let entry = |w: &mut FixedWriter<'_>, y: u16, x: u16, cps: &[u32]| {
            io::write_int::<u16>(w, y).unwrap();
            io::write_int::<u16>(w, x).unwrap();
            io::write_int::<u16>(w, cps.len() as u16).unwrap();
            for &cp in cps {
                io::write_int::<u32>(w, cp).unwrap();
            }
        };
        // Valid entry with one invalid scalar dropped from within it.
        entry(&mut writer, 0, 0, &[0x0301, 0, 0xD800, 0x0302]);
        // Duplicate entry for the same cell is consumed and dropped.
        entry(&mut writer, 0, 0, &[0x0303]);
        // Entry for the background cell is consumed and dropped.
        entry(&mut writer, 0, 2, &[0x0301]);
        // Entry for the empty cell is consumed and dropped.
        entry(&mut writer, 0, 3, &[0x0301]);
        // Entry outside the grid is consumed and dropped.
        entry(&mut writer, 7, 0, &[0x0301]);

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();

        let first = rac(&page, 0, 0).1;
        assert_eq!(&[0x0301u32, 0x0302][..], page.lookup_grapheme(first).unwrap());
        let second = cell_at(&page, 1, 0);
        assert_eq!('y' as u32, second.codepoint());
        assert!(!second.has_grapheme());
        assert!(!cell_at(&page, 2, 0).has_grapheme());
        assert!(!cell_at(&page, 3, 0).has_grapheme());
    }

    #[test]
    fn grid_drops_a_complete_grapheme_when_capacity_fails_mid_cluster() {
        let mut page = Page::init(Capacity {
            cols: 4,
            rows: 1,
            grapheme_bytes: 512,
            ..Default::default()
        })
        .unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 1024];
        let mut writer = FixedWriter::new(&mut payload);
        writer.write_byte(eight_row()).unwrap();
        io::write_int::<u16>(&mut writer, 4).unwrap();
        for i in 0..4u32 {
            io::write_int::<u64>(
                &mut writer,
                Cell {
                    kind: 1,
                    content: 'x' as u32 + i,
                    ..Default::default()
                }
                .to_bits(),
            )
            .unwrap();
        }

        // BitmapAllocator rounds this capacity to 64 four-codepoint chunks. The
        // first three cells consume 34 chunks. On the fourth cell, appending the
        // 61st suffix needs 16 new chunks while the old 15-chunk slice is still
        // live, exceeding capacity and forcing the complete suffix to be dropped.
        io::write_int::<u32>(&mut writer, 4).unwrap();
        for (x, count) in [64u16, 64, 8, 61].into_iter().enumerate() {
            io::write_int::<u16>(&mut writer, 0).unwrap();
            io::write_int::<u16>(&mut writer, x as u16).unwrap();
            io::write_int::<u16>(&mut writer, count).unwrap();
            for i in 0..count as u32 {
                io::write_int::<u32>(&mut writer, 0x0300 + i).unwrap();
            }
        }

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();

        let (row, cell) = rac(&page, 3, 0);
        assert_eq!('x' as u32 + 3, cell.codepoint());
        assert!(!cell.has_grapheme());
        assert!(row.grapheme());
        assert_eq!(3, page.grapheme_count());
    }

    #[test]
    fn grid_encodes_rows_at_their_narrowest_width() {
        let caps = Capacity {
            cols: 2,
            rows: 4,
            styles: 8,
            ..Default::default()
        };
        let mut page = Page::init(caps).unwrap();

        // Width zero: bare Latin-1 text.
        *rac(&page, 0, 0).1 = TerminalCell::init('A' as u32);
        *rac(&page, 1, 0).1 = TerminalCell::init(0xFF);

        // Width one: any BMP codepoint.
        *rac(&page, 0, 1).1 = TerminalCell::init(0x0100);

        // Width two: a small style ID and a background color.
        let mem = page.memory;
        let bold = terminal_style::Style {
            flags: terminal_style::Flags {
                bold: true,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `mem` is the page base.
        let style_id = unsafe { page.styles.add(mem, bold) }.unwrap();
        assert!(style_id <= 63);
        let (srow, styled) = rac(&page, 0, 2);
        *styled = TerminalCell::init('s' as u32);
        styled.set_style_id(style_id);
        srow.set_styled(true);
        let (_, bg) = rac(&page, 1, 2);
        bg.set_content_tag(ContentTag::BgColorPalette);
        bg.set_content_color_palette(7);

        // Width three: a protected cell.
        rac(&page, 0, 3).1.set_protected(true);

        let mut encoded = [0u8; 128];
        let mut writer = FixedWriter::new(&mut encoded);
        encode(&page, &mut writer).unwrap();

        let bytes = writer.buffered().to_vec();
        assert_eq!((3 + 2) + (3 + 2) + (3 + 2 * 4) + (3 + 8) + 4, bytes.len());

        // Each row header carries the expected width bits.
        assert_eq!(0u8 << 4, bytes[0] & 0x30);
        assert_eq!(1u8 << 4, bytes[5] & 0x30);
        assert_eq!(2u8 << 4, bytes[10] & 0x30);
        assert_eq!(3u8 << 4, bytes[21] & 0x30);

        let mut destination = Page::init(caps).unwrap();
        let (mut style_remap, hyperlink_remap) = remaps();
        let dmem = destination.memory;
        // SAFETY: `dmem` is the page base.
        let native_style = unsafe { destination.styles.add(dmem, bold) }.unwrap();
        style_remap.put(style_id, native_style);

        let mut reader = FixedReader::new(&bytes);
        decode(&mut destination, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        destination.verify_integrity().unwrap();

        assert_eq!('A' as u32, cell_at(&destination, 0, 0).codepoint());
        assert_eq!(0xFF, cell_at(&destination, 1, 0).codepoint());
        assert_eq!(0x0100, cell_at(&destination, 0, 1).codepoint());
        let (drow, dstyled) = rac(&destination, 0, 2);
        assert_eq!('s' as u32, dstyled.codepoint());
        assert_eq!(native_style, dstyled.style_id());
        assert!(drow.styled());
        assert_eq!(7, cell_at(&destination, 1, 2).content_color_palette());
        assert!(cell_at(&destination, 0, 3).protected());
    }

    #[test]
    fn grid_decodes_non_canonical_cell_widths() {
        let mut page = Page::init(cap(2, 1)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        // A bare ASCII row encoded at the full width is wasteful but valid.
        let mut payload = [0u8; 64];
        let mut writer = FixedWriter::new(&mut payload);
        writer.write_byte(eight_row()).unwrap();
        io::write_int::<u16>(&mut writer, 2).unwrap();
        io::write_int::<u64>(&mut writer, Cell { content: 'o' as u32, ..Default::default() }.to_bits()).unwrap();
        io::write_int::<u64>(&mut writer, Cell { content: 'k' as u32, ..Default::default() }.to_bits()).unwrap();
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();
        assert_eq!('o' as u32, cell_at(&page, 0, 0).codepoint());
        assert_eq!('k' as u32, cell_at(&page, 1, 0).codepoint());

        // Re-encoding canonicalizes the row back to the one-byte width.
        let mut reencoded = [0u8; 16];
        let mut rewriter = FixedWriter::new(&mut reencoded);
        encode(&page, &mut rewriter).unwrap();
        assert_eq!(3 + 2 + 4, rewriter.buffered().len());
    }

    #[test]
    fn grid_normalizes_surrogates_in_two_byte_cells() {
        let mut page = Page::init(cap(2, 1)).unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 16];
        let mut writer = FixedWriter::new(&mut payload);
        writer
            .write_byte(
                Row {
                    cell_width: EncodedWidth::Two,
                    ..Default::default()
                }
                .to_bits(),
            )
            .unwrap();
        io::write_int::<u16>(&mut writer, 2).unwrap();
        io::write_int::<u16>(&mut writer, 0xD800).unwrap();
        io::write_int::<u16>(&mut writer, 0x0416).unwrap();
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();
        assert_eq!(0xFFFD, cell_at(&page, 0, 0).codepoint());
        assert_eq!(0x0416, cell_at(&page, 1, 0).codepoint());
    }

    #[test]
    fn grid_four_byte_cells_run_full_normalization() {
        let mut page = Page::init(Capacity {
            cols: 3,
            rows: 1,
            styles: 8,
            ..Default::default()
        })
        .unwrap();
        let (style_remap, hyperlink_remap) = remaps();

        let mut payload = [0u8; 32];
        let mut writer = FixedWriter::new(&mut payload);
        writer
            .write_byte(
                Row {
                    cell_width: EncodedWidth::Four,
                    ..Default::default()
                }
                .to_bits(),
            )
            .unwrap();
        io::write_int::<u16>(&mut writer, 3).unwrap();

        // The Kitty placeholder does not fit two-byte cells but fits here
        // and must still derive the native row hint.
        let d = Cell::default();
        io::write_int::<u32>(
            &mut writer,
            Cell {
                content: KITTY_VIRTUAL_PLACEHOLDER,
                ..d
            }
            .to_bits() as u32,
        )
        .unwrap();

        // An unknown small style reference degrades to the default style.
        io::write_int::<u32>(
            &mut writer,
            Cell {
                content: 'x' as u32,
                style_id: 63,
                ..d
            }
            .to_bits() as u32,
        )
        .unwrap();

        // Reserved palette content bits are cleared at this width too.
        io::write_int::<u32>(
            &mut writer,
            Cell {
                kind: 2,
                content: 0xFFFF07,
                ..d
            }
            .to_bits() as u32,
        )
        .unwrap();
        io::write_int::<u32>(&mut writer, 0).unwrap();

        let mut reader = FixedReader::new(writer.buffered());
        decode(&mut page, &mut reader, &style_remap, &hyperlink_remap).unwrap();
        page.verify_integrity().unwrap();

        let (first_row, first) = rac(&page, 0, 0);
        assert_eq!(KITTY_VIRTUAL_PLACEHOLDER, first.codepoint());
        assert!(first_row.kitty_virtual_placeholder());

        let second = cell_at(&page, 1, 0);
        assert_eq!('x' as u32, second.codepoint());
        assert_eq!(0, second.style_id());

        let third = cell_at(&page, 2, 0);
        assert_eq!(ContentTag::BgColorPalette, third.content_tag());
        assert_eq!(7, third.content_color_palette());
    }
}
