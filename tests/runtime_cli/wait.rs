//! Bounded polling for end-to-end tests.
use std::time::{Duration, Instant};

/// Calls `probe` every 100 ms until it returns `Some`; `None` once `timeout` passes.
pub fn until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
