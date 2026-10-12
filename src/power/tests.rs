use super::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A scratch `/sys/class/power_supply`, removed on drop.
struct FakeSysfs(PathBuf);

impl FakeSysfs {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mdma-power-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn supply(&self, name: &str, attrs: &[(&str, &str)]) -> &Self {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), format!("{value}\n")).unwrap();
        }
        self
    }

    fn read(&self) -> Option<PowerReading> {
        SysfsPower::new(&self.0).read().unwrap()
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for FakeSysfs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "fixtures stand in for optional sensor readings"
)]
fn battery(on_battery: bool, percent: Option<u8>) -> Option<PowerReading> {
    Some(PowerReading {
        on_battery,
        battery_percent: percent,
        gpu: None,
    })
}

fn gpu(limit_w: u32, default_w: u32) -> GpuCap {
    GpuCap {
        limit_mw: limit_w * 1000,
        default_mw: default_w * 1000,
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "fixtures stand in for optional sensor readings"
)]
fn on_ac_with_gpu(cap: GpuCap) -> Option<PowerReading> {
    Some(PowerReading {
        on_battery: false,
        battery_percent: Some(90),
        gpu: Some(cap),
    })
}

#[test]
fn sysfs_on_ac() {
    let fs = FakeSysfs::new();
    fs.supply("ADP1", &[("type", "Mains"), ("online", "1")])
        .supply(
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Charging"),
                ("capacity", "80"),
            ],
        );
    assert_eq!(fs.read(), battery(false, Some(80)));
}

#[test]
fn sysfs_on_battery() {
    let fs = FakeSysfs::new();
    fs.supply("ADP1", &[("type", "Mains"), ("online", "0")])
        .supply(
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Discharging"),
                ("capacity", "63"),
            ],
        );
    assert_eq!(fs.read(), battery(true, Some(63)));
}

#[test]
fn sysfs_mains_under_another_name_and_usb_pd() {
    let fs = FakeSysfs::new();
    fs.supply("AC", &[("type", "Mains"), ("online", "0")])
        .supply(
            "ucsi-source-psy-USBC000:001",
            &[("type", "USB"), ("online", "1")],
        )
        .supply(
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging"),
                ("capacity", "50"),
            ],
        );
    assert_eq!(
        fs.read(),
        battery(false, Some(50)),
        "a USB-C charger online counts as AC"
    );
}

#[test]
fn sysfs_without_supplies_or_capacity() {
    let fs = FakeSysfs::new();
    assert_eq!(fs.read(), None, "a desktop has no supplies");
    fs.supply("BAT1", &[("type", "Battery"), ("status", "Discharging")]);
    assert_eq!(fs.read(), battery(true, None));
}

#[test]
fn sysfs_ignores_peripheral_batteries() {
    let fs = FakeSysfs::new();
    fs.supply(
        "hidpp_battery_0",
        &[
            ("type", "Battery"),
            ("scope", "Device"),
            ("status", "Discharging"),
            ("capacity", "5"),
        ],
    );
    assert_eq!(
        fs.read(),
        None,
        "a wireless mouse battery says nothing about the host"
    );
}

#[test]
fn sysfs_missing_root_is_an_error() {
    let fs = FakeSysfs::new();
    let missing = fs.path().join("absent");
    assert!(SysfsPower::new(missing).read().is_err());
}

#[test]
fn classify_thresholds() {
    let policy = WatchPolicy {
        critical_percent: 10,
        ..WatchPolicy::default()
    };
    assert_eq!(classify(None, &policy), ServerState::Serving);
    assert_eq!(
        classify(battery(false, Some(3)), &policy),
        ServerState::Serving
    );
    assert_eq!(
        classify(battery(true, None), &policy),
        ServerState::Degraded
    );
    assert_eq!(
        classify(battery(true, Some(11)), &policy),
        ServerState::Degraded
    );
    assert_eq!(
        classify(battery(true, Some(10)), &policy),
        ServerState::Critical
    );
    assert_eq!(
        classify(battery(true, Some(0)), &policy),
        ServerState::Critical
    );
}

#[test]
fn debounce_ignores_a_single_flap() {
    let mut d = Debouncer::new(2);
    let (s, g) = (ServerState::Serving, ServerState::Degraded);
    assert_eq!(d.observe(s, g), None);
    assert_eq!(d.observe(s, s), None, "flap back resets the count");
    assert_eq!(d.observe(s, g), None);
    assert_eq!(d.observe(s, g), Some(g));
    // A different candidate restarts the count.
    assert_eq!(d.observe(g, ServerState::Critical), None);
    assert_eq!(d.observe(g, ServerState::Serving), None);
    assert_eq!(
        d.observe(g, ServerState::Serving),
        Some(ServerState::Serving)
    );
    assert_eq!(
        Debouncer::new(0).observe(s, g),
        Some(g),
        "debounce 0 behaves as 1"
    );
}

#[test]
fn fsm_transitions() {
    use ServerFsmEvent::*;
    let mut fsm = DynamicServerFsm::new(());
    assert_eq!(fsm.current_state(), ServerState::Serving);
    assert!(fsm.handle(Restore).is_err(), "already serving");
    fsm.handle(Constrain).unwrap();
    assert_eq!(fsm.current_state(), ServerState::Degraded);
    fsm.handle(Critical).unwrap();
    fsm.handle(Constrain).unwrap();
    fsm.handle(Restore).unwrap();
    fsm.handle(Critical).unwrap();
    fsm.handle(Restore).unwrap();
    assert_eq!(fsm.current_state(), ServerState::Serving);

    for setup in [vec![], vec![Constrain], vec![Critical]] {
        let mut fsm = DynamicServerFsm::new(());
        for event in setup {
            fsm.handle(event).unwrap();
        }
        fsm.handle(Drain).unwrap();
        assert_eq!(fsm.current_state(), ServerState::Draining);
        for event in [Restore, Constrain, Critical, Drain] {
            assert!(fsm.handle(event).is_err(), "draining is terminal");
        }
    }
}

struct Script(VecDeque<io::Result<Option<PowerReading>>>);

impl PowerSource for Script {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        self.0.pop_front().unwrap_or(Ok(None))
    }
}

fn script(readings: Vec<io::Result<Option<PowerReading>>>) -> Script {
    Script(readings.into())
}

#[test]
fn watcher_applies_first_reading_then_debounces() {
    let (mut w, rx) = PowerWatcher::new(
        script(vec![
            Ok(battery(true, Some(60))),
            Ok(battery(false, Some(60))),
            Ok(battery(true, Some(59))),
            Ok(battery(false, Some(59))),
            Ok(battery(false, Some(60))),
        ]),
        WatchPolicy::default(),
    );
    w.poll();
    assert_eq!(
        w.state(),
        ServerState::Degraded,
        "started on battery: no grace period"
    );
    w.poll();
    assert_eq!(w.state(), ServerState::Degraded, "one AC reading is a flap");
    w.poll();
    assert_eq!(
        *rx.borrow(),
        PowerStatus {
            state: ServerState::Degraded,
            reading: battery(true, Some(59))
        }
    );
    w.poll();
    w.poll();
    assert_eq!(w.state(), ServerState::Serving);
    assert_eq!(rx.borrow().state, ServerState::Serving);
}

#[test]
fn watcher_read_errors_count_as_mains() {
    let (mut w, rx) = PowerWatcher::new(
        script(vec![
            Ok(battery(true, Some(5))),
            Err(io::Error::other("gone")),
            Err(io::Error::other("gone")),
        ]),
        WatchPolicy::default(),
    );
    w.poll();
    assert_eq!(w.state(), ServerState::Critical);
    w.poll();
    w.poll();
    assert_eq!(w.state(), ServerState::Serving);
    assert_eq!(rx.borrow().reading, None);
}

#[test]
fn watcher_drain_is_final() {
    let (mut w, rx) = PowerWatcher::new(
        script(vec![Ok(battery(true, Some(40)))]),
        WatchPolicy::default(),
    );
    w.poll();
    w.drain();
    assert_eq!(
        *rx.borrow(),
        PowerStatus {
            state: ServerState::Draining,
            reading: battery(true, Some(40))
        }
    );
    w.poll();
    assert_eq!(w.state(), ServerState::Draining);
}

#[tokio::test(start_paused = true)]
async fn spawned_watcher_polls_and_drains_on_shutdown() {
    use rama::graceful::Shutdown;
    use tokio_util::sync::CancellationToken;

    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let policy = WatchPolicy {
        poll: Duration::from_secs(1),
        debounce: 1,
        ..WatchPolicy::default()
    };
    let mut rx = spawn_power_watcher(
        script(vec![
            Ok(battery(false, Some(90))),
            Ok(battery(true, Some(90))),
        ]),
        policy,
        shutdown.guard(),
    );
    assert_eq!(
        rx.borrow().reading,
        battery(false, Some(90)),
        "first read is synchronous"
    );
    rx.wait_for(|s| s.state == ServerState::Degraded)
        .await
        .unwrap();
    trigger.cancel();
    rx.wait_for(|s| s.state == ServerState::Draining)
        .await
        .unwrap();
    shutdown
        .shutdown_with_limit(Duration::from_secs(1))
        .await
        .unwrap();
}

#[test]
fn classify_gpu_cap_on_ac() {
    let policy = WatchPolicy::default();
    assert_eq!(policy.gpu_cap_percent, 50);
    assert_eq!(
        classify(on_ac_with_gpu(gpu(80, 80)), &policy),
        ServerState::Serving
    );
    assert_eq!(
        classify(on_ac_with_gpu(gpu(40, 80)), &policy),
        ServerState::Serving,
        "exactly half is not below half"
    );
    assert_eq!(
        classify(on_ac_with_gpu(gpu(15, 80)), &policy),
        ServerState::Degraded,
        "the thermal ceiling degrades even on AC"
    );
    let off = WatchPolicy {
        gpu_cap_percent: 0,
        ..policy
    };
    assert_eq!(
        classify(on_ac_with_gpu(gpu(15, 80)), &off),
        ServerState::Serving
    );
    let low_battery = Some(PowerReading {
        on_battery: true,
        battery_percent: Some(5),
        gpu: Some(gpu(15, 80)),
    });
    assert_eq!(
        classify(low_battery, &policy),
        ServerState::Critical,
        "battery outranks the GPU cap"
    );
}

#[test]
fn describe_includes_the_gpu_limit() {
    assert_eq!(
        describe(on_ac_with_gpu(gpu(15, 80))),
        "on AC, GPU limit 15 of 80 W"
    );
    assert_eq!(describe(battery(true, Some(40))), "battery 40%");
}

struct GpuScript(VecDeque<io::Result<Option<GpuCap>>>);

impl GpuProbe for GpuScript {
    fn read(&mut self) -> io::Result<Option<GpuCap>> {
        self.0.pop_front().unwrap_or(Ok(None))
    }
}

#[test]
fn with_gpu_merges_and_survives_probe_errors() {
    let mut source = WithGpu::new(
        script(vec![
            Ok(battery(false, Some(90))),
            Ok(battery(true, Some(50))),
            Ok(None),
            Ok(None),
        ]),
        Box::new(GpuScript(
            vec![
                Ok(Some(gpu(15, 80))),
                Err(io::Error::other("nvml gone")),
                Ok(Some(gpu(80, 80))),
                Ok(None),
            ]
            .into(),
        )),
    );
    assert_eq!(source.read().unwrap(), on_ac_with_gpu(gpu(15, 80)));
    assert_eq!(
        source.read().unwrap(),
        battery(true, Some(50)),
        "a failing probe leaves the supply reading intact"
    );
    assert_eq!(
        source.read().unwrap(),
        Some(PowerReading {
            on_battery: false,
            battery_percent: None,
            gpu: Some(gpu(80, 80)),
        }),
        "a desktop with no supplies still reports the GPU"
    );
    assert_eq!(source.read().unwrap(), None);
}

#[test]
fn with_gpu_propagates_host_errors() {
    let mut source = WithGpu::new(
        script(vec![Err(io::Error::other("sysfs gone"))]),
        Box::new(GpuScript(vec![Ok(Some(gpu(15, 80)))].into())),
    );
    assert!(source.read().is_err());
}
