use super::{
    device::{self, Result, Task},
    plan,
};
use mdma::{
    backend::Backend,
    gguf::GgufModel,
    inference::{self, Generated},
    model::planning,
    tokenizer::Tokenizer,
};
use std::path::PathBuf;

#[derive(Debug, usage_rs::Args)]
pub(super) struct Args {
    #[usage(value_hint = usage_rs::ValueHint::FilePath)]
    model: PathBuf,
    prompt: String,
    #[usage(long, default = "cpu")]
    backend: String,
    #[usage(long, default = "32")]
    max_tokens: usize,
    /// Eager CPU-prefix/GPU-suffix placement, N or auto; llama/internlm2 only.
    #[usage(long)]
    gpu_layers: Option<String>,
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
    type Output = Generated;
    fn run<B: Backend>(self, b: B) -> Result<Generated> {
        let mut g = GgufModel::open(&self.model)?;
        let tokenizer = Tokenizer::from_gguf(&g).ok_or("unsupported or invalid tokenizer")?;
        let prompt = tokenizer.try_encode(&self.prompt)?.len();
        let context = prompt
            .checked_add(self.max_tokens)
            .ok_or("context overflow")?;
        if self.max_tokens == 0 {
            return Err("--max-tokens must be positive".into());
        }
        let ram = plan::host_budget(self.ram_bytes)?;
        let reserve = device::mib(self.reserve_mib)?;
        let cpu = self.backend == "cpu";
        let vram = if cpu || self.gpu_layers.as_deref() == Some("0") {
            0
        } else {
            plan::device_budget(&b, self.vram_bytes, ram)?
        };
        let count = if let Some(count) = &self.gpu_layers {
            if cpu && count != "0" {
                return Err("--gpu-layers needs a GPU backend; CPU requires zero".into());
            }
            let p = plan::placement(&b, &g, count, context, prompt, ram, vram, reserve)?;
            if !p.fits(ram, vram, reserve)? {
                return Err("layer placement does not fit RAM/device budgets".into());
            }
            Some(p.gpu_layers)
        } else {
            let e = planning::estimate(&b, &g, context, prompt)?;
            let (host, device) = plan::ordinary_need(&b, &e, cpu, reserve)?;
            if host > ram || (!cpu && plan::sum([device, reserve])? > vram) {
                return Err("model/request does not fit RAM/device budgets; try --gpu-layers auto for llama/internlm2".into());
            }
            None
        };
        inference::generate_raw(&b, &mut g, &self.prompt, self.max_tokens, count)
            .map_err(Into::into)
    }
}

pub(super) fn run(args: &Args) -> Result<()> {
    let generated = device::dispatch(&args.backend, args)?;
    if args.json {
        println!("{}", serde_json::to_string(&generated)?);
    } else {
        println!("{}", generated.text);
    }
    Ok(())
}
