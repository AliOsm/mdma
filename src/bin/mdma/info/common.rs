use super::report::{Memory, MemorySpeed};
use std::{collections::BTreeMap, process::Command};
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

pub(super) fn command(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("command failed");
        return Err(format!("{program}: {detail}"));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{program}: {error}"))
}

#[cfg(target_os = "linux")]
pub(super) fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn fields(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect()
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
pub(super) fn sysctls(names: &[&str]) -> BTreeMap<String, String> {
    names
        .iter()
        .filter_map(|name| {
            Some((
                name.to_string(),
                command("sysctl", &["-n", name]).ok()?.trim().to_string(),
            ))
        })
        .collect()
}

pub(super) fn known(value: &str) -> Option<String> {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        ""
        | "unknown"
        | "not specified"
        | "none"
        | "n/a"
        | "not available"
        | "to be filled by o.e.m."
        | "no dimm"
        | "empty" => None,
        _ => Some(value.into()),
    }
}

#[expect(clippy::cast_sign_loss, reason = "the value is checked finite and within 0..u64::MAX before the cast")]
pub(super) fn capacity(value: &str) -> Option<u64> {
    let mut words = value.split_whitespace();
    let number = words.next()?;
    let unit = match words.next()? {
        "B" | "bytes" => 1_u64,
        "kB" | "KB" | "KiB" => 1024,
        "MB" | "MiB" => 1024_u64.pow(2),
        "GB" | "GiB" => 1024_u64.pow(3),
        "TB" | "TiB" => 1024_u64.pow(4),
        _ => return None,
    };
    if let Ok(integer) = number.parse::<u64>() {
        return integer.checked_mul(unit);
    }
    let value = number.parse::<f64>().ok()? * unit as f64;
    (value.is_finite() && value >= 0.0 && value < u64::MAX as f64).then_some(value as u64)
}

pub(super) fn memory_speed(value: &str) -> Option<MemorySpeed> {
    let mut words = value.split_whitespace();
    let value = words.next()?.parse::<u64>().ok().filter(|v| *v > 0)?;
    let unit = words.next()?;
    matches!(unit, "MT/s" | "MHz").then(|| MemorySpeed {
        value,
        unit: unit.into(),
    })
}

pub(super) fn installed(memory: &Memory) -> Option<u64> {
    if memory.modules.is_empty() {
        return None;
    }
    memory
        .modules
        .iter()
        .try_fold(0_u64, |sum, module| sum.checked_add(module.size_bytes?))
}

#[cfg(any(target_os = "freebsd", target_os = "macos"))]
pub(super) fn cpu_vendor(model: &str) -> Option<String> {
    if model.contains("Intel") {
        Some("Intel".into())
    } else if model.starts_with("AMD") {
        Some("AMD".into())
    } else if model.starts_with("Apple") {
        Some("Apple".into())
    } else {
        None
    }
}

#[cfg(test)]
#[path = "common/tests.rs"]
mod tests;
