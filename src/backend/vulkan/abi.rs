//! Host dispatch constants passed to GLSL as defines by build.rs.
pub(super) const BINARY_THREADS: u32 = 256;
pub(super) const UNARY_THREADS: u32 = 256;
pub(super) const SPLIT_ROPE_THREADS: u32 = 256;
pub(super) const EMBED_DIM_THREADS: u32 = 64;
pub(super) const ROPE_PAIR_THREADS: u32 = 64;
pub(super) const ATTN_QUERY_TILE: u32 = 8;

pub(super) const UNARY_SCALE: u32 = 0;
pub(super) const UNARY_SOFTCAP: u32 = 1;
