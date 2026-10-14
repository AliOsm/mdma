pub(super) use mdma::backend::select::{NAMES, Result, Task, built, dispatch};

pub(super) fn mib(value: usize) -> Result<usize> {
    value
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "MiB byte count overflow".into())
}
