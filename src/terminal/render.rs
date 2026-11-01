//! Port of `src/terminal/render.zig`.
//!
//! STATUS: ported
//!
//! Notes on the port:
//! - `std.MultiArrayList(Row)` is [`RowData`] and `std.MultiArrayList(Cell)`
//!   is [`Cells`]: struct-of-arrays with one `Vec` per field, so the hot
//!   paths (raw cell copy, dirty scans, style fills) work on contiguous
//!   per-field slices exactly like the Zig version.
//! - The per-row `ArenaAllocator` is replaced by per-row containers that are
//!   cleared with their capacity retained when the row is rebuilt: the
//!   grapheme codepoints live in [`Cells`] (`grapheme_data`, the arena)
//!   and each cell stores a range into it; `highlights` and `placeholders`
//!   are plain `Vec`s. The observable behavior (rebuilds reuse memory,
//!   nothing is freed until drop) is the same.
//! - Kitty functions that take a `*Terminal` read its geometry through
//!   `graphics_storage::TerminalInfo` (as the storage port does).
//! - Allocation failures abort (Rust global allocator), so the update
//!   functions don't return `Allocator.Error`.
//! - `fillStyles` (a hand-vectorized `@memset`) is `<[Style]>::fill`, which
//!   Rust/LLVM vectorizes for `Copy` values.
//! - Nested Zig types are flattened: `RenderState.Row.Id` is [`RowId`],
//!   `RenderState.Cursor.Viewport` is [`CursorViewport`], `Kitty.Placeholder`
//!   is [`Placeholder`], and so on.

use core::ptr::NonNull;

use indexmap::IndexSet;

use crate::terminal::build_options;
use crate::terminal::color;
use crate::terminal::cursor;
use crate::terminal::highlight;
use crate::terminal::kitty::graphics_storage::{
    ImageStorage, Location as StorageLocation, Placement as StoragePlacement, TerminalInfo,
};
use crate::terminal::kitty::graphics_unicode::{self, IncompletePlacement, Placement as UnicodePlacement};
use crate::terminal::modes::Mode;
use crate::terminal::page::{self, cell_fields, field_mask, row_fields, ContentTag, Mask, Page, Wide};
use crate::terminal::page_list::{Direction, Node, NodePtr, Pin, PinOverflow};
use crate::terminal::point;
use crate::terminal::screen::Screen;
use crate::terminal::screen_set::Key as ScreenKey;
use crate::terminal::selection::Selection;
use crate::terminal::size::CellCountInt;
use crate::terminal::style::{self, Style};
use crate::terminal::terminal::{Dirty as TerminalDirty, Terminal};

const LOG_TARGET: &str = "render_state";

// Developer note: this is in src/terminal and not src/renderer because
// the goal is that this remains generic to multiple renderers. This can
// aid specifically with libghostty-vt with converting terminal state to
// a renderable form.

/// Contains the state required to render the screen, including optimizing
/// for repeated render calls and only rendering dirty regions.
///
/// Previously, our renderer would use `clone` to clone the screen within
/// the viewport to perform rendering. This worked well enough that we kept
/// it all the way up through the Ghostty 1.2.x series, but the clone time
/// was repeatedly a bottleneck blocking IO.
///
/// Rather than a generic clone that tries to clone all screen state per call
/// (within a region), a stateful approach that optimizes for only what a
/// renderer needs to do makes more sense.
///
/// To use this, initialize the render state to empty, then call `update`
/// on each frame to update the state to the latest terminal state.
///
/// ```text
/// let mut state = RenderState::empty();
/// state.update(&mut terminal);
/// ```
///
/// ## Two-Phase Updates
///
/// For callers that synchronize terminal access (e.g. a renderer thread
/// sharing a lock with an IO thread), the update can be split into two
/// phases to minimize the time the terminal must be held exclusively:
/// `begin_update` requires terminal access, while `end_update` completes
/// any deferred work using only memory owned by the render state.
///
/// ```text
/// {
///     let mut t = mutex.lock();
///     state.begin_update(&mut t);
/// }
///
/// // The IO thread is free to modify the terminal while we
/// // complete the update.
/// state.end_update();
/// ```
///
/// The render state must be treated as incomplete between the two calls.
/// `update` is a convenience that performs both phases in one call.
///
/// ## Overscan
///
/// By default the render state captures exactly the rows of the viewport.
/// A renderer that draws the grid at a fractional row offset, such as for
/// smooth scrolling, also needs the rows just outside the viewport so that
/// partially visible rows at the top and bottom edges can be drawn. Set
/// `overscan_request` to ask for extra rows above and below the viewport:
///
/// ```text
/// let mut state = RenderState::empty();
/// state.overscan_request = Overscan { above: 1, below: 1 };
/// state.update(&mut terminal);
/// ```
///
/// With overscan, `row_data` holds more than `rows` entries, laid out
/// like this for a request of one row above and one below:
///
/// ```text
/// index 0            overscan above   (viewport y = -1)
/// index 1            viewport row 0   <- viewport_start()
/// ...
/// index rows         viewport row rows - 1
/// index rows + 1     overscan below   (viewport y = rows)
/// ```
///
/// The viewport always begins at `viewport_start()`, so viewport rows never
/// move within `row_data` from one update to the next. The overscan rows
/// are only captured if they exist. There is nothing above the first row
/// of scrollback, and nothing below the viewport while it follows the
/// active area (the usual case when not scrolled). Missing rows leave
/// their entries unused, and `overscan` reports how many rows were
/// actually captured. Always read rows through `row_data_range()`:
///
/// ```text
/// let range = state.row_data_range();
/// for i in range.start..range.end {
///     let y = state.viewport_y(i); // negative above the viewport
///     let cells = &state.row_data.cells[i];
///     // draw `cells` at row `y` shifted by the scroll offset
/// }
/// ```
///
/// Overscan rows carry the same data as viewport rows (cells, styles,
/// dirty flags, selection, and highlights). The cursor is only reported
/// in `cursor.viewport` when it is inside the viewport itself. `string`
/// and `link_cells` only look at the viewport.
///
/// With no overscan requested (the default), `row_data` holds exactly
/// the viewport and indices are viewport y values, as before.
///
/// ## Images
///
/// The render state also gives the images to draw, as a list of image
/// placements (`ImagePlacement`). Each one is a rectangle of one image,
/// with its position, size, source rectangle and draw order. These mean
/// the same thing for every image protocol, so a renderer can draw them
/// all the same way. Only reading the pixels depends on the protocol:
///
/// ```text
/// for p in state.image_placement_iterator() {
///     // `kitty_texture` and `queue` stand in for your renderer.
///     // Find or upload a texture keyed by the protocol, image ID
///     // and image generation. Only read pixels on a miss.
///     let texture = match p.protocol {
///         ImagePlacementProtocol::Kitty(_) => kitty_texture(p.image_id, p.image_generation),
///     };
///
///     // Draw p.source_* of the texture at cell (p.x, p.y), offset
///     // by (p.offset_x, p.offset_y) pixels, at a size of p.width by
///     // p.height pixels, sorted by p.layer and then p.z.
///     queue(texture, p);
/// }
/// ```
///
/// ### Knowing When Images Changed
///
/// Most frames don't change any image. A program prints text or moves the
/// cursor, and every image stays where it was. You don't need to rebuild
/// your image draw list on those frames. After each update, call
/// `image_placements_generation` and compare the result with the value
/// from the last time you read the list. If the two match, the list is the
/// same as before, and you can keep drawing what you built last time:
///
/// ```text
/// // Kept by your renderer between frames. None until the list has
/// // been read once, so that the first frame always reads it.
/// let mut placements_generation: Option<u64> = None;
///
/// // Each frame:
/// state.update(&mut terminal);
/// let generation = state.image_placements_generation();
/// if placements_generation != Some(generation) {
///     placements_generation = Some(generation);
///     rebuild_image_draw_list(state.image_placement_iterator());
/// }
/// draw_image_draw_list();
/// ```
///
/// The value changes when an update adds or removes an image, moves one,
/// changes which part of one is shown, or changes the pixels of one. It
/// stays the same when only text changes, as long as the changed rows had
/// no Kitty placeholder cells before or after the change. (See "Kitty
/// Graphics Placeholders" below for what those are.)
///
/// A new value means "read the list again". It doesn't promise that
/// anything on screen moved. The value can change even when every image
/// ends up exactly where it was, for example:
///
///   - When a program rewrites a row that has placeholder cells.
///   - When a program changes any image or placement while images shown
///     with placeholder cells are on screen.
///
/// Only compare the value for equality. It wraps around, so a larger
/// value doesn't mean a newer one. It is always 0 when Kitty graphics are
/// disabled.
///
/// Don't confuse this value with `ImagePlacement::image_generation`. That
/// one belongs to a single image and tells you when to upload its pixels
/// again. This one belongs to the whole list and tells you when to read
/// the list again.
///
/// ### Which Images Are Included
///
/// The list holds every Kitty graphics image on screen:
///
///   - Images shown with placeholder cells. See "Kitty Graphics
///     Placeholders" below.
///   - Images placed at a cell on the screen. These scroll with the
///     text around them, like a line of output. See "Kitty Graphics
///     Fixed Placements" below.
///   - Images placed relative to another image. See "Kitty Graphics
///     Relative Placements" below.
///
/// You don't need to read the terminal's Kitty image storage to find
/// what to draw. Don't draw images from the storage as well, or they are
/// drawn twice.
///
/// ### Images Partly Outside the Captured Rows
///
/// An image is in the list when any part of it is in the captured rows
/// (the viewport plus any overscan rows). Images shown with placeholder
/// cells are split into one placement per row of cells, so each of those
/// placements is always inside the captured rows.
///
/// Every other placement is reported at its full size, even when only
/// part of it can be seen. Its `x` can be negative, and its `y` can be
/// above the first overscan row. It can also extend past the right or
/// bottom edge. Clip it to the area you draw. Most graphics APIs can do
/// this for you with a scissor or clip rectangle. For example, with no
/// overscan:
///
/// ```text
/// // A fixed image 4 rows tall whose top row scrolled 2 rows above
/// // the viewport has y = -2 and rows = 4. Its pixel position is
/// // found the usual way, and `top` is negative.
/// let left = p.x * cell_width + p.offset_x;
/// let top = p.y * cell_height + p.offset_y;
///
/// // Clip to the viewport and draw the whole image. Rows -2 and -1
/// // are cut off, and rows 0 and 1 show at the top of the viewport.
/// set_clip(0, 0, viewport_width, viewport_height);
/// draw_image(texture, p, left, top, p.width, p.height);
/// ```
///
/// ### Kitty Graphics Fixed Placements
///
/// A program can place a Kitty image at a cell on the screen. The image
/// is attached to that cell, so it moves when the text moves: when new
/// output scrolls the screen, when a program scrolls part of the screen,
/// and when the viewport scrolls through the scrollback.
///
/// The render state finds the position of these images during each
/// update. Most updates only need a few comparisons for this. When the
/// images or the cell size change, the render state reads the image
/// storage again. When only their positions can have changed, such as
/// on a scroll, it moves the images it already knows about without
/// reading the storage.
///
/// ### Kitty Graphics Placeholders
///
/// The Kitty graphics protocol lets a program place an image with text.
/// The program creates a virtual placement, which has no position of its
/// own, and prints placeholder characters (U+10EEEE) where the image
/// should appear. Each placeholder cell says which image it shows and
/// which piece of the image belongs in that cell. The image then moves
/// with the text.
///
/// Because the cells decide where these images go, the render state
/// works out where to draw them during each update:
///
///   1. When a row is rebuilt, its placeholder cells are grouped into
///      runs of neighboring cells that show the same image. These are
///      stored in `RowData::placeholders`.
///   2. `begin_update` then looks up each new run's image and placement
///      and calculates the part of the image it shows, and where. Runs
///      on rows that didn't change keep their results, unless the
///      images or the cell size changed.
///   3. Relative placements attached to a virtual placement are
///      positioned from that placement's runs.
///
/// ### Kitty Graphics Relative Placements
///
/// A program can also place a Kitty image relative to another placement,
/// its parent. The image is drawn a number of cells to the right of and
/// below its parent. Negative numbers move it left or up. The parent can
/// be relative to another placement too, so placements can form a chain.
///
/// The render state follows the chain to the placement at its start and
/// positions the image from there:
///
///   - If the chain starts at an image placed at a cell, the image is
///     drawn from that cell, and moves whenever that cell moves.
///   - If the chain starts at a virtual placement, the image is drawn
///     from the top-left corner of that placement's placeholder cells.
///     If none of those cells are in the captured rows, the image is not
///     in the list.
///
/// For example, an image placed 3 columns right of and 2 rows below a
/// parent at column 2, row 1 is drawn at column 5, row 3. Like other
/// placements, it is in the list when any part of it is in the captured
/// rows, even if its parent isn't.
///
/// ## Memory
///
/// Note: the render state retains as much memory as possible between updates
/// to prevent future allocations. If a very large frame is rendered once,
/// the render state will retain that much memory until dropped. To avoid
/// waste, it is recommended that the caller drop it and start with an
/// empty render state every so often.
pub struct RenderState {
    /// The current screen dimensions. It is possible that these don't match
    /// the renderer's current dimensions in grid cells because resizing
    /// can happen asynchronously. For example, for Metal, our NSView resizes
    /// at a different time than when our internal terminal state resizes.
    /// This can lead to a one or two frame mismatch a renderer needs to
    /// handle.
    ///
    /// The viewport is always exactly equal to the active area size so this
    /// is also the viewport size. It does not include overscan rows.
    pub rows: CellCountInt,
    pub cols: CellCountInt,

    /// The color state for the terminal.
    pub colors: Colors,

    /// Cursor state within the viewport.
    pub cursor: Cursor,

    /// The captured rows, from top to bottom.
    ///
    /// Without overscan this has exactly `rows` entries and the index is
    /// the viewport y. With overscan it has room for the requested rows
    /// above and below the viewport, and the viewport starts at
    /// `viewport_start()`. Only the entries in `row_data_range()` hold data
    /// from the last update. See "Overscan" in the `RenderState` docs.
    ///
    /// This is a struct-of-arrays (Zig: `MultiArrayList`) because only the
    /// update cares about some fields. Callers care about all the other
    /// properties, and this better optimizes cache locality for read
    /// access for those use cases.
    pub row_data: RowData,

    /// The dirty state of the render state. This is set by the update method.
    /// The renderer/caller should set this to false when it has handled
    /// the dirty state.
    pub dirty: Dirty,

    /// The screen type that this state represents. This is used primarily
    /// to detect changes.
    pub screen: ScreenKey,

    /// The last viewport pin used to generate this state. This is NOT
    /// a tracked pin and is generally NOT safe to read other than the direct
    /// values for comparison.
    pub viewport_pin: Option<Pin>,

    /// The number of rows to capture above and below the viewport. This is
    /// for renderers that draw partially visible rows, such as during
    /// smooth scrolling. The default of zero captures only the viewport.
    /// See "Overscan" in the `RenderState` docs.
    ///
    /// Only change this immediately before an update, because
    /// `viewport_start()` reads it and `row_data` is laid out for it by the
    /// update. Changing it causes the next update to be a full redraw.
    pub overscan_request: Overscan,

    /// The number of rows above and below the viewport that the last
    /// update actually captured. This is never more than
    /// `overscan_request`, and is less when those rows don't exist:
    ///
    ///   - `above` is less near the top of the scrollback.
    ///   - `below` is less when the viewport is close to the bottom of
    ///     the screen, and is zero when the viewport follows the active
    ///     area.
    ///
    /// This is set by the update and should not be modified.
    pub overscan: Overscan,

    /// The cached selection so we can avoid expensive selection calculations
    /// if possible.
    pub selection_cache: Option<SelectionCache>,

    /// The pending style runs requiring an end_update call, in the
    /// order they were recorded. If multiple begins happen without an
    /// end_update call, runs accumulate; rows rebuilt more than once
    /// may then have superseded (stale) runs in this list, which is
    /// harmless: newer runs are appended later so they win, and cells
    /// not covered by newer runs have a default style ID in their raw
    /// data so their style is undefined by contract anyway. See
    /// begin_update.
    pub pending_styles: Vec<StyleRun>,

    /// Internal state for Kitty graphics placeholders. Read the results
    /// with `image_placement_iterator` instead.
    pub kitty: KittyData,
}

// SAFETY: the raw page-list pointers stored in a render state (row pins,
// the viewport pin, the Kitty chunk nodes and pin anchors) are only
// dereferenced during `begin_update`/`link_cells`, when the caller
// guarantees exclusive access to the terminal they came from. Everything
// else is owned data.
unsafe impl Send for RenderState {}

impl Default for RenderState {
    fn default() -> Self {
        RenderState::empty()
    }
}

/// The color state for the terminal.
///
/// The background/foreground will be reversed if the terminal reverse
/// color mode is on! You do not need to handle that manually!
/// (Zig: `RenderState.Colors`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub background: color::Rgb,
    pub foreground: color::Rgb,
    pub cursor: Option<color::Rgb>,
    pub palette: color::Palette,
}

/// (Zig: `RenderState.Cursor`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The x/y position of the cursor within the active area.
    pub active: point::Coordinate,

    /// The x/y position of the cursor within the viewport. This
    /// may be null if the cursor is not visible within the viewport.
    pub viewport: Option<CursorViewport>,

    /// The cell data for the cursor position. Managed memory is not
    /// safe to access from this.
    pub cell: page::Cell,

    /// The style, always valid even if the cell is default style.
    pub style: Style,

    /// The visual style of the cursor itself, such as a block or
    /// bar.
    pub visual_style: cursor::Style,

    /// True if the cursor is detected to be at a password input field.
    pub password_input: bool,

    /// Cursor visibility state determined by the terminal mode.
    pub visible: bool,

    /// Cursor blink state determined by the terminal mode.
    pub blinking: bool,
}

/// (Zig: `RenderState.Cursor.Viewport`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorViewport {
    /// The x/y position of the cursor within the viewport.
    pub x: CellCountInt,
    pub y: CellCountInt,

    /// Whether the cursor is part of a wide character and
    /// on the tail of it. If so, some renderers may use this
    /// to move the cursor back one.
    pub wide_tail: bool,
}

/// A number of rows above and below the viewport. Used both to request
/// overscan (`overscan_request`) and to report what was captured
/// (`overscan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Overscan {
    /// Rows above the top of the viewport.
    pub above: CellCountInt,

    /// Rows below the bottom of the viewport.
    pub below: CellCountInt,
}

impl Overscan {
    pub fn eql(self, b: Overscan) -> bool {
        self.above == b.above && self.below == b.below
    }
}

/// The captured rows (Zig: `std.MultiArrayList(RenderState.Row)`). Each
/// field of `RenderState.Row` is one `Vec` here, indexed by the
/// `row_data` index. Use [`RowData::get`] for a [`Row`] view of one entry.
#[derive(Debug, Default)]
pub struct RowData {
    /// The page pin. Its copied values may be compared, but its node must
    /// not be dereferenced unless the terminal state is protected from
    /// changes since the last `update` call.
    pub pin: Vec<Pin>,

    /// The page node generation captured alongside `pin`. This lets
    /// consumers validate the pin without dereferencing its node after
    /// the terminal lock has been released.
    pub serial: Vec<u64>,

    /// Raw row data.
    pub raw: Vec<page::Row>,

    /// The cells in this row. Guaranteed to be `cols` length.
    ///
    /// The cell storage uses the general allocator (NOT a per-row
    /// arena). We do this on purpose because as rows become dirty, we
    /// do not want to reallocate space for cells (which are large).
    pub cells: Vec<Cells>,

    /// A dirty flag that can be used by the renderer to track
    /// its own draw state. `update` will mark this true whenever
    /// this row is changed, too.
    pub dirty: Vec<bool>,

    /// The x range of the selection within this row.
    pub selection: Vec<Option<[CellCountInt; 2]>>,

    /// The highlights within this row.
    pub highlights: Vec<Vec<Highlight>>,

    /// The runs of Kitty graphics placeholder cells in this row, from
    /// left to right. Most callers want `image_placement_iterator`,
    /// which reads these for every row. This is always empty when
    /// Kitty graphics are disabled.
    pub placeholders: Vec<Vec<Placeholder>>,

    /// The style runs applied to this row's per-cell style data
    /// by the last `end_update` that touched it. This is what lets
    /// `end_update` skip the (comparatively large) per-cell style
    /// fill when a rebuilt row produced identical runs, which is
    /// the common case: text changes far more often than styling.
    ///
    /// The invariant is that this always describes the current
    /// contents of the cell style data for the covered ranges. It
    /// is cleared whenever the cell storage is reallocated.
    ///
    /// This must survive row rebuilds. `end_update` cannot allocate,
    /// so `begin_update` reserves the capacity.
    pub applied_styles: Vec<Vec<StyleRun>>,
}

impl RowData {
    /// The number of entries (Zig: `MultiArrayList.len`).
    #[inline]
    pub fn len(&self) -> usize {
        self.pin.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pin.is_empty()
    }

    /// A view of the entry at `index` (Zig: `MultiArrayList.get`).
    pub fn get(&self, index: usize) -> Row<'_> {
        Row {
            pin: self.pin[index],
            serial: self.serial[index],
            raw: self.raw[index],
            cells: &self.cells[index],
            dirty: self.dirty[index],
            selection: self.selection[index],
            highlights: &self.highlights[index],
            placeholders: &self.placeholders[index],
            applied_styles: &self.applied_styles[index],
        }
    }

    /// Resize to `len` entries. New entries get the initial row values
    /// (no cells, dirty, no selection); removed entries free their memory.
    fn resize(&mut self, len: usize) {
        // An unused pin. Its node is never dereferenced: entries are only
        // read after an update stores real pins in them.
        let unused_pin = Pin::new(NonNull::dangling());
        self.pin.resize(len, unused_pin);
        self.serial.resize(len, 0);
        self.raw.resize(len, page::Row::default());
        self.cells.resize_with(len, Cells::default);
        self.dirty.resize(len, true);
        self.selection.resize(len, None);
        self.highlights.resize_with(len, Vec::new);
        self.placeholders.resize_with(len, Vec::new);
        self.applied_styles.resize_with(len, Vec::new);
    }
}

/// A captured row. This is either a viewport row or an overscan row,
/// depending on its index in `row_data`. (Zig: `RenderState.Row`; in
/// Rust this is a borrowed view of one [`RowData`] entry.)
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    /// The page pin. Its copied values may be compared, but its node must
    /// not be dereferenced unless the terminal state is protected from
    /// changes since the last `update` call.
    pub pin: Pin,

    /// The page node generation captured alongside `pin`.
    pub serial: u64,

    /// Raw row data.
    pub raw: page::Row,

    /// The cells in this row. Guaranteed to be `cols` length.
    pub cells: &'a Cells,

    /// A dirty flag that can be used by the renderer to track
    /// its own draw state.
    pub dirty: bool,

    /// The x range of the selection within this row.
    pub selection: Option<[CellCountInt; 2]>,

    /// The highlights within this row.
    pub highlights: &'a [Highlight],

    /// The runs of Kitty graphics placeholder cells in this row.
    pub placeholders: &'a [Placeholder],

    /// The style runs applied to this row's per-cell style data.
    pub applied_styles: &'a [StyleRun],
}

impl Row<'_> {
    /// Returns the identity of this row. See [`RowId`].
    ///
    /// This only reads values copied during the update, so it is safe
    /// to call without access to the terminal.
    pub fn id(&self) -> RowId {
        RowId {
            serial: self.serial,
            y: self.pin.y,
        }
    }
}

/// Identifies a row of terminal content across updates, independent
/// of where the row is in `row_data`. Get it with [`Row::id`].
/// (Zig: `RenderState.Row.Id`.)
///
/// When the viewport scrolls, rows move to different `row_data`
/// indices but keep their ids. This lets a renderer keep per-row
/// work (such as shaped text or cached geometry) and look it up by
/// id after each update rather than rebuilding everything:
///
/// ```text
/// let range = state.row_data_range();
/// for i in range.start..range.end {
///     let row = state.row_data.get(i);
///     if !row.dirty {
///         if let Some(cached) = cache.get(&row.id()) {
///             // reuse `cached` at the new position
///             continue;
///         }
///     }
///     // rebuild this row and cache it under row.id()
/// }
/// ```
///
/// The guarantee is: if a row has the same id as a row from the
/// previous update of the same render state and is not marked dirty,
/// its contents are unchanged. An id that no longer appears means
/// the row scrolled out of the captured rows, was removed from
/// scrollback, or was rearranged in place by the terminal (for
/// example, scrolling within a scroll region). Ids are never reused,
/// so a stale id never matches a different row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowId {
    /// The generation of the page holding the row
    /// (`page_list::Node::serial`).
    pub serial: u64,

    /// The row index within its page.
    pub y: CellCountInt,
}

impl RowId {
    pub fn eql(self, b: RowId) -> bool {
        self.serial == b.serial && self.y == b.y
    }
}

/// (Zig: `RenderState.Highlight`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    /// A special tag that can be used by the caller to differentiate
    /// different highlight types. The value is opaque to the RenderState.
    pub tag: u8,

    /// The x ranges of highlights within this row.
    pub range: [CellCountInt; 2],
}

/// A range of grapheme codepoints in a [`Cells`] grapheme arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct GraphemeRange {
    start: u32,
    len: u32,
}

/// The cells of one row (Zig: `std.MultiArrayList(RenderState.Cell)`).
///
/// Each field of [`Cell`] is one `Vec`, so the raw cell data is a plain
/// contiguous copy of the page cells. Grapheme codepoints are stored in a
/// per-row arena (`grapheme_data`) that is cleared (with its capacity
/// retained) whenever the row is rebuilt.
#[derive(Debug, Clone, Default)]
pub struct Cells {
    /// Always set, this is the raw copied cell data from page.Cell.
    /// The managed memory (hyperlinks, graphemes, etc.) is NOT safe
    /// to access from here. It is duplicated into the other fields if
    /// it exists.
    pub raw: Vec<page::Cell>,

    /// The style data for each cell. A style is undefined (stale) unless
    /// the style_id is non-default on raw (or the cell is a background
    /// color cell in a row with managed memory).
    pub style: Vec<Style>,

    /// The grapheme data of each cell, a range in `grapheme_data`. This
    /// is undefined unless the raw cell's content_tag is
    /// `codepoint_grapheme`.
    grapheme: Vec<GraphemeRange>,

    /// The arena for grapheme codepoints of this row.
    grapheme_data: Vec<u32>,
}

impl Cells {
    /// The number of cells (Zig: `MultiArrayList.len`).
    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// The cell at `x` (Zig: `MultiArrayList.get`).
    #[inline]
    pub fn get(&self, x: usize) -> Cell<'_> {
        self.slice().get(x)
    }

    /// The grapheme codepoints (beyond the first) of the cell at `x`.
    /// This is undefined (but memory safe) unless the raw cell's
    /// content_tag is `codepoint_grapheme`.
    #[inline]
    pub fn grapheme(&self, x: usize) -> &[u32] {
        self.slice().grapheme(x)
    }

    /// A borrowed view of all fields (Zig: `MultiArrayList.slice`).
    #[inline]
    pub fn slice(&self) -> CellsSlice<'_> {
        CellsSlice {
            raw: &self.raw,
            style: &self.style,
            grapheme: &self.grapheme,
            grapheme_data: &self.grapheme_data,
        }
    }

    /// Append a cell (Zig: `MultiArrayList.append`).
    pub fn append(&mut self, cell: Cell<'_>) {
        self.raw.push(cell.raw);
        self.style.push(cell.style);
        let range = self.push_grapheme(cell.grapheme);
        self.grapheme.push(range);
    }

    /// Set the grapheme of the cell at `x`, copying `cps` into the arena.
    pub fn set_grapheme(&mut self, x: usize, cps: &[u32]) {
        let range = self.push_grapheme(cps);
        self.grapheme[x] = range;
    }

    fn push_grapheme(&mut self, cps: &[u32]) -> GraphemeRange {
        let start = self.grapheme_data.len() as u32;
        self.grapheme_data.extend_from_slice(cps);
        GraphemeRange {
            start,
            len: cps.len() as u32,
        }
    }

    /// Resize to `len` cells. New cells are zeroed.
    fn resize(&mut self, len: usize) {
        self.raw.resize(len, page::Cell::default());
        self.style.resize(len, Style::default());
        self.grapheme.resize(len, GraphemeRange::default());
    }
}

/// A borrowed view of the cells of one row (Zig:
/// `std.MultiArrayList(RenderState.Cell).Slice`).
#[derive(Debug, Clone, Copy)]
pub struct CellsSlice<'a> {
    /// See [`Cells::raw`].
    pub raw: &'a [page::Cell],

    /// See [`Cells::style`].
    pub style: &'a [Style],

    grapheme: &'a [GraphemeRange],
    grapheme_data: &'a [u32],
}

impl<'a> CellsSlice<'a> {
    /// An empty slice.
    pub const EMPTY: CellsSlice<'static> = CellsSlice {
        raw: &[],
        style: &[],
        grapheme: &[],
        grapheme_data: &[],
    };

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// The cell at `x`.
    #[inline]
    pub fn get(&self, x: usize) -> Cell<'a> {
        Cell {
            raw: self.raw[x],
            grapheme: self.grapheme(x),
            style: self.style[x],
        }
    }

    /// The grapheme codepoints (beyond the first) of the cell at `x`.
    /// This is undefined (but memory safe) unless the raw cell's
    /// content_tag is `codepoint_grapheme`.
    #[inline]
    pub fn grapheme(&self, x: usize) -> &'a [u32] {
        let r = self.grapheme[x];
        let start = r.start as usize;
        self.grapheme_data.get(start..start + r.len as usize).unwrap_or(&[])
    }
}

/// (Zig: `RenderState.Cell`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell<'a> {
    /// Always set, this is the raw copied cell data from page.Cell.
    /// The managed memory (hyperlinks, graphames, etc.) is NOT safe
    /// to access from here. It is duplicated into the other fields if
    /// it exists.
    pub raw: page::Cell,

    /// Grapheme data for the cell. This is undefined unless the
    /// raw cell's content_tag is `codepoint_grapheme`.
    pub grapheme: &'a [u32],

    /// The style data for the cell. This is undefined unless
    /// the style_id is non-default on raw.
    pub style: Style,
}

/// The image protocols that a render state can find image placements
/// for. See `ImagePlacement::protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum ImageProtocol {
    /// The Kitty graphics protocol.
    Kitty,
}

/// Where an image is drawn relative to the text of the terminal.
///
/// Draw in this order: images below backgrounds, then cell
/// backgrounds, then images below text, then text, then images above
/// text. Within a layer, draw images in order of their z-index,
/// lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum ImageLayer {
    /// Below the cell background colors.
    BelowBackground,

    /// Above the cell background colors and below the text.
    BelowText,

    /// Above the text.
    AboveText,
}

/// Returns the layer that an image with the given Kitty graphics
/// z-index is drawn in.
///
/// Kitty graphics uses the z-index for two things: which layer an
/// image is drawn in, and the order of images within that layer. The
/// layer follows these ranges, which match Kitty:
///
///   - Below `i32::MIN / 2` (-1,073,741,824): `BelowBackground`.
///   - From there up to -1: `BelowText`.
///   - Zero and above: `AboveText`.
///
/// Every `ImagePlacement` already has its `layer` set, so you only
/// need this when you get a z-index from somewhere else, such as the
/// terminal's Kitty image storage.
///
/// ```text
/// image_layer_from_z(-1); // BelowText
/// image_layer_from_z(0); // AboveText
/// image_layer_from_z(i32::MIN); // BelowBackground
/// ```
pub fn image_layer_from_z(z: i32) -> ImageLayer {
    if z < i32::MIN / 2 {
        ImageLayer::BelowBackground
    } else if z < 0 {
        ImageLayer::BelowText
    } else {
        ImageLayer::AboveText
    }
}

/// One rectangle of an image to draw. See `image_placement_iterator`.
///
/// To draw it, take the `source_*` rectangle of the image and draw
/// it `width` by `height` pixels in size, at this position:
///
/// ```text
/// px_x = x * cell_width + offset_x
/// px_y = y * cell_height + offset_y
/// ```
///
/// where (px_x, px_y) is relative to the top-left of the viewport.
/// Get the image's pixels from the storage of its `protocol`, using
/// `image_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePlacement {
    /// The protocol the image came from, and the information that
    /// only applies to that protocol.
    pub protocol: ImagePlacementProtocol,

    /// The ID of the image to draw. IDs are only unique within one
    /// protocol, so always look the image up in the storage of
    /// `protocol`.
    pub image_id: u32,

    /// The generation of the image's contents when the placement was
    /// found. It changes whenever the image's pixels change, so it
    /// works as a cache key for textures: if it matches the texture
    /// you already have for this protocol and image ID, you don't
    /// need to read the pixels again.
    pub image_generation: u64,

    /// The layer to draw the image in, and the z-index that orders
    /// images within the layer. Lower z-indexes are drawn first.
    pub layer: ImageLayer,
    pub z: i32,

    /// The top-left cell, where (0, 0) is the top-left cell of the
    /// viewport. `y` is negative for overscan rows above the
    /// viewport, like `viewport_y`.
    ///
    /// A placement that is only partly in the captured rows can start
    /// left of the viewport or outside the captured rows, so `x` can
    /// be negative and `y` can be above the first overscan row. See
    /// "Images Partly Outside the Captured Rows" in the `RenderState`
    /// docs.
    pub x: i32,
    pub y: i32,

    /// The number of columns and rows of cells covered, starting at
    /// `x` and `y`.
    pub cols: u32,
    pub rows: u32,

    /// Where the image starts within the top-left cell, in pixels.
    /// For a virtual run, an image that doesn't fill its cells is
    /// centered, so these can be larger than one cell. For fixed and
    /// relative placements, these are the offsets the program asked
    /// for, limited to less than one cell.
    pub offset_x: u32,
    pub offset_y: u32,

    /// The size to draw the image at, in pixels.
    pub width: u32,
    pub height: u32,

    /// The part of the image to draw, in image pixels.
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

/// (Zig: `RenderState.ImagePlacement.Protocol`, a `union(ImageProtocol)`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePlacementProtocol {
    Kitty(KittyPlacement),
}

impl ImagePlacementProtocol {
    /// The active tag (Zig: `std.meta.activeTag`).
    pub fn tag(&self) -> ImageProtocol {
        match self {
            ImagePlacementProtocol::Kitty(_) => ImageProtocol::Kitty,
        }
    }

    /// The Kitty information (Zig: `protocol.kitty`).
    pub fn kitty(&self) -> &KittyPlacement {
        match self {
            ImagePlacementProtocol::Kitty(k) => k,
        }
    }
}

/// Information that only applies to Kitty graphics placements. See
/// `ImagePlacement::protocol`. (Zig: `Kitty.Placement`, public as
/// `RenderState.KittyPlacement`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacement {
    pub kind: KittyPlacementKind,

    /// The placement ID, or zero if there isn't one. For a
    /// virtual run, this is the ID that the cells ask for. For a
    /// relative or fixed placement, this is the ID the program
    /// gave it.
    pub placement_id: u32,
}

/// (Zig: `Kitty.Placement.Kind`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum KittyPlacementKind {
    /// Part of a virtual placement: one run of placeholder
    /// cells in a single row. Its `rows` is always 1 and its
    /// z-index is always -1.
    Virtual,

    /// A placement that a program positioned relative to
    /// another placement, its parent. The parent is either a
    /// virtual or a fixed placement, possibly through a chain of
    /// other relative placements.
    ///
    /// With a virtual parent, the position is measured from the
    /// top-left corner of the parent's placeholder cells in the
    /// captured rows. That is the leftmost column and the topmost
    /// row that any of those cells use, which matches Kitty. If
    /// none of the parent's cells are in the captured rows, the
    /// relative placement is not reported.
    ///
    /// With a fixed parent, the position is measured from the
    /// parent's cell, which is the cell its `x` and `y` report.
    /// The relative placement is reported when any part of it is
    /// in the captured rows, even if the parent isn't.
    Relative,

    /// A placement at a cell on the screen. It scrolls with the
    /// text, like a line of output would. Its z-index is the one
    /// the program chose, so it can be in any layer.
    ///
    /// It is reported at full size when any part of it is in the
    /// captured rows, so it may need to be clipped when drawn.
    Fixed,
}

/// Iterates over the image placements found by the last update. Get
/// one with `image_placement_iterator`.
///
/// The order is not the order to draw in. Sort by layer and then
/// z-index before drawing. Only placements with something to draw
/// are returned.
///
/// The iterator returns the placeholder runs of each row first, then
/// the placements in `placed`. Build one with
/// `image_placement_iterator` rather than setting the fields yourself.
#[derive(Debug, Clone)]
pub struct ImagePlacementIterator<'a> {
    /// The placeholder runs of each captured row, top to bottom.
    pub rows: &'a [Vec<Placeholder>],

    /// The viewport y position of the first entry in `rows`. It is
    /// negative when there are overscan rows above the viewport.
    pub viewport_y_base: isize,

    /// Placements that already have their final position, returned
    /// after all of the placeholder runs. These are the placements
    /// that are not placeholder runs: fixed and relative placements.
    pub placed: &'a [ImagePlacement],

    /// The current index into `rows`.
    pub y: usize,

    /// The current index into the runs of `rows[y]`, or into
    /// `placed` once every row is done.
    pub i: usize,
}

impl Iterator for ImagePlacementIterator<'_> {
    type Item = ImagePlacement;

    fn next(&mut self) -> Option<ImagePlacement> {
        while self.y < self.rows.len() {
            let runs = &self.rows[self.y];
            while self.i < runs.len() {
                let run = &runs[self.i];
                self.i += 1;
                if run.state != PlaceholderState::Visible {
                    continue;
                }
                let g = run.geometry;
                return Some(ImagePlacement {
                    protocol: ImagePlacementProtocol::Kitty(KittyPlacement {
                        kind: KittyPlacementKind::Virtual,
                        placement_id: run.placement_id,
                    }),
                    image_id: run.image_id,
                    image_generation: g.image_generation,
                    layer: ImageLayer::BelowText,
                    z: -1,
                    x: run.x as i32,
                    y: (self.viewport_y_base + self.y as isize) as i32,
                    cols: run.width as u32,
                    rows: 1,
                    offset_x: g.offset_x,
                    offset_y: g.offset_y,
                    width: g.width,
                    height: g.height,
                    source_x: g.source_x,
                    source_y: g.source_y,
                    source_width: g.source_width,
                    source_height: g.source_height,
                });
            }
            self.y += 1;
            self.i = 0;
        }

        let idx = self.i;
        if idx >= self.placed.len() {
            return None;
        }
        self.i += 1;
        Some(self.placed[idx])
    }
}

/// Dirty state. (Zig: `RenderState.Dirty`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub enum Dirty {
    /// Not dirty at all. Can skip rendering if prior state was
    /// already rendered.
    #[default]
    False,

    /// Some rows changed but not all. None of the global state
    /// changed such as colors.
    Partial,

    /// Global state changed or dimensions changed. All rows should
    /// be redrawn.
    Full,
}

/// (Zig: `RenderState.SelectionCache`.)
#[derive(Debug, Clone, Copy)]
pub struct SelectionCache {
    pub selection: Selection,
    pub tl_pin: Pin,
    pub br_pin: Pin,
}

/// A run of cells within one row sharing one style, pending
/// denormalization into the per-cell data. This is populated by
/// `begin_update` and consumed by `end_update`. This exists so that
/// the (potentially large) denormalization of styles into cells
/// can happen outside of any terminal locks. See `begin_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleRun {
    /// The `row_data` index of the row.
    pub y: CellCountInt,

    /// Start (inclusive) and end (exclusive) x coordinates.
    pub start: CellCountInt,
    pub end: CellCountInt,

    /// The style for this cell range.
    pub style: Style,
}

/// A range of `row_data` indices. `start` is inclusive and `end` is
/// exclusive, so it can be used directly as `range.start..range.end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowDataRange {
    pub start: usize,
    pub end: usize,
}

/// (Zig: `RenderState.StringMap`.)
pub type StringMap = Vec<point::Coordinate>;

/// A set of coordinates representing cells. (Zig: `RenderState.CellSet`,
/// an `AutoArrayHashMapUnmanaged(point.Coordinate, void)`.)
pub type CellSet = IndexSet<point::Coordinate>;

/// Errors from [`RenderState::string`].
#[derive(Debug)]
pub enum StringError {
    /// Zig: `error.WriteFailed` (including invalid codepoints).
    WriteFailed,
}

impl core::fmt::Display for StringError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("WriteFailed")
    }
}

impl std::error::Error for StringError {}

impl RenderState {
    /// Initial state. (Zig: `RenderState.empty`.)
    pub fn empty() -> RenderState {
        RenderState {
            rows: 0,
            cols: 0,
            colors: Colors {
                background: color::Rgb { r: 0, g: 0, b: 0 },
                foreground: color::Rgb { r: 0xff, g: 0xff, b: 0xff },
                cursor: None,
                palette: color::DEFAULT,
            },
            cursor: Cursor {
                active: point::Coordinate { x: 0, y: 0 },
                viewport: None,
                cell: page::Cell::default(),
                style: Style::default(),
                visual_style: cursor::Style::Block,
                password_input: false,
                visible: true,
                blinking: false,
            },
            row_data: RowData::default(),
            dirty: Dirty::False,
            screen: ScreenKey::Primary,
            viewport_pin: None,
            overscan_request: Overscan::default(),
            overscan: Overscan::default(),
            selection_cache: None,
            pending_styles: Vec::new(),
            kitty: KittyData::default(),
        }
    }

    /// Returns an iterator over the image placements that the last update
    /// found. This covers every captured row, including overscan rows.
    ///
    /// See "Which Images Are Included" in the `RenderState` docs for the
    /// kinds of images in the list. Some placements are only partly in
    /// the captured rows and must be clipped when drawn. See "Images
    /// Partly Outside the Captured Rows" in the `RenderState` docs.
    ///
    /// This only reads memory owned by the render state, so it doesn't
    /// need access to the terminal. The next update invalidates the
    /// iterator.
    pub fn image_placement_iterator(&self) -> ImagePlacementIterator<'_> {
        let range = self.row_data_range();
        ImagePlacementIterator {
            rows: &self.row_data.placeholders[range.start..range.end],
            viewport_y_base: self.viewport_y(range.start),
            placed: &self.kitty.placed,
            y: 0,
            i: 0,
        }
    }

    /// Returns a value that changes whenever an update changes the
    /// placements that `image_placement_iterator` returns. If it matches
    /// the value from the last time you read the iterator, the placements
    /// are the same and you don't need to read them again. See "Knowing
    /// When Images Changed" in the `RenderState` docs for an example and
    /// for exactly when the value changes.
    ///
    /// The value belongs to the whole list. To know when the pixels of a
    /// single image changed, use `ImagePlacement::image_generation`.
    ///
    /// Only compare the value for equality. It is always 0 when Kitty
    /// graphics are disabled.
    pub fn image_placements_generation(&self) -> u64 {
        if !build_options::KITTY_GRAPHICS {
            return 0;
        }
        self.kitty.placements_generation
    }

    /// Update the render state to the latest terminal state.
    ///
    /// This is a convenience function that performs a full update in
    /// one call, equivalent to `begin_update` immediately followed by
    /// `end_update`. Callers that hold a lock over the terminal state
    /// should prefer calling the two phases directly so that the lock
    /// is only held for `begin_update`.
    ///
    /// This will reset the terminal dirty state since it is consumed
    /// by this render state update.
    pub fn update(&mut self, t: &mut Terminal) {
        self.begin_update(t);
        self.end_update();
    }

    /// Begin an update of the render state to the latest terminal
    /// state. Every begin must be completed with an `end_update` call
    /// before the render state is read.
    ///
    /// This two-phase structure exists for callers that lock the
    /// terminal state: only this function requires terminal access, so
    /// a caller can hold its lock for this call only and then call
    /// `end_update` after releasing it. `end_update` exclusively reads
    /// and writes memory owned by the render state.
    ///
    /// Work that doesn't require terminal access may be deferred to
    /// `end_update` to keep this call (and therefore lock hold time) as
    /// short as possible. At the time of writing, the deferred work is
    /// the per-cell style denormalization, so between this call and
    /// `end_update` the per-cell `style` data of any updated rows is
    /// stale and must not be read. More work may be deferred in the
    /// future; callers should treat the render state as incomplete
    /// until `end_update` is called.
    ///
    /// This will reset the terminal dirty state since it is consumed
    /// by this render state update.
    pub fn begin_update(&mut self, t: &mut Terminal) {
        let mut screen_ptr = t.screens.active;
        let s: &mut Screen = &mut screen_ptr;
        let viewport_pin = s.pages.get_top_left(point::Tag::Viewport);

        // The overscan rows to capture beyond the viewport. The request
        // decides the layout of row_data, and the actual counts are
        // limited to the rows that exist. With no request, everything
        // below behaves exactly as it does without overscan.
        let above_req: usize = self.overscan_request.above as usize;
        let below_req: usize = self.overscan_request.below as usize;
        let row_data_len: usize = above_req + s.pages.rows as usize + below_req;

        // The first captured row and how many rows above the viewport
        // we actually got.
        let (top_pin, top_above): (Pin, usize) = if above_req == 0 {
            (viewport_pin, 0)
        } else {
            match viewport_pin.up_overflow(above_req) {
                PinOverflow::Offset(p) => (p, above_req),
                PinOverflow::Overflow { end, remaining } => (end, above_req - remaining),
            }
        };

        // How many rows below the viewport we actually get. The page list
        // ends at the last active row, so this is zero whenever the
        // viewport follows the active area. This is computed before the
        // loop below so that a change can force a redraw. When new output
        // appears below a scrolled viewport, it can land in an entry that
        // was never built or that held a different row.
        let below: usize = if below_req == 0 {
            0
        } else {
            let bottom = viewport_pin.down(s.pages.rows as usize - 1).unwrap();
            match bottom.down_overflow(below_req) {
                PinOverflow::Offset(_) => below_req,
                PinOverflow::Overflow { remaining, .. } => below_req - remaining,
            }
        };

        // The index of top.pin in row_data.
        let first: usize = above_req - top_above;

        let redraw = 'redraw: {
            // If our screen key changed, we need to do a full rebuild
            // because our render state is viewport-specific.
            if t.screens.active_key != self.screen {
                break 'redraw true;
            }

            // If our terminal is dirty at all, we do a full rebuild. These
            // dirty values are full-terminal dirty values.
            if t.flags.dirty != TerminalDirty::default() {
                break 'redraw true;
            }

            // If our screen is dirty at all, we do a full rebuild. This is
            // a full screen dirty tracker.
            if s.dirty.selection || s.dirty.hyperlink_hover {
                break 'redraw true;
            }

            // If our dimensions changed, we do a full rebuild.
            if self.rows != s.pages.rows || self.cols != s.pages.cols {
                break 'redraw true;
            }

            // If our row_data layout changed (overscan request changed),
            // we do a full rebuild.
            if self.row_data.len() != row_data_len {
                break 'redraw true;
            }

            // If the captured overscan changed, rows may have entered
            // row_data that were never built. This only happens when the
            // viewport is scrolled, so it's cheap to be safe.
            if self.overscan.above as usize != top_above || self.overscan.below as usize != below {
                break 'redraw true;
            }

            // If our viewport pin changed, we do a full rebuild.
            if let Some(old) = &self.viewport_pin {
                if !old.eql(viewport_pin) {
                    break 'redraw true;
                }
            }

            false
        };

        // Always set our cheap fields, its more expensive to compare
        self.rows = s.pages.rows;
        self.cols = s.pages.cols;
        self.viewport_pin = Some(viewport_pin);
        self.cursor.active = point::Coordinate {
            x: s.cursor.x,
            y: s.cursor.y as u32,
        };
        // SAFETY: the cursor cell pointer is valid while we have
        // exclusive access to the terminal.
        self.cursor.cell = unsafe { *s.cursor.page_cell };
        self.cursor.style = s.cursor.style;
        self.cursor.visual_style = s.cursor.cursor_style;
        self.cursor.password_input = t.flags.password_input;
        self.cursor.visible = t.modes.get(Mode::CursorVisible);
        self.cursor.blinking = t.modes.get(Mode::CursorBlinking);

        // Always reset the cursor viewport position. In the future we can
        // probably cache this by comparing the cursor pin and viewport pin
        // but may not be worth it.
        self.cursor.viewport = None;

        // Colors.
        self.colors.cursor = t.colors.cursor.get();

        // The palette is a relatively large copy (768 bytes at the time
        // of writing) so we only copy it when it could have changed. All
        // palette modifications set a terminal-level dirty flag (see
        // terminal::Dirty::palette), and any terminal-level dirty flag
        // forces a redraw, so checking redraw is sufficient.
        if redraw {
            self.colors.palette = t.colors.palette.current;
        }

        // Background/foreground can be unset initially which would
        // depend on "default" background/foreground. The expected use
        // case of Terminal is that the caller set their own configured
        // defaults on load so this doesn't happen.
        if let (Some(bg), Some(fg)) = (t.colors.background.get(), t.colors.foreground.get()) {
            if t.modes.get(Mode::ReverseColors) {
                self.colors.background = fg;
                self.colors.foreground = bg;
            } else {
                self.colors.background = bg;
                self.colors.foreground = fg;
            }
        }

        // Ensure our row length is exactly our height plus requested
        // overscan, freeing or allocating data as necessary. In
        // most cases we'll have a perfectly matching size.
        if self.row_data.len() != row_data_len {
            self.row_data.resize(row_data_len);
        }

        // If we're redrawing then every row will be rebuilt, superseding
        // any pending style runs from prior updates. Clearing also
        // guarantees pending runs always match the current dimensions
        // (dimension changes force a redraw).
        if redraw {
            self.pending_styles.clear();
        }

        // The viewport start, read before the row data is borrowed below.
        let vp_start = self.viewport_start();
        let rows = self.rows as usize;
        let cursor_pin: Pin = {
            // SAFETY: the cursor pin is tracked and live.
            unsafe { *s.cursor.page_pin.as_ptr() }
        };

        // Break down our row data
        let row_data = &mut self.row_data;
        let row_pins = &mut row_data.pin;
        let row_serials = &mut row_data.serial;

        // Go through and setup our rows. We iterate page chunks rather
        // than individual rows so that per-page work (dirty flags, cursor
        // detection, memory pointers) is hoisted out of the row loop. This
        // makes the common case of a clean (or mostly clean) frame very
        // cheap: a contiguous scan of row dirty flags.
        let mut builder = RowBuilder {
            cols: self.cols as usize,
            raws: &mut row_data.raw,
            cells: &mut row_data.cells,
            sels: &mut row_data.selection,
            highlights: &mut row_data.highlights,
            placeholders: &mut row_data.placeholders,
            kitty_stale: &mut self.kitty.stale,
            dirties: &mut row_data.dirty,
            pending_styles: &mut self.pending_styles,
            applied_styles: &mut row_data.applied_styles,
        };
        let row_data_end: usize = first + top_above + rows + below;
        let mut y: usize = first;
        let mut any_dirty = false;
        let mut page_it = top_pin.page_iterator(Direction::RightDown, None);
        while y < row_data_end {
            let Some(chunk) = page_it.next() else { break };
            let node: NodePtr = chunk.node;
            // SAFETY: the chunk node is live while we have exclusive
            // access to the terminal.
            let node_ref: &mut Node = unsafe { &mut *node.as_ptr() };
            let node_serial = node_ref.serial;
            let p: &mut Page = node_ref.page();

            // The number of rows we consume from this chunk. The chunk
            // may extend beyond what we capture (the viewport is always
            // exactly `rows` tall, plus any overscan) so we clamp.
            let take: usize = ((chunk.end - chunk.start) as usize).min(row_data_end - y);

            // Find our cursor if we haven't found it yet. We do this even
            // if rows are not dirty because the cursor is unrelated. We
            // can check the chunk bounds once rather than every row.
            if self.cursor.viewport.is_none() && node == cursor_pin.node {
                'cursor: {
                    let cy = cursor_pin.y as usize;
                    let cs = chunk.start as usize;
                    if cy < cs || cy >= cs + take {
                        break 'cursor;
                    }

                    // The cursor may be in an overscan row, in which case it
                    // is not visible in the viewport.
                    let idx = y + (cy - cs);
                    if idx < vp_start || idx >= vp_start + rows {
                        break 'cursor;
                    }
                    self.cursor.viewport = Some(CursorViewport {
                        y: (idx - vp_start) as CellCountInt,
                        x: s.cursor.x,

                        // Future: we should use our own state here to look this
                        // up rather than calling this.
                        wide_tail: if s.cursor.x > 0 {
                            // SAFETY: valid cursor cell pointer.
                            unsafe { (*s.cursor_cell_left(1)).wide() == Wide::Wide }
                        } else {
                            false
                        },
                    });
                }
            }

            // The page-level dirty flag applies to every row in the chunk.
            // We consume (clear) it now; each node appears at most once in
            // this iteration and we're the only consumer of dirty state.
            let page_dirty = p.dirty;
            if page_dirty {
                p.dirty = false;
            }

            // Get our contiguous rows for this chunk.
            // SAFETY: rows [chunk.start, chunk.start + take) are within the
            // page's row array.
            let page_rows: &mut [page::Row] = unsafe {
                core::slice::from_raw_parts_mut(p.rows.ptr(p.memory).add(chunk.start as usize), take)
            };
            debug_assert!(p.size.cols == self.cols);

            // Store our pins and their node generations. We have to store
            // these even for rows that aren't dirty because dirty is only a
            // renderer optimization; it doesn't apply to memory movement.
            // This lets us remap any cell pins back to an exact entry in our
            // RenderState and validate them later without dereferencing a
            // potentially stale node.
            //
            // We can skip the writes when the pins and serials are unchanged:
            // if we're not redrawing, every value was stored by a prior update
            // (row count changes force a redraw). Within a single update a
            // node appears at most once and its stored pins have consecutive
            // y values, so if the first and last entries of this chunk's range
            // already match then every entry in between matches too.
            let last = y + take - 1;
            if redraw
                || row_pins[y].node != node
                || row_pins[y].y != chunk.start
                || row_serials[y] != node_serial
                || row_pins[last].node != node
                || row_pins[last].y as usize != chunk.start as usize + take - 1
                || row_serials[last] != node_serial
            {
                for (i, (pin, serial)) in row_pins[y..y + take]
                    .iter_mut()
                    .zip(row_serials[y..y + take].iter_mut())
                    .enumerate()
                {
                    *pin = Pin {
                        node,
                        y: chunk.start + i as CellCountInt,
                        x: 0,
                        garbage: false,
                    };
                    *serial = node_serial;
                }
            }

            if !redraw && !page_dirty {
                // Only dirty rows (usually none) need a rebuild. Scan the
                // dirty flags a group at a time; the dirty bit is directly
                // testable on the packed row representation.
                let mut i: usize = 0;
                while take - i >= RowDirtyMask::GROUP_LEN {
                    if RowDirtyMask::r#match(page_rows, i) {
                        i += RowDirtyMask::GROUP_LEN;
                        continue;
                    }

                    for j in i..i + RowDirtyMask::GROUP_LEN {
                        let page_row = &mut page_rows[j];
                        if !page_row.dirty() {
                            continue;
                        }
                        page_row.set_dirty(false);
                        any_dirty = true;
                        builder.row(p, page_row, y + j);
                    }
                    i += RowDirtyMask::GROUP_LEN;
                }
                while i < take {
                    let page_row = &mut page_rows[i];
                    if page_row.dirty() {
                        page_row.set_dirty(false);
                        any_dirty = true;
                        builder.row(p, page_row, y + i);
                    }
                    i += 1;
                }
            } else {
                // Rebuild every row in the chunk.
                any_dirty = true;
                for (i, page_row) in page_rows.iter_mut().enumerate() {
                    page_row.set_dirty(false);
                    builder.row(p, page_row, y + i);
                }
            }

            y += take;
        }
        debug_assert!(y == row_data_end);

        self.overscan = Overscan {
            above: top_above as CellCountInt,
            below: below as CellCountInt,
        };

        // Work out what the Kitty placeholder runs draw. This must come
        // after setting the overscan above, because it reads the
        // captured rows.
        if build_options::KITTY_GRAPHICS {
            kitty::update(self, t, redraw, any_dirty);
        }

        // Reborrow the active screen: the Kitty update read it through `t`.
        let s: &mut Screen = &mut screen_ptr;

        // If our screen has a selection, then mark the rows with the
        // selection. We do this outside of the loop above because its unlikely
        // a selection exists and because the way our selections are structured
        // today is very inefficient.
        //
        // NOTE: To improve the performance of the block below, we'll need
        // to rethink how we model selections in general.
        //
        // There are performance improvements that can be made here, though.
        // For example, `containedRow` recalculates a bunch of information
        // we can cache.
        if let Some(sel) = s.selection {
            self.update_selection(s, &sel, redraw, &mut any_dirty);
        }

        // Handle dirty state.
        if redraw {
            // Fully redraw resets some other state.
            self.screen = t.screens.active_key;
            self.dirty = Dirty::Full;

            // Note: we don't clear any row_data here because our rebuild
            // above did this.
        } else if any_dirty && self.dirty == Dirty::False {
            self.dirty = Dirty::Partial;
        }

        // Clear our dirty flags
        t.flags.dirty = TerminalDirty::default();
        s.dirty = Default::default();
    }

    /// The selection part of `begin_update`.
    #[cold]
    fn update_selection(&mut self, s: &Screen, sel: &Selection, redraw: bool, any_dirty: &mut bool) {
        // Populate our selection cache to avoid some expensive
        // recalculation.
        let cache: SelectionCache = 'cache: {
            if let Some(c) = &self.selection_cache {
                'cache_check: {
                    // If we're redrawing, we recalculate the cache just to
                    // be safe.
                    if redraw {
                        break 'cache_check;
                    }

                    // If our selection isn't equal, we aren't cached!
                    if !c.selection.eql(*sel) {
                        break 'cache_check;
                    }

                    // If we have no dirty rows, we can not recalculate.
                    if !*any_dirty {
                        return;
                    }

                    // We have dirty rows, we can utilize the cache.
                    break 'cache *c;
                }
            }

            // Create a new cache
            let tl_pin = sel.top_left(s);
            let br_pin = sel.bottom_right(s);
            let c = SelectionCache {
                selection: Selection::init(tl_pin, br_pin, sel.rectangle),
                tl_pin,
                br_pin,
            };
            self.selection_cache = Some(c);
            c
        };

        // Grab the inefficient data we need from the selection. At
        // least we can cache it.
        let tl = s.pages.point_from_pin(point::Tag::Screen, cache.tl_pin).unwrap().coord();

        // Not cached since the cell contents may change.
        // SAFETY: the cached pins are valid (selection pins are tracked).
        let br_pin: Pin =
            if !sel.rectangle && unsafe { (*cache.br_pin.row_and_cell().cell).wide() } == Wide::SpacerHead {
                cache.br_pin.right_wrap(1).unwrap_or(cache.br_pin)
            } else {
                cache.br_pin
            };
        let br = s.pages.point_from_pin(point::Tag::Screen, br_pin).unwrap().coord();

        // We need to determine if our selection is within the viewport.
        // The viewport is generally very small so the efficient way to
        // do this is to traverse the viewport pages and check for the
        // matching selection pages. Unused entries are skipped.
        let range = self.row_data_range();
        let row_data = &mut self.row_data;
        for i in range.start..range.end {
            let pin = row_data.pin[i];
            let p = s.pages.point_from_pin(point::Tag::Screen, pin).unwrap().coord();
            let Some(row_sel) = sel.contained_row_cached(s, cache.tl_pin, br_pin, pin, tl, br, p) else {
                if row_data.selection[i].is_some() {
                    row_data.selection[i] = None;
                    row_data.dirty[i] = true;
                    *any_dirty = true;
                }
                continue;
            };
            let start = row_sel.start();
            let end = row_sel.end();
            debug_assert!(start.node == end.node);
            debug_assert!(start.x <= end.x);
            debug_assert!(start.y == end.y);

            // SAFETY: valid pin.
            let start_x = if start.x > 0 && unsafe { (*start.row_and_cell().cell).wide() } == Wide::SpacerTail {
                start.x - 1
            } else {
                start.x
            };

            row_data.selection[i] = Some([start_x, end.x]);
        }
    }

    /// Complete a prior `begin_update` call by performing any deferred
    /// work. At the time of writing, this denormalizes the pending
    /// style runs into the per-cell style data.
    ///
    /// This only reads and writes memory owned by the render state, so
    /// it is safe to call while the terminal is being modified (no
    /// terminal lock is required).
    pub fn end_update(&mut self) {
        // Common case: no styled rows were rebuilt.
        if self.pending_styles.is_empty() {
            return;
        }

        let row_cells = &mut self.row_data.cells;
        let row_applied = &mut self.row_data.applied_styles;

        // Process the pending runs one row segment at a time. All the
        // runs for a row are appended contiguously by a single
        // begin_update, so a segment boundary is simply a change in y.
        let runs = &self.pending_styles[..];
        let mut i: usize = 0;
        while i < runs.len() {
            let y = runs[i].y as usize;
            let mut j = i + 1;
            while j < runs.len() && runs[j].y as usize == y {
                j += 1;
            }
            let segment = &runs[i..j];
            i = j;

            // Defensive: the row data may have changed shape if the
            // caller violated ordering (e.g. an error path skipped an
            // end_update between updates). Any update that changes
            // dimensions clears the pending list (redraw), so this
            // should never actually trigger, but the cost is trivial.
            if y >= row_cells.len() {
                continue;
            }

            // If the segment matches the runs already denormalized
            // into this row's cell data then the per-cell styles are
            // already correct and the (comparatively large) fill can
            // be skipped entirely. This is the common case: rebuilt
            // rows usually keep their styling (only the text
            // changed), and full redraws of an unchanged screen keep
            // both.
            let applied = &mut row_applied[y];
            if runs_eql(applied, segment) {
                continue;
            }

            for run in segment {
                let styles = &mut row_cells[run.y as usize].style;
                let end = (run.end as usize).min(styles.len());
                let start = (run.start as usize).min(end);

                fill_styles(&mut styles[start..end], run.style);
            }

            // Record what we applied so the next rebuild of this row
            // can skip the fill. begin_update reserved the capacity
            // (we cannot allocate here); if it doesn't fit (e.g.
            // segments merged across multiple begins without an end)
            // leave the cache empty, which never matches and simply
            // means the next rebuild applies its runs.
            applied.clear();
            if applied.capacity() >= segment.len() {
                applied.extend_from_slice(segment);
            }
        }
        self.pending_styles.clear();
    }

    /// Mark all render-state data as consumed by the renderer.
    ///
    /// This clears both the global dirty state and every per-row dirty flag.
    /// Callers that only consume part of a frame should clear the two layers
    /// individually instead.
    pub fn clean(&mut self) {
        self.dirty = Dirty::False;
        self.row_data.dirty.fill(false);
    }

    /// Returns the `row_data` index of the top row of the viewport.
    ///
    /// This is `overscan_request.above`, so it is zero without overscan
    /// and does not change from one update to the next. The viewport is
    /// the `rows` entries starting here.
    pub fn viewport_start(&self) -> usize {
        self.overscan_request.above as usize
    }

    /// Converts a `row_data` index into a y position relative to the top
    /// of the viewport. Rows above the viewport are negative, and rows
    /// below it are `rows` or greater. For example, with one row of
    /// overscan above, index 0 is y = -1 and index 1 is y = 0.
    pub fn viewport_y(&self, index: usize) -> isize {
        index as isize - self.viewport_start() as isize
    }

    /// Returns the range of `row_data` that holds rows from the last
    /// update: the captured overscan rows above, the viewport, and the
    /// captured overscan rows below.
    ///
    /// Entries outside this range are unused. They contain leftover or
    /// uninitialized data and must not be read. Without overscan, this
    /// is always `0..rows`.
    pub fn row_data_range(&self) -> RowDataRange {
        let vp = self.viewport_start();
        RowDataRange {
            start: vp - self.overscan.above as usize,
            end: vp + self.rows as usize + self.overscan.below as usize,
        }
    }

    /// Update the highlights in the render state from the given flattened
    /// highlights. Because this uses flattened highlights, it does not require
    /// reading from the terminal state so it should be done outside of
    /// any critical sections.
    ///
    /// This will not clear any previous highlights, so the caller must
    /// manually clear them if desired.
    pub fn update_highlights_flattened(&mut self, tag: u8, hls: &[highlight::Flattened]) {
        // Fast path, we have no highlights!
        if hls.is_empty() {
            return;
        }

        // This is, admittedly, horrendous. This is some low hanging fruit
        // to optimize. In my defense, screens are usually small, the number
        // of highlights is usually small, and this only happens on the
        // viewport outside of a locked area. Still, I'd love to see this
        // improved someday.

        // We need to track whether any row had a match so we can mark
        // the dirty state.
        let mut any_dirty = false;

        let cols = self.cols;
        let range = self.row_data_range();
        let row_data = &mut self.row_data;
        for r in range.start..range.end {
            let row_pin = row_data.pin[r];
            let row_serial = row_data.serial[r];
            for hl in hls {
                let chunks = &hl.chunks;
                let n = chunks.len();
                for (i, chunk) in chunks.iter().enumerate() {
                    // If this node generation doesn't match or we're not
                    // within the row range, skip it. Both serials are copied
                    // values, so this never dereferences a node outside the
                    // terminal lock.
                    if chunk.node != row_pin.node
                        || chunk.serial != row_serial
                        || row_pin.y < chunk.start
                        || row_pin.y >= chunk.end
                    {
                        continue;
                    }

                    // We're a match!
                    row_data.highlights[r].push(Highlight {
                        tag,
                        range: [
                            if i == 0 && row_pin.y == chunks[0].start {
                                hl.top_x
                            } else {
                                0
                            },
                            if i == n - 1 && row_pin.y == chunks[n - 1].end - 1 {
                                hl.bot_x
                            } else {
                                cols - 1
                            },
                        ],
                    });

                    row_data.dirty[r] = true;
                    any_dirty = true;
                }
            }
        }

        // Mark our dirty state.
        if any_dirty && self.dirty == Dirty::False {
            self.dirty = Dirty::Partial;
        }
    }

    /// Convert the current render state contents to a UTF-8 encoded
    /// string written to the given writer. This will unwrap all the wrapped
    /// rows. This is useful for a minimal viewport search.
    ///
    /// This currently writes empty cell contents as \x00 and writes all
    /// blank lines. This is fine for our current usage (link search) but
    /// we can adjust this later.
    ///
    /// Only viewport rows are included, never overscan rows. The `y`
    /// values in `map` are viewport rows.
    ///
    /// NOTE: There is a limitation in that wrapped lines before/after
    /// the top/bottom line of the viewport are not included, since
    /// the render state cuts them off.
    pub fn string<W: std::io::Write + ?Sized>(
        &self,
        writer: &mut W,
        mut map: Option<&mut StringMap>,
    ) -> Result<(), StringError> {
        fn put<W: std::io::Write + ?Sized>(writer: &mut W, cp: u32) -> Result<usize, StringError> {
            let c = char::from_u32(cp).ok_or(StringError::WriteFailed)?;
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            writer.write_all(s.as_bytes()).map_err(|_| StringError::WriteFailed)?;
            Ok(s.len())
        }

        // This only covers the viewport, never overscan rows.
        let vp_start = self.viewport_start();
        let rows = self.rows as usize;
        let row_rows = &self.row_data.raw[vp_start..vp_start + rows];
        let row_cells = &self.row_data.cells[vp_start..vp_start + rows];

        for (y, (row, cells)) in row_rows.iter().zip(row_cells.iter()).enumerate() {
            let cells_slice = cells.slice();
            for x in 0..cells_slice.len() {
                let cell = cells_slice.raw[x];
                let mut len = put(writer, cell.codepoint())?;
                if cell.has_grapheme() {
                    for &cp in cells_slice.grapheme(x) {
                        len += put(writer, cp)?;
                    }
                }

                if let Some(m) = map.as_deref_mut() {
                    m.extend(core::iter::repeat_n(
                        point::Coordinate {
                            x: x as CellCountInt,
                            y: y as u32,
                        },
                        len,
                    ));
                }
            }

            if !row.wrap() {
                writer.write_all(b"\n").map_err(|_| StringError::WriteFailed)?;
                if let Some(m) = map.as_deref_mut() {
                    m.push(point::Coordinate {
                        x: cells_slice.len() as CellCountInt,
                        y: y as u32,
                    });
                }
            }
        }

        Ok(())
    }

    /// Returns a map of the cells that match to an OSC8 hyperlink over the
    /// given point in the render state.
    ///
    /// IMPORTANT: The terminal must not have updated since the last call to
    /// `update`. If there is any chance the terminal has updated, the caller
    /// must first call `update` again to refresh the render state.
    ///
    /// For example, you may want to hold a lock for the duration of the
    /// update and hyperlink lookup to ensure no updates happen in between.
    ///
    /// Only viewport rows are searched, never overscan rows. Both the
    /// given point and the returned cells use viewport coordinates.
    pub fn link_cells(&self, viewport_point: point::Coordinate) -> CellSet {
        let mut result = CellSet::new();

        // Our viewport point is sent in by the caller and can't be trusted.
        // If it is outside the valid area then just return empty because
        // we can't possibly have a link there.
        if viewport_point.x >= self.cols || viewport_point.y >= self.rows as u32 {
            return result;
        }

        // This only covers the viewport, never overscan rows.
        let vp_start = self.viewport_start();
        let rows = self.rows as usize;
        let row_pins = &self.row_data.pin[vp_start..vp_start + rows];
        let row_cells = &self.row_data.cells[vp_start..vp_start + rows];

        // Grab our link ID
        let link_pin: Pin = row_pins[viewport_point.y as usize];
        // SAFETY: the terminal has not changed since the last update
        // (function contract), so the pin's node is live.
        let link_page: &Page = unsafe { link_pin.node_mut().page() };
        let link = {
            let rac = link_page.get_row_and_cell(viewport_point.x as usize, link_pin.y as usize);
            // SAFETY: valid cell pointer into the page.
            let cell = unsafe { &*rac.cell };

            // The likely scenario is that our mouse isn't even over a link.
            if !cell.hyperlink() {
                return result;
            }

            let Some(link_id) = link_page.lookup_hyperlink(cell) else {
                return result;
            };
            // SAFETY: the id is a live entry of the page's set.
            unsafe { *link_page.hyperlink_set.get(link_page.memory, link_id) }
        };

        for (y, (pin, cells)) in row_pins.iter().zip(row_cells.iter()).enumerate() {
            for (x, cell) in cells.raw.iter().enumerate() {
                if !cell.hyperlink() {
                    continue;
                }

                // SAFETY: see above.
                let other_page: &Page = unsafe { pin.node_mut().page() };
                let other = {
                    let rac = other_page.get_row_and_cell(x, pin.y as usize);
                    // SAFETY: valid cell pointer into the page.
                    let Some(link_id) = other_page.lookup_hyperlink(unsafe { &*rac.cell }) else {
                        continue;
                    };
                    // SAFETY: the id is a live entry of the page's set.
                    unsafe { *other_page.hyperlink_set.get(other_page.memory, link_id) }
                };

                if link.eql(link_page.memory, &other, other_page.memory) {
                    result.insert(point::Coordinate {
                        y: y as u32,
                        x: x as CellCountInt,
                    });
                }
            }
        }

        result
    }
}

/// Fill a slice of styles with one value.
///
/// The Zig version manually vectorizes this because `@memset` with a
/// struct value lowers to a per-element field copy there. `<[T]>::fill`
/// with a `Copy` value is already vectorized by LLVM in Rust.
#[inline]
fn fill_styles(dst: &mut [Style], value: Style) {
    use std::mem::MaybeUninit;

    // Each element is written as two overlapping 16-byte vector
    // stores held in registers, which requires 16 <= size <= 32.
    // (A plain `fill` reloads the value from memory for every element.)
    const ELEM_SIZE: usize = std::mem::size_of::<Style>();
    const _: () = assert!(ELEM_SIZE >= 16 && ELEM_SIZE <= 32);

    // The chunks are MaybeUninit so that padding bytes may be copied; a
    // MaybeUninit of a 16-byte vector type stays in a vector register.
    #[cfg(target_arch = "x86_64")]
    type V = MaybeUninit<std::arch::x86_64::__m128i>;
    #[cfg(not(target_arch = "x86_64"))]
    type V = MaybeUninit<u128>;
    let src = (&value as *const Style).cast::<u8>();
    // SAFETY: both 16-byte reads are within `value` (16 <= ELEM_SIZE).
    let (lo, hi) = unsafe {
        (
            std::ptr::read_unaligned(src.cast::<V>()),
            std::ptr::read_unaligned(src.add(ELEM_SIZE - 16).cast::<V>()),
        )
    };

    let base = dst.as_mut_ptr().cast::<u8>();
    for i in 0..dst.len() {
        // SAFETY: both stores are within element i of `dst`, and together
        // they write all of its bytes, exactly as `value` holds them.
        unsafe {
            let p = base.add(i * ELEM_SIZE);
            std::ptr::write_unaligned(p.cast::<V>(), lo);
            std::ptr::write_unaligned(p.add(ELEM_SIZE - 16).cast::<V>(), hi);
        }
    }
}

/// Returns true if the two style run lists denormalize to
/// identical per-cell style data. This is a semantic comparison
/// (styles are compared field-wise, never by bytes, since padding
/// is undefined).
fn runs_eql(a: &[StyleRun], b: &[StyleRun]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (ar, br) in a.iter().zip(b.iter()) {
        if ar.start != br.start || ar.end != br.end || !ar.style.eql(&br.style) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Kitty graphics
// ---------------------------------------------------------------------------

/// The Kitty image storage key of a placement (Zig:
/// `kitty_gfx.ImageStorage.PlacementKey`).
pub use crate::terminal::kitty::graphics_storage::{PlacementId, PlacementKey, PlacementTag};

/// A run of neighboring Kitty graphics placeholder cells in one row
/// that show one piece of one image. (Zig: `Kitty.Placeholder`, public
/// as `RenderState.Placeholder`.)
///
/// The fields up to `col` come from the cells and are set when the
/// row is rebuilt. The rest are filled in by `begin_update`, which
/// looks up the image and works out what to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder {
    /// The column of the first cell and the number of cells.
    pub x: CellCountInt,
    pub width: CellCountInt,

    /// The image and placement that the cells ask for. The cells
    /// encode these in their foreground and underline colors. A
    /// placement ID of zero means the image's virtual placement,
    /// whichever it is.
    pub image_id: u32,
    pub placement_id: u32,

    /// The piece of the image that the first cell shows. The image
    /// is divided into a grid of cells, and this is the row and
    /// column in that grid. Each following cell in the run shows the
    /// next column.
    pub row: u32,
    pub col: u32,

    /// Whether `begin_update` has looked at this run yet, and if so,
    /// whether it draws anything. `geometry` is only set when this
    /// is `Visible`.
    pub state: PlaceholderState,

    /// The virtual placement this run shows, or None if there is
    /// none. Not set while `state` is `Stale`.
    pub target: Option<PlacementKey>,

    /// What to draw. Only set when `state` is `Visible`.
    pub geometry: PlaceholderGeometry,
}

/// (Zig: `Kitty.Placeholder.State`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PlaceholderState {
    /// The run is new and hasn't been looked at yet.
    #[default]
    Stale,

    /// There is nothing to draw, for example because the image
    /// doesn't exist or hasn't been fully received.
    Hidden,

    /// The run draws part of an image, described by `geometry`.
    Visible,
}

/// What a visible run draws. The fields have the same meaning
/// as the fields of the same name in `ImagePlacement`.
/// (Zig: `Kitty.Placeholder.Geometry`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaceholderGeometry {
    pub image_generation: u64,
    pub offset_x: u32,
    pub offset_y: u32,
    pub width: u32,
    pub height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

/// The Kitty graphics state of a render state (`RenderState::kitty`).
/// (Zig: `Kitty.Data`.)
#[derive(Debug, Default)]
pub struct KittyData {
    /// The image storage generation, the terminal size in pixels,
    /// and the grid size when `roots` was last built. If any of
    /// these change, `roots` is built again and every run is looked
    /// up again. Together the pixel and grid sizes give the cell
    /// size.
    pub generation: u64,
    pub width_px: u32,
    pub height_px: u32,
    pub rows: CellCountInt,
    pub cols: CellCountInt,

    /// The image storage `dirty` counter when `placed` was last
    /// built. The storage changes this counter whenever placements
    /// may have moved, for example when a program scrolls part of
    /// the screen. If it changed but the generation didn't, `placed`
    /// is built again from the same `roots`.
    pub dirty: u64,

    /// True if the placeholder runs changed since the last update.
    /// The row builder sets it in two cases:
    ///
    ///   - A row was rebuilt with runs. The new runs are `Stale` and
    ///     must be looked up.
    ///   - A row that had runs was rebuilt, and its old runs were
    ///     dropped. There is nothing to look up, but the iterator
    ///     no longer returns those runs.
    ///
    /// Updates where this is false and nothing else changed can skip
    /// the slow path.
    pub stale: bool,

    /// The placements that aren't placeholder runs, with everything
    /// except their position worked out. Built again only when
    /// `generation`, the pixel size, or the grid size change. A
    /// redraw alone, such as a viewport scroll, keeps it.
    pub roots: Vec<KittyRoot>,

    /// The number of `roots` positioned from placeholder cells (a
    /// `Virtual` anchor). These move when cells change, so an
    /// update with rebuilt rows must position them again. When this
    /// is zero, rebuilt rows can't move anything in `placed`.
    pub virtual_anchors: u32,

    /// The captured rows grouped by the page they are on, top to
    /// bottom. This lets `row_index_of_pin` find the row of a pin
    /// without walking the page list. It is built from the captured
    /// row pins each time `placed` is built, and only when some root
    /// has a `Pin` anchor.
    pub chunks: Vec<KittyChunk>,

    /// The placements positioned by the last update that aren't
    /// placeholder runs. The image placement iterator returns these
    /// after the runs.
    pub placed: Vec<ImagePlacement>,

    /// The previous contents of `placed`, so that an update can tell
    /// whether `placed` changed. `place_roots` swaps the two lists
    /// instead of copying, so both keep their memory and a steady
    /// stream of updates doesn't allocate.
    pub placed_prev: Vec<ImagePlacement>,

    /// The value returned by `RenderState::image_placements_generation`.
    /// `update_slow` adds one to it, wrapping around, when the
    /// iterator's output may have changed.
    pub placements_generation: u64,
}

/// A placement that isn't a placeholder run, with everything except
/// its position worked out. Its position is its anchor's position
/// plus the offsets. For example, a relative placement 2 columns
/// right of a virtual placement whose cells start at column 5 is
/// drawn at column 7. (Zig: `Kitty.Root`.)
#[derive(Debug, Clone, Copy)]
pub struct KittyRoot {
    /// What the placement is positioned from.
    pub anchor: KittyAnchor,

    /// The offset in cells from the anchor to the placement. For a
    /// relative placement, this adds up the offsets of every
    /// placement in its chain of parents. Zero for fixed placements.
    pub horizontal_offset: i32,
    pub vertical_offset: i32,

    /// The placement, with `x` and `y` not yet set.
    pub placement: ImagePlacement,
}

/// What a root is positioned from. (Zig: `Kitty.Anchor`.)
#[derive(Debug, Clone, Copy)]
pub enum KittyAnchor {
    /// The placeholder cells of a virtual placement. The position is
    /// the smallest column and the smallest row of its runs in the
    /// captured rows.
    Virtual(PlacementKey),

    /// The cell of a fixed placement, as the pin that the image
    /// storage keeps for it. A fixed placement uses its own pin. A
    /// relative placement uses the pin of the fixed placement at the
    /// start of its chain. The page list moves this pin as text
    /// moves, so it is always current.
    ///
    /// The pointer is owned by the image storage. It is only read
    /// during `begin_update`, and only while the storage generation
    /// matches `KittyData::generation`. Removing a placement always
    /// changes the generation, so the pointer is never read after the
    /// pin is freed.
    Pin(*const Pin),
}

/// Neighboring captured rows that are on the same page. See
/// `KittyData::chunks`. (Zig: `Kitty.Chunk`.)
///
/// For example, if the captured rows are rows 98 and 99 of page A
/// followed by rows 0 to 2 of page B, there are two chunks:
/// `{ A, start 98, index 0 }` and `{ B, start 0, index 2 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyChunk {
    /// The page and the row within it of the chunk's first row.
    pub node: NodePtr,
    pub start: CellCountInt,

    /// The position of the chunk's first row among the captured
    /// rows, where 0 is the first captured row. This is not a
    /// `row_data` index. Add `row_data_range().start` to get one.
    pub index: isize,
}

/// The Kitty graphics logic of the render state. See "Images" in the
/// `RenderState` docs for what it produces. (Zig: `Kitty`.)
///
/// There are two kinds of work:
///
///   - Placeholder runs. Each row keeps its own runs
///     (`RowData::placeholders`), and a run is looked up once, when its
///     row is rebuilt.
///   - Every other placement. These are kept in two lists. `roots` holds
///     each placement with everything worked out except its position.
///     `placed` holds the positioned copies that the iterator returns.
///
/// Building `roots` means walking every placement in the image storage,
/// so it only happens when the storage or the cell size changes. Turning
/// `roots` into `placed` is cheap, so it happens whenever a placement may
/// have moved. For example, with one image at a fixed cell:
///
///   1. The program places the image. The storage generation changes, so
///      the next update builds `roots` with one entry, then positions it
///      into `placed`.
///   2. The user scrolls the viewport up by one row. The update positions
///      the same root again, and `placed` now has it one row lower. The
///      storage is not read.
///   3. The program prints text on an unrelated row. Fixed images don't
///      move when cells change, so the update keeps `placed` as it is.
///
/// Each update also works out whether the iterator's output changed, for
/// `RenderState::image_placements_generation`. The runs are easy, because
/// a run only changes when its row is rebuilt or when it is looked up
/// again. For `placed`, the update keeps the previous list in
/// `placed_prev` and compares the two. In the example, step 1 and step 2
/// change the value, and step 3 doesn't.
mod kitty {
    use super::*;

    /// Work out what each placeholder run in the captured rows draws, and
    /// position every placement that isn't a placeholder run. This is
    /// part of `begin_update` and needs access to the terminal. `redraw`
    /// and `any_dirty` say whether every row or some rows were rebuilt.
    ///
    /// Most updates return after a few comparisons. The work is done by
    /// `update_slow` only when something may have changed:
    ///
    ///   - The storage or the cell size changed. Build `roots` again and
    ///     look up every run.
    ///   - Rebuilt rows have new runs. Look up only those runs.
    ///   - Placements may have moved, because the viewport moved, the
    ///     storage `dirty` counter changed, or rebuilt rows can move a
    ///     placement positioned from placeholder cells. Build `placed`
    ///     again from `roots`.
    #[inline]
    pub(super) fn update(state: &mut RenderState, t: &Terminal, redraw: bool, any_dirty: bool) {
        let storage: &ImageStorage = &t.screens.active.kitty_images;

        // `roots` and what a run draws depend only on the image storage
        // and the cell size. Switching to the other screen is caught
        // too, because no two storages share a generation.
        //
        // This runs on every update, so the common case (no images and
        // nothing changed) must stay a few comparisons. A redraw alone,
        // such as a viewport scroll, must not build `roots` again. A
        // change in rows or columns always causes a redraw, so those are
        // only compared on redraws.
        let roots_stale = state.kitty.generation != storage.generation
            || state.kitty.width_px != t.width_px
            || state.kitty.height_px != t.height_px
            || (redraw && (state.kitty.rows != state.rows || state.kitty.cols != state.cols));
        if !roots_stale && !state.kitty.stale {
            // No runs to look up. Placements only move when the viewport
            // moves, when the storage says that they moved, or (for
            // placements positioned from placeholder cells) when rows
            // are rebuilt.
            if state.kitty.roots.is_empty() {
                return;
            }
            if !redraw && state.kitty.dirty == storage.dirty && !(any_dirty && state.kitty.virtual_anchors > 0) {
                return;
            }
        }

        update_slow(state, t, roots_stale);
    }

    /// The part of `update` that does the work. It is kept out of line
    /// so that the common case in `update` stays small. `roots_stale` is
    /// true when `roots` must be built again.
    #[inline(never)]
    fn update_slow(state: &mut RenderState, t: &Terminal, roots_stale: bool) {
        let storage: &ImageStorage = &t.screens.active.kitty_images;
        if roots_stale {
            state.kitty.generation = storage.generation;
            state.kitty.width_px = t.width_px;
            state.kitty.height_px = t.height_px;
            state.kitty.rows = state.rows;
            state.kitty.cols = state.cols;
            rebuild_roots(state, storage, &TerminalInfo::from_terminal(t));
        }

        // Whether the runs that the iterator returns changed. They did if
        // the row builder added or dropped runs, or if the loop below
        // looks up any run again, since its result may differ.
        let mut runs_changed = state.kitty.stale;
        if roots_stale || state.kitty.stale {
            state.kitty.stale = false;
            let cell_width: u32 = t.width_px / state.cols as u32;
            let cell_height: u32 = t.height_px / state.rows as u32;
            let range = state.row_data_range();
            let row_data = &mut state.row_data;
            for i in range.start..range.end {
                let pin = row_data.pin[i];
                for run in row_data.placeholders[i].iter_mut() {
                    if !roots_stale && run.state != PlaceholderState::Stale {
                        continue;
                    }
                    runs_changed = true;
                    resolve_placeholder(storage, pin, run, cell_width, cell_height);
                }
            }
        }

        place_roots(state);
        state.kitty.dirty = storage.dirty;

        // Tell consumers to read the iterator again if anything it
        // returns may be different. Rebuilding `roots` alone doesn't
        // count: if no run was looked up and `placed` is the same, the
        // output is the same.
        if runs_changed || !placed_eql(&state.kitty.placed, &state.kitty.placed_prev) {
            state.kitty.placements_generation = state.kitty.placements_generation.wrapping_add(1);
        }
    }

    /// Returns true if two lists hold the same placements in the same
    /// order.
    pub(super) fn placed_eql(a: &[ImagePlacement], b: &[ImagePlacement]) -> bool {
        a == b
    }

    /// Build `state.kitty.placed` from `state.kitty.roots`: position each
    /// root, and keep the ones with any part in the captured rows. This
    /// never reads the image storage, so it is cheap enough to run on
    /// every scroll. The old contents of `placed` move to `placed_prev`.
    pub(super) fn place_roots(state: &mut RenderState) {
        core::mem::swap(&mut state.kitty.placed, &mut state.kitty.placed_prev);
        state.kitty.placed.clear();
        if state.kitty.roots.is_empty() {
            return;
        }

        let range = state.row_data_range();
        let captured_len: isize = (range.end - range.start) as isize;
        let viewport_y_base = state.viewport_y(range.start);
        let cols = state.cols as i64;
        let captured_placeholders = &state.row_data.placeholders[range.start..range.end];
        let kitty = &mut state.kitty;

        // Group the captured rows by page, for `row_index_of_pin`. Only
        // `Pin` anchors need this.
        kitty.chunks.clear();
        if (kitty.virtual_anchors as usize) < kitty.roots.len() {
            let captured_pins = &state.row_data.pin[range.start..range.end];
            kitty.chunks.reserve(captured_pins.len());
            for (i, pin) in captured_pins.iter().enumerate() {
                if i > 0 && captured_pins[i - 1].node == pin.node {
                    continue;
                }
                kitty.chunks.push(KittyChunk {
                    node: pin.node,
                    start: pin.y,
                    index: i as isize,
                });
            }
        }

        for root in kitty.roots.iter() {
            let p = root.placement;

            // Where the anchor is. `y` counts captured rows, where 0 is
            // the first captured row.
            let (origin_x, origin_y): (i64, i64) = match root.anchor {
                // The top-left corner of the virtual placement's cells:
                // the smallest column and the smallest row of those
                // cells. Without any of those cells in the captured rows,
                // the placement isn't drawn, which matches Kitty. There
                // are rarely more than a few of these, so a simple scan of
                // the runs for each one is fine.
                KittyAnchor::Virtual(key) => {
                    let mut origin: Option<(i64, i64)> = None;
                    for (y, runs) in captured_placeholders.iter().enumerate() {
                        for run in runs.iter() {
                            let Some(target) = run.target else { continue };
                            if !target.eql(key) {
                                continue;
                            }
                            if let Some(o) = &mut origin {
                                o.0 = o.0.min(run.x as i64);
                                o.1 = o.1.min(y as i64);
                            } else {
                                origin = Some((run.x as i64, y as i64));
                            }
                        }
                    }
                    let Some(o) = origin else { continue };
                    o
                }

                // The pin's cell. A pin is marked garbage when its row
                // is removed from the scrollback. It has no position
                // then, so the placement isn't drawn.
                KittyAnchor::Pin(pin) => {
                    // SAFETY: the pin is owned by the image storage and live
                    // while the storage generation matches (see KittyAnchor).
                    let pin: Pin = unsafe { *pin };
                    if pin.garbage {
                        continue;
                    }
                    let Some(y) = row_index_of_pin(&kitty.chunks, pin, captured_len, p.rows, root.vertical_offset)
                    else {
                        continue;
                    };
                    (pin.x as i64, y as i64)
                }
            };

            // The top-left cell, which can be above or left of the
            // captured rows. Placements are reported at full size, so
            // keep any with at least one cell in the captured rows.
            let x: i64 = origin_x + root.horizontal_offset as i64;
            let y: i64 = origin_y + root.vertical_offset as i64;
            if y + p.rows as i64 - 1 < 0 || y >= captured_len as i64 {
                continue;
            }
            if x + p.cols as i64 - 1 < 0 || x >= cols {
                continue;
            }

            let mut positioned = p;
            let Ok(px) = i32::try_from(x) else { continue };
            let Ok(py) = i32::try_from(viewport_y_base as i64 + y) else { continue };
            positioned.x = px;
            positioned.y = py;
            kitty.placed.push(positioned);
        }
    }

    /// Returns the row of a pin, counted in captured rows where 0 is the
    /// first captured row. The result is negative for rows above the
    /// captured rows and `captured_len` or more for rows below them.
    ///
    /// `rows` and `vertical_offset` describe the placement that the pin
    /// anchors: it starts `vertical_offset` rows below the pin and is
    /// `rows` tall. If that placement can't reach the captured rows, the
    /// search may stop early and return None.
    ///
    /// For example, with 5 captured rows and a placement 3 rows tall:
    ///
    ///   - A pin 2 rows above the first captured row returns -2. The
    ///     placement covers rows -2 to 0, so its last row is visible.
    ///   - A pin 10 rows above it may return None, because the placement
    ///     ends at row -8.
    ///
    /// Pins on captured pages are found with the chunk table alone. Other
    /// pins are found by walking the pages above or below. This is much
    /// cheaper than `point_from_pin`, which counts rows from the top of the
    /// scrollback. It reads page list nodes, so it is only safe while
    /// the terminal can't change, such as during `begin_update`.
    pub(super) fn row_index_of_pin(
        chunks: &[KittyChunk],
        pin: Pin,
        captured_len: isize,
        rows: u32,
        vertical_offset: i32,
    ) -> Option<isize> {
        if chunks.is_empty() {
            return None;
        }

        // A pin on a page with captured rows. This also covers pins on
        // rows of those pages that aren't captured.
        for chunk in chunks {
            if chunk.node != pin.node {
                continue;
            }
            return Some(chunk.index + pin.y as isize - chunk.start as isize);
        }

        // SAFETY: nodes are live while the terminal can't change (see
        // the function docs).
        let nd = |n: NodePtr| -> &Node { unsafe { &*n.as_ptr() } };

        // A pin on a page above the captured rows. The placement's last
        // row is the pin's row plus `reach`, so it is only visible when
        // the pin's row is -reach or more. Walk up a page at a time, and
        // stop once a page's last row is too far up. `bottom` is the
        // last row of the page being checked.
        let reach: isize = rows as isize - 1 + (vertical_offset.max(0) as isize);
        let first = chunks[0];
        let mut bottom: isize = first.index - first.start as isize - 1;
        let mut node = nd(first.node).prev;
        while let Some(n) = node {
            if bottom < -reach {
                break;
            }
            let top = bottom - (nd(n).rows() as isize - 1);
            if n == pin.node {
                return Some(top + pin.y as isize);
            }
            bottom = top - 1;
            node = nd(n).prev;
        }

        // A pin on a page below the captured rows. Only a negative
        // offset can move such a placement up into them. Its first row
        // is the pin's row plus the offset, which must be less than
        // `captured_len`. Walk down a page at a time, and stop once a
        // page's first row is too far down. `top` is the first row of
        // the page being checked.
        if vertical_offset >= 0 {
            return None;
        }
        let last = chunks[chunks.len() - 1];
        let mut top: isize = last.index - last.start as isize + nd(last.node).rows() as isize;
        node = nd(last.node).next;
        while let Some(n) = node {
            if top + vertical_offset as isize >= captured_len {
                break;
            }
            if n == pin.node {
                return Some(top + pin.y as isize);
            }
            top += nd(n).rows() as isize;
            node = nd(n).next;
        }

        None
    }

    /// Build `state.kitty.roots` again from the image storage. Everything
    /// about a root except its position depends only on the storage and
    /// the cell size, so this only runs when one of those changes. It
    /// visits every placement in the storage.
    fn rebuild_roots(state: &mut RenderState, storage: &ImageStorage, t: &TerminalInfo) {
        state.kitty.roots.clear();
        state.kitty.virtual_anchors = 0;
        for (key, value) in storage.placements.iter() {
            // Keep placements with garbage pins. A pin can become
            // garbage without a generation change, so `place_roots`
            // checks for it every time instead.
            let root: KittyRoot = match value.location {
                StorageLocation::Pin(pin) => {
                    let Some(placement) = template(storage, t, *key, value, KittyPlacementKind::Fixed) else {
                        continue;
                    };
                    KittyRoot {
                        anchor: KittyAnchor::Pin(pin.as_ptr()),
                        horizontal_offset: 0,
                        vertical_offset: 0,
                        placement,
                    }
                }

                StorageLocation::Virtual => continue,

                StorageLocation::Relative(rel) => {
                    // Follow the chain of parents to the placement at its
                    // start. A broken chain is never drawn.
                    let Some(chain) = storage.resolve_chain(rel) else { continue };
                    let anchor = match chain.root.location {
                        StorageLocation::Virtual => KittyAnchor::Virtual(chain.root_key),
                        StorageLocation::Pin(pin) => KittyAnchor::Pin(pin.as_ptr()),

                        // resolveChain roots are never relative.
                        StorageLocation::Relative(_) => unreachable!(),
                    };
                    let Some(placement) = template(storage, t, *key, value, KittyPlacementKind::Relative) else {
                        continue;
                    };
                    KittyRoot {
                        anchor,
                        horizontal_offset: chain.horizontal_offset,
                        vertical_offset: chain.vertical_offset,
                        placement,
                    }
                }
            };

            if matches!(root.anchor, KittyAnchor::Virtual(_)) {
                state.kitty.virtual_anchors += 1;
            }
            state.kitty.roots.push(root);
        }
    }

    /// Returns the `ImagePlacement` for a placement in the image storage,
    /// with everything except `x` and `y` filled in. Returns None if
    /// there is nothing to draw, such as when the image is missing, still
    /// loading, or the terminal has no cell size in pixels.
    fn template(
        storage: &ImageStorage,
        t: &TerminalInfo,
        key: PlacementKey,
        p: &StoragePlacement,
        kind: KittyPlacementKind,
    ) -> Option<ImagePlacement> {
        // Nothing to draw until the displayed pixels arrive. For an
        // animation, those are the current frame's pixels.
        let image = storage.image_by_id(key.image_id)?;
        if image.render_data().is_pending() {
            return None;
        }

        // The size is zero when the terminal has no cell size in pixels.
        // Then there is nothing to draw.
        let grid = p.grid_size(image, t);
        if grid.cols == 0 || grid.rows == 0 {
            return None;
        }
        let dest = p.pixel_size(image, t);
        if dest.width == 0 || dest.height == 0 {
            return None;
        }
        let offset = p.cell_offset(t);
        let source = p.source_rect(image);

        Some(ImagePlacement {
            protocol: ImagePlacementProtocol::Kitty(KittyPlacement {
                kind,
                placement_id: match key.placement_id.tag {
                    PlacementTag::External => key.placement_id.id,
                    PlacementTag::Internal => 0,
                },
            }),
            image_id: key.image_id,
            image_generation: image.generation,
            layer: image_layer_from_z(p.z),
            z: p.z,
            x: 0,
            y: 0,
            cols: grid.cols,
            rows: grid.rows,
            offset_x: offset.x,
            offset_y: offset.y,
            width: dest.width,
            height: dest.height,
            source_x: source.x,
            source_y: source.y,
            source_width: source.width,
            source_height: source.height,
        })
    }

    /// Look up what one placeholder run draws and store it in the run.
    /// `row_pin` is the pin of the run's row.
    fn resolve_placeholder(
        storage: &ImageStorage,
        row_pin: Pin,
        run: &mut Placeholder,
        cell_width: u32,
        cell_height: u32,
    ) {
        run.state = PlaceholderState::Hidden;
        run.target = None;

        // Remember the virtual placement even if the image can't be
        // drawn. Relative placements attached to it still need the
        // position of these cells.
        let Some(target) = storage.placeholder_target(run.image_id, run.placement_id) else {
            return;
        };
        run.target = Some(target.key);

        // Nothing to draw until the displayed pixels arrive, as in
        // `template`.
        let Some(image) = storage.image_by_id(run.image_id) else { return };
        if image.render_data().is_pending() {
            return;
        }

        // Without a cell size in pixels, there is no size to draw at.
        if cell_width == 0 || cell_height == 0 {
            return;
        }

        let mut pin = row_pin;
        pin.x = run.x;
        let vp = UnicodePlacement {
            pin,
            image_id: run.image_id,
            placement_id: run.placement_id,
            col: run.col,
            row: run.row,
            width: run.width as u32,
            height: 1,
        };
        let rp = match vp.render_placement(&target.placement, image, cell_width, cell_height) {
            Ok(rp) => rp,
            Err(err) => {
                log::warn!(target: LOG_TARGET, "error resolving virtual placement err={err}");
                return;
            }
        };
        if rp.dest_width == 0 || rp.dest_height == 0 {
            return;
        }

        run.geometry = PlaceholderGeometry {
            image_generation: image.generation,
            offset_x: rp.offset_x,
            offset_y: rp.offset_y,
            width: rp.dest_width,
            height: rp.dest_height,
            source_x: rp.source_x,
            source_y: rp.source_y,
            source_width: rp.source_width,
            source_height: rp.source_height,
        };
        run.state = PlaceholderState::Visible;
    }

    /// Groups Kitty placeholder cells into runs as the row scan visits
    /// them. Feed it placeholder cells from left to right. A gap between
    /// two cells (any cell that isn't fed) ends the current run.
    pub(super) struct PlaceholderDecoder<'a> {
        pub list: &'a mut Vec<Placeholder>,
        pub stale: &'a mut bool,
        pub run: Option<IncompletePlacement>,
        pub start: usize,
        pub end: usize,
    }

    impl PlaceholderDecoder<'_> {
        #[inline(always)]
        pub fn feed(&mut self, x: usize, style: &Style, cps: &[u32]) {
            let curr = IncompletePlacement::init(style, cps);
            if let Some(prev) = &mut self.run {
                if x == self.end && prev.append(&curr) {
                    self.end += 1;
                    return;
                }
                self.flush();
            }
            self.run = Some(curr.start());
            self.start = x;
            self.end = x + 1;
        }

        pub fn flush(&mut self) {
            let Some(run) = self.run.take() else { return };
            *self.stale = true;
            self.list.push(Placeholder {
                x: self.start as CellCountInt,
                width: run.width as CellCountInt,
                image_id: run.image_id(),
                placement_id: run.placement_id.unwrap_or(0),
                row: run.row.unwrap(),
                col: run.col.unwrap(),
                state: PlaceholderState::Stale,
                target: None,
                geometry: PlaceholderGeometry::default(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Row builder
// ---------------------------------------------------------------------------

/// The number of rows/cells we scan as a single group when looking for
/// dirty rows or special cells. Rows and cells are small packed structs
/// so a group is scanned with a handful of vector operations.
const SCAN_GROUP_LEN: usize = 8;

/// Group scan helper for the row dirty flag. A row that matches has
/// its dirty flag unset.
type RowDirtyMask = Mask<page::Row, { field_mask(&[row_fields::DIRTY]) }, SCAN_GROUP_LEN>;

/// Group scan helper for the cell fields that require managed memory
/// handling. A cell that matches is a plain (possibly zero) codepoint
/// with a default style, requiring no work beyond the raw copy. See
/// RowBuilder::row.
type CellSpecialMask =
    Mask<page::Cell, { field_mask(&[cell_fields::CONTENT_TAG, cell_fields::STYLE_ID]) }, SCAN_GROUP_LEN>;

/// Internal helper for RenderState::update that rebuilds a single row of
/// the render state from the current page contents.
struct RowBuilder<'a> {
    cols: usize,
    raws: &'a mut [page::Row],
    cells: &'a mut [Cells],
    sels: &'a mut [Option<[CellCountInt; 2]>],
    highlights: &'a mut [Vec<Highlight>],
    placeholders: &'a mut [Vec<Placeholder>],
    kitty_stale: &'a mut bool,
    dirties: &'a mut [bool],
    pending_styles: &'a mut Vec<StyleRun>,
    applied_styles: &'a mut [Vec<StyleRun>],
}

impl RowBuilder<'_> {
    fn row(&mut self, p: &Page, page_row: &page::Row, vy: usize) {
        // Reset our per-row state if we're rebuilding this row. A
        // non-zero cell length means the row was populated by a prior
        // update.
        if !self.cells[vy].is_empty() {
            self.cells[vy].grapheme_data.clear();
            self.sels[vy] = None;
            self.highlights[vy].clear();
            // Dropping this row's runs changes what the image placement
            // iterator returns, so tell the Kitty update. Only a row
            // that had placeholder cells can have runs. Checking that
            // flag first means other rows never read their run list.
            if build_options::KITTY_GRAPHICS
                && self.raws[vy].kitty_virtual_placeholder()
                && !self.placeholders[vy].is_empty()
            {
                *self.kitty_stale = true;
            }
            self.placeholders[vy].clear();
        }
        self.dirties[vy] = true;

        // Get all our cells in the page.
        // SAFETY: the row belongs to page `p` and has `cols` cells.
        let page_cells: &[page::Cell] = unsafe { &(&*p.get_cells(page_row))[..self.cols] };

        // Copy our raw row data
        self.raws[vy] = *page_row;

        // Note: our cells storage uses the general allocator. We do this
        // on purpose because as rows become dirty, we do not want to
        // reallocate space for cells (which are large). This was a source
        // of huge slowdown.
        let cells: &mut Cells = &mut self.cells[vy];
        if cells.len() != self.cols {
            // The cell storage (including the per-cell style data) is
            // being reallocated, so the applied style cache no longer
            // describes it. Clear it before the resize so an error
            // can't leave it stale.
            self.applied_styles[vy].clear();
            cells.resize(self.cols);
        }

        // We always copy our raw cell data. In the case we have no
        // managed memory, we can skip setting any other fields.
        //
        // This is an important optimization. For plain-text screens
        // this ends up being something around 300% faster based on
        // the `screen-clone` benchmark.
        crate::fastmem::copy(&mut cells.raw, page_cells);

        // Rows with Kitty placeholders are rare. They get their own copy
        // of the scan below, so other rows pay nothing for them. These
        // rows are scanned even without managed memory, because a
        // placeholder cell can be a plain character with no style.
        let placeholder_row = build_options::KITTY_GRAPHICS && page_row.kitty_virtual_placeholder();
        if !page_row.managed_memory() && !placeholder_row {
            return;
        }

        if placeholder_row {
            self.scan_placeholders(p, page_cells, vy);
            return;
        }

        self.scan::<false>(p, page_cells, vy, None);
    }

    /// The placeholder version of `scan` (Zig: `scan(true, ...)` followed
    /// by `decoder.flush()`).
    #[cold]
    #[inline(never)]
    fn scan_placeholders(&mut self, p: &Page, page_cells: &[page::Cell], vy: usize) {
        // Temporarily take the run list out so the decoder can borrow it
        // while the builder is borrowed by the scan.
        let mut list = core::mem::take(&mut self.placeholders[vy]);
        let mut stale = false;
        {
            let mut decoder = kitty::PlaceholderDecoder {
                list: &mut list,
                stale: &mut stale,
                run: None,
                start: 0,
                end: 0,
            };
            self.scan::<true>(p, page_cells, vy, Some(&mut decoder));
            decoder.flush();
        }
        if stale {
            *self.kitty_stale = true;
        }
        self.placeholders[vy] = list;
    }

    /// The managed memory scan of `row`. When `PLACEHOLDERS` is true,
    /// this also feeds every Kitty placeholder cell to the decoder.
    ///
    /// This is deliberately not inline. Each value of `PLACEHOLDERS`
    /// produces its own function, so the placeholder handling can't
    /// change how the common version (without placeholders) is
    /// optimized.
    #[inline(never)]
    fn scan<const PLACEHOLDERS: bool>(
        &mut self,
        p: &Page,
        page_cells: &[page::Cell],
        vy: usize,
        mut decoder: Option<&mut kitty::PlaceholderDecoder<'_>>,
    ) {
        let cells = &mut self.cells[vy];
        let n = page_cells.len();
        let runs_start = self.pending_styles.len();
        let mut x: usize = 0;
        'scan: while x < n {
            // Skip runs of plain cells a group at a time. Cells that
            // need managed handling are often rare even within rows that
            // have managed memory (e.g. a row is "styled" if a single
            // cell has a style) so groups are skipped with a few vector
            // operations. On placeholder rows a plain cell can be a
            // placeholder, so those rows check each cell instead.
            if !PLACEHOLDERS {
                while n - x >= CellSpecialMask::GROUP_LEN {
                    if !CellSpecialMask::r#match(page_cells, x) {
                        break;
                    }
                    x += CellSpecialMask::GROUP_LEN;
                }
            }

            // Scalar scan to the next special cell.
            loop {
                if x >= n {
                    break 'scan;
                }
                if !CellSpecialMask::match_scalar(page_cells[x]) {
                    break;
                }
                if PLACEHOLDERS && page_cells[x].codepoint() == graphemes_placeholder() {
                    if let Some(d) = decoder.as_deref_mut() {
                        d.feed(x, &Style::default(), &[]);
                    }
                }
                x += 1;
            }

            let page_cell = &page_cells[x];

            match page_cell.content_tag() {
                // Single-codepoint styled cells are by far the most
                // common special cells, and they usually come in long
                // runs sharing one style ID (e.g. a fully styled row
                // usually uses a single style). Find the run and record
                // it: this does one style lookup per run and defers the
                // (large) per-cell fill to end_update, outside of any
                // terminal locks.
                ContentTag::Codepoint => {
                    let sid = page_cell.style_id();
                    debug_assert!(sid > 0); // special + codepoint implies styled
                    // SAFETY: the style id is a live entry of the page's set.
                    let style_val: Style = unsafe { *p.styles.get(p.memory, sid) };

                    // A cell continues the run if its masked special
                    // bits are exactly the style ID of the run (in
                    // particular the content tag must be a plain
                    // codepoint). We can check groups of cells at a
                    // time this way.
                    let pattern = CellSpecialMask::pattern(*page_cell);
                    let start = x;
                    x += 1;
                    while n - x >= CellSpecialMask::GROUP_LEN {
                        if !CellSpecialMask::eql(page_cells, x, pattern) {
                            break;
                        }
                        x += CellSpecialMask::GROUP_LEN;
                    }
                    while x < n {
                        if !CellSpecialMask::eql_scalar(page_cells[x], pattern) {
                            break;
                        }
                        x += 1;
                    }

                    self.pending_styles.push(StyleRun {
                        y: vy as CellCountInt,
                        start: start as CellCountInt,
                        end: x as CellCountInt,
                        style: style_val,
                    });

                    // A placeholder can leave out its row and column
                    // (it then continues the run to its left), which
                    // makes it a plain styled cell like these. Every
                    // cell in this style run has the same style.
                    if PLACEHOLDERS {
                        if let Some(d) = decoder.as_deref_mut() {
                            for (px, c) in page_cells.iter().enumerate().take(x).skip(start) {
                                if c.codepoint() != graphemes_placeholder() {
                                    continue;
                                }
                                d.feed(px, &style_val, &[]);
                            }
                        }
                    }
                }

                // If we have a multi-codepoint grapheme, look it up and
                // set our content type. Note grapheme cells may also
                // be styled. The style must be recorded as a run (rather
                // than written directly) so that it is ordered correctly
                // relative to possibly-stale runs from prior updates.
                ContentTag::CodepointGrapheme => {
                    let style_val: Style = if page_cell.style_id() > 0 {
                        // SAFETY: the style id is a live entry of the page's set.
                        let style_val = unsafe { *p.styles.get(p.memory, page_cell.style_id()) };
                        self.pending_styles.push(StyleRun {
                            y: vy as CellCountInt,
                            start: x as CellCountInt,
                            end: (x + 1) as CellCountInt,
                            style: style_val,
                        });
                        style_val
                    } else {
                        Style::default()
                    };
                    let cps: &[u32] = p.lookup_grapheme(page_cell).unwrap_or(&[]);
                    cells.set_grapheme(x, cps);

                    // Most placeholders give their row and column in
                    // the image as combining characters, so most are
                    // handled here, using the style and grapheme we
                    // already have.
                    if PLACEHOLDERS && page_cell.codepoint() == graphemes_placeholder() {
                        if let Some(d) = decoder.as_deref_mut() {
                            d.feed(x, &style_val, cps);
                        }
                    }
                    x += 1;
                }

                // Background-color-only cells. The style is derived
                // entirely from the cell contents. Consecutive cleared
                // cells with the same background are bit-identical, so
                // we run-detect on full equality (e.g. a line cleared
                // with a background color pending is one run).
                tag @ (ContentTag::BgColorRgb | ContentTag::BgColorPalette) => {
                    let style_val: Style = match tag {
                        ContentTag::BgColorRgb => Style {
                            bg_color: style::Color::Rgb(page_cell.content_color_rgb()),
                            ..Style::default()
                        },
                        ContentTag::BgColorPalette => Style {
                            bg_color: style::Color::Palette(page_cell.content_color_palette()),
                            ..Style::default()
                        },
                        _ => unreachable!(),
                    };

                    let first_bits = CellSpecialMask::bits(*page_cell);
                    let start = x;
                    x += 1;
                    while n - x >= CellSpecialMask::GROUP_LEN {
                        if !CellSpecialMask::eql_exact(page_cells, x, first_bits) {
                            break;
                        }
                        x += CellSpecialMask::GROUP_LEN;
                    }
                    while x < n {
                        if CellSpecialMask::bits(page_cells[x]) != first_bits {
                            break;
                        }
                        x += 1;
                    }

                    self.pending_styles.push(StyleRun {
                        y: vy as CellCountInt,
                        start: start as CellCountInt,
                        end: x as CellCountInt,
                        style: style_val,
                    });
                }
            }
        }

        // Reserve the applied style cache capacity for the runs we
        // appended so that end_update (which cannot allocate) is able
        // to record what it applies. See RowData::applied_styles.
        let runs_added = self.pending_styles.len() - runs_start;
        if runs_added > 0 {
            let applied = &mut self.applied_styles[vy];
            if applied.capacity() < runs_added {
                applied.reserve(runs_added - applied.len());
            }
        }
    }
}

/// The Kitty placeholder codepoint (`kitty_gfx.unicode.placeholder`).
#[inline(always)]
const fn graphemes_placeholder() -> u32 {
    graphics_unicode::PLACEHOLDER
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::highlight::{Flattened, FlattenedChunk};
    use crate::terminal::page_list::PageList;
    use crate::terminal::terminal::{Options as TerminalOptions, ScrollViewport};

    fn term(cols: CellCountInt, rows: CellCountInt) -> Terminal {
        Terminal::init(TerminalOptions::new(cols, rows)).unwrap()
    }

    fn term_scrollback(cols: CellCountInt, rows: CellCountInt, max: Option<usize>) -> Terminal {
        Terminal::init(TerminalOptions {
            max_scrollback_bytes: max,
            ..TerminalOptions::new(cols, rows)
        })
        .unwrap()
    }

    // fill_styles (two overlapping vector stores per element) against
    // slice::fill, for varied styles, lengths and sub-slices.
    #[test]
    fn fill_styles_matches_fill() {
        use crate::terminal::color::Rgb;
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let color = |r: u64| -> style::Color {
            match r % 3 {
                0 => style::Color::None,
                1 => style::Color::Palette((r >> 8) as u8),
                _ => style::Color::Rgb(Rgb { r: (r >> 8) as u8, g: (r >> 16) as u8, b: (r >> 24) as u8 }),
            }
        };
        for _ in 0..500 {
            let mut value = Style {
                fg_color: color(next()),
                bg_color: color(next()),
                underline_color: color(next()),
                ..Style::default()
            };
            let f = next();
            value.flags.bold = f & 1 != 0;
            value.flags.inverse = f & 2 != 0;
            value.flags.strikethrough = f & 4 != 0;
            let old = Style { fg_color: color(next()), ..Style::default() };
            let len = (next() % 40) as usize;
            let start = (next() % (len as u64 + 1)) as usize;
            let end = start + (next() % ((len - start) as u64 + 1)) as usize;

            let mut a = vec![old; len];
            let mut b = vec![old; len];
            fill_styles(&mut a[start..end], value);
            b[start..end].fill(value);
            for (x, y) in a.iter().zip(&b) {
                assert!(x.eql(y));
            }
        }
    }

    /// Feed `input` to a VT stream of `t` (Zig: `s.nextSlice(input)`).
    fn write(t: &mut Terminal, input: &[u8]) {
        let mut s = t.vt_stream();
        s.next_slice(input);
        s.deinit();
    }

    fn active_pin(t: &Terminal, x: CellCountInt, y: u32) -> Pin {
        t.screens
            .active
            .pages
            .pin(point::Point::Active(point::Coordinate { x, y }))
            .unwrap()
    }

    fn screen(t: &mut Terminal) -> &mut Screen {
        // SAFETY: the active screen is live while `t` is.
        unsafe { t.screens.active.as_mut() }
    }

    #[test]
    fn styled() {
        let mut t = term(80, 24);

        // This fills the screen up
        t.decaln().unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);
    }

    #[test]
    fn basic_text() {
        let mut t = term(10, 3);
        write(&mut t, b"ABCD");

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Verify we have the right number of rows
        assert_eq!(3, state.row_data.len());

        // All rows should have cols cells
        let cells = &state.row_data.cells;
        assert_eq!(10, cells[0].len());
        assert_eq!(10, cells[1].len());
        assert_eq!(10, cells[2].len());

        // Row zero should contain our text
        assert_eq!('A' as u32, cells[0].get(0).raw.codepoint());
        assert_eq!('B' as u32, cells[0].get(1).raw.codepoint());
        assert_eq!('C' as u32, cells[0].get(2).raw.codepoint());
        assert_eq!('D' as u32, cells[0].get(3).raw.codepoint());
        assert_eq!(0, cells[0].get(4).raw.codepoint());
    }

    #[test]
    fn styled_text() {
        let mut t = term(10, 3);
        write(&mut t, b"\x1b[1mA"); // Bold
        write(&mut t, b"\x1b[0;3mB"); // Italic
        write(&mut t, b"\x1b[0;4mC"); // Underline

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Verify we have the right number of rows
        assert_eq!(3, state.row_data.len());

        // All rows should have cols cells
        let cells = &state.row_data.cells;
        assert_eq!(10, cells[0].len());
        assert_eq!(10, cells[1].len());
        assert_eq!(10, cells[2].len());

        // Row zero should contain our text
        {
            let cell = cells[0].get(0);
            assert_eq!('A' as u32, cell.raw.codepoint());
            assert!(cell.style.flags.bold);
        }
        {
            let cell = cells[0].get(1);
            assert_eq!('B' as u32, cell.raw.codepoint());
            assert!(!cell.style.flags.bold);
            assert!(cell.style.flags.italic);
        }
        assert_eq!('C' as u32, cells[0].get(2).raw.codepoint());
        assert_eq!(0, cells[0].get(3).raw.codepoint());
    }

    /// Verifies that an incrementally updated render state has identical
    /// contents to a from-scratch rebuild. This is the load-bearing check
    /// for our dirty tracking: if any terminal operation changes row
    /// contents without setting a dirty signal that `update` honors
    /// (terminal dirty, screen dirty, page dirty, row dirty, viewport pin,
    /// or dimensions), the incremental state will contain stale rows and
    /// this comparison will fail.
    fn test_compare_states(incremental: &RenderState, fresh: &RenderState) {
        // Row metadata that is allowed to be stale in an incremental
        // update. Dirty tracking only guarantees that VISUAL changes are
        // flagged (see page::Row::dirty); these fields are non-visual
        // metadata that the terminal may change without dirtying the row
        // (e.g. Screen.cursorResetWrap clears wrap flags without a dirty
        // mark). This staleness predates the chunked update
        // implementation; it is present in the row-iterator implementation
        // as well.
        type StaleOkMask = Mask<
            page::Row,
            {
                field_mask(&[
                    row_fields::WRAP,
                    row_fields::WRAP_CONTINUATION,
                    row_fields::SEMANTIC_PROMPT,
                    row_fields::DIRTY,
                ])
            },
            1,
        >;

        assert_eq!(fresh.rows, incremental.rows);
        assert_eq!(fresh.cols, incremental.cols);
        assert_eq!(fresh.cursor.active, incremental.cursor.active);
        assert_eq!(fresh.cursor.viewport, incremental.cursor.viewport);
        assert_eq!(fresh.cursor.cell.to_bits(), incremental.cursor.cell.to_bits());

        // Unused entries (outside row_data_range) may hold anything, so
        // we only compare the populated range.
        assert_eq!(fresh.overscan, incremental.overscan);
        assert_eq!(fresh.row_data_range(), incremental.row_data_range());
        let range = fresh.row_data_range();

        let inc_data = &incremental.row_data;
        let new_data = &fresh.row_data;
        assert_eq!(new_data.len(), inc_data.len());
        for y in range.start..range.end {
            // Pins must match exactly.
            let inc_pin = inc_data.pin[y];
            let new_pin = new_data.pin[y];
            assert_eq!(new_pin.node, inc_pin.node, "mismatch on row y={y}");
            assert_eq!(new_pin.y, inc_pin.y, "mismatch on row y={y}");

            // Raw row data must match, except for non-visual metadata
            // fields which may legitimately be stale (see StaleOkMask).
            let inc_row = inc_data.raw[y];
            let new_row = new_data.raw[y];
            assert_eq!(StaleOkMask::strip(new_row), StaleOkMask::strip(inc_row), "mismatch on row y={y}");

            let inc_cells = inc_data.cells[y].slice();
            let new_cells = new_data.cells[y].slice();
            assert_eq!(new_cells.len(), inc_cells.len());
            let managed = new_row.managed_memory();
            for x in 0..new_cells.len() {
                // Raw cell contents must match.
                let inc_cell = inc_cells.raw[x];
                let new_cell = new_cells.raw[x];
                assert_eq!(new_cell.to_bits(), inc_cell.to_bits(), "mismatch on row y={y} cell x={x}");

                // The style is only defined if the cell is styled or is
                // a bg-color cell within a row that has managed memory.
                if new_cell.style_id() != 0
                    || (managed
                        && matches!(new_cell.content_tag(), ContentTag::BgColorRgb | ContentTag::BgColorPalette))
                {
                    assert_eq!(new_cells.style[x], inc_cells.style[x], "mismatch on row y={y} cell x={x}");
                }

                // Graphemes are only defined for grapheme cells.
                if new_cell.content_tag() == ContentTag::CodepointGrapheme {
                    assert_eq!(new_cells.grapheme(x), inc_cells.grapheme(x), "mismatch on row y={y} cell x={x}");
                }
            }
        }
    }

    #[test]
    fn incremental_updates_match_full_rebuild() {
        test_incremental_matches_fresh(Overscan::default());
    }

    #[test]
    fn incremental_updates_match_full_rebuild_with_overscan() {
        test_incremental_matches_fresh(Overscan { above: 2, below: 1 });
    }

    /// A small deterministic PRNG (xorshift64*) standing in for Zig's
    /// `std.Random.DefaultPrng`.
    struct Prng(u64);

    impl Prng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A uniform integer in `[lo, hi]` (Zig: `intRangeAtMost`).
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }

        fn boolean(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    fn test_incremental_matches_fresh(request: Overscan) {
        // Deterministic so failures are reproducible.
        let mut rand = Prng(0xB0BA_CAFE);

        let mut t = term_scrollback(20, 8, Some(500));

        let mut inc = RenderState::empty();
        inc.overscan_request = request;

        for _ in 0..300 {
            // Perform a random batch of operations between updates.
            for _ in 0..rand.range(1, 6) {
                match rand.range(0, 18) {
                    // Plain text (possibly wrapping and scrolling).
                    0..=2 => {
                        for _ in 0..rand.range(1, 30) {
                            write(&mut t, &[rand.range(b'A' as u64, b'Z' as u64) as u8]);
                        }
                    }

                    // Newlines to build scrollback and trigger pruning.
                    3 | 4 => {
                        for _ in 0..rand.range(1, 10) {
                            write(&mut t, b"x\r\n");
                        }
                    }

                    // Cursor movement.
                    5 => {
                        let s = format!("\x1b[{};{}H", rand.range(1, 8), rand.range(1, 20));
                        write(&mut t, s.as_bytes());
                    }

                    // Styling: bold, truecolor bg, palette fg, reset.
                    6 => write(
                        &mut t,
                        match rand.range(0, 3) {
                            0 => b"\x1b[1m".as_slice(),
                            1 => b"\x1b[48;2;30;60;90m",
                            2 => b"\x1b[38;5;120m",
                            _ => b"\x1b[0m",
                        },
                    ),

                    // Erase ops (EL, ED variants including scrollback).
                    7 => write(
                        &mut t,
                        match rand.range(0, 4) {
                            0 => b"\x1b[K".as_slice(),
                            1 => b"\x1b[1K",
                            2 => b"\x1b[J",
                            3 => b"\x1b[2J",
                            _ => b"\x1b[3J",
                        },
                    ),

                    // Insert/delete lines (row rotations within regions).
                    8 => write(&mut t, format!("\x1b[{}L", rand.range(1, 4)).as_bytes()),
                    9 => write(&mut t, format!("\x1b[{}M", rand.range(1, 4)).as_bytes()),

                    // Scroll up/down (page-dirty row rotations).
                    10 => write(&mut t, format!("\x1b[{}S", rand.range(1, 4)).as_bytes()),
                    11 => write(&mut t, format!("\x1b[{}T", rand.range(1, 4)).as_bytes()),

                    // Set/reset scroll regions to exercise bounded scrolls.
                    12 => {
                        let top = rand.range(1, 4);
                        let bot = rand.range(top + 1, 8);
                        write(&mut t, format!("\x1b[{};{}r", top, bot).as_bytes());
                    }

                    // Insert/delete/erase chars within a row.
                    13 => write(&mut t, format!("\x1b[{}@", rand.range(1, 5)).as_bytes()),
                    14 => write(&mut t, format!("\x1b[{}P", rand.range(1, 5)).as_bytes()),

                    // Reverse index (scroll down at top).
                    15 => write(&mut t, b"\x1bM"),

                    // Wide chars and multi-codepoint graphemes.
                    16 => write(&mut t, "字👨‍👩‍👧".as_bytes()),

                    // Alternate screen switching (screen key redraw path).
                    17 => write(
                        &mut t,
                        if rand.boolean() {
                            b"\x1b[?1049h".as_slice()
                        } else {
                            b"\x1b[?1049l"
                        },
                    ),

                    // DECALN full-screen fill.
                    18 => write(&mut t, b"\x1b#8"),

                    _ => unreachable!(),
                }
            }

            // Occasionally scroll the viewport into scrollback and back.
            match rand.range(0, 9) {
                0 => t.scroll_viewport(ScrollViewport::Delta(-3)),
                1 => t.scroll_viewport(ScrollViewport::Delta(2)),
                2 => t.scroll_viewport(ScrollViewport::Bottom),
                3 => t.scroll_viewport(ScrollViewport::Top),
                _ => {}
            }

            // Update our incremental state first: it must consume the dirty
            // state. The fresh state always fully rebuilds (its dimensions
            // start empty so it always redraws) and so does not depend on
            // any dirty flags.
            inc.update(&mut t);

            let mut fresh = RenderState::empty();
            fresh.overscan_request = request;
            fresh.update(&mut t);

            test_compare_states(&inc, &fresh);
        }
    }

    #[test]
    fn begin_and_end_update() {
        let mut t = term(10, 3);
        write(&mut t, b"\x1b[1mAB"); // Bold
        write(&mut t, b"\x1b[0;3mC"); // Italic

        let mut state = RenderState::empty();
        state.begin_update(&mut t);

        // We should have pending style runs on row 0: one for the bold
        // run and one for the italic run.
        {
            let runs = &state.pending_styles;
            assert_eq!(2, runs.len());
            assert_eq!(0, runs[0].y);
            assert_eq!(0, runs[0].start);
            assert_eq!(2, runs[0].end);
            assert!(runs[0].style.flags.bold);
            assert_eq!(0, runs[1].y);
            assert_eq!(2, runs[1].start);
            assert_eq!(3, runs[1].end);
            assert!(runs[1].style.flags.italic);
        }

        // End our update. This should denormalize the runs into cells
        // and clear the pending runs.
        state.end_update();
        {
            assert_eq!(0, state.pending_styles.len());

            let cells = &state.row_data.cells;
            assert!(cells[0].get(0).style.flags.bold);
            assert!(cells[0].get(1).style.flags.bold);
            assert!(cells[0].get(2).style.flags.italic);
        }
    }

    #[test]
    fn end_update_skips_unchanged_style_runs() {
        let mut t = term(10, 3);
        write(&mut t, b"\x1b[1mAB"); // Bold

        let mut state = RenderState::empty();
        state.update(&mut t);

        // The applied cache should record the bold run for row 0.
        {
            let applied = &state.row_data.applied_styles;
            assert_eq!(1, applied[0].len());
            assert!(applied[0][0].style.flags.bold);
            assert!(state.row_data.cells[0].get(0).style.flags.bold);
        }

        // Rewrite the text without changing the styling: the row is
        // rebuilt, the run matches the cache, and the styles must remain
        // correct (the fill is skipped internally).
        write(&mut t, b"\x1b[1;1H\x1b[1mXY");
        state.update(&mut t);
        {
            let cells = &state.row_data.cells[0];
            assert_eq!('X' as u32, cells.get(0).raw.codepoint());
            assert!(cells.get(0).style.flags.bold);
            assert!(cells.get(1).style.flags.bold);
        }

        // Change the styling: the cache mismatches and the new styles
        // must be applied and recorded.
        write(&mut t, b"\x1b[1;1H\x1b[0;3mZW"); // Italic
        state.update(&mut t);
        {
            let cells = &state.row_data.cells[0];
            assert_eq!('Z' as u32, cells.get(0).raw.codepoint());
            assert!(!cells.get(0).style.flags.bold);
            assert!(cells.get(0).style.flags.italic);

            let applied = &state.row_data.applied_styles;
            assert_eq!(1, applied[0].len());
            assert!(applied[0][0].style.flags.italic);
        }
    }

    #[test]
    fn bg_color_cells() {
        let mut t = term(10, 3);

        // Write a styled cell (so the row has managed memory) then erase
        // the rest of the line with a palette background pending. The
        // erase produces bg_color content cells rather than styled cells.
        write(&mut t, b"\x1b[1mA\x1b[48;5;1m\x1b[K");

        let mut state = RenderState::empty();
        state.update(&mut t);

        let cells = &state.row_data.cells;
        {
            let cell = cells[0].get(0);
            assert_eq!('A' as u32, cell.raw.codepoint());
            assert!(cell.style.flags.bold);
        }
        for x in 1..10 {
            let cell = cells[0].get(x);
            assert_eq!(ContentTag::BgColorPalette, cell.raw.content_tag());
            assert_eq!(style::Color::Palette(1), cell.style.bg_color);
        }
    }

    #[test]
    fn grapheme() {
        let mut t = term(10, 3);
        write(&mut t, b"A");
        write(&mut t, "👨‍".as_bytes()); // this has a ZWJ

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Verify we have the right number of rows
        assert_eq!(3, state.row_data.len());

        // All rows should have cols cells
        let cells = &state.row_data.cells;
        assert_eq!(10, cells[0].len());
        assert_eq!(10, cells[1].len());
        assert_eq!(10, cells[2].len());

        // Row zero should contain our text
        {
            let cell = cells[0].get(0);
            assert_eq!('A' as u32, cell.raw.codepoint());
        }
        {
            let cell = cells[0].get(1);
            assert_eq!(0x1F468, cell.raw.codepoint());
            assert_eq!(Wide::Wide, cell.raw.wide());
            assert_eq!(&[0x200D][..], cell.grapheme);
        }
        {
            let cell = cells[0].get(2);
            assert_eq!(0, cell.raw.codepoint());
            assert_eq!(Wide::SpacerTail, cell.raw.wide());
        }
    }

    #[test]
    fn cursor_state_in_viewport() {
        let mut t = term(10, 5);
        write(&mut t, b"A\x1b[H");

        let mut state = RenderState::empty();

        // Initial update
        state.update(&mut t);
        assert_eq!(0, state.cursor.active.x);
        assert_eq!(0, state.cursor.active.y);
        assert_eq!(0, state.cursor.viewport.unwrap().x);
        assert_eq!(0, state.cursor.viewport.unwrap().y);
        assert_eq!('A' as u32, state.cursor.cell.codepoint());
        assert!(state.cursor.style.is_default());

        // Set a style on the cursor
        write(&mut t, b"\x1b[1m"); // Bold
        state.update(&mut t);
        assert!(!state.cursor.style.is_default());
        assert!(state.cursor.style.flags.bold);
        write(&mut t, b"\x1b[0m"); // Reset style

        // Move cursor to 2,1
        write(&mut t, b"\x1b[2;3H");
        state.update(&mut t);
        assert_eq!(2, state.cursor.active.x);
        assert_eq!(1, state.cursor.active.y);
        assert_eq!(2, state.cursor.viewport.unwrap().x);
        assert_eq!(1, state.cursor.viewport.unwrap().y);
    }

    #[test]
    fn cursor_state_out_of_viewport() {
        let mut t = term(10, 2);
        write(&mut t, b"A\r\nB\r\nC\r\nD\r\n");

        let mut state = RenderState::empty();

        // Initial update
        state.update(&mut t);
        assert_eq!(0, state.cursor.active.x);
        assert_eq!(1, state.cursor.active.y);
        assert_eq!(0, state.cursor.viewport.unwrap().x);
        assert_eq!(1, state.cursor.viewport.unwrap().y);

        // Scroll the viewport
        t.scroll_viewport(ScrollViewport::Top);
        state.update(&mut t);

        // Set a style on the cursor
        assert_eq!(0, state.cursor.active.x);
        assert_eq!(1, state.cursor.active.y);
        assert!(state.cursor.viewport.is_none());
    }

    #[test]
    fn dirty_state() {
        let mut t = term(10, 5);

        let mut state = RenderState::empty();

        // First update should trigger redraw due to resize
        state.update(&mut t);
        assert_eq!(Dirty::Full, state.dirty);

        // Reset dirty flag and dirty rows
        state.dirty = Dirty::False;
        state.row_data.dirty.fill(false);

        // Second update with no changes - no dirty rows
        state.update(&mut t);
        assert_eq!(Dirty::False, state.dirty);
        for d in &state.row_data.dirty {
            assert!(!d);
        }

        // Write to first line
        write(&mut t, b"A");
        state.update(&mut t);
        assert_eq!(Dirty::Partial, state.dirty);
        {
            let dirty = &state.row_data.dirty;
            assert!(dirty[0]); // First row dirty
            assert!(!dirty[1]); // Second row clean
        }
    }

    #[test]
    fn colors() {
        let mut t = term(10, 5);

        let mut state = RenderState::empty();

        // Default colors
        state.update(&mut t);

        // Change cursor color
        write(&mut t, b"\x1b]12;#FF0000\x07");
        state.update(&mut t);

        let c = state.colors.cursor.unwrap();
        assert_eq!(0xFF, c.r);
        assert_eq!(0, c.g);
        assert_eq!(0, c.b);

        // Change palette color 0 to White
        write(&mut t, b"\x1b]4;0;#FFFFFF\x07");
        state.update(&mut t);
        let p0 = state.colors.palette[0];
        assert_eq!(0xFF, p0.r);
        assert_eq!(0xFF, p0.g);
        assert_eq!(0xFF, p0.b);
    }

    #[test]
    fn selection_single_line() {
        let mut t = term(10, 3);

        let sel = Selection::init(active_pin(&t, 0, 1), active_pin(&t, 2, 1), false);
        screen(&mut t).select(Some(sel)).unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);

        {
            let sels = &state.row_data.selection;
            assert_eq!(None, sels[0]);
            assert_eq!(Some([0, 2]), sels[1]);
            assert_eq!(None, sels[2]);
        }

        // Clear the selection
        screen(&mut t).select(None).unwrap();
        state.update(&mut t);
        let sels = &state.row_data.selection;
        assert_eq!(None, sels[0]);
        assert_eq!(None, sels[1]);
        assert_eq!(None, sels[2]);
    }

    #[test]
    fn selection_multiple_lines() {
        let mut t = term(10, 3);

        let sel = Selection::init(active_pin(&t, 0, 1), active_pin(&t, 2, 2), false);
        screen(&mut t).select(Some(sel)).unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);

        let sels = &state.row_data.selection;
        assert_eq!(None, sels[0]);
        assert_eq!(Some([0, t.screens.active.pages.cols - 1]), sels[1]);
        assert_eq!(Some([0, 2]), sels[2]);
    }

    #[test]
    fn selection_starting_on_wide_char_spacer_tail() {
        let mut t = term(10, 3);

        // Wide char at x=1 with its spacer tail at x=2.
        write(&mut t, "a宽b".as_bytes());

        // Start on the spacer tail, as when selecting from the right.
        let sel = Selection::init(active_pin(&t, 3, 0), active_pin(&t, 2, 0), false);
        screen(&mut t).select(Some(sel)).unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);

        let sels = &state.row_data.selection;
        assert_eq!(Some([1, 3]), sels[0]);
        assert_eq!(None, sels[1]);
    }

    #[test]
    fn selection_ending_on_wide_char_spacer_head() {
        let mut t = term(10, 3);

        // The wide char wraps, leaving a spacer head at x=9.
        // Position the cursor before rendering so overwriting the spacer
        // head won't dirty the following row through cursor movement.
        write(&mut t, "abcdefghi宽\x1b[1;10H".as_bytes());

        // Select up to and including the spacer head.
        let sel = Selection::init(active_pin(&t, 7, 0), active_pin(&t, 9, 0), false);

        let mut state = RenderState::empty();

        // A rectangle selection doesn't unwrap so it doesn't extend.
        screen(&mut t)
            .select(Some(Selection::init(sel.start(), sel.end(), true)))
            .unwrap();
        state.update(&mut t);

        assert_eq!(Some([7, 9]), state.row_data.selection[0]);
        assert_eq!(None, state.row_data.selection[1]);

        // A normal selection includes the wrapped wide char.
        screen(&mut t).select(Some(sel)).unwrap();
        state.update(&mut t);
        assert_eq!(Some([7, 9]), state.row_data.selection[0]);
        assert_eq!(Some([0, 0]), state.row_data.selection[1]);
        assert_eq!(None, state.row_data.selection[2]);

        // Removing the spacer head must unselect and redraw the following row.
        state.clean();
        write(&mut t, b"Z");
        state.update(&mut t);
        assert_eq!(None, state.row_data.selection[1]);
        assert!(state.row_data.dirty[1]);
        assert!(!state.row_data.dirty[2]);
    }

    #[test]
    fn link_cells() {
        let mut t = term(10, 5);

        let mut state = RenderState::empty();

        // Create a hyperlink
        write(&mut t, b"\x1b]8;;http://example.com\x1b\\LINK\x1b]8;;\x1b\\");
        state.update(&mut t);

        // Query link at 0,0
        let cells = state.link_cells(point::Coordinate { x: 0, y: 0 });

        assert_eq!(4, cells.len());
        assert!(cells.contains(&point::Coordinate { x: 0, y: 0 }));
        assert!(cells.contains(&point::Coordinate { x: 1, y: 0 }));
        assert!(cells.contains(&point::Coordinate { x: 2, y: 0 }));
        assert!(cells.contains(&point::Coordinate { x: 3, y: 0 }));

        // Query no link
        let cells2 = state.link_cells(point::Coordinate { x: 4, y: 0 });
        assert_eq!(0, cells2.len());
    }

    #[test]
    fn string() {
        let mut t = term(5, 2);
        write(&mut t, b"AB");

        let mut state = RenderState::empty();
        state.update(&mut t);

        let mut w: Vec<u8> = Vec::new();
        state.string(&mut w, None).unwrap();

        let expected = b"AB\x00\x00\x00\n\x00\x00\x00\x00\x00\n";
        assert_eq!(&expected[..], &w[..]);
    }

    #[test]
    fn link_cells_with_scrollback_spanning_pages() {
        let viewport_rows: CellCountInt = 10;
        let tail_rows: CellCountInt = 5;

        let mut t = term_scrollback(page::STD_CAPACITY.cols, viewport_rows, Some(10_000));

        let first_page_cap = {
            let pages: &PageList = &t.screens.active.pages;
            // SAFETY: the first node is live.
            unsafe { pages.pages.first.unwrap().as_ref().capacity().rows }
        };

        // Fill first page
        for _ in 0..first_page_cap - 1 {
            write(&mut t, b"\r\n");
        }

        // Create second page with hyperlink
        write(&mut t, b"\r\n");
        write(&mut t, b"\x1b]8;;http://example.com\x1b\\LINK\x1b]8;;\x1b\\");
        for _ in 0..(tail_rows - 1) {
            write(&mut t, b"\r\n");
        }

        let mut state = RenderState::empty();
        state.update(&mut t);

        let expected_viewport_y = (viewport_rows - tail_rows) as u32;
        // BUG: This crashes without the fix
        let cells = state.link_cells(point::Coordinate {
            x: 0,
            y: expected_viewport_y,
        });
        assert_eq!(4, cells.len());
    }

    #[test]
    fn link_cells_with_invalid_viewport_point() {
        let mut t = term(10, 5);

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Row out of bound
        {
            let cells = state.link_cells(point::Coordinate {
                x: 0,
                y: t.rows as u32 + 10,
            });
            assert_eq!(0, cells.len());
        }

        // Col out of bound
        {
            let cells = state.link_cells(point::Coordinate { x: t.cols + 10, y: 0 });
            assert_eq!(0, cells.len());
        }
    }

    #[test]
    fn flattened_highlights_require_matching_page_serial() {
        let mut t = term(10, 3);

        // Capture the live generation while terminal-owned state is in scope so
        // we can also verify begin_update copies it into the render row.
        let live_pin = t.screens.active.pages.get_top_left(point::Tag::Viewport);
        // SAFETY: the viewport pin's node is live.
        let live_serial = unsafe { live_pin.node.as_ref().serial };

        let mut state = RenderState::empty();
        state.update(&mut t);

        let pin: Pin = {
            state.row_data.dirty.fill(false);
            state.dirty = Dirty::False;
            state.row_data.pin[0]
        };
        let row_serial = state.row_data.serial[0];
        assert_eq!(live_pin.node, pin.node);
        assert_eq!(live_serial, row_serial);

        // Use the exact node pointer and row captured by the render state, but a
        // different generation. A reused node address must not make this stale
        // flattened highlight match.
        let mut hl = Flattened {
            chunks: Vec::new(),
            top_x: 2,
            bot_x: 4,
        };
        hl.chunks.push(FlattenedChunk {
            node: pin.node,
            serial: live_serial ^ 1,
            start: pin.y,
            end: pin.y + 1,
        });

        state.update_highlights_flattened(42, core::slice::from_ref(&hl));
        {
            assert_eq!(0, state.row_data.highlights[0].len());
            assert!(!state.row_data.dirty[0]);
            assert_eq!(Dirty::False, state.dirty);
        }

        // The same chunk is accepted once its copied serial also matches.
        hl.chunks[0].serial = live_serial;
        state.update_highlights_flattened(42, core::slice::from_ref(&hl));
        {
            let row_highlights = &state.row_data.highlights[0];
            assert_eq!(1, row_highlights.len());
            assert_eq!(42, row_highlights[0].tag);
            assert_eq!([2, 4], row_highlights[0].range);
            assert!(state.row_data.dirty[0]);
            assert_eq!(Dirty::Partial, state.dirty);
        }
    }

    #[test]
    fn dirty_row_resets_highlights() {
        let mut t = term(10, 3);
        write(&mut t, b"ABC");

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Reset dirty state
        state.dirty = Dirty::False;
        state.row_data.dirty.fill(false);

        // Manually add a highlight to row 0
        state.row_data.highlights[0].push(Highlight { tag: 1, range: [0, 2] });

        // Verify we have a highlight
        assert_eq!(1, state.row_data.highlights[0].len());

        // Write to row 0 to make it dirty
        write(&mut t, b"\x1b[H"); // Move to home
        write(&mut t, b"X");
        state.update(&mut t);

        // Verify the highlight was reset on the dirty row
        assert_eq!(0, state.row_data.highlights[0].len());
    }

    /// Writes lines "0", "1", ... up to `n - 1` into the terminal, each
    /// followed by a newline, so the cursor ends on an empty row below them.
    fn test_write_numbered_lines(t: &mut Terminal, n: usize) {
        let mut s = t.vt_stream();
        for i in 0..n {
            s.next_slice(format!("{i}\r\n").as_bytes());
        }
        s.deinit();
    }

    /// Returns the number written on the row at `row_data` index `idx` by
    /// test_write_numbered_lines, or None if the row is empty.
    fn test_row_number(state: &RenderState, idx: usize) -> Option<usize> {
        let cells = &state.row_data.cells[idx].raw;
        let mut result: Option<usize> = None;
        for cell in cells {
            let cp = cell.codepoint();
            if cp < '0' as u32 || cp > '9' as u32 {
                break;
            }
            result = Some(result.unwrap_or(0) * 10 + (cp - '0' as u32) as usize);
        }
        result
    }

    #[test]
    fn overscan_zero_request_unchanged() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));
        test_write_numbered_lines(&mut t, 50);

        let mut state = RenderState::empty();

        for scrolled in [false, true] {
            if scrolled {
                t.scroll_viewport(ScrollViewport::Delta(-5));
            }
            state.update(&mut t);
            assert_eq!(10, state.row_data.len());
            assert_eq!(0, state.row_data_range().start);
            assert_eq!(10, state.row_data_range().end);
            assert!(state.overscan.eql(Overscan::default()));
            assert_eq!(0, state.viewport_start());
        }

        // Viewport row 0 is at index 0.
        assert_eq!(36, test_row_number(&state, 0).unwrap());
    }

    #[test]
    fn overscan_row_data_layout() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));

        // Screen rows 0..49 hold "0".."49" and screen row 50 is the empty
        // cursor row. The active area (and bottom viewport) is 41..50.
        test_write_numbered_lines(&mut t, 50);

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 3, below: 2 };

        // Viewport follows the active area: nothing exists below.
        {
            state.update(&mut t);
            assert!(state.overscan.eql(Overscan { above: 3, below: 0 }));
            assert_eq!(15, state.row_data.len());
            assert_eq!(0, state.row_data_range().start);
            assert_eq!(13, state.row_data_range().end);
            assert_eq!(3, state.viewport_start());
            assert_eq!(-3, state.viewport_y(0));
            assert_eq!(0, state.viewport_y(3));

            // Viewport row 0 matches a state with no request.
            let mut plain = RenderState::empty();
            plain.update(&mut t);
            let vp = state.viewport_start();
            for y in 0..state.rows as usize {
                let a = state.row_data.get(vp + y);
                let b = plain.row_data.get(y);
                assert_eq!(b.pin.node, a.pin.node);
                assert_eq!(b.pin.y, a.pin.y);
                assert_eq!(b.raw, a.raw);
                for (bc, ac) in b.cells.raw.iter().zip(a.cells.raw.iter()) {
                    assert_eq!(bc.to_bits(), ac.to_bits());
                }
            }
            assert_eq!(41, test_row_number(&state, vp).unwrap());
            assert_eq!(38, test_row_number(&state, 0).unwrap());
            assert_eq!(40, test_row_number(&state, vp - 1).unwrap());
        }

        // Scrolled into history: both sides are fully captured.
        {
            t.scroll_viewport(ScrollViewport::Delta(-5));
            state.update(&mut t);
            assert!(state.overscan.eql(Overscan { above: 3, below: 2 }));
            assert_eq!(15, state.row_data.len());
            assert_eq!(0, state.row_data_range().start);
            assert_eq!(15, state.row_data_range().end);

            let vp = state.viewport_start();
            let rows = state.rows as usize;
            assert_eq!(36, test_row_number(&state, vp).unwrap());
            assert_eq!(35, test_row_number(&state, vp - 1).unwrap());
            assert_eq!(46, test_row_number(&state, vp + rows).unwrap());
            assert_eq!(47, test_row_number(&state, vp + rows + 1).unwrap());
        }

        // At the top of history: nothing exists above.
        {
            t.scroll_viewport(ScrollViewport::Top);
            state.update(&mut t);
            assert_eq!(0, state.overscan.above);
            assert_eq!(2, state.overscan.below);
            assert_eq!(state.viewport_start(), state.row_data_range().start);
            assert_eq!(15, state.row_data.len());
            assert_eq!(0, test_row_number(&state, state.viewport_start()).unwrap());

            t.scroll_viewport(ScrollViewport::Row(1));
            state.update(&mut t);
            assert_eq!(1, state.overscan.above);
            assert_eq!(2, state.row_data_range().start);
            assert_eq!(15, state.row_data.len());
            assert_eq!(0, test_row_number(&state, 2).unwrap());
            assert_eq!(1, test_row_number(&state, 3).unwrap());
        }

        // Partially scrolled: fewer rows below than requested.
        {
            t.scroll_viewport(ScrollViewport::Bottom);
            t.scroll_viewport(ScrollViewport::Delta(-1));
            state.update(&mut t);
            assert!(state.overscan.eql(Overscan { above: 3, below: 1 }));
            assert_eq!(14, state.row_data_range().end);
            assert_eq!(15, state.row_data.len());
        }
    }

    #[test]
    fn overscan_request_change_forces_redraw() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));
        test_write_numbered_lines(&mut t, 50);

        let mut state = RenderState::empty();
        state.update(&mut t);
        state.clean();

        // No change is not dirty.
        state.update(&mut t);
        assert_eq!(Dirty::False, state.dirty);

        state.overscan_request = Overscan { above: 4, below: 1 };
        state.update(&mut t);
        assert_eq!(Dirty::Full, state.dirty);
        assert_eq!(15, state.row_data.len());
        assert_eq!(4, state.viewport_start());
        assert_eq!(41, test_row_number(&state, 4).unwrap());

        // And back to nothing.
        state.clean();
        state.overscan_request = Overscan::default();
        state.update(&mut t);
        assert_eq!(Dirty::Full, state.dirty);
        assert_eq!(10, state.row_data.len());
        assert_eq!(41, test_row_number(&state, 0).unwrap());
    }

    #[test]
    fn overscan_row_ids_stable() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));
        test_write_numbered_lines(&mut t, 50);

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 3, below: 2 };
        state.update(&mut t);

        // Record all populated ids.
        let range = state.row_data_range();
        assert_eq!(0, range.start);
        assert_eq!(13, range.end);
        let ids: Vec<RowId> = (range.start..range.end).map(|i| state.row_data.get(i).id()).collect();
        assert_eq!(13, ids.len());

        // Ids within one update are distinct.
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert!(!a.eql(*b));
            }
        }

        // Write a line while following the active area. Every row moves up
        // one index and the top row is no longer captured.
        test_write_numbered_lines(&mut t, 1);
        state.update(&mut t);
        assert_eq!(13, state.row_data_range().end);
        for (i, id) in ids[1..].iter().enumerate() {
            assert!(id.eql(state.row_data.get(i).id()));
        }

        // Scroll up one row: the recorded ids are back at their original
        // indices, and one more row is captured below.
        t.scroll_viewport(ScrollViewport::Delta(-1));
        state.update(&mut t);
        assert_eq!(14, state.row_data_range().end);
        for (i, id) in ids.iter().enumerate() {
            assert!(id.eql(state.row_data.get(i).id()));
        }
    }

    #[test]
    fn overscan_row_ids_and_dirty_on_in_place_rewrite() {
        let mut t = term(10, 5);

        // Alternate screen has no scrollback, so scrolling a region rewrites
        // rows in place (eraseRowBounded) rather than moving the viewport.
        write(&mut t, b"\x1b[?1049h");
        write(&mut t, b"0\r\n1\r\n2\r\n3\r\n4");
        write(&mut t, b"\x1b[2;5r\x1b[5;2H");

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 2, below: 2 };
        state.update(&mut t);
        assert!(state.overscan.eql(Overscan::default()));

        let vp = state.viewport_start();
        let mut ids: [RowId; 5] = [RowId { serial: 0, y: 0 }; 5];
        let mut nums: [Option<usize>; 5] = [None; 5];
        for (k, i) in (vp..vp + 5).enumerate() {
            ids[k] = state.row_data.get(i).id();
            nums[k] = test_row_number(&state, i);
        }
        state.clean();

        // Scroll the region up one row.
        write(&mut t, b"\r\n5");
        state.update(&mut t);
        assert!(state.dirty != Dirty::False);

        // The id contract: a row with an unchanged id and no dirty mark has
        // unchanged content, and every row whose content changed is dirty.
        for (k, i) in (vp..vp + 5).enumerate() {
            let row = state.row_data.get(i);
            let new_num = test_row_number(&state, i);
            assert_eq!(if i == vp { 0 } else { i - vp + 1 }, new_num.unwrap());
            if ids[k].eql(row.id()) && !row.dirty {
                assert_eq!(nums[k], new_num);
            }
            if nums[k] != new_num {
                assert!(row.dirty);
            }
        }

        // At the time of writing, rotating rows in place invalidates the
        // page serial (PageList.invalidateNodeLayout), so no id survives.
        for (k, i) in (vp..vp + 5).enumerate() {
            assert!(!ids[k].eql(state.row_data.get(i).id()));
        }
    }

    #[test]
    fn overscan_cursor_in_overscan_row() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));

        // The cursor is on the last active row.
        test_write_numbered_lines(&mut t, 50);

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 0, below: 2 };

        t.scroll_viewport(ScrollViewport::Delta(-1));
        state.update(&mut t);
        assert_eq!(1, state.overscan.below);
        assert!(state.cursor.viewport.is_none());

        t.scroll_viewport(ScrollViewport::Bottom);
        state.update(&mut t);
        assert_eq!(state.rows - 1, state.cursor.viewport.unwrap().y);
        assert_eq!(0, state.cursor.viewport.unwrap().x);
    }

    #[test]
    fn overscan_selection_on_overscan_rows() {
        let mut t = term_scrollback(10, 10, Some(1_000_000));
        test_write_numbered_lines(&mut t, 50);

        // Select the last two active rows.
        let sel = Selection::init(active_pin(&t, 0, 8), active_pin(&t, 2, 9), false);
        screen(&mut t).select(Some(sel)).unwrap();

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 0, below: 2 };

        // Scroll so the last active row is overscan-below.
        t.scroll_viewport(ScrollViewport::Delta(-1));
        state.update(&mut t);
        assert_eq!(1, state.overscan.below);

        let vp = state.viewport_start();
        let rows = state.rows as usize;
        let sels = &state.row_data.selection;
        assert_eq!(Some([0, 9]), sels[vp + rows - 1]);
        assert_eq!(Some([0, 2]), sels[vp + rows]);
        assert!(sels[vp + rows - 2].is_none());
    }

    #[test]
    fn placeholders_runs() {
        if !build_options::KITTY_GRAPHICS {
            return;
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        struct Run {
            y: usize,
            x: CellCountInt,
            width: CellCountInt,
            image_id: u32,
            placement_id: u32,
            row: u32,
            col: u32,
        }

        fn run(x: CellCountInt, width: CellCountInt) -> Run {
            Run {
                x,
                width,
                ..Default::default()
            }
        }

        // Each case writes `input` to a new terminal and expects exactly
        // these placeholder runs, in order. No images exist, so none of them
        // have anything to draw.
        let cases: Vec<(&str, Vec<Run>)> = vec![
            // No placeholders.
            ("hello\r\nworld\r\n1\r\n2", vec![]),
            // A single cell.
            ("\u{10EEEE}\u{0305}\u{0305}", vec![run(0, 1)]),
            // Two runs because the column jumps.
            (
                "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030E}",
                vec![run(0, 1), Run { col: 2, ..run(1, 1) }],
            ),
            // Explicit rows and columns that continue the run.
            (
                "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\u{10EEEE}\u{0305}\u{030E}",
                vec![run(0, 3)],
            ),
            // Cells without a column continue the run.
            ("\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{10EEEE}\u{0305}", vec![run(0, 3)]),
            // Plain cells with no style or grapheme.
            ("\u{10EEEE}\u{10EEEE}\u{10EEEE}", vec![run(0, 3)]),
            // Styled plain cells, which are found through their style run.
            (
                "\x1b[38;5;42mA\u{10EEEE}\u{0305}\u{030D}\u{10EEEE}\u{10EEEE}B\u{10EEEE}\u{10EEEE}",
                vec![
                    Run {
                        image_id: 42,
                        col: 1,
                        ..run(1, 3)
                    },
                    Run {
                        image_id: 42,
                        ..run(5, 2)
                    },
                ],
            ),
            // A run ends at a cell that isn't a placeholder.
            ("\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}ABC", vec![run(0, 2)]),
            // A run starting in the middle of the row.
            ("ABC\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}", vec![run(3, 2)]),
            // The image ID as a palette foreground color.
            (
                "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}",
                vec![Run {
                    image_id: 42,
                    ..run(0, 1)
                }],
            ),
            // The third diacritic sets the high 8 bits of the image ID. A
            // cell without it continues the run, and a different one breaks
            // it.
            (
                "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{030E}\u{10EEEE}\u{0305}\u{10EEEE}\u{0305}\u{030E}\u{0305}",
                vec![
                    Run {
                        image_id: 42 | (2 << 24),
                        ..run(0, 2)
                    },
                    Run {
                        image_id: 42,
                        col: 2,
                        ..run(2, 1)
                    },
                ],
            ),
            // The placement ID as the underline color. A different placement
            // ID breaks the run.
            (
                "\x1b[38;5;42m\x1b[58;5;21m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\x1b[58;5;22m\u{10EEEE}",
                vec![
                    Run {
                        image_id: 42,
                        placement_id: 21,
                        ..run(0, 2)
                    },
                    Run {
                        image_id: 42,
                        placement_id: 22,
                        ..run(2, 1)
                    },
                ],
            ),
            // RGB colors for both IDs, across several rows.
            (
                "\x1b[38;2;1;2;3m\x1b[58;2;0;0;7m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\r\nx\r\n\u{10EEEE}\u{030D}\u{0305}\u{10EEEE}\u{10EEEE}",
                vec![
                    Run {
                        y: 0,
                        image_id: 0x010203,
                        placement_id: 7,
                        ..run(0, 2)
                    },
                    Run {
                        y: 2,
                        image_id: 0x010203,
                        placement_id: 7,
                        row: 1,
                        ..run(0, 3)
                    },
                ],
            ),
        ];

        for (input, runs) in cases {
            let mut t = term(10, 5);
            t.modes.set(Mode::GraphemeCluster, true);
            write(&mut t, input.as_bytes());

            let mut state = RenderState::empty();
            state.update(&mut t);

            let mut actual: Vec<Run> = Vec::new();
            for (y, row_runs) in state.row_data.placeholders.iter().enumerate() {
                for r in row_runs {
                    actual.push(Run {
                        y,
                        x: r.x,
                        width: r.width,
                        image_id: r.image_id,
                        placement_id: r.placement_id,
                        row: r.row,
                        col: r.col,
                    });
                }
            }
            assert_eq!(runs, actual, "input={input:?}");

            let mut it = state.image_placement_iterator();
            assert!(it.next().is_none());
        }
    }

    #[test]
    fn placements_generation_zero_without_images() {
        let mut t = term(10, 5);
        t.print_string("hello").unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(0, state.image_placements_generation());
        t.print_string("world").unwrap();
        state.update(&mut t);
        assert_eq!(0, state.image_placements_generation());
    }

    #[test]
    fn image_layer_from_z_ranges() {
        // Not a Zig test: the doc examples of `imageLayerFromZ`.
        assert_eq!(ImageLayer::BelowText, image_layer_from_z(-1));
        assert_eq!(ImageLayer::AboveText, image_layer_from_z(0));
        assert_eq!(ImageLayer::BelowBackground, image_layer_from_z(i32::MIN));
    }
}

/// The Kitty image tests (Zig: the `placeholders:`, `fixed:` and
/// `placements_generation:` tests that use the image storage).
#[cfg(test)]
mod kitty_image_tests {
    use super::*;
    use crate::terminal::kitty::graphics_command::{DeleteAction, DeleteId, Format};
    use crate::terminal::kitty::graphics_image::{Data as ImageData, Image};
    use crate::terminal::kitty::graphics_storage::{Location, Placement as StoragePlacement, Relative};
    use crate::terminal::screen_set::ScreenPtr;
    use crate::terminal::sgr;
    use crate::terminal::terminal::{Options as TerminalOptions, ScrollViewport, SwitchScreenMode};

    fn term(cols: CellCountInt, rows: CellCountInt) -> Terminal {
        Terminal::init(TerminalOptions::new(cols, rows)).unwrap()
    }

    fn term_unlimited(cols: CellCountInt, rows: CellCountInt) -> Terminal {
        Terminal::init(TerminalOptions {
            max_scrollback_bytes: None,
            ..TerminalOptions::new(cols, rows)
        })
        .unwrap()
    }

    fn key(image_id: u32, id: u32) -> PlacementKey {
        PlacementKey {
            image_id,
            placement_id: PlacementId {
                tag: PlacementTag::External,
                id,
            },
        }
    }

    /// Adds a complete `w` by `h` RGB image with the given ID.
    fn add_image(t: &mut Terminal, image_id: u32, w: u32, h: u32) {
        let mut s: ScreenPtr = t.screens.active;
        let s = &mut *s;
        s.kitty_images
            .add_image(
                &mut s.pages,
                Image {
                    id: image_id,
                    width: w,
                    height: h,
                    format: Format::Rgb,
                    data: ImageData::Complete(vec![0u8; (w * h * 3) as usize]),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn add_placement(t: &mut Terminal, image_id: u32, placement_id: u32, p: StoragePlacement) {
        let mut s: ScreenPtr = t.screens.active;
        let s = &mut *s;
        s.kitty_images.add_placement(&mut s.pages, image_id, placement_id, p).unwrap();
    }

    fn add_virtual(t: &mut Terminal, image_id: u32, placement_id: u32, columns: u32, rows: u32) {
        add_placement(
            t,
            image_id,
            placement_id,
            StoragePlacement {
                location: Location::Virtual,
                columns,
                rows,
                ..StoragePlacement::new(Location::Virtual)
            },
        );
    }

    fn placeholder_colors(t: &mut Terminal, fg: u8, underline: u8) {
        t.set_attribute(sgr::Attribute::Fg256(fg)).unwrap();
        t.set_attribute(sgr::Attribute::UnderlineColor256(underline)).unwrap();
    }

    /// Adds a complete 1 by 1 pixel Kitty image with the given ID to the
    /// active screen, for the `fixed:` tests.
    fn test_fixed_image(t: &mut Terminal, image_id: u32) {
        add_image(t, image_id, 1, 1);
    }

    /// Adds a fixed Kitty placement of image 1 at a cell of the active area,
    /// for the `fixed:` tests.
    fn test_fixed_placement(
        t: &mut Terminal,
        placement_id: u32,
        x: CellCountInt,
        y: CellCountInt,
        columns: u32,
        rows: u32,
    ) {
        let mut s: ScreenPtr = t.screens.active;
        let pin = s
            .pages
            .pin(point::Point::Active(point::Coordinate { x, y: y as u32 }))
            .unwrap();
        let pin = s.pages.track_pin(pin).unwrap();
        add_placement(
            t,
            1,
            placement_id,
            StoragePlacement {
                location: Location::Pin(pin),
                columns,
                rows,
                ..StoragePlacement::new(Location::Virtual)
            },
        );
    }

    /// Adds a Kitty placement of image 1 relative to another placement of
    /// image 1, for the `fixed: relative` tests.
    fn test_relative_placement(
        t: &mut Terminal,
        placement_id: u32,
        parent_id: u32,
        horizontal_offset: i32,
        vertical_offset: i32,
        columns: u32,
        rows: u32,
    ) {
        add_placement(
            t,
            1,
            placement_id,
            StoragePlacement {
                location: Location::Relative(Relative {
                    parent: key(1, parent_id),
                    horizontal_offset,
                    vertical_offset,
                }),
                columns,
                rows,
                ..StoragePlacement::new(Location::Virtual)
            },
        );
    }

    /// Returns the only image placement of a render state, failing the test
    /// if there isn't exactly one.
    fn test_only_placement(state: &RenderState) -> ImagePlacement {
        let mut it = state.image_placement_iterator();
        let p = it.next().expect("TestExpectedPlacement");
        assert!(it.next().is_none(), "TestUnexpectedPlacement");
        p
    }

    /// Returns the image placement of a render state with the given
    /// placement ID, failing the test if there isn't exactly one.
    fn test_placement_by_id(state: &RenderState, placement_id: u32) -> ImagePlacement {
        let mut result: Option<ImagePlacement> = None;
        for p in state.image_placement_iterator() {
            if p.protocol.kitty().placement_id != placement_id {
                continue;
            }
            assert!(result.is_none(), "TestUnexpectedPlacement");
            result = Some(p);
        }
        result.expect("TestExpectedPlacement")
    }

    /// Returns the number of placements the iterator returns.
    fn test_placement_count(state: &RenderState) -> usize {
        state.image_placement_iterator().count()
    }

    fn first_page_rows(t: &Terminal) -> usize {
        // SAFETY: the first node is live.
        unsafe { t.screens.active.pages.pages.first.unwrap().as_ref().capacity().rows as usize }
    }

    fn mark_active_row_dirty(t: &Terminal, y: u32) {
        t.screens
            .active
            .pages
            .pin(point::Point::Active(point::Coordinate { x: 0, y }))
            .unwrap()
            .mark_dirty();
    }

    #[test]
    fn placeholders_resolved_virtual_placements() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        t.modes.set(Mode::GraphemeCluster, true);

        add_image(&mut t, 1, 10, 10);
        add_virtual(&mut t, 1, 1, 2, 2);
        placeholder_colors(&mut t, 1, 1);
        t.screens.active.cursor_absolute(3, 1);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}").unwrap();
        t.screens.active.cursor_absolute(3, 2);
        t.print_string("\u{10EEEE}\u{030D}\u{0305}\u{10EEEE}").unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Each run draws half of the image, like the renderer would.
        let mut it = state.image_placement_iterator();
        for y in 0..2u32 {
            let p = it.next().unwrap();
            assert_eq!(KittyPlacementKind::Virtual, p.protocol.kitty().kind);
            assert_eq!(1, p.image_id);
            assert_eq!(1, p.protocol.kitty().placement_id);
            assert_eq!(3, p.x);
            assert_eq!((y + 1) as i32, p.y);
            assert_eq!(2, p.cols);
            assert_eq!(1, p.rows);
            assert_eq!(20, p.width);
            assert_eq!(10, p.height);
            assert_eq!(0, p.source_x);
            assert_eq!(y * 5, p.source_y);
            assert_eq!(10, p.source_width);
            assert_eq!(5, p.source_height);
            assert_eq!(-1, p.z);
            assert_eq!(ImageLayer::BelowText, p.layer);
            assert_eq!(ImageProtocol::Kitty, p.protocol.tag());
            assert_eq!(
                t.screens.active.kitty_images.image_by_id(1).unwrap().generation,
                p.image_generation
            );
        }
        assert!(it.next().is_none());
    }

    #[test]
    fn placeholders_only_dirty_rows_are_resolved_again() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        t.modes.set(Mode::GraphemeCluster, true);

        add_image(&mut t, 1, 10, 10);
        add_virtual(&mut t, 1, 1, 2, 2);
        placeholder_colors(&mut t, 1, 1);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\r\n").unwrap();
        t.print_string("\u{10EEEE}\u{030D}\u{0305}\u{10EEEE}").unwrap();

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Overwrite the geometry with a marker value, so we can tell when a
        // run is looked up again.
        let poison = 12345;
        for runs in &mut state.row_data.placeholders[0..2] {
            assert_eq!(1, runs.len());
            assert_eq!(PlaceholderState::Visible, runs[0].state);
            runs[0].geometry.width = poison;
        }

        // An update with no changes looks up nothing.
        state.update(&mut t);
        assert_eq!(poison, state.row_data.placeholders[0][0].geometry.width);
        assert_eq!(poison, state.row_data.placeholders[1][0].geometry.width);

        // Only the dirty row is rebuilt and looked up again.
        mark_active_row_dirty(&t, 1);
        state.update(&mut t);
        assert_eq!(poison, state.row_data.placeholders[0][0].geometry.width);
        assert_eq!(20, state.row_data.placeholders[1][0].geometry.width);

        // A change to the image storage looks up every row again.
        t.screens.active.kitty_images.mark_mutated();
        state.update(&mut t);
        assert_eq!(20, state.row_data.placeholders[0][0].geometry.width);

        // So does a cell size change. With wider cells, the image is
        // centered in the placement grid.
        state.row_data.placeholders[0][0].geometry.width = poison;
        t.width_px = 200;
        state.update(&mut t);
        assert_eq!(20, state.row_data.placeholders[0][0].geometry.width);
        assert_eq!(10, state.row_data.placeholders[0][0].geometry.offset_x);
    }

    #[test]
    fn placeholders_relative_placement_rooted_at_a_virtual_placement() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        t.modes.set(Mode::GraphemeCluster, true);

        add_image(&mut t, 1, 10, 10);
        add_virtual(&mut t, 1, 1, 2, 2);
        placeholder_colors(&mut t, 1, 1);

        // Placeholder cells at (3, 1) and (1, 2). Relative placements
        // attached to the virtual placement are positioned from the smallest
        // column and row, (1, 1), even though no cell is there.
        t.screens.active.cursor_absolute(3, 1);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}").unwrap();
        t.screens.active.cursor_absolute(1, 2);
        t.print_string("\u{10EEEE}\u{030D}\u{0305}").unwrap();

        add_placement(
            &mut t,
            1,
            2,
            StoragePlacement {
                location: Location::Relative(Relative {
                    parent: key(1, 1),
                    horizontal_offset: 1,
                    vertical_offset: 2,
                }),
                columns: 1,
                rows: 1,
                z: 5,
                ..StoragePlacement::new(Location::Virtual)
            },
        );

        let mut state = RenderState::empty();
        state.update(&mut t);

        let mut it = state.image_placement_iterator();
        assert_eq!(KittyPlacementKind::Virtual, it.next().unwrap().protocol.kitty().kind);
        assert_eq!(KittyPlacementKind::Virtual, it.next().unwrap().protocol.kitty().kind);
        let child = it.next().unwrap();
        assert_eq!(KittyPlacementKind::Relative, child.protocol.kitty().kind);
        assert_eq!(1, child.image_id);
        assert_eq!(2, child.protocol.kitty().placement_id);
        assert_eq!(ImageLayer::AboveText, child.layer);
        assert_eq!(2, child.x);
        assert_eq!(3, child.y);
        assert_eq!(1, child.cols);
        assert_eq!(1, child.rows);
        assert_eq!(10, child.width);
        assert_eq!(10, child.height);
        assert_eq!(5, child.z);
        assert!(it.next().is_none());

        // A new placeholder cell further left moves the child, even
        // though the storage didn't change.
        t.screens.active.cursor_absolute(0, 3);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}").unwrap();
        state.update(&mut t);
        let mut it = state.image_placement_iterator();
        for _ in 0..3 {
            assert_eq!(KittyPlacementKind::Virtual, it.next().unwrap().protocol.kitty().kind);
        }
        let moved = it.next().unwrap();
        assert_eq!(1, moved.x);
        assert_eq!(3, moved.y);
        assert!(it.next().is_none());

        // Without placeholder cells in the viewport the child isn't drawn.
        t.erase_display(crate::terminal::csi::EraseDisplay::Complete, false);
        state.update(&mut t);
        assert!(state.image_placement_iterator().next().is_none());
    }

    #[test]
    fn fixed_pin_placement_in_viewport() {
        let mut t = term(10, 10);
        t.width_px = 100;
        t.height_px = 100;

        test_fixed_image(&mut t, 1);
        let mut s: ScreenPtr = t.screens.active;
        let pin = s
            .pages
            .pin(point::Point::Active(point::Coordinate { x: 2, y: 1 }))
            .unwrap();
        let pin = s.pages.track_pin(pin).unwrap();
        add_placement(
            &mut t,
            1,
            7,
            StoragePlacement {
                location: Location::Pin(pin),
                columns: 3,
                rows: 2,
                x_offset: 4,
                y_offset: 3,
                z: -5,
                ..StoragePlacement::new(Location::Virtual)
            },
        );

        let mut state = RenderState::empty();
        state.update(&mut t);

        let p = test_only_placement(&state);
        let storage = &t.screens.active.kitty_images;
        let image = storage.image_by_id(1).unwrap();
        let sp = storage.placements.get(&key(1, 7)).unwrap();
        let info = TerminalInfo::from_terminal(&t);
        let grid = sp.grid_size(image, &info);
        let dest = sp.pixel_size(image, &info);
        let offset = sp.cell_offset(&info);
        let source = sp.source_rect(image);
        assert_eq!(KittyPlacementKind::Fixed, p.protocol.kitty().kind);
        assert_eq!(7, p.protocol.kitty().placement_id);
        assert_eq!(1, p.image_id);
        assert_eq!(image.generation, p.image_generation);
        assert_eq!(2, p.x);
        assert_eq!(1, p.y);
        assert_eq!(3, p.cols);
        assert_eq!(2, p.rows);
        assert_eq!(grid.cols, p.cols);
        assert_eq!(grid.rows, p.rows);
        assert_eq!(4, p.offset_x);
        assert_eq!(3, p.offset_y);
        assert_eq!(offset.x, p.offset_x);
        assert_eq!(offset.y, p.offset_y);
        assert_eq!(dest.width, p.width);
        assert_eq!(dest.height, p.height);
        assert_eq!(source.x, p.source_x);
        assert_eq!(source.y, p.source_y);
        assert_eq!(source.width, p.source_width);
        assert_eq!(source.height, p.source_height);
        assert_eq!(-5, p.z);
        assert_eq!(ImageLayer::BelowText, p.layer);
    }

    #[test]
    fn fixed_scrolled_partially_above_the_viewport() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 1, 0, 2, 3);

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 1, below: 0 };
        state.update(&mut t);
        {
            let p = test_only_placement(&state);
            assert_eq!(1, p.x);
            assert_eq!(0, p.y);
        }

        // Two lines of output move the placement two rows up. Its top row is
        // now above the one overscan row, and it is still reported whole.
        t.set_cursor_pos(5, 1);
        t.index().unwrap();
        t.index().unwrap();
        state.update(&mut t);
        assert_eq!(1, state.overscan.above);
        {
            let p = test_only_placement(&state);
            assert_eq!(1, p.x);
            assert_eq!(-2, p.y);
            assert_eq!(2, p.cols);
            assert_eq!(3, p.rows);
            assert_eq!(20, p.width);
            assert_eq!(30, p.height);
        }

        // Scrolling the viewport up a row moves it back down.
        t.scroll_viewport(ScrollViewport::Delta(-1));
        state.update(&mut t);
        {
            let p = test_only_placement(&state);
            assert_eq!(-1, p.y);
            assert_eq!(30, p.height);
        }

        // Three lines up, only the overscan row shows it.
        t.scroll_viewport(ScrollViewport::Bottom);
        t.index().unwrap();
        state.update(&mut t);
        assert_eq!(-3, test_only_placement(&state).y);

        // Four lines up, it doesn't reach the captured rows.
        t.index().unwrap();
        state.update(&mut t);
        assert!(state.image_placement_iterator().next().is_none());
    }

    #[test]
    fn fixed_pin_on_a_page_above_the_captured_rows() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        let first = t.screens.active.pages.pages.first.unwrap();
        let page_rows = first_page_rows(&t);

        // Scroll the first row far enough that the viewport is on the next
        // page. The first row is then `distance` rows above the viewport.
        test_fixed_image(&mut t, 1);
        let distance = page_rows + 6;
        t.set_cursor_pos(5, 1);
        for _ in 0..distance {
            t.index().unwrap();
        }
        // SAFETY: the first node is live.
        let second = unsafe { first.as_ref().next.unwrap() };
        assert!(t.screens.active.pages.get_top_left(point::Tag::Viewport).node == second);

        // Placements on the first row: one tall enough to reach the second
        // row of the viewport, and one that ends just above the viewport.
        {
            let top = Pin {
                node: first,
                y: 0,
                x: 1,
                garbage: false,
            };
            let mut s: ScreenPtr = t.screens.active;
            let p1 = s.pages.track_pin(top).unwrap();
            add_placement(
                &mut t,
                1,
                1,
                StoragePlacement {
                    location: Location::Pin(p1),
                    columns: 1,
                    rows: (distance + 2) as u32,
                    ..StoragePlacement::new(Location::Virtual)
                },
            );
            let p2 = s.pages.track_pin(top).unwrap();
            add_placement(
                &mut t,
                1,
                2,
                StoragePlacement {
                    location: Location::Pin(p2),
                    columns: 1,
                    rows: distance as u32,
                    ..StoragePlacement::new(Location::Virtual)
                },
            );
        }

        let mut state = RenderState::empty();
        state.update(&mut t);

        // The viewport is one chunk on the second page, so the first page is
        // only found by the walk up.
        assert_eq!(1, state.kitty.chunks.len());
        assert!(state.kitty.chunks[0].node != first);

        let p = test_only_placement(&state);
        assert_eq!(1, p.protocol.kitty().placement_id);
        assert_eq!(1, p.x);
        assert_eq!(-(distance as i32), p.y);
        assert_eq!((distance + 2) as u32, p.rows);
    }

    #[test]
    fn fixed_region_scroll_moves_the_placement_without_a_generation_change() {
        let mut t = term(10, 6);
        t.width_px = 100;
        t.height_px = 60;
        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 2, 3, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(3, test_only_placement(&state).y);

        // Mark the template so we can tell whether the roots are rebuilt.
        let poison = 12345;
        state.kitty.roots[0].placement.width = poison;

        // Scroll rows 2 to 5 (1-based) up by one with an index at the bottom
        // margin.
        let generation = t.screens.active.kitty_images.generation;
        t.set_top_and_bottom_margin(2, 5);
        t.set_cursor_pos(5, 1);
        t.index().unwrap();
        assert_eq!(generation, t.screens.active.kitty_images.generation);
        state.update(&mut t);
        {
            let p = test_only_placement(&state);
            assert_eq!(2, p.x);
            assert_eq!(2, p.y);
            assert_eq!(poison, p.width);
        }

        // A storage change rebuilds the roots.
        t.screens.active.kitty_images.mark_mutated();
        state.update(&mut t);
        assert_eq!(10, test_only_placement(&state).width);
    }

    #[test]
    fn fixed_viewport_scroll_does_not_rebuild_roots() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 2, 1, 2);
        t.set_cursor_pos(5, 1);
        for _ in 0..3 {
            t.index().unwrap();
        }

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(-1, test_only_placement(&state).y);
        let poison = 12345;
        state.kitty.roots[0].placement.width = poison;

        t.scroll_viewport(ScrollViewport::Delta(-2));
        state.update(&mut t);
        let p = test_only_placement(&state);
        assert_eq!(1, p.y);
        assert_eq!(poison, p.width);
    }

    #[test]
    fn fixed_dirty_row_with_only_fixed_placements_skips_the_slow_path() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 1, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        let poison = 12345;
        state.kitty.placed[0].width = poison;

        // Clear the dirty flags so we can see that the row was rebuilt.
        state.row_data.dirty.fill(false);
        mark_active_row_dirty(&t, 3);
        state.update(&mut t);
        assert!(state.row_data.dirty[3]);
        assert_eq!(poison, test_only_placement(&state).width);
    }

    #[test]
    fn fixed_garbage_pin_is_skipped() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        // A placement on the first page, scrolled onto the second page's
        // history, and tall enough to reach the viewport.
        let page_rows = first_page_rows(&t);
        let distance = page_rows + 6;
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 0, 1, (distance + 2) as u32);
        t.set_cursor_pos(5, 1);
        for _ in 0..distance {
            t.index().unwrap();
        }

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(-(distance as i32), test_only_placement(&state).y);

        // Lowering the scrollback limit prunes the first page. The pin moves
        // to the top of the next page, where it would still reach the
        // viewport, and becomes garbage. The storage keeps the placement and
        // its generation until it reaps garbage.
        let generation = t.screens.active.kitty_images.generation;
        t.set_scrollback_max_lines(Some(1));
        let storage = &t.screens.active.kitty_images;
        assert_eq!(generation, storage.generation);
        assert_eq!(1, storage.placements.len());
        let Location::Pin(pin) = storage.placements.values().next().unwrap().location else {
            panic!("expected a pin placement");
        };
        // SAFETY: the tracked pin is live.
        assert!(unsafe { pin.as_ref().garbage });
        state.update(&mut t);
        assert!(state.image_placement_iterator().next().is_none());
    }

    #[test]
    fn fixed_screen_switch_rebuilds_roots() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 3, 2, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(3, test_only_placement(&state).x);

        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();
        state.update(&mut t);
        assert!(state.image_placement_iterator().next().is_none());

        t.switch_screen_mode(SwitchScreenMode::Mode1049, false).unwrap();
        state.update(&mut t);
        let p = test_only_placement(&state);
        assert_eq!(3, p.x);
        assert_eq!(2, p.y);
    }

    #[test]
    fn fixed_relative_child_at_parent_plus_positive_offsets() {
        let mut t = term(10, 10);
        t.width_px = 100;
        t.height_px = 100;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 2, 1, 1, 1);
        test_relative_placement(&mut t, 2, 1, 3, 2, 2, 3);

        let mut state = RenderState::empty();
        state.update(&mut t);

        let parent = test_placement_by_id(&state, 1);
        assert_eq!(KittyPlacementKind::Fixed, parent.protocol.kitty().kind);
        assert_eq!(2, parent.x);
        assert_eq!(1, parent.y);

        let child = test_placement_by_id(&state, 2);
        assert_eq!(KittyPlacementKind::Relative, child.protocol.kitty().kind);
        assert_eq!(1, child.image_id);
        assert_eq!(5, child.x);
        assert_eq!(3, child.y);
        assert_eq!(2, child.cols);
        assert_eq!(3, child.rows);
        assert_eq!(20, child.width);
        assert_eq!(30, child.height);
    }

    #[test]
    fn fixed_relative_child_with_negative_offsets() {
        let mut t = term(10, 10);
        t.width_px = 100;
        t.height_px = 100;

        // The child hangs above and left of its parent at the top-left
        // corner. It is reported whole even though its top-left cell is
        // outside the screen.
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 0, 1, 1);
        test_relative_placement(&mut t, 2, 1, -1, -1, 2, 2);

        let mut state = RenderState::empty();
        state.update(&mut t);

        let child = test_placement_by_id(&state, 2);
        assert_eq!(KittyPlacementKind::Relative, child.protocol.kitty().kind);
        assert_eq!(-1, child.x);
        assert_eq!(-1, child.y);
        assert_eq!(2, child.cols);
        assert_eq!(2, child.rows);
    }

    #[test]
    fn fixed_relative_chain_two_levels_deep() {
        let mut t = term(10, 10);
        t.width_px = 100;
        t.height_px = 100;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 1, 1, 1, 1);
        test_relative_placement(&mut t, 2, 1, 1, 1, 1, 1);
        test_relative_placement(&mut t, 3, 2, 2, 1, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);

        let child = test_placement_by_id(&state, 2);
        assert_eq!(2, child.x);
        assert_eq!(2, child.y);
        let grandchild = test_placement_by_id(&state, 3);
        assert_eq!(KittyPlacementKind::Relative, grandchild.protocol.kitty().kind);
        assert_eq!(4, grandchild.x);
        assert_eq!(3, grandchild.y);
    }

    #[test]
    fn fixed_relative_to_a_parent_above_the_captured_rows() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        let first = t.screens.active.pages.pages.first.unwrap();
        let page_rows = first_page_rows(&t);

        // The parent is on the first row, and the child is far enough below
        // it to be on the second row of the viewport once the parent scrolls
        // onto an earlier page.
        let distance = page_rows + 6;
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 1, 0, 1, 1);
        test_relative_placement(&mut t, 2, 1, 2, (distance + 1) as i32, 1, 1);
        t.set_cursor_pos(5, 1);
        for _ in 0..distance {
            t.index().unwrap();
        }
        // SAFETY: the first node is live.
        let second = unsafe { first.as_ref().next.unwrap() };
        assert!(t.screens.active.pages.get_top_left(point::Tag::Viewport).node == second);

        let mut state = RenderState::empty();
        state.update(&mut t);

        // Only the child is in the captured rows. The parent's page is only
        // found by the walk up.
        assert_eq!(1, state.kitty.chunks.len());
        assert!(state.kitty.chunks[0].node != first);
        let p = test_only_placement(&state);
        assert_eq!(KittyPlacementKind::Relative, p.protocol.kitty().kind);
        assert_eq!(2, p.protocol.kitty().placement_id);
        assert_eq!(3, p.x);
        assert_eq!(1, p.y);
    }

    #[test]
    fn fixed_relative_to_a_parent_below_the_captured_rows() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        let first = t.screens.active.pages.pages.first.unwrap();
        let page_rows = first_page_rows(&t);

        // Enough output that the active area is on a later page than the top
        // of the scrollback, where the viewport goes.
        t.set_cursor_pos(5, 1);
        for _ in 0..page_rows + 10 {
            t.index().unwrap();
        }
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 1, 0, 1, 1);
        let parent_pin = t
            .screens
            .active
            .pages
            .pin(point::Point::Active(point::Coordinate { x: 1, y: 0 }))
            .unwrap();
        assert!(parent_pin.node != first);
        let parent_y = t
            .screens
            .active
            .pages
            .point_from_pin(point::Tag::Screen, parent_pin)
            .unwrap()
            .coord()
            .y as i32;

        // The child moves up from the parent to the second row of the top of
        // the scrollback.
        test_relative_placement(&mut t, 2, 1, 1, 1 - parent_y, 1, 1);
        t.scroll_viewport(ScrollViewport::Top);

        let mut state = RenderState::empty();
        state.update(&mut t);

        assert_eq!(1, state.kitty.chunks.len());
        assert_eq!(first, state.kitty.chunks[0].node);
        let p = test_only_placement(&state);
        assert_eq!(KittyPlacementKind::Relative, p.protocol.kitty().kind);
        assert_eq!(2, p.x);
        assert_eq!(1, p.y);
    }

    #[test]
    fn fixed_relative_with_a_broken_chain_is_not_drawn() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        // Placement 99 doesn't exist.
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 1, 1, 1, 1);
        test_relative_placement(&mut t, 2, 99, 1, 1, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);

        let p = test_only_placement(&state);
        assert_eq!(1, p.protocol.kitty().placement_id);
        assert_eq!(1, state.kitty.roots.len());
    }

    #[test]
    fn fixed_relative_to_a_parent_with_a_garbage_pin_is_not_drawn() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        // The parent is on the first page, scrolled onto the second page's
        // history. The child is far enough below it to be in the viewport.
        let page_rows = first_page_rows(&t);
        let distance = page_rows + 6;
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 0, 1, 1);
        test_relative_placement(&mut t, 2, 1, 0, (distance + 1) as i32, 1, 1);
        t.set_cursor_pos(5, 1);
        for _ in 0..distance {
            t.index().unwrap();
        }

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(2, test_only_placement(&state).protocol.kitty().placement_id);

        // Pruning the first page makes the parent's pin garbage without a
        // generation change.
        let generation = t.screens.active.kitty_images.generation;
        t.set_scrollback_max_lines(Some(1));
        let storage = &t.screens.active.kitty_images;
        assert_eq!(generation, storage.generation);
        let Location::Pin(pin) = storage.placements.get(&key(1, 1)).unwrap().location else {
            panic!("expected a pin placement");
        };
        // SAFETY: the tracked pin is live.
        assert!(unsafe { pin.as_ref().garbage });
        state.update(&mut t);
        assert!(state.image_placement_iterator().next().is_none());
    }

    #[test]
    fn fixed_relative_moves_with_its_parent_on_a_region_scroll() {
        let mut t = term(10, 6);
        t.width_px = 100;
        t.height_px = 60;
        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 2, 3, 1, 1);
        test_relative_placement(&mut t, 2, 1, 1, 1, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(3, test_placement_by_id(&state, 1).y);
        assert_eq!(4, test_placement_by_id(&state, 2).y);
        let placements_generation = state.image_placements_generation();

        // Scroll rows 2 to 5 (1-based) up by one. Both move up a row, and
        // the storage generation stays the same.
        let generation = t.screens.active.kitty_images.generation;
        t.set_top_and_bottom_margin(2, 5);
        t.set_cursor_pos(5, 1);
        t.index().unwrap();
        assert_eq!(generation, t.screens.active.kitty_images.generation);
        state.update(&mut t);
        {
            let parent = test_placement_by_id(&state, 1);
            assert_eq!(2, parent.x);
            assert_eq!(2, parent.y);
            let child = test_placement_by_id(&state, 2);
            assert_eq!(3, child.x);
            assert_eq!(3, child.y);
        }
        assert!(state.image_placements_generation() != placements_generation);
    }

    #[test]
    fn placeholders_relative_rooted_at_a_virtual_placement_in_an_overscan_row() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        t.modes.set(Mode::GraphemeCluster, true);

        test_fixed_image(&mut t, 1);
        add_virtual(&mut t, 1, 1, 1, 1);
        add_placement(
            &mut t,
            1,
            2,
            StoragePlacement {
                location: Location::Relative(Relative {
                    parent: key(1, 1),
                    horizontal_offset: 1,
                    vertical_offset: 2,
                }),
                columns: 1,
                rows: 1,
                ..StoragePlacement::new(Location::Virtual)
            },
        );

        // A placeholder cell at (3, 0), then one line of output to move it
        // into the overscan row above the viewport.
        placeholder_colors(&mut t, 1, 1);
        t.screens.active.cursor_absolute(3, 0);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}").unwrap();
        t.set_attribute(sgr::Attribute::Unset).unwrap();
        t.set_cursor_pos(5, 1);
        t.index().unwrap();

        let mut state = RenderState::empty();
        state.overscan_request = Overscan { above: 1, below: 0 };
        state.update(&mut t);
        assert_eq!(1, state.overscan.above);

        let mut it = state.image_placement_iterator();
        let parent = it.next().unwrap();
        assert_eq!(KittyPlacementKind::Virtual, parent.protocol.kitty().kind);
        assert_eq!(-1, parent.y);
        let child = it.next().unwrap();
        assert_eq!(KittyPlacementKind::Relative, child.protocol.kitty().kind);
        assert_eq!(4, child.x);
        assert_eq!(1, child.y);
        assert!(it.next().is_none());
    }

    /// Sets up image 1 with a fixed placement (ID 1) at (0, 4) and a virtual
    /// placement (ID 2) with one placeholder run at (3, 1), on a 5x10
    /// terminal.
    fn test_images_generation_scene(t: &mut Terminal) {
        t.width_px = 100;
        t.height_px = 50;
        t.modes.set(Mode::GraphemeCluster, true);

        test_fixed_image(t, 1);
        test_fixed_placement(t, 1, 0, 4, 1, 1);
        add_virtual(t, 1, 2, 1, 1);
        placeholder_colors(t, 1, 2);
        t.screens.active.cursor_absolute(3, 1);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}").unwrap();
        t.set_attribute(sgr::Attribute::Unset).unwrap();
    }

    #[test]
    fn placements_generation_stable_on_a_quiet_update() {
        let mut t = term(10, 5);
        test_images_generation_scene(&mut t);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(2, test_placement_count(&state));
        let generation = state.image_placements_generation();
        assert!(generation != 0);

        state.update(&mut t);
        assert_eq!(generation, state.image_placements_generation());
    }

    #[test]
    fn placements_generation_stable_across_a_dirty_row_with_only_fixed_placements() {
        let mut t = term(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 1, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        let generation = state.image_placements_generation();

        // The slow path would build `placed` again and drop the marker.
        let poison = 12345;
        state.kitty.placed[0].width = poison;

        mark_active_row_dirty(&t, 3);
        state.update(&mut t);
        assert_eq!(generation, state.image_placements_generation());
        assert_eq!(poison, test_only_placement(&state).width);
    }

    #[test]
    fn placements_generation_stable_across_a_dirty_row_that_has_no_placeholders_while_virtual_runs_exist_elsewhere() {
        let mut t = term(10, 5);
        test_images_generation_scene(&mut t);

        // A child of the virtual placement, so that a dirty row builds
        // `placed` again and the comparison with the previous one decides.
        add_placement(
            &mut t,
            1,
            3,
            StoragePlacement {
                location: Location::Relative(Relative {
                    parent: key(1, 2),
                    horizontal_offset: 1,
                    vertical_offset: 1,
                }),
                columns: 1,
                rows: 1,
                ..StoragePlacement::new(Location::Virtual)
            },
        );

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(3, test_placement_count(&state));
        assert!(state.kitty.virtual_anchors > 0);
        let generation = state.image_placements_generation();

        // Mark the row dirty directly. Moving the cursor would also rebuild
        // the placeholder row the cursor leaves.
        state.row_data.dirty.fill(false);
        mark_active_row_dirty(&t, 3);
        state.update(&mut t);
        assert!(state.row_data.dirty[3]);
        assert!(!state.row_data.dirty[1]);
        assert_eq!(generation, state.image_placements_generation());
        assert_eq!(3, test_placement_count(&state));
    }

    #[test]
    fn placements_generation_changes_when_a_placeholder_row_is_overwritten_with_text() {
        let mut t = term(10, 5);
        test_images_generation_scene(&mut t);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(2, test_placement_count(&state));
        let generation = state.image_placements_generation();

        t.screens.active.cursor_absolute(3, 1);
        t.print_string("x").unwrap();
        state.update(&mut t);
        assert!(generation != state.image_placements_generation());

        // Only the fixed placement is left.
        let p = test_only_placement(&state);
        assert_eq!(KittyPlacementKind::Fixed, p.protocol.kitty().kind);
    }

    #[test]
    fn placements_generation_changes_when_a_placeholder_run_is_added() {
        let mut t = term(10, 5);
        test_images_generation_scene(&mut t);

        let mut state = RenderState::empty();
        state.update(&mut t);
        let generation = state.image_placements_generation();

        placeholder_colors(&mut t, 1, 2);
        t.screens.active.cursor_absolute(5, 2);
        t.print_string("\u{10EEEE}\u{0305}\u{0305}").unwrap();
        state.update(&mut t);
        assert!(generation != state.image_placements_generation());
        assert_eq!(3, test_placement_count(&state));
    }

    #[test]
    fn placements_generation_changes_on_viewport_scroll_with_a_fixed_placement() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 2, 1, 2);
        t.set_cursor_pos(5, 1);
        for _ in 0..3 {
            t.index().unwrap();
        }

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(-1, test_only_placement(&state).y);
        let generation = state.image_placements_generation();

        t.scroll_viewport(ScrollViewport::Delta(-2));
        state.update(&mut t);
        assert_eq!(1, test_only_placement(&state).y);
        let scrolled = state.image_placements_generation();
        assert!(generation != scrolled);

        // Stable on a second update with no changes.
        state.update(&mut t);
        assert_eq!(scrolled, state.image_placements_generation());
    }

    #[test]
    fn placements_generation_changes_on_a_storage_mutation() {
        let mut t = term(10, 5);
        test_images_generation_scene(&mut t);

        let mut state = RenderState::empty();
        state.update(&mut t);
        let mut generation = state.image_placements_generation();

        // Add a placement.
        test_fixed_placement(&mut t, 4, 5, 0, 1, 1);
        state.update(&mut t);
        assert!(generation != state.image_placements_generation());
        assert_eq!(3, test_placement_count(&state));
        generation = state.image_placements_generation();

        // Delete it again.
        let mut s: ScreenPtr = t.screens.active;
        let s = &mut *s;
        let info = TerminalInfo::from_terminal(&t);
        s.kitty_images.delete(
            &mut s.pages,
            &info,
            DeleteAction::Id(DeleteId {
                delete: false,
                image_id: 1,
                placement_id: 4,
            }),
        );
        state.update(&mut t);
        assert!(generation != state.image_placements_generation());
        assert_eq!(2, test_placement_count(&state));
        generation = state.image_placements_generation();

        // Any mutation, even one that doesn't move anything.
        t.screens.active.kitty_images.mark_mutated();
        state.update(&mut t);
        assert!(generation != state.image_placements_generation());
    }

    #[test]
    fn placements_generation_changes_on_a_region_scroll_that_moves_a_fixed_placement() {
        let mut t = term(10, 6);
        t.width_px = 100;
        t.height_px = 60;
        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();

        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 2, 3, 1, 1);

        let mut state = RenderState::empty();
        state.update(&mut t);
        assert_eq!(3, test_only_placement(&state).y);
        let generation = state.image_placements_generation();

        let storage_generation = t.screens.active.kitty_images.generation;
        t.set_top_and_bottom_margin(2, 5);
        t.set_cursor_pos(5, 1);
        t.index().unwrap();
        assert_eq!(storage_generation, t.screens.active.kitty_images.generation);
        state.update(&mut t);
        assert_eq!(2, test_only_placement(&state).y);
        assert!(generation != state.image_placements_generation());
    }

    #[test]
    fn fixed_row_index_of_pin_walks_pages_above_and_below() {
        let mut t = term_unlimited(10, 5);
        t.width_px = 100;
        t.height_px = 50;
        let first = t.screens.active.pages.pages.first.unwrap();
        let page_rows = first_page_rows(&t) as isize;

        // Three pages, with the viewport on the middle one, three rows down.
        test_fixed_image(&mut t, 1);
        test_fixed_placement(&mut t, 1, 0, 0, 1, 1);
        t.set_cursor_pos(5, 1);
        for _ in 0..(2 * page_rows + 10) {
            t.index().unwrap();
        }
        // SAFETY: the nodes are live.
        let middle = unsafe { first.as_ref().next.unwrap() };
        let last = unsafe { middle.as_ref().next.unwrap() };
        t.scroll_viewport(ScrollViewport::Top);
        t.scroll_viewport(ScrollViewport::Delta(page_rows + 3));
        assert!(t.screens.active.pages.get_top_left(point::Tag::Viewport).node == middle);

        let mut state = RenderState::empty();
        state.update(&mut t);
        let chunks: &[KittyChunk] = &state.kitty.chunks;
        assert_eq!(1, chunks.len());
        assert_eq!(middle, chunks[0].node);
        assert_eq!(0, chunks[0].index);
        assert_eq!(3, chunks[0].start);
        let captured: isize = 5;
        let pin = |node: NodePtr, y: isize| Pin {
            node,
            y: y as CellCountInt,
            x: 0,
            garbage: false,
        };
        let row_index = kitty::row_index_of_pin;

        // On a captured page, including its rows outside the captured rows.
        assert_eq!(Some(0), row_index(&chunks, pin(middle, 3), captured, 1, 0));
        assert_eq!(Some(-3), row_index(&chunks, pin(middle, 0), captured, 1, 0));
        assert_eq!(Some(7), row_index(&chunks, pin(middle, 10), captured, 1, 0));

        // Above: the last row of the first page is index -4. It is found when
        // the placement can reach index 0, through its height or a positive
        // offset, and not otherwise.
        let above = pin(first, page_rows - 1);
        assert_eq!(Some(-4), row_index(&chunks, above, captured, 5, 0));
        assert_eq!(Some(-4), row_index(&chunks, above, captured, 1, 4));
        assert_eq!(None, row_index(&chunks, above, captured, 4, 0));
        assert_eq!(None, row_index(&chunks, above, captured, 4, -2));
        let first_top = pin(first, 0);
        assert_eq!(
            Some(-3 - page_rows),
            row_index(&chunks, first_top, captured, (page_rows + 4) as u32, 0)
        );

        // Below: the first row of the last page comes right after the middle
        // page's rows. Only a negative offset that moves it up into the
        // captured rows finds it.
        let below_index: isize = -3 + unsafe { middle.as_ref().rows() } as isize;
        let below = pin(last, 0);
        assert_eq!(None, row_index(&chunks, below, captured, 100, 0));
        assert_eq!(
            Some(below_index),
            row_index(&chunks, below, captured, 1, (captured - 1 - below_index) as i32)
        );
        assert_eq!(None, row_index(&chunks, below, captured, 1, (captured - below_index) as i32));
        let below2 = pin(last, 2);
        assert_eq!(
            Some(below_index + 2),
            row_index(&chunks, below2, captured, 1, (-(below_index + 2)) as i32)
        );
    }
}
