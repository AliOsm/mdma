//! Durable, local caller consent. An approval is not a TLS identity or an
//! import grant, and neither the peer nor HTTP metadata can create one.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Disclosure {
    pub caller_spki_sha256: String,
    pub peer_id: String,
    pub model: String,
    pub operation: Operation,
    pub window: config::Window,
}

impl Disclosure {
    fn validate(&self) -> Result<(), Error> {
        config::validate_digest(&self.caller_spki_sha256)?;
        super::super::server_id::validate(&self.peer_id)
            .map_err(|_| Error::Denied("invalid disclosure peer"))?;
        config::validate_model(&self.model)?;
        self.window.validate()?;
        Ok(())
    }

    fn same_target(&self, other: &Self) -> bool {
        self.caller_spki_sha256 == other.caller_spki_sha256
            && self.peer_id == other.peer_id
            && self.model == other.model
            && self.operation == other.operation
    }
}

pub(super) fn validate(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.disclosures.len() > MAX_HISTORY {
        return Err(Error::Denied("too many disclosure approvals"));
    }
    let mut seen = BTreeSet::new();
    for grant in &snapshot.disclosures {
        grant.validate()?;
        if snapshot.tls_denied(&grant.caller_spki_sha256)
            || !snapshot.peers.contains_key(&grant.peer_id)
            || !seen.insert((
                &grant.caller_spki_sha256,
                &grant.peer_id,
                &grant.model,
                grant.operation as u8,
            ))
        {
            return Err(Error::Denied("invalid disclosure approval history"));
        }
    }
    Ok(())
}

impl Store {
    /// Explicit administrative consent to disclose this caller's input to
    /// exactly one peer/model/operation during a finite window.
    pub fn approve_disclosure(&self, grant: Disclosure) -> Result<(), Error> {
        grant.validate()?;
        self.update(move |snapshot| {
            if snapshot.tls_denied(&grant.caller_spki_sha256)
                || grant.window.valid_until_ms <= snapshot.clock_floor_ms
                || !snapshot
                    .peer(&grant.peer_id)
                    .is_some_and(|peer| peer.state == State::Active)
            {
                return Err(Error::Denied("disclosure approval denied"));
            }
            snapshot.disclosures.retain(|old| !old.same_target(&grant));
            snapshot.disclosures.push(grant);
            Ok(())
        })
    }

    pub fn remove_disclosure(
        &self,
        caller_spki_sha256: &str,
        peer_id: &str,
        model: &str,
        operation: Operation,
    ) -> Result<(), Error> {
        config::validate_digest(caller_spki_sha256)?;
        self.update(|snapshot| {
            snapshot.disclosures.retain(|grant| {
                !(grant.caller_spki_sha256 == caller_spki_sha256
                    && grant.peer_id == peer_id
                    && grant.model == model
                    && grant.operation == operation)
            });
            Ok(())
        })
    }

    /// Only the routing controller supplies the digest from an actual full
    /// local-client TLS handshake. Public administrative APIs do not mint a
    /// transport principal.
    pub(crate) fn disclosure_window(
        &self,
        caller_spki_sha256: &str,
        peer_id: &str,
        model: &str,
        operation: Operation,
        now_ms: u64,
    ) -> Result<config::Window, Error> {
        self.observe_time(now_ms)?;
        let snapshot = self.snapshot()?;
        if now_ms < snapshot.clock_floor_ms
            || snapshot.tls_denied(caller_spki_sha256)
            || !snapshot
                .peer(peer_id)
                .is_some_and(|peer| peer.state == State::Active)
        {
            return Err(Error::Denied("caller disclosure denied"));
        }
        snapshot
            .disclosures
            .iter()
            .find(|grant| {
                grant.caller_spki_sha256 == caller_spki_sha256
                    && grant.peer_id == peer_id
                    && grant.model == model
                    && grant.operation == operation
                    && grant.window.contains(now_ms)
            })
            .map(|grant| grant.window)
            .ok_or(Error::Denied("caller disclosure not approved"))
    }
}

#[cfg(test)]
mod tests;
