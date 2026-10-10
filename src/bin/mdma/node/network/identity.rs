//! Hardware identity only: never publish SSIDs, nearby APs or machine serials.
use super::proto;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
use super::command;

pub(super) fn attach(nic: &mut proto::NetworkInterface) {
    nic.os_version = sysinfo::System::os_version().unwrap_or_default();
    nic.os_name = sysinfo::System::name().unwrap_or_default();
    nic.kernel_build = kernel_build();
    nic.hardware_identity = platform(nic);
}

fn id(value: &str, width: usize) -> Option<String> {
    let value = value.trim();
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() || value.len() > width || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let number = u32::from_str_radix(value, 16).ok()?;
    Some(format!("{number:0width$x}"))
}

#[cfg(target_os = "linux")]
#[allow(clippy::needless_pass_by_ref_mut, reason = "FreeBSD's collector fills `driver`; signatures stay uniform")]
fn platform(nic: &mut proto::NetworkInterface) -> Option<proto::NetworkHardwareIdentity> {
    let root = std::path::PathBuf::from(format!("/sys/class/net/{}/device", nic.name));
    let read = |field, width| id(&std::fs::read_to_string(root.join(field)).ok()?, width);
    Some(proto::NetworkHardwareIdentity {
        bus: "pci".into(),
        vendor: read("vendor", 4)?,
        device: read("device", 4)?,
        subsystem_vendor: read("subsystem_vendor", 4).unwrap_or_default(),
        subsystem_device: read("subsystem_device", 4).unwrap_or_default(),
        revision: read("revision", 2).unwrap_or_default(),
    })
}

#[cfg(target_os = "macos")]
#[expect(clippy::needless_pass_by_ref_mut, reason = "FreeBSD's collector fills `driver`; signatures stay uniform")]
fn platform(nic: &mut proto::NetworkInterface) -> Option<proto::NetworkHardwareIdentity> {
    static CARDS: std::sync::OnceLock<std::collections::BTreeMap<String, proto::NetworkHardwareIdentity>> =
        std::sync::OnceLock::new();
    CARDS.get_or_init(|| {
        command("/usr/sbin/system_profiler", &["SPAirPortDataType", "-json"])
            .and_then(|text| serde_json::from_str(&text).ok())
            .map(|value| airport(&value))
            .unwrap_or_default()
    }).get(&nic.name).cloned()
}

#[cfg(any(target_os = "macos", test))]
fn airport(value: &serde_json::Value) -> std::collections::BTreeMap<String, proto::NetworkHardwareIdentity> {
    let mut cards = std::collections::BTreeMap::new();
    if let Some(reports) = value["SPAirPortDataType"].as_array() {
        for report in reports {
            if let Some(interfaces) = report["spairport_airport_interfaces"].as_array() {
                for interface in interfaces {
                    let Some(name) = interface["_name"].as_str() else { continue };
                    let Some(card) = interface["spairport_wireless_card_type"].as_str() else { continue };
                    let Some((_, ids)) = card.split_once('(') else { continue };
                    let Some((ids, _)) = ids.split_once(')') else { continue };
                    let Some((vendor, device)) = ids.split_once(',') else { continue };
                    let (Some(vendor), Some(device)) = (id(vendor, 4), id(device, 4)) else { continue };
                    cards.insert(name.to_owned(), proto::NetworkHardwareIdentity {
                        bus: "apple_airport".into(), vendor, device, ..Default::default()
                    });
                }
            }
        }
    }
    cards
}

#[cfg(target_os = "freebsd")]
fn platform(nic: &mut proto::NetworkInterface) -> Option<proto::NetworkHardwareIdentity> {
    static PCI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let pci = PCI.get_or_init(|| command("/usr/sbin/pciconf", &["-l"]).unwrap_or_default());
    let mut name = nic.name.clone();
    if let Some(unit) = name.strip_prefix("wlan")
        .filter(|unit| !unit.is_empty() && unit.bytes().all(|c| c.is_ascii_digit())) {
        // Do not guess a radio based on whichever PCI Wi-Fi device appears first.
        let key = format!("net.wlan.{unit}.%parent");
        if let Some(parent) = command("/sbin/sysctl", &["-n", &key]) {
            name = parent.trim().into();
        } else if let Some(output) = command("/sbin/ifconfig", &["-v", &name]) {
            if let Some(parent) = output.lines().find_map(|line|
                line.trim().strip_prefix("parent interface: ").map(str::trim)) {
                name = parent.into();
            }
        }
    }
    let identity = freebsd_pci(pci, &name)?;
    nic.driver = name.trim_end_matches(|c: char| c.is_ascii_digit()).into();
    Some(identity)
}

#[cfg(any(target_os = "freebsd", test))]
fn freebsd_pci(output: &str, name: &str) -> Option<proto::NetworkHardwareIdentity> {
    let line = output.lines().find(|line|
        line.split_once('@').is_some_and(|(device, bus)| device == name && bus.starts_with("pci")))?;
    let field = |key: &str, width| {
        let value = line.split_whitespace().find_map(|token| token.strip_prefix(key))?;
        id(value, width)
    };
    Some(proto::NetworkHardwareIdentity {
        bus: "pci".into(), vendor: field("vendor=", 4)?, device: field("device=", 4)?,
        subsystem_vendor: field("subvendor=", 4).unwrap_or_default(),
        subsystem_device: field("subdevice=", 4).unwrap_or_default(),
        revision: field("rev=", 2).unwrap_or_default(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn platform(_: &mut proto::NetworkInterface) -> Option<proto::NetworkHardwareIdentity> { None }

fn kernel_build() -> String {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        #[cfg(target_os = "freebsd")]
        { command("/sbin/sysctl", &["-n", "kern.version"]).unwrap_or_default()
            .lines().next().unwrap_or("").to_owned() }
        #[cfg(not(target_os = "freebsd"))]
        { sysinfo::System::kernel_version().unwrap_or_default() }
    }).clone()
}

#[cfg(test)]
#[path = "identity/tests.rs"]
mod tests;
