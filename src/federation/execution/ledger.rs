//! Process-local request IDs and resource charges, never inputs/results.
//! Claims must follow authenticated peer authorization. A catalog is not a grant.

use crate::federation::{config, server_id};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

const RETENTION: Duration = Duration::from_secs(300);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const NODE_ENTRIES: usize = 16_384;
const NODE_CONCURRENCY: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RequestId([u8; 16]);

impl RequestId {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, Error> {
        Ok(Self(bytes.try_into().map_err(|_| Error::InvalidId)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Completed,
    Failed,
    // Only rejected admissions record these; `reject` is not wired yet.
    #[cfg(test)]
    Denied,
    Cancelled,
    #[cfg(test)]
    DeadlineExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum Error {
    #[error("request ID must be 16 bytes")]
    InvalidId,
    #[error("invalid peer identity or limits")]
    InvalidPolicy,
    #[error("duplicate request ID")]
    Duplicate,
    #[error("peer request rate exceeded")]
    Rate,
    #[error("peer or local concurrency exceeded")]
    Concurrency,
    #[error("peer or local ledger full")]
    Full,
    #[error("execution accounting unavailable")]
    Unavailable,
}

enum Entry {
    Active,
    Terminal {
        finished: Instant,
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "tombstone outcomes are only inspected by tests")
        )]
        outcome: Outcome,
    },
}

struct Peer {
    entries: BTreeMap<RequestId, Entry>,
    active: usize,
    rate_start: Instant,
    requests: u32,
}

struct State {
    peers: BTreeMap<String, Peer>,
    active: usize,
    entries: usize,
    last_now: Instant,
}

impl State {
    fn advance(&mut self, now: Instant) -> Instant {
        self.last_now = self.last_now.max(now);
        self.last_now
    }

    fn prune(&mut self, now: Instant) {
        let mut removed = 0;
        self.peers.retain(|_, peer| {
            let before = peer.entries.len();
            peer.entries.retain(|_, entry| match entry {
                Entry::Active => true,
                Entry::Terminal { finished, .. } => {
                    now.saturating_duration_since(*finished) < RETENTION
                }
            });
            removed += before - peer.entries.len();
            if now.saturating_duration_since(peer.rate_start) >= RATE_WINDOW {
                peer.rate_start = now;
                peer.requests = 0;
            }
            !peer.entries.is_empty() || peer.requests != 0
        });
        self.entries -= removed;
    }
}

pub(super) struct Ledger {
    state: Mutex<State>,
    // Test clocks exercise retention without sleeping five minutes. Production
    // uses Instant::now; UTC, credentials, catalogs and external timestamps
    // cannot shorten retention or reset a running process's accounting.
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl Ledger {
    pub(super) fn new() -> Arc<Self> {
        Self::with_clock(Arc::new(Instant::now))
    }

    fn with_clock(clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                peers: BTreeMap::new(),
                active: 0,
                entries: 0,
                last_now: clock(),
            }),
            clock,
        })
    }

    /// Atomically claim an authenticated origin's ID across both operations.
    /// No admission queue: validation charges peer and local concurrency.
    /// Tokio worker scheduling (including time waiting for a blocking slot)
    /// stays charged; the RPC integration must monitor its fixed deadline.
    /// Caller must still revalidate current authorization and memory/power
    /// admission before starting the job; this claim grants neither.
    pub(super) fn claim(
        self: &Arc<Self>,
        origin: &str,
        id: RequestId,
        limits: &config::Limits,
    ) -> Result<Claim, Error> {
        server_id::validate(origin).map_err(|_| Error::InvalidPolicy)?;
        limits.validate().map_err(|_| Error::InvalidPolicy)?;
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let now = state.advance((self.clock)());
        state.prune(now);
        if !state.peers.contains_key(origin) && state.peers.len() >= config::MAX_PEERS {
            return Err(Error::Full);
        }
        let node_full = state.entries >= NODE_ENTRIES;
        let node_busy = state.active >= NODE_CONCURRENCY;
        let peer = state.peers.entry(origin.into()).or_insert_with(|| Peer {
            entries: BTreeMap::new(),
            active: 0,
            rate_start: now,
            requests: 0,
        });
        let rate_limited = peer.requests >= limits.requests_per_minute;
        peer.requests = peer.requests.saturating_add(1).min(60_000);
        // Duplicates always fail without replay/attachment, even after limits
        // shrink or the ledger fills. Rate work is only this bounded lookup.
        if peer.entries.contains_key(&id) {
            return Err(Error::Duplicate);
        }
        if rate_limited {
            return Err(Error::Rate);
        }
        if node_full || peer.entries.len() >= limits.ledger_entries as usize {
            return Err(Error::Full);
        }
        if node_busy || peer.active >= limits.concurrency as usize {
            // The ID was otherwise claimable. Rejected concurrency admission
            // still becomes a terminal ID, never an implicit retry opportunity.
            peer.entries.insert(
                id,
                Entry::Terminal {
                    finished: now,
                    outcome: Outcome::Failed,
                },
            );
            state.entries += 1;
            return Err(Error::Concurrency);
        }
        peer.entries.insert(id, Entry::Active);
        peer.active += 1;
        state.active += 1;
        state.entries += 1;
        Ok(Claim {
            ledger: self.clone(),
            origin: origin.into(),
            id,
            outcome: Outcome::Failed,
        })
    }

    fn finish(&self, origin: &str, id: RequestId, outcome: Outcome) {
        // Poisoned state stays unusable. Never rebuild or release imaginary
        // capacity after a panic inside accounting.
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = state.advance((self.clock)());
        let Some(peer) = state.peers.get_mut(origin) else {
            return;
        };
        let Some(entry) = peer.entries.get_mut(&id) else {
            return;
        };
        if !matches!(entry, Entry::Active) {
            return;
        }
        *entry = Entry::Terminal {
            finished: now,
            outcome,
        };
        peer.active -= 1;
        state.active -= 1;
    }
}

/// Dropping validation also leaves a tombstone. Move this claim into the
/// actual worker through `spawn_blocking`; dropping/aborting its JoinHandle
/// cannot release a running worker's concurrency or ID.
pub(super) struct Claim {
    ledger: Arc<Ledger>,
    origin: String,
    id: RequestId,
    outcome: Outcome,
}

impl Claim {
    /// No inference queue behind Tokio's shared blocking pool. The already
    /// claimed node slot bounds dedicated native threads to one. Only joining
    /// the FINISHED thread uses Tokio's pool; queued join work cannot run model
    /// compute. Its claim remains charged until the actual join completes.
    pub(super) fn spawn_dedicated(
        mut self,
        job: impl FnOnce() -> Outcome + Send + 'static,
    ) -> Result<JoinHandle<Outcome>, std::io::Error> {
        let (done, ready) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("mdma-federation".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                self.outcome = result.as_ref().copied().unwrap_or(Outcome::Failed);
                // Keep the claim through actual join (including time waiting for
                // a join slot). This also bounds finished thread handles to one.
                // The inference captures have already been destroyed/unwound.
                let _ = done.send(self);
                result
            })?;
        Ok(tokio::spawn(async move {
            let claim = ready.await.ok();
            tokio::task::spawn_blocking(move || {
                let result = thread.join();
                drop(claim);
                match result {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(panic)) | Err(panic) => std::panic::resume_unwind(panic),
                }
            })
            .await
            .expect("native thread join failed")
        }))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.ledger.finish(&self.origin, self.id, self.outcome);
    }
}

#[cfg(test)]
mod tests;
