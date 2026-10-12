//! Node administration behind `mdma-fed`: identity, peer bundles, grants and
//! routes. Changes go through the trust store's locked APIs, so they fail
//! while `mdma-serve` owns the store.
use super::{
    catalog::{self, ModelDescriptor},
    catalog_cache::Cache,
    config::{
        self, CaRoot, ControlPermissions, ExecuteExport, Operation, RouteImport, SigningKey,
        TlsPin, TlsRole, Window,
    },
    routing::Route,
    runtime::{
        Config,
        outbound::{self, Credentials},
    },
    signing::KeyPairHandle,
    tls,
    trust::{Disclosure, State, Store},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::error::BoxError;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Write as _,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

const RUNTIME: &str = "runtime.toml";
const BUNDLE_FORMAT: &str = "mdma.federation.bundle.v1";
const DAY_MS: u64 = 86_400_000;
/// Longest credential window `init` issues.
pub const MAX_DAYS: u32 = 397;

/// What a node hands a peer to be enrolled: public pins and its root, no
/// grants and no private keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub format: String,
    pub peer: config::Peer,
    pub root_pem: String,
}

/// A peer whose federation listener could not be dialed: down, filtered, or
/// refused by the OS, such as macOS Local Network privacy.
#[derive(Debug)]
pub struct Unreachable {
    pub peer: String,
    cause: BoxError,
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot connect to {}: {}", self.peer, self.cause)
    }
}

impl std::error::Error for Unreachable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.cause)
    }
}

/// What an enrolled peer may see and run here.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    pub catalog: Vec<String>,
    pub execute: Vec<ExecuteExport>,
}

/// Writes a new owner-only file; never replaces one.
fn secret(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), BoxError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes.as_ref())?;
    file.sync_all()?;
    Ok(())
}

fn private_dir(path: &Path) -> Result<(), BoxError> {
    std::fs::create_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validity(params: &mut CertificateParams, window: Window) -> Result<(), BoxError> {
    let at = |ms: u64| -> Result<_, BoxError> {
        Ok(x509_parser::time::ASN1Time::from_timestamp(i64::try_from(ms / 1000)?)?.to_datetime())
    };
    params.not_before = at(window.valid_from_ms)?;
    params.not_after = at(window.valid_until_ms)?;
    Ok(())
}

/// The node's own federation root; its key signs one leaf and is dropped.
fn root(server_id: &str, window: Window) -> Result<CertifiedIssuer<'static, KeyPair>, BoxError> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let mut name = DistinguishedName::new();
    name.push(
        DnType::CommonName,
        format!("mdma federation root for {server_id}"),
    );
    params.distinguished_name = name;
    validity(&mut params, window)?;
    Ok(CertifiedIssuer::self_signed(params, KeyPair::generate()?)?)
}

/// The identity leaf: the server id as DNS name and identity URI, usable for
/// both TLS roles. Returns (certificate PEM, key PEM).
pub(crate) fn leaf(
    server_id: &str,
    issuer: &CertifiedIssuer<'_, KeyPair>,
    window: Window,
) -> Result<(String, String), BoxError> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![server_id.to_owned()])?;
    params.subject_alt_names.push(SanType::URI(
        format!("https://{server_id}/.well-known/mdma/identity").try_into()?,
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    validity(&mut params, window)?;
    let cert = params.signed_by(&key, issuer)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn load(dir: &Path) -> Result<(PathBuf, Config), BoxError> {
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let config = Config::load(&dir.join(RUNTIME))
        .map_err(|e| format!("{}: {e}", dir.join(RUNTIME).display()))?;
    Ok((dir, config))
}

fn save(dir: &Path, config: &Config) -> Result<(), BoxError> {
    let path = dir.join(RUNTIME);
    let next = dir.join(format!("{RUNTIME}.next"));
    // A leftover from an interrupted save is ours to replace.
    match std::fs::remove_file(&next) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    secret(&next, toml::to_string(config)?)?;
    std::fs::rename(&next, &path)?;
    Ok(())
}

fn chain(config: &Config) -> Result<Vec<CertificateDer<'static>>, BoxError> {
    Ok(CertificateDer::pem_file_iter(&config.tls_cert)?.collect::<Result<Vec<_>, _>>()?)
}

/// When the peer's TLS pins run out.
fn pins_until(peer: &config::Peer) -> Option<u64> {
    peer.tls_pins
        .iter()
        .map(|pin| pin.window.valid_until_ms)
        .min()
}

fn open_store(config: &Config) -> Result<Store, BoxError> {
    Store::open(&config.trust_state, &config.server_id).map_err(|error| {
        if matches!(error, super::trust::Error::Denied(reason) if reason.contains("owner")) {
            format!(
                "{} is in use; stop mdma-serve first",
                config.trust_state.display()
            )
            .into()
        } else {
            BoxError::from(error)
        }
    })
}

/// A key id unique to the key, so a new identity never reuses an old id.
fn key_id(signer: &KeyPairHandle) -> String {
    format!("ed25519:{}", &hex::encode(signer.public_key_bytes())[..16])
}

/// Creates `dir` with a new identity: root and leaf certificates, catalog
/// signer, trust store and runtime.toml, all valid for `days`. Peers are
/// added with [`enroll`].
pub fn init(dir: &Path, server_id: &str, endpoint: &str, days: u32) -> Result<Config, BoxError> {
    super::server_id::validate(server_id)?;
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(format!("--days must be 1..={MAX_DAYS}").into());
    }
    let uri: rama::net::uri::Uri = endpoint
        .parse()
        .map_err(|e| format!("endpoint {endpoint:?}: {e}"))?;
    let port = uri
        .port_u16()
        .ok_or("endpoint needs an explicit port, e.g. https://192.168.1.10:6365")?;
    private_dir(dir)?;
    let dir = dir.canonicalize()?;
    let now = tls::utc_ms()?;
    let window = Window {
        valid_from_ms: now - 60_000,
        valid_until_ms: now + u64::from(days) * DAY_MS,
    };
    let issuer = root(server_id, window)?;
    let (cert, key) = leaf(server_id, &issuer, window)?;
    let signer = KeyPairHandle::generate()?;
    let signing = SigningKey {
        key_id: key_id(&signer),
        public_key: URL_SAFE_NO_PAD.encode(signer.public_key_bytes()),
        window,
    };
    secret(&dir.join("root.pem"), issuer.pem())?;
    secret(&dir.join("tls.pem"), cert)?;
    secret(&dir.join("tls-key.pem"), key)?;
    secret(&dir.join("catalog.pk8"), signer.to_pkcs8()?)?;
    let store = Store::initialize(&dir.join("trust.json"), server_id)?;
    store.approve_local_signer(signing.clone())?;
    let config = Config {
        server_id: server_id.into(),
        listen: SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
        trust_state: dir.join("trust.json"),
        tls_name: server_id.into(),
        tls_cert: dir.join("tls.pem"),
        tls_key: dir.join("tls-key.pem"),
        identity_roots: vec![CaRoot {
            path: dir.join("root.pem"),
            certificate_sha256: tls::sha256(issuer.der()),
        }],
        signer_id: signing.key_id,
        signer_key: dir.join("catalog.pk8"),
        endpoint: Some(endpoint.into()),
        poll_catalogs: false,
        enable_execution: false,
        enable_routing: false,
        routes: vec![],
    };
    config.local_policy().validate()?;
    secret(&dir.join(RUNTIME), toml::to_string(&config)?)?;
    Ok(config)
}

/// This node's bundle for its peers to enroll.
pub fn export(dir: &Path) -> Result<Bundle, BoxError> {
    let (_, config) = load(dir)?;
    let endpoint = config
        .endpoint
        .clone()
        .ok_or("runtime.toml has no endpoint for peers to dial")?;
    let store = open_store(&config)?;
    let signing = store.local_signer(&config.signer_id, tls::utc_ms()?)?;
    let spki =
        tls::certificate_spki_sha256(chain(&config)?.first().ok_or("empty certificate chain")?)?;
    let [own] = config.identity_roots.as_slice() else {
        return Err("runtime.toml must name exactly one identity root".into());
    };
    let root_pem = std::fs::read_to_string(&own.path)?;
    let peer = config::Peer {
        server_id: config.server_id,
        endpoint,
        tls_name: config.tls_name,
        ca_roots: vec![CaRoot {
            path: "root.pem".into(),
            certificate_sha256: own.certificate_sha256.clone(),
        }],
        tls_pins: [TlsRole::ClientAuth, TlsRole::ServerAuth]
            .map(|role| TlsPin {
                role,
                spki_sha256: spki.clone(),
                window: signing.window,
            })
            .into(),
        signing_keys: vec![signing],
        control_permissions: ControlPermissions::default(),
        catalog_exports: vec![],
        execute_exports: vec![],
        route_imports: vec![],
        limits: config::Limits::default(),
    };
    peer.validate()?;
    Ok(Bundle {
        format: BUNDLE_FORMAT.into(),
        peer,
        root_pem,
    })
}

fn same_credentials(a: &config::Peer, b: &config::Peer) -> bool {
    a.tls_name == b.tls_name
        && a.tls_pins == b.tls_pins
        && a.signing_keys == b.signing_keys
        && a.ca_roots
            .iter()
            .map(|r| &r.certificate_sha256)
            .eq(b.ca_roots.iter().map(|r| &r.certificate_sha256))
}

/// Trusts the peer in `bundle` for control and gives it `grants`, dialing it
/// at `endpoint` instead of its own when given. Enrolling the same bundle
/// again replaces the grants; other credentials for a known peer are refused.
pub fn enroll(
    dir: &Path,
    bundle: &Bundle,
    endpoint: Option<&str>,
    grants: Grants,
) -> Result<config::Peer, BoxError> {
    let (dir, mut config) = load(dir)?;
    if bundle.format != BUNDLE_FORMAT {
        return Err(format!("not a {BUNDLE_FORMAT} bundle").into());
    }
    let mut peer = bundle.peer.clone();
    if peer.server_id == config.server_id {
        return Err("a node cannot enroll itself".into());
    }
    if peer != peer.without_grants() || !peer.route_imports.is_empty() {
        return Err("bundles carry no grants".into());
    }
    let [root] = peer.ca_roots.as_mut_slice() else {
        return Err("a bundle names exactly one root".into());
    };
    let der = CertificateDer::from_pem_slice(bundle.root_pem.as_bytes())
        .map_err(|e| format!("bundle root: {e}"))?;
    if tls::sha256(&der) != root.certificate_sha256 {
        return Err("bundle root does not match its pin".into());
    }
    let roots = dir.join("peers");
    if !roots.exists() {
        private_dir(&roots)?;
    }
    root.path = roots.join(format!("{}.pem", peer.server_id));
    match std::fs::read(&root.path) {
        Ok(on_file) if on_file != bundle.root_pem.as_bytes() => {
            return Err(format!(
                "{} holds another root for {}",
                root.path.display(),
                peer.server_id
            )
            .into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            secret(&root.path, &bundle.root_pem)?;
        }
        Err(error) => return Err(error.into()),
    }
    if let Some(endpoint) = endpoint {
        peer.endpoint = endpoint.into();
    }
    peer.validate()?;
    let store = open_store(&config)?;
    match store.snapshot()?.peer(&peer.server_id) {
        None => {
            store.stage(peer.clone())?;
            store.activate_control(
                &peer.server_id,
                ControlPermissions {
                    identify: true,
                    scan_capabilities: true,
                },
            )?;
        }
        Some(record) if !same_credentials(&record.policy, &peer) => {
            return Err(format!("{} is enrolled with other credentials", peer.server_id).into());
        }
        Some(record) if record.state != State::Active => {
            return Err(format!(
                "{} is {:?}; it cannot be granted anything",
                peer.server_id, record.state
            )
            .into());
        }
        Some(record) => peer.route_imports.clone_from(&record.policy.route_imports),
    }
    peer.control_permissions = ControlPermissions {
        identify: true,
        scan_capabilities: true,
    };
    peer.catalog_exports = grants.catalog;
    peer.execute_exports = grants.execute;
    store.approve_policy(peer.clone())?;
    if !peer.execute_exports.is_empty() && !config.enable_execution {
        config.enable_execution = true;
        save(&dir, &config)?;
    }
    Ok(peer)
}

/// Routes every ready model in `peer`'s signed catalog as `prefix-model`,
/// for the local client whose certificate is `client_cert`. Replaces the
/// peer's earlier routes.
pub async fn routes(
    dir: &Path,
    peer: &str,
    client_cert: &Path,
    prefix: &str,
) -> Result<Vec<Route>, BoxError> {
    let (dir, config) = load(dir)?;
    let caller = tls::certificate_spki_sha256(
        &CertificateDer::from_pem_file(client_cert)
            .map_err(|e| format!("{}: {e}", client_cert.display()))?,
    )?;
    let store = Arc::new(open_store(&config)?);
    let now = tls::utc_ms()?;
    let signing_key = store.local_signer(&config.signer_id, now)?.bytes()?;
    let chain = chain(&config)?;
    let credentials = Credentials {
        store: store.clone(),
        key: Arc::new(PrivateKeyDer::from_pem_file(&config.tls_key)?),
        local_expires_ms: tls::local_client_expiry(
            &chain,
            &config.local_policy(),
            &signing_key,
            now,
        )?,
        chain,
        signing_key,
    };
    let catalog = credentials
        .poll(&Cache::default(), peer, rama::rt::Executor::default())
        .await
        .map_err(|cause| {
            if outbound::connect_failed(&cause) {
                BoxError::from(Unreachable {
                    peer: peer.to_owned(),
                    cause,
                })
            } else {
                format!("{peer}'s catalog: {cause}").into()
            }
        })?;
    if catalog.server_state.forces_unready() {
        return Err(format!("{peer} is not accepting work").into());
    }
    apply_routes(&dir, config, &store, peer, catalog.models, &caller, prefix)
}

pub(crate) fn apply_routes(
    dir: &Path,
    mut config: Config,
    store: &Store,
    peer_id: &str,
    models: Vec<ModelDescriptor>,
    caller: &str,
    prefix: &str,
) -> Result<Vec<Route>, BoxError> {
    let routes: Vec<Route> = models
        .into_iter()
        .filter(|model| model.ready)
        .map(|model| Route {
            alias: format!("{prefix}-{}", model.model),
            peer_id: peer_id.into(),
            operation: match model.kind {
                catalog::Kind::Gguf => Operation::Synthesize,
                catalog::Kind::Laya => Operation::Decide,
            },
            model: model.model,
            artifact_id: model.artifact_id,
            profile_id: model.profile_id,
        })
        .collect();
    if routes.is_empty() {
        return Err(format!(
            "{peer_id} offers no ready models; has it granted this node its catalog?"
        )
        .into());
    }
    for route in &routes {
        route.validate()?;
        if config
            .routes
            .iter()
            .any(|old| old.alias == route.alias && old.peer_id != peer_id)
        {
            return Err(format!(
                "alias {} already routes to another peer; pick another --prefix",
                route.alias
            )
            .into());
        }
    }
    let mut policy = store
        .snapshot()?
        .peer(peer_id)
        .ok_or_else(|| format!("{peer_id} is not enrolled"))?
        .policy
        .clone();
    let until = pins_until(&policy).ok_or("peer has no TLS pins")?;
    policy.route_imports = routes
        .iter()
        .map(|route| RouteImport {
            model: route.model.clone(),
            artifact_id: route.artifact_id.clone(),
            profile_id: route.profile_id.clone(),
            operation: route.operation,
        })
        .collect();
    store.approve_policy(policy)?;
    for old in config.routes.iter().filter(|old| old.peer_id == peer_id) {
        store.remove_disclosure(caller, peer_id, &old.model, old.operation)?;
    }
    let now = tls::utc_ms()?;
    for route in &routes {
        store.approve_disclosure(Disclosure {
            caller_spki_sha256: caller.into(),
            peer_id: peer_id.into(),
            model: route.model.clone(),
            operation: route.operation,
            window: Window {
                valid_from_ms: now,
                valid_until_ms: until,
            },
        })?;
    }
    config.routes.retain(|old| old.peer_id != peer_id);
    config.routes.extend(routes.iter().cloned());
    config.enable_routing = true;
    config.poll_catalogs = true;
    save(dir, &config)?;
    Ok(routes)
}

fn remaining(until_ms: u64, now_ms: u64) -> String {
    match until_ms.checked_sub(now_ms) {
        Some(left) if left > 0 => format!("{}d left", left / DAY_MS),
        _ => "expired".into(),
    }
}

fn operation(operation: Operation) -> &'static str {
    match operation {
        Operation::Synthesize => "synthesize",
        Operation::Decide => "decide",
    }
}

/// Identity, peers, grants and routes, readable while `mdma-serve` runs.
pub fn status(dir: &Path) -> Result<String, BoxError> {
    let (_, config) = load(dir)?;
    let snapshot = Store::read(&config.trust_state, &config.server_id)?;
    let now = tls::utc_ms()?;
    let flag = |on: bool| if on { "on" } else { "off" };
    let mut out = String::new();
    writeln!(
        out,
        "node     {} at {}",
        config.server_id,
        config.endpoint.as_deref().unwrap_or("(no endpoint)")
    )?;
    writeln!(
        out,
        "listen   {}  execution {}  routing {}  polling {}",
        config.listen,
        flag(config.enable_execution),
        flag(config.enable_routing),
        flag(config.poll_catalogs)
    )?;
    for record in snapshot.records() {
        let policy = &record.policy;
        let until = pins_until(policy).unwrap_or(0);
        writeln!(
            out,
            "peer     {} {:?} at {} ({})",
            policy.server_id,
            record.state,
            policy.endpoint,
            remaining(until, now)
        )?;
        if !policy.catalog_exports.is_empty() {
            writeln!(out, "  sees     {}", policy.catalog_exports.join(", "))?;
        }
        if !policy.execute_exports.is_empty() {
            let runs: Vec<_> = policy
                .execute_exports
                .iter()
                .map(|e| format!("{} ({})", e.model, operation(e.operation)))
                .collect();
            writeln!(out, "  runs     {}", runs.join(", "))?;
        }
    }
    for route in &config.routes {
        let callers = snapshot
            .disclosures()
            .iter()
            .filter(|d| {
                d.peer_id == route.peer_id
                    && d.model == route.model
                    && d.operation == route.operation
                    && d.window.contains(now)
            })
            .count();
        writeln!(
            out,
            "route    {} -> {} {} ({}, {callers} client(s))",
            route.alias,
            route.peer_id,
            route.model,
            operation(route.operation)
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
