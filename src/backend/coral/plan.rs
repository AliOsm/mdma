//! Where each program's weights live in the tiles' wide memory. A program uses
//! `t` consecutive compute tiles from a shift `s` (`s + t <= 16`) and the same
//! 256-byte aligned span `[offset, offset + bytes)` on each of them. Placing the
//! largest programs first on the lowest skyline packs a whole model where
//! placing them in model order runs out of room on the last layers.

use super::codegen::fc::{FcProgram, WIDE_BYTES};
use super::isa::round_up;

pub const TILES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// First compute tile.
    pub shift: usize,
    /// Byte offset in every tile's wide memory.
    pub offset: usize,
}

/// Placements for `programs` in their order, or `None` when they do not fit
/// together.
pub fn place(programs: &[FcProgram]) -> Option<Vec<Placement>> {
    let mut order: Vec<usize> = (0..programs.len()).collect();
    let footprint = |i: usize| programs[i].geom().t() * programs[i].tile_bytes();
    order.sort_by_key(|&i| (std::cmp::Reverse(footprint(i)), i));
    let mut skyline = [0usize; TILES];
    let mut out = vec![None; programs.len()];
    for i in order {
        let (t, bytes) = (programs[i].geom().t(), programs[i].tile_bytes());
        let (offset, shift) = (0..=TILES.checked_sub(t)?)
            .map(|s| {
                (
                    round_up(skyline[s..s + t].iter().copied().max().unwrap_or(0), 256),
                    s,
                )
            })
            .min()?;
        if offset + bytes > WIDE_BYTES {
            return None;
        }
        skyline[shift..shift + t].fill(offset + bytes);
        out[i] = Some(Placement { shift, offset });
    }
    out.into_iter().collect()
}

/// The highest wide-memory byte any tile uses under `placements`.
pub fn high_water(programs: &[FcProgram], placements: &[Placement]) -> usize {
    programs
        .iter()
        .zip(placements)
        .map(|(p, at)| at.offset + p.tile_bytes())
        .max()
        .unwrap_or(0)
}
