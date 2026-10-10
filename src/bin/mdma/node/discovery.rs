//! DNS-SD supplies endpoints, never trust or resource capacity.
use super::{query, proto, version};
use mdns_sd::{IfKind, Receiver, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use rama::error::BoxError;
use rama::futures::StreamExt;
use rama::futures::stream::FuturesUnordered;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::{Duration, Instant};
use std::io::{IsTerminal, Write};

#[path = "discovery/table.rs"]
mod table;

const SERVICE_TYPE: &str = "_mdma._tcp.local.";
const PROTOCOL: &str = version::TXT_PROTOCOL;
const MAX_PEERS: usize = 128;
const MAX_IN_FLIGHT: usize = 8;
const POLL: Duration = Duration::from_secs(10);

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    /// Browse for this many seconds; 1..3600 (ignored with --watch).
    #[usage(long, default = "5")]
    seconds: u64,
    /// Keep browsing and refreshing until SIGINT or SIGTERM.
    #[usage(long)]
    watch: bool,
    /// Human-readable live table; default output remains newline-delimited JSON.
    #[usage(long)]
    table: bool,
    #[usage(long)]
    min_peer_version: Option<String>,
    #[usage(long)]
    require_peer_build: Option<String>,
}

pub(super) fn usable_ip(ip: IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(ip) => !ip.is_broadcast(),
        // Scoped HTTP origins are not supported by this POC transport.
        IpAddr::V6(ip) => !ip.is_unicast_link_local(),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Advertisement {
    name: String,
    instance: String,
    endpoints: Vec<String>,
    identity: Option<proto::NodeIdentity>,
    rejection: Option<String>,
}

fn advertisement(info: &ResolvedService, own_instance: Option<&str>, policy: &version::Policy) -> Option<Advertisement> {
    if !info.get_property_val_str("poc").is_some_and(|poc| poc.starts_with("resources-"))
        || info.get_port() == 0
        || own_instance.is_some_and(|own| info.get_property_val_str("instance") == Some(own))
    {
        return None;
    }
    let mut addresses: Vec<_> = info
        .get_addresses()
        .iter()
        .map(mdns_sd::ScopedIp::to_ip_addr)
        .filter(|ip| usable_ip(*ip))
        .collect();
    addresses.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addresses.dedup();
    let endpoints = addresses
        .into_iter()
        .take(4)
        .map(|ip| format!("http://{}", SocketAddr::new(ip, info.get_port())))
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        return None;
    }
    let identity = Some(proto::NodeIdentity {
        mdma_version: info.get_property_val_str("version").unwrap_or("").into(),
        build_id: info.get_property_val_str("build").unwrap_or("").into(),
        protocol_version: info.get_property_val_str("protocol").and_then(|v| v.parse().ok()).unwrap_or(0),
    });
    let rejection = if info.get_property_val_str("poc") == Some(PROTOCOL) { policy.check(identity.as_ref()).err() } else {
        Some(format!("legacy/incompatible advertisement {}; require {PROTOCOL}",
            info.get_property_val_str("poc").unwrap_or("missing")))
    };
    Some(Advertisement {
        name: info
            .get_property_val_str("node")
            .unwrap_or_else(|| info.get_hostname())
            .to_owned(),
        instance: info.get_property_val_str("instance")?.to_owned(),
        endpoints,
        identity,
        rejection,
    })
}

#[expect(clippy::struct_excessive_bools, reason = "independent per-peer discovery and polling flags")]
struct Peer {
    advertisement: Advertisement,
    present: bool,
    generation: u64,
    in_flight: bool,
    next_poll: Instant,
    last_success: Option<Instant>,
    resources: Option<Value>,
    error: Option<String>,
    removed_at: Option<Instant>,
    awaiting_snapshot: bool,
    rejected: bool,
    identity: Option<Value>,
}

impl Peer {
    fn new(advertisement: Advertisement, now: Instant) -> Self {
        let rejected = advertisement.rejection.is_some();
        let error = advertisement.rejection.clone();
        let identity = advertisement.identity.as_ref().map(version::json_identity);
        Self {
            advertisement,
            present: true,
            generation: 0,
            in_flight: false,
            next_poll: now,
            last_success: None,
            resources: None,
            error,
            removed_at: None,
            awaiting_snapshot: true,
            rejected,
            identity,
        }
    }

    fn removed(&mut self, now: Instant) {
        self.present = false;
        self.generation += 1; // Reject results from an RPC started before withdrawal.
        self.in_flight = false;
        self.error = Some("mDNS service removed or expired".into());
        self.removed_at = Some(now);
    }

    fn resolved(&mut self, advertisement: Advertisement, now: Instant) {
        if !self.present || self.advertisement != advertisement {
            self.generation += 1;
            self.in_flight = false;
            self.next_poll = now;
            self.rejected = advertisement.rejection.is_some();
            self.identity = advertisement.identity.as_ref().map(version::json_identity);
            self.error.clone_from(&advertisement.rejection);
            self.advertisement = advertisement;
            self.awaiting_snapshot = true;
        }
        self.present = true;
        self.removed_at = None;
    }

    fn complete(&mut self, generation: u64, result: Result<Value, version::QueryError>, now: Instant) -> bool {
        if generation != self.generation || !self.present {
            return false;
        }
        self.in_flight = false;
        self.awaiting_snapshot = false;
        self.next_poll = now + POLL;
        match result {
            Ok(resources) => {
                self.rejected = false;
                if let Some(identity) = resources.get("identity") { self.identity = Some(identity.clone()); }
                self.resources = Some(resources);
                self.last_success = Some(now);
                self.error = None;
            }
            Err(error) => {
                self.rejected = matches!(error, version::QueryError::Rejected { .. });
                if let version::QueryError::Rejected { identity: Some(identity), .. } = &error {
                    self.identity = Some(identity.clone());
                }
                self.error = Some(error.to_string());
            },
        }
        true
    }

    fn report(&self, service: &str, now: Instant) -> Value {
        let status = if !self.present {
            "offline"
        } else if self.rejected {
            "rejected"
        } else if self.awaiting_snapshot {
            "discovered"
        } else if self.error.is_some() {
            "unreachable"
        } else if self.last_success.is_some() {
            "online"
        } else {
            "discovered"
        };
        json!({
            "service": service, "node_name": self.advertisement.name,
            "endpoints": self.advertisement.endpoints, "status": status,
            "last_success_age_ms": self.last_success.map(|at| now.saturating_duration_since(at).as_millis()),
            "resources": self.resources, "error": self.error,
            "identity": self.identity,
        })
    }
}

pub(super) struct Discovery {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
    own_instance: Option<String>,
    service_name: Option<String>,
    closed: bool,
    policy: version::Policy,
}

impl Discovery {
    /// A read-only browser passes None; a daemon advertises its bound address.
    pub(super) fn start(address: Option<SocketAddr>, policy: version::Policy) -> Result<Self, BoxError> {
        let daemon = ServiceDaemon::new()?;
        // Construct the guard before fallible configuration to avoid leaked threads.
        let events = match daemon.browse(SERVICE_TYPE) {
            Ok(events) => events,
            Err(error) => {
                let _ = daemon.shutdown();
                return Err(error.into());
            }
        };
        let mut discovery = Self {
            daemon,
            events,
            own_instance: None,
            service_name: None,
            closed: false,
            policy,
        };
        discovery
            .daemon
            .disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;
        if let Some(address) = address {
            discovery.daemon.disable_interface(IfKind::All)?;
            discovery
                .daemon
                .enable_interface(IfKind::Addr(address.ip()))?;
            let instance = hex::encode(rand::random::<[u8; 16]>());
            let node = sysinfo::System::host_name().ok_or("cannot determine hostname")?;
            let label: String = node
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(30)
                .collect();
            // Stable service instance across restart; boot token is separate
            // so a changed process invalidates outstanding resource queries.
            let name = format!("mdma-{label}");
            // Separate from the OS Bonjour hostname, avoiding conflicting A/AAAA records.
            let host = format!("mdma-{instance}.local.");
            let local = version::local();
            let protocol = local.protocol_version.to_string();
            let service = ServiceInfo::new(
                SERVICE_TYPE,
                &name,
                &host,
                address.ip(),
                address.port(),
                &[
                    ("poc", PROTOCOL),
                    ("node", label.as_str()),
                    ("instance", instance.as_str()),
                    ("version", local.mdma_version.as_str()),
                    ("build", local.build_id.as_str()),
                    ("protocol", protocol.as_str()),
                ][..],
            )?;
            let service_name: String = service.get_fullname().into();
            discovery.service_name = Some(service_name.clone());
            discovery.own_instance = Some(instance);
            discovery.daemon.register(service)?;
            eprintln!("mDNS advertising {service_name} on {address}");
        }
        Ok(discovery)
    }

    pub(super) async fn close(&mut self) {
        if self.closed {
            return;
        }
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        if let Some(name) = &self.service_name
            && let Ok(done) = self.daemon.unregister(name)
        {
            let _ = tokio::time::timeout(Duration::from_secs(1), done.recv_async()).await;
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = tokio::time::timeout(Duration::from_secs(1), done.recv_async()).await;
        }
        self.closed = true;
    }

    pub(super) async fn watch(&self, duration: Option<Duration>) -> Result<(), BoxError> {
        self.watch_output(duration, false).await
    }

    async fn watch_output(&self, duration: Option<Duration>, as_table: bool) -> Result<(), BoxError> {
        type Query = Pin<Box<dyn Future<Output = (String, u64, Result<Value, version::QueryError>)> + Send>>;
        let mut peers: BTreeMap<String, Peer> = BTreeMap::new();
        let mut pending: FuturesUnordered<Query> = FuturesUnordered::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let terminal = std::io::stdout().is_terminal();
        let mut last_render = Instant::now();
        let draw = |peers: &BTreeMap<String, Peer>| -> Result<(), BoxError> {
            let now = Instant::now();
            let reports: Vec<_> = peers.iter().map(|(key, peer)| peer.report(key, now)).collect();
            let mut stdout = std::io::stdout().lock();
            if terminal { write!(stdout, "\x1b[2J\x1b[H")?; }
            write!(stdout, "{}", table::render(&reports))?;
            stdout.flush()?;
            Ok(())
        };
        if as_table && terminal { draw(&peers)?; }
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let deadline = async {
            match duration {
                Some(duration) => tokio::time::sleep(duration).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                event = self.events.recv_async() => {
                    match event? {
                        ServiceEvent::ServiceResolved(info) => {
                            if let Some(advertisement) = advertisement(&info, self.own_instance.as_deref(), &self.policy) {
                                let key = info.get_fullname().to_lowercase();
                                let now = Instant::now();
                                if let Some(peer) = peers.get_mut(&key) {
                                    peer.resolved(advertisement, now);
                                } else if peers.len() < MAX_PEERS {
                                    let peer = Peer::new(advertisement, now);
                                    if !as_table { println!("{}", peer.report(&key, now)); }
                                    peers.insert(key, peer);
                                }
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, name) => {
                            let key = name.to_lowercase();
                            if let Some(peer) = peers.get_mut(&key) {
                                peer.removed(Instant::now());
                                if !as_table { println!("{}", peer.report(&key, Instant::now())); }
                            }
                        }
                        _ => {},
                    }
                }
                Some((key, generation, result)) = pending.next(), if !pending.is_empty() => {
                    if let Some(peer) = peers.get_mut(&key) {
                        let now = Instant::now();
                        if peer.complete(generation, result, now) && !as_table {
                            println!("{}", peer.report(&key, now));
                        }
                    }
                }
                _ = tick.tick() => {
                    if as_table && (terminal || duration.is_none()
                        && last_render.elapsed() >= POLL) {
                        draw(&peers)?;
                        last_render = Instant::now();
                    }
                },
                () = &mut deadline => {
                    if as_table { draw(&peers)?; }
                    else { println!("{}", json!({"event": "discovery_complete", "peers": peers.len()})); }
                    return Ok(());
                }
            }
            let now = Instant::now();
            peers.retain(|_, peer| {
                peer.removed_at
                    .is_none_or(|at| now.saturating_duration_since(at) < Duration::from_secs(60))
            });
            for (key, peer) in &mut peers {
                if pending.len() >= MAX_IN_FLIGHT {
                    break;
                }
                if peer.present && !peer.rejected && !peer.in_flight && now >= peer.next_poll {
                    peer.in_flight = true;
                    let endpoints = peer.advertisement.endpoints.clone();
                    let key = key.clone();
                    let generation = peer.generation;
                    let policy = self.policy.clone();
                    let expected = peer.advertisement.identity.as_ref().map(version::json_identity);
                    pending.push(Box::pin(async move {
                        let result = tokio::time::timeout(Duration::from_secs(5), async {
                            let mut last_error = version::QueryError::Unreachable("no usable endpoints".into());
                            for endpoint in endpoints {
                                match query(&endpoint, &policy).await {
                                    Ok(resources) => {
                                        if expected.as_ref() != resources.get("identity") {
                                            return Err(version::QueryError::Rejected {
                                                reason: "advertisement/RPC identity mismatch".into(),
                                                identity: resources.get("identity").cloned(),
                                            });
                                        }
                                        return Ok(resources);
                                    },
                                    Err(error @ version::QueryError::Rejected { .. }) => return Err(error),
                                    Err(error) => last_error = error,
                                }
                            }
                            Err(last_error)
                        })
                        .await
                        .unwrap_or_else(|_| Err("resource RPC timed out after 5 seconds".into()));
                        (key, generation, result)
                    }));
                }
            }
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if !self.closed {
            if let Some(name) = &self.service_name {
                let _ = self.daemon.unregister(name);
            }
            let _ = self.daemon.shutdown();
        }
    }
}

pub(super) fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    })
}

pub(super) async fn discover(args: Args) -> Result<(), BoxError> {
    let policy = version::Policy::new(args.min_peer_version.as_deref(), args.require_peer_build.as_deref())?;
    if !args.watch && !(1..=3600).contains(&args.seconds) {
        return Err("--seconds must be in 1..=3600".into());
    }
    let stop = shutdown_signal()?;
    let mut discovery = Discovery::start(None, policy)?;
    let duration = if args.watch {
        None
    } else {
        Some(Duration::from_secs(args.seconds))
    };
    let result = tokio::select! {
        result = discovery.watch_output(duration, args.table) => result,
        () = stop => Ok(()),
    };
    discovery.close().await;
    result
}

#[cfg(test)]
#[path = "discovery/tests.rs"]
mod tests;
