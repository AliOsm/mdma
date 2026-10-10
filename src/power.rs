//! Request admission and limits derived from host and GPU power readings.
//! Battery operation or GPU power caps degrade service; critically low
//! battery rejects new generations.
use rama::graceful::ShutdownGuard;
use state_machines::state_machine;
use std::io;
use std::time::Duration;
use tokio::sync::watch;

#[cfg(target_os = "freebsd")]
mod freebsd;
mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // IOKit FFI
mod macos;
#[cfg(feature = "cuda")]
#[allow(unsafe_code)] // NVML FFI
mod nvml;

#[cfg(target_os = "freebsd")]
pub use freebsd::SysctlPower;
pub use linux::SysfsPower;
#[cfg(target_os = "macos")]
pub use macos::IokitPower;
#[cfg(feature = "cuda")]
pub use nvml::NvmlProbe;

state_machine! {
    name: ServerFsm,
    dynamic: true,
    initial: Serving,
    states: [Serving, Degraded, Critical, Draining],
    events {
        constrain { transition: { from: [Serving, Critical], to: Degraded } }
        critical  { transition: { from: [Serving, Degraded], to: Critical } }
        restore   { transition: { from: [Degraded, Critical], to: Serving } }
        drain     { transition: { from: [Serving, Degraded, Critical], to: Draining } }
    }
}

pub type ServerState = ServerFsmState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerReading {
    pub on_battery: bool,
    pub battery_percent: Option<u8>,
    /// The inference GPU's power limit, when a probe for it exists.
    pub gpu: Option<GpuCap>,
}

/// Enforced vs. default board power limit, in milliwatts. The enforced
/// limit is the lowest of the user, driver and platform (firmware) caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuCap {
    pub limit_mw: u32,
    pub default_mw: u32,
}

impl GpuCap {
    /// True when the limit is below `percent` of the default.
    pub fn below(&self, percent: u8) -> bool {
        u64::from(self.limit_mw) * 100 < u64::from(self.default_mw) * u64::from(percent)
    }
}

/// Reads the inference GPU's power limit. `Ok(None)`: not reported.
pub trait GpuProbe: Send + 'static {
    fn read(&mut self) -> io::Result<Option<GpuCap>>;
}

/// What the service sees: the FSM state plus the reading that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerStatus {
    pub state: ServerState,
    pub reading: Option<PowerReading>,
}

impl Default for PowerStatus {
    fn default() -> Self {
        Self {
            state: ServerState::Serving,
            reading: None,
        }
    }
}

/// `Ok(None)`: the host exposes no supply information and counts as mains.
pub trait PowerSource: Send + 'static {
    fn read(&mut self) -> io::Result<Option<PowerReading>>;
}

impl PowerSource for Box<dyn PowerSource> {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        (**self).read()
    }
}

/// A host source with a GPU probe merged in. A failing probe is logged once
/// and reads as "not reported"; it does not hide the battery state.
pub struct WithGpu<S> {
    host: S,
    gpu: Box<dyn GpuProbe>,
    gpu_failed: bool,
}

impl<S: PowerSource> WithGpu<S> {
    pub fn new(host: S, gpu: Box<dyn GpuProbe>) -> Self {
        Self {
            host,
            gpu,
            gpu_failed: false,
        }
    }
}

impl<S: PowerSource> PowerSource for WithGpu<S> {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        let host = self.host.read()?;
        let gpu = match self.gpu.read() {
            Ok(gpu) => {
                self.gpu_failed = false;
                gpu
            }
            Err(e) => {
                if !self.gpu_failed {
                    eprintln!("mdma power: cannot read GPU power limit ({e})");
                    self.gpu_failed = true;
                }
                None
            }
        };
        Ok(match (host, gpu) {
            (Some(host), gpu) => Some(PowerReading { gpu, ..host }),
            (None, Some(gpu)) => Some(PowerReading {
                on_battery: false,
                battery_percent: None,
                gpu: Some(gpu),
            }),
            (None, None) => None,
        })
    }
}

/// Hosts without a supported power interface always count as mains.
pub struct NoPower;

impl PowerSource for NoPower {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        Ok(None)
    }
}

pub fn default_source() -> Box<dyn PowerSource> {
    #[cfg(target_os = "linux")]
    let source = SysfsPower::default();
    #[cfg(target_os = "freebsd")]
    let source = SysctlPower;
    #[cfg(target_os = "macos")]
    let source = IokitPower;
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
    let source = NoPower;
    Box::new(source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchPolicy {
    pub poll: Duration,
    /// Consecutive polls a new state must hold before the FSM moves.
    pub debounce: u32,
    /// On battery at or below this percentage, stop admitting generations.
    pub critical_percent: u8,
    /// Degrade when the GPU's enforced power limit is below this percentage
    /// of its default. 0 disables the check.
    pub gpu_cap_percent: u8,
}

impl Default for WatchPolicy {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(5),
            debounce: 2,
            critical_percent: 10,
            gpu_cap_percent: 50,
        }
    }
}

/// Request limits while `Degraded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DegradedLimits {
    pub max_tokens: usize,
    pub n_ctx: usize,
}

impl Default for DegradedLimits {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            n_ctx: 2048,
        }
    }
}

pub fn classify(reading: Option<PowerReading>, policy: &WatchPolicy) -> ServerState {
    let Some(reading) = reading else {
        return ServerState::Serving;
    };
    if reading.on_battery {
        return match reading.battery_percent {
            Some(p) if p <= policy.critical_percent => ServerState::Critical,
            _ => ServerState::Degraded,
        };
    }
    if reading
        .gpu
        .is_some_and(|gpu| gpu.below(policy.gpu_cap_percent))
    {
        return ServerState::Degraded;
    }
    ServerState::Serving
}

/// Apply a state change after `needed` consecutive matching readings.
#[derive(Debug)]
pub struct Debouncer {
    needed: u32,
    candidate: Option<ServerState>,
    count: u32,
}

impl Debouncer {
    pub fn new(needed: u32) -> Self {
        Self {
            needed: needed.max(1),
            candidate: None,
            count: 0,
        }
    }

    pub fn observe(&mut self, current: ServerState, target: ServerState) -> Option<ServerState> {
        if target == current {
            self.candidate = None;
            self.count = 0;
            return None;
        }
        if self.candidate == Some(target) {
            self.count += 1;
        } else {
            self.candidate = Some(target);
            self.count = 1;
        }
        if self.count < self.needed {
            return None;
        }
        self.candidate = None;
        self.count = 0;
        Some(target)
    }
}

fn event_for(target: ServerState) -> ServerFsmEvent {
    match target {
        ServerState::Serving => ServerFsmEvent::Restore,
        ServerState::Degraded => ServerFsmEvent::Constrain,
        ServerState::Critical => ServerFsmEvent::Critical,
        ServerState::Draining => ServerFsmEvent::Drain,
    }
}

fn describe(reading: Option<PowerReading>) -> String {
    let Some(reading) = reading else {
        return "no power supply info".into();
    };
    let supply = match reading {
        PowerReading {
            on_battery: false, ..
        } => "on AC".to_string(),
        PowerReading {
            battery_percent: Some(p),
            ..
        } => format!("battery {p}%"),
        _ => "battery".to_string(),
    };
    match reading.gpu {
        Some(gpu) => format!(
            "{supply}, GPU limit {} of {} W",
            gpu.limit_mw / 1000,
            gpu.default_mw / 1000
        ),
        None => supply,
    }
}

/// Drives the FSM from a power source.
pub struct PowerWatcher<S> {
    source: S,
    policy: WatchPolicy,
    fsm: DynamicServerFsm<()>,
    debounce: Debouncer,
    tx: watch::Sender<PowerStatus>,
    read_failed: bool,
    started: bool,
}

impl<S: PowerSource> PowerWatcher<S> {
    pub fn new(source: S, policy: WatchPolicy) -> (Self, watch::Receiver<PowerStatus>) {
        let (tx, rx) = watch::channel(PowerStatus::default());
        let watcher = Self {
            source,
            policy,
            fsm: DynamicServerFsm::new(()),
            debounce: Debouncer::new(policy.debounce),
            tx,
            read_failed: false,
            started: false,
        };
        (watcher, rx)
    }

    pub fn state(&self) -> ServerState {
        self.fsm.current_state()
    }

    /// One poll. The first reading applies immediately, so a server started
    /// on battery does not serve full-size requests for a debounce period.
    pub fn poll(&mut self) {
        let reading = match self.source.read() {
            Ok(reading) => {
                if self.read_failed {
                    eprintln!("mdma power: supply readable again");
                    self.read_failed = false;
                }
                reading
            }
            Err(e) => {
                if !self.read_failed {
                    eprintln!("mdma power: cannot read power supply ({e}); treating host as on AC");
                    self.read_failed = true;
                }
                None
            }
        };
        self.apply(reading);
    }

    fn apply(&mut self, reading: Option<PowerReading>) {
        let current = self.fsm.current_state();
        if current == ServerState::Draining {
            return;
        }
        let target = classify(reading, &self.policy);
        let next = if self.started {
            self.debounce.observe(current, target)
        } else {
            self.started = true;
            (target != current).then_some(target)
        };
        if let Some(next) = next {
            self.transition(next, &describe(reading));
        }
        // Publish the latest reading even without a transition, so Introspect
        // shows the battery percentage moving.
        self.tx.send_replace(PowerStatus {
            state: self.fsm.current_state(),
            reading,
        });
    }

    fn transition(&mut self, next: ServerState, why: &str) {
        let from = self.fsm.current_state();
        match self.fsm.handle(event_for(next)) {
            Ok(()) => eprintln!(
                "mdma power: {} -> {} ({why})",
                from.name().to_ascii_lowercase(),
                next.name().to_ascii_lowercase()
            ),
            Err(e) => eprintln!("mdma power: rejected {from:?} -> {next:?}: {e:?}"),
        }
    }

    pub fn drain(&mut self) {
        let reading = self.tx.borrow().reading;
        if self.fsm.current_state() != ServerState::Draining {
            self.transition(ServerState::Draining, "shutdown");
        }
        self.tx.send_replace(PowerStatus {
            state: self.fsm.current_state(),
            reading,
        });
    }
}

/// Poll `source` until shutdown begins, then publish `Draining` and exit.
/// The first read happens before this returns, so the receiver never shows
/// `Serving` on a host that is already on battery; call it from a blocking
/// context. The task holds `guard`, so shutdown waits for it; it exits
/// promptly.
pub fn spawn_power_watcher<S: PowerSource>(
    source: S,
    policy: WatchPolicy,
    guard: ShutdownGuard,
) -> watch::Receiver<PowerStatus> {
    let (mut watcher, rx) = PowerWatcher::new(source, policy);
    watcher.poll();
    let status = *rx.borrow();
    eprintln!(
        "mdma power: {} ({}, polling every {}s)",
        status.state.name().to_ascii_lowercase(),
        describe(status.reading),
        policy.poll.as_secs()
    );
    tokio::spawn(async move {
        let mut watcher = Some(watcher);
        let start = tokio::time::Instant::now() + policy.poll;
        let mut interval = tokio::time::interval_at(start, policy.poll);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = guard.cancelled() => break,
                _ = interval.tick() => {
                    // Power-source reads may block; keep them off the async workers.
                    let mut w = watcher.take().expect("each poll returns the watcher");
                    watcher = match tokio::task::spawn_blocking(move || { w.poll(); w }).await {
                        Ok(w) => Some(w),
                        Err(e) => {
                            eprintln!("mdma power: watcher poll panicked ({e}); power awareness disabled");
                            return;
                        }
                    };
                }
            }
        }
        if let Some(mut watcher) = watcher {
            watcher.drain();
        }
    });
    rx
}

#[cfg(test)]
mod tests;
