use super::super::report::{Accelerator, Gpu, PcieLink};

pub(super) fn address(selector: &str) -> Option<String> {
    let parts: Vec<_> = selector
        .strip_prefix("pci")?
        .trim_end_matches(':')
        .split(':')
        .map(str::parse::<u32>)
        .collect::<Result<_, _>>()
        .ok()?;
    let [domain, bus, slot, function] = parts.as_slice() else {
        return None;
    };
    if *domain > u32::from(u16::MAX) || *bus > 255 || *slot > 31 || *function > 7 {
        return None;
    }
    Some(format!("{domain:04x}:{bus:02x}:{slot:02x}.{function:x}"))
}

fn hex(header: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}=");
    let value = header
        .split_whitespace()
        .find_map(|s| s.strip_prefix(&prefix))?;
    Some(format!(
        "0x{:04x}",
        u16::from_str_radix(value.trim_start_matches("0x"), 16).ok()?
    ))
}

fn pair(value: &str) -> Option<(f64, f64)> {
    let (current, max) = value.split_once('(')?;
    let current = current.trim_start_matches('x').parse::<f64>().ok()?;
    let max = max
        .trim_end_matches(')')
        .trim_start_matches('x')
        .parse::<f64>()
        .ok()?;
    (current.is_finite() && max.is_finite()).then_some((current, max))
}

#[expect(
    clippy::cast_sign_loss,
    reason = "widths are range-checked to 1..=32 before the cast"
)]
fn links(line: &str) -> Option<(PcieLink, PcieLink)> {
    let (_, rest) = line.split_once("link x")?;
    let mut words = rest.split_whitespace();
    let (current_width, max_width) = pair(words.next()?)?;
    let speed = if words.next() == Some("speed") {
        words.next().and_then(pair)
    } else {
        None
    };
    let width = |n: f64| ((1.0..=32.0).contains(&n) && n.fract() == 0.0).then_some(n as u32);
    Some((
        PcieLink {
            width: width(current_width),
            speed_gt_s: speed.map(|v| v.0).filter(|v| *v > 0.0),
        },
        PcieLink {
            width: width(max_width),
            speed_gt_s: speed.map(|v| v.1).filter(|v| *v > 0.0),
        },
    ))
}

pub(super) fn parse(text: &str) -> Vec<Gpu> {
    let mut output = Vec::new();
    let mut current: Option<Gpu> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some(gpu) = current.take() {
                output.push(gpu);
            }
            let Some((driver, rest)) = line.split_once('@') else {
                continue;
            };
            let class = rest
                .split_whitespace()
                .find_map(|word| word.strip_prefix("class="))
                .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            if class.is_none_or(|v| v >> 16 != 0x03) {
                continue;
            }
            let Some(address) = rest.split_whitespace().next().and_then(address) else {
                continue;
            };
            let driver = driver.trim_end_matches(|c: char| c.is_ascii_digit());
            current = Some(Gpu {
                pci_address: Some(address),
                bus: Some("pci".into()),
                vendor_id: hex(rest, "vendor"),
                device_id: hex(rest, "device"),
                driver: (!matches!(driver, "vgapci" | "none")).then(|| driver.to_string()),
                ..Gpu::default()
            });
        } else if let Some(gpu) = current.as_mut() {
            if let Some((key, value)) = line.trim().split_once('=') {
                let value = value.trim().trim_matches('\'').to_string();
                match key.trim() {
                    "vendor" => gpu.manufacturer = Some(value),
                    "device" => gpu.model = Some(value),
                    _ => {}
                }
            }
            if let Some((current, max)) = links(line) {
                gpu.current_link = current;
                gpu.max_link = max;
            }
        }
    }
    if let Some(gpu) = current {
        output.push(gpu);
    }
    output
}

/// Coral Edge TPU records, whatever PCI class the card reports.
pub(super) fn coral(text: &str) -> Vec<Accelerator> {
    let mut output = Vec::new();
    let mut current: Option<Accelerator> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            output.extend(current.take());
            let Some((device, rest)) = line.split_once('@') else {
                continue;
            };
            if hex(rest, "vendor").as_deref() != Some("0x1ac1")
                || hex(rest, "device").as_deref() != Some("0x089a")
            {
                continue;
            }
            let Some(address) = rest.split_whitespace().next().and_then(address) else {
                continue;
            };
            let driver = device.trim_end_matches(|c: char| c.is_ascii_digit());
            let unit = &device[driver.len()..];
            current = Some(Accelerator {
                kind: "coral_edge_tpu",
                bus: "pci",
                address: Some(address),
                vendor_id: "0x1ac1".into(),
                device_id: "0x089a".into(),
                driver: (driver != "none").then(|| driver.to_string()),
                device_node: (driver == "apex").then(|| format!("/dev/apex{unit}")),
                ..Accelerator::default()
            });
        } else if let Some(accelerator) = current.as_mut()
            && let Some((now, max)) = links(line)
        {
            accelerator.current_link = now;
            accelerator.max_link = max;
        }
    }
    output.extend(current);
    output
}
