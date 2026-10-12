//! Best-effort link metadata and a fresh TCP-connect round trip, not throughput.
use super::proto;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};
#[path = "network/catalog.rs"]
mod catalog;
#[path = "network/identity.rs"]
mod identity;

pub(super) fn interface(address: IpAddr) -> Option<proto::NetworkInterface> {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let (name, network) = networks
        .iter()
        .find(|(_, network)| network.ip_networks().iter().any(|ip| ip.addr == address))?;
    let mut nic = proto::NetworkInterface {
        name: name.clone(),
        address: address.to_string(),
        mtu_bytes: network.mtu(),
        link_speed_bps: link_speed(name),
        underlying_interfaces: underlying(name, &networks),
        ..Default::default()
    };
    classify(&mut nic);
    Some(nic)
}

#[cfg(target_os = "linux")]
fn underlying(name: &str, networks: &sysinfo::Networks) -> Vec<proto::NetworkInterface> {
    let Ok(entries) = std::fs::read_dir(format!("/sys/class/net/{name}")) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix("lower_")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .take(8)
        .map(|name| {
            let network = networks.list().get(&name);
            let mut nic = proto::NetworkInterface {
                address: String::new(), // Members need not own an IP.
                mtu_bytes: network.map_or(0, sysinfo::NetworkData::mtu),
                link_speed_bps: link_speed(&name),
                name,
                underlying_interfaces: Vec::new(),
                ..Default::default()
            };
            classify(&mut nic);
            nic
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn underlying(_: &str, _: &sysinfo::Networks) -> Vec<proto::NetworkInterface> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn link_speed(name: &str) -> Option<u64> {
    if name.contains('/') || name == "." || name == ".." {
        return None;
    }
    // Bridges/bonds expose synthetic rates; never present these as physical links.
    if !std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists() {
        return None;
    }
    let speed = std::fs::read_to_string(format!("/sys/class/net/{name}/speed")).ok()?;
    mbps(speed.trim())
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn link_speed(name: &str) -> Option<u64> {
    let output = std::process::Command::new("/sbin/ifconfig")
        .arg(name)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    media_speed(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn link_speed(_: &str) -> Option<u64> {
    None
}

fn mbps(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)
        .filter(|v| *v > 0)
}

// Parse only selected media, never the interface's supported/max capabilities.
#[cfg(any(target_os = "macos", target_os = "freebsd", test))]
fn media_speed(output: &str) -> Option<u64> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("media:"))?;
    let selected = line.split_once('(')?.1.split_once(')')?.0;
    let rate = selected.split("base").next()?.trim();
    match rate {
        "2.5G" => Some(2_500_000_000),
        _ if rate.ends_with('G') => rate
            .strip_suffix('G')?
            .parse::<u64>()
            .ok()?
            .checked_mul(1_000_000_000),
        _ => mbps(rate),
    }
}

pub(super) fn json(nic: Option<proto::NetworkInterface>) -> serde_json::Value {
    match nic {
        Some(nic) => serde_json::json!({
            "name": nic.name, "address": nic.address, "mtu_bytes": nic.mtu_bytes,
            "link_speed_bps": nic.link_speed_bps,
            "interface_kind": known(&nic.interface_kind),
            "physical_medium": known(&nic.physical_medium),
            "driver": known(&nic.driver),
            "visibility": known(&nic.visibility),
            "classification_source": known(&nic.classification_source),
            "hardware_identity": nic.hardware_identity.map(|i| serde_json::json!({
                "bus": i.bus, "vendor": i.vendor, "device": i.device,
                "subsystem_vendor": i.subsystem_vendor, "subsystem_device": i.subsystem_device,
                "revision": i.revision,
            })),
            "hardware_capabilities": catalog::profile(nic.hardware_capabilities),
            "driver_capabilities": catalog::profile(nic.driver_capabilities),
            "os_version": nic.os_version, "os_name": nic.os_name, "kernel_build": nic.kernel_build,
            "catalog_version": nic.catalog_version, "catalog_status": known(&nic.catalog_status),
            "underlying_interfaces": nic.underlying_interfaces.into_iter()
                .map(|nic| json(Some(nic))).collect::<Vec<_>>(),
        }),
        None => serde_json::Value::Null,
    }
}

fn known(value: &str) -> &str {
    if value.is_empty() { "unknown" } else { value }
}

fn classify(nic: &mut proto::NetworkInterface) {
    nic.interface_kind = "unknown".into();
    nic.physical_medium = "unknown".into();
    nic.visibility = if guest() { "guest" } else { "host_or_unknown" }.into();
    classify_platform(nic);
    identity::attach(nic);
    suppress_guest_physical_claims(nic);
    catalog::apply(nic);
}

fn suppress_guest_physical_claims(nic: &mut proto::NetworkInterface) {
    // Even emulated Ethernet may claim baseT and a speed. It is not the host NIC.
    if nic.visibility == "guest" || nic.interface_kind == "virtual" {
        nic.physical_medium = "unknown".into();
        nic.link_speed_bps = None;
    }
}

fn guest() -> bool {
    static GUEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GUEST.get_or_init(guest_platform)
}

#[cfg(target_os = "linux")]
fn guest_platform() -> bool {
    std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .any(|line| {
            line.starts_with("flags") && line.split_whitespace().any(|word| word == "hypervisor")
        })
        || std::fs::read_to_string("/sys/hypervisor/type")
            .is_ok_and(|value| !value.trim().is_empty())
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
fn guest_platform() -> bool {
    #[cfg(target_os = "freebsd")]
    let key = "kern.vm_guest";
    #[cfg(target_os = "macos")]
    let key = "kern.hv_vmm_present";
    let Some(value) =
        command("/usr/sbin/sysctl", &["-n", key]).or_else(|| command("/sbin/sysctl", &["-n", key]))
    else {
        return false;
    };
    #[cfg(target_os = "freebsd")]
    {
        !value.trim().is_empty() && value.trim() != "none"
    }
    #[cfg(target_os = "macos")]
    {
        value.trim() == "1"
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
fn guest_platform() -> bool {
    false
}

fn command(path: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(path)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn classify_platform(nic: &mut proto::NetworkInterface) {
    let root = std::path::PathBuf::from(format!("/sys/class/net/{}", nic.name));
    nic.classification_source = "linux_sysfs".into();
    nic.driver = std::fs::read_link(root.join("device/driver"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    nic.interface_kind = if nic.name == "lo" {
        "loopback"
    } else if root.join("bridge").exists() {
        "bridge"
    } else if root.join("bonding").exists() {
        "bond"
    } else if root.join("wireless").exists() || root.join("phy80211").exists() {
        "wifi"
    } else if virtual_driver(&nic.driver) || !root.join("device").exists() {
        "virtual"
    } else if std::fs::read_to_string(root.join("type")).is_ok_and(|v| v.trim() == "1") {
        "ethernet"
    } else {
        "unknown"
    }
    .into();
    if virtual_driver(&nic.driver) {
        nic.visibility = "guest".into();
    }
    if nic.visibility == "guest" {
        return;
    }
    if nic.interface_kind == "wifi" {
        nic.physical_medium = "wireless".into();
    } else if nic.interface_kind == "ethernet" {
        // Optional tool: absence or denied access leaves the medium unknown.
        let output = ["/usr/sbin/ethtool", "/sbin/ethtool", "/usr/bin/ethtool"]
            .iter()
            .find_map(|path| command(path, &[&nic.name]));
        if let Some(output) = output {
            nic.physical_medium = ethtool_medium(&output).into();
            nic.classification_source = "linux_sysfs+ethtool_reported_port".into();
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn virtual_driver(driver: &str) -> bool {
    matches!(
        driver,
        "virtio_net" | "vmxnet3" | "hv_netvsc" | "xen-netfront" | "xen_netfront" | "netfront"
    )
}

#[cfg(any(target_os = "linux", test))]
fn ethtool_medium(output: &str) -> &'static str {
    match output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Port:").map(str::trim))
    {
        Some("Twisted Pair") => "copper",
        Some("FIBRE") => "fiber",
        _ => "unknown", // Direct Attach, Other and SFP ambiguity are not guessed.
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn classify_platform(nic: &mut proto::NetworkInterface) {
    let Some(output) = command("/sbin/ifconfig", &[&nic.name]) else {
        return;
    };
    nic.classification_source = "ifconfig_selected_media".into();
    #[cfg(target_os = "freebsd")]
    for driver in ["vtnet", "vmx", "xn", "hn"] {
        if nic
            .name
            .strip_prefix(driver)
            .is_some_and(|unit| !unit.is_empty() && unit.bytes().all(|c| c.is_ascii_digit()))
        {
            nic.driver = driver.into();
            nic.interface_kind = "virtual".into();
            nic.visibility = "guest".into();
            nic.classification_source = "freebsd_virtual_driver_name".into();
            return;
        }
    }
    if output.contains("LOOPBACK") {
        nic.interface_kind = "loopback".into();
    } else if output.contains("member:") {
        nic.interface_kind = "bridge".into();
    } else if output.contains("802.11") || output.contains("ssid ") {
        nic.interface_kind = "wifi".into();
        nic.physical_medium = "wireless".into();
    } else {
        let medium = selected_medium(&output);
        if medium != "unknown" {
            nic.interface_kind = "ethernet".into();
            nic.physical_medium = medium.into();
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(ports) = command("/usr/sbin/networksetup", &["-listallhardwareports"])
        && hardware_wifi(&ports, &nic.name)
    {
        nic.interface_kind = "wifi".into();
        nic.physical_medium = "wireless".into();
        nic.classification_source = "networksetup_hardware_port".into();
    }
}

#[cfg(any(target_os = "macos", test))]
fn hardware_wifi(output: &str, name: &str) -> bool {
    let mut wifi = false;
    for line in output.lines() {
        if let Some(port) = line.strip_prefix("Hardware Port: ") {
            wifi = matches!(port.trim(), "Wi-Fi" | "AirPort");
        }
        if let Some(device) = line.strip_prefix("Device: ")
            && device.trim() == name
        {
            return wifi;
        }
    }
    false
}

#[cfg(any(target_os = "macos", target_os = "freebsd", test))]
fn selected_medium(output: &str) -> &'static str {
    let Some(line) = output
        .lines()
        .find(|line| line.trim_start().starts_with("media:"))
    else {
        return "unknown";
    };
    let Some((_, selected)) = line.split_once('(') else {
        return "unknown";
    };
    let Some((selected, _)) = selected.split_once(')') else {
        return "unknown";
    };
    let token = selected
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some((_, suffix)) = token.split_once("base") else {
        return "unknown";
    };
    match suffix.trim_start_matches('-') {
        "t" | "tx" | "t1" => "copper",
        "sx" | "lx" | "sr" | "lr" | "er" | "zr" | "sr4" | "lr4" => "fiber",
        _ => "unknown",
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn classify_platform(_: &mut proto::NetworkInterface) {}

pub(super) fn probe(endpoint: SocketAddr) -> Result<serde_json::Value, String> {
    let start = Instant::now();
    let stream = TcpStream::connect_timeout(&endpoint, Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let local = stream.local_addr().map_err(|error| error.to_string())?;
    drop(stream);
    Ok(serde_json::json!({
        "endpoint": endpoint.to_string(),
        "tcp_connect_rtt_ms": elapsed,
        "local_interface": json(interface(local.ip())),
    }))
}

#[cfg(test)]
#[path = "network/tests.rs"]
mod tests;
