use super::{PowerReading, PowerSource};
use std::io;

/// FreeBSD ACPI: `hw.acpi.acline` and `hw.acpi.battery.life`.
pub struct SysctlPower;

impl PowerSource for SysctlPower {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        let sysctl = |name: &str| -> io::Result<Option<i64>> {
            let out = std::process::Command::new("sysctl")
                .args(["-n", name])
                .output()?;
            if !out.status.success() {
                // Unknown OID: no ACPI adapter or battery on this host.
                return Ok(None);
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        };
        let Some(acline) = sysctl("hw.acpi.acline")? else {
            return Ok(None);
        };
        let battery_percent = sysctl("hw.acpi.battery.life")?
            .filter(|life| (0..=100).contains(life))
            .and_then(|life| u8::try_from(life).ok());
        Ok(Some(PowerReading {
            on_battery: acline == 0,
            battery_percent,
            gpu: None,
        }))
    }
}
