use crate::backend::{Backend, Error, Result};
use crate::kv_cache::KvCache;
use crate::model::Model;
use crate::model::memory::RequestMemory;
use crate::memory::Reservation;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

pub(super) struct PromptState<B: Backend> {
    pub kv: KvCache<B>,
    tokens: Vec<u32>,
    logits: Vec<f32>,
}

impl<B: Backend> PromptState<B> {
    fn new(kv: KvCache<B>) -> Self {
        Self { kv, tokens: Vec::new(), logits: Vec::new() }
    }

    pub fn prefill_reserved(&mut self, backend: &B, model: &Model<B>, tokens: &[u32], memory: Option<&Arc<Reservation>>) -> Result<(Vec<f32>, usize)> {
        if tokens.is_empty() || tokens.len() > self.kv.capacity() {
            return Err(Error::ShapeMismatch("prompt must be non-empty and fit the session context".into()));
        }
        if self.tokens == tokens && !self.logits.is_empty() {
            self.kv.set_len(tokens.len());
            return Ok((self.logits.clone(), tokens.len()));
        }
        let common = self.tokens.iter().zip(tokens).take_while(|(a, b)| a == b).count();
        // A shortened prompt needs its final row evaluated to obtain logits.
        let reused = common.min(tokens.len() - 1);
        self.tokens.clear();
        self.logits.clear();
        self.kv.set_len(reused);
        let mut ws = model.alloc_workspace_reserved(backend, tokens.len() - reused, memory)?;
        let logits = model.prefill(backend, &mut self.kv, &mut ws, &tokens[reused..], reused as u32)?;
        self.tokens.extend_from_slice(tokens);
        self.logits.clone_from(&logits);
        Ok((logits, reused))
    }
}

pub(super) struct PrefixSlot<B: Backend> {
    epoch: u64,
    state: Option<PromptState<B>>,
}

impl<B: Backend> PrefixSlot<B> {
    pub fn new() -> Self {
        Self { epoch: 0, state: None }
    }

    pub fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.state = None;
    }
}

pub(super) type SharedPrefix<B> = Arc<Mutex<PrefixSlot<B>>>;

pub(super) struct PromptLease<B: Backend> {
    pub state: Option<PromptState<B>>,
    pub retain: Cell<bool>,
    pub memory: RequestMemory,
    slot: Option<SharedPrefix<B>>,
    epoch: u64,
}

impl<B: Backend> PromptLease<B> {
    pub fn reserve(slot: Option<SharedPrefix<B>>) -> Self {
        let (epoch, state) = slot.as_ref().map_or((0, None), |slot| {
            let mut slot = slot.lock().unwrap();
            (slot.epoch, slot.state.take())
        });
        Self { state, retain: Cell::new(true), memory: RequestMemory::default(), slot, epoch }
    }

    pub fn admit(&mut self, backend: &B, model: &Model<B>, capacity: usize, prompt_tokens: usize) -> Result<()> {
        let new_kv = self.state.as_ref().is_none_or(|state| state.kv.capacity() < capacity).then_some(capacity);
        self.memory = model.reserve_request(backend, new_kv, prompt_tokens)?;
        Ok(())
    }

    pub fn allocate(&mut self, backend: &B, model: &Model<B>, n_ctx: usize) -> Result<()> {
        if self.state.as_ref().is_none_or(|state| state.kv.capacity() < n_ctx) {
            // Release an undersized cache before allocating its replacement.
            self.state = None;
            self.state = Some(PromptState::new(model.new_kv_cache_reserved(backend, n_ctx, self.memory.kv.take())?));
        }
        Ok(())
    }
}

impl<B: Backend> Drop for PromptLease<B> {
    fn drop(&mut self) {
        if !self.retain.get() || std::thread::panicking() {
            return;
        }
        if let (Some(slot), Some(state)) = (&self.slot, self.state.take()) {
            if state.tokens.is_empty() {
                return;
            }
            let replaced = {
                let mut slot = slot.lock().unwrap();
                if slot.epoch != self.epoch {
                    return;
                }
                slot.state.replace(state)
            };
            drop(replaced);
        }
    }
}

#[cfg(test)]
mod tests;
