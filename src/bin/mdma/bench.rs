use super::{device::{self, Result, Task}, plan};
use mdma::{backend::Backend, memory_bench};

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    /// CPU RAM copy, plus device and transfer copies for a GPU backend.
    #[usage(long, default = "cpu")]
    backend: String,
    /// Per-buffer MiB, capped at 256. Results may include cache bandwidth.
    #[usage(long, default = "16")]
    size_mib: usize,
    #[usage(long, default = "8")]
    iterations: usize,
    #[usage(long)]
    ram_bytes: Option<usize>,
    #[usage(long)]
    vram_bytes: Option<usize>,
    #[usage(long, default = "256")]
    reserve_mib: usize,
    #[usage(long)]
    json: bool,
}

impl Task for &Args {
    type Output = serde_json::Value;
    fn run<B: Backend>(self, b: B) -> Result<Self::Output> {
        let bytes = device::mib(self.size_mib)?;
        memory_bench::validate(bytes, self.iterations)?;
        let ram = plan::host_budget(self.ram_bytes)?;
        let cpu = self.backend == "cpu";
        let reserve = if cpu { 0 } else { device::mib(self.reserve_mib)? };
        // Reserve one possible staging buffer in each domain. Backends choose
        // where staging lives; unified memory intentionally stays conservative.
        let host = plan::sum([bytes, bytes, if cpu { 0 } else { bytes }])?;
        let gpu = if cpu { 0 } else { plan::sum([bytes, bytes, bytes])? };
        let host_need = if b.unified_memory() { plan::sum([host, gpu, reserve])? } else { host };
        if host_need > ram { return Err("memory bench buffers exceed available RAM budget".into()); }
        if !cpu && plan::sum([gpu, reserve])? > plan::device_budget(&b, self.vram_bytes, ram)? {
            return Err("memory bench buffers exceed live device budget".into());
        }
        let mut measurements = vec![memory_bench::host(bytes, self.iterations)?];
        if !cpu { measurements.extend(memory_bench::device(&b, bytes, self.iterations)?); }
        Ok(serde_json::json!({
            "schema_version": 1, "backend": self.backend, "device": b.device_label(),
            "unified_memory": b.unified_memory(), "measurements": measurements,
            "notes": ["Decimal payload GB/s, not read+write bus traffic. Allocation and full byte verification are outside timing; completion is included. Pageable host transfers include backend staging overhead.",
                "Cache-sized RAM copies are not a DRAM bandwidth measurement. Device copies on unified-memory GPUs are not separate VRAM capacity or PCIe bandwidth.",
                "Level Zero's shared-buffer copy APIs currently use CPU memcpy, not a GPU copy engine."],
        }))
    }
}

pub(super) fn run(args: &Args) -> Result<()> {
    let report = device::dispatch(&args.backend, args)?;
    if args.json { println!("{}", serde_json::to_string_pretty(&report)?); }
    else {
        println!("{}: {}", args.backend, report["device"]);
        for row in report["measurements"].as_array().ok_or("bench report has no measurements")? {
            let operation = row["operation"].as_str().ok_or("measurement without an operation")?;
            let rate = row["payload_gb_per_second"].as_f64().ok_or("measurement without a payload rate")?;
            println!("{operation}: {rate:.2} GB/s (payload)");
        }
        println!("Cache effects and staging are included; unified GPU memory is shared RAM.");
    }
    Ok(())
}
