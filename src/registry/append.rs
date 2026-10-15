//! A list that only grows: readers keep plain references while one writer appends.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct Append<T> {
    /// Boxed so an entry never moves; a set cell is never cleared.
    items: Box<[OnceLock<Box<T>>]>,
    len: AtomicUsize,
}

impl<T> Append<T> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: (0..capacity).map(|_| OnceLock::new()).collect(),
            len: AtomicUsize::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items[..self.len()]
            .get(index)?
            .get()
            .map(|item| &**item)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items[..self.len()]
            .iter()
            .filter_map(|item| item.get().map(|item| &**item))
    }

    /// Its index, or the item back when full. Callers serialize pushes.
    pub fn push(&self, item: T) -> Result<usize, T> {
        let index = self.len.load(Ordering::Acquire);
        let Some(cell) = self.items.get(index) else {
            return Err(item);
        };
        cell.set(Box::new(item)).map_err(|item| *item)?;
        self.len.store(index + 1, Ordering::Release);
        Ok(index)
    }
}

impl<T> std::ops::Index<usize> for Append<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        self.get(index).expect("index within the listed items")
    }
}
