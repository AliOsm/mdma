//! Who keeps which model loaded, and until when.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_TTL: Duration = Duration::from_mins(1);
pub const MIN_TTL: Duration = Duration::from_secs(5);
pub const MAX_TTL: Duration = Duration::from_secs(600);
const PER_CLIENT: usize = 8;
const PER_PEER: usize = 4;
const TOTAL: usize = 256;

/// A lease's owner: a client certificate pin or a federation peer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Holder {
    Client(String),
    Peer(String),
}

impl Holder {
    const fn cap(&self) -> usize {
        match self {
            Self::Client(_) => PER_CLIENT,
            Self::Peer(_) => PER_PEER,
        }
    }
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(pin) => write!(f, "client {pin}"),
            Self::Peer(id) => write!(f, "peer {id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub id: String,
    pub model: String,
    pub ttl: Duration,
}

impl Lease {
    pub fn ttl_ms(&self) -> u32 {
        u32::try_from(self.ttl.as_millis()).unwrap_or(u32::MAX)
    }
}

/// A TTL from the wire; 0 asks for the default.
pub fn requested_ttl(ttl_ms: u32) -> Option<Duration> {
    (ttl_ms > 0).then(|| Duration::from_millis(u64::from(ttl_ms)))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("no model named {0:?}")]
    Missing(String),
    /// Unknown, expired, or held by someone else.
    #[error("no such lease")]
    Unknown,
    #[error("{0} already holds {1} leases")]
    Limit(Holder, usize),
    #[error("{0} leases are held; no more can be taken")]
    Full(usize),
}

struct Held {
    holder: Holder,
    model: String,
    slot: usize,
    expires: Instant,
}

#[derive(Default)]
pub(super) struct Table {
    leases: Mutex<HashMap<String, Held>>,
}

/// `requested` within 5 s to 10 min; 60 s when unset.
pub(crate) fn ttl(requested: Option<Duration>) -> Duration {
    requested.map_or(DEFAULT_TTL, |ttl| ttl.clamp(MIN_TTL, MAX_TTL))
}

impl Table {
    /// A new lease, or the holder's existing one on the same model renewed.
    /// The flag says whether the lease is new.
    pub(super) fn take(&self, holder: Holder, model: String, slot: usize, requested: Option<Duration>) -> Result<(Lease, bool), LeaseError> {
        let ttl = ttl(requested);
        let mut leases = self.leases.lock().expect("lease table lock poisoned");
        if let Some((id, held)) = leases.iter_mut().find(|(_, held)| held.holder == holder && held.slot == slot) {
            held.expires = Instant::now() + ttl;
            return Ok((Lease { id: id.clone(), model: held.model.clone(), ttl }, false));
        }
        let mine = leases.values().filter(|held| held.holder == holder).count();
        if mine >= holder.cap() {
            return Err(LeaseError::Limit(holder, mine));
        }
        if leases.len() >= TOTAL {
            return Err(LeaseError::Full(leases.len()));
        }
        let id = new_id();
        leases.insert(id.clone(), Held { holder, model: model.clone(), slot, expires: Instant::now() + ttl });
        Ok((Lease { id, model, ttl }, true))
    }

    pub(super) fn renew(&self, holder: &Holder, id: &str, requested: Option<Duration>) -> Result<Lease, LeaseError> {
        let ttl = ttl(requested);
        let mut leases = self.leases.lock().expect("lease table lock poisoned");
        let held = leases.get_mut(id).filter(|held| held.holder == *holder).ok_or(LeaseError::Unknown)?;
        held.expires = Instant::now() + ttl;
        Ok(Lease { id: id.to_owned(), model: held.model.clone(), ttl })
    }

    pub(super) fn model(&self, holder: &Holder, id: &str) -> Result<String, LeaseError> {
        let leases = self.leases.lock().expect("lease table lock poisoned");
        leases.get(id).filter(|held| held.holder == *holder).map(|held| held.model.clone()).ok_or(LeaseError::Unknown)
    }

    /// The released lease's slot.
    pub(super) fn release(&self, holder: &Holder, id: &str) -> Result<usize, LeaseError> {
        let mut leases = self.leases.lock().expect("lease table lock poisoned");
        if !leases.get(id).is_some_and(|held| held.holder == *holder) {
            return Err(LeaseError::Unknown);
        }
        Ok(leases.remove(id).expect("checked above").slot)
    }

    /// The slots of the leases that ended by `now`.
    pub(super) fn expire(&self, now: Instant) -> Vec<usize> {
        let mut leases = self.leases.lock().expect("lease table lock poisoned");
        let mut expired = Vec::new();
        leases.retain(|_, held| {
            let live = held.expires > now;
            if !live {
                expired.push(held.slot);
            }
            live
        });
        expired
    }
}

pub(crate) fn new_id() -> String {
    let mut bytes = [0; 16];
    aws_lc_rs::rand::fill(&mut bytes).expect("system random number generator failed");
    hex::encode(bytes)
}

#[cfg(test)]
mod tests;
