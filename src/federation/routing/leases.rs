//! Local leases on federated aliases. One keeper per alias holds a lease on
//! the executor while any local lease is live, and gives it up after.
use crate::registry::lease::{self, Holder, Lease, LeaseError, new_id};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How often a keeper renews; well inside the executor's lease.
pub(super) const RENEW_EVERY: Duration = Duration::from_secs(20);
/// What a keeper asks the executor for, in milliseconds.
pub(super) const REMOTE_TTL_MS: u32 = 60_000;
const PER_HOLDER: usize = 8;

struct Local {
    holder: Holder,
    alias: String,
    expires: Instant,
}

#[derive(Default)]
struct State {
    local: HashMap<String, Local>,
    keepers: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct RemoteLeases {
    state: Mutex<State>,
    /// Wakes keepers when a lease is released.
    pub(super) released: Notify,
}

impl RemoteLeases {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("remote lease lock poisoned")
    }

    /// A lease on `alias`, or the holder's existing one renewed. The flag says
    /// whether the alias needs a keeper started.
    pub(super) fn take(&self, holder: Holder, alias: &str, requested: Option<Duration>) -> Result<(Lease, bool), LeaseError> {
        let ttl = lease::ttl(requested);
        let mut state = self.lock();
        if let Some((id, local)) = state.local.iter_mut().find(|(_, local)| local.holder == holder && local.alias == alias) {
            local.expires = Instant::now() + ttl;
            return Ok((Lease { id: id.clone(), model: alias.to_owned(), ttl }, false));
        }
        let mine = state.local.values().filter(|local| local.holder == holder).count();
        if mine >= PER_HOLDER {
            return Err(LeaseError::Limit(holder, mine));
        }
        let id = new_id();
        state.local.insert(id.clone(), Local { holder, alias: alias.to_owned(), expires: Instant::now() + ttl });
        let start = state.keepers.insert(alias.to_owned());
        Ok((Lease { id, model: alias.to_owned(), ttl }, start))
    }

    pub(super) fn renew(&self, holder: &Holder, id: &str, requested: Option<Duration>) -> Result<Lease, LeaseError> {
        let ttl = lease::ttl(requested);
        let mut state = self.lock();
        let local = state.local.get_mut(id).filter(|local| local.holder == *holder).ok_or(LeaseError::Unknown)?;
        local.expires = Instant::now() + ttl;
        Ok(Lease { id: id.to_owned(), model: local.alias.clone(), ttl })
    }

    pub(super) fn release(&self, holder: &Holder, id: &str) -> Result<(), LeaseError> {
        let mut state = self.lock();
        if !state.local.get(id).is_some_and(|local| local.holder == *holder) {
            return Err(LeaseError::Unknown);
        }
        state.local.remove(id);
        drop(state);
        self.released.notify_waiters();
        Ok(())
    }

    /// Live local leases on `alias`.
    pub(super) fn count(&self, alias: &str, now: Instant) -> u32 {
        let count = self.lock().local.values().filter(|local| local.alias == alias && local.expires > now).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Whether the keeper of `alias` should go on. Expired leases end here;
    /// when none is left the keeper is done, decided under the same lock a
    /// new lease takes, so no lease is left without a keeper.
    pub(super) fn keep(&self, alias: &str, now: Instant) -> bool {
        let mut state = self.lock();
        state.local.retain(|_, local| local.expires > now);
        let live = state.local.values().any(|local| local.alias == alias);
        if !live {
            state.keepers.remove(alias);
        }
        live
    }
}

#[cfg(test)]
mod tests;
