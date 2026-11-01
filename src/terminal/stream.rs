//! Port of `src/terminal/stream.zig`.
//!
//! STATUS: ported
//!
//! Zig's `Stream(Handler)` is comptime-generic over a handler type with a
//! `vt(comptime action: Action.Tag, value: Action.Value(action))` function.
//! In Rust the handler implements the [`Handler`] trait and receives a
//! fully built [`Action`]. The stream is monomorphized per handler, so with
//! an `#[inline]` `vt` the match on the action folds away at every call site
//! just like Zig's comptime dispatch.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::terminal::ansi;
use crate::terminal::charsets;
use crate::terminal::csi;
use crate::terminal::device_attributes;
use crate::terminal::device_status;
use crate::terminal::kitty;
use crate::terminal::lib;
use crate::terminal::modes;
use crate::terminal::mouse::Shape as MouseShape;
use crate::terminal::osc;
use crate::terminal::parser::{self, Parser, State as ParserState, MAX_PARAMS};
use crate::terminal::sgr;
use crate::terminal::stream_continuation as continuationpkg;
use crate::terminal::utf8_decoder::Utf8Decoder;
use crate::terminal::xt_checksum;

const LOG_TARGET: &str = "stream";

/// Logs an unsupported-input message at most once per distinct key
/// per call site per process. See [`should_log_unsupported`].
macro_rules! log_unsupported_once {
    ($key:expr, $($arg:tt)+) => {{
        static SEEN: [AtomicU32; 16] = [const { AtomicU32::new(u32::MAX) }; 16];
        if should_log_unsupported(&SEEN, ($key) as u16) {
            log::warn!(target: LOG_TARGET, $($arg)+);
        }
    }};
}
use log_unsupported_once;

/// Flip this to true when you want verbose debug output for
/// debugging terminal stream issues. In addition to louder
/// output this will also disable the SIMD optimizations in
/// order to make it easier to see every byte. So if you're
/// debugging an issue in the SIMD code then you'll need to
/// do something else.
const DEBUG: bool = false;

/// The vector length of the printable-run scan for the compile target
/// (Zig: `simd.lanes(u32)`); 0 means a scalar loop.
const STREAM_SCAN_LANES: usize = match crate::simd::lanes::<u32>() {
    Some(l) => l,
    None => 0,
};

macro_rules! define_actions {
    ($( $(#[$m:meta])* $variant:ident $(($ty:ty))? = $name:literal, )*) => {
        /// The possible actions that can be emitted by the Stream
        /// function for handling.
        #[derive(Debug, Clone, PartialEq)]
        pub enum Action<'a> {
            $( $(#[$m])* $variant $(($ty))?, )*
        }

        /// The tag of an [`Action`] (Zig: `Action.Key` / `Action.Tag`).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum Key {
            $( $variant, )*
        }

        impl Key {
            /// The original Zig tag name.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Key::$variant => $name, )*
                }
            }
        }

        impl Action<'_> {
            /// The active tag.
            #[inline]
            pub fn key(&self) -> Key {
                match self {
                    $( Action::$variant { .. } => Key::$variant, )*
                }
            }
        }
    };
}

define_actions! {
    Print(Print) = "print",
    PrintSlice(PrintSlice<'a>) = "print_slice",
    PrintRepeat(usize) = "print_repeat",
    Bell = "bell",
    Backspace = "backspace",
    HorizontalTab(u16) = "horizontal_tab",
    HorizontalTabBack(u16) = "horizontal_tab_back",
    Linefeed = "linefeed",
    CarriageReturn = "carriage_return",
    Enquiry = "enquiry",
    InvokeCharset(InvokeCharset) = "invoke_charset",
    CursorUp(CursorMovement) = "cursor_up",
    CursorDown(CursorMovement) = "cursor_down",
    CursorLeft(CursorMovement) = "cursor_left",
    CursorRight(CursorMovement) = "cursor_right",
    CursorCol(CursorMovement) = "cursor_col",
    CursorRow(CursorMovement) = "cursor_row",
    CursorColRelative(CursorMovement) = "cursor_col_relative",
    CursorRowRelative(CursorMovement) = "cursor_row_relative",
    CursorPos(CursorPos) = "cursor_pos",
    CursorStyle(ansi::CursorStyle) = "cursor_style",
    EraseDisplayBelow(bool) = "erase_display_below",
    EraseDisplayAbove(bool) = "erase_display_above",
    EraseDisplayComplete(bool) = "erase_display_complete",
    EraseDisplayScrollback(bool) = "erase_display_scrollback",
    EraseDisplayScrollComplete(bool) = "erase_display_scroll_complete",
    EraseLineRight(bool) = "erase_line_right",
    EraseLineLeft(bool) = "erase_line_left",
    EraseLineComplete(bool) = "erase_line_complete",
    EraseLineRightUnlessPendingWrap(bool) = "erase_line_right_unless_pending_wrap",
    DeleteChars(usize) = "delete_chars",
    EraseChars(usize) = "erase_chars",
    InsertLines(usize) = "insert_lines",
    InsertBlanks(usize) = "insert_blanks",
    DeleteLines(usize) = "delete_lines",
    ScrollUp(usize) = "scroll_up",
    ScrollDown(usize) = "scroll_down",
    TabClearCurrent = "tab_clear_current",
    TabClearAll = "tab_clear_all",
    TabSet = "tab_set",
    TabReset = "tab_reset",
    Index = "index",
    NextLine = "next_line",
    ReverseIndex = "reverse_index",
    FullReset = "full_reset",
    SetMode(Mode) = "set_mode",
    ResetMode(Mode) = "reset_mode",
    SaveMode(Mode) = "save_mode",
    RestoreMode(Mode) = "restore_mode",
    RequestMode(Mode) = "request_mode",
    RequestModeUnknown(RawMode) = "request_mode_unknown",
    TopAndBottomMargin(Margin) = "top_and_bottom_margin",
    LeftAndRightMargin(Margin) = "left_and_right_margin",
    LeftAndRightMarginAmbiguous = "left_and_right_margin_ambiguous",
    SaveCursor = "save_cursor",
    RestoreCursor = "restore_cursor",
    ModifyKeyFormat(ansi::ModifyKeyFormat) = "modify_key_format",
    MouseShiftCapture(bool) = "mouse_shift_capture",
    ProtectedModeOff = "protected_mode_off",
    ProtectedModeIso = "protected_mode_iso",
    ProtectedModeDec = "protected_mode_dec",
    SizeReport(csi::SizeReportStyle) = "size_report",
    TitlePush(u16) = "title_push",
    TitlePop(u16) = "title_pop",
    Xtversion = "xtversion",
    DeviceAttributes(device_attributes::Req) = "device_attributes",
    DeviceStatus(DeviceStatus) = "device_status",
    KittyKeyboardQuery = "kitty_keyboard_query",
    KittyKeyboardPush(KittyKeyboardFlags) = "kitty_keyboard_push",
    KittyKeyboardPop(u16) = "kitty_keyboard_pop",
    KittyKeyboardSet(KittyKeyboardFlags) = "kitty_keyboard_set",
    KittyKeyboardSetOr(KittyKeyboardFlags) = "kitty_keyboard_set_or",
    KittyKeyboardSetNot(KittyKeyboardFlags) = "kitty_keyboard_set_not",
    DcsHook(parser::Dcs<'a>) = "dcs_hook",
    DcsPut(u8) = "dcs_put",
    DcsUnhook = "dcs_unhook",
    ApcStart = "apc_start",
    ApcEnd(ApcEnd) = "apc_end",
    ApcPut(u8) = "apc_put",
    ApcPutSlice(ApcPutSlice<'a>) = "apc_put_slice",
    EndHyperlink = "end_hyperlink",
    ActiveStatusDisplay(ansi::StatusDisplay) = "active_status_display",
    Decaln = "decaln",
    WindowTitle(WindowTitle<'a>) = "window_title",
    ReportPwd(ReportPwd<'a>) = "report_pwd",
    ShowDesktopNotification(ShowDesktopNotification<'a>) = "show_desktop_notification",
    ProgressReport(osc::ProgressReport) = "progress_report",
    StartHyperlink(StartHyperlink<'a>) = "start_hyperlink",
    ClipboardContents(ClipboardContents<'a>) = "clipboard_contents",
    MouseShape(MouseShape) = "mouse_shape",
    ConfigureCharset(ConfigureCharset) = "configure_charset",
    SetAttribute(sgr::Attribute<'a>) = "set_attribute",
    KittyColorReport(kitty::color::Osc) = "kitty_color_report",
    ColorOperation(ColorOperation) = "color_operation",
    SemanticPrompt(SemanticPrompt<'a>) = "semantic_prompt",
    KittyClipboard(KittyClipboard<'a>) = "kitty_clipboard",
    KittyDnd(KittyDnd<'a>) = "kitty_dnd",
    ResizeWindow(ResizeWindow) = "resize_window",
    OscUnknown(osc::Unknown<'a>) = "osc_unknown",
    MouseShapeReset = "mouse_shape_reset",
    RequestXtChecksum(xt_checksum::Request) = "request_xt_checksum",
    XtChecksumExtension(XtChecksumExtension) = "xt_checksum_extension",
    ProgramStatus(ProgramStatus<'a>) = "program_status",
    SoftReset = "soft_reset",
    BackIndex = "back_index",
    ForwardIndex = "forward_index",
    InsertColumns(usize) = "insert_columns",
    DeleteColumns(usize) = "delete_columns",
}

/// Zig: `Action.Tag` (the C-ABI tag, keyed the same as `Action.Key`).
pub type Tag = Key;

/// C ABI tagged union of [`Action`] (Zig: `Action.C`, from
/// `lib.TaggedUnion`). Empty for the application build
/// (`lib.target == .zig`), as in Zig.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionC {}

/// C ABI value union of [`Action`] (Zig: `Action.CValue`). Empty for the
/// application build (`lib.target == .zig`), as in Zig.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct ActionCValue {}

impl Action<'_> {
    /// Convert to the C union (Zig: `Action.cval`; empty for the
    /// application build). The comptime-only `Value(tag)` type function
    /// has no Rust counterpart: use the variant's payload type directly.
    pub fn cval(&self) -> ActionC {
        ActionC {}
    }
}

/// Field types

/// (Zig: `Action.Print`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Print {
    /// A Unicode codepoint (Zig `u21`).
    pub cp: u32,
}

/// (Zig: `Action.Print.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PrintC {
    pub cp: u32,
}

impl Print {
    pub fn cval(self) -> PrintC {
        PrintC { cp: self.cp }
    }
}

/// A run of printable codepoints. This is emitted instead of
/// individual print actions when the stream can decode multiple
/// printable codepoints at once, so handlers can process them in
/// batch with per-run rather than per-codepoint overhead (see
/// Terminal.printSlice). A naive handler can simply loop and
/// handle each codepoint like a print action.
///
/// The slice is only valid for the duration of the handler call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PrintSlice<'a> {
    pub cps: &'a [u32],
}

/// (Zig: `Action.PrintSlice.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PrintSliceC {
    pub cps: *const u32,
    pub len: usize,
}

impl PrintSlice<'_> {
    pub fn cval(self) -> PrintSliceC {
        PrintSliceC {
            cps: self.cps.as_ptr(),
            len: self.cps.len(),
        }
    }
}

/// (Zig: `Action.ApcPutSlice`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApcPutSlice<'a> {
    pub bytes: &'a [u8],
}

/// (Zig: `Action.ApcPutSlice.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ApcPutSliceC {
    pub bytes: *const u8,
    pub len: usize,
}

impl ApcPutSlice<'_> {
    pub fn cval(self) -> ApcPutSliceC {
        ApcPutSliceC {
            bytes: self.bytes.as_ptr(),
            len: self.bytes.len(),
        }
    }
}

/// Sent when an APC string ends, for any reason.
///
/// An APC is only complete when it ends with the string terminator,
/// either `ESC \` or the 8-bit 0x9C. A program can also stop an APC
/// partway through. CAN (0x18) or SUB (0x1A) cancels it, and any other
/// 8-bit control byte, such as CSI (0x9B), aborts it. ECMA-48 says a
/// string stopped this way must be thrown away, so a handler should
/// only act on the APC when `terminated` is true.
///
/// ```ignore
/// Action::ApcEnd(value) => {
///     let Some(cmd) = apc_handler.end() else { return };
///     if !value.terminated { return; }
///     run(cmd);
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ApcEnd {
    /// True when the string terminator ended the APC. False when the
    /// APC was cancelled or aborted, in which case its contents
    /// should be discarded.
    pub terminated: bool,
}

/// (Zig: `Action.InvokeCharset`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InvokeCharset {
    pub bank: charsets::ActiveSlot,
    pub charset: charsets::Slots,
    pub locking: bool,
}

/// (Zig: `Action.CursorMovement`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct CursorMovement {
    /// The value of the cursor movement. Depending on the tag of this
    /// union this may be an absolute value or it may be a relative
    /// value. For example, `cursor_up` is relative, but `cursor_row`
    /// is absolute.
    pub value: u16,
}

/// (Zig: `Action.CursorPos`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct CursorPos {
    pub row: u16,
    pub col: u16,
}

/// (Zig: `Action.DeviceStatus`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceStatus {
    pub request: device_status::Request,
}

/// (Zig: `Action.DeviceStatus.C`.)
pub type DeviceStatusC = u16;

impl DeviceStatus {
    pub fn cval(self) -> DeviceStatusC {
        self.request as u16
    }
}

/// (Zig: `Action.Mode`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mode {
    pub mode: modes::Mode,
}

/// (Zig: `Action.Mode.C`.)
pub type ModeC = u16;

impl Mode {
    pub fn cval(self) -> ModeC {
        self.mode as u16
    }
}

/// (Zig: `Action.RawMode`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawMode {
    pub mode: u16,
    pub ansi: bool,
}

/// (Zig: `Action.Margin`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Margin {
    pub top_left: u16,
    pub bottom_right: u16,
}

/// A request to resize the window's text area (CSI 8 t). A value
/// of zero means the parameter was omitted or zero, and the current
/// size for that dimension should be kept. xterm treats an explicit
/// zero as the screen size, but we can't distinguish it from omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ResizeWindow {
    pub rows: u16,
    pub columns: u16,
}

/// (Zig: `Action.XtChecksumExtension`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XtChecksumExtension {
    pub flags: xt_checksum::Flags,
}

/// (Zig: `Action.XtChecksumExtension.C`.)
pub type XtChecksumExtensionC = u8;

impl XtChecksumExtension {
    pub fn cval(self) -> XtChecksumExtensionC {
        self.flags.to_bits()
    }
}

/// (Zig: `Action.KittyKeyboardFlags`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyKeyboardFlags {
    pub flags: kitty::KeyFlags,
}

/// (Zig: `Action.KittyKeyboardFlags.C`.)
pub type KittyKeyboardFlagsC = u8;

impl KittyKeyboardFlags {
    pub fn cval(self) -> KittyKeyboardFlagsC {
        self.flags.int()
    }
}

/// (Zig: `Action.WindowTitle`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowTitle<'a> {
    pub title: &'a [u8],
}

impl WindowTitle<'_> {
    pub fn cval(self) -> lib::String {
        lib::String::init(self.title)
    }
}

/// (Zig: `Action.ReportPwd`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReportPwd<'a> {
    pub url: &'a [u8],
}

impl ReportPwd<'_> {
    pub fn cval(self) -> lib::String {
        lib::String::init(self.url)
    }
}

/// (Zig: `Action.ShowDesktopNotification`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShowDesktopNotification<'a> {
    pub title: &'a [u8],
    pub body: &'a [u8],
}

/// (Zig: `Action.ShowDesktopNotification.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ShowDesktopNotificationC {
    pub title: lib::String,
    pub body: lib::String,
}

impl ShowDesktopNotification<'_> {
    pub fn cval(self) -> ShowDesktopNotificationC {
        ShowDesktopNotificationC {
            title: lib::String::init(self.title),
            body: lib::String::init(self.body),
        }
    }
}

/// (Zig: `Action.StartHyperlink`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StartHyperlink<'a> {
    pub uri: &'a [u8],
    pub id: Option<&'a [u8]>,
}

/// (Zig: `Action.StartHyperlink.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct StartHyperlinkC {
    pub uri: lib::String,
    pub id: lib::String,
}

impl StartHyperlink<'_> {
    pub fn cval(self) -> StartHyperlinkC {
        StartHyperlinkC {
            uri: lib::String::init(self.uri),
            id: lib::String::init(self.id.unwrap_or(b"")),
        }
    }
}

/// (Zig: `Action.ClipboardContents`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClipboardContents<'a> {
    pub kind: u8,
    pub data: &'a [u8],
    pub terminator: osc::Terminator,
}

/// (Zig: `Action.ClipboardContents.C`.)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ClipboardContentsC {
    pub kind: u8,
    pub data: lib::String,
}

impl ClipboardContents<'_> {
    pub fn cval(self) -> ClipboardContentsC {
        ClipboardContentsC {
            kind: self.kind,
            data: lib::String::init(self.data),
        }
    }
}

/// (Zig: `Action.ConfigureCharset`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigureCharset {
    pub slot: charsets::Slots,
    pub charset: charsets::Charset,
}

/// (Zig: `Action.ColorOperation`.)
#[derive(Debug, Clone, PartialEq)]
pub struct ColorOperation {
    pub op: osc::color::Operation,
    pub requests: osc::color::List,
    pub terminator: osc::Terminator,
}

impl ColorOperation {
    /// The C representation is `void`.
    pub fn cval(&self) {}
}

pub type SemanticPrompt<'a> = osc::SemanticPrompt<'a>;

pub type KittyClipboard<'a> = osc::KittyClipboardProtocol<'a>;

pub type KittyDnd<'a> = osc::KittyDndProtocol<'a>;

pub type ProgramStatus<'a> = osc::ProgramStatus<'a>;

/// An error returned by a [`Handler::vt_raw`] hook.
pub type VtRawError = Box<dyn std::error::Error + Send + Sync>;

/// The handler a [`Stream`] dispatches actions to.
///
/// This is the Rust form of the Zig handler duck type: a `vt` function
/// that receives every action, an optional `vtRaw` hook, and a `deinit`
/// function.
pub trait Handler {
    /// Whether this handler implements [`Handler::vt_raw`] (Zig:
    /// `@hasDecl(T, "vtRaw")`). Handlers with a raw hook disable the
    /// stream's bulk fast paths so every action passes through the hook.
    const HAS_VT_RAW: bool = false;

    /// Handle one action. Printable text is delivered via `PrintSlice`
    /// actions (runs of codepoints) whenever the stream can decode
    /// multiple codepoints at once, and via `Print` actions otherwise.
    /// Handlers that care about text must handle both.
    fn vt(&mut self, action: Action<'_>);

    /// A handler can expose this to get the raw action before it is
    /// further parsed. If this returns `true` then the stream skips
    /// processing it. Only called when `HAS_VT_RAW` is true.
    fn vt_raw(&mut self, _action: &parser::Action<'_>) -> Result<bool, VtRawError> {
        Ok(false)
    }

    /// Release the handler's resources. Called by [`Stream::deinit`].
    fn deinit(&mut self) {}
}

/// A pointer handler (Zig `Stream(*T)`).
impl<T: Handler + ?Sized> Handler for &mut T {
    const HAS_VT_RAW: bool = T::HAS_VT_RAW;

    #[inline(always)]
    fn vt(&mut self, action: Action<'_>) {
        (**self).vt(action)
    }

    #[inline(always)]
    fn vt_raw(&mut self, action: &parser::Action<'_>) -> Result<bool, VtRawError> {
        (**self).vt_raw(action)
    }

    fn deinit(&mut self) {
        (**self).deinit()
    }
}

/// Options for [`Stream::init`].
pub struct Options<H> {
    /// The handler initial value, must be set.
    pub handler: H,

    /// Allocator to use. If this is not set then the stream
    /// will be fully allocation free. There are some operations
    /// that will be dropped in this case such as OSC 52 clipboard
    /// ops. (Memory always comes from the global allocator; the value
    /// only carries Zig's allocator semantics, see `osc::Allocator`.)
    pub allocator: Option<osc::Allocator>,

    /// Maximum size in bytes of the continuation suffix. If this is
    /// None or zero then continuation tracking is disabled. This is
    /// only applied when `allocator` is Some; without an allocator
    /// continuation tracking is disabled. Feeding this continuation
    /// suffix into an equivalent stream at ground reconstructs the
    /// unfinished state without repeating committed terminal effects.
    /// Continuation tracking is only supported by TerminalStream.
    pub continuation_max_bytes: Option<usize>,

    /// The most bytes to keep from each OSC sequence whose number
    /// the OSC parser does not implement. Zero, the default,
    /// discards these sequences. Any other value sends them to the
    /// handler as `osc_unknown` actions. See
    /// `osc::Parser::unknown_max_bytes` for how the limit behaves.
    ///
    /// This only affects OSC. Other kinds of unknown sequences,
    /// such as APC, are collected by the handler and have their own
    /// limits there.
    pub osc_unknown_max_bytes: usize,
}

impl<H> Options<H> {
    /// Options with the given handler and every other field at its
    /// default.
    pub fn new(handler: H) -> Options<H> {
        Options {
            handler,
            allocator: None,
            continuation_max_bytes: None,
            osc_unknown_max_bytes: 0,
        }
    }
}

/// Errors from [`Stream::write_continuation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteContinuationError {
    /// The writer failed (Zig `std.Io.Writer.Error.WriteFailed`).
    WriteFailed,
    ContinuationDisabled,
    ContinuationUnavailable,
}

impl std::fmt::Display for WriteContinuationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WriteContinuationError::WriteFailed => "WriteFailed",
            WriteContinuationError::ContinuationDisabled => "ContinuationDisabled",
            WriteContinuationError::ContinuationUnavailable => "ContinuationUnavailable",
        })
    }
}

impl std::error::Error for WriteContinuationError {}

/// This is the maximum number of codepoints we can decode
/// at one time for this function call. This is somewhat arbitrary
/// so if someone can demonstrate a better number then we can switch.
const CP_BUF_LEN: usize = 4096;

/// A type that can process a stream of tty control characters.
/// This will call [`Handler::vt`] on the handler for every action.
///
/// The handler type H can choose to react to whatever actions it cares
/// about in its pursuit of implementing a terminal emulator or other
/// functionality.
///
/// Note that printable text is delivered via `print_slice` actions
/// (runs of codepoints) whenever the stream can decode multiple
/// codepoints at once, and via `print` actions otherwise. Handlers
/// that care about text must handle both.
///
/// The stream is monomorphized per handler type (the Zig version is
/// comptime-generic), which lets the compiler optimize away
/// unimplemented actions: there is no conditional branching cost on
/// every single action because separate code paths are generated for
/// every single action.
pub struct Stream<H: Handler> {
    pub handler: H,
    pub parser: Parser,
    pub utf8decoder: Utf8Decoder,
    pub continuation: Option<continuationpkg::Tracker>,

    /// Scratch space for decoded codepoints. Zig uses an uninitialized
    /// stack array per call; Rust keeps one buffer per stream so that it
    /// isn't zero-initialized on every call.
    cp_buf: Vec<u32>,
}

impl<H: Handler> Stream<H> {
    /// Initialize a stream. Without an allocator, operations that require
    /// heap allocation are dropped.
    ///
    /// As a concrete example of something that requires heap allocation,
    /// consider OSC 52 (clipboard operations) which can be arbitrarily
    /// large.
    ///
    /// This takes ownership of the handler and will call deinit
    /// when the stream is deinitialized.
    pub fn init(options: Options<H>) -> Stream<H> {
        // Initialize the parser
        let mut parser = Parser::init();
        if let Some(alloc) = options.allocator {
            parser.osc_parser.alloc = Some(alloc);
        }
        parser.osc_parser.unknown_max_bytes = options.osc_unknown_max_bytes;

        // Initialize the continuation tracker if one is requested.
        let mut tracker: Option<continuationpkg::Tracker> = None;
        if let Some(alloc) = options.allocator {
            if let Some(max_bytes) = options.continuation_max_bytes {
                if max_bytes > 0 {
                    tracker = Some(continuationpkg::Tracker::init(alloc, max_bytes));
                }
            }
        }

        Stream {
            handler: options.handler,
            parser,
            utf8decoder: Utf8Decoder::default(),
            continuation: tracker,
            cp_buf: vec![0; CP_BUF_LEN],
        }
    }

    /// Shorthand for `Stream::init(Options::new(handler))`.
    pub fn new(handler: H) -> Stream<H> {
        Stream::init(Options::new(handler))
    }

    pub fn deinit(&mut self) {
        if let Some(tracker) = &mut self.continuation {
            tracker.deinit();
        }
        self.parser.deinit();
        self.handler.deinit();
    }

    /// Write the current continuation suffix directly to a caller-owned
    /// writer. The caller must pause and serialize access to this Stream.
    pub fn write_continuation<W: io::Write + ?Sized>(
        &self,
        writer: &mut W,
    ) -> Result<(), WriteContinuationError> {
        let tracker = self
            .continuation
            .as_ref()
            .ok_or(WriteContinuationError::ContinuationDisabled)?;
        if tracker.broken {
            return Err(WriteContinuationError::ContinuationUnavailable);
        }
        tracker
            .write(writer)
            .map_err(|_| WriteContinuationError::WriteFailed)
    }

    /// True when no continuation suffix is needed to reproduce the
    /// stream's current parsing state.
    #[inline(always)]
    pub fn ground(&self) -> bool {
        // Parser ground alone is not sufficient because the UTF-8
        // decoder may have some state.
        self.parser.state == ParserState::Ground && self.utf8decoder.state == 0
    }

    /// Update the continuation suffix after one complete feed call.
    /// Must only be called when tracking is enabled.
    fn track_continuation(&mut self, input: &[u8]) {
        let ground = self.ground();
        let pending = if self.parser.state != ParserState::Ground {
            continuationpkg::Pending::Vt
        } else {
            continuationpkg::Pending::Utf8
        };
        let tracker = self
            .continuation
            .as_mut()
            .expect("continuation tracking enabled");

        // If we're in a ground state, we have no continuation suffix
        // to track by definition.
        if ground {
            tracker.reset();
            return;
        }

        // Retain the part of this feed needed to replay the unfinished
        // state. When the parser is grounded here, the feed must have
        // ended inside a UTF-8 codepoint instead, because the ground
        // check above covers both state machines.
        tracker.append(pending, input);
    }

    /// Process a string of characters.
    #[inline]
    pub fn next_slice(&mut self, input: &[u8]) {
        let mut u = Until::disabled();
        self.next_slice_untracked::<false>(input, &mut u);

        // Continuation tracking is opt-in and this branch predicts
        // perfectly, so disabled streams pay nothing else here.
        if self.continuation.is_some() {
            self.track_continuation(input);
        }
    }

    /// Process input until the first byte that reaches one of
    /// `boundaries`, and stop right after that byte.
    ///
    /// Returns the stop, or None if the whole input was consumed
    /// without reaching a boundary. Only the consumed bytes are
    /// processed, so handler actions never run for bytes after the
    /// stop. Feed the rest of the input in a later call.
    ///
    /// The bytes consumed across a series of calls are exactly the
    /// bytes the stream parsed, in order, and a second stream fed those
    /// same bytes is in the same parser state at every stop. This is
    /// what a proxy that sits between a program and a terminal needs.
    /// It can find exactly where a control string starts and ends, or
    /// wait for a safe point before inserting its own output, using
    /// the same parse that drives its terminal.
    ///
    /// `Ground` is a state, not an event. If the stream is already at
    /// ground, this consumes nothing and returns a stop with
    /// `consumed == 0`. A loop that keeps asking for `Ground` with the
    /// same input never makes progress. After a ground stop, feed the
    /// next bytes without `Ground` in the set, or with `next_slice`.
    ///
    /// This loop finds the content of every APC string in `input`.
    /// DCS and OSC strings work the same way with their own
    /// boundaries:
    ///
    /// ```ignore
    /// let set = BoundarySet::init_many(&[Boundary::ApcStart, Boundary::ApcEnd]);
    /// let mut rest = input;
    /// let mut content_start = None;
    /// while let Some(stop) = stream.next_slice_until(rest, set) {
    ///     let offset = input.len() - rest.len();
    ///     if stop.start.is_some() { content_start = Some(offset + stop.consumed); }
    ///     if stop.end.is_some() {
    ///         // The ending byte is consumed but is not content.
    ///         let content_end = offset + stop.consumed - 1;
    ///         handle_apc(&input[content_start.unwrap()..content_end]);
    ///     }
    ///     rest = &rest[stop.consumed..];
    /// }
    /// ```
    ///
    /// The loop needs no special case for a string that ends with the
    /// two-byte terminator `ESC \`. The stop lands on the ESC, and the
    /// backslash is just the next byte of `rest`.
    ///
    /// Asking for `Ground` costs 1% to 5% over `next_slice`, because the
    /// stream has to leave its fast path at every return to ground.
    /// Asking only for string boundaries costs nothing on the bulk
    /// paths that handle text and string payloads, and a little for
    /// each byte of an escape sequence. Every check is compiled out
    /// of `next_slice`, so its code is unchanged whether or not this
    /// function is used.
    #[inline]
    pub fn next_slice_until(&mut self, input: &[u8], boundaries: BoundarySet) -> Option<Stop> {
        if boundaries.contains(Boundary::Ground) && self.ground() {
            return Some(Stop {
                consumed: 0,
                reached: BoundarySet::init_one(Boundary::Ground),
                start: None,
                end: None,
            });
        }

        let mut u = Until {
            boundaries,
            base: input.as_ptr() as usize,
            stop: None,
        };
        self.next_slice_untracked::<true>(input, &mut u);

        let consumed = match &u.stop {
            Some(stop) => stop.consumed,
            None => input.len(),
        };
        if self.continuation.is_some() && consumed > 0 {
            self.track_continuation(&input[..consumed]);
        }

        u.stop
    }

    /// In until mode, called after the byte just before `end` (an
    /// address) has been fully processed. Completes the stop if that byte
    /// reached a requested boundary (including ground, which is also
    /// checked here) and returns true when the caller must return.
    #[inline(always)]
    fn until_reached(&self, u: &mut Until, end: usize) -> bool {
        let at_ground = u.boundaries.contains(Boundary::Ground) && self.ground();
        if u.stop.is_none() && !at_ground {
            return false;
        }
        let base = u.base;
        let stop = u.stop_ptr();
        stop.consumed = end - base;
        if at_ground {
            stop.reached.insert(Boundary::Ground);
        }
        true
    }

    #[inline(always)]
    fn next_slice_untracked<const UNTIL: bool>(&mut self, input: &[u8], u: &mut Until) {
        // Byte-at-a-time parsing in debug mode only. Even without
        // SIMD support, the batched path below is much faster than
        // per-byte dispatch because it decodes UTF-8 in bulk (via scalar
        // fallbacks) and hands printable runs to the handler as
        // print_slice actions.
        if DEBUG {
            let base = input.as_ptr() as usize;
            for (i, &c) in input.iter().enumerate() {
                self.next_untracked::<UNTIL>(c, u);
                if UNTIL && self.until_reached(u, base + i + 1) {
                    return;
                }
            }
            return;
        }

        // Take the codepoint buffer out of self so the handler can be
        // called with slices of it. If a previous call panicked while the
        // buffer was out, it is recreated.
        let mut cp_buf = std::mem::take(&mut self.cp_buf);
        if cp_buf.len() != CP_BUF_LEN {
            cp_buf = vec![0; CP_BUF_LEN];
        }

        // The UTF-8 decode and printable-run scan below are vectorized;
        // on x86_64 CPUs with AVX2 (when the compile target lacks it) use
        // an instance compiled for AVX2, as Zig's native build does at
        // compile time (and Highway does at runtime for its decoder).
        #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
        if crate::simd::avx2_available() && !crate::simd::force_portable() {
            // SAFETY: AVX2 is available.
            unsafe { self.next_slice_chunks_avx2::<UNTIL>(input, &mut cp_buf, u) };
            self.cp_buf = cp_buf;
            return;
        }
        self.next_slice_chunks::<UNTIL, false>(input, &mut cp_buf, u);
        self.cp_buf = cp_buf;
    }

    /// `next_slice_chunks` compiled for AVX2.
    ///
    /// # Safety
    ///
    /// The CPU must support AVX2.
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    #[target_feature(enable = "avx2")]
    unsafe fn next_slice_chunks_avx2<const UNTIL: bool>(&mut self, input: &[u8], cp_buf: &mut [u32], u: &mut Until) {
        self.next_slice_chunks::<UNTIL, true>(input, cp_buf, u);
    }

    /// Split the input into chunks that fit into cp_buf. AVX2 selects the
    /// explicit AVX2 loops (only in the AVX2 instance).
    #[inline(always)]
    fn next_slice_chunks<const UNTIL: bool, const AVX2: bool>(&mut self, input: &[u8], cp_buf: &mut [u32], u: &mut Until) {
        let mut i = 0;
        loop {
            let len = CP_BUF_LEN.min(input.len() - i);
            self.next_slice_capped::<UNTIL, AVX2>(&input[i..i + len], cp_buf, u);
            if UNTIL && u.stop.is_some() {
                break;
            }
            i += len;
            if i >= input.len() {
                break;
            }
        }
    }

    #[inline(always)]
    fn next_slice_capped<const UNTIL: bool, const AVX2: bool>(
        &mut self,
        input: &[u8],
        cp_buf: &mut [u32],
        u: &mut Until,
    ) {
        debug_assert!(input.len() <= cp_buf.len());
        let base = input.as_ptr() as usize;

        let mut offset: usize = 0;

        // If the scalar UTF-8 decoder was in the middle of processing
        // a code sequence, we continue until it's not.
        while self.utf8decoder.state != 0 {
            if offset >= input.len() {
                return;
            }
            self.next_utf8(input[offset]);
            offset += 1;
        }
        // Completing the codepoint may have reached ground.
        if UNTIL && self.until_reached(u, base + offset) {
            return;
        }
        if offset >= input.len() {
            return;
        }

        // If we're not in the ground state then we process until
        // we are. This can happen if the last chunk of input put us
        // in the middle of a control sequence.
        offset += self.consume_until_ground::<UNTIL>(&input[offset..], u);
        if UNTIL && self.until_reached(u, base + offset) {
            return;
        }
        if offset >= input.len() {
            return;
        }
        offset += self.consume_all_escapes::<UNTIL>(&input[offset..], u);
        if UNTIL && self.until_reached(u, base + offset) {
            return;
        }

        // If we're in the ground state then we can use SIMD to process
        // input until we see an ESC (0x1B), since all other characters
        // up to that point are just UTF-8.
        while self.parser.state == ParserState::Ground && offset < input.len() {
            let res = if AVX2 {
                #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
                // SAFETY: only reached in the AVX2 instance.
                unsafe {
                    crate::simd::vt::utf8_decode_until_control_seq_avx2(&input[offset..], cp_buf)
                }
                #[cfg(not(all(target_arch = "x86_64", not(target_feature = "avx2"))))]
                unreachable!()
            } else {
                crate::simd::vt::utf8_decode_until_control_seq(&input[offset..], cp_buf)
            };
            let cps = &cp_buf[..res.decoded];

            // Hand runs of printable codepoints to the handler as
            // print_slice actions so it can process them with
            // per-run rather than per-codepoint overhead.
            let mut i = 0;
            while i < cps.len() {
                let cp = cps[i];
                // C0 and UTF-8-decoded C1 controls never enter
                // printable runs. A codepoint is one of those
                // exactly when it has no bit set outside 0x9F:
                // bits 0-4 and bit 7 cover 0x00-0x1F and
                // 0x80-0x9F and nothing else, so a single
                // AND-test classifies both ranges.
                if cp & !0x9F == 0 {
                    if cp <= 0x1F {
                        // C0 controls execute rather than print.
                        Self::execute(&mut self.handler, cp as u8);
                    } else {
                        // C1 controls decoded from UTF-8 are ignored.
                        log_unsupported_once!(
                            0x80,
                            "ignoring UTF-8-decoded C1 controls, first: 0x{:x}",
                            cp
                        );
                    }
                    i += 1;
                    continue;
                }

                // Find the end of the printable run. This is an
                // early-exit search loop that won't auto-vectorize,
                // and printable runs dominate real input, so scan
                // several codepoints at a time manually.
                let mut end = i + 1;
                'scan: {
                    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
                    if AVX2 {
                        // SAFETY: only reached in the AVX2 instance.
                        let (e, found) = unsafe { crate::simd::vt::printable_run_scan_avx2(cps, end) };
                        end = e;
                        if found {
                            break 'scan;
                        }
                    }
                    const LANES: usize = STREAM_SCAN_LANES;
                    if !AVX2 && LANES > 0 {
                        while end + LANES <= cps.len() {
                            // Non-printable (C0 or decoded C1) is
                            // equivalent to "no bit outside 0x9F is
                            // set": one AND-test per lane.
                            let v: &[u32; LANES] = cps[end..end + LANES].try_into().unwrap();
                            let stop = v.iter().fold(false, |acc, &v| acc | (v & !0x9F == 0));
                            if stop {
                                let bits = crate::terminal::terminal::lane_bits(v, |v| v & !0x9F == 0);
                                end += bits.trailing_zeros() as usize;
                                break 'scan;
                            }
                            end += LANES;
                        }
                    }
                    while end < cps.len() && cps[end] & !0x9F != 0 {
                        end += 1;
                    }
                }
                self.handler.vt(Action::PrintSlice(PrintSlice {
                    cps: &cps[i..end],
                }));
                i = end;
            }
            // Consume the bytes we just processed.
            offset += res.consumed;

            if offset >= input.len() {
                return;
            }

            // If our offset is NOT an escape then we must have a
            // partial UTF-8 sequence. In that case, we pass it off
            // to the scalar parser.
            if input[offset] != 0x1B {
                for &c in &input[offset..] {
                    self.next_utf8(c);
                }
                return;
            }

            // Process control sequences until we run out.
            offset += self.consume_all_escapes::<UNTIL>(&input[offset..], u);
            if UNTIL && self.until_reached(u, base + offset) {
                return;
            }
        }
    }

    /// Parses back-to-back escape sequences until none are left.
    /// Returns the number of bytes consumed from the provided input.
    ///
    /// Expects input to start with 0x1B, use consume_until_ground first
    /// if the stream may be in the middle of an escape sequence.
    #[inline(always)]
    fn consume_all_escapes<const UNTIL: bool>(&mut self, input: &[u8], u: &mut Until) -> usize {
        let base = input.as_ptr() as usize;
        let mut offset: usize = 0;
        while input[offset] == 0x1B {
            self.parser.state = ParserState::Escape;
            self.parser.clear();
            offset += 1;
            offset += self.consume_until_ground::<UNTIL>(&input[offset..], u);
            if UNTIL && self.until_reached(u, base + offset) {
                return offset;
            }
            if offset >= input.len() {
                return input.len();
            }
        }
        offset
    }

    /// Parses escape sequences until the parser reaches the ground state.
    /// Returns the number of bytes consumed from the provided input.
    #[inline(always)]
    fn consume_until_ground<const UNTIL: bool>(&mut self, input: &[u8], u: &mut Until) -> usize {
        let base = input.as_ptr() as usize;
        let mut offset: usize = 0;
        while self.parser.state != ParserState::Ground {
            if offset >= input.len() {
                return input.len();
            }

            // Fast path for CSI entry: "ESC [" is by far the most
            // common escape sequence prefix, so handle the '[' and
            // the byte that follows it here rather than paying a
            // next_non_utf8 call for each.
            if self.parser.state == ParserState::Escape && input[offset] == b'[' {
                self.parser.state = ParserState::CsiEntry;
                offset += 1;
                continue;
            }

            if !H::HAS_VT_RAW {
                if self.parser.state == ParserState::CsiEntry && self.csi_entry_byte(input[offset]) {
                    offset += 1;
                    continue;
                }

                // Bulk-consume CSI parameter bytes. This can't be
                // used for handlers with a vt_raw hook because it
                // dispatches the CSI directly (see next_non_utf8).
                if self.parser.state == ParserState::CsiParam {
                    offset += self.consume_csi_params(&input[offset..]);
                    if offset >= input.len() {
                        return input.len();
                    }
                    // If we're still in csi_param then the next byte
                    // isn't a parameter byte; let next_non_utf8 below
                    // handle it. Otherwise re-check our state.
                    if self.parser.state != ParserState::CsiParam {
                        continue;
                    }
                }

                // Bulk-consume APC string bytes into a single slice.
                // APC payloads (e.g. Kitty graphics) can be megabytes
                // of base64 data, so per-byte dispatch is far too slow.
                // This can't be used for handlers with a vt_raw hook
                // because it dispatches the slice directly.
                if self.parser.state == ParserState::SosPmApcString {
                    offset += self.consume_apc_string(&input[offset..]);
                    if offset >= input.len() {
                        return input.len();
                    }

                    // Fast-path normal string termination. This matches
                    // Parser.next's exit and entry actions while avoiding
                    // the generic action loop for every completed APC.
                    match input[offset] {
                        0x1B => {
                            self.parser.clear();
                            self.parser.state = ParserState::Escape;
                            self.handler.vt(Action::ApcEnd(ApcEnd { terminated: true }));
                            if UNTIL {
                                u.string_end(StringKind::Apc, input[offset]);
                                offset += 1;
                                if self.until_reached(u, base + offset) {
                                    return offset;
                                }
                                continue;
                            }
                            offset += 1;
                            continue;
                        }
                        0x9C => {
                            self.parser.state = ParserState::Ground;
                            self.handler.vt(Action::ApcEnd(ApcEnd { terminated: true }));
                            if UNTIL {
                                u.string_end(StringKind::Apc, input[offset]);
                                offset += 1;
                                if self.until_reached(u, base + offset) {
                                    return offset;
                                }
                                continue;
                            }
                            offset += 1;
                            continue;
                        }
                        _ => {}
                    }

                    // Aborting transitions need the scalar path so the
                    // handler can distinguish them from terminators.
                }

                // Bulk-consume OSC string bytes into the OSC parser.
                // OSC payloads (e.g. OSC 52 clipboard operations and
                // the kitty clipboard protocol) can be megabytes of
                // base64 data, so per-byte dispatch is far too slow.
                // This can't be used for handlers with a vt_raw hook
                // because the bytes never produce osc_put actions.
                if self.parser.state == ParserState::OscString {
                    offset += self.consume_osc_string(&input[offset..]);
                    if offset >= input.len() {
                        return input.len();
                    }

                    // Fast-path normal string termination. This matches
                    // Parser.next's exit and entry actions while avoiding
                    // the generic action loop for every completed OSC.
                    match input[offset] {
                        // The sequence should be terminated by a full
                        // ST ("ESC \"); the "\" that should follow is
                        // dispatched as a normal escape sequence.
                        0x1B => {
                            if let Some(cmd) = self.parser.osc_parser.end(Some(0x1B)) {
                                Self::osc_dispatch(&mut self.handler, cmd);
                            }
                            self.parser.clear();
                            self.parser.state = ParserState::Escape;
                            if UNTIL {
                                u.string_end(StringKind::Osc, input[offset]);
                                offset += 1;
                                if self.until_reached(u, base + offset) {
                                    return offset;
                                }
                                continue;
                            }
                            offset += 1;
                            continue;
                        }
                        // BEL terminates the string directly.
                        0x07 => {
                            if let Some(cmd) = self.parser.osc_parser.end(Some(0x07)) {
                                Self::osc_dispatch(&mut self.handler, cmd);
                            }
                            self.parser.state = ParserState::Ground;
                            if UNTIL {
                                u.string_end(StringKind::Osc, input[offset]);
                                offset += 1;
                                if self.until_reached(u, base + offset) {
                                    return offset;
                                }
                                continue;
                            }
                            offset += 1;
                            continue;
                        }
                        // CAN/SUB abort and ignored C0 bytes go
                        // through the state machine below.
                        _ => {}
                    }
                }
            }

            self.next_non_utf8::<UNTIL>(input[offset], u);
            offset += 1;
            if UNTIL && self.until_reached(u, base + offset) {
                return offset;
            }
        }
        offset
    }

    /// Fast path for a byte in the csi_entry state, the state right
    /// after "ESC [". Virtually every CSI sequence spends exactly
    /// one byte in this state, on either a digit, a private marker,
    /// or a final byte. Returns true if the byte was fully handled;
    /// false means the caller must process it through the general
    /// state machine.
    ///
    /// Must not be used by handlers with a vt_raw hook because the
    /// final byte case dispatches the CSI directly.
    #[inline(always)]
    fn csi_entry_byte(&mut self, c: u8) -> bool {
        debug_assert!(!H::HAS_VT_RAW);
        debug_assert!(self.parser.state == ParserState::CsiEntry);
        match c {
            // First parameter digit.
            b'0'..=b'9' => {
                self.parser.state = ParserState::CsiParam;
                // param_acc is zero (cleared on escape entry)
                // so accumulating is just the digit value.
                self.parser.param_acc = (c - b'0') as u16;
                self.parser.param_acc_idx = 1;
            }
            // An empty first parameter.
            b';' => {
                self.parser.state = ParserState::CsiParam;
                self.parser.params[0] = 0;
                self.parser.params_idx = 1;
            }
            // Private marker (e.g. '?' in "ESC [ ? 2004 h").
            0x3C..=0x3F => {
                self.parser.state = ParserState::CsiParam;
                self.parser.collect(c);
            }
            // A final byte: a parameterless CSI.
            0x40..=0x7E => self.csi_dispatch_final(c),
            // Defer to the state machine for anything else
            // (C0 controls, intermediates, colon).
            _ => return false,
        }
        true
    }

    /// Bulk-consume CSI parameter bytes (digits and separators)
    /// and, if reached, the final byte (dispatching the CSI).
    /// Returns the number of bytes consumed. Stops at the first
    /// byte that isn't handled here, leaving the parser in the
    /// csi_param state so the caller can process that byte.
    fn consume_csi_params(&mut self, input: &[u8]) -> usize {
        debug_assert!(self.parser.state == ParserState::CsiParam);

        // Accumulate parser state in locals for the hot loop.
        let mut acc = self.parser.param_acc;
        let mut acc_idx = self.parser.param_acc_idx;
        let mut idx = self.parser.params_idx;

        let mut offset: usize = 0;
        while offset < input.len() {
            let c = input[offset];
            match c {
                // A parameter digit.
                b'0'..=b'9' => {
                    if (idx as usize) < MAX_PARAMS {
                        acc = acc.saturating_mul(10);
                        acc = acc.saturating_add((c - b'0') as u16);
                        acc_idx |= 1;
                    }
                    offset += 1;
                }

                // A parameter separator.
                b':' | b';' => {
                    if (idx as usize) < MAX_PARAMS {
                        self.parser.params[idx as usize] = acc;
                        if c == b':' {
                            self.parser.params_sep.set(idx as usize);
                        }
                        idx += 1;
                        acc = 0;
                        acc_idx = 0;
                    }
                    offset += 1;
                }

                // A final byte: dispatch the CSI.
                0x40..=0x7E => {
                    self.parser.param_acc = acc;
                    self.parser.param_acc_idx = acc_idx;
                    self.parser.params_idx = idx;
                    self.csi_dispatch_final(c);
                    return offset + 1;
                }

                // Anything else (C0 controls, intermediates, etc.)
                // is handled by the caller.
                _ => break,
            }
        }

        self.parser.param_acc = acc;
        self.parser.param_acc_idx = acc_idx;
        self.parser.params_idx = idx;
        offset
    }

    /// Bulk-consume APC string bytes and dispatch them as a single
    /// apc_put_slice action. Returns the number of bytes consumed.
    /// Stops at the first byte that is not an apc_put byte in the
    /// parse table, leaving it for the caller to process through
    /// the state machine. CAN, SUB, ESC, and most C1 bytes exit
    /// or abort the string state; 0xA0-0xFF are ignored by the
    /// table (not payload), so they can't be bulk-consumed either.
    ///
    /// Must not be used by handlers with a vt_raw hook because it
    /// dispatches the slice directly.
    fn consume_apc_string(&mut self, input: &[u8]) -> usize {
        debug_assert!(!H::HAS_VT_RAW);
        debug_assert!(self.parser.state == ParserState::SosPmApcString);

        let mut end: usize = 0;
        {
            // Check a vector's worth of bytes at a time; the fold (rather
            // than an early-exit `any`) lets the compiler vectorize it.
            const VECTOR_LEN: usize = 32;
            while end + VECTOR_LEN <= input.len() {
                let invalid = input[end..end + VECTOR_LEN].iter().fold(false, |acc, &b| {
                    acc | (b == 0x18) | (b == 0x1A) | (b == 0x1B) | (b >= 0x80)
                });
                if invalid {
                    break;
                }
                end += VECTOR_LEN;
            }
        }
        while end < input.len() {
            match input[end] {
                // Not apc_put bytes: CAN/SUB/ESC and most C1 exit
                // or abort the state; 0xA0-0xFF are ignored by it.
                0x18 | 0x1A | 0x1B | 0x80..=0xFF => break,
                // Everything else is an apc_put byte.
                _ => end += 1,
            }
        }

        if end > 0 {
            self.handler.vt(Action::ApcPutSlice(ApcPutSlice {
                bytes: &input[..end],
            }));
        }
        end
    }

    /// Bulk-consume OSC string bytes into the OSC parser. Returns
    /// the number of bytes consumed. Stops at the first byte that
    /// is not an osc_put byte in the parse table, leaving it for
    /// the caller to process through the state machine. Every byte
    /// >= 0x20 is an osc_put byte; bytes below either terminate or
    /// abort the string (BEL, CAN, SUB, ESC) or are ignored.
    ///
    /// Must not be used by handlers with a vt_raw hook because the
    /// consumed bytes never produce osc_put actions.
    fn consume_osc_string(&mut self, input: &[u8]) -> usize {
        debug_assert!(!H::HAS_VT_RAW);
        debug_assert!(self.parser.state == ParserState::OscString);

        let mut end: usize = 0;
        {
            const VECTOR_LEN: usize = 32;
            while end + VECTOR_LEN <= input.len() {
                let stop = input[end..end + VECTOR_LEN]
                    .iter()
                    .fold(false, |acc, &b| acc | (b < 0x20));
                if stop {
                    break;
                }
                end += VECTOR_LEN;
            }
        }
        while end < input.len() {
            match input[end] {
                // Not osc_put bytes: BEL/CAN/SUB/ESC terminate or
                // abort the state; other C0 bytes are ignored by it.
                0x00..=0x1F => break,
                // Everything else is an osc_put byte.
                _ => end += 1,
            }
        }

        if end > 0 {
            self.parser.osc_parser.next_slice(&input[..end]);
        }
        end
    }

    /// Like next_slice but takes one byte and is necessarily a scalar
    /// operation that can't use SIMD. Prefer next_slice if you can and
    /// try to get multiple bytes at once.
    #[inline]
    pub fn next(&mut self, c: u8) {
        let mut u = Until::disabled();
        self.next_untracked::<false>(c, &mut u);
        if self.continuation.is_some() {
            self.track_continuation(&[c]);
        }
    }

    #[inline(always)]
    fn next_untracked<const UNTIL: bool>(&mut self, c: u8, u: &mut Until) {
        // The scalar path can be responsible for decoding UTF-8.
        if self.parser.state == ParserState::Ground {
            self.next_utf8(c);
            return;
        }

        self.next_non_utf8::<UNTIL>(c, u);
    }

    /// Process the next byte and print as necessary.
    ///
    /// This assumes we're in the UTF-8 decoding state. If we may not
    /// be in the UTF-8 decoding state call next_slice or next.
    #[inline(always)]
    fn next_utf8(&mut self, c: u8) {
        debug_assert!(self.parser.state == ParserState::Ground);

        let (cp, consumed) = self.utf8decoder.next(c);
        if let Some(codepoint) = cp {
            self.handle_codepoint(codepoint);
        }
        if !consumed {
            // We optimize for the scenario where the text being
            // printed in the terminal ISN'T full of ill-formed
            // UTF-8 sequences.
            let retry = self.utf8decoder.next(c);
            // It should be impossible for the decoder
            // to not consume the byte twice in a row.
            debug_assert!(retry.1);
            if let Some(codepoint) = retry.0 {
                self.handle_codepoint(codepoint);
            }
        }
    }

    /// To be called whenever the utf-8 decoder produces a codepoint.
    ///
    /// This function is abstracted this way to handle the case where
    /// the decoder emits a 0x1B after rejecting an ill-formed sequence.
    #[inline(always)]
    fn handle_codepoint(&mut self, c: u32) {
        // C0 control or a C1 control decoded from UTF-8: exactly
        // the codepoints with no bit set outside 0x9F (bits 0-4
        // and bit 7 cover 0x00-0x1F and 0x80-0x9F and nothing
        // else), so the printable fast path stays a single
        // AND-test.
        if c & !0x9F == 0 {
            // ESC
            if c == 0x1B {
                self.parser.state = ParserState::Escape;
                self.parser.clear();
                return;
            }

            // Ignore C1 that came via UTF-8 decoding, matching xterm.
            if c > 0x1F {
                log_unsupported_once!(
                    0x80,
                    "ignoring UTF-8-decoded C1 controls, first: 0x{:x}",
                    c
                );
                return;
            }

            Self::execute(&mut self.handler, c as u8);
            return;
        }
        Self::print(&mut self.handler, c);
    }

    /// Process the next character and call any callbacks if necessary.
    ///
    /// This assumes that we're not in the UTF-8 decoding state. If
    /// we may be in the UTF-8 decoding state call next_slice or next.
    fn next_non_utf8<const UNTIL: bool>(&mut self, c: u8, u: &mut Until) {
        debug_assert!(self.parser.state != ParserState::Ground);

        // Fast path for CSI entry.
        if self.parser.state == ParserState::Escape && c == b'[' {
            self.parser.state = ParserState::CsiEntry;
            return;
        }

        // The fast paths below dispatch actions directly rather than
        // going through Parser.next, so they'd bypass a handler's
        // vt_raw hook. Handlers with vt_raw (e.g. the inspector) use
        // the general path for anything that produces an action.
        let has_vt_raw = H::HAS_VT_RAW;

        // Fast path for CSI params.
        if self.parser.state == ParserState::CsiParam {
            // csi_param is the most common parser state
            // other than ground by a fairly wide margin.
            //
            // ref: https://github.com/qwerasd205/asciinema-stats
            'csi_param: {
                match c {
                    // A C0 escape (yes, this is valid):
                    0x00..=0x0F => Self::execute(&mut self.handler, c),
                    // We ignore C0 escapes > 0xF since execute
                    // doesn't have processing for them anyway:
                    0x10..=0x17 | 0x19 | 0x1C..=0x1F => {}
                    // We don't currently have any handling for
                    // 0x18 or 0x1A, but they should still move
                    // the parser state to ground.
                    0x18 | 0x1A => self.parser.state = ParserState::Ground,
                    // A parameter digit:
                    b'0'..=b'9' => {
                        if (self.parser.params_idx as usize) < MAX_PARAMS {
                            self.parser.param_acc = self.parser.param_acc.saturating_mul(10);
                            self.parser.param_acc =
                                self.parser.param_acc.saturating_add((c - b'0') as u16);
                            // The parser's CSI param action uses param_acc_idx
                            // to decide if there's a final param that needs to
                            // be consumed or not, but it doesn't matter really
                            // what it is as long as it's not 0.
                            self.parser.param_acc_idx |= 1;
                        }
                    }
                    // A parameter separator:
                    b':' | b';' => {
                        let idx = self.parser.params_idx as usize;
                        if idx < MAX_PARAMS {
                            self.parser.params[idx] = self.parser.param_acc;
                            if c == b':' {
                                self.parser.params_sep.set(idx);
                            }
                            self.parser.params_idx += 1;

                            self.parser.param_acc = 0;
                            self.parser.param_acc_idx = 0;
                        }
                    }
                    // A final byte: dispatch the CSI directly.
                    0x40..=0x7E => {
                        if !has_vt_raw {
                            self.csi_dispatch_final(c);
                        } else {
                            break 'csi_param;
                        }
                    }
                    // Explicitly ignored:
                    0x7F => {}
                    // Defer to the state machine to
                    // handle any other characters:
                    _ => break 'csi_param,
                }
                return;
            }
        }

        // Fast path for CSI entry, the state right after "ESC [".
        if !has_vt_raw && self.parser.state == ParserState::CsiEntry && self.csi_entry_byte(c) {
            return;
        }

        // String boundaries are parser state transitions, not
        // handler actions: an OSC or DCS can start and end without
        // producing one, and a vt_raw handler can skip any action.
        // So in until mode they are found by comparing the parser
        // state before and after the byte.
        let prev_state = self.parser.state;

        let actions = self.parser.next(c);

        for action in actions.into_iter().flatten() {
            if DEBUG {
                log::info!(target: LOG_TARGET, "action: {}", action);
            }

            // A handler can expose this to get the raw action before
            // it is further parsed. If this returns `true` then we skip
            // processing ourselves.
            if H::HAS_VT_RAW {
                let skip = match self.handler.vt_raw(&action) {
                    Ok(skip) => skip,
                    Err(err) => {
                        log::warn!(
                            target: LOG_TARGET,
                            "error handling action manually err={} action={}",
                            err,
                            action
                        );
                        // Always skip erroneous actions because we can't
                        // be sure...
                        true
                    }
                };

                if skip {
                    continue;
                }
            }

            match action {
                parser::Action::Print(p) => Self::print(&mut self.handler, p),
                parser::Action::Execute(code) => Self::execute(&mut self.handler, code),
                parser::Action::CsiDispatch(csi_action) => {
                    Self::csi_dispatch(&mut self.handler, csi_action)
                }
                parser::Action::EscDispatch(esc) => Self::esc_dispatch(&mut self.handler, esc),
                parser::Action::OscDispatch(cmd) => Self::osc_dispatch(&mut self.handler, cmd),
                parser::Action::DcsHook(dcs) => self.handler.vt(Action::DcsHook(dcs)),
                parser::Action::DcsPut(code) => self.handler.vt(Action::DcsPut(code)),
                parser::Action::DcsUnhook => self.handler.vt(Action::DcsUnhook),
                parser::Action::ApcStart => self.handler.vt(Action::ApcStart),
                parser::Action::ApcPut(code) => self.handler.vt(Action::ApcPut(code)),
                parser::Action::ApcEnd => self.handler.vt(Action::ApcEnd(ApcEnd {
                    terminated: c == 0x1B || c == 0x9C,
                })),
            }
        }

        // Handlers can't change the parser state, so recording the
        // transition after dispatch is equivalent to Zig recording it
        // right after Parser.next (the actions borrow the parser).
        if UNTIL {
            u.transition(prev_state, self.parser.state, c);
        }
    }

    /// Finalize and dispatch a CSI directly from parser state for
    /// the fast paths in next_non_utf8, without going through
    /// Parser.next. This must match the behavior of the parser's
    /// csi_dispatch action.
    fn csi_dispatch_final(&mut self, c: u8) {
        let p = &mut self.parser;
        p.state = ParserState::Ground;

        // Ignore sequences with too many parameters, matching the
        // parser's behavior of dropping the dispatch entirely.
        if p.params_idx as usize >= MAX_PARAMS {
            return;
        }

        // Finalize the last parameter if we have one.
        if p.param_acc_idx > 0 {
            p.params[p.params_idx as usize] = p.param_acc;
            p.params_idx += 1;
        }

        let action = parser::Csi {
            intermediates: &p.intermediates[..p.intermediates_idx as usize],
            params: &p.params[..p.params_idx as usize],
            params_sep: p.params_sep,
            r#final: c,
        };

        // We only allow colon or mixed separators for the 'm' command.
        if c != b'm' && p.params_sep.count() > 0 {
            log::warn!(
                target: LOG_TARGET,
                "CSI colon or mixed separators only allowed for 'm' command, got: {}",
                action
            );
            return;
        }

        if DEBUG {
            log::info!(target: LOG_TARGET, "action: {}", parser::Action::CsiDispatch(action));
        }
        Self::csi_dispatch(&mut self.handler, action);
    }

    #[inline(always)]
    fn print(handler: &mut H, c: u32) {
        handler.vt(Action::Print(Print { cp: c }));
    }

    #[inline(always)]
    fn execute(handler: &mut H, c: u8) {
        // If the character is > 0x7F, it's a C1 (8-bit) control,
        // which is strictly equivalent to `ESC` plus `c - 0x40`.
        if c > 0x7F {
            log::info!(target: LOG_TARGET, "executing C1 0x{:x} as ESC {}", c, (c - 0x40) as char);
            Self::esc_dispatch(
                handler,
                parser::Esc {
                    intermediates: &[],
                    r#final: c - 0x40,
                },
            );
            return;
        }

        let c0 = ansi::C0(c);
        if DEBUG {
            log::info!(target: LOG_TARGET, "execute: {:?}", c0);
        }
        match c0 {
            // We ignore SOH/STX: https://github.com/microsoft/terminal/issues/10786
            ansi::C0::NUL | ansi::C0::SOH | ansi::C0::STX => {}

            ansi::C0::ENQ => handler.vt(Action::Enquiry),
            ansi::C0::BEL => handler.vt(Action::Bell),
            ansi::C0::BS => handler.vt(Action::Backspace),
            ansi::C0::HT => handler.vt(Action::HorizontalTab(1)),
            ansi::C0::LF | ansi::C0::VT | ansi::C0::FF => handler.vt(Action::Linefeed),
            ansi::C0::CR => handler.vt(Action::CarriageReturn),
            ansi::C0::SO => handler.vt(Action::InvokeCharset(InvokeCharset {
                bank: charsets::ActiveSlot::GL,
                charset: charsets::Slots::G1,
                locking: false,
            })),
            ansi::C0::SI => handler.vt(Action::InvokeCharset(InvokeCharset {
                bank: charsets::ActiveSlot::GL,
                charset: charsets::Slots::G0,
                locking: false,
            })),

            _ => log_unsupported_once!(c, "invalid C0 character, ignoring: 0x{:x}", c),
        }
    }

    /// The single numeric parameter of a CSI, defaulting to 1 when
    /// omitted. Logs `invalid <what> command` and returns None when
    /// there is more than one parameter.
    #[inline(always)]
    fn one_param(input: &parser::Csi<'_>, what: &str) -> Option<u16> {
        match input.params.len() {
            0 => Some(1),
            1 => Some(input.params[0]),
            _ => {
                log::warn!(target: LOG_TARGET, "invalid {} command: {}", what, input);
                None
            }
        }
    }

    #[inline(always)]
    fn warn_intermediates(input: &parser::Csi<'_>) {
        log::warn!(
            target: LOG_TARGET,
            "ignoring unimplemented CSI {} with intermediates: {}",
            input.r#final as char,
            String::from_utf8_lossy(input.intermediates)
        );
    }

    #[inline(always)]
    fn csi_dispatch(handler: &mut H, input: parser::Csi<'_>) {
        // The branch hints here are based on real world data
        // which indicates that the most common CSI finals are:
        //
        // 1. m
        // 2. H
        // 3. K
        // 4. A
        // 5. C
        // 6. X
        // 7. l
        // 8. h
        // 9. r
        //
        // Together, these 9 finals make up about 96% of all
        // CSI sequences encountered in real world scenarios.
        //
        // ref: https://github.com/qwerasd205/asciinema-stats

        match input.r#final {
            // CUU - Cursor Up
            b'A' | b'k' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor up") else { return };
                    handler.vt(Action::CursorUp(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CUD - Cursor Down
            b'B' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor down") else { return };
                    handler.vt(Action::CursorDown(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CUF - Cursor Right
            b'C' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor right") else { return };
                    handler.vt(Action::CursorRight(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CUB - Cursor Left
            b'D' | b'j' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor left") else { return };
                    handler.vt(Action::CursorLeft(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CNL - Cursor Next Line
            b'E' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor up") else { return };
                    handler.vt(Action::CursorDown(CursorMovement { value }));
                    handler.vt(Action::CarriageReturn);
                }
                _ => Self::warn_intermediates(&input),
            },

            // CPL - Cursor Previous Line
            b'F' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "cursor down") else { return };
                    handler.vt(Action::CursorUp(CursorMovement { value }));
                    handler.vt(Action::CarriageReturn);
                }
                _ => Self::warn_intermediates(&input),
            },

            // HPA - Cursor Horizontal Position Absolute
            // TODO: test
            b'G' | b'`' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "HPA") else { return };
                    handler.vt(Action::CursorCol(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CUP - Set Cursor Position.
            // TODO: test
            b'H' | b'f' => match input.intermediates.len() {
                0 => {
                    let pos = match input.params.len() {
                        0 => CursorPos { row: 1, col: 1 },
                        1 => CursorPos {
                            row: input.params[0],
                            col: 1,
                        },
                        2 => CursorPos {
                            row: input.params[0],
                            col: input.params[1],
                        },
                        _ => {
                            log::warn!(target: LOG_TARGET, "invalid CUP command: {}", input);
                            return;
                        }
                    };
                    handler.vt(Action::CursorPos(pos));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CHT - Cursor Horizontal Tabulation
            b'I' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "horizontal tab") else { return };
                    handler.vt(Action::HorizontalTab(value));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Erase Display
            b'J' => {
                let protected = match input.intermediates.len() {
                    0 => Some(false),
                    1 => {
                        if input.intermediates[0] == b'?' {
                            Some(true)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let Some(protected) = protected else {
                    log::warn!(target: LOG_TARGET, "invalid erase display command: {}", input);
                    return;
                };

                let mode = match input.params.len() {
                    0 => Some(csi::EraseDisplay::Below),
                    1 => u8::try_from(input.params[0])
                        .ok()
                        .and_then(csi::EraseDisplay::from_int),
                    _ => None,
                };

                let Some(mode) = mode else {
                    log::warn!(target: LOG_TARGET, "invalid erase display command: {}", input);
                    return;
                };

                match mode {
                    csi::EraseDisplay::Below => handler.vt(Action::EraseDisplayBelow(protected)),
                    csi::EraseDisplay::Above => handler.vt(Action::EraseDisplayAbove(protected)),
                    csi::EraseDisplay::Complete => {
                        handler.vt(Action::EraseDisplayComplete(protected))
                    }
                    csi::EraseDisplay::Scrollback => {
                        handler.vt(Action::EraseDisplayScrollback(protected))
                    }
                    csi::EraseDisplay::ScrollComplete => {
                        handler.vt(Action::EraseDisplayScrollComplete(protected))
                    }
                }
            }

            // Erase Line
            b'K' => {
                let protected = match input.intermediates.len() {
                    0 => Some(false),
                    1 => {
                        if input.intermediates[0] == b'?' {
                            Some(true)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let Some(protected) = protected else {
                    log::warn!(target: LOG_TARGET, "invalid erase line command: {}", input);
                    return;
                };

                let mode = match input.params.len() {
                    0 => Some(csi::EraseLine::RIGHT),
                    1 => {
                        if input.params[0] < 3 {
                            Some(csi::EraseLine(input.params[0] as u8))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let Some(mode) = mode else {
                    log::warn!(target: LOG_TARGET, "invalid erase line command: {}", input);
                    return;
                };

                match mode {
                    csi::EraseLine::RIGHT => handler.vt(Action::EraseLineRight(protected)),
                    csi::EraseLine::LEFT => handler.vt(Action::EraseLineLeft(protected)),
                    csi::EraseLine::COMPLETE => handler.vt(Action::EraseLineComplete(protected)),
                    csi::EraseLine::RIGHT_UNLESS_PENDING_WRAP => {
                        handler.vt(Action::EraseLineRightUnlessPendingWrap(protected))
                    }
                    _ => {
                        log::warn!(target: LOG_TARGET, "invalid erase line mode: {:?}", mode);
                    }
                }
            }

            // IL - Insert Lines
            // TODO: test
            b'L' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "IL") else { return };
                    handler.vt(Action::InsertLines(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // DL - Delete Lines
            // TODO: test
            b'M' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "DL") else { return };
                    handler.vt(Action::DeleteLines(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Delete Character (DCH)
            b'P' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "delete characters") else {
                        return;
                    };
                    handler.vt(Action::DeleteChars(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Scroll Up (SD)
            b'S' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "scroll up") else { return };
                    handler.vt(Action::ScrollUp(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Scroll Down (SD)
            b'T' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "scroll down") else { return };
                    handler.vt(Action::ScrollDown(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Cursor Tabulation Control
            b'W' => match input.intermediates.len() {
                0 => {
                    if input.params.is_empty() || (input.params.len() == 1 && input.params[0] == 0)
                    {
                        handler.vt(Action::TabSet);
                        return;
                    }

                    if input.params.len() == 1 {
                        match input.params[0] {
                            0 => unreachable!(),
                            2 => handler.vt(Action::TabClearCurrent),
                            5 => handler.vt(Action::TabClearAll),
                            _ => {}
                        }
                    }

                    log::warn!(target: LOG_TARGET, "invalid cursor tabulation control: {}", input);
                }

                1 => {
                    if input.intermediates[0] == b'?'
                        && input.params.len() == 1
                        && input.params[0] == 5
                    {
                        handler.vt(Action::TabReset);
                    } else {
                        log::warn!(
                            target: LOG_TARGET,
                            "invalid cursor tabulation control: {}",
                            input
                        );
                    }
                }

                _ => Self::warn_intermediates(&input),
            },

            // Erase Characters (ECH)
            b'X' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "erase characters") else {
                        return;
                    };
                    handler.vt(Action::EraseChars(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // CHT - Cursor Horizontal Tabulation Back
            b'Z' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "horizontal tab back") else {
                        return;
                    };
                    handler.vt(Action::HorizontalTabBack(value));
                }
                _ => Self::warn_intermediates(&input),
            },

            // HPR - Cursor Horizontal Position Relative
            b'a' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "HPR") else { return };
                    handler.vt(Action::CursorColRelative(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // Repeat Previous Char (REP)
            b'b' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "print repeat") else { return };
                    handler.vt(Action::PrintRepeat(value as usize));
                }
                _ => Self::warn_intermediates(&input),
            },

            // c - Device Attributes (DA1)
            b'c' => {
                let req = match input.intermediates.len() {
                    0 => Some(device_attributes::Req::Primary),
                    1 => match input.intermediates[0] {
                        b'>' => Some(device_attributes::Req::Secondary),
                        b'=' => Some(device_attributes::Req::Tertiary),
                        _ => None,
                    },
                    _ => None,
                };

                if let Some(r) = req {
                    handler.vt(Action::DeviceAttributes(r));
                } else {
                    log_unsupported_once!(
                        if input.params.is_empty() { 0 } else { input.params[0] },
                        "invalid device attributes command: {}",
                        input
                    );
                }
            }

            // VPA - Cursor Vertical Position Absolute
            b'd' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "VPA") else { return };
                    handler.vt(Action::CursorRow(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // VPR - Cursor Vertical Position Relative
            b'e' => match input.intermediates.len() {
                0 => {
                    let Some(value) = Self::one_param(&input, "VPR") else { return };
                    handler.vt(Action::CursorRowRelative(CursorMovement { value }));
                }
                _ => Self::warn_intermediates(&input),
            },

            // TBC - Tab Clear
            // TODO: test
            b'g' => match input.intermediates.len() {
                0 => {
                    let mode = match input.params.len() {
                        0 => csi::TabClear::CURRENT,
                        1 => match u8::try_from(input.params[0]) {
                            Ok(v) => csi::TabClear(v),
                            Err(_) => {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid tab clear mode: {}",
                                    input.params[0]
                                );
                                return;
                            }
                        },
                        _ => {
                            log::warn!(target: LOG_TARGET, "invalid tab clear command: {}", input);
                            return;
                        }
                    };
                    match mode {
                        csi::TabClear::CURRENT => handler.vt(Action::TabClearCurrent),
                        csi::TabClear::ALL => handler.vt(Action::TabClearAll),
                        _ => log::warn!(target: LOG_TARGET, "unknown tab clear mode: {:?}", mode),
                    }
                }
                _ => Self::warn_intermediates(&input),
            },

            // SM - Set Mode
            b'h' => 'mode: {
                let ansi_mode = 'ansi: {
                    if input.intermediates.is_empty() {
                        break 'ansi true;
                    }
                    if input.intermediates.len() == 1 && input.intermediates[0] == b'?' {
                        break 'ansi false;
                    }

                    log::warn!(target: LOG_TARGET, "invalid set mode command: {}", input);
                    break 'mode;
                };

                for &mode_int in input.params {
                    if let Some(mode) = modes::mode_from_int(mode_int, ansi_mode) {
                        handler.vt(Action::SetMode(Mode { mode }));
                    } else {
                        log_unsupported_once!(mode_int, "unimplemented mode: {}", mode_int);
                    }
                }
            }

            // RM - Reset Mode
            b'l' => 'mode: {
                let ansi_mode = 'ansi: {
                    if input.intermediates.is_empty() {
                        break 'ansi true;
                    }
                    if input.intermediates.len() == 1 && input.intermediates[0] == b'?' {
                        break 'ansi false;
                    }

                    log::warn!(target: LOG_TARGET, "invalid set mode command: {}", input);
                    break 'mode;
                };

                for &mode_int in input.params {
                    if let Some(mode) = modes::mode_from_int(mode_int, ansi_mode) {
                        handler.vt(Action::ResetMode(Mode { mode }));
                    } else {
                        log_unsupported_once!(mode_int, "unimplemented mode: {}", mode_int);
                    }
                }
            }

            // SGR - Select Graphic Rendition
            b'm' => match input.intermediates.len() {
                0 => {
                    // This is the most common case.
                    let mut p = sgr::Parser {
                        params: input.params,
                        params_sep: input.params_sep,
                        idx: 0,
                    };
                    while let Some(attr) = p.next() {
                        handler.vt(Action::SetAttribute(attr));
                    }
                }

                1 => match input.intermediates[0] {
                    b'>' => 'blk: {
                        if input.params.is_empty() {
                            // Reset
                            handler.vt(Action::ModifyKeyFormat(ansi::ModifyKeyFormat::Legacy));
                            break 'blk;
                        }

                        let mut format = match input.params[0] {
                            0 => ansi::ModifyKeyFormat::Legacy,
                            1 => ansi::ModifyKeyFormat::CursorKeys,
                            2 => ansi::ModifyKeyFormat::FunctionKeys,
                            4 => ansi::ModifyKeyFormat::OtherKeysNone,
                            _ => {
                                log::warn!(target: LOG_TARGET, "invalid setModifyKeyFormat: {}", input);
                                break 'blk;
                            }
                        };

                        if input.params.len() > 2 {
                            log::warn!(target: LOG_TARGET, "invalid setModifyKeyFormat: {}", input);
                            break 'blk;
                        }

                        if input.params.len() == 2 {
                            match format {
                                // We don't support any of the subparams yet for these.
                                ansi::ModifyKeyFormat::Legacy => {}
                                ansi::ModifyKeyFormat::CursorKeys => {}
                                ansi::ModifyKeyFormat::FunctionKeys => {}

                                // We only support the numeric form.
                                ansi::ModifyKeyFormat::OtherKeysNone => {
                                    if input.params[1] == 2 {
                                        format = ansi::ModifyKeyFormat::OtherKeysNumeric;
                                    }
                                }
                                ansi::ModifyKeyFormat::OtherKeysNumericExcept => {}
                                ansi::ModifyKeyFormat::OtherKeysNumeric => {}
                            }
                        }

                        handler.vt(Action::ModifyKeyFormat(format));
                    }

                    other => log_unsupported_once!(
                        other,
                        "unknown CSI m with intermediate: {}",
                        other
                    ),
                },

                _ => {
                    // Nothing, but I wanted a place to put this comment:
                    // there are others forms of CSI m that have intermediates.
                    // `vim --clean` uses `CSI ? 4 m` and I don't know what
                    // that means.
                    Self::warn_intermediates(&input);
                }
            },

            // TODO: test
            b'n' => {
                // Handle deviceStatusReport first
                if input.intermediates.is_empty() || input.intermediates[0] == b'?' {
                    if input.params.len() != 1 {
                        log::warn!(
                            target: LOG_TARGET,
                            "invalid device status report command: {}",
                            input
                        );
                        return;
                    }

                    let question = 'question: {
                        if input.intermediates.is_empty() {
                            break 'question false;
                        }
                        if input.intermediates.len() == 1 && input.intermediates[0] == b'?' {
                            break 'question true;
                        }

                        log::warn!(target: LOG_TARGET, "invalid set mode command: {}", input);
                        return;
                    };

                    let Some(req) = device_status::req_from_int(input.params[0], question) else {
                        log::warn!(
                            target: LOG_TARGET,
                            "invalid device status report command: {}",
                            input
                        );
                        return;
                    };

                    handler.vt(Action::DeviceStatus(DeviceStatus { request: req }));
                    return;
                }

                // Handle other forms of CSI n
                match input.intermediates.len() {
                    0 => unreachable!(), // handled above

                    1 => match input.intermediates[0] {
                        b'>' => {
                            // This isn't strictly correct. CSI > n has parameters that
                            // control what exactly is being disabled. However, we
                            // only support reverting back to modify other keys in
                            // numeric except format.
                            handler.vt(Action::ModifyKeyFormat(
                                ansi::ModifyKeyFormat::OtherKeysNumericExcept,
                            ));
                        }

                        other => log::warn!(
                            target: LOG_TARGET,
                            "unknown CSI n with intermediate: {}",
                            other
                        ),
                    },

                    _ => Self::warn_intermediates(&input),
                }
            }

            // DECRQM - Request Mode, DECSTR - Soft Terminal Reset
            b'p' => match input.intermediates.len() {
                1 | 2 => 'decrqm: {
                    // DECSTR - Soft Terminal Reset
                    if input.intermediates.len() == 1 && input.intermediates[0] == b'!' {
                        if !input.params.is_empty() {
                            log::warn!(target: LOG_TARGET, "invalid DECSTR command: {}", input);
                            break 'decrqm;
                        }

                        handler.vt(Action::SoftReset);
                        break 'decrqm;
                    }

                    let ansi_mode = 'ansi: {
                        match input.intermediates.len() {
                            1 => {
                                if input.intermediates[0] == b'$' {
                                    break 'ansi true;
                                }
                            }
                            2 => {
                                if input.intermediates[0] == b'?' && input.intermediates[1] == b'$'
                                {
                                    break 'ansi false;
                                }
                            }
                            _ => {}
                        }

                        Self::warn_intermediates(&input);
                        break 'decrqm;
                    };

                    if input.params.len() != 1 {
                        log::warn!(target: LOG_TARGET, "invalid DECRQM command: {}", input);
                        break 'decrqm;
                    }

                    let mode_raw = input.params[0];
                    let mode = modes::mode_from_int(mode_raw, ansi_mode);
                    if let Some(m) = mode {
                        handler.vt(Action::RequestMode(Mode { mode: m }));
                    } else {
                        handler.vt(Action::RequestModeUnknown(RawMode {
                            mode: mode_raw,
                            ansi: ansi_mode,
                        }));
                    }
                }

                _ => Self::warn_intermediates(&input),
            },

            b'q' => match input.intermediates.len() {
                1 => match input.intermediates[0] {
                    // DECSCUSR - Select Cursor Style
                    // TODO: test
                    b' ' => {
                        let style = match input.params.len() {
                            0 => ansi::CursorStyle::Default,
                            1 => match input.params[0] {
                                0 => ansi::CursorStyle::Default,
                                1 => ansi::CursorStyle::BlinkingBlock,
                                2 => ansi::CursorStyle::SteadyBlock,
                                3 => ansi::CursorStyle::BlinkingUnderline,
                                4 => ansi::CursorStyle::SteadyUnderline,
                                5 => ansi::CursorStyle::BlinkingBar,
                                6 => ansi::CursorStyle::SteadyBar,
                                _ => {
                                    log::warn!(
                                        target: LOG_TARGET,
                                        "invalid cursor style value: {}",
                                        input.params[0]
                                    );
                                    return;
                                }
                            },
                            _ => {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid set cursor style command: {}",
                                    input
                                );
                                return;
                            }
                        };
                        handler.vt(Action::CursorStyle(style));
                    }

                    // DECSCA
                    b'"' => {
                        let mode = match input.params.len() {
                            0 => Some(ansi::ProtectedMode::Off),
                            1 => match input.params[0] {
                                0 | 2 => Some(ansi::ProtectedMode::Off),
                                1 => Some(ansi::ProtectedMode::Dec),
                                _ => None,
                            },
                            _ => None,
                        };

                        let Some(mode) = mode else {
                            log::warn!(
                                target: LOG_TARGET,
                                "invalid set protected mode command: {}",
                                input
                            );
                            return;
                        };

                        match mode {
                            ansi::ProtectedMode::Off => handler.vt(Action::ProtectedModeOff),
                            ansi::ProtectedMode::Iso => handler.vt(Action::ProtectedModeIso),
                            ansi::ProtectedMode::Dec => handler.vt(Action::ProtectedModeDec),
                        }
                    }

                    // XTVERSION
                    b'>' => handler.vt(Action::Xtversion),
                    _ => Self::warn_intermediates(&input),
                },

                _ => Self::warn_intermediates(&input),
            },

            b'r' => match input.intermediates.len() {
                // DECSTBM - Set Top and Bottom Margins
                0 => match input.params.len() {
                    0 => handler.vt(Action::TopAndBottomMargin(Margin {
                        top_left: 0,
                        bottom_right: 0,
                    })),
                    1 => handler.vt(Action::TopAndBottomMargin(Margin {
                        top_left: input.params[0],
                        bottom_right: 0,
                    })),
                    2 => handler.vt(Action::TopAndBottomMargin(Margin {
                        top_left: input.params[0],
                        bottom_right: input.params[1],
                    })),
                    _ => log::warn!(target: LOG_TARGET, "invalid DECSTBM command: {}", input),
                },

                1 => match input.intermediates[0] {
                    // Restore Mode
                    b'?' => {
                        for &mode_int in input.params {
                            if let Some(mode) = modes::mode_from_int(mode_int, false) {
                                handler.vt(Action::RestoreMode(Mode { mode }));
                            } else {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "unimplemented restore mode: {}",
                                    mode_int
                                );
                            }
                        }
                    }

                    _ => log::warn!(target: LOG_TARGET, "unknown CSI s with intermediate: {}", input),
                },

                _ => log::warn!(
                    target: LOG_TARGET,
                    "ignoring unimplemented CSI s with intermediates: {}",
                    input
                ),
            },

            b's' => match input.intermediates.len() {
                // DECSLRM
                0 => match input.params.len() {
                    // CSI S is ambiguous with zero params so we defer
                    // to our handler to do the proper logic. If mode 69
                    // is set, then we should invoke DECSLRM, otherwise
                    // we should invoke SC.
                    0 => handler.vt(Action::LeftAndRightMarginAmbiguous),
                    1 => handler.vt(Action::LeftAndRightMargin(Margin {
                        top_left: input.params[0],
                        bottom_right: 0,
                    })),
                    2 => handler.vt(Action::LeftAndRightMargin(Margin {
                        top_left: input.params[0],
                        bottom_right: input.params[1],
                    })),
                    _ => log::warn!(target: LOG_TARGET, "invalid DECSLRM command: {}", input),
                },

                1 => match input.intermediates[0] {
                    b'?' => {
                        for &mode_int in input.params {
                            if let Some(mode) = modes::mode_from_int(mode_int, false) {
                                handler.vt(Action::SaveMode(Mode { mode }));
                            } else {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "unimplemented save mode: {}",
                                    mode_int
                                );
                            }
                        }
                    }

                    // XTSHIFTESCAPE
                    b'>' => 'capture: {
                        let capture = match input.params.len() {
                            0 => false,
                            1 => match input.params[0] {
                                0 => false,
                                1 => true,
                                _ => {
                                    log::warn!(
                                        target: LOG_TARGET,
                                        "invalid XTSHIFTESCAPE command: {}",
                                        input
                                    );
                                    break 'capture;
                                }
                            },
                            _ => {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid XTSHIFTESCAPE command: {}",
                                    input
                                );
                                break 'capture;
                            }
                        };

                        handler.vt(Action::MouseShiftCapture(capture));
                    }

                    _ => log::warn!(target: LOG_TARGET, "unknown CSI s with intermediate: {}", input),
                },

                _ => log::warn!(
                    target: LOG_TARGET,
                    "ignoring unimplemented CSI s with intermediates: {}",
                    input
                ),
            },

            // XTWINOPS
            b't' => match input.intermediates.len() {
                0 => {
                    if input.params.is_empty() {
                        log::error!(target: LOG_TARGET, "ignoring CSI t with no parameters: {}", input);
                        return;
                    }

                    let size_report = |handler: &mut H, n: u16, style: csi::SizeReportStyle| {
                        if input.params.len() == 1 {
                            handler.vt(Action::SizeReport(style));
                        } else {
                            log::warn!(
                                target: LOG_TARGET,
                                "ignoring CSI {} t with extra parameters: {}",
                                n,
                                input
                            );
                        }
                    };

                    match input.params[0] {
                        8 => {
                            if input.params.len() <= 3 {
                                // resize the text area in characters
                                handler.vt(Action::ResizeWindow(ResizeWindow {
                                    rows: if input.params.len() > 1 { input.params[1] } else { 0 },
                                    columns: if input.params.len() > 2 {
                                        input.params[2]
                                    } else {
                                        0
                                    },
                                }));
                            } else {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "ignoring CSI 8 t with extra parameters: {}",
                                    input
                                );
                            }
                        }
                        // report the text area size in pixels
                        14 => size_report(handler, 14, csi::SizeReportStyle::Csi14T),
                        // report cell size in pixels
                        16 => size_report(handler, 16, csi::SizeReportStyle::Csi16T),
                        // report text area size in characters
                        18 => size_report(handler, 18, csi::SizeReportStyle::Csi18T),
                        // report screen size in characters
                        19 => size_report(handler, 19, csi::SizeReportStyle::Csi19T),
                        // report window title
                        21 => size_report(handler, 21, csi::SizeReportStyle::Csi21T),
                        number @ (22 | 23) => {
                            if (input.params.len() == 2 || input.params.len() == 3)
                                // we only support window title
                                && (input.params[1] == 0 || input.params[1] == 2)
                            {
                                // push/pop title
                                let index: u16 = if input.params.len() == 3 {
                                    input.params[2]
                                } else {
                                    0
                                };
                                match number {
                                    22 => handler.vt(Action::TitlePush(index)),
                                    23 => handler.vt(Action::TitlePop(index)),
                                    _ => unreachable!(),
                                }
                            } else {
                                log_unsupported_once!(
                                    input.params[0],
                                    "ignoring CSI 22/23 t with extra parameters: {}",
                                    input
                                );
                            }
                        }
                        _ => log_unsupported_once!(
                            input.params[0],
                            "ignoring CSI t with unimplemented parameter: {}",
                            input
                        ),
                    }
                }
                _ => log::warn!(
                    target: LOG_TARGET,
                    "ignoring unimplemented CSI t with intermediates: {}",
                    input
                ),
            },

            b'u' => match input.intermediates.len() {
                0 => handler.vt(Action::RestoreCursor),

                // Kitty keyboard protocol
                1 => match input.intermediates[0] {
                    b'?' => handler.vt(Action::KittyKeyboardQuery),

                    b'>' => 'push: {
                        let flags: u8 = if input.params.len() == 1 {
                            if input.params[0] > 0x1F {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid pushKittyKeyboard command: {}",
                                    input
                                );
                                break 'push;
                            }
                            input.params[0] as u8
                        } else {
                            0
                        };

                        handler.vt(Action::KittyKeyboardPush(KittyKeyboardFlags {
                            flags: kitty::KeyFlags::from_int(flags),
                        }));
                    }

                    b'<' => {
                        let number: u16 = if input.params.len() == 1 {
                            input.params[0]
                        } else {
                            1
                        };

                        handler.vt(Action::KittyKeyboardPop(number));
                    }

                    b'=' => 'set: {
                        let flags: u8 = if !input.params.is_empty() {
                            if input.params[0] > 0x1F {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid setKittyKeyboard command: {}",
                                    input
                                );
                                break 'set;
                            }
                            input.params[0] as u8
                        } else {
                            0
                        };

                        let number: u16 = if input.params.len() >= 2 {
                            input.params[1]
                        } else {
                            1
                        };

                        let kitty_flags = KittyKeyboardFlags {
                            flags: kitty::KeyFlags::from_int(flags),
                        };
                        match number {
                            1 => handler.vt(Action::KittyKeyboardSet(kitty_flags)),
                            2 => handler.vt(Action::KittyKeyboardSetOr(kitty_flags)),
                            3 => handler.vt(Action::KittyKeyboardSetNot(kitty_flags)),
                            _ => {
                                log::warn!(
                                    target: LOG_TARGET,
                                    "invalid setKittyKeyboard command: {}",
                                    input
                                );
                                break 'set;
                            }
                        }
                    }

                    _ => log::warn!(target: LOG_TARGET, "unknown CSI s with intermediate: {}", input),
                },

                _ => log::warn!(target: LOG_TARGET, "ignoring unimplemented CSI u: {}", input),
            },

            // ICH - Insert Blanks
            b'@' => match input.intermediates.len() {
                0 => {
                    let value = match input.params.len() {
                        0 => 1,
                        1 => input.params[0].max(1),
                        _ => {
                            log::warn!(target: LOG_TARGET, "invalid ICH command: {}", input);
                            return;
                        }
                    };
                    handler.vt(Action::InsertBlanks(value as usize));
                }

                _ => log::warn!(target: LOG_TARGET, "ignoring unimplemented CSI @: {}", input),
            },

            b'y' => match input.intermediates.len() {
                1 => match input.intermediates[0] {
                    // DECRQCRA - Request Checksum of Rectangular Area.
                    // The page number (the second parameter) is ignored.
                    b'*' => {
                        if input.params.len() > 6 {
                            log::warn!(target: LOG_TARGET, "invalid DECRQCRA command: {}", input);
                            return;
                        }

                        let mut params = [0u16; 6];
                        params[..input.params.len()].copy_from_slice(input.params);
                        handler.vt(Action::RequestXtChecksum(xt_checksum::Request {
                            id: params[0],
                            top: params[2],
                            left: params[3],
                            bottom: params[4],
                            right: params[5],
                        }));
                    }

                    // XTCHECKSUM - Select checksum extension. Bits
                    // beyond the ones we know are ignored, as in xterm.
                    b'#' => {
                        if input.params.len() > 1 {
                            log::warn!(target: LOG_TARGET, "invalid XTCHECKSUM command: {}", input);
                            return;
                        }

                        let bits: u16 = if input.params.len() == 1 {
                            input.params[0]
                        } else {
                            0
                        };
                        handler.vt(Action::XtChecksumExtension(XtChecksumExtension {
                            flags: xt_checksum::Flags::from_bits((bits & 0x1F) as u8),
                        }));
                    }

                    _ => log::warn!(target: LOG_TARGET, "ignoring unimplemented CSI y: {}", input),
                },

                _ => log::warn!(target: LOG_TARGET, "ignoring unimplemented CSI y: {}", input),
            },

            b'}' => 'decsasd: {
                if input.intermediates.len() != 1 {
                    log::warn!(target: LOG_TARGET, "unimplemented CSI callback: {}", input);
                    break 'decsasd;
                }

                match input.intermediates[0] {
                    // DECSASD - Select Active Status Display
                    b'$' => {}

                    // DECIC - Insert Column
                    b'\'' => {
                        let value = match input.params.len() {
                            0 => 1,
                            1 => input.params[0].max(1),
                            _ => {
                                log::warn!(target: LOG_TARGET, "invalid DECIC command: {}", input);
                                break 'decsasd;
                            }
                        };
                        handler.vt(Action::InsertColumns(value as usize));
                        break 'decsasd;
                    }

                    _ => {
                        log::warn!(target: LOG_TARGET, "unimplemented CSI callback: {}", input);
                        break 'decsasd;
                    }
                }

                if input.params.len() != 1 {
                    log::warn!(target: LOG_TARGET, "unimplemented CSI callback: {}", input);
                    break 'decsasd;
                }

                let display = match input.params[0] {
                    0 => ansi::StatusDisplay::Main,
                    1 => ansi::StatusDisplay::StatusLine,
                    _ => {
                        log::warn!(target: LOG_TARGET, "unimplemented CSI callback: {}", input);
                        break 'decsasd;
                    }
                };

                handler.vt(Action::ActiveStatusDisplay(display));
            }

            // DECDC - Delete Column
            b'~' => match input.intermediates.len() {
                1 => match input.intermediates[0] {
                    b'\'' => {
                        let value = match input.params.len() {
                            0 => 1,
                            1 => input.params[0].max(1),
                            _ => {
                                log::warn!(target: LOG_TARGET, "invalid DECDC command: {}", input);
                                return;
                            }
                        };
                        handler.vt(Action::DeleteColumns(value as usize));
                    }

                    _ => log::warn!(target: LOG_TARGET, "unimplemented CSI action: {}", input),
                },

                _ => log::warn!(target: LOG_TARGET, "unimplemented CSI action: {}", input),
            },

            _ => log::warn!(target: LOG_TARGET, "unimplemented CSI action: {}", input),
        }
    }

    #[inline(always)]
    fn osc_dispatch(handler: &mut H, cmd: osc::Command<'_>) {
        // The branch hints here are based on real world data
        // which indicates that the most common OSC commands are:
        //
        // 1. hyperlink_end
        // 2. change_window_title
        // 3. change_window_icon
        // 4. hyperlink_start
        // 5. report_pwd
        // 6. color_operation
        // 7. semantic_prompt
        //
        // Together, these 7 commands make up about 96% of all
        // OSC commands encountered in real world scenarios.
        //
        // ref: https://github.com/qwerasd205/asciinema-stats

        match cmd {
            osc::Command::SemanticPrompt(sp) => handler.vt(Action::SemanticPrompt(sp)),

            osc::Command::ChangeWindowTitle(title) => {
                if std::str::from_utf8(title).is_err() {
                    log::warn!(
                        target: LOG_TARGET,
                        "change title request: invalid utf-8, ignoring request"
                    );
                    return;
                }

                handler.vt(Action::WindowTitle(WindowTitle { title }));
            }

            osc::Command::ChangeWindowIcon(icon) => {
                log_unsupported_once!(
                    0,
                    "OSC 1 (change icon) received and ignored icon={}",
                    String::from_utf8_lossy(icon)
                );
            }

            osc::Command::ClipboardContents(clip) => {
                handler.vt(Action::ClipboardContents(ClipboardContents {
                    kind: clip.kind,
                    data: clip.data,
                    terminator: clip.terminator,
                }));
            }

            osc::Command::ReportPwd(v) => handler.vt(Action::ReportPwd(ReportPwd { url: v.value })),

            osc::Command::MouseShape(v) => {
                if v.value.is_empty() {
                    handler.vt(Action::MouseShapeReset);
                    return;
                }

                let Some(shape) = MouseShape::from_string(v.value) else {
                    log::warn!(
                        target: LOG_TARGET,
                        "unknown cursor shape: {}",
                        String::from_utf8_lossy(v.value)
                    );
                    return;
                };

                handler.vt(Action::MouseShape(shape));
            }

            osc::Command::ColorOperation(v) => {
                handler.vt(Action::ColorOperation(ColorOperation {
                    op: v.op,
                    requests: v.requests,
                    terminator: v.terminator,
                }));
            }

            osc::Command::KittyColorProtocol(v) => handler.vt(Action::KittyColorReport(v)),

            osc::Command::ShowDesktopNotification(v) => {
                handler.vt(Action::ShowDesktopNotification(ShowDesktopNotification {
                    title: v.title,
                    body: v.body,
                }));
            }

            osc::Command::HyperlinkStart(v) => {
                handler.vt(Action::StartHyperlink(StartHyperlink { uri: v.uri, id: v.id }));
            }

            osc::Command::HyperlinkEnd => handler.vt(Action::EndHyperlink),

            osc::Command::ConemuProgressReport(v) => handler.vt(Action::ProgressReport(v)),

            osc::Command::KittyClipboardProtocol(v) => handler.vt(Action::KittyClipboard(v)),

            osc::Command::KittyDndProtocol(v) => handler.vt(Action::KittyDnd(v)),

            osc::Command::ProgramStatus(v) => handler.vt(Action::ProgramStatus(v)),

            osc::Command::Unknown(v) => handler.vt(Action::OscUnknown(v)),

            cmd @ (osc::Command::ConemuSleep(_)
            | osc::Command::ConemuShowMessageBox(_)
            | osc::Command::ConemuChangeTabTitle(_)
            | osc::Command::ConemuWaitInput
            | osc::Command::ConemuGuimacro(_)
            | osc::Command::ConemuComment(_)
            | osc::Command::ConemuXtermEmulation(_)
            | osc::Command::ConemuOutputEnvironmentVariable(_)
            | osc::Command::ConemuRunProcess(_)
            | osc::Command::KittyTextSizing(_)
            | osc::Command::KittyDesktopNotification(_)
            | osc::Command::HierarchicalContext(_)) => {
                log::debug!(target: LOG_TARGET, "unimplemented OSC callback: {:?}", cmd);
            }

            osc::Command::Invalid => {
                // This is an invalid internal state, not an invalid OSC
                // string being parsed. We shouldn't see this.
                log::warn!(target: LOG_TARGET, "invalid OSC, should never happen");
            }
        }
    }

    #[inline(always)]
    fn configure_charset(handler: &mut H, intermediates: &[u8], set: charsets::Charset) {
        if intermediates.len() != 1 {
            log::warn!(target: LOG_TARGET, "invalid charset intermediate: {:?}", intermediates);
            return;
        }

        let slot = match intermediates[0] {
            // TODO: support slots '-', '.', '/'
            b'(' => charsets::Slots::G0,
            b')' => charsets::Slots::G1,
            b'*' => charsets::Slots::G2,
            b'+' => charsets::Slots::G3,
            _ => {
                log::warn!(target: LOG_TARGET, "invalid charset intermediate: {:?}", intermediates);
                return;
            }
        };

        handler.vt(Action::ConfigureCharset(ConfigureCharset { slot, charset: set }));
    }

    #[inline(always)]
    fn esc_dispatch(handler: &mut H, action: parser::Esc<'_>) {
        // The branch hints here are based on real world data
        // which indicates that the most common ESC finals are:
        //
        // 1. B
        // 2. \
        // 3. 0
        // 4. M
        // 5. 8
        // 6. 7
        // 7. >
        // 8. =
        //
        // Together, these 8 finals make up nearly 99% of all
        // ESC sequences encountered in real world scenarios.
        //
        // ref: https://github.com/qwerasd205/asciinema-stats

        let no_intermediates = action.intermediates.is_empty();
        match action.r#final {
            // Charsets
            b'B' => Self::configure_charset(handler, action.intermediates, charsets::Charset::Ascii),
            b'A' => {
                Self::configure_charset(handler, action.intermediates, charsets::Charset::British)
            }
            b'0' => Self::configure_charset(
                handler,
                action.intermediates,
                charsets::Charset::DecSpecial,
            ),

            // DECSC - Save Cursor
            b'7' => {
                if no_intermediates {
                    handler.vt(Action::SaveCursor);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid command: {}", action);
                }
            }

            b'8' => 'blk: {
                match action.intermediates.len() {
                    // DECRC - Restore Cursor
                    0 => {
                        handler.vt(Action::RestoreCursor);
                        break 'blk;
                    }

                    1 => {
                        // DECALN - Fill Screen with E
                        if action.intermediates[0] == b'#' {
                            handler.vt(Action::Decaln);
                            break 'blk;
                        }
                    }

                    _ => {} // fall through
                }

                log_unsupported_once!(
                    action.r#final,
                    "unimplemented ESC action: {}",
                    action
                );
            }

            // DECBI - Back Index
            b'6' => {
                if no_intermediates {
                    handler.vt(Action::BackIndex);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid back index command: {}", action);
                }
            }

            // DECFI - Forward Index
            b'9' => {
                if no_intermediates {
                    handler.vt(Action::ForwardIndex);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid forward index command: {}", action);
                }
            }

            // IND - Index
            b'D' => {
                if no_intermediates {
                    handler.vt(Action::Index);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid index command: {}", action);
                }
            }

            // NEL - Next Line
            b'E' => {
                if no_intermediates {
                    handler.vt(Action::NextLine);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid next line command: {}", action);
                }
            }

            // HTS - Horizontal Tab Set
            b'H' => {
                if no_intermediates {
                    handler.vt(Action::TabSet);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid tab set command: {}", action);
                }
            }

            // RI - Reverse Index
            b'M' => {
                if no_intermediates {
                    handler.vt(Action::ReverseIndex);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid reverse index command: {}", action);
                }
            }

            // SS2 - Single Shift 2
            b'N' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GL,
                        charset: charsets::Slots::G2,
                        locking: true,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "invalid single shift 2 command: {}", action);
                }
            }

            // SS3 - Single Shift 3
            b'O' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GL,
                        charset: charsets::Slots::G3,
                        locking: true,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "invalid single shift 3 command: {}", action);
                }
            }

            // SPA - Start of Guarded Area
            b'V' => {
                if no_intermediates {
                    handler.vt(Action::ProtectedModeIso);
                } else {
                    log::warn!(target: LOG_TARGET, "unimplemented ESC callback: {}", action);
                }
            }

            // EPA - End of Guarded Area
            b'W' => {
                if no_intermediates {
                    handler.vt(Action::ProtectedModeOff);
                } else {
                    log::warn!(target: LOG_TARGET, "unimplemented ESC callback: {}", action);
                }
            }

            // DECID
            b'Z' => {
                if no_intermediates {
                    handler.vt(Action::DeviceAttributes(device_attributes::Req::Primary));
                } else {
                    log::warn!(target: LOG_TARGET, "unimplemented ESC callback: {}", action);
                }
            }

            // RIS - Full Reset
            b'c' => {
                if no_intermediates {
                    handler.vt(Action::FullReset);
                } else {
                    log::warn!(target: LOG_TARGET, "invalid full reset command: {}", action);
                }
            }

            // LS2 - Locking Shift 2
            b'n' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GL,
                        charset: charsets::Slots::G2,
                        locking: false,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "invalid single shift 2 command: {}", action);
                }
            }

            // LS3 - Locking Shift 3
            b'o' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GL,
                        charset: charsets::Slots::G3,
                        locking: false,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "invalid single shift 3 command: {}", action);
                }
            }

            // LS1R - Locking Shift 1 Right
            b'~' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GR,
                        charset: charsets::Slots::G1,
                        locking: false,
                    }));
                } else {
                    log::warn!(
                        target: LOG_TARGET,
                        "invalid locking shift 1 right command: {}",
                        action
                    );
                }
            }

            // LS2R - Locking Shift 2 Right
            b'}' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GR,
                        charset: charsets::Slots::G2,
                        locking: false,
                    }));
                } else {
                    log::warn!(
                        target: LOG_TARGET,
                        "invalid locking shift 2 right command: {}",
                        action
                    );
                }
            }

            // LS3R - Locking Shift 3 Right
            b'|' => {
                if no_intermediates {
                    handler.vt(Action::InvokeCharset(InvokeCharset {
                        bank: charsets::ActiveSlot::GR,
                        charset: charsets::Slots::G3,
                        locking: false,
                    }));
                } else {
                    log::warn!(
                        target: LOG_TARGET,
                        "invalid locking shift 3 right command: {}",
                        action
                    );
                }
            }

            // Set application keypad mode
            b'=' => {
                if no_intermediates {
                    handler.vt(Action::SetMode(Mode {
                        mode: modes::Mode::KeypadKeys,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "unimplemented setMode: {}", action);
                }
            }

            // Reset application keypad mode
            b'>' => {
                if no_intermediates {
                    handler.vt(Action::ResetMode(Mode {
                        mode: modes::Mode::KeypadKeys,
                    }));
                } else {
                    log::warn!(target: LOG_TARGET, "unimplemented setMode: {}", action);
                }
            }

            // Sets ST (string terminator). We don't have to do anything
            // because our parser always accepts ST.
            b'\\' => {}

            _ => log_unsupported_once!(action.r#final, "unimplemented ESC action: {}", action),
        }
    }
}

/// A point in the byte stream where `Stream::next_slice_until` can stop.
///
/// Each boundary is reached by exactly one byte. The stream stops right
/// after that byte and describes it in a `Stop`, so you learn where a
/// string starts or ends from the same parse that handles it, without
/// parsing any byte twice.
///
/// Most boundaries mark the first or last byte of a control string: an
/// APC, DCS or OSC sequence, made of an introducer, some content, and a
/// terminator. `Ground` is different. It marks the state between
/// sequences rather than a byte inside one.
///
/// Terminal protocols spell control bytes two ways, and both are
/// recognized here. A 7-bit introducer is ESC followed by one byte, such
/// as `ESC ]` for OSC. The 8-bit form is a single byte in the range 0x80
/// to 0x9F, such as 0x9D for OSC. The string terminator ST is `ESC \` or
/// the single byte 0x9C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Boundary {
    /// The stream is not inside any escape sequence or partial UTF-8
    /// character. This is a safe point to insert your own sequences.
    ///
    /// Ground is a state, not an event. If the stream is already at
    /// ground when you ask for it, the call stops immediately and
    /// consumes nothing.
    Ground,

    /// The byte that started an APC string: `_` after ESC, or 0x9F.
    ///
    /// SOS (`X` or 0x98) and PM (`^` or 0x9E) strings report this too,
    /// because the parser handles all three the same way. Use
    /// `StopStart::byte` to tell them apart.
    ApcStart,

    /// The byte that ended an APC, SOS or PM string.
    ApcEnd,

    /// The byte that started a DCS string: `P` after ESC, or 0x90.
    ///
    /// A DCS has a header before its payload: parameters, intermediates
    /// and a final byte, in the same shape as a CSI sequence. The stop
    /// lands on the introducer, so the header follows it. The handler's
    /// `dcs_hook` action still marks where the payload begins. A 0x90
    /// inside the header restarts the header without leaving the
    /// string, so it does not stop.
    DcsStart,

    /// The byte that ended a DCS string, whether it was still in its
    /// header or already in its payload. A DCS cut short in its header
    /// reports this without the handler ever seeing a `dcs_hook`.
    DcsEnd,

    /// The byte that started an OSC string: `]` after ESC, or 0x9D.
    OscStart,

    /// The byte that ended an OSC string. This is reported for every
    /// OSC, including ones with a number the handler does not know.
    OscEnd,
}

/// A set of boundaries to stop at (Zig: `Boundary.Set`, a
/// `std.EnumSet(Boundary)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BoundarySet {
    bits: u8,
}

impl BoundarySet {
    pub const EMPTY: BoundarySet = BoundarySet { bits: 0 };

    pub const fn init_one(b: Boundary) -> BoundarySet {
        BoundarySet { bits: 1 << b as u8 }
    }

    pub const fn init_many(bs: &[Boundary]) -> BoundarySet {
        let mut bits = 0;
        let mut i = 0;
        while i < bs.len() {
            bits |= 1 << bs[i] as u8;
            i += 1;
        }
        BoundarySet { bits }
    }

    #[inline(always)]
    pub const fn contains(&self, b: Boundary) -> bool {
        self.bits & (1 << b as u8) != 0
    }

    #[inline(always)]
    pub fn insert(&mut self, b: Boundary) {
        self.bits |= 1 << b as u8;
    }

    #[inline(always)]
    pub fn remove(&mut self, b: Boundary) {
        self.bits &= !(1 << b as u8);
    }

    pub const fn union_with(self, other: BoundarySet) -> BoundarySet {
        BoundarySet {
            bits: self.bits | other.bits,
        }
    }

    pub fn eql(&self, other: BoundarySet) -> bool {
        self.bits == other.bits
    }
}

/// The kind of control string that started or ended. `Apc` covers
/// SOS and PM too, see `Boundary::ApcStart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StringKind {
    Apc = 0,
    Dcs = 1,
    Osc = 2,
}

/// How a control string started.
///
/// - `Esc`: ESC followed by `_` (APC), `X` (SOS), `^` (PM), `P` (DCS)
///   or `]` (OSC). The stop's `consumed` count includes only the final
///   byte, because the ESC may have been consumed by an earlier call.
/// - `C1`: a single 8-bit byte, 0x9F (APC), 0x98 (SOS), 0x9E (PM),
///   0x90 (DCS) or 0x9D (OSC). At ground these bytes are decoded as
///   UTF-8 instead, so they only start a string when they arrive
///   inside another escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Introducer {
    Esc = 0,
    C1 = 1,
}

/// How a control string ended.
///
/// - `StEsc`: ESC, the first byte of the terminator `ESC \`. The ESC
///   is consumed but the byte after it is not. That byte is handled
///   as a normal escape sequence by the next call.
/// - `StC1`: 0x9C, the 8-bit terminator.
/// - `Bel`: BEL (0x07). Only an OSC ends this way.
/// - `Cancelled`: CAN (0x18) or SUB (0x1A).
/// - `Aborted`: any other 8-bit control byte. It either runs as a
///   control or starts a new sequence of its own. See `Stop::reached`.
///
/// `StEsc`, `StC1` and `Bel` are proper terminators. `Cancelled`
/// and `Aborted` cut the string short.
///
/// Inside an OSC, and inside a DCS once its header is complete, every
/// 8-bit byte is content. Only ESC, CAN, SUB and (for OSC) BEL end
/// those strings, so `StC1` and `Aborted` are only ever reported for
/// an APC or for a DCS still in its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EndReason {
    StEsc = 0,
    StC1 = 1,
    Cancelled = 2,
    Aborted = 3,
    Bel = 4,
}

/// Where `Stream::next_slice_until` stopped and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    /// The number of input bytes consumed, including the byte that
    /// reached the boundary. This is 0 when the stream was already at
    /// ground and `Ground` was requested.
    pub consumed: usize,

    /// Every requested boundary that the stopping byte reached.
    ///
    /// This is usually one boundary. It holds two when a single byte
    /// does two things:
    ///
    /// - It ends a string and returns to ground. BEL, CAN, SUB, 0x9C
    ///   and the other 8-bit controls all do this, so an end stop also
    ///   includes `Ground` when you asked for it.
    /// - It ends one string and starts another. An 8-bit introducer
    ///   of a different kind does this inside an APC, or inside a DCS
    ///   header: 0x90 or 0x9D in an APC ends it and starts a DCS or
    ///   OSC, and 0x9F, 0x98, 0x9E or 0x9D in a DCS header ends it
    ///   and starts an APC or OSC. Both `start` and `end` are set and
    ///   the end's reason is `Aborted`.
    ///
    /// For example, in `ESC _ a 0x9D 2 ; t BEL` the 0x9D ends the APC
    /// and starts an OSC. Asking for `ApcEnd` and `OscStart` gives
    /// one stop on the 0x9D with both in `reached`, and the OSC's
    /// content `2;t` begins right after it.
    ///
    /// The second case never happens inside an OSC, or inside a DCS
    /// past its header, because 8-bit bytes are content there. It
    /// also does not happen for 0x98, 0x9E or 0x9F inside an APC,
    /// which the parser drops without a stop. A start and an end of
    /// the same kind never coincide.
    pub reached: BoundarySet,

    /// Set when `reached` contains a string start.
    pub start: Option<StopStart>,

    /// Set when `reached` contains a string end.
    pub end: Option<StopEnd>,
}

/// (Zig: `Boundary.Stop.Start`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopStart {
    pub kind: StringKind,
    pub introducer: Introducer,

    /// The byte that started the string, such as `_`, `P`, `]`
    /// or 0x9F.
    pub byte: u8,
}

/// (Zig: `Boundary.Stop.End`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopEnd {
    pub kind: StringKind,
    pub reason: EndReason,

    /// The byte that ended the string, such as ESC, BEL or 0x9C.
    pub byte: u8,
}

/// Scratch state for `Stream::next_slice_until`. The parsing functions
/// take a `const UNTIL: bool` and a `&mut Until`, which is only consulted
/// in until mode, so the plain `next_slice` instantiation contains none of
/// the boundary checks.
#[derive(Debug)]
struct Until {
    /// The boundaries the caller asked to stop at.
    boundaries: BoundarySet,

    /// The start address of the full input. Every parsing function sees
    /// a subslice of it, so the byte that reaches a boundary is turned
    /// into a consumed count from its address, without threading
    /// offsets back up through every return value.
    base: usize,

    /// The stop being built. None until a byte reaches a boundary.
    stop: Option<Stop>,
}

impl Until {
    /// The placeholder passed when not in until mode. Never consulted.
    #[inline(always)]
    fn disabled() -> Until {
        Until {
            boundaries: BoundarySet::EMPTY,
            base: 0,
            stop: None,
        }
    }

    /// The string kind a parser state belongs to, or None for a state
    /// that is not inside a control string. The DCS states form one
    /// family: a string starts when the parser enters a family from
    /// outside it and ends when the parser leaves it, so moving
    /// between the DCS header and payload states is not a boundary.
    fn family(state: ParserState) -> Option<StringKind> {
        match state {
            ParserState::SosPmApcString => Some(StringKind::Apc),
            ParserState::DcsEntry
            | ParserState::DcsParam
            | ParserState::DcsIntermediate
            | ParserState::DcsPassthrough
            | ParserState::DcsIgnore => Some(StringKind::Dcs),
            ParserState::OscString => Some(StringKind::Osc),
            _ => None,
        }
    }

    /// Note that `c` moved the parser from state `old` to state `new`,
    /// and record a string end, a string start, or both, if that
    /// transition left or entered a string family.
    fn transition(&mut self, old: ParserState, new: ParserState, c: u8) {
        if old == new {
            return;
        }
        let from = Self::family(old);
        let to = Self::family(new);
        if let Some(kind) = from {
            if to != Some(kind) {
                self.string_end(kind, c);
            }
        }
        if let Some(kind) = to {
            if from != Some(kind) {
                self.string_start(kind, c);
            }
        }
    }

    /// Note that `c` entered a string of the given kind.
    fn string_start(&mut self, kind: StringKind, c: u8) {
        let boundary = match kind {
            StringKind::Apc => Boundary::ApcStart,
            StringKind::Dcs => Boundary::DcsStart,
            StringKind::Osc => Boundary::OscStart,
        };
        if !self.boundaries.contains(boundary) {
            return;
        }
        let stop = self.stop_ptr();
        stop.reached.insert(boundary);
        stop.start = Some(StopStart {
            kind,
            introducer: if c >= 0x80 { Introducer::C1 } else { Introducer::Esc },
            byte: c,
        });
    }

    /// Note that `c` left a string of the given kind.
    fn string_end(&mut self, kind: StringKind, c: u8) {
        let boundary = match kind {
            StringKind::Apc => Boundary::ApcEnd,
            StringKind::Dcs => Boundary::DcsEnd,
            StringKind::Osc => Boundary::OscEnd,
        };
        if !self.boundaries.contains(boundary) {
            return;
        }
        let stop = self.stop_ptr();
        stop.reached.insert(boundary);
        stop.end = Some(StopEnd {
            kind,
            reason: match c {
                0x07 => EndReason::Bel,
                0x18 | 0x1A => EndReason::Cancelled,
                0x1B => EndReason::StEsc,
                0x9C => EndReason::StC1,
                _ => EndReason::Aborted,
            },
            byte: c,
        });
    }

    fn stop_ptr(&mut self) -> &mut Stop {
        self.stop.get_or_insert(Stop {
            consumed: 0,
            reached: BoundarySet::EMPTY,
            start: None,
            end: None,
        })
    }
}


/// Decides whether an unsupported-input message should be logged
/// (Zig: `logUnsupportedOnce`).
///
/// These messages are emitted in response to input that the terminal
/// application controls, so a misbehaving (or merely chatty) program
/// can trigger the same message millions of times, e.g. by toggling
/// an unimplemented mode on every frame. Each log call has a real
/// throughput cost (formatting plus a blocking write per message)
/// while adding no diagnostic value beyond the first occurrence.
///
/// The keys seen so far are tracked in a small fixed table (64 bytes)
/// instantiated per call site. Real streams only ever produce a handful
/// of distinct unsupported values per site, so if the table ever fills,
/// messages for further new values are suppressed as well: by that point
/// the log already shows this class of problem and unbounded distinct
/// values would flood it anyway.
fn should_log_unsupported(seen: &[AtomicU32; 16], key: u16) -> bool {
    // u32 slots so every u16 key is representable alongside an empty
    // sentinel.
    const EMPTY: u32 = u32::MAX;
    let key = key as u32;

    // The atomics make concurrent streams safe: slots are only ever
    // claimed, never changed, so the scan can stop at the first empty
    // slot. The worst case race is a benign duplicate message.
    for slot in seen {
        let cur = slot.load(Ordering::Acquire);
        if cur == key {
            return false; // already logged
        }
        if cur != EMPTY {
            continue; // other key, keep scanning
        }

        // Empty slot: claim it for this key and log.
        match slot.compare_exchange(EMPTY, key, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            // Lost the race: suppress if it was to the same key, keep
            // scanning otherwise.
            Err(actual) => {
                if actual == key {
                    return false;
                }
            }
        }
    }

    // table full: suppress new values too
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::stream_continuation::FixedWriter;

    fn s<H: Handler + Default>() -> Stream<H> {
        Stream::new(H::default())
    }

    #[test]
    fn action() {
        // Zig forces the C type to be reified here. The Rust port checks
        // that every key round-trips through its name instead.
        assert_eq!("print", Key::Print.as_str());
        assert_eq!("delete_columns", Key::DeleteColumns.as_str());
        assert_eq!(Key::Bell, Action::Bell.key());
    }

    #[test]
    fn stream_print() {
        #[derive(Default)]
        struct H {
            c: Option<u32>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::Print(p) = action {
                    self.c = Some(p.cp);
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next(b'x');
        assert_eq!(b'x' as u32, s.handler.c.unwrap());
    }

    /// Records the last printed codepoint, from print or print_slice.
    #[derive(Default)]
    struct LastPrint {
        c: Option<u32>,
    }
    impl Handler for LastPrint {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::Print(p) => self.c = Some(p.cp),
                Action::PrintSlice(v) => self.c = Some(v.cps[v.cps.len() - 1]),
                _ => {}
            }
        }
    }

    #[test]
    fn simd_print_invalid_utf_8() {
        let mut s: Stream<LastPrint> = s();
        s.next_slice(&[0xFF]);
        assert_eq!(0xFFFD, s.handler.c.unwrap());
    }

    #[test]
    fn simd_complete_incomplete_utf_8() {
        let mut s: Stream<LastPrint> = s();
        s.next_slice(&[0xE0]); // 3 byte
        assert!(s.handler.c.is_none());
        s.next_slice(&[0xA0]); // still incomplete
        assert!(s.handler.c.is_none());
        s.next_slice(&[0x80]);
        assert_eq!(0x800, s.handler.c.unwrap());
    }

    /// Records every printed codepoint.
    #[derive(Default)]
    struct PrintBuf {
        buf: Vec<u32>,
    }
    impl Handler for PrintBuf {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::Print(p) => self.buf.push(p.cp),
                Action::PrintSlice(v) => self.buf.extend_from_slice(v.cps),
                _ => {}
            }
        }
    }

    #[test]
    fn stream_ground_state_c0_controls_are_executed_not_printed() {
        // Every C0 control except ESC must never produce a print action
        // in the ground state. ESC is excluded because it begins an
        // escape sequence rather than executing.
        for c in 0u8..0x20 {
            if c == 0x1B {
                continue;
            }

            // Scalar path.
            {
                let mut s: Stream<PrintBuf> = s();
                s.next(b'A');
                s.next(c);
                s.next(b'B');
                assert_eq!(vec![b'A' as u32, b'B' as u32], s.handler.buf);
            }

            // Batched path.
            {
                let mut s: Stream<PrintBuf> = s();
                s.next_slice(&[b'A', b'B', c, b'C', b'D']);
                let expected: Vec<u32> = b"ABCD".iter().map(|&b| b as u32).collect();
                assert_eq!(expected, s.handler.buf);
            }

            // Batched path with a run long enough to exercise the
            // vectorized printable-run scan on either side of the control.
            {
                let mut s: Stream<PrintBuf> = s();
                let mut input = [b'A'; 65];
                input[32] = c;
                s.next_slice(&input);
                assert_eq!(64, s.handler.buf.len());
                for &cp in &s.handler.buf {
                    assert_eq!(b'A' as u32, cp);
                }
            }
        }
    }

    #[test]
    fn stream_ground_state_utf_8_decoded_c1_controls_are_ignored() {
        // Every C1 control that arrives as well-formed UTF-8 (a two byte
        // 0xC2-lead sequence) must be dropped: not printed and not
        // interpreted as a control (e.g. U+009B must not start a CSI).
        for c in 0x80u8..0xA0 {
            let enc: [u8; 2] = [0xC2, c];

            // Scalar path.
            {
                let mut s: Stream<PrintBuf> = s();
                s.next(b'A');
                s.next(enc[0]);
                s.next(enc[1]);
                s.next(b'B');
                assert_eq!(vec![b'A' as u32, b'B' as u32], s.handler.buf);
            }

            // Batched path.
            {
                let mut s: Stream<PrintBuf> = s();
                s.next_slice(&[b'A', b'B', enc[0], enc[1], b'C', b'D']);
                let expected: Vec<u32> = b"ABCD".iter().map(|&b| b as u32).collect();
                assert_eq!(expected, s.handler.buf);
            }

            // Batched path with a run long enough to exercise the
            // vectorized printable-run scan on either side of the control.
            {
                let mut s: Stream<PrintBuf> = s();
                let mut input = [b'A'; 66];
                input[32] = enc[0];
                input[33] = enc[1];
                s.next_slice(&input);
                assert_eq!(64, s.handler.buf.len());
                for &cp in &s.handler.buf {
                    assert_eq!(b'A' as u32, cp);
                }
            }
        }

        // U+00A0 (NBSP), just past the C1 range, must still print.
        {
            let mut s: Stream<PrintBuf> = s();
            s.next_slice(&[0xC2, 0xA0]);
            assert_eq!(vec![0xA0], s.handler.buf);
        }

        // A codepoint whose continuation byte falls in the C1 range must
        // still print: "Ü" is 0xC3 0x9C.
        {
            let mut s: Stream<PrintBuf> = s();
            s.next_slice("Ü".as_bytes());
            assert_eq!(vec![0xDC], s.handler.buf);
        }

        // A raw C1 byte is ill-formed UTF-8, not a decoded C1: it must
        // still produce a U+FFFD replacement.
        {
            let mut s: Stream<PrintBuf> = s();
            s.next_slice(&[0x9B]);
            assert_eq!(vec![0xFFFD], s.handler.buf);
        }
    }

    #[test]
    fn stream_cursor_right_cuf() {
        #[derive(Default)]
        struct H {
            amount: u16,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::CursorRight(v) = action {
                    self.amount = v.value;
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[C");
        assert_eq!(1, s.handler.amount);

        s.next_slice(b"\x1B[5C");
        assert_eq!(5, s.handler.amount);

        s.handler.amount = 0;
        s.next_slice(b"\x1B[5;4C");
        assert_eq!(0, s.handler.amount);

        s.handler.amount = 0;
        s.next_slice(b"\x1b[?3C");
        assert_eq!(0, s.handler.amount);
    }

    #[test]
    fn stream_dec_set_mode_sm_and_reset_mode_rm() {
        // Zig uses the invalid mode `@fromBackingInt(1)` as the sentinel;
        // Rust uses None.
        #[derive(Default)]
        struct H {
            mode: Option<modes::Mode>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::SetMode(v) => self.mode = Some(v.mode),
                    Action::ResetMode(_) => self.mode = None,
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[?6h");
        assert_eq!(Some(modes::Mode::Origin), s.handler.mode);

        s.next_slice(b"\x1B[?6l");
        assert_eq!(None, s.handler.mode);

        s.handler.mode = None;
        s.next_slice(b"\x1B[6 h");
        assert_eq!(None, s.handler.mode);
    }

    #[test]
    fn stream_ansi_set_mode_sm_and_reset_mode_rm() {
        #[derive(Default)]
        struct H {
            mode: Option<modes::Mode>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::SetMode(v) => self.mode = Some(v.mode),
                    Action::ResetMode(_) => self.mode = None,
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[4h");
        assert_eq!(modes::Mode::Insert, s.handler.mode.unwrap());

        s.next_slice(b"\x1B[4l");
        assert!(s.handler.mode.is_none());

        s.handler.mode = None;
        s.next_slice(b"\x1B[>5h");
        assert!(s.handler.mode.is_none());
    }

    #[test]
    fn stream_decstr_dispatch() {
        #[derive(Default)]
        struct H {
            calls: usize,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::SoftReset = action {
                    self.calls += 1;
                }
            }
        }

        let cases: &[(&[u8], usize)] = &[
            (b"\x1b[!p", 1),
            (b"\x1b[1!p", 0),
            (b"\x1b[?!p", 0),
            (b"\x1b[$p", 0),
        ];

        for &(input, calls) in cases {
            let mut s: Stream<H> = s();
            s.next_slice(input);
            assert_eq!(calls, s.handler.calls);
        }
    }

    #[test]
    fn stream_decrqcra_dispatch() {
        #[derive(Default)]
        struct H {
            calls: usize,
            req: Option<xt_checksum::Request>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::RequestXtChecksum(v) = action {
                    self.calls += 1;
                    self.req = Some(v);
                }
            }
        }

        let req = |id, top, left, bottom, right| xt_checksum::Request {
            id,
            top,
            left,
            bottom,
            right,
        };
        let cases: &[(&[u8], Option<xt_checksum::Request>)] = &[
            (b"\x1b[7;1;2;3;4;5*y", Some(req(7, 2, 3, 4, 5))),
            (b"\x1b[7*y", Some(req(7, 0, 0, 0, 0))),
            (b"\x1b[*y", Some(xt_checksum::Request::default())),
            (b"\x1b[1;1;2;3*y", Some(req(1, 2, 3, 0, 0))),
            (b"\x1b[1;1;1;1;1;1;1*y", None),
            (b"\x1b[1;1;1;1;1;1y", None),
            (b"\x1b[1;1;1;1;1;1\"y", None),
        ];

        for &(input, expected) in cases {
            let mut s: Stream<H> = s();
            s.next_slice(input);
            if let Some(req) = expected {
                assert_eq!(1, s.handler.calls);
                assert_eq!(req, s.handler.req.unwrap());
            } else {
                assert_eq!(0, s.handler.calls);
            }
        }
    }

    #[test]
    fn stream_xtchecksum_dispatch() {
        #[derive(Default)]
        struct H {
            calls: usize,
            flags: Option<xt_checksum::Flags>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::XtChecksumExtension(v) = action {
                    self.calls += 1;
                    self.flags = Some(v.flags);
                }
            }
        }

        let cases: &[(&[u8], Option<xt_checksum::Flags>)] = &[
            (b"\x1b[#y", Some(xt_checksum::Flags::default())),
            (b"\x1b[0#y", Some(xt_checksum::Flags::default())),
            (
                b"\x1b[5#y",
                Some(xt_checksum::Flags {
                    positive: true,
                    no_trim: true,
                    ..Default::default()
                }),
            ),
            (
                b"\x1b[48#y",
                Some(xt_checksum::Flags {
                    full: true,
                    ..Default::default()
                }),
            ),
            (b"\x1b[1;2#y", None),
        ];

        for &(input, expected) in cases {
            let mut s: Stream<H> = s();
            s.next_slice(input);
            if let Some(flags) = expected {
                assert_eq!(1, s.handler.calls);
                assert_eq!(flags, s.handler.flags.unwrap());
            } else {
                assert_eq!(0, s.handler.calls);
            }
        }
    }

    #[test]
    fn stream_decrqm_dispatch() {
        #[derive(Default)]
        struct H {
            calls: usize,
            mode: Option<modes::Mode>,
            raw: Option<RawMode>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::RequestMode(v) => {
                        self.calls += 1;
                        self.mode = Some(v.mode);
                    }
                    Action::RequestModeUnknown(v) => {
                        self.calls += 1;
                        self.raw = Some(v);
                    }
                    _ => {}
                }
            }
        }

        let cases: &[(&[u8], Option<modes::Mode>, Option<RawMode>)] = &[
            (b"\x1b[4$p", Some(modes::Mode::Insert), None),
            (b"\x1b[?4$p", Some(modes::Mode::SlowScroll), None),
            (
                b"\x1b[9999$p",
                None,
                Some(RawMode {
                    mode: 9999,
                    ansi: true,
                }),
            ),
            (
                b"\x1b[?9999$p",
                None,
                Some(RawMode {
                    mode: 9999,
                    ansi: false,
                }),
            ),
            (b"\x1b[4p", None, None),
            (b"\x1b[?4p", None, None),
            (b"\x1b[4!p", None, None),
            (b"\x1b[4 p", None, None),
            (b"\x1b[>4$p", None, None),
            (b"\x1b[?4!p", None, None),
            (b"\x1b[$p", None, None),
            (b"\x1b[?$p", None, None),
            (b"\x1b[4;20$p", None, None),
            (b"\x1b[?4;7$p", None, None),
            (b"\x1b[4:20$p", None, None),
        ];
        for &(input, mode, raw) in cases {
            for split in 0..input.len() + 1 {
                let mut s: Stream<H> = s();
                s.next_slice(&input[..split]);
                if split < input.len() {
                    assert_eq!(0, s.handler.calls);
                }
                s.next_slice(&input[split..]);
                let expected_calls = if mode.is_some() || raw.is_some() { 1 } else { 0 };
                assert_eq!(expected_calls, s.handler.calls);
                assert_eq!(mode, s.handler.mode);
                assert_eq!(raw, s.handler.raw);
            }
        }
    }

    #[test]
    fn stream_ansi_set_mode_sm_and_reset_mode_rm_with_unknown_value() {
        #[derive(Default)]
        struct H {
            mode: Option<modes::Mode>,
        }
        impl H {
            #[allow(dead_code)]
            fn set_mode(&mut self, mode: modes::Mode, v: bool) {
                self.mode = None;
                if v {
                    self.mode = Some(mode);
                }
            }
        }
        impl Handler for H {
            fn vt(&mut self, _action: Action<'_>) {}
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[6h");
        assert!(s.handler.mode.is_none());

        s.next_slice(b"\x1B[6l");
        assert!(s.handler.mode.is_none());
    }

    #[test]
    fn stream_restore_mode() {
        #[derive(Default)]
        struct H {
            called: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::TopAndBottomMargin(_) = action {
                    self.called = true;
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[?42r" {
            s.next(c);
        }
        assert!(!s.handler.called);
    }

    #[test]
    fn stream_pop_kitty_keyboard_with_no_params_defaults_to_1() {
        #[derive(Default)]
        struct H {
            n: u16,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::KittyKeyboardPop(v) = action {
                    self.n = v;
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[<u" {
            s.next(c);
        }
        assert_eq!(1, s.handler.n);
    }

    #[test]
    fn stream_decsca() {
        #[derive(Default)]
        struct H {
            v: Option<ansi::ProtectedMode>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::ProtectedModeOff => self.v = Some(ansi::ProtectedMode::Off),
                    Action::ProtectedModeIso => self.v = Some(ansi::ProtectedMode::Iso),
                    Action::ProtectedModeDec => self.v = Some(ansi::ProtectedMode::Dec),
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        for (input, expected) in [
            (&b"\x1B[\"q"[..], ansi::ProtectedMode::Off),
            (b"\x1B[0\"q", ansi::ProtectedMode::Off),
            (b"\x1B[2\"q", ansi::ProtectedMode::Off),
            (b"\x1B[1\"q", ansi::ProtectedMode::Dec),
        ] {
            for &c in input {
                s.next(c);
            }
            assert_eq!(expected, s.handler.v.unwrap());
        }
    }

    #[test]
    fn stream_deced_decsed() {
        #[derive(Default)]
        struct H {
            mode: Option<csi::EraseDisplay>,
            protected: Option<bool>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                let (mode, protected) = match action {
                    Action::EraseDisplayBelow(v) => (csi::EraseDisplay::Below, v),
                    Action::EraseDisplayAbove(v) => (csi::EraseDisplay::Above, v),
                    Action::EraseDisplayComplete(v) => (csi::EraseDisplay::Complete, v),
                    Action::EraseDisplayScrollback(v) => (csi::EraseDisplay::Scrollback, v),
                    Action::EraseDisplayScrollComplete(v) => {
                        (csi::EraseDisplay::ScrollComplete, v)
                    }
                    _ => return,
                };
                self.mode = Some(mode);
                self.protected = Some(protected);
            }
        }

        let mut s: Stream<H> = s();
        let cases: &[(&[u8], csi::EraseDisplay, bool)] = &[
            (b"\x1B[?J", csi::EraseDisplay::Below, true),
            (b"\x1B[?0J", csi::EraseDisplay::Below, true),
            (b"\x1B[?1J", csi::EraseDisplay::Above, true),
            (b"\x1B[?2J", csi::EraseDisplay::Complete, true),
            (b"\x1B[?3J", csi::EraseDisplay::Scrollback, true),
            (b"\x1B[J", csi::EraseDisplay::Below, false),
            (b"\x1B[0J", csi::EraseDisplay::Below, false),
            (b"\x1B[1J", csi::EraseDisplay::Above, false),
            (b"\x1B[2J", csi::EraseDisplay::Complete, false),
            (b"\x1B[3J", csi::EraseDisplay::Scrollback, false),
            // Invalid and ignored by the handler
            (b"\x1B[>0J", csi::EraseDisplay::Scrollback, false),
        ];
        for &(input, mode, protected) in cases {
            for &c in input {
                s.next(c);
            }
            assert_eq!(mode, s.handler.mode.unwrap());
            assert_eq!(protected, s.handler.protected.unwrap());
        }
    }

    #[test]
    fn stream_decel_decsel() {
        #[derive(Default)]
        struct H {
            mode: Option<csi::EraseLine>,
            protected: Option<bool>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                let (mode, protected) = match action {
                    Action::EraseLineRight(v) => (csi::EraseLine::RIGHT, v),
                    Action::EraseLineLeft(v) => (csi::EraseLine::LEFT, v),
                    Action::EraseLineComplete(v) => (csi::EraseLine::COMPLETE, v),
                    Action::EraseLineRightUnlessPendingWrap(v) => {
                        (csi::EraseLine::RIGHT_UNLESS_PENDING_WRAP, v)
                    }
                    _ => return,
                };
                self.mode = Some(mode);
                self.protected = Some(protected);
            }
        }

        let mut s: Stream<H> = s();
        let cases: &[(&[u8], csi::EraseLine, bool)] = &[
            (b"\x1B[?K", csi::EraseLine::RIGHT, true),
            (b"\x1B[?0K", csi::EraseLine::RIGHT, true),
            (b"\x1B[?1K", csi::EraseLine::LEFT, true),
            (b"\x1B[?2K", csi::EraseLine::COMPLETE, true),
            (b"\x1B[K", csi::EraseLine::RIGHT, false),
            (b"\x1B[0K", csi::EraseLine::RIGHT, false),
            (b"\x1B[1K", csi::EraseLine::LEFT, false),
            (b"\x1B[2K", csi::EraseLine::COMPLETE, false),
            // Invalid and ignored by the handler
            (b"\x1B[<1K", csi::EraseLine::COMPLETE, false),
        ];
        for &(input, mode, protected) in cases {
            for &c in input {
                s.next(c);
            }
            assert_eq!(mode, s.handler.mode.unwrap());
            assert_eq!(protected, s.handler.protected.unwrap());
        }
    }

    #[derive(Default)]
    struct CursorStyleHandler {
        style: Option<ansi::CursorStyle>,
    }
    impl Handler for CursorStyleHandler {
        fn vt(&mut self, action: Action<'_>) {
            if let Action::CursorStyle(v) = action {
                self.style = Some(v);
            }
        }
    }

    #[test]
    fn stream_decscusr() {
        let mut s: Stream<CursorStyleHandler> = s();
        s.next_slice(b"\x1B[ q");
        assert_eq!(ansi::CursorStyle::Default, s.handler.style.unwrap());

        s.next_slice(b"\x1B[1 q");
        assert_eq!(ansi::CursorStyle::BlinkingBlock, s.handler.style.unwrap());

        // Invalid and ignored by the handler
        s.next_slice(b"\x1B[?0 q");
        assert_eq!(ansi::CursorStyle::BlinkingBlock, s.handler.style.unwrap());
    }

    #[test]
    fn stream_decscusr_without_space() {
        let mut s: Stream<CursorStyleHandler> = s();
        s.next_slice(b"\x1B[q");
        assert!(s.handler.style.is_none());

        s.next_slice(b"\x1B[1q");
        assert!(s.handler.style.is_none());
    }

    #[test]
    fn stream_xtshiftescape() {
        #[derive(Default)]
        struct H {
            escape: Option<bool>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::MouseShiftCapture(v) = action {
                    self.escape = Some(v);
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[>2s");
        assert!(s.handler.escape.is_none());

        s.next_slice(b"\x1B[>s");
        assert_eq!(Some(false), s.handler.escape);

        s.next_slice(b"\x1B[>0s");
        assert_eq!(Some(false), s.handler.escape);

        s.next_slice(b"\x1B[>1s");
        assert_eq!(Some(true), s.handler.escape);

        // Invalid and ignored by the handler
        s.next_slice(b"\x1B[1 s");
        assert_eq!(Some(true), s.handler.escape);
    }

    #[test]
    fn stream_change_window_title_with_invalid_utf_8() {
        #[derive(Default)]
        struct H {
            seen: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::WindowTitle(_) = action {
                    self.seen = true;
                }
            }
        }

        {
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b]2;abc\x1b\\");
            assert!(s.handler.seen);
        }

        {
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b]2;abc\xc0\x1b\\");
            assert!(!s.handler.seen);
        }
    }

    #[test]
    fn stream_osc_52_large_payload_in_chunks() {
        #[derive(Default)]
        struct H {
            data: Vec<u8>,
            kind: u8,
            terminator: Option<osc::Terminator>,
            count: usize,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::ClipboardContents(v) = action {
                    self.count += 1;
                    self.kind = v.kind;
                    self.terminator = Some(v.terminator);
                    self.data.extend_from_slice(v.data);
                }
            }
        }

        // A payload that far exceeds the fixed buffer so the allocating
        // capture is used, fed in chunk sizes that don't align with the
        // sequence so the bulk path sees every kind of boundary.
        let prefix = b"\x1b]52;c;";
        let payload_len = 150_000;
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(prefix);
        for i in 0..payload_len {
            input.push(b'A' + (i % 26) as u8);
        }
        input.extend_from_slice(b"\x1b\\");

        let mut s = Stream::init(Options {
            allocator: Some(osc::Allocator::GLOBAL),
            ..Options::new(H::default())
        });

        let mut i = 0;
        while i < input.len() {
            let end = (i + 4093).min(input.len());
            s.next_slice(&input[i..end]);
            i = end;
        }

        assert_eq!(1, s.handler.count);
        assert_eq!(b'c', s.handler.kind);
        assert_eq!(osc::Terminator::St, s.handler.terminator.unwrap());
        assert_eq!(
            &input[prefix.len()..prefix.len() + payload_len],
            s.handler.data.as_slice()
        );
    }

    #[test]
    fn stream_osc_bulk_path_matches_per_byte_path() {
        // Records every dispatch relevant to OSC processing in a
        // normalized text form so streams fed different ways can be
        // compared byte-for-byte.
        #[derive(Default)]
        struct H {
            journal: Vec<u8>,
        }
        impl H {
            fn record(&mut self, s: String) {
                self.journal.extend_from_slice(s.as_bytes());
            }
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::ClipboardContents(v) => self.record(format!(
                        "clip kind={} term={} data={:?}\n",
                        v.kind as char,
                        match v.terminator {
                            osc::Terminator::St => "st",
                            osc::Terminator::Bel => "bel",
                        },
                        v.data
                    )),
                    Action::KittyClipboard(v) => self.record(format!(
                        "kitty meta={:?} payload={:?}\n",
                        v.metadata, v.payload
                    )),
                    Action::WindowTitle(v) => self.record(format!("title {:?}\n", v.title)),
                    // Normalize prints to per-codepoint so the per-byte
                    // and slice paths journal identically.
                    Action::Print(p) => self.record(format!("print {}\n", p.cp)),
                    Action::PrintSlice(v) => {
                        for &cp in v.cps {
                            self.record(format!("print {}\n", cp));
                        }
                    }
                    _ => {}
                }
            }
        }

        let ys = [b'y'; 3000];
        let xs = [b'x'; 3000];
        let mut big_clip = b"\x1b]52;c;".to_vec();
        big_clip.extend_from_slice(&ys);
        big_clip.extend_from_slice(b"\x1b\\");
        let mut big_title = b"\x1b]0;".to_vec();
        big_title.extend_from_slice(&xs);
        big_title.extend_from_slice(b"\x07");

        let cases: Vec<&[u8]> = vec![
            b"\x1b]52;c;aGVsbG8=\x1b\\",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]52;;aGVsbG8=\x07",
            // Ignored C0 byte embedded in the payload.
            b"\x1b]52;c;aGVs\x01bG8=\x07",
            // CAN and SUB aborts.
            b"\x1b]52;c;aGVsbG8=\x18",
            b"\x1b]52;c;aGVsbG8=\x1a",
            // C1 byte embedded in the payload is data.
            b"\x1b]0;ab\x9ccd\x07",
            // Terminated with trailing printable text.
            b"\x1b]0;a title\x07x",
            // Back-to-back sequences.
            b"\x1b]2;another title\x1b\\\x1b]0;t2\x07",
            b"\x1b]5522;type=write;aGVsbG8=\x1b\\",
            // Invalid OSC number.
            b"\x1b]999;junk\x07",
            // Exceeds the fixed buffer: allocating capture.
            &big_clip,
            // Exceeds the fixed buffer: overflow, no dispatch.
            &big_title,
        ];

        let new_stream = || {
            Stream::init(Options {
                allocator: Some(osc::Allocator::GLOBAL),
                ..Options::new(H::default())
            })
        };

        for case in cases {
            // Reference: byte-at-a-time.
            let mut reference = new_stream();
            for &c in case {
                reference.next(c);
            }

            // The whole slice at once.
            {
                let mut s = new_stream();
                s.next_slice(case);
                assert_eq!(reference.handler.journal, s.handler.journal);
            }

            // Split into two slices at every possible boundary.
            for split in 0..case.len() + 1 {
                let mut s = new_stream();
                s.next_slice(&case[..split]);
                s.next_slice(&case[split..]);
                assert_eq!(reference.handler.journal, s.handler.journal);
            }
        }
    }

    #[test]
    fn stream_insert_characters() {
        #[derive(Default)]
        struct H {
            called: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::InsertBlanks(_) = action {
                    self.called = true;
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[42@" {
            s.next(c);
        }
        assert!(s.handler.called);

        s.handler.called = false;
        for &c in b"\x1B[?42@" {
            s.next(c);
        }
        assert!(!s.handler.called);
    }

    #[test]
    fn stream_insert_characters_explicit_zero_clamps_to_1() {
        #[derive(Default)]
        struct H {
            value: Option<usize>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::InsertBlanks(v) = action {
                    self.value = Some(v);
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[0@" {
            s.next(c);
        }
        assert_eq!(1, s.handler.value.unwrap());
    }

    #[test]
    fn stream_decic_and_decdc() {
        #[derive(Default)]
        struct H {
            insert: Option<usize>,
            delete: Option<usize>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::InsertColumns(v) => self.insert = Some(v),
                    Action::DeleteColumns(v) => self.delete = Some(v),
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B['}" {
            s.next(c);
        }
        assert_eq!(1, s.handler.insert.unwrap());
        for &c in b"\x1B[3'}" {
            s.next(c);
        }
        assert_eq!(3, s.handler.insert.unwrap());
        for &c in b"\x1B[0'~" {
            s.next(c);
        }
        assert_eq!(1, s.handler.delete.unwrap());
        for &c in b"\x1B[5'~" {
            s.next(c);
        }
        assert_eq!(5, s.handler.delete.unwrap());

        // Without the intermediate these are other sequences.
        s.handler = H::default();
        for &c in b"\x1B[3}\x1B[3~" {
            s.next(c);
        }
        assert!(s.handler.insert.is_none());
        assert!(s.handler.delete.is_none());
    }

    #[test]
    fn stream_decbi_and_decfi() {
        #[derive(Default)]
        struct H {
            back: usize,
            forward: usize,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::BackIndex => self.back += 1,
                    Action::ForwardIndex => self.forward += 1,
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B6\x1B9\x1B9" {
            s.next(c);
        }
        assert_eq!(1, s.handler.back);
        assert_eq!(2, s.handler.forward);
    }

    #[test]
    fn stream_scosc() {
        #[derive(Default)]
        struct H {
            called: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                match action {
                    Action::LeftAndRightMargin(_) => panic!("bad"),
                    Action::LeftAndRightMarginAmbiguous => self.called = true,
                    _ => {}
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[s" {
            s.next(c);
        }
        assert!(s.handler.called);
    }

    #[test]
    fn stream_scorc() {
        #[derive(Default)]
        struct H {
            called: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::RestoreCursor = action {
                    self.called = true;
                }
            }
        }

        let mut s: Stream<H> = s();
        for &c in b"\x1B[u" {
            s.next(c);
        }
        assert!(s.handler.called);
    }

    #[test]
    fn stream_too_many_csi_params() {
        #[derive(Default)]
        struct H;
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::CursorRight(_) = action {
                    unreachable!();
                }
            }
        }

        let mut s: Stream<H> = s();
        s.next_slice(b"\x1B[1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1C");
    }

    #[test]
    fn stream_csi_param_too_long() {
        #[derive(Default)]
        struct H;
        impl Handler for H {
            fn vt(&mut self, _action: Action<'_>) {}
        }

        let mut s: Stream<H> = s();
        let mut input = b"\x1B[".to_vec();
        input.extend_from_slice(&[b'1'; 259]);
        input.push(b'C');
        s.next_slice(&input);
    }

    #[test]
    fn stream_send_report_with_csi_t() {
        #[derive(Default)]
        struct H {
            style: Option<csi::SizeReportStyle>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::SizeReport(v) = action {
                    self.style = Some(v);
                }
            }
        }

        let mut s: Stream<H> = s();

        s.next_slice(b"\x1b[14t");
        assert_eq!(Some(csi::SizeReportStyle::Csi14T), s.handler.style);

        s.next_slice(b"\x1b[16t");
        assert_eq!(Some(csi::SizeReportStyle::Csi16T), s.handler.style);

        s.next_slice(b"\x1b[18t");
        assert_eq!(Some(csi::SizeReportStyle::Csi18T), s.handler.style);

        s.next_slice(b"\x1b[19t");
        assert_eq!(Some(csi::SizeReportStyle::Csi19T), s.handler.style);

        s.next_slice(b"\x1b[21t");
        assert_eq!(Some(csi::SizeReportStyle::Csi21T), s.handler.style);
    }

    #[test]
    fn stream_csi_8_t_resize_window() {
        #[derive(Default)]
        struct H {
            size: Option<ResizeWindow>,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::ResizeWindow(v) = action {
                    self.size = Some(v);
                }
            }
        }

        let mut s: Stream<H> = s();

        s.next_slice(b"\x1b[8;40;120t");
        assert_eq!(40, s.handler.size.unwrap().rows);
        assert_eq!(120, s.handler.size.unwrap().columns);

        // Omitted parameters keep the current size
        s.next_slice(b"\x1b[8;;100t");
        assert_eq!(0, s.handler.size.unwrap().rows);
        assert_eq!(100, s.handler.size.unwrap().columns);

        s.next_slice(b"\x1b[8;30t");
        assert_eq!(30, s.handler.size.unwrap().rows);
        assert_eq!(0, s.handler.size.unwrap().columns);

        s.next_slice(b"\x1b[8t");
        assert_eq!(0, s.handler.size.unwrap().rows);
        assert_eq!(0, s.handler.size.unwrap().columns);

        // Extra parameters are invalid
        s.handler.size = None;
        s.next_slice(b"\x1b[8;30;100;1t");
        assert!(s.handler.size.is_none());
    }

    #[test]
    fn stream_invalid_csi_t() {
        #[derive(Default)]
        struct H {
            style: Option<csi::SizeReportStyle>,
        }
        impl H {
            #[allow(dead_code)]
            fn send_size_report(&mut self, style: csi::SizeReportStyle) {
                self.style = Some(style);
            }
        }
        impl Handler for H {
            fn vt(&mut self, _action: Action<'_>) {}
        }

        let mut s: Stream<H> = s();

        s.next_slice(b"\x1b[19t");
        assert_eq!(None, s.handler.style);
    }

    #[derive(Default)]
    struct TitlePushHandler {
        index: Option<u16>,
    }
    impl Handler for TitlePushHandler {
        fn vt(&mut self, action: Action<'_>) {
            if let Action::TitlePush(v) = action {
                self.index = Some(v);
            }
        }
    }

    #[derive(Default)]
    struct TitlePopHandler {
        index: Option<u16>,
    }
    impl Handler for TitlePopHandler {
        fn vt(&mut self, action: Action<'_>) {
            if let Action::TitlePop(v) = action {
                self.index = Some(v);
            }
        }
    }

    #[test]
    fn stream_csi_t_push_title() {
        let mut s: Stream<TitlePushHandler> = s();
        s.next_slice(b"\x1b[22;0t");
        assert_eq!(0, s.handler.index.unwrap());
    }

    #[test]
    fn stream_csi_t_push_title_with_explicit_window() {
        let mut s: Stream<TitlePushHandler> = s();
        s.next_slice(b"\x1b[22;2t");
        assert_eq!(0, s.handler.index.unwrap());
    }

    #[test]
    fn stream_csi_t_push_title_with_explicit_icon() {
        let mut s: Stream<TitlePushHandler> = s();
        s.next_slice(b"\x1b[22;1t");
        assert_eq!(None, s.handler.index);
    }

    #[test]
    fn stream_csi_t_push_title_with_index() {
        let mut s: Stream<TitlePushHandler> = s();
        s.next_slice(b"\x1b[22;0;5t");
        assert_eq!(5, s.handler.index.unwrap());
    }

    #[test]
    fn stream_csi_t_pop_title() {
        let mut s: Stream<TitlePopHandler> = s();
        s.next_slice(b"\x1b[23;0t");
        assert_eq!(0, s.handler.index.unwrap());
    }

    #[test]
    fn stream_csi_t_pop_title_with_explicit_window() {
        let mut s: Stream<TitlePopHandler> = s();
        s.next_slice(b"\x1b[23;2t");
        assert_eq!(0, s.handler.index.unwrap());
    }

    #[test]
    fn stream_csi_t_pop_title_with_explicit_icon() {
        let mut s: Stream<TitlePopHandler> = s();
        s.next_slice(b"\x1b[23;1t");
        assert_eq!(None, s.handler.index);
    }

    #[test]
    fn stream_csi_t_pop_title_with_index() {
        let mut s: Stream<TitlePopHandler> = s();
        s.next_slice(b"\x1b[23;0;5t");
        assert_eq!(5, s.handler.index.unwrap());
    }

    /// Records the key of the last action.
    #[derive(Default)]
    struct LastKey {
        action: Option<Key>,
    }
    impl Handler for LastKey {
        fn vt(&mut self, action: Action<'_>) {
            self.action = Some(action.key());
        }
    }

    #[test]
    fn stream_csi_w_clear_tab_stops() {
        let mut s: Stream<LastKey> = s();

        s.next_slice(b"\x1b[2W");
        assert_eq!(Key::TabClearCurrent, s.handler.action.unwrap());

        s.next_slice(b"\x1b[5W");
        assert_eq!(Key::TabClearAll, s.handler.action.unwrap());
    }

    #[test]
    fn stream_csi_w_tab_set() {
        let mut s: Stream<LastKey> = s();

        s.next_slice(b"\x1b[W");
        assert_eq!(Key::TabSet, s.handler.action.unwrap());

        s.handler.action = None;
        s.next_slice(b"\x1b[0W");
        assert_eq!(Key::TabSet, s.handler.action.unwrap());

        s.handler.action = None;
        s.next_slice(b"\x1b[>W");
        assert!(s.handler.action.is_none());

        s.handler.action = None;
        s.next_slice(b"\x1b[99W");
        assert!(s.handler.action.is_none());
    }

    #[test]
    fn stream_csi_question_w_reset_tab_stops() {
        let mut s: Stream<LastKey> = s();

        s.next_slice(b"\x1b[?2W");
        assert!(s.handler.action.is_none());

        s.next_slice(b"\x1b[?5W");
        assert_eq!(Key::TabReset, s.handler.action.unwrap());

        // Invalid and ignored by the handler
        s.handler.action = None;
        s.next_slice(b"\x1b[?1;2;3W");
        assert!(s.handler.action.is_none());
    }

    #[test]
    fn stream_sgr_with_17_parameters_for_underline_color() {
        #[derive(Default)]
        struct H {
            called: bool,
        }
        impl Handler for H {
            fn vt(&mut self, action: Action<'_>) {
                if let Action::SetAttribute(_) = action {
                    self.called = true;
                }
            }
        }

        let mut s: Stream<H> = s();

        // Kakoune-style SGR with underline color as 17th parameter
        // This tests the fix where param 17 was being dropped
        s.next_slice(b"\x1b[4:3;38;2;51;51;51;48;2;170;170;170;58;2;255;97;136;0m");
        assert!(s.handler.called);
    }

    #[derive(Default)]
    struct TabClearHandler {
        action: Option<Key>,
    }
    impl Handler for TabClearHandler {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::TabClearCurrent | Action::TabClearAll => self.action = Some(action.key()),
                _ => {}
            }
        }
    }

    #[test]
    fn stream_tab_clear_with_no_param_clears_the_current_stop() {
        let mut s: Stream<TabClearHandler> = s();
        s.next_slice(b"\x1b[g");
        assert_eq!(Some(Key::TabClearCurrent), s.handler.action);

        s.handler.action = None;
        s.next_slice(b"\x1b[0g");
        assert_eq!(Some(Key::TabClearCurrent), s.handler.action);

        s.handler.action = None;
        s.next_slice(b"\x1b[3g");
        assert_eq!(Some(Key::TabClearAll), s.handler.action);
    }

    #[test]
    fn stream_tab_clear_with_overflowing_param() {
        // Regression test for a fuzz crash: CSI with a parameter value that
        // saturates to 65535 (u16 max) causes @fromBackingInt to panic when
        // converting to TabClear (enum(u8)).
        let mut s: Stream<TabClearHandler> = s();
        // This is the exact input from the fuzz crash (minus the mode byte):
        // CSI with a huge numeric param that saturates to 65535, followed by 'g'.
        s.next_slice(b"\x1b[388888888888888888888888888888888888g\x1b[0m");
    }

    /// A test handler that accumulates APC bytes regardless of whether they
    /// arrive per-byte (apc_put) or in bulk (apc_put_slice).
    #[derive(Default)]
    struct ApcTestHandler {
        buf: Vec<u8>,
        slices: usize,
        puts: usize,
        started: usize,
        ended: usize,
    }
    impl Handler for ApcTestHandler {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::ApcStart => self.started += 1,
                Action::ApcEnd(_) => self.ended += 1,
                Action::ApcPut(v) => {
                    self.buf.push(v);
                    self.puts += 1;
                }
                Action::ApcPutSlice(v) => {
                    self.buf.extend_from_slice(v.bytes);
                    self.slices += 1;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn stream_apc_bulk_slice() {
        let mut s: Stream<ApcTestHandler> = s();
        s.next_slice(b"\x1b_Gf=24,s=10,v=20;aGVsbG8=\x1b\\");

        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(b"Gf=24,s=10,v=20;aGVsbG8=", s.handler.buf.as_slice());

        // With SIMD enabled the body must arrive as a single slice.
        if crate::terminal::build_options::SIMD && !DEBUG {
            assert_eq!(1, s.handler.slices);
            assert_eq!(0, s.handler.puts);
        }
    }

    #[test]
    fn stream_apc_bulk_slice_c1_st() {
        let mut s: Stream<ApcTestHandler> = s();
        s.next_slice(b"\x1b_Gpayload\x9c");

        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(b"Gpayload", s.handler.buf.as_slice());
    }

    #[test]
    fn stream_apc_bulk_slice_split_across_inputs() {
        let mut s: Stream<ApcTestHandler> = s();
        s.next_slice(b"\x1b_Gf=24,s=10");
        s.next_slice(b",v=20;aGVs");
        s.next_slice(b"bG8=\x1b\\");

        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(b"Gf=24,s=10,v=20;aGVsbG8=", s.handler.buf.as_slice());
    }

    #[test]
    fn stream_apc_bulk_slice_keeps_c0_bytes_as_data() {
        let mut s: Stream<ApcTestHandler> = s();
        // BEL does not terminate an APC string; it is payload data.
        s.next_slice(b"\x1b_Gx\x07y\x1b\\");

        assert_eq!(1, s.handler.ended);
        assert_eq!(b"Gx\x07y", s.handler.buf.as_slice());
    }

    #[test]
    fn stream_apc_aborted_by_can() {
        let mut s: Stream<ApcTestHandler> = s();
        // CAN (0x18) aborts the APC string via the anywhere => ground
        // transition. Exiting the sos_pm_apc_string state emits apc_end,
        // and the trailing bytes are printed, not treated as APC data.
        s.next_slice(b"\x1b_Gabcdefghijklmnopqrstuvwxyz0123456789\x18def");

        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(
            b"Gabcdefghijklmnopqrstuvwxyz0123456789",
            s.handler.buf.as_slice()
        );
    }

    #[test]
    fn stream_apc_scalar_path_matches() {
        let mut s: Stream<ApcTestHandler> = s();
        for &c in b"\x1b_Gf=24;aGVsbG8=\x1b\\" {
            s.next(c);
        }

        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(b"Gf=24;aGVsbG8=", s.handler.buf.as_slice());
    }

    #[test]
    fn stream_apc_vector_boundaries_match_scalar_path() {
        let positions = [15usize, 16, 17, 31, 32, 33, 63, 64, 65];
        let controls = [0x18u8, 0x1A, 0x1B, 0x80, 0xFF];

        for &position in &positions {
            for &control in &controls {
                let mut input = [0u8; 96];
                input[0..3].copy_from_slice(b"\x1b_G");
                input[3..3 + position].fill(b'a');
                input[3 + position] = control;
                input[4 + position] = b'\\';
                let bytes = &input[..5 + position];

                let mut bulk: Stream<ApcTestHandler> = s();
                bulk.next_slice(bytes);
                let mut scalar: Stream<ApcTestHandler> = s();
                for &byte in bytes {
                    scalar.next(byte);
                }

                assert_eq!(scalar.handler.started, bulk.handler.started);
                assert_eq!(scalar.handler.ended, bulk.handler.ended);
                assert_eq!(scalar.handler.buf, bulk.handler.buf);
            }
        }
    }

    #[derive(Default)]
    struct ContinuationTestHandler {
        committed: usize,
        apc_active: bool,
        apc_buf: Vec<u8>,
        dcs_active: bool,
    }
    impl Handler for ContinuationTestHandler {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::ApcStart => self.apc_active = true,
                Action::ApcPut(v) => self.apc_buf.push(v),
                Action::ApcPutSlice(v) => self.apc_buf.extend_from_slice(v.bytes),
                Action::DcsHook(_) => self.dcs_active = true,
                Action::DcsPut(_) => {}
                Action::ApcEnd(_) => {
                    self.apc_active = false;
                    self.apc_buf.clear();
                    self.committed += 1;
                }
                Action::DcsUnhook => {
                    self.dcs_active = false;
                    self.committed += 1;
                }
                Action::Print(_) => self.committed += 1,
                Action::PrintSlice(v) => self.committed += v.cps.len(),
                Action::PrintRepeat(v) => self.committed += v,
                _ => self.committed += 1,
            }
        }
    }

    #[derive(Default)]
    struct ContinuationNullHandler;
    impl Handler for ContinuationNullHandler {
        fn vt(&mut self, _action: Action<'_>) {}
    }

    /// Test helper: the ground-only form of next_slice_until, returning the
    /// consumed count the way the former nextSliceUntilGround did.
    fn until_ground<H: Handler>(stream: &mut Stream<H>, input: &[u8]) -> Option<usize> {
        let stop = stream.next_slice_until(input, BoundarySet::init_one(Boundary::Ground))?;
        assert!(stop.reached.eql(BoundarySet::init_one(Boundary::Ground)));
        assert!(stop.start.is_none());
        assert!(stop.end.is_none());
        Some(stop.consumed)
    }

    #[test]
    fn stream_next_slice_until_ground_stops_at_the_earliest_boundary() {
        let mut stream: Stream<ContinuationTestHandler> = s();

        stream.next_slice(b"\x1b[31");
        assert!(!stream.ground());
        stream.handler.committed = 0;

        let input = b"mABC\x1b[";
        let consumed = until_ground(&mut stream, input).unwrap();
        assert_eq!(1, consumed);
        assert!(stream.ground());
        assert_eq!(1, stream.handler.committed);

        // The suffix was not inspected by the handler and can be processed after
        // the caller performs work at the boundary.
        stream.next_slice(&input[consumed..]);
        assert!(!stream.ground());
        assert_eq!(ParserState::CsiEntry, stream.parser.state);
        assert_eq!(4, stream.handler.committed);
    }

    #[test]
    fn stream_next_slice_until_ground_consumes_all_input_without_a_boundary() {
        let mut stream: Stream<ContinuationNullHandler> = s();

        assert_eq!(Some(0), until_ground(&mut stream, b"unprocessed"));
        assert!(stream.ground());

        stream.next_slice(b"\x1b[");
        assert_eq!(None, until_ground(&mut stream, b"123"));
        assert!(!stream.ground());

        // A boundary on the final byte is distinguishable from exhausting the
        // input while still pending.
        assert_eq!(Some(1), until_ground(&mut stream, b"m"));
        assert!(stream.ground());
    }

    #[test]
    fn stream_next_slice_until_ground_handles_utf_8_boundaries() {
        // A completed codepoint is committed synchronously, and the printable
        // suffix remains untouched.
        let mut valid: Stream<ContinuationTestHandler> = s();
        valid.next_slice(&[0xF0]);
        let valid_input = [0x9F, 0x98, 0x84, b'X'];
        assert_eq!(Some(3), until_ground(&mut valid, &valid_input));
        assert!(valid.ground());
        assert_eq!(1, valid.handler.committed);

        // A malformed continuation emits the replacement codepoint and retries
        // the same byte. Ground is observed after that complete byte operation.
        let mut malformed: Stream<ContinuationTestHandler> = s();
        malformed.next_slice(&[0xE0, 0xA0]);
        assert_eq!(Some(1), until_ground(&mut malformed, b"A!"));
        assert!(malformed.ground());
        assert_eq!(2, malformed.handler.committed);

        // If the retried byte is ESC, the stream has begun VT state at the end of
        // that byte and must continue to the following ground boundary.
        let mut retry_escape: Stream<ContinuationTestHandler> = s();
        retry_escape.next_slice(&[0xE0, 0xA0]);
        assert_eq!(Some(3), until_ground(&mut retry_escape, b"\x1b[mX"));
        assert!(retry_escape.ground());
    }

    #[test]
    fn stream_next_slice_until_ground_handles_aborts_and_bulk_strings() {
        let mut aborted: Stream<ContinuationTestHandler> = s();
        aborted.next_slice(b"\x1b[123");
        assert_eq!(Some(1), until_ground(&mut aborted, &[0x18, b'X']));
        assert!(aborted.ground());

        let mut apc: Stream<ContinuationTestHandler> = s();
        apc.next_slice(b"\x1b_Gseed");
        let mut input = [0u8; 131];
        input[..128].fill(b'a');
        input[128..130].copy_from_slice(b"\x1b\\");
        input[130] = b'X';
        assert_eq!(Some(130), until_ground(&mut apc, &input));
        assert!(apc.ground());
    }

    #[test]
    fn stream_next_slice_until_ground_tracks_only_the_consumed_prefix() {
        let mut stream = Stream::init(Options {
            allocator: Some(osc::Allocator::GLOBAL),
            continuation_max_bytes: Some(64),
            ..Options::new(ContinuationNullHandler)
        });

        stream.next_slice(b"\x1b[31");
        assert_eq!(Some(1), until_ground(&mut stream, b"mX\x1b["));

        let mut buf = [0u8; 64];
        let mut writer = FixedWriter::fixed(&mut buf);
        stream.write_continuation(&mut writer).unwrap();
        assert_eq!(0, writer.buffered().len());

        stream.next_slice(b"\x1b[");
        assert_eq!(None, until_ground(&mut stream, b"123"));
        let mut pending_writer = FixedWriter::fixed(&mut buf);
        stream.write_continuation(&mut pending_writer).unwrap();
        assert_eq!(b"\x1b[123", pending_writer.buffered());
    }

    /// A test handler for boundary stops: records APC lifecycle, content,
    /// how many codepoints were printed, and the DCS and OSC actions that
    /// a string of those kinds produces.
    #[derive(Default)]
    struct BoundaryTestHandler {
        started: usize,
        ended: usize,
        last_terminated: Option<bool>,
        buf: Vec<u8>,
        prints: usize,
        dcs_hooks: usize,
        dcs_unhooks: usize,
        titles: usize,
        clipboards: usize,
    }
    impl Handler for BoundaryTestHandler {
        fn vt(&mut self, action: Action<'_>) {
            match action {
                Action::ApcStart => self.started += 1,
                Action::ApcEnd(v) => {
                    self.ended += 1;
                    self.last_terminated = Some(v.terminated);
                }
                Action::ApcPut(v) => self.buf.push(v),
                Action::ApcPutSlice(v) => self.buf.extend_from_slice(v.bytes),
                Action::Print(_) => self.prints += 1,
                Action::PrintSlice(v) => self.prints += v.cps.len(),
                Action::DcsHook(_) => self.dcs_hooks += 1,
                Action::DcsUnhook => self.dcs_unhooks += 1,
                Action::WindowTitle(_) => self.titles += 1,
                Action::ClipboardContents(_) => self.clipboards += 1,
                _ => {}
            }
        }
    }

    /// Same as BoundaryTestHandler but with a vt_raw hook, which disables
    /// every bulk fast path so every byte goes through Parser.next.
    #[derive(Default)]
    struct BoundaryRawTestHandler {
        inner: BoundaryTestHandler,
    }
    impl Handler for BoundaryRawTestHandler {
        const HAS_VT_RAW: bool = true;

        fn vt_raw(&mut self, _action: &parser::Action<'_>) -> Result<bool, VtRawError> {
            Ok(false)
        }

        fn vt(&mut self, action: Action<'_>) {
            self.inner.vt(action);
        }
    }

    /// Zig: `boundaryHandler(stream)`.
    trait BoundaryHandler: Handler + Default {
        fn boundary(&self) -> &BoundaryTestHandler;
    }
    impl BoundaryHandler for BoundaryTestHandler {
        fn boundary(&self) -> &BoundaryTestHandler {
            self
        }
    }
    impl BoundaryHandler for BoundaryRawTestHandler {
        fn boundary(&self) -> &BoundaryTestHandler {
            &self.inner
        }
    }

    const APC_END_SET: BoundarySet = BoundarySet::init_one(Boundary::ApcEnd);
    const APC_START_SET: BoundarySet = BoundarySet::init_one(Boundary::ApcStart);
    const APC_SET: BoundarySet = BoundarySet::init_many(&[Boundary::ApcStart, Boundary::ApcEnd]);
    const DCS_END_SET: BoundarySet = BoundarySet::init_one(Boundary::DcsEnd);
    const OSC_END_SET: BoundarySet = BoundarySet::init_one(Boundary::OscEnd);
    const DCS_SET: BoundarySet = BoundarySet::init_many(&[Boundary::DcsStart, Boundary::DcsEnd]);
    const OSC_SET: BoundarySet = BoundarySet::init_many(&[Boundary::OscStart, Boundary::OscEnd]);
    /// Every string boundary, without ground.
    const STRINGS_SET: BoundarySet = APC_SET.union_with(DCS_SET).union_with(OSC_SET);
    /// Every boundary.
    const ALL_SET: BoundarySet = STRINGS_SET.union_with(BoundarySet::init_one(Boundary::Ground));

    /// Expected result of feeding one byte to a stream inside an APC or an
    /// OSC: the end reason if the byte ends the string, and the parser state
    /// after it.
    struct StringExit {
        reason: Option<EndReason>,
        state: ParserState,
    }

    fn expected_apc_exit(c: u8) -> StringExit {
        let (reason, state) = match c {
            0x18 | 0x1A => (Some(EndReason::Cancelled), ParserState::Ground),
            0x1B => (Some(EndReason::StEsc), ParserState::Escape),
            0x9C => (Some(EndReason::StC1), ParserState::Ground),
            0x90 => (Some(EndReason::Aborted), ParserState::DcsEntry),
            0x9B => (Some(EndReason::Aborted), ParserState::CsiEntry),
            0x9D => (Some(EndReason::Aborted), ParserState::OscString),
            // A string introducer inside a string is a self-transition
            // in the parser: no exit, no entry, the byte is dropped.
            0x98 | 0x9E | 0x9F => (None, ParserState::SosPmApcString),
            0x80..=0x8F | 0x91..=0x97 | 0x99 | 0x9A => {
                (Some(EndReason::Aborted), ParserState::Ground)
            }
            // Ignored by the table: not data, not an exit.
            0xA0..=0xFF => (None, ParserState::SosPmApcString),
            // Everything else is data.
            _ => (None, ParserState::SosPmApcString),
        };
        StringExit { reason, state }
    }

    fn test_apc_exits<H: BoundaryHandler>() {
        for c in 0u8..=0xFF {
            let expected = expected_apc_exit(c);
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b_Gab");
            assert_eq!(ParserState::SosPmApcString, s.parser.state);
            assert_eq!(1, s.handler.boundary().started);
            assert_eq!(b"Gab", s.handler.boundary().buf.as_slice());

            let input = [c];
            let stop = s.next_slice_until(&input, APC_END_SET);
            assert_eq!(expected.state, s.parser.state);
            let h = s.handler.boundary();

            if let Some(reason) = expected.reason {
                let stop = stop.expect("ExpectedStop");
                assert_eq!(1, stop.consumed);
                assert!(stop.reached.eql(APC_END_SET));
                assert!(stop.start.is_none());
                let end = stop.end.expect("ExpectedEnd");
                assert_eq!(StringKind::Apc, end.kind);
                assert_eq!(reason, end.reason);
                assert_eq!(c, end.byte);
                assert_eq!(1, h.ended);
                let terminated = match reason {
                    EndReason::StEsc | EndReason::StC1 | EndReason::Bel => true,
                    EndReason::Cancelled | EndReason::Aborted => false,
                };
                assert_eq!(terminated, h.last_terminated.unwrap());
            } else {
                assert_eq!(None, stop);
                assert_eq!(0, h.ended);
                // 0x7F and C0 bytes are data; the rest are dropped.
                let data = matches!(c, 0x00..=0x17 | 0x19 | 0x1C..=0x7F);
                let mut expected_buf = b"Gab".to_vec();
                if data {
                    expected_buf.push(c);
                }
                assert_eq!(expected_buf, h.buf);
            }
        }
    }

    #[test]
    fn stream_next_slice_until_every_apc_exit() {
        test_apc_exits::<BoundaryTestHandler>();
    }

    #[test]
    fn stream_next_slice_until_every_apc_exit_through_the_scalar_path() {
        test_apc_exits::<BoundaryRawTestHandler>();
    }

    #[test]
    fn stream_next_slice_until_apc_st_esc_leaves_the_next_byte_for_the_next_call() {
        // A full ST: the backslash is an ordinary escape on the next call.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b_Gab");
            let stop = s.next_slice_until(b"\x1b\\x", APC_END_SET).unwrap();
            assert_eq!(1, stop.consumed);
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert_eq!(ParserState::Escape, s.parser.state);
            assert_eq!(None, s.next_slice_until(b"\\x", APC_END_SET));
            assert!(s.ground());
            assert_eq!(1, s.handler.prints);
            assert_eq!(b"Gab", s.handler.buf.as_slice());
        }

        // ESC then another byte: the string still ended at the ESC and the
        // byte after it is dispatched as that escape sequence.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b_Gab");
            let stop = s.next_slice_until(b"\x1b[1mx", APC_END_SET).unwrap();
            assert_eq!(1, stop.consumed);
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert_eq!(1, s.handler.ended);
            assert!(s.handler.last_terminated.unwrap());
            assert_eq!(None, s.next_slice_until(b"[1mx", APC_END_SET));
            assert!(s.ground());
            assert_eq!(1, s.handler.prints);
        }
    }

    #[test]
    fn stream_next_slice_until_apc_introducers() {
        // ESC introducers from ground, ESC and final in one call.
        for r#final in [b'_', b'X', b'^'] {
            let mut s: Stream<BoundaryTestHandler> = s();
            let input = [0x1B, r#final, b'G'];
            let stop = s.next_slice_until(&input, APC_START_SET).unwrap();
            assert_eq!(2, stop.consumed);
            assert!(stop.reached.eql(APC_START_SET));
            assert!(stop.end.is_none());
            let start = stop.start.unwrap();
            assert_eq!(StringKind::Apc, start.kind);
            assert_eq!(Introducer::Esc, start.introducer);
            assert_eq!(r#final, start.byte);
            assert_eq!(ParserState::SosPmApcString, s.parser.state);
            assert_eq!(1, s.handler.started);
            // The first byte after the introducer is the first byte of
            // content and has not been consumed.
            assert_eq!(0, s.handler.buf.len());
        }

        // ESC consumed by an earlier call: consumed covers only the final.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            assert_eq!(None, s.next_slice_until(b"\x1b", APC_START_SET));
            let stop = s.next_slice_until(b"_G", APC_START_SET).unwrap();
            assert_eq!(1, stop.consumed);
            assert_eq!(Introducer::Esc, stop.start.unwrap().introducer);
            assert_eq!(b'_', stop.start.unwrap().byte);
        }

        // C1 introducers from non-ground states. At ground a raw C1 byte
        // goes through the UTF-8 decoder instead, so prime with a CSI
        // (the csi_param fast path) and with a bare ESC (Parser.next).
        for c1 in [0x98u8, 0x9E, 0x9F] {
            for prime in [&b"\x1b[1"[..], b"\x1b"] {
                let mut s: Stream<BoundaryTestHandler> = s();
                s.next_slice(prime);
                assert!(!s.ground());
                let input = [c1, b'G'];
                let stop = s.next_slice_until(&input, APC_START_SET).unwrap();
                assert_eq!(1, stop.consumed);
                assert!(stop.reached.eql(APC_START_SET));
                let start = stop.start.unwrap();
                assert_eq!(StringKind::Apc, start.kind);
                assert_eq!(Introducer::C1, start.introducer);
                assert_eq!(c1, start.byte);
                assert_eq!(ParserState::SosPmApcString, s.parser.state);
                assert_eq!(1, s.handler.started);
            }
        }
    }

    /// One stop as seen by a caller looping over next_slice_until, with
    /// consumed made absolute within the whole input.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct AbsoluteStop {
        end: usize,
        reached: BoundarySet,
        start: Option<StopStart>,
        end_info: Option<StopEnd>,
    }

    /// Feed input through next_slice_until in a loop, recording every stop
    /// at its absolute offset. When ground is in the set and the stream is
    /// at ground, it is dropped for that call so the loop makes progress.
    fn collect_stops<H: Handler>(
        stream: &mut Stream<H>,
        input: &[u8],
        base: usize,
        set: BoundarySet,
        out: &mut Vec<AbsoluteStop>,
    ) {
        let mut rem = input;
        while !rem.is_empty() {
            let mut call_set = set;
            if stream.ground() {
                call_set.remove(Boundary::Ground);
            }
            let Some(stop) = stream.next_slice_until(rem, call_set) else {
                break;
            };
            assert!(stop.consumed > 0);
            assert!(stop.consumed <= rem.len());
            out.push(AbsoluteStop {
                end: base + (input.len() - rem.len()) + stop.consumed,
                reached: stop.reached,
                start: stop.start,
                end_info: stop.end,
            });
            rem = &rem[stop.consumed..];
        }
    }

    #[test]
    fn stream_next_slice_until_stops_do_not_depend_on_the_split() {
        let input = b"\x1b_Gf=100\x1b\\";

        // Reference: one call.
        let mut reference: Vec<AbsoluteStop> = Vec::new();
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            collect_stops(&mut s, input, 0, APC_SET, &mut reference);
            assert!(s.ground());
        }
        assert_eq!(2, reference.len());
        assert_eq!(2, reference[0].end);
        assert!(reference[0].reached.eql(APC_START_SET));
        assert_eq!(b'_', reference[0].start.unwrap().byte);
        assert_eq!(9, reference[1].end);
        assert!(reference[1].reached.eql(APC_END_SET));
        assert_eq!(EndReason::StEsc, reference[1].end_info.unwrap().reason);

        // Every split point, including between ESC and '_' (0..1) and
        // between the terminating ESC and the backslash (0..9).
        for split in 0..input.len() + 1 {
            let mut stops: Vec<AbsoluteStop> = Vec::new();
            let mut s: Stream<BoundaryTestHandler> = s();
            collect_stops(&mut s, &input[..split], 0, APC_SET, &mut stops);
            collect_stops(&mut s, &input[split..], split, APC_SET, &mut stops);
            assert!(s.ground());
            assert_eq!(reference, stops);
            assert_eq!(1, s.handler.started);
            assert_eq!(1, s.handler.ended);
            assert_eq!(b"Gf=100", s.handler.buf.as_slice());
        }
    }

    #[test]
    fn stream_next_slice_until_c1_introducer_inside_an_apc_is_dropped() {
        // The parser treats 0x98, 0x9E, 0x9F inside a string as a
        // self-transition: no apc_end, no apc_start, and the byte is not
        // data. So this does not restart the string and reports nothing.
        let mut s: Stream<BoundaryTestHandler> = s();
        let input = b"\x1b_a\x9fb\x1b\\";
        let mut stops: Vec<AbsoluteStop> = Vec::new();
        collect_stops(&mut s, input, 0, APC_SET, &mut stops);
        assert_eq!(2, stops.len());
        assert_eq!(2, stops[0].end);
        assert!(stops[0].reached.eql(APC_START_SET));
        assert_eq!(6, stops[1].end);
        assert!(stops[1].reached.eql(APC_END_SET));
        assert_eq!(1, s.handler.started);
        assert_eq!(1, s.handler.ended);
        assert_eq!(b"ab", s.handler.buf.as_slice());
    }

    #[test]
    fn stream_next_slice_until_combined_ground_and_apc_end() {
        let set = BoundarySet::init_many(&[Boundary::Ground, Boundary::ApcEnd]);

        // CAN ends the string and reaches ground on the same byte.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b_Gab");
            let stop = s.next_slice_until(b"\x18x", set).unwrap();
            assert_eq!(1, stop.consumed);
            assert!(stop.reached.eql(set));
            assert_eq!(EndReason::Cancelled, stop.end.unwrap().reason);
            assert!(s.ground());
        }

        // 0x9C too.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b_Gab");
            let stop = s.next_slice_until(b"\x9cx", set).unwrap();
            assert_eq!(1, stop.consumed);
            assert!(stop.reached.eql(set));
            assert_eq!(EndReason::StC1, stop.end.unwrap().reason);
            assert!(s.ground());
        }

        // ESC ends the string but ground is only reached on the backslash.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b_Gab");
            let stop = s.next_slice_until(b"\x1b\\x", set).unwrap();
            assert_eq!(1, stop.consumed);
            assert!(stop.reached.eql(APC_END_SET));
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert!(!s.ground());

            let ground_stop = s.next_slice_until(b"\\x", set).unwrap();
            assert_eq!(1, ground_stop.consumed);
            assert!(ground_stop.reached.eql(BoundarySet::init_one(Boundary::Ground)));
            assert!(ground_stop.end.is_none());
            assert!(s.ground());

            // Already at ground: a stop with nothing consumed.
            let idle = s.next_slice_until(b"x", set).unwrap();
            assert_eq!(0, idle.consumed);
            assert!(idle.reached.eql(BoundarySet::init_one(Boundary::Ground)));
            assert_eq!(0, s.handler.prints);
        }
    }

    #[test]
    fn stream_next_slice_until_apc_start_with_ground_requested() {
        // From ground, the start of an APC is the first boundary reached:
        // ESC leaves ground, '_' enters the string.
        let mut s: Stream<BoundaryTestHandler> = s();
        let stop = s.next_slice_until(b"\x1b_Gab\x1b\\", APC_SET).unwrap();
        assert_eq!(2, stop.consumed);
        assert!(stop.reached.eql(APC_START_SET));

        // Ground is reached before the next APC, so with ground requested
        // the stop lands on the backslash of the ST.
        let end = s.next_slice_until(b"Gab\x1b\\\x1b_x", ALL_SET).unwrap();
        assert_eq!(4, end.consumed);
        assert!(end.reached.eql(APC_END_SET));
        let ground = s.next_slice_until(b"\\\x1b_x", ALL_SET).unwrap();
        assert_eq!(1, ground.consumed);
        assert!(ground.reached.eql(BoundarySet::init_one(Boundary::Ground)));
    }

    /// Expected result of feeding one byte to a stream inside an OSC.
    fn expected_osc_exit(c: u8) -> StringExit {
        let (reason, state) = match c {
            0x07 => (Some(EndReason::Bel), ParserState::Ground),
            0x18 | 0x1A => (Some(EndReason::Cancelled), ParserState::Ground),
            0x1B => (Some(EndReason::StEsc), ParserState::Escape),
            // Every byte from 0x20 up is data, including 0x9C, 0x9D, 0x9F
            // and 0xA0 to 0xFF. The remaining C0 bytes are ignored.
            _ => (None, ParserState::OscString),
        };
        StringExit { reason, state }
    }

    fn test_osc_exits<H: BoundaryHandler>() {
        for c in 0u8..=0xFF {
            let expected = expected_osc_exit(c);
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b]2;ab");
            assert_eq!(ParserState::OscString, s.parser.state);

            let input = [c];
            let stop = s.next_slice_until(&input, OSC_END_SET);
            assert_eq!(expected.state, s.parser.state);
            let h = s.handler.boundary();

            if let Some(reason) = expected.reason {
                let stop = stop.expect("ExpectedStop");
                assert_eq!(1, stop.consumed);
                assert!(stop.reached.eql(OSC_END_SET));
                assert!(stop.start.is_none());
                let end = stop.end.expect("ExpectedEnd");
                assert_eq!(StringKind::Osc, end.kind);
                assert_eq!(reason, end.reason);
                assert_eq!(c, end.byte);
                // A proper terminator dispatches the title.
                if reason == EndReason::Bel || reason == EndReason::StEsc {
                    assert_eq!(1, h.titles);
                }
            } else {
                assert_eq!(None, stop);
                assert_eq!(0, h.titles);
            }
        }
    }

    #[test]
    fn stream_next_slice_until_every_osc_exit() {
        test_osc_exits::<BoundaryTestHandler>();
    }

    #[test]
    fn stream_next_slice_until_every_osc_exit_through_the_scalar_path() {
        test_osc_exits::<BoundaryRawTestHandler>();
    }

    fn osc_end_without_a_dispatch<H: BoundaryHandler>() {
        let mut s: Stream<H> = s();
        let stop = s.next_slice_until(b"\x1b]9999;ab\x07x", OSC_END_SET).unwrap();
        assert_eq!(10, stop.consumed);
        assert!(stop.reached.eql(OSC_END_SET));
        assert_eq!(StringKind::Osc, stop.end.unwrap().kind);
        assert_eq!(EndReason::Bel, stop.end.unwrap().reason);
        assert!(s.ground());
        assert_eq!(0, s.handler.boundary().titles);
        assert_eq!(0, s.handler.boundary().prints);
    }

    #[test]
    fn stream_next_slice_until_osc_end_without_a_dispatch() {
        // An OSC with a number the terminal doesn't know never reaches
        // the handler, but it still ends.
        osc_end_without_a_dispatch::<BoundaryTestHandler>();
        osc_end_without_a_dispatch::<BoundaryRawTestHandler>();
    }

    fn osc_st_esc_leaves_the_next_byte<H: BoundaryHandler>() {
        // ESC then backslash: the proper terminator.
        {
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b]2;ab");
            let stop = s.next_slice_until(b"\x1b\\x", OSC_END_SET).unwrap();
            assert_eq!(1, stop.consumed);
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert_eq!(ParserState::Escape, s.parser.state);
            assert_eq!(None, s.next_slice_until(b"\\x", OSC_END_SET));
            assert!(s.ground());
            assert_eq!(1, s.handler.boundary().titles);
            assert_eq!(1, s.handler.boundary().prints);
        }

        // ESC then another byte: the string still ended at the ESC and
        // the next byte starts its own sequence.
        {
            let mut s: Stream<H> = s();
            s.next_slice(b"\x1b]2;ab");
            let stop = s.next_slice_until(b"\x1b[1mx", OSC_END_SET).unwrap();
            assert_eq!(1, stop.consumed);
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert_eq!(None, s.next_slice_until(b"[1mx", OSC_END_SET));
            assert!(s.ground());
            assert_eq!(1, s.handler.boundary().titles);
            assert_eq!(1, s.handler.boundary().prints);
        }
    }

    #[test]
    fn stream_next_slice_until_osc_st_esc_leaves_the_next_byte_for_the_next_call() {
        osc_st_esc_leaves_the_next_byte::<BoundaryTestHandler>();
        osc_st_esc_leaves_the_next_byte::<BoundaryRawTestHandler>();
    }

    /// Expected result of feeding one byte to a stream inside a DCS. A None
    /// state means the byte stays inside the DCS family; which state it
    /// lands in is the parse table's business.
    struct DcsExit {
        reason: Option<EndReason>,
        state: Option<ParserState>,
    }

    /// The inputs that put the parser in each DCS state.
    const DCS_ENTRIES: [(&[u8], ParserState); 5] = [
        (b"\x1bP", ParserState::DcsEntry),
        (b"\x1bP1", ParserState::DcsParam),
        (b"\x1bP ", ParserState::DcsIntermediate),
        (b"\x1bP1q", ParserState::DcsPassthrough),
        (b"\x1bP:", ParserState::DcsIgnore),
    ];

    fn expected_dcs_exit(state: ParserState, c: u8) -> DcsExit {
        // In the header the 8-bit controls take their usual transitions.
        // In the payload (passthrough and ignore) they are data.
        let header = matches!(
            state,
            ParserState::DcsEntry | ParserState::DcsParam | ParserState::DcsIntermediate
        );
        let data = DcsExit {
            reason: None,
            state: Some(state),
        };
        let exit = |reason: Option<EndReason>, state: ParserState| DcsExit {
            reason,
            state: Some(state),
        };
        match c {
            0x18 | 0x1A => exit(Some(EndReason::Cancelled), ParserState::Ground),
            0x1B => exit(Some(EndReason::StEsc), ParserState::Escape),
            0x9C if header => exit(Some(EndReason::StC1), ParserState::Ground),
            0x9B if header => exit(Some(EndReason::Aborted), ParserState::CsiEntry),
            0x9D if header => exit(Some(EndReason::Aborted), ParserState::OscString),
            0x98 | 0x9E | 0x9F if header => {
                exit(Some(EndReason::Aborted), ParserState::SosPmApcString)
            }
            // 0x90 restarts the header without leaving the string.
            0x90 if header => exit(None, ParserState::DcsEntry),
            0x80..=0x8F | 0x91..=0x97 | 0x99 | 0x9A if header => {
                exit(Some(EndReason::Aborted), ParserState::Ground)
            }
            0x80..=0x9F => data,
            // 0x00 to 0x7F move within the family or stay put; 0xA0 to
            // 0xFF are ignored in the header and data in the payload.
            _ => DcsExit {
                reason: None,
                state: None,
            },
        }
    }

    fn test_dcs_exits<H: BoundaryHandler>() {
        for &(entry_input, entry_state) in &DCS_ENTRIES {
            for c in 0u8..=0xFF {
                let expected = expected_dcs_exit(entry_state, c);
                let mut s: Stream<H> = s();
                s.next_slice(entry_input);
                assert_eq!(entry_state, s.parser.state);

                let input = [c];
                let stop = s.next_slice_until(&input, DCS_END_SET);
                if let Some(state) = expected.state {
                    assert_eq!(state, s.parser.state);
                } else {
                    assert_eq!(Some(StringKind::Dcs), Until::family(s.parser.state));
                }
                let h = s.handler.boundary();

                if let Some(reason) = expected.reason {
                    let stop = stop.expect("ExpectedStop");
                    assert_eq!(1, stop.consumed);
                    assert!(stop.reached.eql(DCS_END_SET));
                    assert!(stop.start.is_none());
                    let end = stop.end.expect("ExpectedEnd");
                    assert_eq!(StringKind::Dcs, end.kind);
                    assert_eq!(reason, end.reason);
                    assert_eq!(c, end.byte);
                    // Leaving the payload unhooks. Leaving the header never
                    // hooked, so there is nothing to unhook.
                    let hooked = if entry_state == ParserState::DcsPassthrough {
                        1
                    } else {
                        0
                    };
                    assert_eq!(hooked, h.dcs_hooks);
                    assert_eq!(hooked, h.dcs_unhooks);
                } else {
                    assert_eq!(None, stop);
                    assert_eq!(0, h.dcs_unhooks);
                }
            }
        }
    }

    #[test]
    fn stream_next_slice_until_every_dcs_exit() {
        test_dcs_exits::<BoundaryTestHandler>();
    }

    #[test]
    fn stream_next_slice_until_every_dcs_exit_through_the_scalar_path() {
        test_dcs_exits::<BoundaryRawTestHandler>();
    }

    fn dcs_header_restart<H: BoundaryHandler>() {
        let mut s: Stream<H> = s();
        let input = b"\x1bP1\x902qx\x1b\\";
        let mut stops: Vec<AbsoluteStop> = Vec::new();
        collect_stops(&mut s, input, 0, DCS_SET, &mut stops);
        assert_eq!(2, stops.len());
        assert_eq!(2, stops[0].end);
        assert!(stops[0].reached.eql(BoundarySet::init_one(Boundary::DcsStart)));
        assert_eq!(b'P', stops[0].start.unwrap().byte);
        assert_eq!(input.len() - 1, stops[1].end);
        assert!(stops[1].reached.eql(DCS_END_SET));
        assert_eq!(EndReason::StEsc, stops[1].end_info.unwrap().reason);
        assert!(s.ground());
        assert_eq!(1, s.handler.boundary().dcs_hooks);
        assert_eq!(1, s.handler.boundary().dcs_unhooks);
    }

    #[test]
    fn stream_next_slice_until_dcs_header_restart_is_not_a_boundary() {
        dcs_header_restart::<BoundaryTestHandler>();
        dcs_header_restart::<BoundaryRawTestHandler>();
    }

    fn dcs_and_osc_introducers<H: BoundaryHandler>() {
        struct Case {
            input: &'static [u8],
            kind: StringKind,
            introducer: Introducer,
            byte: u8,
            consumed: usize,
            state: ParserState,
        }
        let cases = [
            // From ground.
            Case {
                input: b"\x1bP1q",
                kind: StringKind::Dcs,
                introducer: Introducer::Esc,
                byte: b'P',
                consumed: 2,
                state: ParserState::DcsEntry,
            },
            Case {
                input: b"\x1b]2;ab",
                kind: StringKind::Osc,
                introducer: Introducer::Esc,
                byte: b']',
                consumed: 2,
                state: ParserState::OscString,
            },
            // From inside a CSI, where ESC restarts.
            Case {
                input: b"\x1b[1\x1bP1q",
                kind: StringKind::Dcs,
                introducer: Introducer::Esc,
                byte: b'P',
                consumed: 5,
                state: ParserState::DcsEntry,
            },
            Case {
                input: b"\x1b[1\x1b]2;ab",
                kind: StringKind::Osc,
                introducer: Introducer::Esc,
                byte: b']',
                consumed: 5,
                state: ParserState::OscString,
            },
            // C1 introducers from inside a CSI. At ground they would be
            // decoded as UTF-8 and never start a string.
            Case {
                input: b"\x1b[1\x901q",
                kind: StringKind::Dcs,
                introducer: Introducer::C1,
                byte: 0x90,
                consumed: 4,
                state: ParserState::DcsEntry,
            },
            Case {
                input: b"\x1b[1\x9d2;ab",
                kind: StringKind::Osc,
                introducer: Introducer::C1,
                byte: 0x9D,
                consumed: 4,
                state: ParserState::OscString,
            },
        ];

        for case in &cases {
            let mut s: Stream<H> = s();
            let stop = s.next_slice_until(case.input, STRINGS_SET).unwrap();
            assert_eq!(case.consumed, stop.consumed);
            let start_boundary = match case.kind {
                StringKind::Apc => Boundary::ApcStart,
                StringKind::Dcs => Boundary::DcsStart,
                StringKind::Osc => Boundary::OscStart,
            };
            assert!(stop.reached.eql(BoundarySet::init_one(start_boundary)));
            assert!(stop.end.is_none());
            let start = stop.start.expect("ExpectedStart");
            assert_eq!(case.kind, start.kind);
            assert_eq!(case.introducer, start.introducer);
            assert_eq!(case.byte, start.byte);
            assert_eq!(case.state, s.parser.state);
        }
    }

    #[test]
    fn stream_next_slice_until_dcs_and_osc_introducers() {
        dcs_and_osc_introducers::<BoundaryTestHandler>();
        dcs_and_osc_introducers::<BoundaryRawTestHandler>();
    }

    /// Handler counts expected after a complete input.
    #[derive(Default)]
    struct ExpectedCounts {
        started: usize,
        ended: usize,
        dcs_hooks: usize,
        dcs_unhooks: usize,
        titles: usize,
        clipboards: usize,
    }

    impl ExpectedCounts {
        fn check(&self, h: &BoundaryTestHandler) {
            assert_eq!(self.started, h.started);
            assert_eq!(self.ended, h.ended);
            assert_eq!(self.dcs_hooks, h.dcs_hooks);
            assert_eq!(self.dcs_unhooks, h.dcs_unhooks);
            assert_eq!(self.titles, h.titles);
            assert_eq!(self.clipboards, h.clipboards);
        }
    }

    /// Feed `input` in one call, collecting the reference stops into
    /// `reference`, then at every split point, and require the same stops
    /// and the same handler counts every time.
    fn test_splits<H: BoundaryHandler>(
        input: &[u8],
        reference: &mut Vec<AbsoluteStop>,
        counts: ExpectedCounts,
    ) {
        {
            let mut s: Stream<H> = s();
            collect_stops(&mut s, input, 0, STRINGS_SET, reference);
            assert!(s.ground());
            counts.check(s.handler.boundary());
        }

        for split in 0..input.len() + 1 {
            let mut stops: Vec<AbsoluteStop> = Vec::new();
            let mut s: Stream<H> = s();
            collect_stops(&mut s, &input[..split], 0, STRINGS_SET, &mut stops);
            collect_stops(&mut s, &input[split..], split, STRINGS_SET, &mut stops);
            assert!(s.ground());
            assert_eq!(*reference, stops);
            counts.check(s.handler.boundary());
        }
    }

    fn osc_stops_split<H: BoundaryHandler>() {
        let input = b"\x1b]52;c;QUJD\x07";
        let mut reference: Vec<AbsoluteStop> = Vec::new();
        test_splits::<H>(
            input,
            &mut reference,
            ExpectedCounts {
                clipboards: 1,
                ..Default::default()
            },
        );
        assert_eq!(2, reference.len());
        assert_eq!(2, reference[0].end);
        assert!(reference[0].reached.eql(BoundarySet::init_one(Boundary::OscStart)));
        assert_eq!(b']', reference[0].start.unwrap().byte);
        assert_eq!(input.len(), reference[1].end);
        assert!(reference[1].reached.eql(OSC_END_SET));
        assert_eq!(EndReason::Bel, reference[1].end_info.unwrap().reason);
    }

    #[test]
    fn stream_next_slice_until_osc_stops_do_not_depend_on_the_split() {
        osc_stops_split::<BoundaryTestHandler>();
        osc_stops_split::<BoundaryRawTestHandler>();
    }

    fn dcs_stops_split<H: BoundaryHandler>() {
        // A DCS with an intermediate (DECRQSS) and one with parameters.
        for input in [&b"\x1bP$qm\x1b\\"[..], b"\x1bP1;2|data\x1b\\"] {
            let mut reference: Vec<AbsoluteStop> = Vec::new();
            test_splits::<H>(
                input,
                &mut reference,
                ExpectedCounts {
                    dcs_hooks: 1,
                    dcs_unhooks: 1,
                    ..Default::default()
                },
            );
            assert_eq!(2, reference.len());
            assert_eq!(2, reference[0].end);
            assert!(reference[0].reached.eql(BoundarySet::init_one(Boundary::DcsStart)));
            assert_eq!(b'P', reference[0].start.unwrap().byte);
            assert_eq!(input.len() - 1, reference[1].end);
            assert!(reference[1].reached.eql(DCS_END_SET));
            assert_eq!(EndReason::StEsc, reference[1].end_info.unwrap().reason);
        }
    }

    #[test]
    fn stream_next_slice_until_dcs_stops_do_not_depend_on_the_split() {
        dcs_stops_split::<BoundaryTestHandler>();
        dcs_stops_split::<BoundaryRawTestHandler>();
    }

    fn string_end_that_is_also_a_start<H: BoundaryHandler>() {
        // 0x9D inside an APC ends it and starts an OSC on the same byte.
        {
            let mut s: Stream<H> = s();
            let input = b"\x1b_a\x9d0;x\x07";
            let mut stops: Vec<AbsoluteStop> = Vec::new();
            collect_stops(&mut s, input, 0, STRINGS_SET, &mut stops);
            assert_eq!(3, stops.len());
            assert!(stops[0].reached.eql(APC_START_SET));

            let both = stops[1];
            assert_eq!(4, both.end);
            assert!(both
                .reached
                .eql(BoundarySet::init_many(&[Boundary::ApcEnd, Boundary::OscStart])));
            assert_eq!(StringKind::Apc, both.end_info.unwrap().kind);
            assert_eq!(EndReason::Aborted, both.end_info.unwrap().reason);
            assert_eq!(0x9D, both.end_info.unwrap().byte);
            assert_eq!(StringKind::Osc, both.start.unwrap().kind);
            assert_eq!(Introducer::C1, both.start.unwrap().introducer);
            assert_eq!(0x9D, both.start.unwrap().byte);

            assert_eq!(input.len(), stops[2].end);
            assert!(stops[2].reached.eql(OSC_END_SET));
            assert_eq!(EndReason::Bel, stops[2].end_info.unwrap().reason);
            assert!(s.ground());
            assert_eq!(1, s.handler.boundary().ended);
            assert_eq!(false, s.handler.boundary().last_terminated.unwrap());
        }

        // 0x9F inside a DCS header ends it and starts an APC. The DCS
        // never hooked.
        {
            let mut s: Stream<H> = s();
            let input = b"\x1bP1\x9fb\x1b\\";
            let mut stops: Vec<AbsoluteStop> = Vec::new();
            collect_stops(&mut s, input, 0, STRINGS_SET, &mut stops);
            assert_eq!(3, stops.len());
            assert!(stops[0].reached.eql(BoundarySet::init_one(Boundary::DcsStart)));

            let both = stops[1];
            assert_eq!(4, both.end);
            assert!(both
                .reached
                .eql(BoundarySet::init_many(&[Boundary::DcsEnd, Boundary::ApcStart])));
            assert_eq!(StringKind::Dcs, both.end_info.unwrap().kind);
            assert_eq!(EndReason::Aborted, both.end_info.unwrap().reason);
            assert_eq!(StringKind::Apc, both.start.unwrap().kind);
            assert_eq!(Introducer::C1, both.start.unwrap().introducer);
            assert_eq!(0x9F, both.start.unwrap().byte);

            assert_eq!(input.len() - 1, stops[2].end);
            assert!(stops[2].reached.eql(APC_END_SET));
            assert_eq!(EndReason::StEsc, stops[2].end_info.unwrap().reason);
            assert!(s.ground());
            assert_eq!(0, s.handler.boundary().dcs_hooks);
            assert_eq!(0, s.handler.boundary().dcs_unhooks);
            assert_eq!(1, s.handler.boundary().started);
            assert_eq!(b"b", s.handler.boundary().buf.as_slice());
        }

        // 0x90 inside an APC ends it and starts a DCS.
        {
            let mut s: Stream<H> = s();
            let input = b"\x1b_a\x90q\x1b\\";
            let mut stops: Vec<AbsoluteStop> = Vec::new();
            collect_stops(&mut s, input, 0, STRINGS_SET, &mut stops);
            assert_eq!(3, stops.len());
            assert!(stops[0].reached.eql(APC_START_SET));

            let both = stops[1];
            assert_eq!(4, both.end);
            assert!(both
                .reached
                .eql(BoundarySet::init_many(&[Boundary::ApcEnd, Boundary::DcsStart])));
            assert_eq!(StringKind::Apc, both.end_info.unwrap().kind);
            assert_eq!(EndReason::Aborted, both.end_info.unwrap().reason);
            assert_eq!(StringKind::Dcs, both.start.unwrap().kind);
            assert_eq!(Introducer::C1, both.start.unwrap().introducer);
            assert_eq!(0x90, both.start.unwrap().byte);

            assert_eq!(input.len() - 1, stops[2].end);
            assert!(stops[2].reached.eql(DCS_END_SET));
            assert_eq!(EndReason::StEsc, stops[2].end_info.unwrap().reason);
            assert!(s.ground());
            assert_eq!(1, s.handler.boundary().dcs_hooks);
            assert_eq!(1, s.handler.boundary().dcs_unhooks);
        }
    }

    #[test]
    fn stream_next_slice_until_string_end_that_is_also_a_string_start() {
        string_end_that_is_also_a_start::<BoundaryTestHandler>();
        string_end_that_is_also_a_start::<BoundaryRawTestHandler>();
    }

    #[test]
    fn stream_next_slice_until_combined_ground_and_osc_end() {
        let set = BoundarySet::init_many(&[Boundary::Ground, Boundary::OscEnd]);

        // BEL ends the string and reaches ground on the same byte.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b]2;ab");
            let stop = s.next_slice_until(b"\x07x", set).unwrap();
            assert_eq!(1, stop.consumed);
            assert!(stop.reached.eql(set));
            assert_eq!(EndReason::Bel, stop.end.unwrap().reason);
            assert!(s.ground());
            assert_eq!(0, s.handler.prints);
        }

        // ESC ends the string but ground is only reached on the backslash.
        {
            let mut s: Stream<BoundaryTestHandler> = s();
            s.next_slice(b"\x1b]2;ab");
            let stop = s.next_slice_until(b"\x1b\\x", set).unwrap();
            assert_eq!(1, stop.consumed);
            assert!(stop.reached.eql(OSC_END_SET));
            assert_eq!(EndReason::StEsc, stop.end.unwrap().reason);
            assert!(!s.ground());

            let ground_stop = s.next_slice_until(b"\\x", set).unwrap();
            assert_eq!(1, ground_stop.consumed);
            assert!(ground_stop.reached.eql(BoundarySet::init_one(Boundary::Ground)));
            assert!(ground_stop.end.is_none());
            assert!(s.ground());
        }
    }

    /// Hashes every handler action in a chunking-independent way: bulk
    /// actions are flattened to their per-element form so two streams fed
    /// the same bytes in different pieces hash the same.
    struct DifferentialHandler {
        hasher: std::collections::hash_map::DefaultHasher,
        actions: usize,
    }
    impl Default for DifferentialHandler {
        fn default() -> Self {
            DifferentialHandler {
                hasher: std::collections::hash_map::DefaultHasher::new(),
                actions: 0,
            }
        }
    }
    impl Handler for DifferentialHandler {
        fn vt(&mut self, action: Action<'_>) {
            use std::hash::Hasher;
            match action {
                Action::PrintSlice(v) => {
                    for &cp in v.cps {
                        self.record(Key::Print, |h| h.write_u32(cp));
                    }
                }
                Action::Print(p) => self.record(Key::Print, |h| h.write_u32(p.cp)),
                Action::ApcPutSlice(v) => {
                    for &b in v.bytes {
                        self.record(Key::ApcPut, |h| h.write_u8(b));
                    }
                }
                Action::ApcPut(b) => self.record(Key::ApcPut, |h| h.write_u8(b)),
                other => {
                    // Zig hashes the value deeply; the Debug form covers
                    // every field of every action value.
                    let s = format!("{:?}", other);
                    self.record(other.key(), |h| h.write(s.as_bytes()));
                }
            }
        }
    }
    impl DifferentialHandler {
        fn record(
            &mut self,
            tag: Key,
            value: impl FnOnce(&mut std::collections::hash_map::DefaultHasher),
        ) {
            use std::hash::Hasher;
            self.actions += 1;
            self.hasher.write(tag.as_str().as_bytes());
            value(&mut self.hasher);
        }

        fn r#final(&self) -> u64 {
            use std::hash::Hasher;
            self.hasher.finish()
        }
    }

    /// Feed one input through a single next_slice and through a loop of
    /// next_slice_until requesting every boundary, and require the same
    /// handler actions and the same parser state at every stop.
    fn differential(input: &[u8]) {
        let mut plain: Stream<DifferentialHandler> = s();
        plain.next_slice(input);

        // `reference` is fed exactly the bytes `until` reports consumed,
        // call by call, so it is the "second Stream fed those bytes".
        let mut until: Stream<DifferentialHandler> = s();
        let mut reference: Stream<DifferentialHandler> = s();

        let mut rem = input;
        while !rem.is_empty() {
            let mut set = ALL_SET;
            if until.ground() {
                set.remove(Boundary::Ground);
            }
            let Some(stop) = until.next_slice_until(rem, set) else {
                reference.next_slice(rem);
                break;
            };
            assert!(stop.consumed > 0);
            assert!(stop.consumed <= rem.len());
            reference.next_slice(&rem[..stop.consumed]);
            rem = &rem[stop.consumed..];

            assert_eq!(reference.parser.state, until.parser.state);
            assert_eq!(reference.utf8decoder.state, until.utf8decoder.state);
            if stop.reached.contains(Boundary::Ground) {
                assert!(until.ground());
            }
            let kind = Until::family(until.parser.state);
            if stop.reached.contains(Boundary::ApcStart) {
                assert_eq!(Some(StringKind::Apc), kind);
                assert_eq!(StringKind::Apc, stop.start.unwrap().kind);
            }
            if stop.reached.contains(Boundary::DcsStart) {
                assert_eq!(Some(StringKind::Dcs), kind);
                assert_eq!(StringKind::Dcs, stop.start.unwrap().kind);
            }
            if stop.reached.contains(Boundary::OscStart) {
                assert_eq!(Some(StringKind::Osc), kind);
                assert_eq!(StringKind::Osc, stop.start.unwrap().kind);
            }
            if stop.reached.contains(Boundary::ApcEnd) {
                assert!(kind != Some(StringKind::Apc));
                assert_eq!(StringKind::Apc, stop.end.unwrap().kind);
            }
            if stop.reached.contains(Boundary::DcsEnd) {
                assert!(kind != Some(StringKind::Dcs));
                assert_eq!(StringKind::Dcs, stop.end.unwrap().kind);
            }
            if stop.reached.contains(Boundary::OscEnd) {
                assert!(kind != Some(StringKind::Osc));
                assert_eq!(StringKind::Osc, stop.end.unwrap().kind);
            }
            // A start and an end of the same kind never coincide.
            if let (Some(start), Some(end)) = (stop.start, stop.end) {
                assert!(start.kind != end.kind);
            }
        }

        assert_eq!(plain.parser.state, until.parser.state);
        assert_eq!(plain.utf8decoder.state, until.utf8decoder.state);
        assert_eq!(plain.handler.actions, until.handler.actions);
        assert_eq!(plain.handler.r#final(), until.handler.r#final());
        assert_eq!(plain.handler.r#final(), reference.handler.r#final());
    }

    /// Sees every parser action before the stream handles it and notes the
    /// one input shape the stream logs at error level (CSI t with no
    /// parameters), which the Zig test runner would count as a failure.
    #[derive(Default)]
    struct LogErrProbe {
        csi_t_no_params: bool,
    }
    impl Handler for LogErrProbe {
        const HAS_VT_RAW: bool = true;

        fn vt_raw(&mut self, action: &parser::Action<'_>) -> Result<bool, VtRawError> {
            if let parser::Action::CsiDispatch(dispatch) = action {
                if dispatch.r#final == b't'
                    && dispatch.intermediates.is_empty()
                    && dispatch.params.is_empty()
                {
                    self.csi_t_no_params = true;
                }
            }
            Ok(true)
        }

        fn vt(&mut self, _action: Action<'_>) {}
    }

    /// True if the stream would log at error level for this input.
    fn logs_err(input: &[u8]) -> bool {
        let mut probe: Stream<LogErrProbe> = s();
        probe.next_slice(input);
        probe.handler.csi_t_no_params
    }

    /// Zig's `std.Random.DefaultPrng` (xoshiro256++ seeded by splitmix64),
    /// with the few helpers the generator test uses.
    struct Prng {
        s: [u64; 4],
    }

    impl Prng {
        fn init(seed: u64) -> Prng {
            let mut x = seed;
            let mut next = || {
                x = x.wrapping_add(0x9e3779b97f4a7c15);
                let mut z = x;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
                z ^ (z >> 31)
            };
            Prng {
                s: [next(), next(), next(), next()],
            }
        }

        fn next(&mut self) -> u64 {
            let r = self.s[0]
                .wrapping_add(self.s[3])
                .rotate_left(23)
                .wrapping_add(self.s[0]);
            let t = self.s[1] << 17;
            self.s[2] ^= self.s[0];
            self.s[3] ^= self.s[1];
            self.s[1] ^= self.s[2];
            self.s[0] ^= self.s[3];
            self.s[2] ^= t;
            self.s[3] = self.s[3].rotate_left(45);
            r
        }

        /// `intRangeAtMost`.
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }

        /// `uintLessThan`.
        fn less_than(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn boolean(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    #[test]
    fn stream_next_slice_until_differential_on_generated_streams() {
        let mut rand = Prng::init(0x5eed);

        // Pieces a generated stream is made of. Weighted toward control
        // strings and the bytes that end one, with enough text to cross
        // the 4096-byte decode chunk.
        #[derive(Clone, Copy, PartialEq)]
        enum Piece {
            Text,
            Utf8,
            Csi,
            Osc,
            OscCut,
            Dcs,
            DcsCut,
            ApcEsc,
            ApcC1,
            ApcCut,
            Raw,
            BigText,
            BigApc,
        }
        const PIECES: [Piece; 13] = [
            Piece::Text,
            Piece::Utf8,
            Piece::Csi,
            Piece::Osc,
            Piece::OscCut,
            Piece::Dcs,
            Piece::DcsCut,
            Piece::ApcEsc,
            Piece::ApcC1,
            Piece::ApcCut,
            Piece::Raw,
            Piece::BigText,
            Piece::BigApc,
        ];
        let terminators: [&[u8]; 12] = [
            b"\x1b\\", b"\x9c", b"\x18", b"\x1a", b"\x9f", b"\x90", b"\x9b", b"\x85", b"\x1b[m",
            b"\x1b_", b"\x9d", b"\x07",
        ];

        let mut buf: Vec<u8> = Vec::new();
        let mut runs = 0;
        while runs < 64 {
            buf.clear();
            let pieces = rand.range(1, 48);
            for _ in 0..pieces {
                let piece = PIECES[rand.less_than(PIECES.len())];
                match piece {
                    Piece::Text => {
                        for _ in 0..rand.range(1, 64) {
                            buf.push(rand.range(0x20, 0x7E) as u8);
                        }
                    }
                    Piece::Utf8 => {
                        for _ in 0..rand.range(1, 32) {
                            let cp = rand.range(0x80, 0x10FFFF) as u32;
                            let Some(ch) = char::from_u32(cp) else { continue };
                            let mut cp_buf = [0u8; 4];
                            buf.extend_from_slice(ch.encode_utf8(&mut cp_buf).as_bytes());
                        }
                    }
                    Piece::Csi => {
                        let a = rand.range(0, 99);
                        let b = rand.range(0, 99);
                        let f = b"mHJKABCDhlr"[rand.less_than(11)] as char;
                        buf.extend_from_slice(format!("\x1b[{};{}{}", a, b, f).as_bytes());
                    }
                    Piece::Osc => {
                        let n = rand.range(0, 9);
                        let t = rand.next() as u16;
                        let term = if rand.boolean() { "\x07" } else { "\x1b\\" };
                        buf.extend_from_slice(format!("\x1b]{};title{}{}", n, t, term).as_bytes());
                    }
                    Piece::OscCut => {
                        buf.extend_from_slice(if rand.boolean() { b"\x1b]" } else { b"\x9d" });
                        buf.extend_from_slice(b"52;c;");
                        for _ in 0..rand.range(0, 100) {
                            buf.push(rand.range(0x20, 0xFF) as u8);
                        }
                        buf.extend_from_slice(terminators[rand.less_than(terminators.len())]);
                    }
                    Piece::Dcs | Piece::DcsCut => {
                        buf.extend_from_slice(if rand.boolean() { b"\x1bP" } else { b"\x90" });
                        // A header that may be cut short, or a payload.
                        let a = rand.range(0, 99);
                        let b = rand.range(0, 99);
                        buf.extend_from_slice(format!("{};{}", a, b).as_bytes());
                        if piece == Piece::Dcs || rand.boolean() {
                            buf.push(b"q|$p"[rand.less_than(4)]);
                            for _ in 0..rand.range(0, 100) {
                                buf.push(rand.range(0x00, 0xFF) as u8);
                            }
                        }
                        if piece == Piece::Dcs {
                            buf.extend_from_slice(b"\x1b\\");
                        } else {
                            buf.extend_from_slice(terminators[rand.less_than(terminators.len())]);
                        }
                    }
                    Piece::ApcEsc | Piece::ApcC1 | Piece::ApcCut => {
                        buf.extend_from_slice(if piece == Piece::ApcC1 { b"\x9f" } else { b"\x1b_" });
                        buf.extend_from_slice(b"Gf=100,a=T;");
                        for _ in 0..rand.range(0, 200) {
                            buf.push(rand.range(0x20, 0x7E) as u8);
                        }
                        buf.extend_from_slice(terminators[rand.less_than(terminators.len())]);
                    }
                    Piece::Raw => {
                        for _ in 0..rand.range(1, 16) {
                            buf.push(rand.next() as u8);
                        }
                    }
                    Piece::BigText => {
                        for _ in 0..rand.range(4000, 9000) {
                            buf.push(rand.range(0x20, 0x7E) as u8);
                        }
                    }
                    Piece::BigApc => {
                        buf.extend_from_slice(b"\x1b_Gf=100,a=T;");
                        let alphabet =
                            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                        for _ in 0..rand.range(4000, 9000) {
                            buf.push(alphabet[rand.less_than(64)]);
                        }
                        buf.extend_from_slice(b"\x1b\\");
                    }
                }
            }
            // Random bytes can spell the one sequence the stream logs an
            // error for; skip those inputs rather than fail the run.
            if logs_err(&buf) {
                continue;
            }
            differential(&buf);
            runs += 1;
        }
    }

    #[test]
    fn stream_next_slice_until_differential_on_the_fuzz_corpus() {
        // The corpus lives in the Zig reference checkout; tests run from
        // the crate root.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ghostty/test/fuzz-libghostty/corpus/stream-cmin"
        );
        let dir = match std::fs::read_dir(path) {
            Ok(dir) => dir,
            // Zig: error.SkipZigTest when the corpus is missing.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return,
            Err(err) => panic!("{}", err),
        };

        let mut files = 0usize;
        let mut skipped = 0usize;
        for entry in dir {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            let data = std::fs::read(entry.path()).unwrap();
            assert!(data.len() <= 4 << 20);

            if logs_err(&data) {
                skipped += 1;
                continue;
            }

            differential(&data);
            files += 1;
        }
        assert!(files > 1000);
        assert!(skipped < files / 10);
    }

    fn tracked<H: Handler + Default>(max_bytes: usize) -> Stream<H> {
        Stream::init(Options {
            allocator: Some(osc::Allocator::GLOBAL),
            continuation_max_bytes: Some(max_bytes),
            ..Options::new(H::default())
        })
    }

    #[test]
    fn stream_continuation_lifecycle() {
        let disabled: Stream<ContinuationTestHandler> = s();
        let mut disabled_buf = [0u8; 1];
        let mut disabled_writer = FixedWriter::fixed(&mut disabled_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationDisabled),
            disabled.write_continuation(&mut disabled_writer)
        );

        let zero_capacity = Stream::init(Options {
            continuation_max_bytes: Some(0),
            ..Options::new(ContinuationTestHandler::default())
        });
        let mut zero_capacity_buf = [0u8; 1];
        let mut zero_capacity_writer = FixedWriter::fixed(&mut zero_capacity_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationDisabled),
            zero_capacity.write_continuation(&mut zero_capacity_writer)
        );
        let no_allocator = Stream::init(Options {
            continuation_max_bytes: Some(64),
            ..Options::new(ContinuationTestHandler::default())
        });
        let mut no_allocator_buf = [0u8; 1];
        let mut no_allocator_writer = FixedWriter::fixed(&mut no_allocator_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationDisabled),
            no_allocator.write_continuation(&mut no_allocator_writer)
        );

        let mut tracked: Stream<ContinuationTestHandler> = tracked(64);

        tracked.next_slice(b"complete input");
        let mut empty_buf = [0u8; 1];
        let mut empty_writer = FixedWriter::fixed(&mut empty_buf);
        tracked.write_continuation(&mut empty_writer).unwrap();
        assert_eq!(0, empty_writer.end);

        tracked.next_slice(b"\x1b[");
        let mut short_buf = [0u8; 1];
        let mut short_writer = FixedWriter::fixed(&mut short_buf);
        assert_eq!(
            Err(WriteContinuationError::WriteFailed),
            tracked.write_continuation(&mut short_writer)
        );

        let mut failing_stream = Stream::init(Options {
            allocator: Some(osc::Allocator::failing(0, usize::MAX)),
            continuation_max_bytes: Some(64),
            ..Options::new(ContinuationNullHandler)
        });
        failing_stream.next_slice(b"\x1b[");
        let mut unavailable_buf = [0u8; 1];
        let mut unavailable_writer = FixedWriter::fixed(&mut unavailable_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationUnavailable),
            failing_stream.write_continuation(&mut unavailable_writer)
        );
    }

    #[test]
    fn stream_continuation_suffixes_are_replay_safe() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"text\x1b", b"\x1b"),
            (b"text\x1b[12;", b"\x1b[12;"),
            (b"text\x1b[1\x07;2", b"\x1b[1;2"),
            (b"text\x1b]2;hello", b"\x1b]2;hello"),
            (b"text\x1b_Gabc", b"\x1b_Gabc"),
            (b"text\x1bP+qabc", b"\x1bP+qabc"),
            (b"text\xE0\xA0\xF0", b"\xF0"),
            (b"text\x1b[12\x1b", b"\x1b"),
            (b"text\x1b[12\x9D2;title", b"\x1b]2;title"),
            (b"text\x1b_Gabc\x9B1", b"\x1b[1"),
        ];

        for &(input, expected) in cases {
            let mut stream: Stream<ContinuationTestHandler> = tracked(1024);
            stream.next_slice(input);

            let mut buf = [0u8; 1024];
            let mut writer = FixedWriter::fixed(&mut buf);
            stream.write_continuation(&mut writer).unwrap();
            assert_eq!(expected, writer.buffered());
        }
    }

    #[test]
    fn stream_continuation_reconstructs_every_unfinished_vt_state() {
        let cases: &[(&[u8], ParserState)] = &[
            (b"\x1b", ParserState::Escape),
            (b"\x1b(", ParserState::EscapeIntermediate),
            (b"\x1b[", ParserState::CsiEntry),
            (b"\x1b[1", ParserState::CsiParam),
            (b"\x1b[1$", ParserState::CsiIntermediate),
            (b"\x1b[:", ParserState::CsiIgnore),
            (b"\x1bP", ParserState::DcsEntry),
            (b"\x1bP1", ParserState::DcsParam),
            (b"\x1bP1$", ParserState::DcsIntermediate),
            (b"\x1bP1q", ParserState::DcsPassthrough),
            (b"\x1bP:", ParserState::DcsIgnore),
            (b"\x1b]2;title", ParserState::OscString),
            (b"\x1b_Gpayload", ParserState::SosPmApcString),
        ];

        for &(input, state) in cases {
            let mut source: Stream<ContinuationTestHandler> = tracked(1024);
            source.next_slice(input);
            assert_eq!(state, source.parser.state);

            let mut buf = [0u8; 1024];
            let mut writer = FixedWriter::fixed(&mut buf);
            source.write_continuation(&mut writer).unwrap();

            let mut restored: Stream<ContinuationTestHandler> = tracked(1024);
            restored.next_slice(writer.buffered());
            assert_eq!(0, restored.handler.committed);
            assert_eq!(source.parser.state, restored.parser.state);
            assert_eq!(source.utf8decoder.state, restored.utf8decoder.state);
        }

        // Parser ground is still unfinished while the UTF-8 decoder is waiting
        // for the remaining bytes of a codepoint.
        let mut utf8: Stream<ContinuationTestHandler> = tracked(4);
        utf8.next(0xF0);
        assert_eq!(ParserState::Ground, utf8.parser.state);
        assert!(utf8.utf8decoder.state != 0);
        let mut utf8_buf = [0u8; 4];
        let mut utf8_writer = FixedWriter::fixed(&mut utf8_buf);
        utf8.write_continuation(&mut utf8_writer).unwrap();
        assert_eq!(&[0xF0], utf8_writer.buffered());
    }

    #[test]
    fn stream_continuation_is_chunking_independent_and_idempotent() {
        let input = b"committed\x1b[1\x07;2";

        let mut bulk: Stream<ContinuationTestHandler> = tracked(1024);
        bulk.next_slice(input);

        let mut scalar: Stream<ContinuationTestHandler> = tracked(1024);
        for &c in input {
            scalar.next(c);
        }

        let mut bulk_buf = [0u8; 1024];
        let mut bulk_writer = FixedWriter::fixed(&mut bulk_buf);
        bulk.write_continuation(&mut bulk_writer).unwrap();
        let mut scalar_buf = [0u8; 1024];
        let mut scalar_writer = FixedWriter::fixed(&mut scalar_buf);
        scalar.write_continuation(&mut scalar_writer).unwrap();
        assert_eq!(bulk_writer.buffered(), scalar_writer.buffered());

        let mut restored: Stream<ContinuationTestHandler> = tracked(1024);
        restored.next_slice(bulk_writer.buffered());
        assert_eq!(0, restored.handler.committed);

        let mut restored_buf = [0u8; 1024];
        let mut restored_writer = FixedWriter::fixed(&mut restored_buf);
        restored.write_continuation(&mut restored_writer).unwrap();
        assert_eq!(bulk_writer.buffered(), restored_writer.buffered());

        bulk.handler.committed = 0;
        restored.handler.committed = 0;
        bulk.next_slice(b"mZ");
        restored.next(b'm');
        restored.next(b'Z');
        assert_eq!(bulk.handler.committed, restored.handler.committed);
    }

    #[test]
    fn stream_continuation_rebuilds_apc_handler_input() {
        let mut source: Stream<ContinuationTestHandler> = tracked(1024);
        source.next_slice(b"committed\x1b_Gabc");

        let mut continuation = [0u8; 1024];
        let mut writer = FixedWriter::fixed(&mut continuation);
        source.write_continuation(&mut writer).unwrap();

        let mut restored: Stream<ContinuationTestHandler> = tracked(1024);
        restored.next_slice(writer.buffered());
        assert_eq!(0, restored.handler.committed);
        assert!(restored.handler.apc_active);
        assert_eq!(source.handler.apc_buf, restored.handler.apc_buf);

        source.handler.committed = 0;
        restored.handler.committed = 0;
        source.next_slice(b"\x1b\\");
        restored.next_slice(b"\x1b\\");
        assert_eq!(source.handler.committed, restored.handler.committed);
        assert!(!source.handler.apc_active);
        assert!(!restored.handler.apc_active);
    }

    #[test]
    fn stream_continuation_cap_and_recovery() {
        // The raw feed exceeds the cap, but only the unfinished three-byte
        // CSI suffix is retained.
        let mut seeded: Stream<ContinuationTestHandler> = tracked(4);
        seeded.next_slice(b"committed text\x1b[1");
        let mut seeded_buf = [0u8; 4];
        let mut seeded_writer = FixedWriter::fixed(&mut seeded_buf);
        seeded.write_continuation(&mut seeded_writer).unwrap();
        assert_eq!(b"\x1b[1", seeded_writer.buffered());

        let mut exceeded: Stream<ContinuationTestHandler> = tracked(4);
        exceeded.next_slice(b"\x1b[123");
        let mut unavailable_buf = [0u8; 1];
        let mut unavailable_writer = FixedWriter::fixed(&mut unavailable_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationUnavailable),
            exceeded.write_continuation(&mut unavailable_writer)
        );

        // Completing the CSI reaches ground and recovers without rebuilding the
        // Stream. A later unfinished sequence is tracked normally.
        exceeded.next_slice(b"mtext\x1b[");
        let mut recovered_buf = [0u8; 4];
        let mut recovered_writer = FixedWriter::fixed(&mut recovered_buf);
        exceeded.write_continuation(&mut recovered_writer).unwrap();
        assert_eq!(b"\x1b[", recovered_writer.buffered());

        // A fresh ESC seed also recovers broken tracking even when the stream
        // never reaches ground: the ESC abandons the previous unfinished
        // state and everything after it is retained.
        exceeded.next_slice(b"\x1b[123");
        let mut reexceeded_buf = [0u8; 1];
        let mut reexceeded_writer = FixedWriter::fixed(&mut reexceeded_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationUnavailable),
            exceeded.write_continuation(&mut reexceeded_writer)
        );
        exceeded.next_slice(b"\x1b]0;");
        let mut seed_buf = [0u8; 4];
        let mut seed_writer = FixedWriter::fixed(&mut seed_buf);
        exceeded.write_continuation(&mut seed_writer).unwrap();
        assert_eq!(b"\x1b]0;", seed_writer.buffered());
    }

    #[test]
    fn stream_continuation_spans_multiple_bulk_feeds() {
        // An unfinished APC grows across feeds that contain no new seed.
        let mut apc: Stream<ContinuationTestHandler> = tracked(1024);
        apc.next_slice(b"text\x1b_Gab");
        apc.next_slice(b"cd");
        apc.next_slice(b"ef");
        let mut apc_buf = [0u8; 16];
        let mut apc_writer = FixedWriter::fixed(&mut apc_buf);
        apc.write_continuation(&mut apc_writer).unwrap();
        assert_eq!(b"\x1b_Gabcdef", apc_writer.buffered());

        // An incomplete UTF-8 sequence grows across feeds of its
        // continuation bytes.
        let mut utf8: Stream<ContinuationTestHandler> = tracked(1024);
        utf8.next_slice(b"text\xF0");
        utf8.next_slice(b"\x9F");
        let mut utf8_buf = [0u8; 4];
        let mut utf8_writer = FixedWriter::fixed(&mut utf8_buf);
        utf8.write_continuation(&mut utf8_writer).unwrap();
        assert_eq!(b"\xF0\x9F", utf8_writer.buffered());

        // A later feed with its own seed drops everything retained earlier.
        utf8.next_slice(b"\x98\x84 done \x1b[38;5");
        let mut seed_buf = [0u8; 8];
        let mut seed_writer = FixedWriter::fixed(&mut seed_buf);
        utf8.write_continuation(&mut seed_writer).unwrap();
        assert_eq!(b"\x1b[38;5", seed_writer.buffered());
    }

    #[test]
    fn stream_continuation_exact_cap_and_large_unfinished_string() {
        let mut exact: Stream<ContinuationNullHandler> = tracked(5);
        exact.next_slice(b"\x1b[123");
        let mut exact_buf = [0u8; 5];
        let mut exact_writer = FixedWriter::fixed(&mut exact_buf);
        exact.write_continuation(&mut exact_writer).unwrap();
        assert_eq!(b"\x1b[123", exact_writer.buffered());

        let payload_len = 12 * 1024;
        let mut input = vec![b'A'; payload_len];
        input[0..3].copy_from_slice(b"\x1b_G");

        let mut large: Stream<ContinuationNullHandler> = tracked(payload_len);
        large.next_slice(&input);
        let mut large_buf = vec![0u8; payload_len];
        let mut large_writer = FixedWriter::fixed(&mut large_buf);
        large.write_continuation(&mut large_writer).unwrap();
        assert_eq!(input.as_slice(), large_writer.buffered());
    }

    #[test]
    fn stream_continuation_allocation_failure_recovers() {
        let mut stream: Stream<ContinuationNullHandler> = tracked(16 * 1024);

        let mut input = vec![b'1'; 12 * 1024];
        input[0..2].copy_from_slice(b"\x1b[");

        // Zig arms the FailingAllocator's fail and resize indexes at their
        // current counts, so every allocation and resize from now on fails.
        stream.continuation.as_mut().unwrap().alloc = osc::Allocator::failing(0, 0);
        stream.next_slice(&input);
        let mut unavailable_buf = [0u8; 1];
        let mut unavailable_writer = FixedWriter::fixed(&mut unavailable_buf);
        assert_eq!(
            Err(WriteContinuationError::ContinuationUnavailable),
            stream.write_continuation(&mut unavailable_writer)
        );

        stream.continuation.as_mut().unwrap().alloc = osc::Allocator::GLOBAL;
        stream.next(b'm');
        let mut recovered_buf = [0u8; 1];
        let mut recovered_writer = FixedWriter::fixed(&mut recovered_buf);
        stream.write_continuation(&mut recovered_writer).unwrap();
        assert_eq!(0, recovered_writer.end);
    }

    #[test]
    fn stream_continuation_every_byte_cuts_preserve_future_behavior() {
        let corpora: [&[u8]; 10] = [
            b"plain \xF0\x9F\x98\x84 utf8",
            b"bad \xE0\xA0\xF0\x9F\x98\x84 utf8",
            b"\x1b[1\x07;2mstyled\x1b[0m",
            b"\x1b]2;window title\x1b\\text",
            b"\x1bP$qm\x1b\\text",
            b"\x1b_Ga=q;payload\x1b\\text",
            b"\x1b_25a1;s\x1b\\text",
            b"\x1b]2;first\x1b\\\x1b_Gsecond",
            b"\x1b[12\x9D2;title\x1b\\text",
            b"\x1b[12\x18text\x1b[1\x1Atext",
        ];

        for corpus in corpora {
            for cut in 0..corpus.len() + 1 {
                let mut source: Stream<ContinuationTestHandler> = tracked(64 * 1024);
                source.next_slice(&corpus[..cut]);

                let mut continuation = vec![0u8; 64 * 1024];
                let mut continuation_writer = FixedWriter::fixed(&mut continuation);
                source.write_continuation(&mut continuation_writer).unwrap();

                let mut restored: Stream<ContinuationTestHandler> = tracked(64 * 1024);
                restored.next_slice(continuation_writer.buffered());
                assert_eq!(0, restored.handler.committed);
                assert_eq!(source.handler.apc_active, restored.handler.apc_active);
                assert_eq!(source.handler.dcs_active, restored.handler.dcs_active);
                if source.handler.apc_active {
                    assert_eq!(source.handler.apc_buf, restored.handler.apc_buf);
                }

                let mut reexport = vec![0u8; 64 * 1024];
                let mut reexport_writer = FixedWriter::fixed(&mut reexport);
                restored.write_continuation(&mut reexport_writer).unwrap();
                assert_eq!(continuation_writer.buffered(), reexport_writer.buffered());

                source.handler.committed = 0;
                restored.handler.committed = 0;
                source.next_slice(&corpus[cut..]);
                let mut offset = cut;
                let mut partition = cut.wrapping_add(corpus.len()).wrapping_add(1);
                while offset < corpus.len() {
                    partition = partition.wrapping_mul(1664525).wrapping_add(1013904223);
                    let len = (1 + partition % 7).min(corpus.len() - offset);
                    restored.next_slice(&corpus[offset..offset + len]);
                    offset += len;
                }
                assert_eq!(source.handler.committed, restored.handler.committed);
                assert_eq!(source.handler.apc_active, restored.handler.apc_active);
                assert_eq!(source.handler.dcs_active, restored.handler.dcs_active);

                let mut source_final = [0u8; 64];
                let mut source_final_writer = FixedWriter::fixed(&mut source_final);
                source.write_continuation(&mut source_final_writer).unwrap();
                let mut restored_final = [0u8; 64];
                let mut restored_final_writer = FixedWriter::fixed(&mut restored_final);
                restored.write_continuation(&mut restored_final_writer).unwrap();
                assert_eq!(source_final_writer.buffered(), restored_final_writer.buffered());
            }
        }
    }
}
