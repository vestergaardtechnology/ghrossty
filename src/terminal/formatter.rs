//! Port of `src/terminal/formatter.zig`.
//!
//! STATUS: ported
//!
//! Notes on the port:
//! - Writers are `std::io::Write` trait objects; Zig's
//!   `std.Io.Writer.Error` is `std::io::Error`.
//! - The comptime `emit` specialization is a runtime match.
//! - Nested Zig types are flattened: `PinMap.Map` is [`PinMapMap`],
//!   `PinMap.NodeRun` is [`PinMapNodeRun`], `ScreenFormatter.Content` is
//!   [`ScreenFormatterContent`], `ScreenFormatter.Extra` is
//!   [`ScreenFormatterExtra`], `ScreenFormatter.CursorOrigin` is
//!   [`CursorOrigin`], `PageFormatter.PointMap` is [`PointMap`] and
//!   `PageFormatter.TrailingState` is [`TrailingState`].
//! - `CodepointMap` lists are a `Vec` (Zig: `std.MultiArrayList`).
//! - `TerminalFormatter.Extra` is [`TerminalFormatterExtra`].

use std::io::{self, Write};

use crate::terminal::charsets;
use crate::terminal::color;
use crate::terminal::hyperlink;
use crate::terminal::kitty;
use crate::terminal::modes;
use crate::terminal::page::{Cell, ContentTag, Page, Row, Wide};
use crate::terminal::page_list::{Direction, NodePtr, PageList, Pin};
use crate::terminal::point::{Coordinate, Tag};
use crate::terminal::screen::Screen;
use crate::terminal::selection::Selection;
use crate::terminal::size::CellCountInt;
use crate::terminal::style::{self, Style};
use crate::terminal::terminal::Terminal;

/// Formats available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum Format {
    /// Plain text.
    Plain,

    /// Include VT sequences to preserve colors, styles, URLs, etc.
    /// This is predominantly SGR sequences but may contain others as needed.
    ///
    /// Note that for reference colors, like palette indices, this will
    /// vary based on the formatter and you should see the docs. For example,
    /// PageFormatter with VT will emit SGR sequences with palette indices,
    /// not the color itself.
    ///
    /// For VT, newlines will be emitted as `\r\n` so that the cursor properly
    /// moves back to the beginning prior emitting follow-up lines.
    Vt,

    /// HTML output.
    ///
    /// This will emit inline styles for as much styling as possible,
    /// in the interest of simplicity and ease of editing. This isn't meant
    /// to build the most beautiful or efficient HTML, but rather to be
    /// stylistically correct.
    ///
    /// For colors, RGB values are emitted as inline CSS (#RRGGBB) while palette
    /// indices use CSS variables (var(--vt-palette-N)). The palette colors are
    /// emitted by TerminalFormatter.Extra.palette as a <style> block if you
    /// want to also include that. But if you only format a screen or lower,
    /// the formatter doesn't have access to the current palette to render it.
    ///
    /// Newlines are emitted as actual '\n' characters. Consumers should use
    /// CSS white-space: pre or pre-wrap to preserve spacing and alignment.
    Html,
}

/// Returns true if the format emits styled output (not plaintext).
/// The `Format` with the given discriminant, for code specialized on the
/// format through a const generic (Zig: `comptime emit: Format`).
const fn format_from_const(emit: u8) -> Format {
    match emit {
        0 => Format::Plain,
        1 => Format::Vt,
        _ => Format::Html,
    }
}

pub fn format_styled(fmt: Format) -> bool {
    match fmt {
        Format::Plain => false,
        Format::Html | Format::Vt => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodepointMap {
    /// Unicode codepoint range to replace.
    /// Asserts: range[0] <= range[1]
    pub range: [u32; 2],

    /// Replacement value for this range.
    pub replacement: Replacement,
}

/// (Zig: `CodepointMap.Replacement`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// A single replacement codepoint.
    Codepoint(u32),

    /// A UTF-8 encoded string to replace with.
    String(String),
}

/// Common encoding options regardless of what exact formatter is used.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// The format to emit.
    pub emit: Format,

    /// Whether to unwrap soft-wrapped lines. If false, this will emit the
    /// screen contents as it is rendered on the page in the given size.
    pub unwrap: bool,

    /// Trim trailing whitespace on lines with other text. Trailing blank
    /// lines are always trimmed, except in VT output without trimming,
    /// where they are part of the layout. This only affects trailing
    /// whitespace on rows that have at least one other cell with text.
    /// Whitespace is currently only space characters (0x20).
    pub trim: bool,

    /// Replace matching Unicode codepoints with some other values.
    /// This will use the last matching range found in the list.
    pub codepoint_map: Option<Vec<CodepointMap>>,

    /// Set a background and foreground color to use for the "screen".
    /// For styled formats, this will emit the proper sequences or styles.
    pub background: Option<color::Rgb>,
    pub foreground: Option<color::Rgb>,

    /// If set, then styled formats in `emit` will use this palette to
    /// emit colors directly as RGB. If this is null, styled formats will
    /// still work but will use deferred palette styling (e.g. CSS variables
    /// for HTML or the actual palette indexes for VT).
    pub palette: Option<&'a color::Palette>,
}

impl Options<'static> {
    pub const PLAIN: Options<'static> = Options::new(Format::Plain);
    pub const VT: Options<'static> = Options::new(Format::Vt);
    pub const HTML: Options<'static> = Options::new(Format::Html);

    /// Options with the given format and all other fields defaulted.
    pub const fn new(emit: Format) -> Options<'static> {
        Options {
            emit,
            unwrap: false,
            trim: true,
            codepoint_map: Some(Vec::new()),
            background: None,
            foreground: None,
            palette: None,
        }
    }
}

/// Maps byte positions in formatted output to PageList pins.
///
/// Used by formatters that operate on PageLists to track the source position
/// of each byte written. The caller is responsible for freeing the map.
///
/// The mapping is stored in two parts: a per-byte x/y coordinate (8
/// bytes per output byte, half the size of a Pin) and a tiny table of
/// page nodes covering byte ranges (there are only ever a handful of
/// pages). This also lets page formatters write coordinates directly
/// into the map without a separate coordinate-to-pin conversion pass.
pub struct PinMap<'a> {
    pub map: &'a mut PinMapMap,
}

/// The type of the page node referenced by pins (Zig: `PinMap.Node`).
pub type PinMapNode = NodePtr;

/// A page node covering output bytes starting at `offset`
/// (inclusive) until the next entry's offset (or the end of the
/// output). (Zig: `PinMap.NodeRun`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMapNodeRun {
    pub offset: usize,
    pub node: PinMapNode,
}

/// (Zig: `PinMap.Map`.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinMapMap {
    /// The x/y coordinate within its page for every output byte.
    pub points: Vec<Coordinate>,

    /// The page node for ranges of output bytes, ordered by offset.
    pub nodes: Vec<PinMapNodeRun>,
}

impl PinMapMap {
    pub const EMPTY: PinMapMap = PinMapMap {
        points: Vec::new(),
        nodes: Vec::new(),
    };

    pub fn deinit(&mut self) {
        self.points = Vec::new();
        self.nodes = Vec::new();
    }

    pub fn clear_retaining_capacity(&mut self) {
        self.points.clear();
        self.nodes.clear();
    }

    /// The total number of bytes mapped.
    pub fn count(&self) -> usize {
        self.points.len()
    }

    /// Set the page node for all bytes appended from here on,
    /// until the next call. No-op if the node is unchanged.
    pub fn set_node(&mut self, node: PinMapNode) {
        if let Some(last) = self.nodes.last() {
            if last.node == node {
                return;
            }
        }

        self.nodes.push(PinMapNodeRun {
            offset: self.points.len(),
            node,
        });
    }

    /// Append `n` bytes that map to `pin`.
    pub fn append(&mut self, pin: Pin, n: usize) {
        if n == 0 {
            return;
        }
        self.set_node(pin.node);
        let c = Coordinate {
            x: pin.x,
            y: pin.y as u32,
        };
        self.points.extend(std::iter::repeat_n(c, n));
    }

    /// Returns the pin that the byte at the given offset maps to,
    /// or null if the offset is out of range.
    pub fn get(&self, offset: usize) -> Option<Pin> {
        if offset >= self.points.len() {
            return None;
        }
        let coord = self.points[offset];
        let mut p = Pin::new(PinMap::find_node(&self.nodes, offset)?);
        p.x = coord.x;
        p.y = coord.y as CellCountInt;
        Some(p)
    }

    /// Returns the last pin in the map, if any.
    pub fn get_last_or_null(&self) -> Option<Pin> {
        let len = self.points.len();
        if len == 0 {
            return None;
        }
        self.get(len - 1)
    }
}

impl PinMap<'_> {
    /// Binary search for the node covering `offset` in a slice of node
    /// runs sorted by offset. Returns null only if the slice is empty
    /// or the offset precedes the first run.
    pub fn find_node(runs: &[PinMapNodeRun], offset: usize) -> Option<PinMapNode> {
        if runs.is_empty() || offset < runs[0].offset {
            return None;
        }
        let mut lo: usize = 0;
        let mut hi: usize = runs.len();
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if runs[mid].offset <= offset {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some(runs[lo].node)
    }
}

/// Terminal formatter formats the active terminal screen.
///
/// This will always only emit data related to the currently active screen.
/// If you want to emit data for a specific screen (e.g. primary vs alt), then
/// switch to that screen in the terminal prior to using this.
///
/// If you want to emit data for all screens (a less common operation), then
/// you must create a no-content TerminalFormatter followed by multiple
/// explicit ScreenFormatter calls. This isn't a common operation so this
/// little extra work should be acceptable.
///
/// For styled formatting, this will emit the palette colors at the
/// beginning so that the output can be rendered properly according to
/// the current terminal state.
pub struct TerminalFormatter<'a> {
    /// The terminal to format.
    pub terminal: &'a Terminal,

    /// The common options
    pub opts: Options<'a>,

    /// The content to include.
    pub content: ScreenFormatterContent,

    /// Extra stuff to emit, such as terminal modes, palette, cursor, etc.
    /// This information is ONLY emitted when the format is "vt".
    pub extra: TerminalFormatterExtra,

    /// If non-null, then `map` will contain the Pin of every byte
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// Note that some emitted bytes may not correspond to any Pin, such as
    /// the extra data around terminal state (palette, modes, etc.). For these,
    /// we'll map it to the most previous pin so there is some continuity but
    /// its an arbitrary choice.
    ///
    /// Warning: there is a significant performance hit to track this
    pub pin_map: Option<PinMap<'a>>,
}

/// (Zig: `TerminalFormatter.Extra`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalFormatterExtra {
    /// Emit the palette using OSC 4 sequences.
    pub palette: bool,

    /// Emit terminal modes that differ from their defaults using CSI h/l
    /// sequences. Defaults are according to the Ghostty defaults which
    /// are generally match most terminal defaults. This will include
    /// things like current screen, bracketed mode, mouse event reporting,
    /// etc.
    pub modes: bool,

    /// Emit scrolling region state using DECSTBM and DECSLRM sequences.
    pub scrolling_region: bool,

    /// Emit tabstop positions by clearing all tabs (CSI 3 g) and setting
    /// each configured tabstop with HTS.
    pub tabstops: bool,

    /// Emit the present working directory using OSC 7.
    pub pwd: bool,

    /// Emit keyboard modes such as ModifyOtherKeys using CSI > 4 m
    /// sequences.
    pub keyboard: bool,

    /// The screen extras to emit. TerminalFormatter always only
    /// emits data for the currently active screen. If you want to emit
    /// data for all screens, you should manually construct a no-content
    /// terminal formatter, followed by screen formatters.
    pub screen: ScreenFormatterExtra,
}

impl TerminalFormatterExtra {
    /// Emit nothing.
    pub const NONE: TerminalFormatterExtra = TerminalFormatterExtra {
        palette: false,
        modes: false,
        scrolling_region: false,
        tabstops: false,
        pwd: false,
        keyboard: false,
        screen: ScreenFormatterExtra::NONE,
    };

    /// Emit style-relevant information only such as palettes.
    pub const STYLES: TerminalFormatterExtra = TerminalFormatterExtra {
        palette: true,
        modes: false,
        scrolling_region: false,
        tabstops: false,
        pwd: false,
        keyboard: false,
        screen: ScreenFormatterExtra::STYLES,
    };

    /// Emit everything. This reconstructs the terminal state as closely
    /// as possible.
    pub const ALL: TerminalFormatterExtra = TerminalFormatterExtra {
        palette: true,
        modes: true,
        scrolling_region: true,
        tabstops: true,
        pwd: true,
        keyboard: true,
        screen: ScreenFormatterExtra::ALL,
    };
}

impl<'a> TerminalFormatter<'a> {
    pub fn init(terminal: &'a Terminal, opts: Options<'a>) -> TerminalFormatter<'a> {
        TerminalFormatter {
            terminal,
            opts,
            content: ScreenFormatterContent::Selection(None),
            extra: TerminalFormatterExtra::STYLES,
            pin_map: None,
        }
    }

    /// The active screen of the terminal.
    fn active_screen(&self) -> &'a Screen {
        let terminal: &'a Terminal = self.terminal;
        &terminal.screens.active
    }

    /// Count the bytes a formatter with no content, no pin map and only
    /// the given extras writes (Zig: the `extra_formatter` formatted into
    /// a `Discarding` writer).
    fn count_extra(&self, extra: TerminalFormatterExtra) -> io::Result<usize> {
        let mut discarding = Discarding::default();
        let mut extra_formatter = TerminalFormatter {
            terminal: self.terminal,
            opts: self.opts.clone(),
            content: ScreenFormatterContent::None,
            extra,
            pin_map: None,
        };
        extra_formatter.format(&mut discarding)?;
        usize::try_from(discarding.count).map_err(|_| write_failed())
    }

    /// Map `n` bytes of extra state to the top left pin of the screen.
    fn map_top_left(&mut self, n: usize) {
        let screen = self.active_screen();
        if let Some(m) = self.pin_map.as_mut() {
            m.map.append(screen.pages.get_top_left(Tag::Screen), n);
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        let terminal = self.terminal;
        let screen = self.active_screen();

        // Emit palette before screen content if using VT format. Technically
        // we could do this after but this way if replay is slow for whatever
        // reason the colors will be right right away.
        if self.extra.palette {
            'palette: {
                match self.opts.emit {
                    Format::Plain => break 'palette,

                    Format::Vt => {
                        for (i, rgb) in terminal.colors.palette.current.iter().enumerate() {
                            write!(writer, "\x1b]4;{};rgb:{:02x}/{:02x}/{:02x}\x1b\\", i, rgb.r, rgb.g, rgb.b)?;
                        }
                    }

                    // For HTML, we emit CSS to setup our palette variables.
                    Format::Html => {
                        writer.write_all(b"<style>:root{")?;
                        for (i, rgb) in terminal.colors.palette.current.iter().enumerate() {
                            write!(writer, "--vt-palette-{}: #{:02x}{:02x}{:02x};", i, rgb.r, rgb.g, rgb.b)?;
                        }
                        writer.write_all(b"}</style>")?;
                    }
                }

                // If we have a pin_map, add the bytes we wrote to map.
                if self.pin_map.is_some() {
                    let mut extra = TerminalFormatterExtra::NONE;
                    extra.palette = true;
                    let n = self.count_extra(extra)?;

                    // Map all those bytes to the same pin. Use the top left to ensure
                    // the node pointer is always properly initialized.
                    self.map_top_left(n);
                }
            }
        }

        // Emit terminal modes that differ from defaults. We probably have
        // some modes we want to emit before and some after, but for now for
        // simplicity we just emit them all before. If we make this more complex
        // later we should add test cases for it.
        if self.opts.emit == Format::Vt && self.extra.modes {
            for mode in modes::Mode::all() {
                let current = terminal.modes.get(mode);
                let default_val = terminal.modes.get_default(mode);

                if current != default_val {
                    let tag = mode.tag();
                    let prefix = if tag.ansi { "" } else { "?" };
                    let suffix = if current { "h" } else { "l" };
                    write!(writer, "\x1b[{}{}{}", prefix, tag.value, suffix)?;
                }
            }

            // If we have a pin_map, add the bytes we wrote to map.
            if self.pin_map.is_some() {
                let mut extra = TerminalFormatterExtra::NONE;
                extra.modes = true;
                let n = self.count_extra(extra)?;

                // Map all those bytes to the same pin. Use the top left to ensure
                // the node pointer is always properly initialized.
                self.map_top_left(n);
            }
        }

        // Emit tabstop positions before the screen contents because setting
        // them moves the cursor. Screen formatting will restore the requested
        // cursor position afterwards.
        if self.opts.emit == Format::Vt && self.extra.tabstops {
            // Clear all tabs (CSI 3 g)
            writer.write_all(b"\x1b[3g")?;

            // Set each configured tabstop by moving cursor and using HTS
            for col in 0..terminal.cols as usize {
                if terminal.tabstops.get(col) {
                    // Move cursor to the column (1-indexed)
                    write!(writer, "\x1b[{}G", col + 1)?;
                    // Set tab (HTS)
                    writer.write_all(b"\x1bH")?;
                }
            }

            // Screen contents are formatted relative to the top-left.
            writer.write_all(b"\x1b[H")?;

            // If we have a pin_map, add the bytes we wrote to map.
            if self.pin_map.is_some() {
                let mut extra = TerminalFormatterExtra::NONE;
                extra.tabstops = true;
                let n = self.count_extra(extra)?;

                // Map all those bytes to the same pin. Use the top left to ensure
                // the node pointer is always properly initialized.
                self.map_top_left(n);
            }
        }

        {
            let mut screen_formatter = ScreenFormatter::init(screen, self.opts.clone());
            screen_formatter.content = self.content;
            screen_formatter.pin_map = self.pin_map.as_mut().map(|m| PinMap { map: &mut *m.map });
            screen_formatter.format(writer)?;
        }

        // Extra terminal state to emit after the screen contents so that
        // it doesn't impact the emitted contents.
        if self.opts.emit == Format::Vt {
            // Emit scrolling region using DECSTBM and DECSLRM
            if self.extra.scrolling_region {
                let region = &terminal.scrolling_region;

                // DECSTBM: top and bottom margins (1-indexed)
                // Only emit if not the full screen
                if region.top != 0 || region.bottom != terminal.rows - 1 {
                    write!(writer, "\x1b[{};{}r", region.top as u32 + 1, region.bottom as u32 + 1)?;
                }

                // DECSLRM: left and right margins (1-indexed)
                // Only emit if not the full width
                if region.left != 0 || region.right != terminal.cols - 1 {
                    write!(writer, "\x1b[{};{}s", region.left as u32 + 1, region.right as u32 + 1)?;
                }
            }

            // Emit keyboard modes such as ModifyOtherKeys
            if self.extra.keyboard {
                // Only emit if modify_other_keys_2 is true
                if terminal.flags.modify_other_keys_2 {
                    writer.write_all(b"\x1b[>4;2m")?;
                }
            }

            // Emit present working directory using OSC 7
            if self.extra.pwd {
                let pwd = &terminal.pwd;
                if !pwd.is_empty() {
                    writer.write_all(b"\x1b]7;")?;
                    writer.write_all(pwd)?;
                    writer.write_all(b"\x1b\\")?;
                }
            }

            // If we have a pin_map, add the bytes we wrote to map.
            if self.pin_map.is_some() {
                let mut extra = TerminalFormatterExtra::NONE;
                extra.scrolling_region = self.extra.scrolling_region;
                extra.keyboard = self.extra.keyboard;
                extra.pwd = self.extra.pwd;
                let n = self.count_extra(extra)?;

                if let Some(m) = self.pin_map.as_mut() {
                    let pin = m
                        .map
                        .get_last_or_null()
                        .unwrap_or_else(|| screen.pages.get_top_left(Tag::Screen));
                    m.map.append(pin, n);
                }
            }
        }

        // Emit extra screen state last because terminal state such
        // as scrolling regions can move the cursor, so we have to set
        // cursor last.
        let mut screen_formatter = ScreenFormatter::init(screen, self.opts.clone());
        screen_formatter.pin_map = self.pin_map.as_mut().map(|m| PinMap { map: &mut *m.map });
        screen_formatter.content = ScreenFormatterContent::None;
        screen_formatter.extra = self.extra.screen;

        // In origin mode (DECOM), which the modes above restore, CUP counts
        // from the top-left of the scrolling region restored above, so the
        // cursor position is written relative to it.
        if self.opts.emit == Format::Vt
            && self.extra.modes
            && self.extra.scrolling_region
            && terminal.modes.get(modes::Mode::Origin)
        {
            screen_formatter.cursor_origin = CursorOrigin {
                x: terminal.scrolling_region.left,
                y: terminal.scrolling_region.top,
            };
        }

        screen_formatter.format(writer)
    }
}

/// A writer that only counts bytes (Zig: `std.Io.Writer.Discarding`).
#[derive(Debug, Default)]
struct Discarding {
    count: u64,
}

impl Write for Discarding {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_failed() -> io::Error {
    io::Error::other("WriteFailed")
}

/// Write a codepoint as UTF-8, replacing invalid codepoints with the
/// replacement character (Zig: `Writer.printUnicodeCodepoint`).
fn print_unicode_codepoint(writer: &mut dyn Write, cp: u32) -> io::Result<()> {
    let c = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
    let mut buf = [0u8; 4];
    writer.write_all(c.encode_utf8(&mut buf).as_bytes())
}

/// Print a decimal into buf, returning the number of bytes
/// (Zig: `fastprint.printDecimal`).
fn print_decimal(buf: &mut [u8], v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = v;
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let len = tmp.len() - i;
    buf[..len].copy_from_slice(&tmp[i..]);
    len
}

pub struct ScreenFormatter<'a> {
    /// The screen to format.
    pub screen: &'a Screen,

    /// The common options
    pub opts: Options<'a>,

    /// The content to include.
    pub content: ScreenFormatterContent,

    /// Extra stuff to emit, such as cursor, style, hyperlinks, etc.
    /// This information is ONLY emitted when the format is "vt".
    pub extra: ScreenFormatterExtra,

    /// If non-null, then `map` will contain the Pin of every byte
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// Note that some emitted bytes may not correspond to any Pin, such as
    /// the extra data around screen state. For these, we'll map it to the
    /// most previous pin so there is some continuity but its an arbitrary
    /// choice.
    ///
    /// Warning: there is a significant performance hit to track this
    pub pin_map: Option<PinMap<'a>>,

    /// The point the emitted cursor position (CUP) counts from. This is the
    /// top-left of the screen unless the output is replayed in origin mode
    /// (DECOM), where CUP counts from the top-left of the scrolling region.
    pub cursor_origin: CursorOrigin,
}

/// (Zig: `ScreenFormatter.Content`.)
#[derive(Debug, Clone, Copy)]
pub enum ScreenFormatterContent {
    /// Emit no content, only terminal state such as modes, palette, etc.
    /// via extra.
    None,

    /// Emit the content specified by the selection. Null for all.
    /// The selection is inclusive on both ends.
    Selection(Option<Selection>),
}

/// (Zig: `ScreenFormatter.CursorOrigin`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorOrigin {
    pub x: CellCountInt,
    pub y: CellCountInt,
}

/// (Zig: `ScreenFormatter.Extra`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScreenFormatterExtra {
    /// Emit cursor position using CUP (CSI H).
    pub cursor: bool,

    /// Emit current SGR style state based on the cursor's active style_id.
    /// This reconstructs the SGR attributes (bold, italic, colors, etc.) at
    /// the cursor position.
    pub style: bool,

    /// Emit current hyperlink state using OSC 8 sequences.
    /// This sets the active hyperlink based on cursor.hyperlink_id.
    pub hyperlink: bool,

    /// Emit character protection mode using DECSCA.
    pub protection: bool,

    /// Emit Kitty keyboard protocol state using CSI > u and CSI = sequences.
    pub kitty_keyboard: bool,

    /// Emit character set designations and invocations.
    /// This includes G0-G3 designations (ESC ( ) * +) and GL/GR invocations.
    pub charsets: bool,
}

impl ScreenFormatterExtra {
    /// Emit nothing.
    pub const NONE: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: false,
        style: false,
        hyperlink: false,
        protection: false,
        kitty_keyboard: false,
        charsets: false,
    };

    /// Emit style-relevant information only.
    pub const STYLES: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: false,
        style: true,
        hyperlink: true,
        protection: false,
        kitty_keyboard: false,
        charsets: false,
    };

    /// Emit everything. This reconstructs the screen state as closely
    /// as possible.
    pub const ALL: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: true,
        style: true,
        hyperlink: true,
        protection: true,
        kitty_keyboard: true,
        charsets: true,
    };

    fn is_set(&self) -> bool {
        self.cursor || self.style || self.hyperlink || self.protection || self.kitty_keyboard || self.charsets
    }
}

impl<'a> ScreenFormatter<'a> {
    pub fn init(screen: &'a Screen, opts: Options<'a>) -> ScreenFormatter<'a> {
        ScreenFormatter {
            screen,
            opts,
            content: ScreenFormatterContent::Selection(None),
            extra: ScreenFormatterExtra::NONE,
            pin_map: None,
            cursor_origin: CursorOrigin::default(),
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        match self.content {
            ScreenFormatterContent::None => {}

            ScreenFormatterContent::Selection(selection_) => {
                // Emit our pagelist contents according to our selection.
                let mut list_formatter = PageListFormatter::init(&self.screen.pages, self.opts.clone());
                list_formatter.pin_map = self.pin_map.as_mut().map(|m| PinMap { map: &mut *m.map });
                if let Some(sel) = selection_ {
                    list_formatter.top_left = Some(sel.top_left(self.screen));
                    list_formatter.bottom_right = Some(sel.bottom_right(self.screen));
                    list_formatter.rectangle = sel.rectangle;
                }
                list_formatter.format(writer)?;
            }
        }

        // Emit extra screen state after content if we care. The state has
        // to be emitted after since some state such as cursor position and
        // style are impacted by content rendering.
        match self.opts.emit {
            Format::Plain => return Ok(()),
            Format::Vt => {
                if !self.extra.is_set() {
                    return Ok(());
                }
            }

            // HTML doesn't preserve any screen state because it has
            // nothing to do with rendering.
            Format::Html => return Ok(()),
        }

        self.format_extra(writer)?;

        // If we have a pin_map, we need to count how many bytes the extras
        // will emit so we can map them all to the same pin. We do this by
        // formatting to a discarding writer with content=none.
        if let Some(m) = self.pin_map.as_mut() {
            let mut discarding = Discarding::default();
            let mut extra_formatter = ScreenFormatter {
                screen: self.screen,
                opts: self.opts.clone(),
                content: ScreenFormatterContent::None,
                extra: self.extra,
                pin_map: None,
                cursor_origin: self.cursor_origin,
            };
            extra_formatter.format(&mut discarding)?;

            // Map all those bytes to the same pin. Use the first page node
            // to ensure the node pointer is always properly initialized.
            let pin = m
                .map
                .get_last_or_null()
                .unwrap_or_else(|| self.screen.pages.get_top_left(Tag::Screen));
            let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
            m.map.append(pin, n);
        }

        Ok(())
    }

    fn format_extra(&self, writer: &mut dyn Write) -> io::Result<()> {
        let screen = self.screen;

        // Emit cursor position before the other extras because restoring a
        // pending wrap requires reprinting the cell at the right edge. That
        // print uses and changes active screen state, so the requested style,
        // hyperlink, protection, and charset must be restored afterwards.
        if self.extra.cursor {
            'cursor: {
                let cursor = &screen.cursor;
                let origin = self.cursor_origin;

                // If we don't have pending wrap, then we can just use CUP.
                if !cursor.pending_wrap || cursor.x != screen.pages.cols - 1 {
                    write!(
                        writer,
                        "\x1b[{};{}H",
                        cursor.y.saturating_sub(origin.y) as u32 + 1,
                        cursor.x.saturating_sub(origin.x) as u32 + 1,
                    )?;
                    break 'cursor;
                }

                // SAFETY: the cursor cell pointer is live.
                let page_cell = unsafe { *cursor.page_cell };

                // Pending wrap, we can't use CUP because it resets pending wrap.
                let start_x = match page_cell.wide() {
                    Wide::SpacerTail => cursor.x - 1,
                    Wide::Narrow | Wide::Wide | Wide::SpacerHead => cursor.x,
                };

                // Move cursor to the edge.
                write!(
                    writer,
                    "\x1b[{};{}H",
                    cursor.y.saturating_sub(origin.y) as u32 + 1,
                    start_x.saturating_sub(origin.x) as u32 + 1,
                )?;

                // A blank cell formats to nothing, which would leave the wrap
                // unset. Print a space to set it, save the cursor with the wrap
                // (DECSC), erase the space (ECH, which clears the wrap) and
                // restore the cursor (DECRC), which sets the wrap again over a
                // blank cell. This replaces the saved cursor, which the
                // formatter does not emit.
                if page_cell.wide() != Wide::SpacerTail && !page_cell.has_text() {
                    writer.write_all(b" \x1b7\x1b[X\x1b8")?;
                    break 'cursor;
                }

                // Reformat the cell which sets the proper pending wrap state.
                // SAFETY: the cursor pin is a live tracked pin.
                let pin = unsafe { *cursor.page_pin.as_ptr() };
                // SAFETY: valid pin.
                let page = unsafe { pin.node_mut().page() };
                let mut cell_formatter = PageFormatter::init(page, self.opts.clone());
                cell_formatter.start_x = cursor.x;
                cell_formatter.end_x = Some(cursor.x);
                cell_formatter.start_y = pin.y;
                cell_formatter.end_y = Some(pin.y);
                cell_formatter.format(writer)?;
            }
        }

        // Emit current SGR style state
        if self.extra.style {
            let cursor = &screen.cursor;
            write!(writer, "{}", cursor.style.formatter_vt())?;
        }

        // Emit current hyperlink state using OSC 8
        if self.extra.hyperlink {
            let cursor = &screen.cursor;
            if let Some(link) = &cursor.hyperlink {
                // Start hyperlink with uri (and explicit id if present)
                match &link.id {
                    hyperlink::HyperlinkId::Explicit(id) => {
                        writer.write_all(b"\x1b]8;id=")?;
                        writer.write_all(id)?;
                        writer.write_all(b";")?;
                        writer.write_all(&link.uri)?;
                        writer.write_all(b"\x1b\\")?;
                    }
                    hyperlink::HyperlinkId::Implicit(_) => {
                        writer.write_all(b"\x1b]8;;")?;
                        writer.write_all(&link.uri)?;
                        writer.write_all(b"\x1b\\")?;
                    }
                }
            }
        }

        // Emit character protection mode using DECSCA
        if self.extra.protection {
            let cursor = &screen.cursor;
            if cursor.protected {
                // DEC protected mode
                writer.write_all(b"\x1b[1\"q")?;
            }
        }

        // Emit Kitty keyboard protocol state using CSI = u
        if self.extra.kitty_keyboard {
            let current_flags = screen.kitty_keyboard.current();
            if current_flags.int() != kitty::KeyFlags::DISABLED.int() {
                let flags = current_flags.int();
                write!(writer, "\x1b[={flags};1u")?;
            }
        }

        // Emit character set designations and invocations
        if self.extra.charsets {
            let charset = &screen.charset;

            // Emit G0-G3 designations
            for slot in charsets::Slots::ALL {
                let cs = charset.charsets.get(slot);
                if cs != charsets::Charset::Utf8 {
                    // Only emit non-default charsets
                    let intermediate: u8 = match slot {
                        charsets::Slots::G0 => b'(',
                        charsets::Slots::G1 => b')',
                        charsets::Slots::G2 => b'*',
                        charsets::Slots::G3 => b'+',
                    };
                    let final_: u8 = match cs {
                        charsets::Charset::Ascii => b'B',
                        charsets::Charset::British => b'A',
                        charsets::Charset::DecSpecial => b'0',
                        _ => continue,
                    };
                    writer.write_all(&[0x1b, intermediate, final_])?;
                }
            }

            // Emit GL invocation if not G0
            if charset.gl != charsets::Slots::G0 {
                let seq: &[u8] = match charset.gl {
                    charsets::Slots::G0 => unreachable!(),
                    charsets::Slots::G1 => b"\x0e", // SO - Shift Out
                    charsets::Slots::G2 => b"\x1bn", // LS2
                    charsets::Slots::G3 => b"\x1bo", // LS3
                };
                writer.write_all(seq)?;
            }

            // Emit GR invocation if not G2
            if charset.gr != charsets::Slots::G2 {
                let seq: &[u8] = match charset.gr {
                    charsets::Slots::G0 => unreachable!(), // GR can't be G0
                    charsets::Slots::G1 => b"\x1b~",      // LS1R
                    charsets::Slots::G2 => unreachable!(),
                    charsets::Slots::G3 => b"\x1b|", // LS3R
                };
                writer.write_all(seq)?;
            }
        }

        Ok(())
    }
}

/// PageList formatter formats multiple pages as represented by a PageList.
pub struct PageListFormatter<'a> {
    /// The pagelist to format.
    pub list: &'a PageList,

    /// The common options
    pub opts: Options<'a>,

    /// The bounds of the PageList to format. The top left and bottom right
    /// MUST be ordered properly.
    pub top_left: Option<Pin>,
    pub bottom_right: Option<Pin>,

    /// If true, the boundaries define a rectangle selection where start_x
    /// and end_x apply to every row, not just the first and last.
    pub rectangle: bool,

    /// If non-null, then `map` will contain the Pin of every byte
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// Warning: there is a significant performance hit to track this
    pub pin_map: Option<PinMap<'a>>,
}

impl<'a> PageListFormatter<'a> {
    pub fn init(list: &'a PageList, opts: Options<'a>) -> PageListFormatter<'a> {
        PageListFormatter {
            list,
            opts,
            top_left: None,
            bottom_right: None,
            rectangle: false,
            pin_map: None,
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        let tl: Pin = self.top_left.unwrap_or_else(|| self.list.get_top_left(Tag::Screen));
        let mut br: Pin = self
            .bottom_right
            .unwrap_or_else(|| self.list.get_bottom_right(Tag::Screen).unwrap());

        // If the end is a spacer head, move it onto the wrapped wide char,
        // which may be on another page.
        // SAFETY: valid pin.
        if self.opts.unwrap && !self.rectangle && unsafe { (*br.row_and_cell().cell).wide() } == Wide::SpacerHead {
            br = br.right_wrap(1).unwrap_or(br);
        }

        let mut page_state: Option<TrailingState> = None;
        let iter = tl.page_iterator(Direction::RightDown, Some(br));
        for chunk in iter {
            debug_assert!(chunk.start < chunk.end);
            debug_assert!(chunk.end > 0);

            // SAFETY: chunk nodes are live nodes of the list.
            let page = unsafe { (*chunk.node.as_ptr()).page() };
            let mut formatter = PageFormatter::init(page, self.opts.clone());
            formatter.start_y = chunk.start;
            formatter.end_y = Some(chunk.end - 1);
            formatter.trailing_state = page_state;
            formatter.rectangle = self.rectangle;

            // For rectangle selection, apply start_x and end_x to all chunks
            if self.rectangle {
                formatter.start_x = tl.x;
                formatter.end_x = Some(br.x);
            } else {
                // Otherwise only on the first/last, respectively.
                if chunk.node == tl.node {
                    formatter.start_x = tl.x;
                }
                if chunk.node == br.node {
                    formatter.end_x = Some(br.x);
                }
            }

            // If we're tracking pins, the page formatter writes its
            // per-byte coordinates directly into our map's point list
            // and we record which page node covers those bytes.
            if let Some(m) = self.pin_map.as_mut() {
                m.map.set_node(chunk.node);
                let base = m.map.points.len();
                formatter.point_map = Some(PointMap {
                    map: &mut m.map.points,
                    base,
                });
            }

            page_state = Some(formatter.format_with_state(writer)?);
        }

        // VT output without trimming keeps the blank rows at the end:
        // they are part of the layout, and a replay without them scrolls
        // less, so the rows above them end up too low on the screen.
        if self.opts.emit == Format::Vt && !self.opts.trim {
            let Some(state) = page_state else { return Ok(()) };
            if state.rows <= 1 {
                return Ok(());
            }

            // The last row needs no newline after it.
            let count = state.rows - 1;
            for _ in 0..count {
                writer.write_all(b"\r\n")?;
            }

            // Each newline maps to the row it ends, as between pages:
            // the first one to the end of the last row with text.
            if let Some(m) = self.pin_map.as_mut() {
                for i in 0..count {
                    let row = br.up(count - i).unwrap_or(tl);
                    let pin = if i == 0 {
                        m.map.get(m.map.count().saturating_sub(1)).unwrap_or(row)
                    } else {
                        row
                    };
                    let mut p = Pin::new(pin.node);
                    p.x = if i == 0 { pin.x } else { 0 };
                    p.y = pin.y;
                    m.map.append(p, 2);
                }
            }
        }

        Ok(())
    }
}

/// (Zig: `PageFormatter.PointMap`.)
pub struct PointMap<'a> {
    pub map: &'a mut Vec<Coordinate>,

    /// The index in `map` at which this formatter's output begins.
    /// Entries before this index belong to a caller (e.g. previous
    /// pages of a PageListFormatter) and aren't inspected. This
    /// exists so that callers can share one list across pages.
    pub base: usize,
}

impl PointMap<'_> {
    fn append_n(&mut self, c: Coordinate, n: usize) {
        self.map.extend(std::iter::repeat_n(c, n));
    }
}

/// Trailing state. This is used to ensure that rows wrapped across
/// multiple pages are unwrapped properly, as well as other accounting
/// we may do in the future. (Zig: `PageFormatter.TrailingState`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrailingState {
    pub rows: usize,
    pub cells: usize,
}

impl TrailingState {
    pub const EMPTY: TrailingState = TrailingState { rows: 0, cells: 0 };
}

/// Page formatter.
///
/// For styled formatting such as VT, this will emit references for palette
/// colors. If you want to capture the palette as-is at the type of formatting,
/// you'll have to emit the sequences for setting up the palette prior to
/// this formatting. (TODO: A function to do this)
pub struct PageFormatter<'a> {
    /// The page to format.
    pub page: &'a Page,

    /// The common options
    pub opts: Options<'a>,

    /// Start and end points within the page to format. If end x is not given
    /// then it will be the full width. If end y is not given then it will be
    /// the full height.
    ///
    /// The start and end are both inclusive, so equal values will still
    /// return a non-empty result (i.e. a single cell or row).
    ///
    /// The start x is considered the X in the first row and end X is
    /// X in the final row. This isn't a rectangle selection by default.
    ///
    /// If start X falls on the second column of a wide character, then
    /// the entire character will be included (as if you specified the
    /// previous column).
    pub start_x: CellCountInt,
    pub start_y: CellCountInt,
    pub end_x: Option<CellCountInt>,
    pub end_y: Option<CellCountInt>,

    /// If true, the start x/y and end x/y define a rectangle selection.
    /// In this case, the boundaries will apply to every row, not just
    /// the first and last.
    pub rectangle: bool,

    /// If non-null, then `map` will contain the x/y coordinate of every
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// The x/y coordinate will be the coordinates within the page.
    ///
    /// Warning: there is a significant performance hit to track this
    pub point_map: Option<PointMap<'a>>,

    /// The previous trailing state from the prior page. If you're iterating
    /// over multiple pages this helps ensure that unwrapping and other
    /// accounting works properly.
    pub trailing_state: Option<TrailingState>,
}

/// The largest single-cell encoding must fit after a flush: the
/// HTML entity for the maximum codepoint ("&#2097151;") is 10
/// bytes, escapes are up to 6.
const MAX_ENCODING_LEN: usize = 16;

/// Style id marking a style that didn't come from an interned id.
const INVALID_STYLE_ID: u32 = u32::MAX;

impl<'a> PageFormatter<'a> {
    /// Initializes a page formatter. Other options can be set directly on the
    /// struct after initialization and before calling `format()`.
    pub fn init(page: &'a Page, opts: Options<'a>) -> PageFormatter<'a> {
        PageFormatter {
            page,
            opts,
            start_x: 0,
            start_y: 0,
            end_x: None,
            end_y: None,
            rectangle: false,
            point_map: None,
            trailing_state: None,
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        let _ = self.format_with_state(writer)?;
        Ok(())
    }

    pub fn format_with_state(&mut self, writer: &mut dyn Write) -> io::Result<TrailingState> {
        // Specialize the hot path on the emitted format so that the
        // per-cell loop contains no per-cell format dispatch.
        match self.opts.emit {
            Format::Plain => self.format_with_state_emit::<{ Format::Plain as u8 }>(writer),
            Format::Vt => self.format_with_state_emit::<{ Format::Vt as u8 }>(writer),
            Format::Html => self.format_with_state_emit::<{ Format::Html as u8 }>(writer),
        }
    }

    fn cells_of(&self, row: &Row) -> &'a [Cell] {
        // SAFETY: the row belongs to this page.
        unsafe { &*self.page.get_cells(row) }
    }

    fn format_with_state_emit<const EMIT: u8>(&mut self, writer: &mut dyn Write) -> io::Result<TrailingState> {
        let emit = format_from_const(EMIT);
        let mut blank_rows: usize = 0;
        let mut blank_cells: usize = 0;

        // Continue our prior trailing state if we have it, but only if we're
        // starting from the beginning (start_y and start_x are both 0).
        // If a non-zero start position is specified, ignore trailing state.
        if let Some(state) = self.trailing_state {
            if self.start_y == 0 && self.start_x == 0 {
                blank_rows = state.rows;
                blank_cells = state.cells;
            }
        }

        let early = |blank_rows, blank_cells| TrailingState {
            rows: blank_rows,
            cells: blank_cells,
        };

        // Setup our starting column and perform some validation for overflows.
        // Note: start_x only applies to the first row, end_x only applies to the last row.
        let start_x: CellCountInt = self.start_x;
        if start_x >= self.page.size.cols {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_x_unclamped: CellCountInt = self.end_x.unwrap_or(self.page.size.cols - 1);
        let end_x = end_x_unclamped.min(self.page.size.cols - 1);

        // Setup our starting row and perform some validation for overflows.
        let start_y: CellCountInt = self.start_y;
        if start_y >= self.page.size.rows {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_y_unclamped: CellCountInt = self.end_y.unwrap_or(self.page.size.rows - 1);
        if start_y > end_y_unclamped {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_y = end_y_unclamped.min(self.page.size.rows - 1);

        // If we only have a single row, validate that start_x <= end_x
        if start_y == end_y && start_x > end_x {
            return Ok(early(blank_rows, blank_cells));
        }

        // Wrap HTML output in monospace font styling
        match emit {
            Format::Plain => {}

            Format::Html => {
                // Setup our div. We use a buffer here that should always
                // fit the stuff we need, in order to make counting bytes easier.
                let mut buf: Vec<u8> = Vec::with_capacity(1024);

                // Monospace and whitespace preserving
                buf.extend_from_slice(b"<div style=\"font-family: monospace; white-space: pre;");

                // Background/foreground colors
                if let Some(bg) = self.opts.background {
                    write!(buf, "background-color: #{:02x}{:02x}{:02x};", bg.r, bg.g, bg.b)?;
                }
                if let Some(fg) = self.opts.foreground {
                    write!(buf, "color: #{:02x}{:02x}{:02x};", fg.r, fg.g, fg.b)?;
                }

                buf.extend_from_slice(b"\">");

                writer.write_all(&buf)?;
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x: 0, y: 0 }, buf.len());
                }
            }

            Format::Vt => {
                // OSC 10 sets foreground color, OSC 11 sets background color
                let mut buf: Vec<u8> = Vec::with_capacity(512);
                if let Some(fg) = self.opts.foreground {
                    write!(buf, "\x1b]10;rgb:{:02x}/{:02x}/{:02x}\x1b\\", fg.r, fg.g, fg.b)?;
                }
                if let Some(bg) = self.opts.background {
                    write!(buf, "\x1b]11;rgb:{:02x}/{:02x}/{:02x}\x1b\\", bg.r, bg.g, bg.b)?;
                }

                writer.write_all(&buf)?;
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x: 0, y: 0 }, buf.len());
                }
            }
        }

        // Our style for non-plain formats. Alongside the style itself we
        // track the page-local interned style id it corresponds to (styles
        // are interned per-page so id equality implies style equality).
        // The id is only a fast-path hint: it is set to `invalid_style_id`
        // whenever the current style didn't come from an interned id
        // (e.g. bg-color-only cells which synthesize styles).
        let mut style: Style = Style::default();
        let mut style_id: u32 = 0;

        // Whether the codepoint map has any entries. Hoisted out of the
        // per-codepoint path so that the common no-map case can use the
        // fast cell run path below.
        let cp_map_empty: bool = match &self.opts.codepoint_map {
            Some(m) => m.is_empty(),
            None => true,
        };

        // Track hyperlink state for HTML and VT output. We need to close
        // </a> tags or OSC 8 when the hyperlink changes or ends.
        let mut current_hyperlink_id: Option<hyperlink::Id> = None;

        // Most pages hold no hyperlinks, so check once instead of per cell.
        let page_hyperlinks = self.page.hyperlink_count() > 0;

        for y_usize in start_y as usize..end_y as usize + 1 {
            let y: CellCountInt = y_usize as CellCountInt;
            // SAFETY: y is within the page rows.
            let row: &Row = unsafe { &*self.page.get_row(y_usize) };
            let cells: &[Cell] = self.cells_of(row);

            // Determine the x range for this row
            // - First row: start_x to end of row (or end_x if single row)
            // - Last row: start of row to end_x
            // - Middle rows: full width
            // The end is always straightforward
            let row_end_x: CellCountInt = if self.rectangle || y == end_y {
                end_x + 1
            } else {
                self.page.size.cols
            };

            // The first we have to check if our start X falls on the
            // tail of a wide character.
            let row_start_x: CellCountInt = if start_x > 0 && (self.rectangle || y == start_y) {
                match cells[start_x as usize].wide() {
                    // Include the prior cell to get the full wide char
                    Wide::SpacerTail => start_x - 1,

                    // If we're a spacer head on our first row then we
                    // skip this whole row.
                    Wide::SpacerHead => continue,

                    Wide::Narrow | Wide::Wide => start_x,
                }
            } else {
                0
            };

            let cells_subset = &cells[row_start_x as usize..row_end_x as usize];

            // If this row is blank, accumulate to avoid a bunch of extra
            // work later. If it isn't blank, make sure we dump all our
            // blanks. In VT a row of cells that only carry a background
            // (erased while a background color was set) isn't blank,
            // since dropping it would lose the color.
            if !Cell::has_text_any(cells_subset) && !(emit == Format::Vt && has_background_any(cells_subset)) {
                blank_rows += 1;
                continue;
            }

            if blank_rows > 0 {
                // Reset style before emitting newlines to prevent background
                // colors from bleeding into the next line's leading cells.
                if !style.is_default() {
                    self.format_style_close(emit, writer)?;
                    style = Style::default();
                    style_id = 0;
                }

                let sequence: &[u8] = match emit {
                    // Plaintext just uses standard newlines because newlines
                    // on their own usually move the cursor back in anywhere
                    // you type plaintext.
                    Format::Plain => b"\n",

                    // VT uses \r\n because in a raw pty, \n alone doesn't
                    // guarantee moving the cursor back to column 0. \r
                    // makes it work for sure.
                    Format::Vt => b"\r\n",

                    // HTML uses just \n because HTML rendering will move
                    // the cursor back.
                    Format::Html => b"\n",
                };

                for _ in 0..blank_rows {
                    writer.write_all(sequence)?;
                }

                // \r and \n map to the row that ends with this newline.
                // If we're continuing (trailing state) then this will be
                // in a prior page, so we just map to the first row of this
                // page.
                if let Some(map) = self.point_map.as_mut() {
                    let start: Coordinate = if map.map.len() > map.base {
                        map.map[map.map.len() - 1]
                    } else {
                        Coordinate { x: 0, y: 0 }
                    };

                    // The first one inherits the x value.
                    map.append_n(
                        Coordinate {
                            x: start.x,
                            y: start.y,
                        },
                        sequence.len(),
                    );

                    // All others have x = 0 since they reference their prior
                    // blank line.
                    for y_offset in 1..blank_rows {
                        map.append_n(
                            Coordinate {
                                x: 0,
                                y: start.y + y_offset as u32,
                            },
                            sequence.len(),
                        );
                    }
                }

                blank_rows = 0;
            }

            // If we're not wrapped, we always add a newline so after
            // the row is printed we can add a newline.
            if !row.wrap() || !self.opts.unwrap {
                blank_rows += 1;
            }

            // If the row doesn't continue a wrap then we need to reset
            // our blank cell count.
            if !row.wrap_continuation() || !self.opts.unwrap {
                blank_cells = 0;
            }

            // Go through each cell and print it
            let mut cell_i: usize = 0;
            'cells: while cell_i < cells_subset.len() {
                let cell: &Cell = &cells_subset[cell_i];
                let x: CellCountInt = (row_start_x as usize + cell_i) as CellCountInt;

                // Fast path: runs of simple cells (single codepoint, no
                // style/hyperlink transitions) are encoded in batches,
                // avoiding all of the per-cell bookkeeping below. This is
                // only valid when we have no codepoint map and when our
                // current style/hyperlink state is known-stable.
                'fast: {
                    if !cp_map_empty {
                        break 'fast;
                    }
                    if format_styled(emit) && style_id == INVALID_STYLE_ID {
                        break 'fast;
                    }

                    // VT blanks carried over from a wrapped row (when
                    // unwrapping) can't be stepped over, so the run only
                    // starts with blanks on this row.
                    if emit == Format::Vt && blank_cells > cell_i {
                        break 'fast;
                    }

                    // Pages without hyperlinks take a copy of the run
                    // loop that never checks for them.
                    let hyperlinks = format_styled(emit) && page_hyperlinks;
                    let track_points = self.point_map.is_some();
                    let run = if hyperlinks {
                        Self::write_cell_run::<EMIT, true>
                    } else {
                        Self::write_cell_run::<EMIT, false>
                    };
                    let consumed = run(
                        self,
                        track_points,
                        writer,
                        &cells_subset[cell_i..],
                        x,
                        y,
                        style_id,
                        current_hyperlink_id,
                        &mut blank_cells,
                    )?;

                    // Zero cells consumed means the first cell isn't
                    // eligible for the fast path; handle it below.
                    if consumed == 0 {
                        break 'fast;
                    }

                    cell_i += consumed;
                    continue 'cells;
                }

                // Skip spacers. These happen naturally when wide characters
                // are printed again on the screen (for well-behaved terminals!)
                match cell.wide() {
                    Wide::Narrow | Wide::Wide => {}
                    Wide::SpacerHead | Wide::SpacerTail => {
                        cell_i += 1;
                        continue 'cells;
                    }
                }

                // If we have a zero value, then we accumulate a counter. We
                // only want to turn zero values into spaces if we have a non-zero
                // char sometime later.
                'blank: {
                    // If we're emitting styled output (not plaintext) and
                    // the cell has some kind of styling or is not empty
                    // then this isn't blank.
                    if format_styled(emit) && (!cell.is_empty() || cell.has_styling()) {
                        break 'blank;
                    }

                    // Cells with no text are blank
                    if !cell.has_text() {
                        blank_cells += 1;
                        cell_i += 1;
                        continue 'cells;
                    }

                    // Trailing spaces are blank. We know it is trailing
                    // because if we get a non-empty cell later we'll
                    // fill the blanks.
                    if cell.codepoint() == ' ' as u32 && self.opts.trim {
                        blank_cells += 1;
                        cell_i += 1;
                        continue 'cells;
                    }
                }

                // This cell is not blank. If we have accumulated blank cells
                // then we want to emit them now.
                if emit == Format::Vt && blank_cells > 0 {
                    if blank_cells <= cell_i {
                        // Blanks on this row are stepped over (CUF).
                        // Spaces would turn them into text, in the pen
                        // of the text before them.
                        self.write_blank_step(writer, blank_cells, x, y)?;
                        blank_cells = 0;
                    } else if !style.is_default() {
                        // Blanks carried over from a wrapped row can't
                        // be: CUF stops at the end of the row. They are
                        // written as spaces, without the pen of the text
                        // before them.
                        self.format_style_close(emit, writer)?;
                        style = Style::default();
                        style_id = 0;
                    }
                }

                if blank_cells > 0 {
                    // Blank cells have no hyperlink. In VT output an open
                    // hyperlink would apply to the spaces written for them,
                    // so close it first.
                    if emit == Format::Vt && current_hyperlink_id.is_some() {
                        self.format_hyperlink_close(emit, writer)?;
                        current_hyperlink_id = None;
                    }

                    for _ in 0..blank_cells {
                        writer.write_all(b" ")?;
                    }

                    if self.point_map.is_some() {
                        self.append_blank_points(blank_cells, x, y);
                    }

                    blank_cells = 0;
                }

                'style: {
                    // If we aren't emitting styled output then we don't
                    // have to worry about styles.
                    if !format_styled(emit) {
                        break 'style;
                    }

                    // Fast path: styles are interned per-page, so if this
                    // cell's style id matches the id of our current style
                    // then the style is unchanged.
                    let cell_style_id: u32 = match cell.content_tag() {
                        ContentTag::Codepoint | ContentTag::CodepointGrapheme => cell.style_id() as u32,
                        ContentTag::BgColorPalette | ContentTag::BgColorRgb => INVALID_STYLE_ID,
                    };
                    if cell_style_id == style_id && cell_style_id != INVALID_STYLE_ID {
                        break 'style;
                    }

                    // Get our cell style.
                    let cell_style = self.cell_style(cell);

                    // If the style hasn't changed, don't bloat output.
                    // When both ids are interned (and thus different, since
                    // equal ids broke out above), interning guarantees the
                    // styles differ so we can skip the comparison entirely.
                    if (cell_style_id == INVALID_STYLE_ID || style_id == INVALID_STYLE_ID) && cell_style.eql(&style) {
                        style_id = cell_style_id;
                        break 'style;
                    }

                    // If we had a previous style, we need to close it,
                    // because we've confirmed we have some new style
                    // (which is maybe default).
                    if !style.is_default() {
                        match emit {
                            Format::Html => self.format_style_close(emit, writer)?,

                            // For VT, we only close if we're switching to a default
                            // style because any non-default style will emit
                            // a \x1b[0m as the start of a VT coloring sequence.
                            Format::Vt => {
                                if cell_style.is_default() {
                                    self.format_style_close(emit, writer)?;
                                }
                            }

                            // Unreachable because of the styled() check at the
                            // top of this block.
                            Format::Plain => unreachable!(),
                        }
                    }

                    // At this point, we can copy our style over
                    style = cell_style;
                    style_id = cell_style_id;

                    // If we're just the default style now, we're done.
                    if cell_style.is_default() {
                        break 'style;
                    }

                    // New style, emit it.
                    self.format_style_open(emit, writer, &style)?;

                    // If we have a point map, we map the style to
                    // this cell.
                    if self.point_map.is_some() {
                        let mut discarding = Discarding::default();
                        self.format_style_open(emit, &mut discarding, &style)?;
                        let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.append_n(Coordinate { x, y: y as u32 }, n);
                        }
                    }
                }

                // Hyperlink state
                'hyperlink: {
                    // HTML emits hyperlinks as anchors and VT as OSC 8.
                    if !format_styled(emit) {
                        break 'hyperlink;
                    }
                    if !page_hyperlinks {
                        break 'hyperlink;
                    }

                    // Get the hyperlink ID. This ID is our internal ID,
                    // not necessarily the OSC8 ID.
                    let link_id_: Option<hyperlink::Id> = if cell.hyperlink() {
                        self.page.lookup_hyperlink(cell)
                    } else {
                        None
                    };

                    // If our hyperlink IDs match (even null) then we have
                    // identical hyperlink state and we do nothing.
                    if current_hyperlink_id == link_id_ {
                        break 'hyperlink;
                    }

                    // If our prior hyperlink ID was non-null, we need to
                    // close it because the ID has changed. In VT, opening
                    // a hyperlink replaces the open one, so it is only
                    // closed when no hyperlink follows.
                    if current_hyperlink_id.is_some() && (emit == Format::Html || link_id_.is_none()) {
                        self.format_hyperlink_close(emit, writer)?;
                    }
                    current_hyperlink_id = None;

                    // Set our current hyperlink ID
                    let Some(link_id) = link_id_ else { break 'hyperlink };
                    current_hyperlink_id = Some(link_id);

                    // Emit the opening hyperlink tag
                    // SAFETY: page memory is the base of its hyperlink set.
                    let link = unsafe { *self.page.hyperlink_set.get(self.page.memory, link_id) };
                    self.format_hyperlink_open(emit, writer, &link)?;

                    // If we have a point map, we map the hyperlink to
                    // this cell.
                    if self.point_map.is_some() {
                        let n = self.hyperlink_open_len(emit, &link)?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.append_n(Coordinate { x, y: y as u32 }, n);
                        }
                    }
                }

                match cell.content_tag() {
                    // We combine codepoint and graphemes because both have
                    // shared style handling. We use comptime to dup it.
                    tag @ (ContentTag::Codepoint | ContentTag::CodepointGrapheme) => {
                        self.write_cell(tag, emit, writer, cell)?;

                        // If we have a point map, all codepoints map to this
                        // cell.
                        if self.point_map.is_some() {
                            let mut discarding = Discarding::default();
                            self.write_cell(tag, emit, &mut discarding, cell)?;
                            let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
                            if let Some(map) = self.point_map.as_mut() {
                                map.append_n(Coordinate { x, y: y as u32 }, n);
                            }
                        }
                    }

                    // Cells with only background color (no text).
                    ContentTag::BgColorPalette | ContentTag::BgColorRgb => {
                        // VT restores them as they are made: erasing with
                        // the background set (ECH) leaves empty cells
                        // carrying it, where a space would be text. The
                        // pen is already the background, from the style
                        // block above. A run of the same background is
                        // erased at once, then stepped over (CUF).
                        if emit == Format::Vt {
                            let mut run: usize = 1;
                            while cell_i + run < cells_subset.len() && same_background(cell, &cells_subset[cell_i + run])
                            {
                                run += 1;
                            }

                            let seq = format!("\x1b[{run}X\x1b[{run}C");
                            writer.write_all(seq.as_bytes())?;
                            if let Some(map) = self.point_map.as_mut() {
                                map.append_n(Coordinate { x, y: y as u32 }, seq.len());
                            }

                            cell_i += run;
                            continue 'cells;
                        }

                        // HTML emits a space, in the background's style.
                        writer.write_all(b" ")?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.map.push(Coordinate { x, y: y as u32 });
                        }
                    }
                }

                cell_i += 1;
            }
        }

        // If the style is non-default, we need to close our style tag.
        if !style.is_default() {
            self.format_style_close(emit, writer)?;
        }

        // Close any open hyperlink
        if current_hyperlink_id.is_some() {
            self.format_hyperlink_close(emit, writer)?;
        }

        // Close the monospace wrapper for HTML output
        if emit == Format::Html {
            let closing = b"</div>";
            writer.write_all(closing)?;
            if let Some(map) = self.point_map.as_mut() {
                let last = map.map[map.map.len() - 1];
                map.append_n(last, closing.len());
            }
            // Closing the div creates a newline in the output
            // so make sure to create one less newline
            if blank_rows >= 1 {
                blank_rows -= 1;
            }
        }

        Ok(TrailingState {
            rows: blank_rows,
            cells: blank_cells,
        })
    }

    /// Fast path for writing runs of simple cells: single-codepoint cells
    /// that require no style or hyperlink handling. Output bytes are
    /// batched into a stack buffer to avoid per-cell writer dispatch.
    /// Returns the number of cells consumed, which may be zero if the
    /// first cell isn't eligible for the fast path (in which case the
    /// caller must handle it via the slow path).
    ///
    /// Requirements (asserted by the caller, not here):
    ///
    ///   - The codepoint map is empty.
    ///   - For styled formats, `run_style_id` is the valid interned
    ///     page-local id of the currently active style.
    ///   - `run_hyperlink_id` is the open hyperlink (or none).
    ///
    /// `hyperlinks` is whether the page holds any hyperlinks. Without
    /// them the run never checks a cell's hyperlink state, which keeps
    /// the common case free of the per-cell check.
    ///
    /// `run_x`/`run_y` are the page coordinates of `cells[0]`, used for
    /// point map tracking.
    ///
    /// Blank cell accounting matches the slow path: accumulated blanks
    /// are only materialized once a non-blank cell is found, and any
    /// remainder is written back to `blank_cells`.
    // Deliberately not inlined: this is instantiated per emit format and
    // hyperlink mode (Zig: `comptime emit`, `comptime hyperlinks`), and
    // inlining every copy into the row loop bloats the binary.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn write_cell_run<const EMIT: u8, const HYPERLINKS: bool>(
        &mut self,
        track_points: bool,
        writer: &mut dyn Write,
        cells: &[Cell],
        run_x: CellCountInt,
        run_y: CellCountInt,
        run_style_id: u32,
        run_hyperlink_id: Option<hyperlink::Id>,
        blank_cells: &mut usize,
    ) -> io::Result<usize> {
        debug_assert!(track_points == self.point_map.is_some());
        let emit = format_from_const(EMIT);
        let hyperlinks = HYPERLINKS;

        let mut buf = [0u8; 512];
        let mut len: usize = 0;
        let mut pending: usize = *blank_cells;

        let mut i: usize = 0;
        while i < cells.len() {
            let cell = &cells[i];

            // Spacers produce no output, matching the slow path which
            // skips them before any blank/style handling.
            match cell.wide() {
                Wide::Narrow | Wide::Wide => {}
                Wide::SpacerHead | Wide::SpacerTail => {
                    i += 1;
                    continue;
                }
            }

            // Only text cells: bg-color cells synthesize styles and take
            // the slow path.
            match cell.content_tag() {
                ContentTag::Codepoint | ContentTag::CodepointGrapheme => {}
                ContentTag::BgColorPalette | ContentTag::BgColorRgb => break,
            }

            // Style transition, take the slow path.
            if format_styled(emit) && cell.style_id() as u32 != run_style_id {
                break;
            }

            let cp: u32 = cell.content_codepoint();

            // Blank cell accounting, matching the slow path blank block.
            if format_styled(emit) {
                // Styled formats only treat unstyled empty cells as
                // blank; anything else (including spaces) is written
                // so that styling is preserved.
                if cp == 0 && cell.wide() == Wide::Narrow && run_style_id == 0 {
                    pending += 1;
                    i += 1;
                    continue;
                }
            } else {
                // Cells with no text are blank.
                if cp == 0 {
                    pending += 1;
                    i += 1;
                    continue;
                }

                // Trailing spaces are blank.
                if cp == ' ' as u32 && self.opts.trim {
                    pending += 1;
                    i += 1;
                    continue;
                }
            }

            // Hyperlink state must be stable within a run: any non-blank
            // cell must belong to the currently open hyperlink (or none).
            // Transitions take the slow path. This is checked after blank
            // accounting because blank cells never touch hyperlink state.
            if hyperlinks {
                if cell.hyperlink() {
                    let Some(run_id) = run_hyperlink_id else { break };
                    let Some(cell_id) = self.page.lookup_hyperlink(cell) else { break };
                    if cell_id != run_id {
                        break;
                    }
                } else if run_hyperlink_id.is_some() {
                    break;
                }
            }

            // The page coordinate of this cell, for point tracking.
            let x: CellCountInt = (run_x as usize + i) as CellCountInt;

            // This cell produces output: materialize accumulated blanks.
            // VT steps over them (CUF), which the caller guarantees stays
            // on this row.
            if emit == Format::Vt && pending > 0 {
                writer.write_all(&buf[..len])?;
                len = 0;
                self.write_blank_step(writer, pending, x, run_y)?;
                pending = 0;
            }

            if pending > 0 {
                if track_points {
                    self.append_blank_points(pending, x, run_y);
                }

                while pending > 0 {
                    if len == buf.len() {
                        writer.write_all(&buf[..len])?;
                        len = 0;
                    }
                    let n = pending.min(buf.len() - len);
                    buf[len..len + n].fill(b' ');
                    len += n;
                    pending -= n;
                }
            }

            // Flush if the largest possible encoding may not fit.
            if len + MAX_ENCODING_LEN > buf.len() {
                writer.write_all(&buf[..len])?;
                len = 0;
            }

            let mut cell_bytes: usize;

            // Empty (but styled or wide) cells emit a space, matching
            // writeCell.
            if cp == 0 {
                buf[len] = b' ';
                len += 1;
                cell_bytes = 1;
            } else {
                cell_bytes = encode_codepoint(emit, &mut buf, &mut len, cp);

                // Multi-codepoint graphemes emit their extra codepoints,
                // matching writeCell. This is out-of-line to keep the
                // hot loop for the common single-codepoint case small.
                if cell.content_tag() == ContentTag::CodepointGrapheme {
                    cell_bytes += self.write_grapheme_cps(emit, writer, cell, &mut buf, &mut len)?;
                }
            }

            // All of the cell's bytes map to the cell's coordinate.
            if track_points {
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x, y: run_y as u32 }, cell_bytes);
                }
            }

            i += 1;
        }

        if len > 0 {
            writer.write_all(&buf[..len])?;
        }
        *blank_cells = pending;
        Ok(i)
    }

    /// Encode the extra codepoints of a multi-codepoint grapheme into
    /// buf, flushing to the writer as needed. Returns the number of
    /// bytes written. This is deliberately not inlined so that the
    /// (rare) grapheme case doesn't bloat the writeCellRun hot loop.
    #[inline(never)]
    fn write_grapheme_cps(
        &self,
        emit: Format,
        writer: &mut dyn Write,
        cell: &Cell,
        buf: &mut [u8; 512],
        len: &mut usize,
    ) -> io::Result<usize> {
        let mut bytes: usize = 0;
        for &gcp in self.page.lookup_grapheme(cell).unwrap() {
            if *len + MAX_ENCODING_LEN > buf.len() {
                writer.write_all(&buf[..*len])?;
                *len = 0;
            }
            bytes += encode_codepoint(emit, buf, len, gcp);
        }
        Ok(bytes)
    }

    /// Step over `count` blank cells (CUF) just before the cell at
    /// (x, y), all on that row. The point map entries map to the first
    /// blank cell.
    fn write_blank_step(&mut self, writer: &mut dyn Write, count: usize, x: CellCountInt, y: CellCountInt) -> io::Result<()> {
        let seq = format!("\x1b[{count}C");
        writer.write_all(seq.as_bytes())?;
        if let Some(map) = self.point_map.as_mut() {
            map.append_n(
                Coordinate {
                    x: x - count as CellCountInt,
                    y: y as u32,
                },
                seq.len(),
            );
        }
        Ok(())
    }

    /// Append the point map entries for a run of `count` blank cells
    /// that are materialized as spaces just before the cell at (x, y).
    /// Blank cells can span multiple rows if they carry over from wrap
    /// continuation, so this walks backwards from (x, y).
    fn append_blank_points(&mut self, count: usize, x: CellCountInt, y: CellCountInt) {
        let cols = self.page.size.cols;
        let Some(map) = self.point_map.as_mut() else { return };
        map.map.reserve(count);

        let mut remaining = count;
        let mut blank_x = x;
        let mut blank_y = y;
        while remaining > 0 {
            if blank_x > 0 {
                // We have space in this row
                blank_x -= 1;
            } else if blank_y > 0 {
                // Wrap to previous row
                blank_y -= 1;
                blank_x = cols - 1;
            } else {
                // Can't go back further, just use (0, 0)
                blank_x = 0;
                blank_y = 0;
            }

            map.map.push(Coordinate {
                x: blank_x,
                y: blank_y as u32,
            });
            remaining -= 1;
        }
    }

    fn write_cell(&self, tag: ContentTag, emit: Format, writer: &mut dyn Write, cell: &Cell) -> io::Result<()> {
        // Blank cells get an empty space that isn't replaced by anything
        // because it isn't really a space. We do this so that formatting
        // is preserved if we're emitting styles.
        if !cell.has_text() {
            writer.write_all(b" ")?;
            return Ok(());
        }

        self.write_codepoint_with_replacement(emit, writer, cell.content_codepoint())?;
        if tag == ContentTag::CodepointGrapheme {
            for &cp in self.page.lookup_grapheme(cell).unwrap() {
                self.write_codepoint_with_replacement(emit, writer, cp)?;
            }
        }
        Ok(())
    }

    fn write_codepoint_with_replacement(&self, emit: Format, writer: &mut dyn Write, codepoint: u32) -> io::Result<()> {
        // Search for our replacement
        let r_: Option<&Replacement> = 'replacement: {
            let Some(map) = &self.opts.codepoint_map else { break 'replacement None };
            for entry in map.iter().rev() {
                let range = entry.range;
                if range[0] <= codepoint && codepoint <= range[1] {
                    break 'replacement Some(&entry.replacement);
                }
            }

            None
        };

        // If no replacement, write it directly.
        let Some(r) = r_ else {
            return write_codepoint(emit, writer, codepoint);
        };

        match r {
            Replacement::Codepoint(v) => write_codepoint(emit, writer, *v),

            Replacement::String(s) => {
                for cp in s.chars() {
                    write_codepoint(emit, writer, cp as u32)?;
                }
                Ok(())
            }
        }
    }

    /// Returns the style for the given cell. If there is no styling this
    /// will return the default style.
    fn cell_style(&self, cell: &Cell) -> Style {
        match cell.content_tag() {
            ContentTag::Codepoint | ContentTag::CodepointGrapheme => {
                if !cell.has_styling() {
                    Style::default()
                } else {
                    // SAFETY: page memory is the base of its style set.
                    unsafe { *self.page.styles.get(self.page.memory, cell.style_id()) }
                }
            }

            ContentTag::BgColorPalette => Style {
                bg_color: style::Color::Palette(cell.content_color_palette()),
                ..Style::default()
            },

            ContentTag::BgColorRgb => {
                let rgb = cell.content_color_rgb();
                Style {
                    bg_color: style::Color::Rgb(color::Rgb {
                        r: rgb.r,
                        g: rgb.g,
                        b: rgb.b,
                    }),
                    ..Style::default()
                }
            }
        }
    }

    /// Write a string with HTML escaping. Used for escaping href attributes
    /// and other HTML attribute values.
    fn format_style_open(&self, emit: Format, writer: &mut dyn Write, style: &Style) -> io::Result<()> {
        match emit {
            Format::Plain => unreachable!(),

            Format::Vt => {
                let mut formatter = style.formatter_vt();
                formatter.palette = self.opts.palette;
                write!(writer, "{formatter}")
            }

            // We use `display: inline` so that the div doesn't impact
            // layout since we're primarily using it as a CSS wrapper.
            Format::Html => {
                let mut formatter = style.formatter_html();
                formatter.palette = self.opts.palette;
                writer.write_all(b"<div style=\"display: inline;")?;
                write!(writer, "{formatter}")?;
                writer.write_all(b"\">")
            }
        }
    }

    fn format_style_close(&mut self, emit: Format, writer: &mut dyn Write) -> io::Result<()> {
        let str_: &[u8] = match emit {
            Format::Plain => return Ok(()),
            Format::Vt => b"\x1b[0m",
            Format::Html => b"</div>",
        };

        writer.write_all(str_)?;
        if let Some(m) = self.point_map.as_mut() {
            debug_assert!(!m.map.is_empty());
            let last = m.map[m.map.len() - 1];
            m.append_n(last, str_.len());
        }
        Ok(())
    }

    fn format_hyperlink_open(&self, emit: Format, writer: &mut dyn Write, link: &hyperlink::PageEntry) -> io::Result<()> {
        // SAFETY: the slices are in this page's memory.
        let uri = unsafe { link.uri.slice(self.page.memory) };
        match emit {
            Format::Plain => unreachable!(),

            // OSC 8, with the explicit id if there is one. Keep
            // hyperlinkOpenLen in sync.
            Format::Vt => match link.id {
                hyperlink::PageEntryId::Explicit(id) => {
                    writer.write_all(b"\x1b]8;id=")?;
                    // SAFETY: as above.
                    writer.write_all(unsafe { id.slice(self.page.memory) })?;
                    writer.write_all(b";")?;
                    writer.write_all(uri)?;
                    writer.write_all(b"\x1b\\")
                }
                hyperlink::PageEntryId::Implicit(_) => {
                    writer.write_all(b"\x1b]8;;")?;
                    writer.write_all(uri)?;
                    writer.write_all(b"\x1b\\")
                }
            },

            // layout since we're primarily using it as a CSS wrapper.
            Format::Html => {
                writer.write_all(b"<a href=\"")?;
                for &byte in uri {
                    write_codepoint(emit, writer, byte as u32)?;
                }
                writer.write_all(b"\">")
            }
        }
    }

    /// The number of bytes formatHyperlinkOpen writes for `link`.
    fn hyperlink_open_len(&self, emit: Format, link: &hyperlink::PageEntry) -> io::Result<usize> {
        match emit {
            Format::Plain => unreachable!(),

            // "\x1b]8;id=" ID ";" URI "\x1b\\" or "\x1b]8;;" URI "\x1b\\"
            Format::Vt => Ok(link.uri.len
                + match link.id {
                    hyperlink::PageEntryId::Explicit(id) => id.len + 10,
                    hyperlink::PageEntryId::Implicit(_) => 7,
                }),

            // The URI is escaped, so count the bytes.
            Format::Html => {
                let mut discarding = Discarding::default();
                self.format_hyperlink_open(emit, &mut discarding, link)?;
                usize::try_from(discarding.count).map_err(|_| write_failed())
            }
        }
    }

    fn format_hyperlink_close(&mut self, emit: Format, writer: &mut dyn Write) -> io::Result<()> {
        let str_: &[u8] = match emit {
            Format::Html => b"</a>",
            Format::Vt => b"\x1b]8;;\x1b\\",
            Format::Plain => return Ok(()),
        };

        writer.write_all(str_)?;
        if let Some(m) = self.point_map.as_mut() {
            debug_assert!(!m.map.is_empty());
            let last = m.map[m.map.len() - 1];
            m.append_n(last, str_.len());
        }
        Ok(())
    }
}

/// Encode a single codepoint into buf at len, advancing len and
/// returning the number of bytes written. The caller must guarantee
/// enough remaining buffer space for the largest possible encoding.
#[inline(always)]
fn encode_codepoint(emit: Format, buf: &mut [u8; 512], len: &mut usize, cp: u32) -> usize {
    let start = *len;
    match emit {
        Format::Plain | Format::Vt => {
            if cp < 0x80 {
                buf[start] = cp as u8;
                *len += 1;
            } else {
                // Matches Writer.printUnicodeCodepoint: invalid
                // codepoints become the replacement character.
                let c = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                *len += c.encode_utf8(&mut buf[start..start + 4]).len();
            }
        }

        Format::Html => {
            let esc: Option<&[u8]> = match cp {
                0x3C => Some(b"&lt;"),
                0x3E => Some(b"&gt;"),
                0x26 => Some(b"&amp;"),
                0x22 => Some(b"&quot;"),
                0x27 => Some(b"&#39;"),
                _ => None,
            };
            if let Some(s) = esc {
                buf[start..start + s.len()].copy_from_slice(s);
                *len += s.len();
            } else if cp < 0x80 {
                // ASCII is emitted directly, everything else as a
                // numeric entity. See writeCodepoint.
                buf[start] = cp as u8;
                *len += 1;
            } else {
                buf[start..start + 2].copy_from_slice(b"&#");
                *len += 2;
                *len += print_decimal(&mut buf[*len..], cp);
                buf[*len] = b';';
                *len += 1;
            }
        }
    }

    *len - start
}

/// Whether any of the cells only carries a background color.
fn has_background_any(cells: &[Cell]) -> bool {
    for cell in cells {
        match cell.content_tag() {
            ContentTag::BgColorPalette | ContentTag::BgColorRgb => return true,
            ContentTag::Codepoint | ContentTag::CodepointGrapheme => {}
        }
    }
    false
}

/// Whether two cells both only carry the same background color.
fn same_background(a: &Cell, b: &Cell) -> bool {
    if a.content_tag() != b.content_tag() {
        return false;
    }
    match a.content_tag() {
        ContentTag::BgColorPalette => a.content_color_palette() == b.content_color_palette(),
        ContentTag::BgColorRgb => a.content_color_rgb() == b.content_color_rgb(),
        ContentTag::Codepoint | ContentTag::CodepointGrapheme => false,
    }
}

fn write_codepoint(emit: Format, writer: &mut dyn Write, codepoint: u32) -> io::Result<()> {
    match emit {
        Format::Plain | Format::Vt => print_unicode_codepoint(writer, codepoint),
        Format::Html => match codepoint {
            0x3C => writer.write_all(b"&lt;"),
            0x3E => writer.write_all(b"&gt;"),
            0x26 => writer.write_all(b"&amp;"),
            0x22 => writer.write_all(b"&quot;"),
            0x27 => writer.write_all(b"&#39;"),
            _ => {
                // For HTML, emit ASCII (< 0x80) directly, but encode
                // all non-ASCII as numeric entities to avoid encoding
                // detection issues (fixes #9426). We can't set the
                // meta tag because we emit partial HTML so this ensures
                // proper unicode handling.
                if codepoint < 0x80 {
                    writer.write_all(&[codepoint as u8])
                } else {
                    let mut buf = [0u8; 16];
                    buf[0..2].copy_from_slice(b"&#");
                    let mut len: usize = 2 + print_decimal(&mut buf[2..], codepoint);
                    buf[len] = b';';
                    len += 1;
                    writer.write_all(&buf[..len])
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ansi;
    use crate::terminal::csi;
    use crate::terminal::osc;
    use crate::terminal::point::Point;
    use crate::terminal::stream::{Action, Handler, Stream};
    use crate::terminal::terminal::{Options as TerminalOptions, SwitchScreenMode};

    // ----- test helpers (Rust-only) -----

    /// Feed bytes to the terminal (Zig: `t.vtStream().nextSlice(bytes)`).
    fn feed(t: &mut Terminal, bytes: impl AsRef<[u8]>) {
        let mut s = t.vt_stream();
        s.next_slice(bytes.as_ref());
        s.deinit();
    }

    fn term(cols: CellCountInt, rows: CellCountInt) -> Terminal {
        Terminal::init(TerminalOptions::new(cols, rows)).unwrap()
    }

    fn c(x: usize, y: usize) -> Coordinate {
        Coordinate {
            x: x as CellCountInt,
            y: y as u32,
        }
    }

    /// Asserts the page list has exactly one page and returns it.
    fn single_page<'a>(t: &Terminal) -> &'a mut Page {
        let pages = &t.screens.active.pages;
        assert!(pages.pages.first.is_some());
        assert!(pages.pages.first == pages.pages.last);
        last_page(t)
    }

    /// The last page of the active screen.
    fn last_page<'a>(t: &Terminal) -> &'a mut Page {
        let node = t.screens.active.pages.pages.last.unwrap();
        // SAFETY: the node is live while the terminal is alive.
        unsafe { (*node.as_ptr()).page() }
    }

    struct PageOut {
        out: String,
        state: TrailingState,
        map: Vec<Coordinate>,
    }

    /// Format a page with a point map. `f` configures the formatter.
    fn fmt_page(page: &Page, opts: Options<'_>, f: impl FnOnce(&mut PageFormatter<'_>)) -> PageOut {
        let mut map: Vec<Coordinate> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let state = {
            let mut formatter = PageFormatter::init(page, opts);
            f(&mut formatter);
            formatter.point_map = Some(PointMap { map: &mut map, base: 0 });
            formatter.format_with_state(&mut buf).unwrap()
        };
        PageOut {
            out: String::from_utf8(buf).unwrap(),
            state,
            map,
        }
    }

    fn opts(emit: Format) -> Options<'static> {
        Options::new(emit)
    }

    /// Assert `n` map entries starting at `off` are `(x0 + i, y)`.
    #[track_caller]
    fn assert_run(map: &[Coordinate], off: usize, n: usize, x0: usize, y: usize) {
        for i in 0..n {
            assert_eq!(c(x0 + i, y), map[off + i], "index {}", off + i);
        }
    }

    /// Assert `n` map entries starting at `off` are all `(x, y)`.
    #[track_caller]
    fn assert_same(map: &[Coordinate], off: usize, n: usize, x: usize, y: usize) {
        for i in 0..n {
            assert_eq!(c(x, y), map[off + i], "index {}", off + i);
        }
    }

    // ----- tests -----

    #[test]
    fn page_plain_single_line() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, world");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello, world", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 12, r.state.cells);

        // Verify our point map
        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, r.out.len(), 0, 0);
    }

    #[test]
    fn page_plain_single_line_soft_wrapped_unwrapped() {
        let mut t = term(3, 5);
        feed(&mut t, "hello!");
        let page = single_page(&t);

        let mut o = opts(Format::Plain);
        o.unwrap = true;
        // Note: we don't test the trailing state, which may have bugs
        // with unwrap...
        let r = fmt_page(page, o, |_| {});
        assert_eq!("hello!", r.out);

        // Verify our point map
        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 3, 0, 0);
        assert_run(&r.map, 3, 3, 0, 1);
    }

    #[test]
    fn page_plain_single_wide_char() {
        let mut t = term(80, 24);
        feed(&mut t, "1A⚡");
        let page = single_page(&t);

        // Full string
        {
            let r = fmt_page(page, Options::PLAIN, |_| {});
            assert_eq!("1A⚡", r.out);
            assert_eq!(page.size.rows as usize, r.state.rows);
            assert_eq!(page.size.cols as usize - 4, r.state.cells);

            // Verify our point map
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 2, r.out.len() - 2, 2, 0);
        }

        // Wide only (from start)
        {
            let r = fmt_page(page, Options::PLAIN, |f| f.start_x = 2);
            assert_eq!("⚡", r.out);
            assert_eq!(page.size.rows as usize, r.state.rows);
            assert_eq!(page.size.cols as usize - 4, r.state.cells);
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 0, r.out.len(), 2, 0);
        }

        // Wide only (from tail)
        {
            let r = fmt_page(page, Options::PLAIN, |f| f.start_x = 3);
            assert_eq!("⚡", r.out);
            assert_eq!(page.size.rows as usize, r.state.rows);
            assert_eq!(page.size.cols as usize - 4, r.state.cells);
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 0, r.out.len(), 2, 0);
        }
    }

    #[test]
    fn page_plain_single_wide_char_soft_wrapped_unwrapped() {
        let mut t = term(3, 24);
        feed(&mut t, "1A⚡");
        let page = single_page(&t);

        let mut o = opts(Format::Plain);
        o.unwrap = true;

        // Full string
        {
            let r = fmt_page(page, o.clone(), |_| {});
            assert_eq!("1A⚡", r.out);
            assert_eq!(page.size.rows as usize - 1, r.state.rows);
            assert_eq!(page.size.cols as usize - 2, r.state.cells);
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 2, r.out.len() - 2, 0, 1);
        }

        // Wide only (from start)
        {
            let r = fmt_page(page, o.clone(), |f| f.start_x = 2);
            assert_eq!("⚡", r.out);
            assert_eq!(page.size.rows as usize - 1, r.state.rows);
            assert_eq!(page.size.cols as usize - 2, r.state.cells);
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 0, r.out.len(), 0, 1);
        }

        // Wide only (from tail)
        {
            let r = fmt_page(page, o.clone(), |f| {
                f.start_x = 1;
                f.start_y = 1;
            });
            assert_eq!("⚡", r.out);
            assert_eq!(page.size.rows as usize - 1, r.state.rows);
            assert_eq!(page.size.cols as usize - 2, r.state.cells);
            assert_eq!(r.out.len(), r.map.len());
            assert_same(&r.map, 0, r.out.len(), 0, 1);
        }
    }

    #[test]
    fn page_plain_multiline() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
        assert_eq!(c(4, 0), r.map[5]); // \n
        assert_run(&r.map, 6, 5, 0, 1);
    }

    #[test]
    fn page_plain_multiline_rectangle() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_x = 1;
            f.end_x = Some(3);
            f.rectangle = true;
        });
        assert_eq!("ell\norl", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(0, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 3, 1, 0);
        assert_eq!(c(3, 0), r.map[3]); // \n
        assert_run(&r.map, 4, 3, 1, 1);
    }

    #[test]
    fn page_plain_multi_blank_lines() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\n\r\n\r\nworld");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello\n\n\nworld", r.out);
        assert_eq!(page.size.rows as usize - 3, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
        assert_eq!(c(4, 0), r.map[5]); // \n after row 0
        assert_eq!(c(0, 1), r.map[6]); // \n after blank row 1
        assert_eq!(c(0, 2), r.map[7]); // \n after blank row 2
        assert_run(&r.map, 8, 5, 0, 3);
    }

    #[test]
    fn page_plain_trailing_blank_lines() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld\r\n\r\n");
        let page = single_page(&t);

        // Verify output. We expect there to be no trailing newlines because
        // we can't differentiate trailing blank lines as being meaningful because
        // the page formatter can't see the cursor position.
        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
        assert_eq!(c(4, 0), r.map[5]); // \n
        assert_run(&r.map, 6, 5, 0, 1);
    }

    #[test]
    fn page_plain_trailing_whitespace() {
        let mut t = term(80, 24);
        feed(&mut t, "hello   \r\nworld   ");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
        assert_eq!(c(4, 0), r.map[5]); // \n
        assert_run(&r.map, 6, 5, 0, 1);
    }

    #[test]
    fn page_plain_trailing_whitespace_no_trim() {
        let mut t = term(80, 24);
        feed(&mut t, "hello   \r\nworld  ");
        let page = single_page(&t);

        let mut o = opts(Format::Plain);
        o.trim = false;
        let r = fmt_page(page, o, |_| {});
        assert_eq!("hello   \nworld  ", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 7, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 8, 0, 0);
        assert_eq!(c(7, 0), r.map[8]); // \n
        assert_run(&r.map, 9, 7, 0, 1);
    }

    #[test]
    fn page_plain_with_prior_trailing_state_rows() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.trailing_state = Some(TrailingState { rows: 2, cells: 0 }));
        assert_eq!("\n\nhello", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_eq!(c(0, 0), r.map[0]); // \n first blank row
        assert_eq!(c(0, 1), r.map[1]); // \n second blank row
        assert_run(&r.map, 2, 5, 0, 0);
    }

    #[test]
    fn page_plain_with_prior_trailing_state_cells_no_wrapped_line() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.trailing_state = Some(TrailingState { rows: 0, cells: 3 }));
        // Blank cells are reset when row is not a wrap continuation
        assert_eq!("hello", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
    }

    #[test]
    fn page_plain_with_prior_trailing_state_cells_with_wrap_continuation() {
        let mut t = term(80, 24);
        feed(&mut t, "world");
        let page = single_page(&t);

        // Surgically modify the first row to be a wrap continuation
        // SAFETY: row 0 is within the page.
        let row = unsafe { &mut *page.get_row(0) };
        row.set_wrap_continuation(true);

        let mut o = opts(Format::Plain);
        o.unwrap = true;
        let r = fmt_page(page, o, |f| f.trailing_state = Some(TrailingState { rows: 0, cells: 3 }));
        // Blank cells are preserved when row is a wrap continuation with unwrap enabled
        assert_eq!("   world", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        // Verify point map - 3 spaces from prior trailing state + "world"
        assert_eq!(r.out.len(), r.map.len());
        // The 3 blank cells can't go back beyond (0,0) so they all map to (0,0)
        assert_same(&r.map, 0, 3, 0, 0);
        assert_run(&r.map, 3, 5, 0, 0);
    }

    #[test]
    fn page_plain_soft_wrapped_without_unwrap() {
        let mut t = term(10, 24);
        feed(&mut t, "hello world test");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        // Without unwrap, wrapped lines show as separate lines
        assert_eq!("hello worl\nd test", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 6, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 10, 0, 0);
        assert_eq!(c(9, 0), r.map[10]); // \n
        assert_run(&r.map, 11, 6, 0, 1);
    }

    #[test]
    fn page_plain_soft_wrapped_with_unwrap() {
        let mut t = term(10, 24);
        feed(&mut t, "hello world test");
        let page = single_page(&t);

        let mut o = opts(Format::Plain);
        o.unwrap = true;
        let r = fmt_page(page, o, |_| {});
        // With unwrap, wrapped lines are joined together
        assert_eq!("hello world test", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 6, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 10, 0, 0);
        assert_run(&r.map, 10, 6, 0, 1);
    }

    #[test]
    fn page_plain_soft_wrapped_3_lines_without_unwrap() {
        let mut t = term(10, 24);
        feed(&mut t, "hello world this is a test");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        // Without unwrap, wrapped lines show as separate lines
        assert_eq!("hello worl\nd this is\na test", r.out);
        assert_eq!(page.size.rows as usize - 2, r.state.rows);
        assert_eq!(page.size.cols as usize - 6, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 10, 0, 0);
        assert_eq!(c(9, 0), r.map[10]); // \n
        assert_run(&r.map, 11, 9, 0, 1);
        assert_eq!(c(8, 1), r.map[20]); // \n
        assert_run(&r.map, 21, 6, 0, 2);
    }

    #[test]
    fn page_plain_soft_wrapped_3_lines_with_unwrap() {
        let mut t = term(10, 24);
        feed(&mut t, "hello world this is a test");
        let page = single_page(&t);

        let mut o = opts(Format::Plain);
        o.unwrap = true;
        let r = fmt_page(page, o, |_| {});
        // With unwrap, wrapped lines are joined together
        assert_eq!("hello world this is a test", r.out);
        assert_eq!(page.size.rows as usize - 2, r.state.rows);
        assert_eq!(page.size.cols as usize - 6, r.state.cells);

        // Verify point map - unwrapped text spans 3 rows
        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 10, 0, 0);
        assert_run(&r.map, 10, 10, 0, 1);
        assert_run(&r.map, 20, 6, 0, 2);
    }

    #[test]
    fn page_plain_start_y_subset() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld\r\ntest");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.start_y = 1);
        assert_eq!("world\ntest", r.out);
        assert_eq!(page.size.rows as usize - 2, r.state.rows);
        assert_eq!(page.size.cols as usize - 4, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 1);
        assert_eq!(c(4, 1), r.map[5]); // \n
        assert_run(&r.map, 6, 4, 0, 2);
    }

    #[test]
    fn page_plain_end_y_subset() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld\r\ntest");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.end_y = Some(1));
        assert_eq!("hello\nworld", r.out);
        assert_eq!(1, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
        assert_eq!(c(4, 0), r.map[5]); // \n
        assert_run(&r.map, 6, 5, 0, 1);
    }

    #[test]
    fn page_plain_start_y_and_end_y_range() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld\r\ntest\r\nfoo");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_y = 1;
            f.end_y = Some(2);
        });
        assert_eq!("world\ntest", r.out);
        assert_eq!(1, r.state.rows);
        assert_eq!(page.size.cols as usize - 4, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 1);
        assert_eq!(c(4, 1), r.map[5]); // \n
        assert_run(&r.map, 6, 4, 0, 2);
    }

    #[test]
    fn page_plain_start_y_out_of_bounds() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.start_y = 30);
        assert_eq!("", r.out);
        assert_eq!(0, r.state.rows);
        assert_eq!(0, r.state.cells);

        // Verify point map is empty
        assert_eq!(0, r.map.len());
    }

    #[test]
    fn page_plain_end_y_greater_than_rows() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.end_y = Some(30));
        // Should clamp to page.size.rows and work normally
        assert_eq!("hello", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
    }

    #[test]
    fn page_plain_end_y_less_than_start_y() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_y = 5;
            f.end_y = Some(2);
        });
        assert_eq!("", r.out);
        assert_eq!(0, r.state.rows);
        assert_eq!(0, r.state.cells);

        // Verify point map is empty
        assert_eq!(0, r.map.len());
    }

    #[test]
    fn page_plain_start_x_on_first_row_only() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.start_x = 6);
        assert_eq!("world", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 11, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 6, 0);
    }

    #[test]
    fn page_plain_end_x_on_last_row_only() {
        let mut t = term(80, 24);
        feed(&mut t, "first line\r\nsecond line\r\nthird line");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.end_y = Some(2);
            f.end_x = Some(4);
        });
        assert_eq!("first line\nsecond line\nthird", r.out);
        assert_eq!(1, r.state.rows);
        assert_eq!(0, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 10, 0, 0);
        assert_eq!(c(9, 0), r.map[10]); // \n
        assert_run(&r.map, 11, 11, 0, 1);
        assert_eq!(c(10, 1), r.map[22]); // \n
        assert_run(&r.map, 23, 5, 0, 2);
    }

    #[test]
    fn page_plain_start_x_and_end_x_multiline() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world\r\ntest case\r\nfoo bar");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_x = 6;
            f.end_y = Some(2);
            f.end_x = Some(2);
        });
        // First row: "world" (start_x=6 to end of row)
        // Second row: "test case" (full row)
        // Third row: "foo" (start to end_x=2, inclusive)
        assert_eq!("world\ntest case\nfoo", r.out);
        assert_eq!(1, r.state.rows);
        assert_eq!(0, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 6, 0);
        assert_eq!(c(10, 0), r.map[5]); // \n
        assert_run(&r.map, 6, 9, 0, 1);
        assert_eq!(c(8, 1), r.map[15]); // \n
        assert_run(&r.map, 16, 3, 0, 2);
    }

    #[test]
    fn page_plain_start_x_out_of_bounds() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.start_x = 100);
        assert_eq!("", r.out);
        assert_eq!(0, r.state.rows);
        assert_eq!(0, r.state.cells);
        assert_eq!(0, r.map.len());
    }

    #[test]
    fn page_plain_end_x_greater_than_cols() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| f.end_x = Some(100));
        assert_eq!("hello", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
    }

    #[test]
    fn page_plain_end_x_less_than_start_x_single_row() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_x = 10;
            f.end_y = Some(0);
            f.end_x = Some(5);
        });
        assert_eq!("", r.out);
        assert_eq!(0, r.state.rows);
        assert_eq!(0, r.state.cells);
        assert_eq!(0, r.map.len());
    }

    #[test]
    fn page_plain_start_y_non_zero_ignores_trailing_state() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_y = 1;
            f.trailing_state = Some(TrailingState { rows: 5, cells: 10 });
        });
        // Should NOT output the 5 newlines from trailing_state because start_y is non-zero
        assert_eq!("world", r.out);
        assert_eq!(page.size.rows as usize - 1, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 1);
    }

    #[test]
    fn page_plain_start_x_non_zero_ignores_trailing_state() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_x = 6;
            f.trailing_state = Some(TrailingState { rows: 2, cells: 8 });
        });
        // Should NOT output the 2 newlines or 8 spaces from trailing_state because start_x is non-zero
        assert_eq!("world", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 11, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 6, 0);
    }

    #[test]
    fn page_plain_start_y_and_start_x_zero_uses_trailing_state() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::PLAIN, |f| {
            f.start_y = 0;
            f.start_x = 0;
            f.trailing_state = Some(TrailingState { rows: 2, cells: 0 });
        });
        // SHOULD output the 2 newlines from trailing_state because both start_y and start_x are 0
        assert_eq!("\n\nhello", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 5, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_eq!(c(0, 0), r.map[0]); // \n first blank row
        assert_eq!(c(0, 1), r.map[1]); // \n second blank row
        assert_run(&r.map, 2, 5, 0, 0);
    }

    #[test]
    fn page_plain_single_line_with_styling() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, \x1b[1mworld\x1b[0m");
        let page = single_page(&t);

        let r = fmt_page(page, Options::PLAIN, |_| {});
        assert_eq!("hello, world", r.out);
        assert_eq!(page.size.rows as usize, r.state.rows);
        assert_eq!(page.size.cols as usize - 12, r.state.cells);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 12, 0, 0);
    }

    #[test]
    fn page_vt_single_line_plain_text() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("hello", r.out);

        assert_eq!(r.out.len(), r.map.len());
        assert_run(&r.map, 0, 5, 0, 0);
    }

    #[test]
    fn page_vt_single_line_with_bold() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b[1mhello\x1b[0m");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b[0m\x1b[1mhello\x1b[0m", r.out);

        // Verify point map - style sequences should point to first character they style
        assert_eq!(r.out.len(), r.map.len());
        // \x1b[0m = 4 bytes, \x1b[1m = 4 bytes, total 8 bytes of style sequences
        // All style bytes should map to the first styled character at (0, 0)
        assert_same(&r.map, 0, 8, 0, 0);
        // Then "hello" maps to its respective positions
        assert_run(&r.map, 8, 5, 0, 0);
    }

    #[test]
    fn page_vt_multiple_styles() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b[1mhello \x1b[3mworld\x1b[0m");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b[0m\x1b[1mhello \x1b[0m\x1b[1m\x1b[3mworld\x1b[0m", r.out);
        assert_eq!(r.out.len(), r.map.len());
    }

    #[test]
    fn page_vt_with_foreground_color() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b[31mred\x1b[0m");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b[0m\x1b[38;5;1mred\x1b[0m", r.out);

        assert_eq!(r.out.len(), r.map.len());
        // \x1b[0m = 4 bytes, \x1b[38;5;1m = 9 bytes, total 13 bytes of style sequences
        assert_same(&r.map, 0, 13, 0, 0);
        assert_run(&r.map, 13, 3, 0, 0);
    }

    #[test]
    fn page_vt_with_background_and_foreground_colors() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");
        let page = last_page(&t);

        let mut o = opts(Format::Vt);
        o.background = Some(color::Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        });
        o.foreground = Some(color::Rgb {
            r: 0xab,
            g: 0xcd,
            b: 0xef,
        });
        let r = fmt_page(page, o, |_| {});
        // Should emit OSC 10 for foreground, OSC 11 for background, then the text
        assert_eq!("\x1b]10;rgb:ab/cd/ef\x1b\\\x1b]11;rgb:12/34/56\x1b\\hello", r.out);
    }

    #[test]
    fn page_vt_multi_line_with_styles() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b[1mfirst\x1b[0m\r\n\x1b[3msecond\x1b[0m");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        // Note: style is reset before newline to prevent background colors from
        // bleeding to the next line's leading cells.
        assert_eq!("\x1b[0m\x1b[1mfirst\x1b[0m\r\n\x1b[0m\x1b[3msecond\x1b[0m", r.out);
        assert_eq!(r.out.len(), r.map.len());
    }

    #[test]
    fn page_vt_duplicate_style_not_emitted_twice() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b[1mhel\x1b[1mlo\x1b[0m");
        let page = last_page(&t);

        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b[0m\x1b[1mhello\x1b[0m", r.out);
        assert_eq!(r.out.len(), r.map.len());
    }

    fn node_ref<'a>(n: NodePtr) -> &'a crate::terminal::page_list::Node {
        // SAFETY: test nodes are live while their terminal is alive.
        unsafe { &*n.as_ptr() }
    }

    fn pin_at(node: NodePtr, x: usize, y: usize) -> Pin {
        let mut p = Pin::new(node);
        p.x = x as CellCountInt;
        p.y = y as CellCountInt;
        p
    }

    /// Format a page list with a pin map. `f` configures the formatter.
    fn fmt_list(list: &PageList, opts: Options<'_>, f: impl FnOnce(&mut PageListFormatter<'_>)) -> (String, PinMapMap) {
        let mut map = PinMapMap::EMPTY;
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut formatter = PageListFormatter::init(list, opts);
            f(&mut formatter);
            formatter.pin_map = Some(PinMap { map: &mut map });
            formatter.format(&mut buf).unwrap();
        }
        (String::from_utf8(buf).unwrap(), map)
    }

    /// Format a page list without a pin map.
    fn fmt_list_nomap(list: &PageList, opts: Options<'_>, f: impl FnOnce(&mut PageListFormatter<'_>)) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut formatter = PageListFormatter::init(list, opts);
        f(&mut formatter);
        formatter.format(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn first_page_rows(t: &Terminal) -> usize {
        node_ref(t.screens.active.pages.pages.first.unwrap()).capacity().rows as usize
    }

    fn trim_start_crlf(s: &str) -> &str {
        s.trim_start_matches(['\r', '\n'])
    }

    #[test]
    fn page_list_plain_single_line() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, world");

        let (out, pin_map) = fmt_list(&t.screens.active.pages, Options::PLAIN, |_| {});
        assert_eq!("hello, world", out);

        // Verify pin map
        assert_eq!(out.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..out.len() {
            assert_eq!(pin_at(node, i, 0), pin_map.get(i).unwrap());
        }
    }

    #[test]
    fn page_list_plain_spanning_two_pages() {
        let mut t = term(80, 24);
        let first_page_rows = first_page_rows(&t);

        // Fill the first page almost completely
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "page one");

        // Verify we're still on one page
        assert!(t.screens.active.pages.pages.first == t.screens.active.pages.pages.last);

        // Add one more newline to push content to a second page
        feed(&mut t, "\r\n");
        assert!(t.screens.active.pages.pages.first != t.screens.active.pages.pages.last);

        // Write content on the second page
        feed(&mut t, "page two");

        // Format the entire PageList
        let pages = &t.screens.active.pages;
        let (full_output, pin_map) = fmt_list(pages, Options::PLAIN, |_| {});
        let output = full_output.trim_start_matches('\n');
        assert_eq!("page one\npage two", output);

        // Verify pin map
        assert_eq!(full_output.len(), pin_map.count());
        let first_node = pages.pages.first.unwrap();
        let last_node = pages.pages.last.unwrap();
        let trimmed_count = full_output.len() - output.len();

        // First part (trimmed blank lines) maps to first node
        for i in 0..trimmed_count {
            assert_eq!(first_node, pin_map.get(i).unwrap().node);
        }

        // "page one" (8 chars) maps to first node
        for i in 0..8 {
            let idx = trimmed_count + i;
            assert_eq!(first_node, pin_map.get(idx).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(idx).unwrap().x);
        }

        // \n - maps to last node as it represents the transition to new page
        assert_eq!(last_node, pin_map.get(trimmed_count + 8).unwrap().node);

        // "page two" (8 chars) maps to last node
        for i in 0..8 {
            let idx = trimmed_count + 9 + i;
            assert_eq!(last_node, pin_map.get(idx).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(idx).unwrap().x);
        }
    }

    #[test]
    fn page_list_soft_wrapped_line_spanning_two_pages_without_unwrap() {
        let mut t = term(10, 3);
        let first_page_rows = first_page_rows(&t);

        // Fill the first page with soft-wrapped content
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "hello world test");

        // Verify we're on two pages due to wrapping
        let pages = &t.screens.active.pages;
        assert!(pages.pages.first != pages.pages.last);

        // Format without unwrap - should show line breaks
        let (full_output, pin_map) = fmt_list(pages, Options::PLAIN, |_| {});
        let output = full_output.trim_start_matches('\n');
        assert_eq!("hello worl\nd test", output);

        // Verify pin map
        assert_eq!(full_output.len(), pin_map.count());
        let first_node = pages.pages.first.unwrap();
        let last_node = pages.pages.last.unwrap();
        let trimmed_count = full_output.len() - output.len();

        // First part (trimmed blank lines) maps to first node
        for i in 0..trimmed_count {
            assert_eq!(first_node, pin_map.get(i).unwrap().node);
        }

        // First line maps to first node
        for i in 0..10 {
            assert_eq!(first_node, pin_map.get(trimmed_count + i).unwrap().node);
        }

        // \n - maps to last node as it represents the transition to new page
        assert_eq!(last_node, pin_map.get(trimmed_count + 10).unwrap().node);

        // "d test" (6 chars) maps to last node
        for i in 0..6 {
            assert_eq!(last_node, pin_map.get(trimmed_count + 11 + i).unwrap().node);
        }
    }

    #[test]
    fn page_list_soft_wrapped_line_spanning_two_pages_with_unwrap() {
        let mut t = term(10, 3);
        let first_page_rows = first_page_rows(&t);

        // Fill the first page with soft-wrapped content
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "hello world test");

        // Verify we're on two pages due to wrapping
        let pages = &t.screens.active.pages;
        assert!(pages.pages.first != pages.pages.last);

        // Format with unwrap - should join the wrapped lines
        let mut o = opts(Format::Plain);
        o.unwrap = true;
        let (full_output, pin_map) = fmt_list(pages, o, |_| {});
        let output = trim_start_crlf(&full_output);
        assert_eq!("hello world test", output);

        // Verify pin map
        assert_eq!(full_output.len(), pin_map.count());
        let first_node = pages.pages.first.unwrap();
        let last_node = pages.pages.last.unwrap();
        let trimmed_count = full_output.len() - output.len();

        // First part (trimmed blank lines) maps to first node
        for i in 0..trimmed_count {
            assert_eq!(first_node, pin_map.get(i).unwrap().node);
        }

        // First line from first page
        for i in 0..10 {
            assert_eq!(first_node, pin_map.get(trimmed_count + i).unwrap().node);
        }

        // "d test" (6 chars) from last page
        for i in 0..6 {
            assert_eq!(last_node, pin_map.get(trimmed_count + 10 + i).unwrap().node);
        }
    }

    #[test]
    fn page_list_selection_ending_on_spacer_head() {
        for across_pages in [false, true] {
            let mut t = term(3, 3);
            let first_node = t.screens.active.pages.pages.first.unwrap();
            let y = if across_pages {
                node_ref(first_node).capacity().rows as usize - 1
            } else {
                0
            };
            for _ in 0..y {
                feed(&mut t, "\r\n");
            }
            feed(&mut t, "1A⚡Z");

            let pages = &t.screens.active.pages;
            let start = pin_at(first_node, 0, y);
            let end = pin_at(first_node, 2, y);
            let mut glyph = end.down(1).unwrap();
            glyph.x = 0;
            assert_eq!(across_pages, glyph.node != first_node);

            let mut o = opts(Format::Plain);
            o.unwrap = true;

            for only_glyph in [true, false] {
                let (output, pin_map) = fmt_list(pages, o.clone(), |f| {
                    f.bottom_right = Some(end);
                    f.top_left = Some(if only_glyph { end } else { start });
                });
                assert_eq!(if only_glyph { "⚡" } else { "1A⚡" }, output);
                assert_eq!(output.len(), pin_map.count());
                for i in output.len() - "⚡".len()..output.len() {
                    assert_eq!(glyph, pin_map.get(i).unwrap());
                }
            }

            // Neither wrapped output nor rectangle selections extend the end.
            for (unwrap, rectangle) in [(false, false), (true, true)] {
                let mut o2 = o.clone();
                o2.unwrap = unwrap;
                let out = fmt_list_nomap(pages, o2, |f| {
                    f.bottom_right = Some(end);
                    f.top_left = Some(start);
                    f.rectangle = rectangle;
                });
                assert_eq!("1A", out);
            }
        }
    }

    #[test]
    fn page_list_vt_spanning_two_pages() {
        let mut t = term(80, 24);
        let first_page_rows = first_page_rows(&t);

        // Fill the first page almost completely
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "\x1b[1mpage one");

        // Verify we're still on one page
        assert!(t.screens.active.pages.pages.first == t.screens.active.pages.pages.last);

        // Add one more newline to push content to a second page
        feed(&mut t, "\r\n");
        assert!(t.screens.active.pages.pages.first != t.screens.active.pages.pages.last);

        // New content is still styled
        feed(&mut t, "page two");

        // Format the entire PageList with VT
        let pages = &t.screens.active.pages;
        let (full_output, pin_map) = fmt_list(pages, Options::VT, |_| {});
        let output = trim_start_crlf(&full_output);
        assert_eq!("\x1b[0m\x1b[1mpage one\x1b[0m\r\n\x1b[0m\x1b[1mpage two\x1b[0m", output);

        // Verify pin map
        assert_eq!(full_output.len(), pin_map.count());
        let first_node = pages.pages.first.unwrap();
        let last_node = pages.pages.last.unwrap();

        // Just verify we have entries for both pages in the pin map
        let mut first_count: usize = 0;
        let mut last_count: usize = 0;
        for byte_i in 0..pin_map.count() {
            let pin = pin_map.get(byte_i).unwrap();
            if pin.node == first_node {
                first_count += 1;
            }
            if pin.node == last_node {
                last_count += 1;
            }
        }
        assert!(first_count > 0);
        assert!(last_count > 0);
    }

    #[test]
    fn page_list_plain_with_x_offset_on_single_page() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world\r\ntest case\r\nfoo bar");

        let pages = &t.screens.active.pages;
        let node = pages.pages.first.unwrap();

        let (output, pin_map) = fmt_list(pages, Options::PLAIN, |f| {
            f.top_left = Some(pin_at(node, 6, 0));
            f.bottom_right = Some(pin_at(node, 2, 2));
        });
        assert_eq!("world\ntest case\nfoo", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        for byte_i in 0..pin_map.count() {
            assert_eq!(node, pin_map.get(byte_i).unwrap().node);
        }

        // "world" starts at x=6, y=0
        for i in 0..5 {
            assert_eq!((6 + i) as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(0, pin_map.get(i).unwrap().y);
        }
    }

    #[test]
    fn page_list_plain_with_x_offset_spanning_two_pages() {
        let mut t = term(80, 24);
        let first_page_rows = first_page_rows(&t);

        // Fill first page almost completely
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "hello world");

        // Verify we're still on one page
        assert!(t.screens.active.pages.pages.first == t.screens.active.pages.pages.last);

        // Push to second page
        feed(&mut t, "\r\n");
        assert!(t.screens.active.pages.pages.first != t.screens.active.pages.pages.last);

        feed(&mut t, "foo bar test");

        let pages = &t.screens.active.pages;
        let first_node = pages.pages.first.unwrap();
        let last_node = pages.pages.last.unwrap();

        let (full_output, pin_map) = fmt_list(pages, Options::PLAIN, |f| {
            f.top_left = Some(pin_at(first_node, 6, node_ref(first_node).rows() as usize - 1));
            f.bottom_right = Some(pin_at(last_node, 2, 1));
        });
        let output = full_output.trim_start_matches('\n');
        assert_eq!("world\nfoo", output);

        // Verify pin map
        assert_eq!(full_output.len(), pin_map.count());
        let trimmed_count = full_output.len() - output.len();

        // "world" (5 chars) from first page
        for i in 0..5 {
            let idx = trimmed_count + i;
            assert_eq!(first_node, pin_map.get(idx).unwrap().node);
            assert_eq!((6 + i) as CellCountInt, pin_map.get(idx).unwrap().x);
        }

        // \n - maps to last node as it represents the transition to new page
        assert_eq!(last_node, pin_map.get(trimmed_count + 5).unwrap().node);

        // "foo" (3 chars) from last page
        for i in 0..3 {
            let idx = trimmed_count + 6 + i;
            assert_eq!(last_node, pin_map.get(idx).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(idx).unwrap().x);
        }
    }

    #[test]
    fn page_list_plain_with_start_x_only() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");

        let pages = &t.screens.active.pages;
        let node = pages.pages.first.unwrap();

        let (output, pin_map) = fmt_list(pages, Options::PLAIN, |f| {
            f.top_left = Some(pin_at(node, 6, 0));
        });
        assert_eq!("world", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        for i in 0..5 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!((6 + i) as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(0, pin_map.get(i).unwrap().y);
        }
    }

    #[test]
    fn page_list_plain_with_end_x_only() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world\r\ntest");

        let pages = &t.screens.active.pages;
        let node = pages.pages.first.unwrap();

        let (output, pin_map) = fmt_list(pages, Options::PLAIN, |f| {
            f.bottom_right = Some(pin_at(node, 2, 1));
        });
        assert_eq!("hello world\ntes", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());

        // "hello world" (11 chars) on y=0
        for i in 0..11 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(0, pin_map.get(i).unwrap().y);
        }

        // \n
        assert_eq!(node, pin_map.get(11).unwrap().node);

        // "tes" (3 chars) on y=1
        for i in 0..3 {
            assert_eq!(node, pin_map.get(12 + i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(12 + i).unwrap().x);
            assert_eq!(1, pin_map.get(12 + i).unwrap().y);
        }
    }

    #[test]
    fn page_list_vt_without_trim_keeps_trailing_blank_rows() {
        let mut t = term(10, 6);
        feed(&mut t, "one\r\ntwo");

        let pages = &t.screens.active.pages;
        let mut o = opts(Format::Vt);
        o.trim = false;
        let (output, pin_map) = fmt_list(pages, o, |_| {});

        // Six rows, two with text, so five newlines.
        assert_eq!("one\r\ntwo\r\n\r\n\r\n\r\n", output);

        // Each newline maps to the row it ends: the first to the end of
        // the text, the others to the start of a blank row.
        assert_eq!(output.len(), pin_map.count());
        let trailing = "one\r\ntwo".len();
        for i in 0..4 {
            let pin = pin_map.get(trailing + i * 2).unwrap();
            assert_eq!(pin, pin_map.get(trailing + i * 2 + 1).unwrap());
            assert_eq!(if i == 0 { 2 } else { 0 }, pin.x);
            assert_eq!((1 + i) as CellCountInt, pin.y);
        }

        // Trimmed output and other formats still drop them.
        let trimmed = fmt_list_nomap(pages, Options::VT, |_| {});
        assert_eq!("one\r\ntwo", trimmed);

        let mut po = opts(Format::Plain);
        po.trim = false;
        let plain = fmt_list_nomap(pages, po, |_| {});
        assert_eq!("one\ntwo", plain);
    }

    #[test]
    fn page_list_vt_without_trim_keeps_a_blank_screen() {
        let t = term(10, 4);
        let mut o = opts(Format::Vt);
        o.trim = false;
        let out = fmt_list_nomap(&t.screens.active.pages, o, |_| {});
        assert_eq!("\r\n\r\n\r\n", out);
    }

    #[test]
    fn page_list_vt_without_trim_keeps_trailing_blank_rows_across_pages() {
        let mut t = term(10, 3);

        // Text on the last row of the first page, then two blank rows on
        // the next one.
        let first_page_rows = first_page_rows(&t);
        for _ in 0..first_page_rows - 1 {
            feed(&mut t, "\r\n");
        }
        feed(&mut t, "x\r\n\r\n");
        let pages = &t.screens.active.pages;
        assert!(pages.pages.first != pages.pages.last);

        let mut o = opts(Format::Vt);
        o.trim = false;
        let (output, pin_map) = fmt_list(pages, o, |_| {});
        assert!(output.ends_with("x\r\n\r\n"));
        assert_eq!(output.len(), pin_map.count());

        // The first newline ends the row with text on the first page, the
        // second the first blank row on the next page.
        let first = pin_map.get(output.len() - 4).unwrap();
        assert_eq!(pages.pages.first.unwrap(), first.node);
        assert_eq!(first_page_rows - 1, first.y as usize);
        let second = pin_map.get(output.len() - 2).unwrap();
        assert_eq!(pages.pages.last.unwrap(), second.node);
        assert_eq!(0, second.x);
    }

    fn scr(x: usize, y: usize) -> Point {
        Point::Screen(c(x, y))
    }

    #[test]
    fn page_list_plain_rectangle_basic() {
        let mut t = term(30, 5);
        feed(&mut t, "Lorem ipsum dolor\r\n");
        feed(&mut t, "sit amet, consectetur\r\n");
        feed(&mut t, "adipiscing elit, sed do\r\n");
        feed(&mut t, "eiusmod tempor incididunt\r\n");
        feed(&mut t, "ut labore et dolore");

        let pages = &t.screens.active.pages;
        let out = fmt_list_nomap(pages, Options::PLAIN, |f| {
            f.top_left = Some(pages.pin(scr(2, 1)).unwrap());
            f.bottom_right = Some(pages.pin(scr(6, 3)).unwrap());
            f.rectangle = true;
        });

        let expected = "t ame\nipisc\nusmod";
        assert_eq!(expected, out);
    }

    #[test]
    fn page_list_plain_rectangle_with_eol() {
        let mut t = term(30, 5);
        feed(&mut t, "Lorem ipsum dolor\r\n");
        feed(&mut t, "sit amet, consectetur\r\n");
        feed(&mut t, "adipiscing elit, sed do\r\n");
        feed(&mut t, "eiusmod tempor incididunt\r\n");
        feed(&mut t, "ut labore et dolore");

        let pages = &t.screens.active.pages;
        let out = fmt_list_nomap(pages, Options::PLAIN, |f| {
            f.top_left = Some(pages.pin(scr(12, 0)).unwrap());
            f.bottom_right = Some(pages.pin(scr(26, 4)).unwrap());
            f.rectangle = true;
        });

        let expected = "dolor\nnsectetur\nlit, sed do\nor incididunt\n dolore";
        assert_eq!(expected, out);
    }

    #[test]
    fn page_list_plain_rectangle_more_complex_with_breaks() {
        let mut t = term(30, 8);
        feed(&mut t, "Lorem ipsum dolor\r\n");
        feed(&mut t, "sit amet, consectetur\r\n");
        feed(&mut t, "adipiscing elit, sed do\r\n");
        feed(&mut t, "eiusmod tempor incididunt\r\n");
        feed(&mut t, "ut labore et dolore\r\n");
        feed(&mut t, "\r\n");
        feed(&mut t, "magna aliqua. Ut enim\r\n");
        feed(&mut t, "ad minim veniam, quis");

        let pages = &t.screens.active.pages;
        let out = fmt_list_nomap(pages, Options::PLAIN, |f| {
            f.top_left = Some(pages.pin(scr(11, 2)).unwrap());
            f.bottom_right = Some(pages.pin(scr(26, 7)).unwrap());
            f.rectangle = true;
        });

        let expected = "elit, sed do\npor incididunt\nt dolore\n\na. Ut enim\nniam, quis";
        assert_eq!(expected, out);
    }

    /// Format a terminal with a pin map. `f` configures the formatter.
    fn fmt_term(t: &Terminal, opts: Options<'_>, f: impl FnOnce(&mut TerminalFormatter<'_>)) -> (String, PinMapMap) {
        let mut map = PinMapMap::EMPTY;
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut formatter = TerminalFormatter::init(t, opts);
            f(&mut formatter);
            formatter.pin_map = Some(PinMap { map: &mut map });
            formatter.format(&mut buf).unwrap();
        }
        (String::from_utf8(buf).unwrap(), map)
    }

    /// Format a terminal without a pin map.
    fn fmt_term_nomap(t: &Terminal, opts: Options<'_>, f: impl FnOnce(&mut TerminalFormatter<'_>)) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut formatter = TerminalFormatter::init(t, opts);
        f(&mut formatter);
        formatter.format(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Format a screen with a pin map. `f` configures the formatter.
    fn fmt_screen(s: &Screen, opts: Options<'_>, f: impl FnOnce(&mut ScreenFormatter<'_>)) -> (String, PinMapMap) {
        let mut map = PinMapMap::EMPTY;
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut formatter = ScreenFormatter::init(s, opts);
            f(&mut formatter);
            formatter.pin_map = Some(PinMap { map: &mut map });
            formatter.format(&mut buf).unwrap();
        }
        (String::from_utf8(buf).unwrap(), map)
    }

    fn act(x: usize, y: usize) -> Point {
        Point::Active(c(x, y))
    }

    /// The selection of the active cells from (x0, y0) to (x1, y1).
    fn sel(t: &Terminal, x0: usize, y0: usize, x1: usize, y1: usize) -> Selection {
        Selection::init(
            t.screens.active.pages.pin(act(x0, y0)).unwrap(),
            t.screens.active.pages.pin(act(x1, y1)).unwrap(),
            false,
        )
    }

    fn dump(t: &Terminal) -> String {
        t.screens
            .active
            .dump_string_alloc(Point::Screen(Coordinate::default()))
            .unwrap()
    }

    #[test]
    fn terminal_formatter_plain_no_selection() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");

        let out = fmt_term_nomap(&t, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", out);
    }

    #[test]
    fn terminal_formatter_vt_with_palette() {
        let mut t = term(80, 24);

        // Modify some palette colors using VT sequences
        feed(&mut t, "\x1b]4;0;rgb:12/34/56\x1b\\");
        feed(&mut t, "\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        feed(&mut t, "\x1b]4;255;rgb:ff/00/ff\x1b\\");
        feed(&mut t, "test");

        let output = fmt_term_nomap(&t, Options::VT, |_| {});

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify the palettes match
        assert_eq!(t.colors.palette.current[0], t2.colors.palette.current[0]);
        assert_eq!(t.colors.palette.current[1], t2.colors.palette.current[1]);
        assert_eq!(t.colors.palette.current[255], t2.colors.palette.current[255]);
    }

    #[test]
    fn terminal_formatter_with_selection() {
        let mut t = term(80, 24);
        feed(&mut t, "line1\r\nline2\r\nline3");

        let s = sel(&t, 0, 1, 4, 1);
        let out = fmt_term_nomap(&t, Options::PLAIN, |f| {
            f.content = ScreenFormatterContent::Selection(Some(s));
        });
        assert_eq!("line2", out);
    }

    #[test]
    fn terminal_formatter_plain_with_pin_map() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, world");

        let (output, pin_map) = fmt_term(&t, Options::PLAIN, |_| {});
        assert_eq!("hello, world", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(pin_at(node, i, 0), pin_map.get(i).unwrap());
        }
    }

    #[test]
    fn terminal_formatter_plain_multiline_with_pin_map() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");

        let (output, pin_map) = fmt_term(&t, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();

        // "hello" (5 chars)
        for i in 0..5 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(0, pin_map.get(i).unwrap().y);
        }

        // "\n" maps to end of first line
        assert_eq!(node, pin_map.get(5).unwrap().node);

        // "world" (5 chars)
        for i in 0..5 {
            let idx = 6 + i;
            assert_eq!(node, pin_map.get(idx).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(idx).unwrap().x);
            assert_eq!(1, pin_map.get(idx).unwrap().y);
        }
    }

    #[test]
    fn terminal_formatter_vt_with_palette_and_pin_map() {
        let mut t = term(80, 24);

        // Modify some palette colors using VT sequences
        feed(&mut t, "\x1b]4;0;rgb:12/34/56\x1b\\");
        feed(&mut t, "test");

        let (output, pin_map) = fmt_term(&t, Options::VT, |_| {});

        // Verify pin map - palette bytes should be mapped to top left
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    #[test]
    fn terminal_formatter_with_selection_and_pin_map() {
        let mut t = term(80, 24);
        feed(&mut t, "line1\r\nline2\r\nline3");

        let s = sel(&t, 0, 1, 4, 1);
        let (output, pin_map) = fmt_term(&t, Options::PLAIN, |f| {
            f.content = ScreenFormatterContent::Selection(Some(s));
        });
        assert_eq!("line2", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();

        // "line2" (5 chars) from row 1
        for i in 0..5 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(1, pin_map.get(i).unwrap().y);
        }
    }

    #[test]
    fn screen_plain_single_line() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, world");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::PLAIN, |_| {});
        assert_eq!("hello, world", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(pin_at(node, i, 0), pin_map.get(i).unwrap());
        }
    }

    #[test]
    fn screen_plain_multiline() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::PLAIN, |_| {});
        assert_eq!("hello\nworld", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();

        // "hello" (5 chars)
        for i in 0..5 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(0, pin_map.get(i).unwrap().y);
        }

        // "\n" maps to end of first line
        assert_eq!(node, pin_map.get(5).unwrap().node);

        // "world" (5 chars)
        for i in 0..5 {
            let idx = 6 + i;
            assert_eq!(node, pin_map.get(idx).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(idx).unwrap().x);
            assert_eq!(1, pin_map.get(idx).unwrap().y);
        }
    }

    #[test]
    fn screen_plain_with_selection() {
        let mut t = term(80, 24);
        feed(&mut t, "line1\r\nline2\r\nline3");

        let s = sel(&t, 0, 1, 4, 1);
        let (output, pin_map) = fmt_screen(&t.screens.active, Options::PLAIN, |f| {
            f.content = ScreenFormatterContent::Selection(Some(s));
        });
        assert_eq!("line2", output);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();

        // "line2" (5 chars) from row 1
        for i in 0..5 {
            assert_eq!(node, pin_map.get(i).unwrap().node);
            assert_eq!(i as CellCountInt, pin_map.get(i).unwrap().x);
            assert_eq!(1, pin_map.get(i).unwrap().y);
        }
    }

    #[test]
    fn screen_vt_with_cursor_position() {
        let mut t = term(80, 24);

        // Position cursor at a specific location
        feed(&mut t, "hello\r\nworld");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.cursor = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify cursor positions match
        assert_eq!(t.screens.active.cursor.x, t2.screens.active.cursor.x);
        assert_eq!(t.screens.active.cursor.y, t2.screens.active.cursor.y);

        // Verify pin map - the extras should be mapped to the last pin
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        let content_len = "hello\r\nworld".len();

        // Content bytes map to their positions
        for i in 0..content_len {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }

        // Extra bytes (cursor position) map to last content pin
        for i in content_len..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    /// TerminalFormatter extras with only the screen cursor.
    fn cursor_only(f: &mut TerminalFormatter<'_>) {
        f.extra = TerminalFormatterExtra::NONE;
        f.extra.screen.cursor = true;
    }

    fn cursor_cell(t: &Terminal) -> Cell {
        // SAFETY: the cursor cell pointer is live.
        unsafe { *t.screens.active.cursor.page_cell }
    }

    #[test]
    fn terminal_vt_cursor_preserves_pending_wrap() {
        let mut source = term(4, 2);
        feed(&mut source, "abcd");
        assert!(source.screens.active.cursor.pending_wrap);

        let (output, pin_map) = fmt_term(&source, Options::VT, cursor_only);
        assert_eq!(output.len(), pin_map.count());

        let mut target = term(4, 2);
        feed(&mut target, &output);
        assert_eq!(
            source.screens.active.cursor.pending_wrap,
            target.screens.active.cursor.pending_wrap
        );

        feed(&mut source, "X");
        feed(&mut target, "X");

        assert_eq!(dump(&source), dump(&target));
    }

    #[test]
    fn terminal_vt_cursor_preserves_pending_wrap_over_a_blank_cell() {
        let mut source = term(4, 2);

        // Saving the cursor saves the pending wrap and erasing the line clears
        // it, so restoring the cursor leaves a pending wrap over a blank cell.
        feed(&mut source, "abcd\x1b7\x1b[2K\x1b8");
        assert!(source.screens.active.cursor.pending_wrap);
        assert!(!cursor_cell(&source).has_text());

        let (output, pin_map) = fmt_term(&source, Options::VT, cursor_only);
        assert_eq!(output.len(), pin_map.count());

        let mut target = term(4, 2);
        feed(&mut target, &output);
        assert_eq!(
            source.screens.active.cursor.pending_wrap,
            target.screens.active.cursor.pending_wrap
        );
        assert!(!cursor_cell(&target).has_text());

        // Formatting the restored terminal gives the same output.
        let target_output = fmt_term_nomap(&target, Options::VT, cursor_only);
        assert_eq!(output, target_output);

        // The next character goes on the next row.
        feed(&mut source, "X");
        feed(&mut target, "X");

        assert_eq!(dump(&source), dump(&target));
    }

    #[test]
    fn terminal_vt_cursor_preserves_pending_wrap_after_a_wide_character() {
        let mut source = term(4, 2);
        feed(&mut source, "ab\u{1F600}");
        assert!(source.screens.active.cursor.pending_wrap);
        assert_eq!(Wide::SpacerTail, cursor_cell(&source).wide());

        let output = fmt_term_nomap(&source, Options::VT, cursor_only);

        let mut target = term(4, 2);
        feed(&mut target, &output);
        assert_eq!(
            source.screens.active.cursor.pending_wrap,
            target.screens.active.cursor.pending_wrap
        );

        feed(&mut source, "X");
        feed(&mut target, "X");

        assert_eq!(dump(&source), dump(&target));
    }

    #[test]
    fn screen_vt_with_style() {
        let mut t = term(80, 24);

        // Set some style attributes
        feed(&mut t, "\x1b[1;31mhello");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.style = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify styles match
        assert!(t.screens.active.cursor.style.eql(&t2.screens.active.cursor.style));

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    fn node_page<'a>(n: NodePtr) -> &'a mut Page {
        // SAFETY: test nodes are live while their terminal is alive.
        unsafe { (*n.as_ptr()).page() }
    }

    #[test]
    fn screen_vt_with_hyperlink() {
        let mut t = term(80, 24);

        // Set a hyperlink
        feed(&mut t, "\x1b]8;;http://example.com\x1b\\hello");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.hyperlink = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify hyperlinks match
        let has_link1 = t.screens.active.cursor.hyperlink.is_some();
        let has_link2 = t2.screens.active.cursor.hyperlink.is_some();
        assert_eq!(has_link1, has_link2);
        if has_link1 {
            let link1 = t.screens.active.cursor.hyperlink.as_ref().unwrap();
            let link2 = t2.screens.active.cursor.hyperlink.as_ref().unwrap();
            assert_eq!(link1.uri, link2.uri);
        }

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    /// Format the last page without a point map.
    fn fmt_last_page_nomap(t: &Terminal, opts: Options<'_>) -> String {
        let page = last_page(t);
        let mut buf: Vec<u8> = Vec::new();
        let mut formatter = PageFormatter::init(page, opts);
        formatter.format(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn page_vt_with_hyperlinks() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\link text\x1b]8;;\x1b\\ normal");

        let page = last_page(&t);
        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b]8;;https://example.com\x1b\\link text\x1b]8;;\x1b\\ normal", r.out);
        assert_eq!(r.out.len(), r.map.len());
    }

    #[test]
    fn page_vt_with_explicit_hyperlink_id() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b]8;id=a;https://example.com\x1b\\one\x1b]8;;\x1b\\");

        assert_eq!(
            "\x1b]8;id=a;https://example.com\x1b\\one\x1b]8;;\x1b\\",
            fmt_last_page_nomap(&t, Options::VT)
        );
    }

    #[test]
    fn page_vt_with_hyperlink_across_rows() {
        let mut t = term(80, 24);

        // The hyperlink stays open across the line break.
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\ab\r\ncd\x1b]8;;\x1b\\");

        assert_eq!(
            "\x1b]8;;https://example.com\x1b\\ab\r\ncd\x1b]8;;\x1b\\",
            fmt_last_page_nomap(&t, Options::VT)
        );
    }

    #[test]
    fn page_vt_steps_over_blank_cells_in_a_hyperlink() {
        let mut t = term(80, 24);

        // The cells skipped by the cursor movement are blank and have no
        // hyperlink. Stepping over them doesn't write them, so the
        // hyperlink stays open.
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\ab\x1b[5Gcd\x1b]8;;\x1b\\");

        assert_eq!(
            "\x1b]8;;https://example.com\x1b\\ab\x1b[2Ccd\x1b]8;;\x1b\\",
            fmt_last_page_nomap(&t, Options::VT)
        );
    }

    #[test]
    fn page_vt_unwrap_closes_a_hyperlink_before_blank_cells() {
        let mut t = term(10, 5);

        // The blanks before the wrapped wide character carry over to the
        // next row, so they are written as spaces, which must not have the
        // hyperlink.
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\abc\x1b[10G中\x1b]8;;\x1b\\");

        let mut o = opts(Format::Vt);
        o.unwrap = true;
        assert_eq!(
            concat!(
                "\x1b]8;;https://example.com\x1b\\abc\x1b]8;;\x1b\\",
                "      ",
                "\x1b]8;;https://example.com\x1b\\中\x1b]8;;\x1b\\",
            ),
            fmt_last_page_nomap(&t, o)
        );
    }

    #[test]
    fn page_vt_switches_between_hyperlinks_without_closing() {
        let mut t = term(80, 24);

        // Opening a hyperlink replaces the open one, so no close is needed
        // between two hyperlinks.
        feed(
            &mut t,
            "\x1b]8;;https://a.example\x1b\\ab\x1b]8;;https://b.example\x1b\\cd\x1b]8;;\x1b\\",
        );

        let page = last_page(&t);
        let r = fmt_page(page, Options::VT, |_| {});

        let open_a = "\x1b]8;;https://a.example\x1b\\";
        let open_b = "\x1b]8;;https://b.example\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        assert_eq!(format!("{open_a}ab{open_b}cd{close}"), r.out);

        // Each opening maps to its first cell, the close to the last cell.
        let mut expected: Vec<Coordinate> = Vec::new();
        expected.extend(std::iter::repeat_n(c(0, 0), open_a.len() + 1));
        expected.push(c(1, 0));
        expected.extend(std::iter::repeat_n(c(2, 0), open_b.len() + 1));
        expected.extend(std::iter::repeat_n(c(3, 0), 1 + close.len()));
        assert_eq!(expected, r.map);
    }

    #[test]
    fn page_vt_closes_a_hyperlink_on_a_row_without_hyperlinks() {
        let mut t = term(80, 24);

        // The hyperlink is still open at the end of the first row, and the
        // second row has none.
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\ab\x1b]8;;\x1b\\\r\ncd");

        assert_eq!(
            "\x1b]8;;https://example.com\x1b\\ab\r\n\x1b]8;;\x1b\\cd",
            fmt_last_page_nomap(&t, Options::VT)
        );
    }

    #[test]
    fn terminal_vt_restores_hyperlinks_of_cells() {
        let mut source = term(20, 3);
        feed(
            &mut source,
            concat!(
                "\x1b]8;;https://a.example\x1b\\aa\x1b]8;;\x1b\\ ",
                "\x1b]8;id=x;https://b.example\x1b\\bb\x1b[5Gbb\x1b]8;;\x1b\\\r\n",
                "\x1b]8;;https://a.example\x1b\\\x1b[1mcc\x1b[0m\x1b]8;;\x1b\\ plain",
            ),
        );

        let output = fmt_term_nomap(&source, Options::VT, |_| {});

        let mut target = term(20, 3);
        feed(&mut target, &output);

        for y in 0..3 {
            for x in 0..20 {
                let pt = act(x, y);
                let a = source.screens.active.pages.get_cell(pt).unwrap();
                let b = target.screens.active.pages.get_cell(pt).unwrap();
                // SAFETY: cell pointers of live pages.
                let (a_cell, b_cell) = unsafe { (&*a.cell, &*b.cell) };
                assert_eq!(a_cell.hyperlink(), b_cell.hyperlink());
                if !a_cell.hyperlink() {
                    continue;
                }

                let a_page = node_page(a.node);
                let b_page = node_page(b.node);
                // SAFETY: the hyperlink ids are live entries of their pages.
                let a_link = unsafe { *a_page.hyperlink_set.get(a_page.memory, a_page.lookup_hyperlink(a_cell).unwrap()) };
                let b_link = unsafe { *b_page.hyperlink_set.get(b_page.memory, b_page.lookup_hyperlink(b_cell).unwrap()) };
                // SAFETY: the slices are in their page's memory.
                unsafe {
                    assert_eq!(a_link.uri.slice(a_page.memory), b_link.uri.slice(b_page.memory));
                }
                match (a_link.id, b_link.id) {
                    (hyperlink::PageEntryId::Explicit(a_id), hyperlink::PageEntryId::Explicit(b_id)) => {
                        // SAFETY: the slices are in their page's memory.
                        unsafe {
                            assert_eq!(a_id.slice(a_page.memory), b_id.slice(b_page.memory));
                        }
                    }
                    (hyperlink::PageEntryId::Implicit(_), hyperlink::PageEntryId::Implicit(_)) => {}
                    _ => panic!("hyperlink id kinds differ"),
                }
            }
        }
    }

    #[test]
    fn screen_vt_with_protection() {
        let mut t = term(80, 24);

        // Enable protection mode
        feed(&mut t, "\x1b[1\"qhello");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.protection = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify protection state matches
        assert_eq!(t.screens.active.cursor.protected, t2.screens.active.cursor.protected);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    #[test]
    fn screen_vt_with_kitty_keyboard() {
        let mut t = term(80, 24);

        // Set kitty keyboard flags (disambiguate + report_events = 3)
        feed(&mut t, "\x1b[=3;1uhello");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.kitty_keyboard = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify kitty keyboard state matches
        let flags1 = t.screens.active.kitty_keyboard.current().int();
        let flags2 = t2.screens.active.kitty_keyboard.current().int();
        assert_eq!(flags1, flags2);

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    #[test]
    fn screen_vt_with_charsets() {
        let mut t = term(80, 24);

        // Set G0 to DEC special and shift to G1
        feed(&mut t, "\x1b(0\x0ehello");

        let (output, pin_map) = fmt_screen(&t.screens.active, Options::VT, |f| f.extra.charsets = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify charset state matches
        assert_eq!(t.screens.active.charset.gl, t2.screens.active.charset.gl);
        assert_eq!(t.screens.active.charset.gr, t2.screens.active.charset.gr);
        assert_eq!(
            t.screens.active.charset.charsets.get(charsets::Slots::G0),
            t2.screens.active.charset.charsets.get(charsets::Slots::G0)
        );

        // Verify pin map
        assert_eq!(output.len(), pin_map.count());
        let node = t.screens.active.pages.pages.first.unwrap();
        for i in 0..output.len() {
            assert_eq!(node, pin_map.get(i).unwrap().node);
        }
    }

    #[test]
    fn terminal_vt_without_trim_keeps_blank_rows_at_the_bottom() {
        let mut source = term(10, 5);

        // Scroll three lines into the scrollback, then erase the bottom of
        // the screen, as clearing below a prompt does.
        feed(&mut source, "1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8");
        feed(&mut source, "\x1b[3;1H\x1b[J");

        let mut o = opts(Format::Vt);
        o.trim = false;
        let (output, pin_map) = fmt_term(&source, o, cursor_only);
        assert_eq!(output.len(), pin_map.count());

        let mut target = term(10, 5);
        feed(&mut target, &output);

        // The blank rows are part of the layout: without them, the replay
        // scrolls less and the rows above them end up too low.
        let source_contents = source
            .screens
            .active
            .dump_string_alloc(Point::Active(Coordinate::default()))
            .unwrap();
        let target_contents = target
            .screens
            .active
            .dump_string_alloc(Point::Active(Coordinate::default()))
            .unwrap();
        assert_eq!(source_contents, target_contents);
        assert_eq!(source.screens.active.cursor.y, target.screens.active.cursor.y);
    }

    #[test]
    fn terminal_vt_with_scrolling_region() {
        let mut t = term(80, 24);

        // Set scrolling region: top=5, bottom=20
        feed(&mut t, "\x1b[6;21rhello");

        let output = fmt_term_nomap(&t, Options::VT, |f| f.extra.scrolling_region = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify scrolling regions match
        assert_eq!(t.scrolling_region.top, t2.scrolling_region.top);
        assert_eq!(t.scrolling_region.bottom, t2.scrolling_region.bottom);
        assert_eq!(t.scrolling_region.left, t2.scrolling_region.left);
        assert_eq!(t.scrolling_region.right, t2.scrolling_region.right);
    }

    #[test]
    fn terminal_vt_with_cursor_in_origin_mode() {
        // Each stream sets a scrolling region and origin mode, then moves
        // the cursor within the region; the last one leaves a pending wrap
        // in the right column, which is restored by printing that cell again.
        // The left and right margins need mode 69.
        let streams: [&str; 3] = [
            "\x1b[3;8r\x1b[?6h\x1b[2;5H",
            "\x1b[?69h\x1b[3;8r\x1b[4;15s\x1b[?6h\x1b[2;3H",
            "\x1b[3;8r\x1b[?6h\x1b[2;20Hx",
        ];

        for stream in streams {
            let mut t = term(20, 10);
            feed(&mut t, stream);

            let output = fmt_term_nomap(&t, Options::VT, |f| f.extra = TerminalFormatterExtra::ALL);

            let mut t2 = term(20, 10);
            feed(&mut t2, &output);

            assert!(t2.modes.get(modes::Mode::Origin));
            assert_eq!(t.screens.active.cursor.x, t2.screens.active.cursor.x);
            assert_eq!(t.screens.active.cursor.y, t2.screens.active.cursor.y);
            assert_eq!(
                t.screens.active.cursor.pending_wrap,
                t2.screens.active.cursor.pending_wrap
            );
        }
    }

    #[test]
    fn terminal_vt_with_cursor_in_origin_mode_without_the_region() {
        let mut t = term(20, 10);
        feed(&mut t, "\x1b[3;8r\x1b[?6h\x1b[2;5H");

        // Without the scrolling region in the output, the replaying terminal
        // keeps full-screen margins, so the position stays absolute.
        let output = fmt_term_nomap(&t, Options::VT, |f| {
            f.extra = TerminalFormatterExtra::ALL;
            f.extra.scrolling_region = false;
        });
        assert!(output.contains("\x1b[4;5H"));
    }

    #[test]
    fn terminal_vt_with_modes() {
        let mut t = term(80, 24);

        // Enable some modes that differ from defaults
        feed(&mut t, "\x1b[?2004h"); // Bracketed paste
        feed(&mut t, "\x1b[?1000h"); // Mouse event normal
        feed(&mut t, "\x1b[?7l"); // Disable wraparound (default is true)
        feed(&mut t, "hello");

        let output = fmt_term_nomap(&t, Options::VT, |f| f.extra.modes = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify modes match
        assert_eq!(
            t.modes.get(modes::Mode::BracketedPaste),
            t2.modes.get(modes::Mode::BracketedPaste)
        );
        assert_eq!(
            t.modes.get(modes::Mode::MouseEventNormal),
            t2.modes.get(modes::Mode::MouseEventNormal)
        );
        assert_eq!(t.modes.get(modes::Mode::Wraparound), t2.modes.get(modes::Mode::Wraparound));
    }

    #[test]
    fn terminal_vt_with_tabstops() {
        let mut t = term(80, 24);

        // Clear all tabs and set custom tabstops
        feed(&mut t, "\x1b[3g"); // Clear all tabs
        feed(&mut t, "\x1b[5G\x1bH"); // Set tab at column 5
        feed(&mut t, "\x1b[15G\x1bH"); // Set tab at column 15
        feed(&mut t, "\x1b[30G\x1bH"); // Set tab at column 30
        feed(&mut t, "\x1b[Hhello");

        let (output, pin_map) = fmt_term(&t, Options::VT, |f| {
            f.extra.tabstops = true;
            f.extra.screen.cursor = true;
        });

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify tabstops match (columns are 0-indexed in the API)
        assert_eq!(t.tabstops.get(4), t2.tabstops.get(4));
        assert_eq!(t.tabstops.get(14), t2.tabstops.get(14));
        assert_eq!(t.tabstops.get(29), t2.tabstops.get(29));
        assert!(t2.tabstops.get(4)); // Column 5 (1-indexed)
        assert!(t2.tabstops.get(14)); // Column 15 (1-indexed)
        assert!(t2.tabstops.get(29)); // Column 30 (1-indexed)
        assert!(!t2.tabstops.get(8)); // Not a tab

        // Tabstop serialization must not offset the screen contents.
        for (col, expected) in "hello".bytes().enumerate() {
            let cell = t2.screens.active.pages.get_cell(scr(col, 0)).unwrap();
            // SAFETY: cell pointer of a live page.
            assert_eq!(expected as u32, unsafe { (*cell.cell).codepoint() });
        }

        // Emitting tabstops moves the cursor to each configured column. When
        // cursor state is included, it must be restored afterwards.
        assert_eq!(t.screens.active.cursor.x, t2.screens.active.cursor.x);
        assert_eq!(t.screens.active.cursor.y, t2.screens.active.cursor.y);

        // Verify the reordered terminal state is still represented in the map.
        assert_eq!(output.len(), pin_map.count());
    }

    #[test]
    fn terminal_vt_with_keyboard_modes() {
        let mut t = term(80, 24);

        // Set modify other keys mode 2
        feed(&mut t, "\x1b[>4;2m");
        feed(&mut t, "hello");

        let output = fmt_term_nomap(&t, Options::VT, |f| f.extra.keyboard = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify keyboard mode matches
        assert_eq!(t.flags.modify_other_keys_2, t2.flags.modify_other_keys_2);
        assert!(t2.flags.modify_other_keys_2);
    }

    #[test]
    fn terminal_vt_with_pwd() {
        let mut t = term(80, 24);

        // Set pwd using OSC 7
        feed(&mut t, "\x1b]7;file://host/home/user\x1b\\hello");

        let output = fmt_term_nomap(&t, Options::VT, |f| f.extra.pwd = true);

        // Create a second terminal and apply the output
        let mut t2 = term(80, 24);
        feed(&mut t2, &output);

        // Verify pwd matches
        assert_eq!(t.pwd, t2.pwd);
    }

    const HTML_WRAPPER_START: &str = "<div style=\"font-family: monospace; white-space: pre;\">";

    #[test]
    fn page_html_with_multiple_styles() {
        let mut t = term(80, 24);

        // Set bold, then italic, then reset
        feed(&mut t, "\x1b[1mbold\x1b[3mitalic\x1b[0mnormal");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;font-weight: bold;\">bold</div>",
                "<div style=\"display: inline;font-weight: bold;font-style: italic;\">italic</div>",
                "normal",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_plain_text() {
        let mut t = term(80, 24);
        feed(&mut t, "hello, world");

        // Plain text without styles should be wrapped in monospace div
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">hello, world</div>",
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_with_colors() {
        let mut t = term(80, 24);

        // Set red foreground, blue background
        feed(&mut t, "\x1b[31;44mcolored");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;color: var(--vt-palette-1);background-color: var(--vt-palette-4);\">colored</div>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn terminal_formatter_html_with_palette() {
        let mut t = term(80, 24);

        // Modify some palette colors
        feed(&mut t, "\x1b]4;0;rgb:12/34/56\x1b\\");
        feed(&mut t, "\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        feed(&mut t, "\x1b]4;255;rgb:ff/00/ff\x1b\\");
        feed(&mut t, "test");

        let output = fmt_term_nomap(&t, Options::HTML, |f| f.extra.palette = true);

        // Verify palette CSS variables are emitted
        assert!(output.contains("<style>:root{"));
        assert!(output.contains("--vt-palette-0: #123456;"));
        assert!(output.contains("--vt-palette-1: #abcdef;"));
        assert!(output.contains("--vt-palette-255: #ff00ff;"));
        assert!(output.contains("}</style>"));
        assert!(output.contains("test"));
    }

    #[test]
    fn page_html_with_background_and_foreground_colors() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");

        let mut o = opts(Format::Html);
        o.background = Some(color::Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        });
        o.foreground = Some(color::Rgb {
            r: 0xab,
            g: 0xcd,
            b: 0xef,
        });
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;background-color: #123456;color: #abcdef;\">hello</div>",
            fmt_last_page_nomap(&t, o)
        );
    }

    #[test]
    fn page_html_with_escaping() {
        let mut t = term(80, 24);
        feed(&mut t, "<tag>&\"'text");

        let page = last_page(&t);
        let r = fmt_page(page, Options::HTML, |_| {});
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">&lt;tag&gt;&amp;&quot;&#39;text</div>",
            r.out
        );

        // Verify point map length matches output
        assert_eq!(r.out.len(), r.map.len());

        // Opening wrapper div
        let wrapper_start_len = HTML_WRAPPER_START.len();
        assert_same(&r.map, 0, wrapper_start_len, 0, 0);

        // Verify each character maps correctly, accounting for escaping
        let offset = wrapper_start_len;

        // < (4 bytes: &lt;) -> x=0
        assert_same(&r.map, offset, 4, 0, 0);
        // t (1 byte) -> x=1
        assert_eq!(c(1, 0), r.map[offset + 4]);
        // a (1 byte) -> x=2
        assert_eq!(c(2, 0), r.map[offset + 5]);
        // g (1 byte) -> x=3
        assert_eq!(c(3, 0), r.map[offset + 6]);
        // > (4 bytes: &gt;) -> x=4
        assert_same(&r.map, offset + 7, 4, 4, 0);
        // & (5 bytes: &amp;) -> x=5
        assert_same(&r.map, offset + 11, 5, 5, 0);
        // " (6 bytes: &quot;) -> x=6
        assert_same(&r.map, offset + 16, 6, 6, 0);
        // ' (5 bytes: &#39;) -> x=7
        assert_same(&r.map, offset + 22, 5, 7, 0);
        // t (1 byte) -> x=8
        assert_eq!(c(8, 0), r.map[offset + 27]);
        // e (1 byte) -> x=9
        assert_eq!(c(9, 0), r.map[offset + 28]);
        // x (1 byte) -> x=10
        assert_eq!(c(10, 0), r.map[offset + 29]);
        // t (1 byte) -> x=11
        assert_eq!(c(11, 0), r.map[offset + 30]);
    }

    #[test]
    fn page_html_with_unicode_as_numeric_entities() {
        let mut t = term(80, 24);

        // Box drawing characters that caused issue #9426
        feed(&mut t, "╰─ ❯");

        // Expected: box drawing chars as numeric entities
        // ╰ = U+2570 = 9584, ─ = U+2500 = 9472, ❯ = U+276F = 10095
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">&#9584;&#9472; &#10095;</div>",
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_trailing_blank_lines() {
        let mut t = term(80, 24);
        feed(&mut t, "hello\r\nworld\r\n\r\n");
        let page = single_page(&t);

        let mut buf: Vec<u8> = Vec::new();
        let mut formatter = PageFormatter::init(page, Options::HTML);
        let state = formatter.format_with_state(&mut buf).unwrap();

        // The closing div behaves as a newline
        assert_eq!(page.size.rows as usize - 2, state.rows);
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">hello\nworld</div>",
            String::from_utf8(buf).unwrap()
        );
    }

    #[test]
    fn page_html_ascii_characters_unchanged() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");

        // ASCII should be emitted directly
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">hello world</div>",
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_mixed_ascii_and_unicode() {
        let mut t = term(80, 24);
        feed(&mut t, "test ╰─❯ ok");

        // Mix of ASCII and Unicode entities
        assert_eq!(
            "<div style=\"font-family: monospace; white-space: pre;\">test &#9584;&#9472;&#10095; ok</div>",
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_vt_with_palette_option_emits_rgb() {
        let mut t = term(80, 24);

        // Set a custom palette color and use it
        feed(&mut t, "\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        feed(&mut t, "\x1b[31mred");

        // Without palette option - should emit palette index
        assert_eq!("\x1b[0m\x1b[38;5;1mred\x1b[0m", fmt_last_page_nomap(&t, Options::VT));

        // With palette option - should emit RGB directly
        let palette = t.colors.palette.current;
        let mut o = opts(Format::Vt);
        o.palette = Some(&palette);
        assert_eq!("\x1b[0m\x1b[38;2;171;205;239mred\x1b[0m", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn page_html_with_palette_option_emits_rgb() {
        let mut t = term(80, 24);

        // Set a custom palette color and use it
        feed(&mut t, "\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        feed(&mut t, "\x1b[31mred");

        // Without palette option - should emit CSS variable
        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;color: var(--vt-palette-1);\">red</div>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );

        // With palette option - should emit RGB directly
        let palette = t.colors.palette.current;
        let mut o = opts(Format::Html);
        o.palette = Some(&palette);
        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;color: rgb(171, 205, 239);\">red</div>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, o)
        );
    }

    #[test]
    fn page_vt_style_reset_properly_closes_styles() {
        let mut t = term(80, 24);

        // Set bold, then reset with SGR 0
        feed(&mut t, "\x1b[1mbold\x1b[0mnormal");

        // The reset should properly close the bold style
        assert_eq!("\x1b[0m\x1b[1mbold\x1b[0mnormal", fmt_last_page_nomap(&t, Options::VT));
    }

    fn cp_map(range: [char; 2], replacement: Replacement) -> CodepointMap {
        CodepointMap {
            range: [range[0] as u32, range[1] as u32],
            replacement,
        }
    }

    #[test]
    fn page_codepoint_map_single_replacement() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");

        // Replace 'o' with 'x'
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![cp_map(['o', 'o'], Replacement::Codepoint('x' as u32))]);

        let page = last_page(&t);
        let r = fmt_page(page, o, |_| {});
        assert_eq!("hellx wxrld", r.out);

        // Verify point map - each output byte should map to original cell position
        assert_eq!(r.out.len(), r.map.len());
        // "hello world" -> "hellx wxrld"
        // h e l l o   w o r l d
        // 0 1 2 3 4 5 6 7 8 9 10
        assert_run(&r.map, 0, 11, 0, 0);
    }

    #[test]
    fn page_codepoint_map_conflicting_replacement_prefers_last() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");

        // Replace 'o' with 'x', then with 'y' - should prefer last
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![
            cp_map(['o', 'o'], Replacement::Codepoint('x' as u32)),
            cp_map(['o', 'o'], Replacement::Codepoint('y' as u32)),
        ]);

        let page = last_page(&t);
        let r = fmt_page(page, o, |_| {});
        assert_eq!("helly", r.out);
    }

    #[test]
    fn page_codepoint_map_replace_with_string() {
        let mut t = term(80, 24);
        feed(&mut t, "hello");

        // Replace 'o' with a multi-byte string
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![cp_map(['o', 'o'], Replacement::String("XYZ".to_owned()))]);

        let page = last_page(&t);
        let r = fmt_page(page, o, |_| {});
        assert_eq!("hellXYZ", r.out);

        // Verify point map - string replacements should all map to the original cell
        assert_eq!(r.out.len(), r.map.len());
        // "hello" -> "hellXYZ"
        // h e l l o
        // 0 1 2 3 4
        assert_run(&r.map, 0, 4, 0, 0);
        // All bytes of the replacement string "XYZ" should point to position 4 (where 'o' was)
        assert_same(&r.map, 4, 3, 4, 0);
    }

    #[test]
    fn page_codepoint_map_range_replacement() {
        let mut t = term(80, 24);
        feed(&mut t, "abcdefg");

        // Replace 'b' through 'e' with 'X'
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![cp_map(['b', 'e'], Replacement::Codepoint('X' as u32))]);

        assert_eq!("aXXXXfg", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn page_codepoint_map_multiple_ranges() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");

        // Replace 'a'-'m' with 'A' and 'n'-'z' with 'Z'
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![
            cp_map(['a', 'm'], Replacement::Codepoint('A' as u32)),
            cp_map(['n', 'z'], Replacement::Codepoint('Z' as u32)),
        ]);

        // h e l l o   w o r l d
        // A A A A Z   Z Z Z A A
        assert_eq!("AAAAZ ZZZAA", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn page_codepoint_map_unicode_replacement() {
        let mut t = term(80, 24);
        feed(&mut t, "hello ⚡ world");

        // Replace lightning bolt with fire emoji
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(vec![cp_map(['⚡', '⚡'], Replacement::String("🔥".to_owned()))]);

        let page = last_page(&t);
        let r = fmt_page(page, o, |_| {});
        assert_eq!("hello 🔥 world", r.out);

        // Verify point map
        assert_eq!(r.out.len(), r.map.len());
        // "hello ⚡ world"
        // h e l l o   ⚡   w o r l  d
        // 0 1 2 3 4 5 6   8 9 10 11 12
        // Note: ⚡ is a wide character occupying cells 6-7
        assert_run(&r.map, 0, 6, 0, 0);
        // 🔥 is 4 UTF-8 bytes, all should map to cell 6 (where ⚡ was)
        let fire_start = 6; // "hello " is 6 bytes
        assert_same(&r.map, fire_start, 4, 6, 0);
        // " world" follows
        let world_start = fire_start + 4;
        assert_run(&r.map, world_start, 6, 8, 0);
    }

    #[test]
    fn page_codepoint_map_with_styled_formats() {
        let mut t = term(10, 24);
        feed(&mut t, "\x1b[31mred text\x1b[0m");

        // Replace 'e' with 'X' in styled text
        let mut o = opts(Format::Vt);
        o.codepoint_map = Some(vec![cp_map(['e', 'e'], Replacement::Codepoint('X' as u32))]);

        // Should preserve styles while replacing text
        // "red text" becomes "rXd tXxt"
        // VT format uses \x1b[38;5;1m for palette color 1
        assert_eq!("\x1b[0m\x1b[38;5;1mrXd tXxt\x1b[0m", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn page_codepoint_map_empty_map() {
        let mut t = term(80, 24);
        feed(&mut t, "hello world");

        // Empty map should not change anything
        let mut o = opts(Format::Plain);
        o.codepoint_map = Some(Vec::new());

        assert_eq!("hello world", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn page_vt_background_color_on_trailing_blank_cells() {
        // This test reproduces a bug where trailing cells with background color
        // but no text are emitted as plain spaces without SGR sequences.
        // This causes TUIs like htop to lose background colors on rehydration.
        let mut t = term(20, 5);

        // Simulate a TUI row: "CPU:" with text, then trailing cells with red background
        // to end of line (no text after the colored region).
        // \x1b[41m sets red background, then EL fills rest of row with that bg.
        feed(&mut t, "CPU:\x1b[41m\x1b[K");
        // Reset colors and move to next line with different content
        feed(&mut t, "\x1b[0m\r\nline2");

        let mut o = opts(Format::Vt);
        o.trim = false; // Don't trim so we can see the trailing behavior
        let output = fmt_last_page_nomap(&t, o);

        // The output should preserve the red background SGR for trailing cells on line 1.
        // Bug: the first row outputs "CPU:\r\n" only - losing the background color fill.
        // The red background should appear BEFORE the newline, not after.

        // Find position of CRLF
        let crlf_pos = output.find("\r\n").expect("No CRLF found");

        // Check that red background (48;5;1) appears BEFORE the newline (on line 1)
        let line1 = &output[..crlf_pos];
        let has_red_bg_line1 = line1.contains("\x1b[41m") || line1.contains("\x1b[48;5;1m");

        // This should be true but currently fails due to the bug
        assert!(has_red_bg_line1);
    }

    #[test]
    fn page_vt_steps_over_a_gap() {
        let mut t = term(20, 5);

        // The gap after the red text is empty and has no background.
        feed(&mut t, "\x1b[41mred\x1b[0m\x1b[10Gplain");

        let page = last_page(&t);
        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("\x1b[0m\x1b[48;5;1mred\x1b[6C\x1b[0mplain", r.out);
        assert_eq!(r.out.len(), r.map.len());

        // The CUF maps to the first cell of the gap.
        let cuf = r.out.find("\x1b[6C").unwrap();
        assert_same(&r.map, cuf, 4, 3, 0);
    }

    #[test]
    fn page_vt_erases_cells_that_only_have_a_background() {
        let mut t = term(10, 5);

        // Erasing to the end of the row with a background set.
        feed(&mut t, "CPU:\x1b[41m\x1b[K\x1b[0m");

        let page = last_page(&t);
        let r = fmt_page(page, Options::VT, |_| {});
        assert_eq!("CPU:\x1b[0m\x1b[48;5;1m\x1b[6X\x1b[6C\x1b[0m", r.out);
        assert_eq!(r.out.len(), r.map.len());
    }

    #[test]
    fn page_vt_keeps_rows_that_only_have_a_background() {
        let mut t = term(5, 2);

        // Erasing the screen with a background set paints every cell.
        feed(&mut t, "\x1b[44m\x1b[2J\x1b[Hab\x1b[0m");

        assert_eq!(
            concat!(
                "\x1b[0m\x1b[48;5;4mab\x1b[3X\x1b[3C\x1b[0m\r\n",
                "\x1b[0m\x1b[48;5;4m\x1b[5X\x1b[5C\x1b[0m",
            ),
            fmt_last_page_nomap(&t, Options::VT)
        );
    }

    #[test]
    fn page_vt_unwrap_writes_blanks_before_a_wrapped_wide_character() {
        let mut t = term(10, 5);

        // The wide character doesn't fit in the last column, so it wraps,
        // and the blanks before it carry over to the next row.
        feed(&mut t, "\x1b[41mabc\x1b[0m\x1b[10G中");

        // CUF can't cross the row, so the blanks are spaces, written
        // without the pen of the text before them.
        let mut o = opts(Format::Vt);
        o.unwrap = true;
        assert_eq!("\x1b[0m\x1b[48;5;1mabc\x1b[0m      中", fmt_last_page_nomap(&t, o));
    }

    #[test]
    fn terminal_vt_restores_gaps_and_cells_that_only_have_a_background() {
        let mut source = term(20, 4);
        feed(
            &mut source,
            concat!(
                "\x1b[41mred\x1b[0m\x1b[10Gplain\r\n",
                "\x1b[44m\x1b[K\x1b[0m\r\n",
                "CPU:\x1b[42m\x1b[K\x1b[48;2;1;2;3m\x1b[15G\x1b[3X\x1b[0m\x1b[19Gx\r\n",
                "  spaces  \x1b[1mbold\x1b[0m",
            ),
        );

        let output = fmt_term_nomap(&source, Options::VT, |_| {});

        let mut target = term(20, 4);
        feed(&mut target, &output);

        for y in 0..4 {
            for x in 0..20 {
                let pt = act(x, y);
                let a = source.screens.active.pages.get_cell(pt).unwrap();
                let b = target.screens.active.pages.get_cell(pt).unwrap();
                // SAFETY: cell pointers of live pages.
                let (a_cell, b_cell) = unsafe { (&*a.cell, &*b.cell) };
                assert_eq!(a_cell.content_tag(), b_cell.content_tag());
                assert_eq!(a_cell.wide(), b_cell.wide());
                match a_cell.content_tag() {
                    ContentTag::Codepoint | ContentTag::CodepointGrapheme => {
                        assert_eq!(a_cell.codepoint(), b_cell.codepoint())
                    }
                    ContentTag::BgColorPalette => {
                        assert_eq!(a_cell.content_color_palette(), b_cell.content_color_palette())
                    }
                    ContentTag::BgColorRgb => assert_eq!(a_cell.content_color_rgb(), b_cell.content_color_rgb()),
                }
                assert!(a.style().eql(&b.style()));
            }
        }
    }

    #[test]
    fn page_html_with_hyperlinks() {
        let mut t = term(80, 24);

        // Start a hyperlink, write some text, end it
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\link text\x1b]8;;\x1b\\ normal");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<a href=\"https://example.com\">link text</a> normal",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_with_multiple_hyperlinks() {
        let mut t = term(80, 24);

        // Two different hyperlinks
        feed(&mut t, "\x1b]8;;https://first.com\x1b\\first\x1b]8;;\x1b\\ ");
        feed(&mut t, "\x1b]8;;https://second.com\x1b\\second\x1b]8;;\x1b\\");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<a href=\"https://first.com\">first</a>",
                " ",
                "<a href=\"https://second.com\">second</a>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_with_hyperlink_escaping() {
        let mut t = term(80, 24);

        // URL with special characters that need escaping
        feed(&mut t, "\x1b]8;;https://example.com?a=1&b=2\x1b\\link\x1b]8;;\x1b\\");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<a href=\"https://example.com?a=1&amp;b=2\">link</a>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_with_styled_hyperlink() {
        let mut t = term(80, 24);

        // Bold hyperlink
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\\x1b[1mbold link\x1b[0m\x1b]8;;\x1b\\");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;font-weight: bold;\">",
                "<a href=\"https://example.com\">bold link</div></a>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_hyperlink_closes_style_before_anchor() {
        let mut t = term(80, 24);

        // Styled hyperlink followed by plain text
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\\x1b[1mbold\x1b[0m plain");

        assert_eq!(
            concat!(
                "<div style=\"font-family: monospace; white-space: pre;\">",
                "<div style=\"display: inline;font-weight: bold;\">",
                "<a href=\"https://example.com\">bold</div> plain</a>",
                "</div>",
            ),
            fmt_last_page_nomap(&t, Options::HTML)
        );
    }

    #[test]
    fn page_html_hyperlink_point_map_maps_closing_to_previous_cell() {
        let mut t = term(80, 24);
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ normal");

        let page = last_page(&t);
        let r = fmt_page(page, Options::HTML, |_| {});

        let expected_output = concat!(
            "<div style=\"font-family: monospace; white-space: pre;\">",
            "<a href=\"https://example.com\">link</a> normal",
            "</div>",
        );
        assert_eq!(expected_output, r.out);
        assert_eq!(expected_output.len(), r.map.len());

        // The </a> closing tag bytes should all map to the last cell of the link
        let closing_idx = expected_output.find("</a>").unwrap();
        let expected_coord = r.map[closing_idx - 1];
        for i in closing_idx..closing_idx + "</a>".len() {
            assert_eq!(expected_coord, r.map[i]);
        }
    }
}
