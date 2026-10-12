//! Single outstanding challenge per peer and replace-only verified snapshots.
//! A cache hit is metadata, never permanent permission to route a workload.

use super::catalog::{self, CatalogBody, SignedCatalogEnvelope, VerifyContext};
use super::config::TlsRole;
use super::control::SignedCatalog;
use super::tls::Binding;
use super::trust::{Action, Store};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const POLL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("catalog poll denied: {0}")]
    Poll(&'static str),
    #[error(transparent)]
    Trust(#[from] super::trust::Error),
    #[error(transparent)]
    Tls(#[from] super::tls::Error),
    #[error(transparent)]
    Catalog(#[from] catalog::Error),
}

#[derive(Clone)]
struct Cached {
    body: CatalogBody,
    key_id: String,
    generation: u64,
    deadline: Instant,
    expires_ms: u64,
}

#[derive(Default)]
struct Entry {
    outstanding: Option<([u8; 32], Instant)>,
    latest_challenge: Option<[u8; 32]>,
    cached: Option<Cached>,
}

#[derive(Clone, Default)]
pub struct Cache {
    peers: Arc<Mutex<BTreeMap<String, Entry>>>,
}

/// Drop consumes the challenge on cancellation. No clone or replay operation.
pub struct Poll {
    peers: Arc<Mutex<BTreeMap<String, Entry>>>,
    binding: Binding,
    nonce: [u8; 32],
    generation: u64,
    deadline: Instant,
}

impl Cache {
    pub fn begin(
        &self,
        store: &Store,
        binding: &Binding,
        now_ms: u64,
        now: Instant,
    ) -> Result<Poll, Error> {
        self.begin_until(store, binding, now_ms, now, now + POLL_TIMEOUT)
    }

    /// The runtime includes dialing/handshake time in its two-second budget.
    pub(crate) fn begin_until(
        &self,
        store: &Store,
        binding: &Binding,
        now_ms: u64,
        now: Instant,
        operation_deadline: Instant,
    ) -> Result<Poll, Error> {
        if now >= operation_deadline {
            return Err(Error::Poll("catalog poll timed out before challenge"));
        }
        if binding.role() != TlsRole::ServerAuth {
            return Err(Error::Poll("outbound server-auth binding required"));
        }
        let snapshot = binding.authorize(store, Action::PollCatalog, now_ms, now)?;
        let mut peers = self
            .peers
            .lock()
            .map_err(|_| Error::Poll("cache lock poisoned"))?;
        peers.retain(|_, entry| {
            entry
                .outstanding
                .as_ref()
                .is_some_and(|(_, deadline)| now < *deadline)
                || entry.cached.as_ref().is_some_and(|c| now < c.deadline)
        });
        if peers.len() >= super::config::MAX_PEERS && !peers.contains_key(binding.server_id()) {
            return Err(Error::Poll("catalog cache full"));
        }
        let entry = peers.entry(binding.server_id().into()).or_default();
        if entry
            .outstanding
            .as_ref()
            .is_some_and(|(_, deadline)| now < *deadline)
        {
            return Err(Error::Poll("a catalog poll is already outstanding"));
        }
        let mut nonce = [0; 32];
        aws_lc_rs::rand::fill(&mut nonce).map_err(|_| Error::Poll("nonce generation failed"))?;
        let deadline = (now + POLL_TIMEOUT)
            .min(binding.deadline())
            .min(operation_deadline);
        entry.outstanding = Some((nonce, deadline));
        entry.latest_challenge = Some(nonce);
        Ok(Poll {
            peers: self.peers.clone(),
            binding: binding.clone(),
            nonce,
            generation: snapshot.generation,
            deadline,
        })
    }

    /// Cancel outstanding challenges and withdraw snapshots on runtime trust
    /// notifications. Readers still recheck current authorization independently.
    pub(crate) fn invalidate_all(&self) -> Result<(), Error> {
        self.peers
            .lock()
            .map_err(|_| Error::Poll("cache lock poisoned"))?
            .clear();
        Ok(())
    }

    /// Recheck state, generation, signer and both clocks before every use.
    /// Import/disclosure and model/digest grants are an additional routing gate.
    pub fn snapshot(
        &self,
        store: &Store,
        peer_id: &str,
        now_ms: u64,
        now: Instant,
    ) -> Result<Option<CatalogBody>, Error> {
        let snapshot = store.authorize(peer_id, Action::PollCatalog, now_ms)?;
        let mut peers = self
            .peers
            .lock()
            .map_err(|_| Error::Poll("cache lock poisoned"))?;
        let Some(entry) = peers.get_mut(peer_id) else {
            return Ok(None);
        };
        let Some(cached) = &entry.cached else {
            return Ok(None);
        };
        if cached.generation != snapshot.generation
            || now >= cached.deadline
            || now_ms >= cached.expires_ms
        {
            entry.cached = None;
            return Ok(None);
        }
        if store.signing_key(peer_id, &cached.key_id, now_ms).is_err() {
            entry.cached = None;
            return Ok(None);
        }
        Ok(entry.cached.as_ref().map(|c| c.body.clone()))
    }
}

impl Poll {
    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn peer_id(&self) -> &str {
        self.binding.server_id()
    }

    pub fn finish(
        self,
        store: &Store,
        wire: SignedCatalog,
        now_ms: u64,
        now: Instant,
    ) -> Result<CatalogBody, Error> {
        let started = Instant::now();
        self.finish_at(store, wire, now_ms, now, || {
            let elapsed = started.elapsed();
            (
                now_ms.saturating_add(elapsed.as_millis().try_into().unwrap_or(u64::MAX)),
                now + elapsed,
            )
        })
    }

    fn finish_at(
        self,
        store: &Store,
        wire: SignedCatalog,
        now_ms: u64,
        now: Instant,
        mut completed: impl FnMut() -> (u64, Instant),
    ) -> Result<CatalogBody, Error> {
        // Consume first, even on invalid signatures or malformed data.
        {
            let mut peers = self
                .peers
                .lock()
                .map_err(|_| Error::Poll("cache lock poisoned"))?;
            let entry = peers
                .get_mut(self.peer_id())
                .ok_or(Error::Poll("poll no longer outstanding"))?;
            if entry
                .outstanding
                .as_ref()
                .is_none_or(|(nonce, _)| nonce != &self.nonce)
            {
                return Err(Error::Poll("poll challenge replaced or canceled"));
            }
            entry.outstanding = None;
        }
        if now >= self.deadline {
            return Err(Error::Poll("catalog poll timed out"));
        }
        let snapshot = self
            .binding
            .authorize(store, Action::PollCatalog, now_ms, now)?;
        if snapshot.generation != self.generation {
            return Err(Error::Poll("trust changed during poll"));
        }
        if wire.body_json.len() > catalog::MAX_BODY_JSON_BYTES
            || wire.key_id.len() > 40
            || wire.signature.len() != 64
        {
            return Err(Error::Poll("catalog envelope exceeds bounds"));
        }
        let key = store.signing_key(self.peer_id(), &wire.key_id, now_ms)?;
        let envelope = SignedCatalogEnvelope {
            key_id: wire.key_id,
            body_json: wire.body_json,
            signature: wire
                .signature
                .try_into()
                .map_err(|_| Error::Poll("invalid signature size"))?,
        };
        let body = catalog::verify_signed_catalog(
            &envelope,
            &key.bytes().map_err(super::trust::Error::from)?,
            &VerifyContext {
                now_ms,
                expected_origin: self.peer_id(),
                expected_destination: &snapshot.server_id,
                expected_nonce: &self.nonce,
                key_valid_from_ms: key.window.valid_from_ms,
                key_valid_until_ms: key.window.valid_until_ms,
            },
        )?;
        let expires_ms = body.expires_at_ms.min(key.window.valid_until_ms);
        // Verification is synchronous but not free. It must neither extend
        // the received lifetime nor publish after the operation's deadline.
        // Advance both receipt clocks by measured monotonic elapsed time;
        // wall-clock adjustments cannot buy more verification time.
        let (now_ms, now) = completed();
        if now >= self.deadline {
            return Err(Error::Poll("catalog verification exceeded poll deadline"));
        }
        let latest = self
            .binding
            .authorize(store, Action::PollCatalog, now_ms, now)?;
        if latest.generation != self.generation {
            return Err(Error::Poll("trust changed during verification"));
        }
        let mut peers = self
            .peers
            .lock()
            .map_err(|_| Error::Poll("cache lock poisoned"))?;
        let entry = peers
            .get_mut(self.peer_id())
            .ok_or(Error::Poll("cache entry removed"))?;
        if entry.latest_challenge != Some(self.nonce) {
            return Err(Error::Poll("newer poll started during verification"));
        }
        // Include final authorization and cache-lock acquisition, too.
        let (now_ms, now) = completed();
        if now >= self.deadline {
            return Err(Error::Poll("catalog publication exceeded poll deadline"));
        }
        let remaining_ms = expires_ms
            .checked_sub(now_ms)
            .filter(|ms| *ms > 0)
            .ok_or(Error::Poll("catalog already expired"))?
            .min(catalog::MAX_CATALOG_LIFETIME_MS);
        entry.cached = Some(Cached {
            body: body.clone(),
            key_id: envelope.key_id,
            generation: self.generation,
            deadline: now + Duration::from_millis(remaining_ms),
            expires_ms,
        });
        Ok(body)
    }
}

impl Drop for Poll {
    fn drop(&mut self) {
        if let Ok(mut peers) = self.peers.lock()
            && let Some(entry) = peers.get_mut(self.peer_id())
            && entry
                .outstanding
                .as_ref()
                .is_some_and(|(nonce, _)| nonce == &self.nonce)
        {
            entry.outstanding = None;
        }
    }
}

#[cfg(test)]
mod tests;
