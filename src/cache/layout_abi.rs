//! On-disk v3 and HIP kernel ABI; changing these requires a layout version bump.
pub const Q4K_BLOCK_BYTES: usize = 144;
pub const Q4K_QUANTS_OFFSET: usize = 16;
pub const Q4K_DTYPE_TAG: i32 = 6;
