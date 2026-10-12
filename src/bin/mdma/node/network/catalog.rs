//! Source-backed, versioned ceilings. Never updates observed link measurements.
use super::super::proto;
use serde::Deserialize;

const DATA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/network-cards.toml"
));

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_field_names,
    reason = "fields mirror the TOML schema keys"
)]
struct Catalog {
    schema_version: u32,
    catalog_version: String,
    #[serde(default)]
    hardware: Vec<Hardware>,
    #[serde(default)]
    driver_limits: Vec<DriverLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hardware {
    id: String,
    bus: String,
    vendor: String,
    device: String,
    subsystem_vendor: Option<String>,
    subsystem_device: Option<String>,
    revision: Option<String>,
    model: String,
    #[serde(default)]
    standards: Vec<String>,
    max_phy_bps: Option<u64>,
    sources: Vec<String>,
    notes: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverLimit {
    id: String,
    os: String,
    os_name: Option<String>,
    driver: String,
    os_min: String,
    os_before: String,
    kernel_revision: Option<String>,
    bus: Option<String>,
    vendor: Option<String>,
    device: Option<String>,
    #[serde(default)]
    standards: Vec<String>,
    max_phy_bps: Option<u64>,
    sources: Vec<String>,
    notes: String,
}

fn version(value: &str) -> Option<[u32; 3]> {
    // Accept OS suffixes (14.3-RELEASE-p2), not lexical version comparison.
    let base = value.split('-').next()?;
    let mut result = [0; 3];
    let parts: Vec<_> = base.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (i, part) in parts.into_iter().enumerate() {
        result[i] = part.parse().ok()?;
    }
    Some(result)
}

fn hex(value: &str, width: usize) -> bool {
    value.len() == width
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl Catalog {
    fn parse(data: &str) -> Result<Self, String> {
        let catalog: Self = toml::from_str(data).map_err(|error| error.to_string())?;
        if catalog.schema_version != 1 || catalog.catalog_version.is_empty() {
            return Err("unsupported or missing catalog version".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for entry in &catalog.hardware {
            if !ids.insert(&entry.id)
                || entry.id.is_empty()
                || entry.model.is_empty()
                || !matches!(entry.bus.as_str(), "pci" | "usb" | "apple_airport")
                || !hex(&entry.vendor, 4)
                || !hex(&entry.device, 4)
                || entry
                    .subsystem_vendor
                    .as_deref()
                    .is_some_and(|v| !hex(v, 4))
                || entry
                    .subsystem_device
                    .as_deref()
                    .is_some_and(|v| !hex(v, 4))
                || entry.revision.as_deref().is_some_and(|v| !hex(v, 2))
                || entry.max_phy_bps == Some(0)
                || entry.sources.is_empty()
                || entry.sources.iter().any(|v| v.trim().is_empty())
            {
                return Err(format!("invalid hardware entry {}", entry.id));
            }
        }
        for entry in &catalog.driver_limits {
            let min = version(&entry.os_min).ok_or("invalid minimum OS version")?;
            let before = version(&entry.os_before).ok_or("invalid upper OS version")?;
            if !ids.insert(&entry.id)
                || entry.id.is_empty()
                || entry.driver.is_empty()
                || !matches!(entry.os.as_str(), "linux" | "macos" | "freebsd")
                || entry.os == "linux" && entry.os_name.as_deref().is_none_or(str::is_empty)
                || min >= before
                || entry.max_phy_bps == Some(0)
                || entry.sources.is_empty()
                || entry.sources.iter().any(|v| v.trim().is_empty())
                || entry
                    .kernel_revision
                    .as_deref()
                    .is_some_and(|v| v.len() < 12 || !hex(v, v.len()))
                || (entry.bus.is_some() || entry.vendor.is_some() || entry.device.is_some())
                    && !(entry
                        .bus
                        .as_deref()
                        .is_some_and(|v| matches!(v, "pci" | "usb" | "apple_airport"))
                        && entry.vendor.as_deref().is_some_and(|v| hex(v, 4))
                        && entry.device.as_deref().is_some_and(|v| hex(v, 4)))
            {
                return Err(format!("invalid driver entry {}", entry.id));
            }
        }
        Ok(catalog)
    }

    fn hardware(&self, identity: &proto::NetworkHardwareIdentity) -> Option<&Hardware> {
        let matches = self.hardware.iter().filter(|e| {
            e.bus == identity.bus
                && e.vendor == identity.vendor
                && e.device == identity.device
                && e.subsystem_vendor
                    .as_ref()
                    .is_none_or(|v| v == &identity.subsystem_vendor)
                && e.subsystem_device
                    .as_ref()
                    .is_none_or(|v| v == &identity.subsystem_device)
                && e.revision.as_ref().is_none_or(|v| v == &identity.revision)
        });
        unique_best(matches, |e| {
            usize::from(e.subsystem_vendor.is_some())
                + usize::from(e.subsystem_device.is_some())
                + usize::from(e.revision.is_some())
        })
    }

    fn driver(&self, nic: &proto::NetworkInterface, os: &str) -> Option<&DriverLimit> {
        let current = version(&nic.os_version)?;
        let matches = self.driver_limits.iter().filter(|e| {
            e.os == os && e.driver == nic.driver
                && e.os_name.as_ref().is_none_or(|name| name == &nic.os_name)
                && (!(nic.os_version.contains("CURRENT") || nic.os_version.contains("STABLE")
                    || nic.kernel_build.contains("CURRENT") || nic.kernel_build.contains("STABLE"))
                    || e.kernel_revision.is_some())
                // Bounds are validated when the catalog loads.
                && version(&e.os_min).is_some_and(|min| current >= min)
                && version(&e.os_before).is_some_and(|before| current < before)
                && e.kernel_revision.as_ref().is_none_or(|revision| {
                    nic.kernel_build.split(|c: char| !c.is_ascii_hexdigit())
                        .any(|token| token.starts_with(revision))
                })
                && e.bus.as_ref().is_none_or(|bus| nic.hardware_identity.as_ref().is_some_and(|i|
                    bus == &i.bus && e.vendor.as_ref() == Some(&i.vendor)
                        && e.device.as_ref() == Some(&i.device)))
        });
        unique_best(matches, |e| {
            usize::from(e.bus.is_some()) + usize::from(e.kernel_revision.is_some())
        })
    }
}

// Specific entries override generic ones; equal-specificity overlap is unknown.
fn unique_best<'a, T>(
    entries: impl Iterator<Item = &'a T>,
    score: impl Fn(&T) -> usize,
) -> Option<&'a T> {
    let mut best = None;
    let mut best_score = 0;
    let mut tied = false;
    for entry in entries {
        let value = score(entry);
        if best.is_none() || value > best_score {
            best = Some(entry);
            best_score = value;
            tied = false;
        } else if value == best_score {
            tied = true;
        }
    }
    if tied { None } else { best }
}

pub(super) fn apply(nic: &mut proto::NetworkInterface) {
    static CATALOG: std::sync::OnceLock<Result<Catalog, String>> = std::sync::OnceLock::new();
    nic.hardware_capabilities = None;
    nic.driver_capabilities = None;
    let Ok(catalog) = CATALOG.get_or_init(|| Catalog::parse(DATA)) else {
        nic.catalog_status = "invalid_catalog".into();
        return;
    };
    nic.catalog_version.clone_from(&catalog.catalog_version);
    // An emulated ID is not evidence for the physical host's card.
    if nic.visibility == "guest"
        || matches!(
            nic.interface_kind.as_str(),
            "virtual" | "bridge" | "bond" | "loopback"
        )
    {
        nic.catalog_status = "physical_hardware_hidden".into();
        return;
    }
    if let Some(entry) = nic
        .hardware_identity
        .as_ref()
        .and_then(|i| catalog.hardware(i))
    {
        nic.hardware_capabilities = Some(proto::NetworkCapabilityProfile {
            entry_id: entry.id.clone(),
            model: entry.model.clone(),
            standards: entry.standards.clone(),
            max_phy_bps: entry.max_phy_bps,
            sources: entry.sources.clone(),
            notes: entry.notes.clone(),
        });
    }
    if let Some(entry) = catalog.driver(nic, std::env::consts::OS) {
        nic.driver_capabilities = Some(proto::NetworkCapabilityProfile {
            entry_id: entry.id.clone(),
            model: String::new(),
            standards: entry.standards.clone(),
            max_phy_bps: entry.max_phy_bps,
            sources: entry.sources.clone(),
            notes: entry.notes.clone(),
        });
    }
    nic.catalog_status =
        if nic.hardware_capabilities.is_some() || nic.driver_capabilities.is_some() {
            "matched"
        } else {
            "unknown_or_ambiguous"
        }
        .into();
}

pub(super) fn profile(value: Option<proto::NetworkCapabilityProfile>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, |v| {
        serde_json::json!({
            "entry_id": v.entry_id, "model": v.model, "standards": v.standards,
            "max_phy_bps": v.max_phy_bps, "sources": v.sources, "notes": v.notes,
        })
    })
}

#[cfg(test)]
#[path = "catalog/tests.rs"]
mod tests;
