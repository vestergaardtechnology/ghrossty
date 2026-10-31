//! Port of `src/terminal/osc.zig`.
//!
//! STATUS: ported
//!
//! OSC (Operating System Command) related functions and types.
//!
//! OSC is another set of control sequences for terminal programs that start with
//! "ESC ]". Unlike CSI or standard ESC sequences, they may contain strings
//! and other irregular formatting so a dedicated parser is created to handle it.
//!
//! Rust notes: `Parser::end` returns the parsed [`Command`] by value. Its
//! string fields borrow the parser's capture buffer (the Zig version stores
//! the command in `parser.command` and returns a pointer to it; the
//! borrowed data has the same "valid until the next parser call" lifetime,
//! now enforced by the borrow checker). Strings that are `[:0]const u8` in
//! Zig are `&[u8]` without the sentinel here (the sentinel byte is still
//! written to the capture so buffer limits are identical).

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "osc/encoding.rs"]
pub mod encoding;
#[path = "osc/kitty_metadata.rs"]
pub mod kitty_metadata;
#[path = "osc/parsers.rs"]
pub mod parsers;
// @generated-mods-end

use std::fmt;

use crate::lib;
use crate::terminal::kitty::color as kitty_color;

pub use self::parsers::color;
pub use self::parsers::hierarchical_context;
pub use self::parsers::program_status;
pub use self::parsers::semantic_prompt;

const LOG_TARGET: &str = "osc";

/// An OSC command parsed by [`Parser`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Command<'a> {
    /// This generally shouldn't ever be set except as an initial zero value.
    /// Ignore it.
    #[default]
    Invalid,

    /// Set the window title of the terminal
    ///
    /// If title mode 0 is set text is expect to be hex encoded (i.e. utf-8
    /// with each code unit further encoded with two hex digits).
    ///
    /// If title mode 2 is set or the terminal is setup for unconditional
    /// utf-8 titles text is interpreted as utf-8. Else text is interpreted
    /// as latin1.
    ChangeWindowTitle(&'a [u8]),

    /// Set the icon of the terminal window. The name of the icon is not
    /// well defined, so this is currently ignored by Ghostty at the time
    /// of writing this. We just parse it so that we don't get parse errors
    /// in the log.
    ChangeWindowIcon(&'a [u8]),

    /// Semantic prompt command: https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md
    SemanticPrompt(SemanticPrompt<'a>),

    /// Set or get clipboard contents. If data is "?", then the current
    /// clipboard contents are sent to the pty. Otherwise, the contents
    /// are set on the clipboard.
    ClipboardContents(ClipboardContents<'a>),

    /// OSC 7. Reports the current working directory of the shell. This is
    /// a moderately flawed escape sequence but one that many major terminals
    /// support so we also support it. To understand the flaws, read through
    /// this terminal-wg issue: https://gitlab.freedesktop.org/terminal-wg/specifications/-/issues/20
    ReportPwd(ReportPwd<'a>),

    /// OSC 22. Set the mouse shape. There doesn't seem to be a standard
    /// naming scheme for cursors but it looks like terminals such as Foot
    /// are moving towards using the W3C CSS cursor names. For OSC parsing,
    /// we just parse whatever string is given.
    MouseShape(MouseShape<'a>),

    /// OSC color operations to set, reset, or report color settings. Some OSCs
    /// allow multiple operations to be specified in a single OSC so we need a
    /// list-like datastructure to manage them.
    ///
    /// Currently, these OSCs are handled by `color_operation`:
    ///
    /// 4, 5, 10-19, 104, 105, 110-119
    ColorOperation(ColorOperation),

    /// Kitty color protocol, OSC 21
    /// https://sw.kovidgoyal.net/kitty/color-stack/#id1
    KittyColorProtocol(kitty_color::Osc),

    /// Show a desktop notification (OSC 9 or OSC 777)
    ShowDesktopNotification(ShowDesktopNotification<'a>),

    /// Start a hyperlink (OSC 8)
    HyperlinkStart(HyperlinkStart<'a>),

    /// End a hyperlink (OSC 8)
    HyperlinkEnd,

    /// ConEmu sleep (OSC 9;1)
    ConemuSleep(ConemuSleep),

    /// ConEmu show GUI message box (OSC 9;2)
    ConemuShowMessageBox(&'a [u8]),

    /// ConEmu change tab title (OSC 9;3)
    ConemuChangeTabTitle(ConemuChangeTabTitle<'a>),

    /// ConEmu progress report (OSC 9;4)
    ConemuProgressReport(ProgressReport),

    /// ConEmu wait input (OSC 9;5)
    ConemuWaitInput,

    /// ConEmu GUI macro (OSC 9;6)
    ConemuGuimacro(&'a [u8]),

    /// ConEmu run process (OSC 9;7)
    ConemuRunProcess(&'a [u8]),

    /// ConEmu output environment variable (OSC 9;8)
    ConemuOutputEnvironmentVariable(&'a [u8]),

    /// ConEmu XTerm keyboard and output emulation (OSC 9;10)
    /// https://conemu.github.io/en/TerminalModes.html
    ConemuXtermEmulation(ConemuXtermEmulation),

    /// ConEmu comment (OSC 9;11)
    ConemuComment(&'a [u8]),

    /// Kitty text sizing protocol (OSC 66)
    KittyTextSizing(parsers::kitty_text_sizing::Osc<'a>),

    KittyClipboardProtocol(KittyClipboardProtocol<'a>),

    /// Kitty drag and drop protocol (OSC 72)
    KittyDndProtocol(KittyDndProtocol<'a>),

    /// OSC 3008. Hierarchical context signalling (UAPI spec).
    /// https://uapi-group.org/specifications/specs/osc_context/
    HierarchicalContext(parsers::hierarchical_context::Command<'a>),

    /// Kitty desktop notifications (OSC 99)
    KittyDesktopNotification(KittyDesktopNotification<'a>),

    /// An OSC sequence whose number this parser does not implement. Only
    /// produced when `Parser::unknown_max_bytes` is nonzero.
    Unknown(Unknown<'a>),

    /// Program status protocol (OSC 7501). A program reports what it is
    /// doing, such as working or waiting on the user, or asks whether the
    /// terminal supports the protocol. See `ProgramStatus`.
    ProgramStatus(ProgramStatus<'a>),
}

pub type SemanticPrompt<'a> = parsers::semantic_prompt::Command<'a>;
pub type HierarchicalContext<'a> = parsers::hierarchical_context::Command<'a>;
pub type KittyClipboardProtocol<'a> = parsers::kitty_clipboard_protocol::Osc<'a>;
pub type KittyDndProtocol<'a> = parsers::kitty_dnd_protocol::Osc<'a>;
pub type KittyDesktopNotification<'a> = parsers::kitty_desktop_notification::Osc<'a>;
pub type ProgramStatus<'a> = parsers::program_status::Command<'a>;

/// Payload of [`Command::ClipboardContents`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardContents<'a> {
    pub kind: u8,
    pub data: &'a [u8],
    pub terminator: Terminator,
}

/// Payload of [`Command::ReportPwd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportPwd<'a> {
    /// The reported pwd value. This is not checked for validity. It should
    /// be a file URL but it is up to the caller to utilize this value.
    pub value: &'a [u8],
}

/// Payload of [`Command::MouseShape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseShape<'a> {
    pub value: &'a [u8],
}

/// Payload of [`Command::ColorOperation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorOperation {
    pub op: color::Operation,
    pub requests: color::List,
    pub terminator: Terminator,
}

/// Payload of [`Command::ShowDesktopNotification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowDesktopNotification<'a> {
    pub title: &'a [u8],
    pub body: &'a [u8],
}

/// Payload of [`Command::HyperlinkStart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HyperlinkStart<'a> {
    pub id: Option<&'a [u8]>,
    pub uri: &'a [u8],
}

/// Payload of [`Command::ConemuSleep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConemuSleep {
    pub duration_ms: u16,
}

/// Payload of [`Command::ConemuChangeTabTitle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConemuChangeTabTitle<'a> {
    Reset,
    Value(&'a [u8]),
}

/// Payload of [`Command::ConemuXtermEmulation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConemuXtermEmulation {
    /// null => do not change
    /// false => turn off
    /// true => turn on
    pub keyboard: Option<bool>,
    /// null => do not change
    /// false => turn off
    /// true => turn on
    pub output: Option<bool>,
}

/// The tag of a [`Command`] (Zig: `Command.Key`, a `lib.Enum`; the order
/// matters for the C ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum Key {
    Invalid,
    ChangeWindowTitle,
    ChangeWindowIcon,
    SemanticPrompt,
    ClipboardContents,
    ReportPwd,
    MouseShape,
    ColorOperation,
    KittyColorProtocol,
    ShowDesktopNotification,
    HyperlinkStart,
    HyperlinkEnd,
    ConemuSleep,
    ConemuShowMessageBox,
    ConemuChangeTabTitle,
    ConemuProgressReport,
    ConemuWaitInput,
    ConemuGuimacro,
    ConemuRunProcess,
    ConemuOutputEnvironmentVariable,
    ConemuXtermEmulation,
    ConemuComment,
    KittyTextSizing,
    KittyClipboardProtocol,
    KittyDndProtocol,
    HierarchicalContext,
    KittyDesktopNotification,
    Unknown,
    ProgramStatus,
}

impl Command<'_> {
    /// The tag of this command.
    pub fn key(&self) -> Key {
        match self {
            Command::Invalid => Key::Invalid,
            Command::ChangeWindowTitle(_) => Key::ChangeWindowTitle,
            Command::ChangeWindowIcon(_) => Key::ChangeWindowIcon,
            Command::SemanticPrompt(_) => Key::SemanticPrompt,
            Command::ClipboardContents(_) => Key::ClipboardContents,
            Command::ReportPwd(_) => Key::ReportPwd,
            Command::MouseShape(_) => Key::MouseShape,
            Command::ColorOperation(_) => Key::ColorOperation,
            Command::KittyColorProtocol(_) => Key::KittyColorProtocol,
            Command::ShowDesktopNotification(_) => Key::ShowDesktopNotification,
            Command::HyperlinkStart(_) => Key::HyperlinkStart,
            Command::HyperlinkEnd => Key::HyperlinkEnd,
            Command::ConemuSleep(_) => Key::ConemuSleep,
            Command::ConemuShowMessageBox(_) => Key::ConemuShowMessageBox,
            Command::ConemuChangeTabTitle(_) => Key::ConemuChangeTabTitle,
            Command::ConemuProgressReport(_) => Key::ConemuProgressReport,
            Command::ConemuWaitInput => Key::ConemuWaitInput,
            Command::ConemuGuimacro(_) => Key::ConemuGuimacro,
            Command::ConemuRunProcess(_) => Key::ConemuRunProcess,
            Command::ConemuOutputEnvironmentVariable(_) => Key::ConemuOutputEnvironmentVariable,
            Command::ConemuXtermEmulation(_) => Key::ConemuXtermEmulation,
            Command::ConemuComment(_) => Key::ConemuComment,
            Command::KittyTextSizing(_) => Key::KittyTextSizing,
            Command::KittyClipboardProtocol(_) => Key::KittyClipboardProtocol,
            Command::KittyDndProtocol(_) => Key::KittyDndProtocol,
            Command::HierarchicalContext(_) => Key::HierarchicalContext,
            Command::KittyDesktopNotification(_) => Key::KittyDesktopNotification,
            Command::Unknown(_) => Key::Unknown,
            Command::ProgramStatus(_) => Key::ProgramStatus,
        }
    }
}

/// An OSC sequence whose number this parser does not implement. For
/// the sequence `ESC ] 7400;status=busy BEL`, `content` is
/// "7400;status=busy" and `terminator` is `.bel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unknown<'a> {
    /// Every byte fed to the parser for this sequence, including the
    /// number at the start. Owned by the parser and only valid until
    /// the next parser call.
    ///
    /// When the bytes come from the VT stream, this never contains
    /// control characters (bytes below 0x20). Those end the sequence,
    /// cancel it, or are dropped before they reach the OSC parser.
    pub content: &'a [u8],

    /// True if the sequence was longer than `Parser::unknown_max_bytes`,
    /// or memory ran out while reading it. In that case `content`
    /// holds only the beginning of the sequence.
    pub truncated: bool,

    /// How the program ended the sequence. If you send a reply, end it
    /// the same way.
    pub terminator: Terminator,
}

/// C representation of [`Unknown`].
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UnknownC {
    pub truncated: bool,
    pub content: lib::String,
    pub terminator: TerminatorC,
}

impl Unknown<'_> {
    pub fn cval(&self) -> UnknownC {
        UnknownC {
            truncated: self.truncated,
            content: lib::String::init(self.content),
            terminator: self.terminator.cval(),
        }
    }
}

/// The state of a ConEmu progress report (Zig:
/// `Command.ProgressReport.State`, a `lib.Enum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum ProgressReportState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

/// ConEmu progress report (OSC 9;4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressReport {
    pub state: ProgressReportState,
    pub progress: Option<u8>,
}

/// sync with ghostty_action_progress_report_s
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ProgressReportC {
    pub state: std::ffi::c_int,
    pub progress: i8,
}

impl ProgressReport {
    pub fn new(state: ProgressReportState) -> ProgressReport {
        ProgressReport {
            state,
            progress: None,
        }
    }

    pub fn cval(&self) -> ProgressReportC {
        ProgressReportC {
            state: self.state as std::ffi::c_int,
            progress: match self.progress {
                Some(progress) => progress.clamp(0, 100) as i8,
                None => -1,
            },
        }
    }
}

/// The terminator used to end an OSC command. For OSC commands that demand
/// a response, we try to match the terminator used in the request since that
/// is most likely to be accepted by the calling program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Terminator {
    /// The preferred string terminator is ESC followed by \
    #[default]
    St,

    /// Some applications and terminals use BELL (0x07) as the string terminator.
    Bel,
}

/// C representation of [`Terminator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum TerminatorC {
    St,
    Bel,
}

impl Terminator {
    /// Initialize the terminator based on the last byte seen. If the
    /// last byte is a BEL then we use BEL, otherwise we just assume ST.
    pub fn init(ch: Option<u8>) -> Terminator {
        match ch {
            Some(0x07) => Terminator::Bel,
            _ => Terminator::St,
        }
    }

    /// The terminator as a string. This is static memory so it doesn't
    /// need to be freed.
    pub fn string(self) -> &'static [u8] {
        match self {
            Terminator::St => b"\x1b\\",
            Terminator::Bel => b"\x07",
        }
    }

    pub fn cval(self) -> TerminatorC {
        match self {
            Terminator::St => TerminatorC::St,
            Terminator::Bel => TerminatorC::Bel,
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::St => f.write_str("\x1b\\"),
            Terminator::Bel => f.write_str("\x07"),
        }
    }
}

/// Stand-in for the optional `std.mem.Allocator` the Zig parser takes.
///
/// Whether an allocator is present changes parsing behavior (some OSCs
/// require one), so it is kept as an `Option<Allocator>` on [`Parser`].
/// Memory always comes from the global allocator; this type only
/// carries Zig's `std.testing.FailingAllocator` semantics so allocation
/// failures can be exercised (`Allocator::GLOBAL` never fails).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocator {
    /// The number of allocations that succeed before failing.
    pub fail_index: usize,
    /// The number of resizes that succeed before failing.
    pub resize_fail_index: usize,
    alloc_index: usize,
    resize_index: usize,
}

impl Allocator {
    /// The global allocator. Never fails (other than real OOM).
    pub const GLOBAL: Allocator = Allocator::failing(usize::MAX, usize::MAX);

    /// Zig's `std.testing.failing_allocator`: every allocation fails.
    pub const FAILING: Allocator = Allocator::failing(0, usize::MAX);

    /// Zig's `std.testing.FailingAllocator`.
    pub const fn failing(fail_index: usize, resize_fail_index: usize) -> Allocator {
        Allocator {
            fail_index,
            resize_fail_index,
            alloc_index: 0,
            resize_index: 0,
        }
    }

    /// Allocate a buffer with exactly `cap` capacity.
    pub fn alloc_vec(&mut self, cap: usize) -> Option<Vec<u8>> {
        if self.alloc_index == self.fail_index {
            return None;
        }
        self.alloc_index += 1;
        let mut v = Vec::new();
        v.try_reserve_exact(cap).ok()?;
        Some(v)
    }

    /// Grow `v` to exactly `new_cap` capacity (Zig `realloc`: try a
    /// resize in place, then a new allocation).
    pub fn realloc(&mut self, v: &mut Vec<u8>, new_cap: usize) -> bool {
        if self.resize_index != self.resize_fail_index {
            self.resize_index += 1;
        } else if self.alloc_index != self.fail_index {
            self.alloc_index += 1;
        } else {
            return false;
        }
        v.try_reserve_exact(new_cap.saturating_sub(v.len())).is_ok()
    }
}

/// The capture write failed (buffer full or allocation failure). Zig:
/// `error.WriteFailed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFailed;

/// The kind of storage backing a [`Capture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// The parser's fixed buffer.
    Fixed,
    /// An allocated buffer.
    Allocating,
}

/// Capture mode for `Parser::capture_trailing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    Fixed,
    Allocating,
}

/// Capture state. Accumulates the bytes of an OSC once the parser has
/// identified it.
///
/// The fixed backing uses the parser's reusable buffer, which is lent to
/// the capture and returned on reset.
#[derive(Debug)]
pub struct Capture {
    pub backing: Backing,
    /// The captured bytes. Its logical capacity is `buffer_len`.
    data: Vec<u8>,
    /// Zig: `writer.buffer.len`, the size of the buffer.
    buffer_len: usize,
    pub max_bytes: usize,
    /// The allocator used to grow an allocating capture.
    pub(crate) alloc: Option<Allocator>,
}

impl Capture {
    fn fixed(mut buf: Vec<u8>, len: usize) -> Capture {
        buf.clear();
        if buf.capacity() < len {
            buf.reserve_exact(len);
        }
        Capture {
            backing: Backing::Fixed,
            data: buf,
            buffer_len: len,
            max_bytes: len,
            alloc: None,
        }
    }

    fn allocating(mut alloc: Allocator, max_bytes: usize) -> Option<Capture> {
        let cap = Parser::MAX_BUF.min(max_bytes);
        let data = alloc.alloc_vec(cap)?;
        Some(Capture {
            backing: Backing::Allocating,
            data,
            buffer_len: cap,
            max_bytes,
            alloc: Some(alloc),
        })
    }

    /// Grow an allocating capture's buffer to `new_capacity`.
    fn grow(&mut self, new_capacity: usize) -> Result<(), WriteFailed> {
        let alloc = self.alloc.as_mut().ok_or(WriteFailed)?;
        if !alloc.realloc(&mut self.data, new_capacity) {
            return Err(WriteFailed);
        }
        self.buffer_len = new_capacity;
        Ok(())
    }

    /// Append one byte without permitting the backing allocation to grow
    /// beyond max_bytes. The allocating writer normally grows
    /// super-linearly, so grow it explicitly to keep the allocation itself
    /// bounded too.
    ///
    /// This runs for every byte of a captured OSC, so the common case (room
    /// left in the buffer) is inline and the rest is out of line.
    #[inline]
    pub fn write_byte(&mut self, byte: u8) -> Result<(), WriteFailed> {
        if self.data.len() >= self.max_bytes {
            return Err(WriteFailed);
        }

        match self.backing {
            Backing::Fixed => {}
            Backing::Allocating => {
                if self.data.len() >= self.buffer_len {
                    self.grow_for_byte()?;
                }
            }
        }

        self.writer_write_byte(byte)
    }

    /// The growth step of `write_byte` for a full allocating buffer.
    #[cold]
    #[inline(never)]
    fn grow_for_byte(&mut self) -> Result<(), WriteFailed> {
        let new_capacity = self
            .max_bytes
            .min(self.buffer_len.saturating_mul(2).max(1));
        self.grow(new_capacity)
    }

    /// Append a slice without permitting the backing allocation to
    /// grow beyond max_bytes. This matches the byte-at-a-time
    /// semantics of write_byte: bytes are retained up to exactly
    /// max_bytes and the first byte that doesn't fit fails the
    /// write.
    pub fn write_slice(&mut self, bytes: &[u8]) -> Result<(), WriteFailed> {
        let avail = self.max_bytes - self.data.len();
        let n = bytes.len().min(avail);

        match self.backing {
            Backing::Fixed => {}
            Backing::Allocating => {
                let needed = self.data.len() + n;
                if needed > self.buffer_len {
                    let new_capacity = self
                        .max_bytes
                        .min(self.buffer_len.saturating_mul(2).max(needed));
                    self.grow(new_capacity)?;
                }
            }
        }

        if self.data.len() + n > self.buffer_len {
            return Err(WriteFailed);
        }
        self.data.extend_from_slice(&bytes[..n]);
        if n < bytes.len() {
            return Err(WriteFailed);
        }
        Ok(())
    }

    /// Write a byte directly to the underlying writer (Zig:
    /// `cap.writer.writeByte`). A fixed buffer fails when full; an
    /// allocating buffer grows without regard to `max_bytes`.
    #[inline]
    pub fn writer_write_byte(&mut self, byte: u8) -> Result<(), WriteFailed> {
        if self.data.len() >= self.buffer_len {
            self.writer_make_room()?;
        }
        self.data.push(byte);
        Ok(())
    }

    /// The full-buffer case of `writer_write_byte`.
    #[cold]
    #[inline(never)]
    fn writer_make_room(&mut self) -> Result<(), WriteFailed> {
        match self.backing {
            Backing::Fixed => Err(WriteFailed),
            Backing::Allocating => {
                let new_capacity = (self.buffer_len + self.buffer_len / 2 + 8)
                    .max(self.data.len() + 1);
                self.grow(new_capacity)
            }
        }
    }

    /// Return the captured trailing data. This is the data from the
    /// point that trailing data capture was requested.
    pub fn trailing(&self) -> &[u8] {
        &self.data
    }

    /// Mutable access to the captured trailing data.
    pub fn trailing_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// The size of the buffer backing the capture (Zig:
    /// `writer.buffer.len`).
    pub fn buffer_len(&self) -> usize {
        self.buffer_len
    }
}

/// The OSC parser state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Start,
    Invalid,

    /// Collecting the bytes of a sequence whose number is not
    /// implemented, to return as `Command::Unknown`. Only entered when
    /// `unknown_max_bytes` is nonzero.
    Unknown,

    /// Like `Unknown`, but the byte limit has been reached. Later bytes
    /// are dropped and the command is returned with `truncated` set.
    UnknownTruncated,

    // OSC command prefixes. Not all of these are valid OSCs, but may be
    // needed to "bridge" to a valid OSC (e.g. to support OSC 777 we need to
    // have a state "77" even though there is no OSC 77).
    //
    // The name of each prefix state (see `State::as_str`) must be exactly
    // the bytes consumed to reach it. `unknown_or_invalid` relies on this
    // to recover the prefix.
    Osc0,
    Osc1,
    Osc2,
    Osc3,
    Osc4,
    Osc5,
    Osc6,
    Osc7,
    Osc8,
    Osc9,
    Osc30,
    Osc300,
    Osc3008,
    Osc10,
    Osc11,
    Osc12,
    Osc13,
    Osc14,
    Osc15,
    Osc16,
    Osc17,
    Osc18,
    Osc19,
    Osc21,
    Osc22,
    Osc52,
    Osc55,
    Osc66,
    Osc72,
    Osc75,
    Osc750,
    Osc7501,
    Osc77,
    Osc99,
    Osc104,
    Osc105,
    Osc110,
    Osc111,
    Osc112,
    Osc113,
    Osc114,
    Osc115,
    Osc116,
    Osc117,
    Osc118,
    Osc119,
    Osc133,
    Osc552,
    Osc777,
    Osc1337,
    Osc5522,
}

impl State {
    /// Every state, in declaration order.
    pub const ALL: [State; 55] = [
        State::Start,
        State::Invalid,
        State::Unknown,
        State::UnknownTruncated,
        State::Osc0,
        State::Osc1,
        State::Osc2,
        State::Osc3,
        State::Osc4,
        State::Osc5,
        State::Osc6,
        State::Osc7,
        State::Osc8,
        State::Osc9,
        State::Osc30,
        State::Osc300,
        State::Osc3008,
        State::Osc10,
        State::Osc11,
        State::Osc12,
        State::Osc13,
        State::Osc14,
        State::Osc15,
        State::Osc16,
        State::Osc17,
        State::Osc18,
        State::Osc19,
        State::Osc21,
        State::Osc22,
        State::Osc52,
        State::Osc55,
        State::Osc66,
        State::Osc72,
        State::Osc75,
        State::Osc750,
        State::Osc7501,
        State::Osc77,
        State::Osc99,
        State::Osc104,
        State::Osc105,
        State::Osc110,
        State::Osc111,
        State::Osc112,
        State::Osc113,
        State::Osc114,
        State::Osc115,
        State::Osc116,
        State::Osc117,
        State::Osc118,
        State::Osc119,
        State::Osc133,
        State::Osc552,
        State::Osc777,
        State::Osc1337,
        State::Osc5522,
    ];

    /// The Zig tag name of the state. For prefix states this is exactly
    /// the bytes consumed to reach the state.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Start => "start",
            State::Invalid => "invalid",
            State::Unknown => "unknown",
            State::UnknownTruncated => "unknown_truncated",
            State::Osc0 => "0",
            State::Osc1 => "1",
            State::Osc2 => "2",
            State::Osc3 => "3",
            State::Osc4 => "4",
            State::Osc5 => "5",
            State::Osc6 => "6",
            State::Osc7 => "7",
            State::Osc8 => "8",
            State::Osc9 => "9",
            State::Osc30 => "30",
            State::Osc300 => "300",
            State::Osc3008 => "3008",
            State::Osc10 => "10",
            State::Osc11 => "11",
            State::Osc12 => "12",
            State::Osc13 => "13",
            State::Osc14 => "14",
            State::Osc15 => "15",
            State::Osc16 => "16",
            State::Osc17 => "17",
            State::Osc18 => "18",
            State::Osc19 => "19",
            State::Osc21 => "21",
            State::Osc22 => "22",
            State::Osc52 => "52",
            State::Osc55 => "55",
            State::Osc66 => "66",
            State::Osc72 => "72",
            State::Osc75 => "75",
            State::Osc750 => "750",
            State::Osc7501 => "7501",
            State::Osc77 => "77",
            State::Osc99 => "99",
            State::Osc104 => "104",
            State::Osc105 => "105",
            State::Osc110 => "110",
            State::Osc111 => "111",
            State::Osc112 => "112",
            State::Osc113 => "113",
            State::Osc114 => "114",
            State::Osc115 => "115",
            State::Osc116 => "116",
            State::Osc117 => "117",
            State::Osc118 => "118",
            State::Osc119 => "119",
            State::Osc133 => "133",
            State::Osc552 => "552",
            State::Osc777 => "777",
            State::Osc1337 => "1337",
            State::Osc5522 => "5522",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The OSC parser.
#[derive(Debug)]
pub struct Parser {
    /// Optional allocator used to accept data longer than MAX_BUF.
    /// This only applies to some commands (e.g. OSC 52) that can
    /// reasonably exceed MAX_BUF.
    pub alloc: Option<Allocator>,

    /// Maximum number of bytes retained by an allocating capture.
    /// This is configurable primarily so callers and tests can choose a
    /// smaller policy than the default.
    pub max_allocating_bytes: usize,

    /// The most bytes to keep from each OSC sequence whose number this
    /// parser does not implement.
    ///
    /// Zero, the default, discards these sequences. The parser stops
    /// reading one as soon as it sees a number it doesn't know, and `end`
    /// returns null. Any other value makes `end` return `Command::Unknown`
    /// with the sequence's bytes, so you can implement the sequence
    /// yourself:
    ///
    /// ```ignore
    /// let mut p = Parser::init(Some(Allocator::GLOBAL));
    /// p.unknown_max_bytes = 1024;
    ///
    /// p.next_slice(b"7400;status=busy");
    /// let cmd = p.end(Some(0x07)).unwrap(); // Unknown, content "7400;status=busy"
    /// ```
    ///
    /// A sequence longer than the limit is still returned, with the
    /// first bytes up to the limit and `truncated` set to true.
    ///
    /// Limits up to `MAX_BUF` use a buffer inside the parser and never
    /// allocate. Larger limits allocate for each unknown sequence. Without
    /// an allocator, a larger limit behaves like `MAX_BUF`. If memory runs
    /// out partway through a sequence, it is returned truncated.
    ///
    /// Sequences with a number the parser does implement are not affected,
    /// even when their contents are malformed.
    pub unknown_max_bytes: usize,

    /// Current state of the parser.
    pub state: State,

    /// Buffer for temporary storage of OSC data. Lent to a fixed capture
    /// while one is active.
    buffer: Vec<u8>,

    /// Capture state. If this is set then we're actively capturing the
    /// bytes coming into the parser.
    pub capture: Option<Capture>,
}

impl Parser {
    /// Maximum size of a "normal" OSC.
    pub const MAX_BUF: usize = 2048;

    /// Maximum size of an OSC that requires dynamically allocated storage.
    /// OSC input is untrusted, so these captures must have a finite bound.
    pub const MAX_ALLOCATING_BUF: usize = 8 * 1024 * 1024;

    pub fn init(alloc: Option<Allocator>) -> Parser {
        Parser {
            alloc,
            max_allocating_bytes: Self::MAX_ALLOCATING_BUF,
            unknown_max_bytes: 0,
            state: State::Start,
            buffer: Vec::with_capacity(Self::MAX_BUF),
            capture: None,
        }
    }

    /// This must be called to clean up any allocated memory.
    pub fn deinit(&mut self) {
        self.reset();
    }

    /// Reset the parser state.
    pub fn reset(&mut self) {
        // If we're capturing, then stop it. A fixed capture returns the
        // parser's buffer.
        if let Some(cap) = self.capture.take() {
            match cap.backing {
                Backing::Fixed => {
                    self.buffer = cap.data;
                    self.buffer.clear();
                }
                Backing::Allocating => {
                    // Keep the allocator state shared with the capture
                    // (failure injection counters).
                    if let (Some(parser_alloc), Some(cap_alloc)) = (self.alloc.as_mut(), cap.alloc) {
                        *parser_alloc = cap_alloc;
                    }
                }
            }
        }

        self.state = State::Start;
        self.capture = None;
    }

    /// Make sure that we have an allocator. If we don't, set the state to
    /// invalid so that any additional OSC data is discarded.
    #[inline]
    fn ensure_allocator(&mut self) -> bool {
        if self.alloc.is_some() {
            return true;
        }
        log::warn!(
            target: LOG_TARGET,
            "An allocator is required to process OSC {} but none was provided.",
            self.state
        );
        self.state = State::Invalid;
        false
    }

    /// Take the parser's fixed buffer for a fixed capture.
    fn take_buffer(&mut self) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.buffer);
        buf.clear();
        buf
    }

    /// Begin capturing trailing data. All inputs to next from this point
    /// forward will be captured into the capture buffer which may be
    /// backed by either a fixed size or allocating buffer depending on mode.
    ///
    /// Get the trailing data using `capture.trailing()`.
    #[inline]
    fn capture_trailing(&mut self, mode: CaptureMode) {
        debug_assert!(self.capture.is_none());
        match mode {
            CaptureMode::Fixed => {
                let buf = self.take_buffer();
                self.capture = Some(Capture::fixed(buf, Self::MAX_BUF));
            }

            CaptureMode::Allocating => {
                let Some(alloc) = self.alloc.as_mut() else {
                    // We don't have an allocator - fall back to a fixed buffer and hope
                    // that it's big enough.
                    self.capture_trailing(CaptureMode::Fixed);
                    return;
                };

                // The capture tracks its own copy of the allocator state;
                // count the allocation on the parser's allocator too.
                let mut cap_alloc = *alloc;
                match Capture::allocating(cap_alloc, self.max_allocating_bytes) {
                    Some(cap) => {
                        cap_alloc = cap.alloc.unwrap();
                        *alloc = cap_alloc;
                        self.capture = Some(cap);
                    }
                    None => {
                        // The allocator failed for some reason, fall back to a fixed buffer
                        // and hope that it's big enough.
                        self.capture_trailing(CaptureMode::Fixed);
                    }
                }
            }
        }
    }

    /// Called when the sequence's number turns out not to be one this
    /// parser implements, either at byte c or at the end of the sequence
    /// (c is null). Supported OSCs never reach this.
    ///
    /// With `unknown_max_bytes` at zero, this marks the sequence invalid,
    /// exactly as the parser did before unknown sequences were supported.
    /// Otherwise it starts collecting the sequence's bytes. The digits read
    /// so far are not stored anywhere, but the current state's name is
    /// exactly those digits (state `Osc13` after reading "13"). So the
    /// state name is written first, then c, and the collected bytes hold
    /// the whole sequence from its first byte.
    ///
    /// This is deliberately not inline. It is referenced from every
    /// prefix state in `next`, and inlining it there grows the state
    /// machine enough to measurably slow per-byte parsing of supported
    /// OSCs.
    #[inline(never)]
    fn unknown_or_invalid(&mut self, c: Option<u8>) {
        let max_bytes = self.unknown_max_bytes;
        if max_bytes == 0 {
            self.state = State::Invalid;
            return;
        }

        debug_assert!(self.capture.is_none());
        let prefix: &'static str = match self.state {
            State::Start => "",
            State::Invalid | State::Unknown | State::UnknownTruncated => unreachable!(),
            s => s.as_str(),
        };
        self.state = State::Unknown;

        // Limits past the fixed buffer need an allocation. Without an
        // allocator, or if allocating fails, the fixed buffer is used and
        // longer sequences are reported as truncated.
        let alloc = if max_bytes > Self::MAX_BUF {
            self.alloc.as_mut()
        } else {
            None
        };
        let cap = match alloc {
            Some(a) => match Capture::allocating(*a, max_bytes) {
                Some(cap) => {
                    *a = cap.alloc.unwrap();
                    cap
                }
                None => {
                    let buf = self.take_buffer();
                    Capture::fixed(buf, Self::MAX_BUF)
                }
            },
            None => {
                let buf = self.take_buffer();
                Capture::fixed(buf, max_bytes.min(Self::MAX_BUF))
            }
        };
        self.capture = Some(cap);

        let cap = self.capture.as_mut().unwrap();
        if cap.write_slice(prefix.as_bytes()).is_err() {
            return self.capture_failed();
        }
        if let Some(byte) = c {
            if cap.write_byte(byte).is_err() {
                return self.capture_failed();
            }
        }
    }

    /// Handle a failed capture write, either from reaching the capture's
    /// limit or from an allocation failure. Supported OSCs become invalid.
    /// Unknown OSCs keep what was captured and are reported as truncated.
    #[cold]
    #[inline(never)]
    fn capture_failed(&mut self) {
        match self.state {
            State::Unknown => {
                // Pin the limit to what was retained so every later write
                // fails at the first bounds check. Without this, a later
                // allocating write could succeed after an allocation
                // failure and leave a gap in the content.
                let cap = self.capture.as_mut().unwrap();
                cap.max_bytes = cap.trailing().len();
                self.state = State::UnknownTruncated;
            }
            // Already truncated: the write failed at the first bounds
            // check and there is nothing more to do.
            State::UnknownTruncated => {}
            _ => self.state = State::Invalid,
        }
    }

    /// Consume a slice of bytes, advancing the parser state. This is
    /// equivalent to calling `next` for each byte in order, but is much
    /// faster once a data capture is active because the remaining bytes
    /// are appended to the capture in bulk.
    pub fn next_slice(&mut self, input: &[u8]) {
        if self.state == State::Invalid {
            return;
        }

        // Run the state machine byte-at-a-time until a capture begins.
        // The command prefix before a capture starts is only a handful
        // of bytes so this loop is short in practice.
        let mut offset: usize = 0;
        while self.capture.is_none() {
            if offset >= input.len() {
                return;
            }
            self.next(input[offset]);
            offset += 1;
            if self.state == State::Invalid {
                return;
            }
        }

        let rem = &input[offset..];
        if rem.is_empty() {
            return;
        }
        if self.capture.as_mut().unwrap().write_slice(rem).is_err() {
            // We have overflowed our buffer or had some other error.
            // Discard any further input.
            self.capture_failed();
        }
    }

    /// Consume the next character c and advance the parser state.
    ///
    /// Inline so that the per-byte capture path costs no call (the Zig
    /// benchmark calls `next` with `.always_inline`). The prefix state
    /// machine, which only runs for the first few bytes of a sequence, is
    /// `next_prefix`.
    #[inline]
    pub fn next(&mut self, c: u8) {
        // If the state becomes invalid for any reason, just discard
        // any further input.
        if self.state == State::Invalid {
            return;
        }

        // If a writer has been initialized, we just accumulate the rest of the
        // OSC sequence in the writer's buffer and skip the state machine.
        if let Some(cap) = self.capture.as_mut() {
            if cap.write_byte(c).is_err() {
                // We have overflowed our buffer or had some other error.
                // Discard any further input.
                self.capture_failed();
            }
            return;
        }

        self.next_prefix(c);
    }

    /// The state machine part of `next`: the command prefix before a
    /// capture starts.
    fn next_prefix(&mut self, c: u8) {
        use State as S;
        match self.state {
            // handled above, so should never be here
            S::Invalid => unreachable!(),

            // unknown states always have an active capture
            S::Unknown | S::UnknownTruncated => unreachable!(),

            S::Start => match c {
                b'0' => self.state = S::Osc0,
                b'1' => self.state = S::Osc1,
                b'2' => self.state = S::Osc2,
                b'3' => self.state = S::Osc3,
                b'4' => self.state = S::Osc4,
                b'5' => self.state = S::Osc5,
                b'6' => self.state = S::Osc6,
                b'7' => self.state = S::Osc7,
                b'8' => self.state = S::Osc8,
                b'9' => self.state = S::Osc9,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc3 => match c {
                b'0' => self.state = S::Osc30,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc30 => match c {
                b'0' => self.state = S::Osc300,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc300 => match c {
                b'8' => self.state = S::Osc3008,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc3008 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc1 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                b'0' => self.state = S::Osc10,
                b'1' => self.state = S::Osc11,
                b'2' => self.state = S::Osc12,
                b'3' => self.state = S::Osc13,
                b'4' => self.state = S::Osc14,
                b'5' => self.state = S::Osc15,
                b'6' => self.state = S::Osc16,
                b'7' => self.state = S::Osc17,
                b'8' => self.state = S::Osc18,
                b'9' => self.state = S::Osc19,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc10 => match c {
                b';' => {
                    if self.ensure_allocator() {
                        self.capture_trailing(CaptureMode::Fixed)
                    }
                }
                b'4' => self.state = S::Osc104,
                b'5' => self.state = S::Osc105,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc11 => match c {
                b';' => {
                    if self.ensure_allocator() {
                        self.capture_trailing(CaptureMode::Fixed)
                    }
                }
                b'0' => self.state = S::Osc110,
                b'1' => self.state = S::Osc111,
                b'2' => self.state = S::Osc112,
                b'3' => self.state = S::Osc113,
                b'4' => self.state = S::Osc114,
                b'5' => self.state = S::Osc115,
                b'6' => self.state = S::Osc116,
                b'7' => self.state = S::Osc117,
                b'8' => self.state = S::Osc118,
                b'9' => self.state = S::Osc119,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc4
            | S::Osc12
            | S::Osc14
            | S::Osc15
            | S::Osc16
            | S::Osc17
            | S::Osc18
            | S::Osc19
            | S::Osc21
            | S::Osc104
            | S::Osc105
            | S::Osc110
            | S::Osc111
            | S::Osc112
            | S::Osc113
            | S::Osc114
            | S::Osc115
            | S::Osc116
            | S::Osc117
            | S::Osc118
            | S::Osc119 => match c {
                b';' => {
                    if self.ensure_allocator() {
                        self.capture_trailing(CaptureMode::Fixed)
                    }
                }
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc13 => match c {
                b';' => {
                    if self.ensure_allocator() {
                        self.capture_trailing(CaptureMode::Fixed)
                    }
                }
                b'3' => self.state = S::Osc133,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc2 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                b'1' => self.state = S::Osc21,
                b'2' => self.state = S::Osc22,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc5 => match c {
                b';' => {
                    if self.ensure_allocator() {
                        self.capture_trailing(CaptureMode::Fixed)
                    }
                }
                b'2' => self.state = S::Osc52,
                b'5' => self.state = S::Osc55,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc6 => match c {
                b'6' => self.state = S::Osc66,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc52 | S::Osc66 => match c {
                b';' => self.capture_trailing(CaptureMode::Allocating),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc55 => match c {
                b'2' => self.state = S::Osc552,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc7 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                b'2' => self.state = S::Osc72,
                b'5' => self.state = S::Osc75,
                b'7' => self.state = S::Osc77,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc75 => match c {
                b'0' => self.state = S::Osc750,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc750 => match c {
                b'1' => self.state = S::Osc7501,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc7501 => match c {
                b';' => self.capture_trailing(CaptureMode::Allocating),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc72 => match c {
                b';' => self.capture_trailing(CaptureMode::Allocating),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc77 => match c {
                b'7' => self.state = S::Osc777,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc133 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                b'7' => self.state = S::Osc1337,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc552 => match c {
                b'2' => self.state = S::Osc5522,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc1337 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc5522 => match c {
                b';' => self.capture_trailing(CaptureMode::Allocating),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc9 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                b'9' => self.state = S::Osc99,
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc99 => match c {
                // OSC 99 encoded payloads can exceed the fixed buffer.
                b';' => self.capture_trailing(CaptureMode::Allocating),
                _ => self.unknown_or_invalid(Some(c)),
            },

            S::Osc0 | S::Osc22 | S::Osc777 | S::Osc8 => match c {
                b';' => self.capture_trailing(CaptureMode::Fixed),
                _ => self.unknown_or_invalid(Some(c)),
            },
        }
    }

    /// End the sequence and return the command it contains, or null if it
    /// isn't a valid command.
    ///
    /// `terminator_ch` is the byte that ended the sequence. Commands that
    /// reply to the program end their reply the same way: BEL (0x07) gets
    /// a BEL reply, and any other byte, or null, gets an ST reply.
    ///
    /// A program can also cancel a sequence partway through by sending CAN
    /// or SUB instead of a terminator. Pass that byte as `terminator_ch`.
    /// The sequence is then discarded and this returns null, whatever
    /// command it contained. This matches xterm.
    ///
    /// The returned command borrows the parser and is only valid until the
    /// next call to the parser. Copy out any data you need to keep.
    pub fn end(&mut self, terminator_ch: Option<u8>) -> Option<Command<'_>> {
        if let Some(ch) = terminator_ch {
            // CAN, SUB
            if ch == 0x18 || ch == 0x1A {
                return None;
            }
        }

        use State as S;
        match self.state {
            S::Start => None,

            S::Invalid => None,

            S::Unknown | S::UnknownTruncated => parsers::unknown::parse(self, terminator_ch),

            // These states only lead to longer numbers (3 leads to 3008)
            // and are not OSCs themselves, so ending on one is unknown.
            S::Osc3
            | S::Osc30
            | S::Osc300
            | S::Osc55
            | S::Osc552
            | S::Osc6
            | S::Osc75
            | S::Osc750
            | S::Osc77 => {
                self.unknown_or_invalid(None);
                if self.state == S::Invalid {
                    return None;
                }
                parsers::unknown::parse(self, terminator_ch)
            }

            S::Osc0 | S::Osc2 => parsers::change_window_title::parse(self, terminator_ch),

            S::Osc1 => parsers::change_window_icon::parse(self, terminator_ch),

            S::Osc4
            | S::Osc5
            | S::Osc10
            | S::Osc11
            | S::Osc12
            | S::Osc13
            | S::Osc14
            | S::Osc15
            | S::Osc16
            | S::Osc17
            | S::Osc18
            | S::Osc19
            | S::Osc104
            | S::Osc105
            | S::Osc110
            | S::Osc111
            | S::Osc112
            | S::Osc113
            | S::Osc114
            | S::Osc115
            | S::Osc116
            | S::Osc117
            | S::Osc118
            | S::Osc119 => parsers::color::parse(self, terminator_ch),

            S::Osc7 => parsers::report_pwd::parse(self, terminator_ch),

            S::Osc8 => parsers::hyperlink::parse(self, terminator_ch),

            S::Osc9 => parsers::osc9::parse(self, terminator_ch),

            S::Osc21 => parsers::kitty_color::parse(self, terminator_ch),

            S::Osc22 => parsers::mouse_shape::parse(self, terminator_ch),

            S::Osc52 => parsers::clipboard_operation::parse(self, terminator_ch),

            S::Osc3008 => parsers::hierarchical_context::parse(self, terminator_ch),

            S::Osc66 => parsers::kitty_text_sizing::parse(self, terminator_ch),

            S::Osc72 => parsers::kitty_dnd_protocol::parse(self, terminator_ch),

            S::Osc99 => parsers::kitty_desktop_notification::parse(self, terminator_ch),

            S::Osc7501 => parsers::program_status::parse(self, terminator_ch),

            S::Osc133 => parsers::semantic_prompt::parse(self, terminator_ch),

            S::Osc777 => parsers::rxvt_extension::parse(self, terminator_ch),

            S::Osc1337 => parsers::iterm2::parse(self, terminator_ch),

            S::Osc5522 => parsers::kitty_clipboard_protocol::parse(self, terminator_ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAN: u8 = 0x18;
    const SUB: u8 = 0x1A;
    const BEL: u8 = 0x07;

    #[test]
    fn parser_end_with_can_or_sub_cancels_the_command() {
        let mut p = Parser::init(Some(Allocator::GLOBAL));

        // A fixed capture (OSC 2) cancelled by CAN.
        for &ch in b"2;title" {
            p.next(ch);
        }
        assert!(p.end(Some(CAN)).is_none());

        // An allocating capture (OSC 52) cancelled by SUB.
        p.reset();
        for &ch in b"52;c;Zm9v" {
            p.next(ch);
        }
        assert!(p.end(Some(SUB)).is_none());

        // The parser still works normally after a cancel.
        p.reset();
        for &ch in b"2;title" {
            p.next(ch);
        }
        let cmd = p.end(Some(BEL)).unwrap();
        assert_eq!(Command::ChangeWindowTitle(b"title"), cmd);
    }

    #[test]
    fn parser_allocating_captures_have_a_hard_limit() {
        let prefixes: [&[u8]; 5] = [b"52;", b"66;", b"72;", b"99;", b"5522;"];
        let limit = Parser::MAX_BUF + 1;

        for prefix in prefixes {
            let mut p = Parser::init(Some(Allocator::GLOBAL));
            p.max_allocating_bytes = limit;

            for &ch in prefix {
                p.next(ch);
            }
            for _ in 0..limit {
                p.next(b'a');
            }

            {
                let cap = p.capture.as_ref().unwrap();
                assert_eq!(limit, cap.trailing().len());
                assert_eq!(limit, cap.buffer_len());
            }

            p.next(b'a');
            assert_eq!(State::Invalid, p.state);
            let cap = p.capture.as_ref().unwrap();
            assert_eq!(limit, cap.trailing().len());
            assert_eq!(limit, cap.buffer_len());
        }
    }

    #[test]
    fn parser_next_slice_allocating_captures_have_a_hard_limit() {
        let limit = Parser::MAX_BUF + 1;

        let mut p = Parser::init(Some(Allocator::GLOBAL));
        p.max_allocating_bytes = limit;

        let data = vec![b'a'; limit];

        // Exactly at the limit stays valid and bounded.
        p.next_slice(b"52;");
        p.next_slice(&data);
        assert!(p.state != State::Invalid);
        {
            let cap = p.capture.as_ref().unwrap();
            assert_eq!(limit, cap.trailing().len());
            assert_eq!(limit, cap.buffer_len());
        }

        // One more byte overflows: the state becomes invalid and the
        // retained bytes and allocation stay bounded.
        p.next_slice(b"a");
        assert_eq!(State::Invalid, p.state);
        {
            let cap = p.capture.as_ref().unwrap();
            assert_eq!(limit, cap.trailing().len());
            assert_eq!(limit, cap.buffer_len());
        }
        assert!(p.end(None).is_none());
    }

    #[test]
    fn parser_next_slice_overflowing_slice_is_truncated_at_the_limit() {
        let mut p = Parser::init(Some(Allocator::GLOBAL));
        p.max_allocating_bytes = 4;

        p.next_slice(b"52;abcdef");
        assert_eq!(State::Invalid, p.state);
        assert!(p.end(None).is_none());

        let cap = p.capture.as_ref().unwrap();
        assert_eq!(b"abcd", cap.trailing());
        assert_eq!(4, cap.buffer_len());
    }

    #[test]
    fn parser_next_slice_matches_per_byte_parsing() {
        let input: &[u8] = b"52;c;aGVsbG8=";

        // Every two-way split of the input must parse identically to
        // the byte-at-a-time path.
        for split in 0..input.len() + 1 {
            let mut p = Parser::init(Some(Allocator::GLOBAL));
            p.next_slice(&input[..split]);
            p.next_slice(&input[split..]);

            let cmd = p.end(None).unwrap();
            let Command::ClipboardContents(cc) = cmd else {
                panic!("expected clipboard_contents");
            };
            assert_eq!(b'c', cc.kind);
            assert_eq!(b"aGVsbG8=", cc.data);
        }
    }

    #[test]
    fn parser_allocating_capture_limit_includes_parser_added_bytes() {
        let mut p = Parser::init(Some(Allocator::GLOBAL));
        p.max_allocating_bytes = 4;

        for &ch in b"52;abcd" {
            p.next(ch);
        }
        assert!(p.end(None).is_none());
        assert_eq!(State::Invalid, p.state);

        let cap = p.capture.as_ref().unwrap();
        assert_eq!(4, cap.trailing().len());
        assert_eq!(4, cap.buffer_len());
    }
}
