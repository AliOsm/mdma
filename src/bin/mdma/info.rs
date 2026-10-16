//! Read-only hardware inventory; no model or GPU backend initialization.
use std::path::PathBuf;

#[path = "info/common.rs"]
mod common;
#[path = "info/dmi.rs"]
mod dmi;
#[cfg(target_os = "linux")]
#[path = "info/linux.rs"]
mod platform;
#[cfg(target_os = "freebsd")]
#[path = "info/freebsd.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "info/macos.rs"]
mod platform;
#[path = "info/render.rs"]
mod render;
#[path = "info/report.rs"]
pub(super) mod report;

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    /// Emit a machine-readable report with explicit units and nulls for unavailable data.
    #[usage(long)]
    pub(super) json: bool,
    /// Read saved `dmidecode --type 17,9` text instead of invoking dmidecode.
    #[usage(long, value_hint = usage_rs::ValueHint::FilePath)]
    pub(super) dmi_file: Option<PathBuf>,
}

pub(super) fn collect(args: &Args) -> Result<report::Report, Box<dyn std::error::Error>> {
    let mut report = platform::collect(args.dmi_file.as_deref())?;
    let isa = mdma::backend::cpu::isa();
    report.cpu.instruction_sets.clone_from(&isa.features);
    report.cpu.kernel_level = isa.level.name();
    Ok(report)
}

pub(super) fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let report = collect(args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render::text(&report));
    }
    Ok(())
}
