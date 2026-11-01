//! Port of `src/unicode/lut.zig`.
//!
//! STATUS: ported
//!
//! Deviations: Zig's `Tables(Elem)` holds slices that are either static
//! (generated) or owned by the caller (from `Generator.generate`). In Rust the
//! borrowed form is [`Tables`] and the generator returns [`OwnedTables`]
//! (`Vec`s) which can be viewed as a [`Tables`]. `writeZig` became
//! [`Tables::write_rust`], which emits the format of the committed
//! `*_table_data.rs` files.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

// This whole file is based on the algorithm described here:
// https://here-be-braces.com/fast-lookup-of-unicode-properties/

/// The largest value of a Zig `u21`, the domain of the lookup tables.
const MAX_U21: u32 = 0x1FFFFF;

/// The context of a [`Generator`]: `get` returns the mapping for a given
/// codepoint and `eql` returns true if two mappings are equal.
pub trait Context<Elem> {
    fn get(&self, cp: u32) -> Elem;
    fn eql(&self, a: &Elem, b: &Elem) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerateError {
    BlockTooLarge,
    Stage2TooLarge,
}

impl fmt::Display for GenerateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlockTooLarge => f.write_str("BlockTooLarge"),
            Self::Stage2TooLarge => f.write_str("Stage2TooLarge"),
        }
    }
}

impl std::error::Error for GenerateError {}

const BLOCK_SIZE: usize = 256;
type Block = [u16; BLOCK_SIZE];

/// Creates a type that is able to generate a 3-level lookup table
/// from a Unicode codepoint to a mapping of type Elem. The lookup table
/// generally is expected to be codegen'd and then reloaded, although it
/// can in theory be generated at runtime.
pub struct Generator<Elem, C: Context<Elem>> {
    pub ctx: C,
    _elem: PhantomData<Elem>,
}

impl<Elem, C: Context<Elem>> Generator<Elem, C> {
    pub fn new(ctx: C) -> Self {
        Self {
            ctx,
            _elem: PhantomData,
        }
    }

    /// Generate the lookup tables.
    pub fn generate(&self) -> Result<OwnedTables<Elem>, GenerateError> {
        // Maps block => stage2 index
        let mut blocks_map: HashMap<Block, u16> = HashMap::new();

        // Our stages
        let mut stage1: Vec<u16> = Vec::new();
        let mut stage2: Vec<u16> = Vec::new();
        let mut stage3: Vec<Elem> = Vec::new();

        let mut block: Block = [0; BLOCK_SIZE];
        let mut block_len: usize = 0;
        for cp in 0..=MAX_U21 {
            // Get our block value and find the matching result value
            // in our list of possible values in stage3. This way, each
            // possible mapping only gets one entry in stage3.
            let elem = self.ctx.get(cp);
            let block_idx = match stage3.iter().position(|item| self.ctx.eql(item, &elem)) {
                Some(i) => i,
                None => {
                    stage3.push(elem);
                    stage3.len() - 1
                }
            };

            // The block stores the mapping to the stage3 index
            block[block_len] = u16::try_from(block_idx).map_err(|_| GenerateError::BlockTooLarge)?;
            block_len += 1;

            // If we still have space and we're not done with codepoints,
            // we keep building up the block. Conversely: we finalize this
            // block if we've filled it or are out of codepoints.
            if block_len < BLOCK_SIZE && cp != MAX_U21 {
                continue;
            }
            if block_len < BLOCK_SIZE {
                block[block_len..].fill(0);
            }

            // Look for the stage2 index for this block. If it doesn't exist
            // we add it to stage2 and update the mapping.
            let idx = match blocks_map.get(&block) {
                Some(&idx) => idx,
                None => {
                    let idx = u16::try_from(stage2.len()).map_err(|_| GenerateError::Stage2TooLarge)?;
                    blocks_map.insert(block, idx);
                    stage2.extend_from_slice(&block[..block_len]);
                    idx
                }
            };

            // Map stage1 => stage2 and reset our block
            stage1.push(idx);
            block_len = 0;
        }

        // All of our lengths must fit in a u16 for this to work
        assert!(stage1.len() <= u16::MAX as usize);
        assert!(stage2.len() <= u16::MAX as usize);
        assert!(stage3.len() <= u16::MAX as usize);

        Ok(OwnedTables {
            stage1,
            stage2,
            stage3,
        })
    }
}

/// Tables produced by [`Generator::generate`], owned by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedTables<Elem> {
    pub stage1: Vec<u16>,
    pub stage2: Vec<u16>,
    pub stage3: Vec<Elem>,
}

impl<Elem> OwnedTables<Elem> {
    pub fn as_tables(&self) -> Tables<'_, Elem> {
        Tables {
            stage1: &self.stage1,
            stage2: &self.stage2,
            stage3: &self.stage3,
        }
    }
}

/// Given a 3-level lookup table, can be used to look up a mapping for a
/// given codepoint, encode it out to Rust, etc.
#[derive(Debug)]
pub struct Tables<'a, Elem> {
    pub stage1: &'a [u16],
    pub stage2: &'a [u16],
    pub stage3: &'a [Elem],
}

impl<Elem> Clone for Tables<'_, Elem> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Elem> Copy for Tables<'_, Elem> {}

impl<Elem: Copy> Tables<'_, Elem> {
    /// Given a codepoint, returns the mapping for that codepoint. The
    /// codepoint must fit in a `u21` (`<= 0x1FFFFF`).
    #[inline(always)]
    pub fn get(&self, cp: u32) -> Elem {
        let high = (cp >> 8) as usize;
        let low = (cp & 0xFF) as usize;
        self.stage3[self.stage2[self.stage1[high] as usize + low] as usize]
    }
}

impl<Elem> Tables<'_, Elem> {
    /// Whether every stage2 index that `get` can form from stage1 and every
    /// stage3 index in stage2 is in bounds. Usable in const context.
    pub const fn indices_in_bounds(&self) -> bool {
        let mut i = 0;
        while i < self.stage1.len() {
            if self.stage1[i] as usize + 0xFF >= self.stage2.len() {
                return false;
            }
            i += 1;
        }
        let mut j = 0;
        while j < self.stage2.len() {
            if self.stage2[j] as usize >= self.stage3.len() {
                return false;
            }
            j += 1;
        }
        true
    }
}

/// Static lookup tables whose stage2 and stage3 indices were checked when
/// the table was built (at compile time for statics), so lookups only
/// bounds-check stage1 (the codepoint range). Zig's ReleaseFast lookups
/// carry no checks at all. Derefs to the plain [`Tables`].
#[derive(Debug)]
pub struct CheckedTables<Elem: 'static>(Tables<'static, Elem>);

impl<Elem> Clone for CheckedTables<Elem> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Elem> Copy for CheckedTables<Elem> {}

impl<Elem: Copy> CheckedTables<Elem> {
    /// Panics (at compile time in a static initializer) unless
    /// [`Tables::indices_in_bounds`] holds.
    pub const fn new(tables: Tables<'static, Elem>) -> Self {
        assert!(tables.indices_in_bounds(), "lookup table index out of bounds");
        CheckedTables(tables)
    }

    /// Given a codepoint, returns the mapping for that codepoint. The
    /// codepoint must fit in a `u21` (`<= 0x1FFFFF`). Same result as
    /// [`Tables::get`].
    #[inline(always)]
    pub fn get(&self, cp: u32) -> Elem {
        let high = (cp >> 8) as usize;
        let low = (cp & 0xFF) as usize;
        let i2 = self.0.stage1[high] as usize + low;
        // SAFETY: `new` checked that stage1 entries plus any low byte index
        // stage2 in bounds and that stage2 entries index stage3 in bounds.
        unsafe {
            let i3 = *self.0.stage2.get_unchecked(i2) as usize;
            *self.0.stage3.get_unchecked(i3)
        }
    }
}

impl<Elem> std::ops::Deref for CheckedTables<Elem> {
    type Target = Tables<'static, Elem>;

    fn deref(&self) -> &Tables<'static, Elem> {
        &self.0
    }
}

impl<Elem: fmt::Display> Tables<'_, Elem> {
    /// Writes the lookup table as Rust to the given writer. The written
    /// file exports three statics: STAGE1, STAGE2, and STAGE3, of which the
    /// type of STAGE3's elements is `elem_type`. These can be used to rebuild
    /// the lookup table in Rust (see `props_table.rs`).
    pub fn write_rust(&self, writer: &mut dyn std::io::Write, elem_type: &str) -> std::io::Result<()> {
        writeln!(writer, "// @generated. DO NOT EDIT.")?;
        writeln!(writer)?;
        write_u16_array(writer, "STAGE1", self.stage1)?;
        writeln!(writer)?;
        write_u16_array(writer, "STAGE2", self.stage2)?;
        writeln!(writer)?;
        writeln!(writer, "pub static STAGE3: [{}; {}] = [", elem_type, self.stage3.len())?;
        for entry in self.stage3 {
            writeln!(writer, "    {entry},")?;
        }
        writeln!(writer, "];")?;
        Ok(())
    }
}

fn write_u16_array(writer: &mut dyn std::io::Write, name: &str, values: &[u16]) -> std::io::Result<()> {
    writeln!(writer, "pub static {}: [u16; {}] = [", name, values.len())?;
    for chunk in values.chunks(24) {
        let line: Vec<String> = chunk.iter().map(|v| v.to_string()).collect();
        writeln!(writer, "    {},", line.join(", "))?;
    }
    writeln!(writer, "];")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ranges;

    impl Context<u8> for Ranges {
        fn get(&self, cp: u32) -> u8 {
            match cp {
                0x41..=0x5A => 1,
                0x1000..=0x10FF => 2,
                0x110000.. => 3,
                _ => 0,
            }
        }
        fn eql(&self, a: &u8, b: &u8) -> bool {
            a == b
        }
    }

    #[test]
    fn generate_and_lookup() {
        let gen = Generator::new(Ranges);
        let t = gen.generate().unwrap();
        assert_eq!(t.stage1.len(), 0x2000);
        assert_eq!(t.stage3, vec![0, 1, 2, 3]);
        let tables = t.as_tables();
        for cp in (0..=MAX_U21).step_by(7) {
            assert_eq!(Ranges.get(cp), tables.get(cp));
        }
        assert_eq!(1, tables.get(0x41));
        assert_eq!(2, tables.get(0x10FF));
        assert_eq!(3, tables.get(MAX_U21));

        let mut out = Vec::new();
        tables.write_rust(&mut out, "u8").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("pub static STAGE1: [u16; 8192] = ["));
        assert!(s.contains("pub static STAGE3: [u8; 4] = ["));
    }
}
