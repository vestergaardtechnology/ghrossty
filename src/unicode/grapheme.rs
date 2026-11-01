//! Port of `src/unicode/grapheme.zig`.
//!
//! STATUS: ported
//!
//! Deviation: Zig's `graphemeWidth(comptime T: type, cps: []const T)` is
//! generic over `u21`/`u32`; Rust has no `u21`, so [`grapheme_width`] takes
//! `&[u32]` and always applies the `u32` invalid-codepoint checks (which are
//! no-ops for valid `u21` input).

use super::props_table::TABLE;
use super::uucode;
use super::uucode::grapheme::BreakState;
use super::uucode::types::GraphemeBreakNoControl;

/// Width change requested by a codepoint that continues a grapheme cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphemeWidthEffect {
    /// Do not append the codepoint to the cluster and leave break state as it
    /// was before seeing it.
    Ignore,

    /// Append the codepoint but leave the current cluster width unchanged.
    NoChange,

    /// Make the cluster occupy two terminal cells.
    Wide,

    /// Make the cluster occupy one terminal cell.
    Narrow,
}

/// Result of measuring the first grapheme cluster in a codepoint slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphemeWidth {
    /// Number of codepoints consumed from the input slice.
    pub len: usize,

    /// Display width in terminal cells.
    pub width: u8,
}

/// Determines if there is a grapheme break between two codepoints. This
/// must be called sequentially maintaining the state between calls.
///
/// This function does NOT work with control characters. Control characters,
/// line feeds, and carriage returns are expected to be filtered out before
/// calling this function. This is because this function is tuned for
/// Ghostty.
#[inline(always)]
pub fn grapheme_break(cp1: u32, cp2: u32, state: &mut BreakState) -> bool {
    let value = precompute::lookup(precompute::Key {
        gb1: TABLE.get(cp1).grapheme_break,
        gb2: TABLE.get(cp2).grapheme_break,
        state: *state,
    });
    *state = value.state();
    value.result()
}

/// Returns the width effect of appending cp after prev within a grapheme.
///
/// This is the shared width-decision kernel for the streaming terminal
/// printer and for grapheme_width. It assumes grapheme_break has already said
/// there is no break between prev and cp; it does not perform segmentation.
///
/// The .ignore result is important for invalid emoji variation selectors. The
/// terminal does not store those selectors in the cell, so callers must also
/// restore their grapheme break state and leave prev unchanged when they see
/// .ignore.
#[inline(always)]
pub fn grapheme_width_effect(prev: u32, cp: u32) -> GraphemeWidthEffect {
    // Emoji variation selectors modify the width of a valid base:
    // VS16 makes the grapheme wide and VS15 makes it narrow. Check that
    // prev forms a valid variation sequence in emoji-variation-sequences.txt;
    // if it does not, ignore the selector entirely.
    if cp == 0xFE0F || cp == 0xFE0E {
        let prev_props = TABLE.get(prev);
        if !prev_props.emoji_vs_base {
            return GraphemeWidthEffect::Ignore;
        }

        return match cp {
            0xFE0F => GraphemeWidthEffect::Wide,
            0xFE0E => GraphemeWidthEffect::Narrow,
            _ => unreachable!(),
        };
    }

    // If a code point contributes to the width of a grapheme, the whole
    // grapheme is at least width 2 because the first code point must be at
    // least width 1 to start. Prepend code points could effectively mean
    // the first code point should be width 0, but we don't handle that yet.
    if !TABLE.get(cp).width_zero_in_grapheme {
        return GraphemeWidthEffect::Wide;
    }

    GraphemeWidthEffect::NoChange
}

/// Measures the first grapheme cluster in cps using the same segmentation and
/// width rules as Terminal.print with mode 2027.
///
/// This is not a streaming API: cps must contain a complete first grapheme
/// cluster or the logical end of the string. If bytes/codepoints arrive in
/// chunks, keep buffering when this consumes all available codepoints and more
/// input may still arrive.
///
/// Values greater than U+10FFFF are accepted so FFI-facing callers can use
/// u32 input without trapping. An invalid value consumes one codepoint at
/// width 1 when it starts the slice, and terminates the current cluster when
/// it appears later.
pub fn grapheme_width(cps: &[u32]) -> GraphemeWidth {
    if cps.is_empty() {
        return GraphemeWidth { len: 0, width: 0 };
    }

    // The C API accepts u32 codepoints, so it can receive values outside
    // Unicode's range.
    if invalid_codepoint(cps[0]) {
        return GraphemeWidth { len: 1, width: 1 };
    }

    let mut len: usize = 1;
    let mut width = TABLE.get(cps[0]).width;
    let mut prev: u32 = cps[0];
    let mut state = BreakState::default();

    while len < cps.len() {
        // Treat invalid u32 input as a boundary so a valid prefix cluster can
        // still be returned without attempting to narrow the invalid value.
        if invalid_codepoint(cps[len]) {
            break;
        }

        let cp = cps[len];
        let state_before = state;
        if grapheme_break(prev, cp, &mut state) {
            break;
        }

        match grapheme_width_effect(prev, cp) {
            GraphemeWidthEffect::Ignore => state = state_before,
            GraphemeWidthEffect::NoChange => prev = cp,
            GraphemeWidthEffect::Wide => {
                width = 2;
                prev = cp;
            }
            GraphemeWidthEffect::Narrow => {
                width = 1;
                prev = cp;
            }
        }

        len += 1;
    }

    GraphemeWidth { len, width }
}

#[inline(always)]
fn invalid_codepoint(cp: u32) -> bool {
    cp > 0x10FFFF
}

/// This is all the structures and data for the precomputed lookup table
/// for all possible permutations of state and grapheme break properties.
/// Precomputation requires 2^13 keys of byte-sized values so the whole
/// table is 8KB.
mod precompute {
    use super::super::uucode::grapheme::{compute_grapheme_break_no_control, BreakState};
    use super::GraphemeBreakNoControl;

    /// Zig: `packed struct(u13) { state: u3, gb1: u5, gb2: u5 }`.
    pub(super) struct Key {
        pub state: BreakState,
        pub gb1: GraphemeBreakNoControl,
        pub gb2: GraphemeBreakNoControl,
    }

    impl Key {
        #[inline(always)]
        pub const fn index(&self) -> usize {
            self.state.table_index() | ((self.gb1 as usize) << 3) | ((self.gb2 as usize) << 8)
        }
    }

    /// Zig: `packed struct(u8) { result: bool, state: u3, _padding: u4 }`.
    #[derive(Clone, Copy)]
    pub(super) struct Value(u8);

    impl Value {
        const fn new(result: bool, state: BreakState) -> Value {
            Value((result as u8) | ((state.table_index() as u8) << 1))
        }

        #[inline(always)]
        pub fn result(self) -> bool {
            self.0 & 1 != 0
        }

        #[inline(always)]
        pub fn state(self) -> BreakState {
            let i = ((self.0 >> 1) & 0b111) as usize;
            // Every value in DATA comes from `Value::new` (the table index
            // of a valid BreakState, whose base is at most 2) or is zero,
            // so from_table_index's invalid-base panic is unreachable.
            if (i >> 1) & 0b11 == 3 {
                // SAFETY: see above.
                unsafe { std::hint::unreachable_unchecked() }
            }
            BreakState::from_table_index(i)
        }
    }

    /// `DATA[key.index()]` without a bounds check: a key packs into 13
    /// bits (a 3-bit state and two 5-bit break properties, as Zig's
    /// `packed struct(u13)`), so it always indexes DATA.
    #[inline(always)]
    pub(super) fn lookup(key: Key) -> Value {
        const _: () = assert!(GraphemeBreakNoControl::ALL.len() <= 1 << 5);
        const _: () = assert!(BreakState::TABLE_LEN <= 1 << 3);
        let i = key.index();
        debug_assert!(i < DATA.len());
        // SAFETY: i < 2^13 == DATA.len() (asserted above).
        unsafe { *DATA.get_unchecked(i) }
    }

    pub(super) static DATA: [Value; 1 << 13] = {
        let mut result = [Value(0); 1 << 13];
        let all = GraphemeBreakNoControl::ALL;
        let mut state_int = 0;
        while state_int < BreakState::TABLE_LEN {
            let mut i = 0;
            while i < all.len() {
                let mut j = 0;
                while j < all.len() {
                    let mut state = BreakState::from_table_index(state_int);
                    let key = Key {
                        gb1: all[i],
                        gb2: all[j],
                        state,
                    };
                    let v = compute_grapheme_break_no_control(key.gb1, key.gb2, &mut state);
                    result[key.index()] = Value::new(v, state);
                    j += 1;
                }
                i += 1;
            }
            state_int += 1;
        }

        assert!(std::mem::size_of::<[Value; 1 << 13]>() == 8192);
        result
    };
}

/// If you build this as a binary, we will verify the grapheme break
/// implementation. This iterates over billions of codepoints so it is
/// SLOW. It's not meant to be run in CI, but it's useful for debugging.
pub fn main() {
    // Set the min and max to control the test range.
    let min: u32 = 0;
    let max: u32 = uucode::config::MAX_CODE_POINT + 1;

    let mut state = BreakState::default();
    let mut uu_state = BreakState::default();
    for cp1 in min..max {
        if cp1 % 1000 == 0 {
            log::warn!("progress cp1={}", cp1);
        }

        if cp1 == '\r' as u32
            || cp1 == '\n' as u32
            || uucode::grapheme_break(cp1) == uucode::types::GraphemeBreak::Control
        {
            continue;
        }

        for cp2 in min..max {
            // Note: like the Zig original, this checks cp1's property.
            if cp2 == '\r' as u32
                || cp2 == '\n' as u32
                || uucode::grapheme_break(cp1) == uucode::types::GraphemeBreak::Control
            {
                continue;
            }

            let gb = grapheme_break(cp1, cp2, &mut state);
            let uu_gb = uucode::grapheme::is_break(cp1, cp2, &mut uu_state);
            if gb != uu_gb {
                log::warn!(
                    "cp1={:x} cp2={:x} gb={} state={:?} uu_gb={} uu_state={:?}",
                    cp1,
                    cp2,
                    gb,
                    state,
                    uu_gb,
                    uu_state
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_break_emoji_modifier() {
        // Emoji and modifier
        {
            let mut state = BreakState::default();
            assert!(!grapheme_break(0x261D, 0x1F3FF, &mut state));
        }

        // Non-emoji and emoji modifier
        {
            let mut state = BreakState::default();
            assert!(grapheme_break(0x22, 0x1F3FF, &mut state));
        }
    }

    #[test]
    fn long_emoji_zwj_sequences() {
        let mut state = BreakState::default();
        // 👩‍👩‍👧‍👦 (family: woman, woman, girl, boy)
        let mut it = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}_"
            .chars()
            .map(|c| c as u32);
        let mut cp1 = it.next().unwrap();
        let mut cp2 = it.next().unwrap();
        assert!(cp1 == 0x1F469); // 👩
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x200D);
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x1F469); // 👩
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x200D);
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x1F467); // 👧
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x200D);
        assert!(!grapheme_break(cp1, cp2, &mut state));

        cp1 = cp2;
        cp2 = it.next().unwrap();
        assert!(cp1 == 0x1F466); // 👦
        assert!(grapheme_break(cp1, cp2, &mut state)); // break
    }

    fn gw(len: usize, width: u8) -> GraphemeWidth {
        GraphemeWidth { len, width }
    }

    #[test]
    fn grapheme_width_variation_selectors() {
        assert_eq!(GraphemeWidthEffect::Wide, grapheme_width_effect(0x2764, 0xFE0F));
        assert_eq!(GraphemeWidthEffect::Narrow, grapheme_width_effect(0x23, 0xFE0E));
        assert_eq!(GraphemeWidthEffect::Ignore, grapheme_width_effect('x' as u32, 0xFE0F));

        assert_eq!(gw(2, 2), grapheme_width(&[0x2764, 0xFE0F]));
        assert_eq!(gw(2, 2), grapheme_width(&[0x23, 0xFE0F]));
        assert_eq!(gw(2, 1), grapheme_width(&['x' as u32, 0xFE0F]));
        assert_eq!(gw(3, 1), grapheme_width(&['x' as u32, 0xFE0F, 0xFE0F]));
        assert_eq!(gw(2, 1), grapheme_width(&[0x23, 0xFE0E]));
        assert_eq!(gw(2, 1), grapheme_width(&[0x231A, 0xFE0E]));
        assert_eq!(gw(3, 1), grapheme_width(&[0x231A, 0xFE0E, 0xFE0F]));
        assert_eq!(gw(4, 2), grapheme_width(&[0x1F3F4, 0x200D, 0x2620, 0xFE0F]));
    }

    #[test]
    fn grapheme_width_emoji_sequences() {
        assert_eq!(gw(5, 2), grapheme_width(&[0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467]));
        assert_eq!(gw(3, 2), grapheme_width(&[0x23, 0xFE0F, 0x20E3]));
        assert_eq!(gw(2, 1), grapheme_width(&['1' as u32, 0x20E3]));
        assert_eq!(gw(2, 2), grapheme_width(&[0x1F44B, 0x1F3FF]));
    }

    #[test]
    fn grapheme_width_spacing_marks_can_widen_narrow_clusters() {
        let mut mark: Option<u32> = None;
        for cp in 0..0x110000u32 {
            let props = TABLE.get(cp);
            if props.width != 1 || props.width_zero_in_grapheme {
                continue;
            }

            let mut state = BreakState::default();
            if !grapheme_break('a' as u32, cp, &mut state) {
                mark = Some(cp);
                break;
            }
        }

        assert!(mark.is_some());
        let cp = mark.unwrap();
        assert_eq!(1u8, TABLE.get(cp).width);
        assert!(!TABLE.get(cp).width_zero_in_grapheme);
        assert_eq!(gw(2, 2), grapheme_width(&['a' as u32, cp]));
    }

    #[test]
    fn grapheme_width_segmentation() {
        assert_eq!(gw(1, 1), grapheme_width(&['a' as u32]));
        assert_eq!(gw(1, 1), grapheme_width(&['a' as u32, 'b' as u32]));
        assert_eq!(gw(2, 2), grapheme_width(&[0x1F1E6, 0x1F1E7, 0x1F1E8]));
        assert_eq!(gw(1, 2), grapheme_width(&[0x1F1E8]));
        assert_eq!(gw(0, 0), grapheme_width(&[]));
        assert_eq!(gw(2, 0), grapheme_width(&[0x0301, 0x0302]));
    }

    #[test]
    fn grapheme_width_u32_invalid_codepoints_stand_alone() {
        assert_eq!(gw(1, 1), grapheme_width(&[0x110000, 0x0301]));
        assert_eq!(gw(1, 1), grapheme_width(&['a' as u32, 0x110000]));
    }

    /// Exhaustive check that the precomputed table agrees with uucode's
    /// own (precomputed) no-control break for every state and pair of
    /// properties.
    #[test]
    fn precomputed_table_matches_uucode() {
        for s in 0..BreakState::TABLE_LEN {
            for &gb1 in GraphemeBreakNoControl::ALL.iter() {
                for &gb2 in GraphemeBreakNoControl::ALL.iter() {
                    let mut a = BreakState::from_table_index(s);
                    let mut b = a;
                    let v = precompute::DATA[precompute::Key { state: a, gb1, gb2 }.index()];
                    a = v.state();
                    let expected = uucode::grapheme::precomputed_grapheme_break_no_control(gb1, gb2, &mut b);
                    assert_eq!(expected, v.result());
                    assert_eq!(b, a);
                }
            }
        }
    }

    /// GraphemeBreakTest.txt conformance through Ghostty's table-driven
    /// `grapheme_break` (lines containing controls, CR or LF are skipped
    /// since this function does not handle them).
    #[test]
    fn grapheme_break_test_txt() {
        let content = include_str!("testdata/GraphemeBreakTest.txt");
        let mut checked = 0usize;
        'line: for line in content.lines() {
            let line = match line.find('#') {
                Some(i) => &line[..i],
                None => line,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split(' ').collect();
            let cps: Vec<u32> = parts[1..]
                .iter()
                .step_by(2)
                .map(|s| u32::from_str_radix(s, 16).unwrap())
                .collect();
            for &cp in &cps {
                if matches!(
                    uucode::grapheme_break(cp),
                    uucode::types::GraphemeBreak::Control
                        | uucode::types::GraphemeBreak::Cr
                        | uucode::types::GraphemeBreak::Lf
                ) {
                    continue 'line;
                }
            }
            let mut state = BreakState::default();
            for i in 0..cps.len() - 1 {
                let mut expected = parts[2 + 2 * i] == "÷";
                let gb1 = TABLE.get(cps[i]).grapheme_break;
                let gb2 = TABLE.get(cps[i + 1]).grapheme_break;
                // Ghostty (via uucode) diverges from UAX #29 for emoji
                // modifiers not following an emoji modifier base.
                if gb2 == GraphemeBreakNoControl::EmojiModifier
                    && gb1 != GraphemeBreakNoControl::EmojiModifierBase
                {
                    expected = true;
                }
                assert_eq!(
                    expected,
                    grapheme_break(cps[i], cps[i + 1], &mut state),
                    "line: {line}"
                );
                checked += 1;
            }
        }
        assert!(checked > 1000);
    }
}
