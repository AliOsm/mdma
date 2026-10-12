use super::{
    device::{self, Result, Task},
    info,
};
use mdma::backend::Backend;
use mdma::gguf::GgufModel;
use mdma::model::planning::{self, OffloadPlan};
use std::path::PathBuf;

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    #[usage(value_hint = usage_rs::ValueHint::FilePath)]
    model: PathBuf,
    #[usage(long, default = "cpu")]
    backend: String,
    #[usage(long, default = "512")]
    ctx: usize,
    /// Prefill tokens; defaults to min(32, context).
    #[usage(long)]
    prompt_tokens: Option<usize>,
    /// Contiguous GPU suffix: an integer or auto. Llama/internlm2 only.
    #[usage(long)]
    gpu_layers: Option<String>,
    /// Override OS available RAM, in bytes; not a reservation.
    #[usage(long)]
    ram_bytes: Option<usize>,
    /// Override live driver GPU budget, in bytes; not total installed VRAM.
    #[usage(long)]
    vram_bytes: Option<usize>,
    #[usage(long, default = "256")]
    reserve_mib: usize,
    #[usage(long)]
    json: bool,
}

pub(super) fn host_budget(explicit: Option<usize>) -> Result<usize> {
    if let Some(bytes) = explicit {
        return Ok(bytes);
    }
    let inventory = info::collect(&info::Args {
        json: false,
        dmi_file: None,
    })?;
    inventory
        .memory
        .available_bytes
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "available RAM not exposed; supply --ram-bytes explicitly".into())
}

pub(super) fn sum(values: impl IntoIterator<Item = usize>) -> Result<usize> {
    values
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(|| "byte size overflow".into())
}

pub(super) fn ordinary_need<B: Backend>(
    b: &B,
    e: &planning::Estimate,
    cpu: bool,
    reserve: usize,
) -> Result<(usize, usize)> {
    let device = if cpu {
        0
    } else {
        sum([
            e.weights_bytes,
            e.kv_bytes,
            e.prefill_workspace_bytes.max(e.decode_workspace_bytes),
        ])?
    };
    let host = if cpu {
        e.peak_bytes
    } else if b.unified_memory() {
        sum([device, e.load_staging_bytes, reserve])?
    } else {
        e.load_staging_bytes
    };
    Ok((host, device))
}

pub(super) fn device_budget<B: Backend>(
    b: &B,
    explicit: Option<usize>,
    ram: usize,
) -> Result<usize> {
    if let Some(bytes) = explicit {
        return Ok(bytes);
    }
    if let Some(bytes) = b.available_device_bytes()? {
        return Ok(bytes);
    }
    if b.unified_memory() {
        return Ok(ram);
    }
    Err("live GPU budget not exposed; supply --vram-bytes explicitly (not the advertised total VRAM)".into())
}

#[expect(
    clippy::too_many_arguments,
    reason = "forwards the planner's independent budget inputs"
)]
pub(super) fn placement<B: Backend>(
    b: &B,
    g: &GgufModel,
    count: &str,
    context: usize,
    prompt: usize,
    ram: usize,
    vram: usize,
    reserve: usize,
) -> Result<OffloadPlan> {
    if count == "auto" {
        Ok(planning::auto_offload(
            b, g, context, prompt, ram, vram, reserve,
        )?)
    } else {
        let count = count
            .parse::<usize>()
            .map_err(|_| "--gpu-layers must be a nonnegative integer or auto")?;
        Ok(planning::offload(b, g, context, prompt, count)?)
    }
}

impl Task for &Args {
    type Output = serde_json::Value;
    fn run<B: Backend>(self, b: B) -> Result<Self::Output> {
        let g = GgufModel::open(&self.model)?;
        let ram = host_budget(self.ram_bytes)?;
        let reserve = device::mib(self.reserve_mib)?;
        let cpu = self.backend == "cpu";
        let prompt = self.prompt_tokens.unwrap_or_else(|| 32.min(self.ctx));
        let vram = if cpu || self.gpu_layers.as_deref() == Some("0") {
            0
        } else {
            device_budget(&b, self.vram_bytes, ram)?
        };
        let (estimate, host_need, device_need, fits) = if let Some(count) = &self.gpu_layers {
            if cpu && count != "0" {
                return Err("--gpu-layers needs a GPU backend; CPU requires zero".into());
            }
            let p = placement(&b, &g, count, self.ctx, prompt, ram, vram, reserve)?;
            let fits = p.fits(ram, vram, reserve)?;
            let host = if p.unified_memory {
                sum([
                    p.host.peak_bytes,
                    p.device.peak_bytes,
                    if p.gpu_layers > 0 { reserve } else { 0 },
                ])?
            } else {
                p.host.peak_bytes
            };
            let device = p.device.peak_bytes;
            (serde_json::to_value(p)?, host, device, fits)
        } else {
            let e = planning::estimate(&b, &g, self.ctx, prompt)?;
            let (host, device) = ordinary_need(&b, &e, cpu, reserve)?;
            let device_total = if cpu {
                0
            } else {
                device.checked_add(reserve).ok_or("byte size overflow")?
            };
            let fits = host <= ram && device_total <= vram;
            (serde_json::to_value(e)?, host, device, fits)
        };
        let device_used = !cpu
            && estimate
                .get("gpu_layers")
                .is_none_or(|n| n.as_u64() != Some(0));
        Ok(serde_json::json!({
            "schema_version": 1, "model": self.model, "backend": self.backend,
            "device": b.device_label(), "unified_memory": b.unified_memory(), "fits": fits,
            "ram_budget_bytes": ram, "device_budget_bytes": vram, "device_headroom_bytes": if device_used { reserve } else { 0 },
            "ram_budget_source": if self.ram_bytes.is_some() { "explicit" } else { "OS available RAM snapshot/estimate" },
            "device_budget_source": if !device_used { "not used" } else if self.vram_bytes.is_some() { "explicit" } else if b.unified_memory() { "driver or shared RAM snapshot" } else { "live driver snapshot" },
            "host_required_bytes": host_need, "device_required_bytes": device_need, "estimate": estimate,
            "notes": ["Raw GGUF layout; prepared caches are not attached. Estimates include tensor buffers, bridge and one source tensor during load, not total process RSS, allocator padding, CPU dequantization temporaries or driver/kernel scratch.",
                "Budgets are snapshots, not reservations; other applications can consume memory before loading. Unified GPU memory shares RAM and is never added as extra capacity."],
        }))
    }
}

pub(super) fn run(args: &Args) -> Result<()> {
    let report = device::dispatch(&args.backend, args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Model: {}\nBackend: {}\nFits: {}\nHost required: {} bytes / budget: {} bytes\nDevice required: {} bytes / budget: {} bytes",
            args.model.display(),
            args.backend,
            report["fits"],
            report["host_required_bytes"],
            report["ram_budget_bytes"],
            report["device_required_bytes"],
            report["device_budget_bytes"]
        );
        println!("{}", serde_json::to_string_pretty(&report["estimate"])?);
        println!(
            "Estimates are not total RSS or a memory reservation; use --json for limitations."
        );
    }
    if report["fits"] == false {
        return Err("model/request does not fit the supplied budgets".into());
    }
    Ok(())
}
