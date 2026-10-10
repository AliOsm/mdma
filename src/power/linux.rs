use super::{PowerReading, PowerSource};
use std::io;
use std::path::PathBuf;

/// Linux `/sys/class/power_supply`.
pub struct SysfsPower {
    root: PathBuf,
}

impl SysfsPower {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Default for SysfsPower {
    fn default() -> Self {
        Self::new("/sys/class/power_supply")
    }
}

impl PowerSource for SysfsPower {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        let attr = |dir: &PathBuf, name: &str| {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .map(|s| s.trim().to_string())
        };
        let mut supply_seen = false;
        let mut external_online = false;
        let mut discharging = false;
        let mut battery_percent = None;
        for entry in std::fs::read_dir(&self.root)? {
            let dir = entry?.path();
            match attr(&dir, "type").as_deref() {
                // A USB-C PD charger registers as USB (ucsi-source-psy-*), not Mains.
                Some("Mains" | "USB") => {
                    supply_seen = true;
                    external_online |= attr(&dir, "online").as_deref() == Some("1");
                }
                // Mice and headsets report scope=Device batteries; only the
                // system battery says anything about the host.
                Some("Battery") if attr(&dir, "scope").as_deref() != Some("Device") => {
                    supply_seen = true;
                    discharging |= attr(&dir, "status").as_deref() == Some("Discharging");
                    if battery_percent.is_none() {
                        battery_percent = attr(&dir, "capacity")
                            .and_then(|c| c.parse::<u8>().ok())
                            .map(|c| c.min(100));
                    }
                }
                _ => {}
            }
        }
        if !supply_seen {
            return Ok(None);
        }
        Ok(Some(PowerReading {
            on_battery: !external_online && discharging,
            battery_percent,
            gpu: None,
        }))
    }
}
