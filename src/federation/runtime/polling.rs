//! Bounded, single-flight control polling with jitter and finite backoff.
use super::outbound::Credentials;
use crate::federation::{
    catalog_cache::Cache,
    trust::{Snapshot, State, Verification},
};
use rama::{
    error::BoxError,
    futures::{
        future::BoxFuture,
        stream::{FuturesUnordered, StreamExt},
    },
    rt::Executor,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const MAX_INFLIGHT: usize = 8;

fn jitter(min_ms: u64, max_ms: u64) -> Result<Duration, BoxError> {
    let mut bytes = [0; 8];
    aws_lc_rs::rand::fill(&mut bytes)?;
    Ok(Duration::from_millis(
        min_ms + u64::from_le_bytes(bytes) % (max_ms - min_ms + 1),
    ))
}

struct Schedule {
    due: Instant,
    failures: u32,
    running: bool,
}

impl Schedule {
    fn initial(now: Instant) -> Result<Self, BoxError> {
        Ok(Self {
            due: now + jitter(0, 2000)?,
            failures: 0,
            running: false,
        })
    }

    fn finish(&mut self, success: bool, now: Instant) -> Result<(), BoxError> {
        self.running = false;
        let delay = if success {
            self.failures = 0;
            jitter(18_000, 22_000)?
        } else {
            self.failures = self.failures.saturating_add(1).min(5);
            let base = (5000u64 << (self.failures - 1)).min(60_000);
            jitter(base * 4 / 5, base)?
        };
        self.due = now + delay;
        Ok(())
    }
}

fn synchronize(
    schedule: &mut BTreeMap<String, Schedule>,
    snapshot: &Snapshot,
    now: Instant,
) -> Result<(), BoxError> {
    schedule.retain(|id, _| snapshot.peer(id).is_some_and(|p| p.state == State::Active));
    for record in snapshot.records().filter(|p| p.state == State::Active) {
        if !schedule.contains_key(&record.policy.server_id) {
            schedule.insert(record.policy.server_id.clone(), Schedule::initial(now)?);
        }
    }
    Ok(())
}

fn verifications(snapshot: &Snapshot) -> BTreeMap<String, Verification> {
    snapshot
        .records()
        .map(|record| (record.policy.server_id.clone(), record.verification()))
        .collect()
}

fn due_peers(
    schedule: &mut BTreeMap<String, Schedule>,
    now: Instant,
    capacity: usize,
) -> Vec<String> {
    let mut ready: Vec<_> = schedule
        .iter()
        .filter(|(_, s)| !s.running && s.due <= now)
        .map(|(id, s)| (s.due, id.clone()))
        .collect();
    ready.sort();
    let ids: Vec<_> = ready
        .into_iter()
        .take(capacity.min(MAX_INFLIGHT))
        .map(|(_, id)| id)
        .collect();
    for id in &ids {
        if let Some(entry) = schedule.get_mut(id) {
            entry.running = true;
        }
    }
    ids
}

pub(super) struct Poller {
    credentials: Credentials,
    pub(super) cache: Cache,
}

impl Poller {
    pub(super) fn credentials(&self) -> &Credentials {
        &self.credentials
    }
    pub fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            cache: Cache::default(),
        }
    }

    pub async fn run(self, executor: Executor) -> Result<(), BoxError> {
        let mut changes = self.credentials.store.subscribe();
        let mut schedule = BTreeMap::new();
        let first = self.credentials.store.snapshot()?;
        let mut verified = verifications(&first);
        synchronize(&mut schedule, &first, Instant::now())?;
        let mut pending: FuturesUnordered<BoxFuture<'static, (String, bool)>> =
            FuturesUnordered::new();
        let stopping = async {
            match executor.guard() {
                Some(guard) => guard.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(stopping);
        loop {
            let now = Instant::now();
            // Earliest due first; a down peer cannot starve later peer IDs.
            for id in due_peers(
                &mut schedule,
                now,
                MAX_INFLIGHT.saturating_sub(pending.len()),
            ) {
                let credentials = self.credentials.clone();
                let cache = self.cache.clone();
                let executor = executor.clone();
                pending.push(Box::pin(async move {
                    let result = credentials.poll(&cache, &id, executor).await;
                    let success = result.is_ok();
                    let category = result.as_ref().err().map_or("none", |error| {
                        match error.downcast_ref::<crate::federation::catalog_cache::Error>() {
                            Some(crate::federation::catalog_cache::Error::Poll(reason)) => *reason,
                            Some(crate::federation::catalog_cache::Error::Busy) => "busy",
                            Some(crate::federation::catalog_cache::Error::Trust(_)) => "trust_denied",
                            Some(crate::federation::catalog_cache::Error::Tls(_)) => "tls_binding",
                            Some(crate::federation::catalog_cache::Error::Catalog(_)) => "catalog_invalid",
                            None => error.downcast_ref::<super::outbound::Stage>().map_or("transport_or_setup", |stage| stage.name),
                        }
                    });
                    // Never log credentials, payloads or remote error text.
                    let io_kind = result.as_ref().err().and_then(|error|
                        error.downcast_ref::<super::outbound::Stage>().and_then(super::outbound::Stage::io_kind));
                    eprintln!("mdma federation catalog poll peer={id} verified={success} category={category} io_kind={io_kind:?}");
                    (id, success)
                }));
            }
            let next = if pending.len() < MAX_INFLIGHT {
                schedule
                    .values()
                    .filter(|s| !s.running)
                    .map(|s| s.due)
                    .min()
            } else {
                None
            };
            let wake = async {
                match next {
                    Some(due) => tokio::time::sleep_until(due.into()).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                () = &mut stopping => return Ok(()),
                changed = changes.changed() => {
                    changed.map_err(|_| "federation trust notifications closed")?;
                    let snapshot = self.credentials.store.snapshot()?;
                    let now = Instant::now();
                    // Polls begun under the old trust cannot publish: start
                    // them over soon rather than back off after a failure.
                    pending.clear();
                    for entry in schedule.values_mut().filter(|entry| entry.running) {
                        *entry = Schedule::initial(now)?;
                    }
                    // A peer whose state or credentials changed loses its
                    // catalog at once; grant, route and consent edits keep it.
                    let current = verifications(&snapshot);
                    for (id, before) in &verified {
                        if current.get(id) != Some(before) {
                            self.cache.invalidate(id)?;
                            schedule.remove(id);
                        }
                    }
                    verified = current;
                    synchronize(&mut schedule, &snapshot, now)?;
                }
                Some((id, success)) = pending.next(), if !pending.is_empty() => {
                    if let Some(entry) = schedule.get_mut(&id) {
                        entry.finish(success, Instant::now())?;
                    }
                }
                () = wake => {},
            }
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        // Shutdown/cancellation withdraws metadata even if a read handle
        // outlives the listener. Poisoned cache locks already fail closed.
        let _ = self.cache.invalidate_all();
    }
}

#[cfg(test)]
mod tests;
