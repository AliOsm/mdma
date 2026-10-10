//! Metal dispatch geometry and host-generated shader definitions.
use super::dtype_code;

pub(super) const ELEMENTWISE_THREADS: usize = 64;
pub(super) const SIMD_GROUPS: usize = 4;
pub(super) const ATTN_ELEMENTS_PER_LANE: usize = 16;
pub(super) const ROPE_CACHE_KEYS: usize = 16;
pub(super) const TILE_ROWS: usize = 32;
pub(super) const TILE_TOKENS: usize = 8;
pub(super) const TILE_K: usize = 32;
pub(super) const TILE_THREADS: usize = 128;
pub(super) const TILE_SIMD_WIDTH: usize = 32;

/// The shader consumes the same dtype tags as the other decoder backends.
pub(super) fn source(matrix_tile: bool) -> String {
    format!(
        "#define DTYPE_F32 {}\n#define DTYPE_F16 {}\n#define DTYPE_Q4K {}\n\
         #define DTYPE_Q6K {}\n#define DTYPE_BF16 {}\n#define DTYPE_Q5K {}\n\
         #define ATTN_ELEMENTS_PER_LANE {}\n#define METAL_MATRIX_TILE {}\n\
         #define TILE_ROWS {}\n#define TILE_TOKENS {}\n#define TILE_K {}\n#define TILE_THREADS {}\n{}",
        dtype_code::F32,
        dtype_code::F16,
        dtype_code::Q4K,
        dtype_code::Q6K,
        dtype_code::BF16,
        dtype_code::Q5K,
        ATTN_ELEMENTS_PER_LANE,
        u32::from(matrix_tile),
        TILE_ROWS,
        TILE_TOKENS,
        TILE_K,
        TILE_THREADS,
        include_str!("kernels.metal"),
    )
}
