//! Local administrative enrollment and durable revocations.
//!
//! Opening a missing state file never creates an empty trust store. Initialize
//! explicitly, and keep its lock file: replacing that inode defeats locking.
//! Publication is validated, persisted and synced before becoming visible.

use super::config::{self, ControlPermissions, Operation, Peer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::watch;

const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HISTORY: usize = 4096;
/// Models peers may install here; one catalog lists at most this many.
pub const MAX_INSTALLED: usize = super::catalog::MAX_DESCRIPTORS;

/// A model a peer pushed here. Who may see and run it follows each peer's
/// `installed_exports`, not who pushed it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub artifact_id: String,
    pub operation: Operation,
    pub installer: String,
    pub installed_at_ms: u64,
}

mod disclosure;
pub use disclosure::Disclosure;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyHistory {
    public_key: String,
    window: config::Window,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("trust storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid trust state: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Config(#[from] config::Error),
    #[error("trust denied: {0}")]
    Denied(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Staged,
    Active,
    Suspended,
    Revoked,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub state: State,
    pub policy: Peer,
    /// Immutable associations, including keys absent from the current policy.
    signing_history: BTreeMap<String, KeyHistory>,
    /// Retired key IDs cannot become eligible again after clock rollback/reload.
    retired_signing: BTreeSet<String>,
    retired_tls: BTreeSet<String>,
    tls_history: BTreeMap<String, config::TlsPin>,
}

impl Record {
    pub fn pin_eligible(&self, pin: &config::TlsPin, now_ms: u64) -> bool {
        pin.window.contains(now_ms) && !self.retired_tls.contains(&pin_label(pin))
    }

    /// What this peer's signed catalog was checked against.
    pub fn verification(&self) -> Verification {
        Verification {
            state: self.state,
            peer: self.policy.without_grants(),
            signing_history: self.signing_history.clone(),
            retired_signing: self.retired_signing.clone(),
            retired_tls: self.retired_tls.clone(),
            tls_history: self.tls_history.clone(),
        }
    }
}

/// A peer's state and credentials without its grants. A catalog cached under
/// one stays usable while it is unchanged: editing grants, routes or
/// disclosures keeps it, a new state, key, pin or endpoint does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    state: State,
    peer: Peer,
    signing_history: BTreeMap<String, KeyHistory>,
    retired_signing: BTreeSet<String>,
    retired_tls: BTreeSet<String>,
    tls_history: BTreeMap<String, config::TlsPin>,
}

fn pin_label(pin: &config::TlsPin) -> String {
    format!(
        "{:?}|{}|{}",
        pin.role, pin.spki_sha256, pin.window.valid_until_ms
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    format: String,
    pub server_id: String,
    pub generation: u64,
    /// Persistence assumes trusted local storage, not malicious backup rollback.
    clock_floor_ms: u64,
    peers: BTreeMap<String, Record>,
    denied_tls: BTreeSet<String>,
    denied_signing: BTreeSet<String>,
    local_signing_history: BTreeMap<String, KeyHistory>,
    retired_local_signing: BTreeSet<String>,
    /// Local caller approvals are never imported from a peer catalog.
    #[serde(default)]
    disclosures: Vec<Disclosure>,
    /// Models peers pushed here, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    installed: BTreeMap<String, Installed>,
}

impl Snapshot {
    pub fn installed(&self) -> &BTreeMap<String, Installed> {
        &self.installed
    }

    /// What `record` sees in its catalog: its explicit exports, then, with
    /// `installed_exports`, every installed model, at most one catalog's worth.
    pub fn catalog_exports(&self, record: &Record) -> Vec<String> {
        let mut exports = record.policy.catalog_exports.clone();
        if record.policy.installed_exports {
            for name in self.installed.keys() {
                if !exports.contains(name) {
                    exports.push(name.clone());
                }
            }
        }
        exports.truncate(super::catalog::MAX_DESCRIPTORS);
        exports
    }

    /// Current reviewed records, including suspended/revoked history. Reading
    /// these does not confer authorization.
    pub fn disclosures(&self) -> &[Disclosure] {
        &self.disclosures
    }

    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.peers.values()
    }

    pub fn peer(&self, id: &str) -> Option<&Record> {
        self.peers.get(id)
    }

    pub fn tls_denied(&self, digest: &str) -> bool {
        self.denied_tls.contains(digest)
    }

    pub fn signing_denied(&self, key: &str) -> bool {
        self.denied_signing.contains(key)
    }

    fn validate(&self, expected_id: &str) -> Result<(), Error> {
        disclosure::validate(self)?;
        if self.format != "mdma.federation.trust.v1"
            || self.server_id != expected_id
            || self.generation == 0
            || self.clock_floor_ms > config::MAX_SAFE_TIMESTAMP
            || self.peers.len() > config::MAX_PEERS
            || self.denied_tls.len() > MAX_HISTORY
            || self.denied_signing.len() > MAX_HISTORY
        {
            return Err(Error::Denied("invalid trust header or bounds"));
        }
        if self.local_signing_history.len() > MAX_HISTORY
            || !self
                .retired_local_signing
                .iter()
                .all(|id| self.local_signing_history.contains_key(id))
        {
            return Err(Error::Denied("invalid local signing history"));
        }
        for (id, key) in &self.local_signing_history {
            super::signing::validate_key_id(id)
                .map_err(|_| Error::Denied("invalid local signing ID"))?;
            decode_key(&key.public_key)?;
            key.window.validate()?;
        }
        super::server_id::validate(&self.server_id)
            .map_err(|_| Error::Denied("invalid local identity"))?;
        for digest in &self.denied_tls {
            config::validate_digest(digest)?;
        }
        for key in &self.denied_signing {
            decode_key(key)?;
        }
        for (id, record) in &self.peers {
            record.policy.validate()?;
            if id != &record.policy.server_id
                || id == expected_id
                || record.signing_history.len() > MAX_HISTORY
                || record.retired_tls.len() > MAX_HISTORY
                || record.tls_history.len() > MAX_HISTORY
                || !record
                    .retired_signing
                    .iter()
                    .all(|id| record.signing_history.contains_key(id))
                || !record
                    .retired_tls
                    .iter()
                    .all(|id| record.tls_history.contains_key(id))
            {
                return Err(Error::Denied("invalid peer history"));
            }
            for (id, key) in &record.signing_history {
                super::signing::validate_key_id(id)
                    .map_err(|_| Error::Denied("invalid historical signing key ID"))?;
                decode_key(&key.public_key)?;
                key.window.validate()?;
            }
            for (label, pin) in &record.tls_history {
                config::validate_digest(&pin.spki_sha256)?;
                pin.window.validate()?;
                if label != &pin_label(pin) {
                    return Err(Error::Denied("invalid TLS window history"));
                }
            }
            for key in &record.policy.signing_keys {
                if !record
                    .signing_history
                    .get(&key.key_id)
                    .is_some_and(|history| {
                        history.public_key == key.public_key && history.window == key.window
                    })
                {
                    return Err(Error::Denied("signing key history mismatch"));
                }
            }
            for pin in &record.policy.tls_pins {
                if record.tls_history.get(&pin_label(pin)) != Some(pin) {
                    return Err(Error::Denied("TLS window history mismatch"));
                }
            }
            if record.state != State::Active
                && (record.policy.has_workload_grants()
                    || record.policy.control_permissions.scan_capabilities)
            {
                return Err(Error::Denied("inactive peer has workload/catalog grants"));
            }
            if matches!(record.state, State::Suspended | State::Revoked)
                && record.policy.control_permissions.identify
            {
                return Err(Error::Denied("disabled peer has Identify grant"));
            }
        }
        if self.installed.len() > MAX_INSTALLED {
            return Err(Error::Denied("too many installed models"));
        }
        for (model, installed) in &self.installed {
            config::validate_model(model)?;
            config::validate_digest(&installed.artifact_id)?;
            super::server_id::validate(&installed.installer)
                .map_err(|_| Error::Denied("invalid installer identity"))?;
            if installed.installed_at_ms > config::MAX_SAFE_TIMESTAMP {
                return Err(Error::Denied("invalid install time"));
            }
        }
        Ok(())
    }
}

fn decode_key(key: &str) -> Result<(), Error> {
    let key = config::SigningKey {
        key_id: "ed25519:validation".into(),
        public_key: key.into(),
        window: config::Window {
            valid_from_ms: 0,
            valid_until_ms: 1,
        },
    };
    key.bytes()?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum Action<'a> {
    /// Connection eligibility alone; never an RPC or workload permission.
    Authenticate,
    Identify,
    ScanCapabilities,
    /// Outbound catalog retrieval is not permission to disclose local inventory.
    PollCatalog,
    Execute {
        model: &'a str,
        operation: Operation,
    },
    Import {
        model: &'a str,
        artifact_id: &'a str,
        profile_id: &'a str,
        operation: Operation,
    },
    /// The peer pushes a model here.
    Install,
    /// This node pushes a model to the peer, which decides whether to take it.
    Push,
}

pub struct Store {
    path: PathBuf,
    lock: File,
    snapshot: Mutex<Snapshot>,
    changed: watch::Sender<u64>,
    /// A directory-sync error after rename is ambiguous. Stop all authorization
    /// until restart/reconciliation instead of claiming the update succeeded.
    healthy: std::sync::atomic::AtomicBool,
}

impl Store {
    /// Administrative action, never an automatic fallback from `open`.
    pub fn initialize(path: &Path, server_id: &str) -> Result<Self, Error> {
        let lock = lock(path)?;
        let snapshot = Snapshot {
            format: "mdma.federation.trust.v1".into(),
            server_id: server_id.into(),
            generation: 1,
            clock_floor_ms: 0,
            peers: BTreeMap::new(),
            denied_tls: BTreeSet::new(),
            denied_signing: BTreeSet::new(),
            local_signing_history: BTreeMap::new(),
            retired_local_signing: BTreeSet::new(),
            disclosures: Vec::new(),
            installed: BTreeMap::new(),
        };
        snapshot.validate(server_id)?;
        let bytes = serde_json::to_vec(&snapshot)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_parent(path)?;
        Ok(Self::new(path, lock, snapshot))
    }

    pub fn open(path: &Path, server_id: &str) -> Result<Self, Error> {
        let lock = lock(path)?;
        Ok(Self::new(path, lock, Self::read(path, server_id)?))
    }

    /// The state as last written, without taking ownership; for display
    /// while its owner runs. Writes replace the file whole.
    pub fn read(path: &Path, server_id: &str) -> Result<Snapshot, Error> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(Error::Denied("trust state too large"));
        }
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        snapshot.validate(server_id)?;
        Ok(snapshot)
    }

    fn new(path: &Path, lock: File, snapshot: Snapshot) -> Self {
        let (changed, _) = watch::channel(snapshot.generation);
        Self {
            path: path.into(),
            lock,
            snapshot: Mutex::new(snapshot),
            changed,
            healthy: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Changes are invalidation notifications, not transferable authorization.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        self.ensure_healthy()?;
        Ok(self
            .snapshot
            .lock()
            .map_err(|_| Error::Denied("trust lock poisoned"))?
            .clone())
    }

    /// Administrative approval of local metadata key material and its window.
    /// Selection remains an explicit key ID/private key in the service config.
    pub fn approve_local_signer(&self, key: config::SigningKey) -> Result<(), Error> {
        key.bytes()?;
        key.window.validate()?;
        super::signing::validate_key_id(&key.key_id)
            .map_err(|_| Error::Denied("invalid local signer ID"))?;
        self.update(|snapshot| {
            if snapshot.signing_denied(&key.public_key)
                || snapshot.retired_local_signing.contains(&key.key_id)
            {
                return Err(Error::Denied("local signer revoked or retired"));
            }
            if snapshot
                .local_signing_history
                .get(&key.key_id)
                .is_some_and(|old| old.public_key != key.public_key)
            {
                return Err(Error::Denied("local signing ID cannot change material"));
            }
            snapshot.local_signing_history.insert(
                key.key_id,
                KeyHistory {
                    public_key: key.public_key,
                    window: key.window,
                },
            );
            Ok(())
        })
    }

    pub fn local_signer(&self, key_id: &str, now_ms: u64) -> Result<config::SigningKey, Error> {
        self.observe_time(now_ms)?;
        let snapshot = self.snapshot()?;
        if now_ms < snapshot.clock_floor_ms || snapshot.retired_local_signing.contains(key_id) {
            return Err(Error::Denied("local signer retired or invalid UTC clock"));
        }
        let key = snapshot
            .local_signing_history
            .get(key_id)
            .ok_or(Error::Denied("local signer not approved"))?;
        if snapshot.signing_denied(&key.public_key) || !key.window.contains(now_ms) {
            return Err(Error::Denied("local signer denied or outside its window"));
        }
        Ok(config::SigningKey {
            key_id: key_id.into(),
            public_key: key.public_key.clone(),
            window: key.window,
        })
    }

    fn ensure_healthy(&self) -> Result<(), Error> {
        if !self.healthy.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Denied("trust persistence requires reconciliation"));
        }
        Ok(())
    }

    fn update(&self, edit: impl FnOnce(&mut Snapshot) -> Result<(), Error>) -> Result<(), Error> {
        self.ensure_healthy()?;
        let mut current = self
            .snapshot
            .lock()
            .map_err(|_| Error::Denied("trust lock poisoned"))?;
        let mut next = current.clone();
        edit(&mut next)?;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Denied("trust generation exhausted"))?;
        next.validate(&current.server_id)?;
        let bytes = serde_json::to_vec(&next)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(Error::Denied("trust state too large"));
        }
        let parent = parent(&self.path);
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| Error::Io(e.error))?;
        if let Err(error) = sync_parent(&self.path) {
            self.healthy
                .store(false, std::sync::atomic::Ordering::Release);
            self.changed.send_replace(next.generation);
            return Err(error.into());
        }
        *current = next;
        self.changed.send_replace(current.generation);
        Ok(())
    }

    /// Independently verified material enters staged with *all* grants cleared.
    /// Historical IDs, revocations and execution tombstones are not enrollment.
    pub fn stage(&self, peer: Peer) -> Result<(), Error> {
        peer.validate()?;
        self.update(move |snapshot| {
            if snapshot
                .peers
                .get(&peer.server_id)
                .is_some_and(|p| p.state == State::Active)
            {
                return Err(Error::Denied("suspend an active peer before recovery"));
            }
            check_material(snapshot, &peer)?;
            snapshot
                .disclosures
                .retain(|grant| grant.peer_id != peer.server_id);
            let old = snapshot.peers.remove(&peer.server_id);
            let mut record = old.unwrap_or_else(|| Record {
                state: State::Staged,
                policy: peer.without_grants(),
                signing_history: BTreeMap::new(),
                retired_signing: BTreeSet::new(),
                retired_tls: BTreeSet::new(),
                tls_history: BTreeMap::new(),
            });
            remember_keys(&mut record, &peer)?;
            record.state = State::Staged;
            record.policy = peer.without_grants();
            snapshot.peers.insert(peer.server_id.clone(), record);
            Ok(())
        })
    }

    /// Staged probes are a separate explicit grant, never catalog/work access.
    pub fn allow_staged_identify(&self, id: &str) -> Result<(), Error> {
        self.update(|snapshot| {
            let record = snapshot
                .peers
                .get_mut(id)
                .ok_or(Error::Denied("unknown peer"))?;
            if record.state != State::Staged {
                return Err(Error::Denied("peer is not staged"));
            }
            record.policy.control_permissions.identify = true;
            Ok(())
        })
    }

    /// Safe initial activation: control grants only, empty exports/imports.
    pub fn activate_control(&self, id: &str, permissions: ControlPermissions) -> Result<(), Error> {
        self.update(|snapshot| {
            let record = snapshot
                .peers
                .get_mut(id)
                .ok_or(Error::Denied("unknown peer"))?;
            if record.state != State::Staged {
                return Err(Error::Denied("activation requires staged enrollment"));
            }
            record.state = State::Active;
            record.policy = record.policy.without_grants();
            record.policy.control_permissions = permissions;
            Ok(())
        })
    }

    /// Explicit reviewed policy publication, not a reload from a remote document.
    pub fn approve_policy(&self, peer: Peer) -> Result<(), Error> {
        peer.validate()?;
        self.update(move |snapshot| {
            check_material(snapshot, &peer)?;
            let record = snapshot
                .peers
                .get_mut(&peer.server_id)
                .ok_or(Error::Denied("unknown peer"))?;
            if record.state != State::Active {
                return Err(Error::Denied("policy requires active enrollment"));
            }
            remember_keys(record, &peer)?;
            record.policy = peer;
            Ok(())
        })
    }

    /// Records `model` as pushed by `installer`, which must still hold the
    /// install grant. Replaces an entry whose file is gone.
    pub fn record_install(
        &self,
        installer: &str,
        model: &str,
        installed: Installed,
    ) -> Result<(), Error> {
        self.update(|snapshot| {
            if !snapshot
                .peers
                .get(installer)
                .is_some_and(|r| r.state == State::Active && r.policy.install.is_some())
            {
                return Err(Error::Denied("installer lost its install grant"));
            }
            snapshot.installed.insert(model.into(), installed);
            Ok(())
        })
    }

    pub fn suspend(&self, id: &str) -> Result<(), Error> {
        self.disable(id, State::Suspended)
    }
    pub fn revoke(&self, id: &str) -> Result<(), Error> {
        self.disable(id, State::Revoked)
    }

    fn disable(&self, id: &str, state: State) -> Result<(), Error> {
        self.update(|snapshot| {
            snapshot.disclosures.retain(|grant| grant.peer_id != id);
            let record = snapshot
                .peers
                .get_mut(id)
                .ok_or(Error::Denied("unknown peer"))?;
            if record.state == State::Revoked && state != State::Revoked {
                return Err(Error::Denied("revoked peer requires new staged enrollment"));
            }
            record.state = state;
            record.policy = record.policy.without_grants();
            Ok(())
        })
    }

    /// Compromise: deny material under every identity/role and suspend all peers
    /// whose current policy uses it. Relabeling cannot bypass the denial.
    pub fn revoke_tls(&self, digest: &str) -> Result<(), Error> {
        config::validate_digest(digest)?;
        self.update(|snapshot| {
            snapshot.denied_tls.insert(digest.into());
            snapshot
                .disclosures
                .retain(|grant| grant.caller_spki_sha256 != digest);
            for record in snapshot.peers.values_mut() {
                if record
                    .policy
                    .tls_pins
                    .iter()
                    .any(|p| p.spki_sha256 == digest)
                {
                    if record.state != State::Revoked {
                        record.state = State::Suspended;
                    }
                    record.policy = record.policy.without_grants();
                }
            }
            Ok(())
        })
    }

    pub fn revoke_signing(&self, public_key: &str) -> Result<(), Error> {
        decode_key(public_key)?;
        self.update(|snapshot| {
            snapshot.denied_signing.insert(public_key.into());
            for record in snapshot.peers.values_mut() {
                if record
                    .signing_history
                    .values()
                    .any(|key| key.public_key == public_key)
                {
                    if record.state != State::Revoked {
                        record.state = State::Suspended;
                    }
                    record.policy = record.policy.without_grants();
                }
            }
            Ok(())
        })
    }

    /// Persist observed key retirement before exposing eligibility. Binding and
    /// catalog monotonic deadlines additionally prevent rollback extensions.
    pub fn observe_time(&self, now_ms: u64) -> Result<(), Error> {
        self.ensure_healthy()?;
        let current = self.snapshot()?;
        if now_ms < current.clock_floor_ms || now_ms > config::MAX_SAFE_TIMESTAMP {
            return Err(Error::Denied("invalid or rolled-back UTC clock"));
        }
        let needs_update = current
            .disclosures
            .iter()
            .any(|grant| now_ms >= grant.window.valid_until_ms)
            || current.local_signing_history.iter().any(|(id, key)| {
                now_ms >= key.window.valid_until_ms && !current.retired_local_signing.contains(id)
            })
            || current.peers.values().any(|r| {
                r.signing_history.iter().any(|(id, k)| {
                    now_ms >= k.window.valid_until_ms && !r.retired_signing.contains(id)
                }) || r.tls_history.values().any(|p| {
                    now_ms >= p.window.valid_until_ms && !r.retired_tls.contains(&pin_label(p))
                })
            });
        if !needs_update {
            return Ok(());
        }
        self.update(|snapshot| {
            if now_ms < snapshot.clock_floor_ms {
                return Err(Error::Denied("rolled-back UTC clock"));
            }
            snapshot.clock_floor_ms = now_ms;
            snapshot
                .disclosures
                .retain(|grant| now_ms < grant.window.valid_until_ms);
            for (id, key) in &snapshot.local_signing_history {
                if now_ms >= key.window.valid_until_ms {
                    snapshot.retired_local_signing.insert(id.clone());
                }
            }
            for record in snapshot.peers.values_mut() {
                for (id, key) in &record.signing_history {
                    if now_ms >= key.window.valid_until_ms {
                        record.retired_signing.insert(id.clone());
                    }
                }
                for pin in record.tls_history.values() {
                    if now_ms >= pin.window.valid_until_ms {
                        record.retired_tls.insert(pin_label(pin));
                    }
                }
            }
            Ok(())
        })
    }

    pub fn authorize(&self, id: &str, action: Action<'_>, now_ms: u64) -> Result<Snapshot, Error> {
        self.observe_time(now_ms)?;
        let snapshot = self.snapshot()?;
        if now_ms < snapshot.clock_floor_ms {
            return Err(Error::Denied("rolled-back UTC clock"));
        }
        let record = snapshot
            .peers
            .get(id)
            .ok_or(Error::Denied("unknown peer"))?;
        let allowed = match action {
            Action::Authenticate => matches!(record.state, State::Staged | State::Active),
            Action::Identify => {
                matches!(record.state, State::Staged | State::Active)
                    && record.policy.control_permissions.identify
            }
            Action::ScanCapabilities => {
                record.state == State::Active && record.policy.control_permissions.scan_capabilities
            }
            Action::PollCatalog | Action::Push => record.state == State::Active,
            Action::Execute { model, operation } => {
                record.state == State::Active
                    && (record
                        .policy
                        .execute_exports
                        .iter()
                        .any(|e| e.model == model && e.operation == operation)
                        || record.policy.installed_exports
                            && snapshot
                                .installed
                                .get(model)
                                .is_some_and(|m| m.operation == operation))
            }
            Action::Install => record.state == State::Active && record.policy.install.is_some(),
            Action::Import {
                model,
                artifact_id,
                profile_id,
                operation,
            } => {
                record.state == State::Active
                    && record.policy.route_imports.iter().any(|i| {
                        i.model == model
                            && i.artifact_id == artifact_id
                            && i.profile_id == profile_id
                            && i.operation == operation
                    })
            }
        };
        if !allowed {
            return Err(Error::Denied("current state or grant denies RPC"));
        }
        Ok(snapshot)
    }

    pub fn signing_key(
        &self,
        id: &str,
        key_id: &str,
        now_ms: u64,
    ) -> Result<config::SigningKey, Error> {
        self.observe_time(now_ms)?;
        let snapshot = self.snapshot()?;
        if now_ms < snapshot.clock_floor_ms {
            return Err(Error::Denied("rolled-back UTC clock"));
        }
        let record = snapshot
            .peers
            .get(id)
            .ok_or(Error::Denied("unknown peer"))?;
        if record.state != State::Active || record.retired_signing.contains(key_id) {
            return Err(Error::Denied("signing key not eligible"));
        }
        let key = record
            .policy
            .signing_keys
            .iter()
            .find(|k| k.key_id == key_id)
            .ok_or(Error::Denied("unapproved signing key"))?;
        if snapshot.signing_denied(&key.public_key) || !key.window.contains(now_ms) {
            return Err(Error::Denied("signing key denied or outside its window"));
        }
        Ok(key.clone())
    }
}

fn remember_keys(record: &mut Record, peer: &Peer) -> Result<(), Error> {
    if peer
        .tls_pins
        .iter()
        .any(|p| record.retired_tls.contains(&pin_label(p)))
    {
        return Err(Error::Denied(
            "retired TLS pin window cannot be reactivated",
        ));
    }
    for pin in &peer.tls_pins {
        record.tls_history.insert(pin_label(pin), pin.clone());
    }
    for key in &peer.signing_keys {
        if record.retired_signing.contains(&key.key_id) {
            return Err(Error::Denied(
                "retired signing key ID cannot be reactivated",
            ));
        }
        if let Some(previous) = record.signing_history.get(&key.key_id)
            && previous.public_key != key.public_key
        {
            return Err(Error::Denied("signing key ID cannot change material"));
        }
        record.signing_history.insert(
            key.key_id.clone(),
            KeyHistory {
                public_key: key.public_key.clone(),
                window: key.window,
            },
        );
    }
    Ok(())
}

fn check_material(snapshot: &Snapshot, peer: &Peer) -> Result<(), Error> {
    if peer
        .tls_pins
        .iter()
        .any(|p| snapshot.tls_denied(&p.spki_sha256))
        || peer
            .signing_keys
            .iter()
            .any(|k| snapshot.signing_denied(&k.public_key))
    {
        return Err(Error::Denied("revoked material cannot be reenrolled"));
    }
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    File::open(parent(path))?.sync_all()
}

fn lock(path: &Path) -> Result<File, Error> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(name))?;
    file.try_lock()
        .map_err(|_| Error::Denied("trust state already has an owner"))?;
    Ok(file)
}

impl Drop for Store {
    fn drop(&mut self) {
        // Closing the descriptor releases the lock even if this explicit unlock
        // fails, and Drop has no caller to report the error to.
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
mod tests;
