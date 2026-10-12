//! Read-only presentation of the same peer reports emitted as JSON.
use serde_json::Value;

fn text(value: &Value) -> String {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("?")
        .to_owned()
}

// Peer TXT/RPC strings are untrusted: no terminal escapes, controls or newlines.
fn cell(value: &str, width: usize) -> String {
    let mut clean: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(width + 1)
        .collect();
    if clean.len() > width {
        clean.truncate(width.saturating_sub(3));
        clean.push_str("...");
    }
    format!("{clean:<width$}")
}

fn row(values: &[String], widths: &[usize]) -> String {
    values
        .iter()
        .zip(widths)
        .map(|(value, width)| cell(value, *width))
        .collect::<Vec<_>>()
        .join("  ")
}

fn capacity(available: &Value, total: &Value) -> String {
    match (available.as_u64(), total.as_u64()) {
        (Some(available), Some(total)) => format!(
            "{:.1}/{:.1}G",
            available as f64 / 1_073_741_824.0,
            total as f64 / 1_073_741_824.0
        ),
        _ => "?".into(),
    }
}

fn speed(value: &Value) -> String {
    match value.as_u64().filter(|v| *v > 0) {
        Some(v) if v >= 1_000_000_000 => format!("{:.1}G", v as f64 / 1_000_000_000.0),
        Some(v) => format!("{:.0}M", v as f64 / 1_000_000.0),
        None => "?".into(),
    }
}

fn stale(value: String, old: bool) -> String {
    if old && value != "?" {
        format!("~{value}")
    } else {
        value
    }
}

fn nic(resources: &Value) -> (&Value, String) {
    let parent = &resources["network_interface"];
    if let Some(children) = parent["underlying_interfaces"].as_array() {
        if children.len() == 1 {
            return (
                &children[0],
                format!("{}/{}", text(&parent["name"]), text(&children[0]["name"])),
            );
        }
        if children.len() > 1 {
            return (
                parent,
                format!("{}+{}members", text(&parent["name"]), children.len()),
            );
        }
    }
    (parent, text(&parent["name"]))
}

fn model(nic: &Value) -> String {
    let label = nic["hardware_capabilities"]["model"]
        .as_str()
        .filter(|v| !v.is_empty());
    if let (Some(vendor), Some(device)) = (
        nic["hardware_identity"]["vendor"].as_str(),
        nic["hardware_identity"]["device"].as_str(),
    ) {
        match label {
            Some(label) => format!("{vendor}:{device} {label}"),
            None => format!("{vendor}:{device}"),
        }
    } else {
        label.unwrap_or("?").into()
    }
}

pub(super) fn render(reports: &[Value]) -> String {
    let mut output = format!(
        "MDMA LAN | {} discovered entries | read-only\n",
        reports.len()
    );
    output.push_str(
        "RAM/DISK: available/total GiB. ~ = retained snapshot, not current. ? = unknown.\n",
    );
    let resource_widths = [16, 12, 5, 16, 20, 8];
    output.push_str(&row(
        &["NODE", "STATUS", "CPU", "RAM A/T", "DISK A/T", "AGE"].map(str::to_owned),
        &resource_widths,
    ));
    output.push('\n');
    for report in reports {
        let resources = &report["resources"];
        let old = report["status"] != "online";
        let age = report["last_success_age_ms"]
            .as_u64()
            .map_or_else(|| "?".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0));
        let cores = resources["logical_cpu_count"]
            .as_u64()
            .map_or_else(|| "?".into(), |v| v.to_string());
        output.push_str(&row(
            &[
                text(&report["node_name"]),
                text(&report["status"]),
                stale(cores, old),
                stale(
                    capacity(
                        &resources["memory_available_bytes"],
                        &resources["memory_total_bytes"],
                    ),
                    old,
                ),
                stale(
                    capacity(
                        &resources["disk_available_bytes"],
                        &resources["disk_total_bytes"],
                    ),
                    old,
                ),
                age,
            ],
            &resource_widths,
        ));
        output.push('\n');
    }
    output.push_str(
        "\nNetwork: LINK is observed; HW/DRV are catalog ceilings (Mbps/Gbps), not throughput.\n",
    );
    output.push_str(
        "NIC parent/member shows a sole configured lower interface, not proof of flow selection.\n",
    );
    let network_widths = [16, 21, 9, 20, 7, 7, 7, 8];
    output.push_str(&row(
        &[
            "NODE",
            "NIC",
            "MEDIUM",
            "MODEL / ID",
            "LINK",
            "HW MAX",
            "DRV MAX",
            "TCP ms",
        ]
        .map(str::to_owned),
        &network_widths,
    ));
    output.push('\n');
    for report in reports {
        let resources = &report["resources"];
        let old = report["status"] != "online";
        let (nic, name) = nic(resources);
        let medium = if nic["visibility"] == "guest" {
            "guest".into()
        } else {
            text(&nic["physical_medium"])
        };
        let rtt = resources["network_observation"]["tcp_connect_rtt_ms"]
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map_or_else(|| "?".into(), |value| format!("{value:.2}"));
        output.push_str(&row(
            &[
                text(&report["node_name"]),
                stale(name, old),
                stale(medium, old),
                stale(model(nic), old),
                stale(speed(&nic["link_speed_bps"]), old),
                stale(speed(&nic["hardware_capabilities"]["max_phy_bps"]), old),
                stale(speed(&nic["driver_capabilities"]["max_phy_bps"]), old),
                stale(rtt, old),
            ],
            &network_widths,
        ));
        output.push('\n');
    }
    output.push_str("\nIdentity: self-reported compatibility metadata, not authenticated.\n");
    let identity_widths = [16, 18, 8, 20, 12];
    output.push_str(&row(
        &["NODE", "MDMA", "PROTOCOL", "BUILD", "STATUS"].map(str::to_owned),
        &identity_widths,
    ));
    output.push('\n');
    for report in reports {
        let identity = &report["identity"];
        let build = identity["build_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map_or_else(
                || "?".into(),
                |build| {
                    if build.len() >= 40 && build.as_bytes()[..40].iter().all(u8::is_ascii_hexdigit)
                    {
                        format!(
                            "{}{}",
                            &build[..12],
                            if build.ends_with("-dirty") {
                                "-dirty"
                            } else {
                                ""
                            }
                        )
                    } else {
                        build.to_owned()
                    }
                },
            );
        output.push_str(&row(
            &[
                text(&report["node_name"]),
                text(&identity["mdma_version"]),
                identity["protocol_version"]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .map_or_else(|| "?".into(), |v| v.to_string()),
                build,
                text(&report["status"]),
            ],
            &identity_widths,
        ));
        output.push('\n');
        if report["status"] == "rejected" {
            output.push_str("  Rejected: ");
            output.push_str(cell(&text(&report["error"]), 100).trim_end());
            output.push('\n');
        }
    }
    if reports.is_empty() {
        output.push_str("Waiting for _mdma._tcp.local. advertisements...\n");
    }
    output
}

#[cfg(test)]
#[path = "table/tests.rs"]
mod tests;
