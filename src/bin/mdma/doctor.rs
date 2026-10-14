use super::{
    device::{self, Result, Task},
    info,
};
use mdma::backend::{Backend, Operation, Pool, QWeight};
use mdma::gguf::GgmlDType;
use serde::Serialize;

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    /// Check every backend this target can host, or one: cpu and metal on macOS;
    /// cpu, vulkan, rocm, cuda, level0 or coral on FreeBSD and Linux; cpu,
    /// vulkan, rocm, cuda or level0 elsewhere.
    #[usage(long, default = "all")]
    backend: String,
    #[usage(long)]
    json: bool,
}

#[derive(Serialize)]
struct Capabilities {
    device: Option<String>,
    unified_memory: bool,
    available_device_bytes: Option<usize>,
    kv_element_bytes: usize,
    max_head_dim: Option<usize>,
    weight_cache_profile: Option<String>,
    operations: Vec<(&'static str, bool)>,
}

#[derive(Serialize)]
struct Check {
    backend: String,
    status: &'static str,
    detail: String,
    capabilities: Option<Capabilities>,
}

struct Probe;

impl Task for Probe {
    type Output = Capabilities;
    fn run<B: Backend>(self, b: B) -> Result<Capabilities> {
        let input = [1f32, 2., 3., 4.];
        let mut x = b.alloc(16, Pool::Scratch)?;
        let mut copy = b.alloc(16, Pool::Scratch)?;
        let mut y = b.alloc(16, Pool::Scratch)?;
        b.upload(&mut x, 0, bytemuck::cast_slice(&input))?;
        b.copy(&x, 0, &mut copy, 0, 16)?;
        let identity: Vec<f32> = (0..16)
            .map(|i| if i / 4 == i % 4 { 1. } else { 0. })
            .collect();
        let weight = b.upload_weight(
            bytemuck::cast_slice(&identity),
            mdma::cache::WeightLayout::Gguf,
        )?;
        b.qmatmul(
            &QWeight {
                dtype: GgmlDType::F32,
                shape: vec![4, 4],
                buf: weight,
            },
            &copy,
            &mut y,
        )?;
        let mut actual = [0f32; 4];
        b.download(&y, 0, bytemuck::cast_slice_mut(&mut actual))?;
        if actual != input {
            return Err(
                "allocation/transfer/identity-matmul smoke test returned incorrect data".into(),
            );
        }
        let ops = [
            ("scale", Operation::Scale),
            ("gelu_tanh_mul", Operation::GeluTanhMul),
            ("softcap", Operation::Softcap),
            ("rope_split_half", Operation::RopeSplitHalf),
            ("windowed_attention", Operation::WindowedAttention),
            ("add_bias", Operation::AddBias),
            (
                "rope_partial_interleaved",
                Operation::RopePartialInterleaved,
            ),
            ("rope_partial_split_half", Operation::RopePartialSplitHalf),
            ("sigmoid_mul", Operation::SigmoidMul),
            ("gated_delta_net", Operation::GatedDeltaNet),
        ];
        Ok(Capabilities {
            device: b.device_label(),
            unified_memory: b.unified_memory(),
            available_device_bytes: b.available_device_bytes()?,
            kv_element_bytes: b.kv_elem_bytes(),
            max_head_dim: b.max_head_dim(),
            weight_cache_profile: b.weight_cache_profile().map(str::to_owned),
            operations: ops
                .into_iter()
                .map(|(name, op)| (name, b.supports(op)))
                .collect(),
        })
    }
}

pub(super) fn run(args: &Args) -> Result<()> {
    if args.backend != "all" && !device::NAMES.contains(&args.backend.as_str()) {
        return Err(format!("unknown backend {:?}", args.backend).into());
    }
    let inventory = info::collect(&info::Args {
        json: false,
        dmi_file: None,
    })?;
    let names = if args.backend == "all" {
        device::NAMES.to_vec()
    } else {
        vec![args.backend.as_str()]
    };
    let checks: Vec<_> = names
        .into_iter()
        .map(|name| {
            if !device::built(name) {
                return Check {
                    backend: name.into(),
                    status: "not_built",
                    detail: format!("rebuild with --features {name}"),
                    capabilities: None,
                };
            }
            match device::dispatch(name, Probe) {
                Ok(caps) => Check {
                    backend: name.into(),
                    status: "ready",
                    detail:
                        "allocation, upload, copy, F32 matrix multiplication and download passed"
                            .into(),
                    capabilities: Some(caps),
                },
                Err(error) => Check {
                    backend: name.into(),
                    status: "failed",
                    detail: error.to_string(),
                    capabilities: None,
                },
            }
        })
        .collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1, "os": inventory.os, "architecture": inventory.architecture,
                "checks": checks, "detected_gpus": inventory.gpus,
                "detected_accelerators": inventory.accelerators, "inventory_warnings": inventory.warnings,
            }))?
        );
    } else {
        for check in &checks {
            println!("{}: {} — {}", check.backend, check.status, check.detail);
            if let Some(caps) = &check.capabilities {
                println!(
                    "  device: {} / KV: {} bytes / available device budget: {:?} bytes / unified RAM: {}",
                    caps.device.as_deref().unwrap_or("not exposed"),
                    caps.kv_element_bytes,
                    caps.available_device_bytes,
                    caps.unified_memory
                );
            }
        }
        for accelerator in &inventory.accelerators {
            let backend = if accelerator.kind == "coral_edge_tpu" && device::built("coral") {
                "served by the coral backend"
            } else {
                "no compute backend yet"
            };
            println!(
                "{}: detected — {} {} via {}, {backend}",
                accelerator.kind,
                accelerator.bus,
                accelerator.address.as_deref().unwrap_or("unknown address"),
                accelerator.driver.as_deref().unwrap_or("no driver")
            );
        }
        for warning in &inventory.warnings {
            println!("note: {warning}");
        }
    }
    if checks.iter().any(|c| c.status == "failed")
        || (args.backend != "all" && checks[0].status == "not_built")
    {
        return Err("requested backend checks failed; see diagnostics above".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "doctor/tests.rs"]
mod tests;
