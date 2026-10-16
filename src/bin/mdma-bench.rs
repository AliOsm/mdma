//! Isolated decoder benchmark. Run one fresh process per backend/model.
use mdma::backend::{Backend, select};
use mdma::{
    gguf::GgufModel, memory::NativeMemoryBudget, model::Model, sampler::greedy,
    tokenizer::Tokenizer,
};
use serde_json::json;
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn finite(logits: &[f32]) -> Result<()> {
    if logits.is_empty() || logits.iter().any(|x| !x.is_finite()) {
        return Err("nonfinite or empty logits".into());
    }
    Ok(())
}

fn run<B: Backend>(backend: &B, name: &str, init_seconds: f64, args: &[String]) -> Result<()> {
    let prompt_len: usize = args[1].parse()?;
    let steps: usize = args[2].parse()?;
    let repetitions: usize = args[3].parse()?;
    let execution = std::env::var("MDMA_BENCH_DECODER").unwrap_or_else(|_| "eager".into());
    if !matches!(execution.as_str(), "eager" | "executor") {
        return Err("MDMA_BENCH_DECODER must be eager or executor".into());
    }
    let capacity = prompt_len.checked_add(steps).ok_or("context overflow")?;
    if prompt_len == 0 || steps == 0 || repetitions == 0 || capacity > 2048 {
        return Err(
            "positive prompt/decode/repetition counts required; context capped at 2048".into(),
        );
    }
    let load = Instant::now();
    let mut gguf = GgufModel::open(&args[0])?;
    let architecture = gguf
        .metadata_str("general.architecture")
        .ok_or("missing architecture")?
        .to_string();
    let tokenizer = Tokenizer::from_gguf(&gguf).ok_or("unsupported tokenizer")?;
    let artifact_bytes = std::fs::metadata(&args[0])?.len();
    // Counts logical native tensors only, not host RSS or driver allocations.
    let budget = NativeMemoryBudget::new(usize::MAX);
    let model = Model::load_with_budget(backend, &mut gguf, &budget)?;
    drop(gguf);
    let load_seconds = load.elapsed().as_secs_f64();
    let weight_bytes = budget.used();
    if capacity > model.config().n_ctx {
        return Err("request exceeds model context".into());
    }
    let seed = tokenizer.encode("The capital of France is Paris.\n");
    if seed.is_empty() {
        return Err("empty seed".into());
    }
    let tokens: Vec<_> = (0..prompt_len).map(|i| seed[i % seed.len()]).collect();
    let kv_bytes = model.kv_bytes(backend, capacity)?;
    let prefill_bytes = model.workspace_bytes(prompt_len)?;
    let decode_bytes = model.workspace_bytes(1)?;
    let mut trials = Vec::new();
    // One complete untimed warmup, then independent fresh-KV trials.
    for trial in 0..=repetitions {
        let mut kv = model.new_kv_cache(backend, capacity)?;
        let mut prefill = model.alloc_workspace(backend, prompt_len)?;
        let start = Instant::now();
        let mut logits = model.prefill(backend, &mut kv, &mut prefill, &tokens, 0)?;
        let prefill_seconds = start.elapsed().as_secs_f64();
        finite(&logits)?;
        drop(prefill);
        let mut decode = model.alloc_workspace(backend, 1)?;
        // Executor setup is outside the timer, just like workspace allocation.
        // Fixed steps, including logits download and greedy argmax.
        // EOS does not shorten a benchmark.
        let decode_seconds = if execution == "executor" {
            let mut decoder = backend.decoder(&model, &mut kv, &mut decode)?;
            let start = Instant::now();
            for i in 0..steps {
                logits = decoder.forward(greedy(&logits), (prompt_len + i) as u32)?;
            }
            start.elapsed().as_secs_f64()
        } else {
            let start = Instant::now();
            for i in 0..steps {
                logits = model.forward(
                    backend,
                    &mut kv,
                    &mut decode,
                    greedy(&logits),
                    (prompt_len + i) as u32,
                )?;
            }
            start.elapsed().as_secs_f64()
        };
        finite(&logits)?;
        assert_eq!(kv.len(), capacity);
        if trial > 0 {
            trials.push(json!({
                "prefill_seconds": prefill_seconds, "decode_seconds": decode_seconds,
                "prefill_tokens_per_second": prompt_len as f64 / prefill_seconds,
                "decode_tokens_per_second": steps as f64 / decode_seconds,
            }));
        }
    }
    println!(
        "{}",
        json!({
            "backend": name, "architecture": architecture, "model": args[0], "execution": execution,
            "cpu_matmul": mdma::backend::cpu::matmul().name(),
            "cpu_kernels": mdma::backend::cpu::isa().level.name(),
            "cpu_instruction_sets": &mdma::backend::cpu::isa().features,
            "artifact_bytes": artifact_bytes, "backend_init_seconds": init_seconds,
            "load_seconds": load_seconds, "prompt_tokens": prompt_len, "decode_steps": steps,
            "context_capacity": capacity, "kv_element_bytes": backend.kv_elem_bytes(),
            "native_weight_bytes": weight_bytes, "native_kv_bytes": kv_bytes,
            "native_prefill_workspace_bytes": prefill_bytes, "native_decode_workspace_bytes": decode_bytes,
            "native_peak_tensor_bytes": weight_bytes + kv_bytes + prefill_bytes.max(decode_bytes),
            "warmup_trials": 1, "trials": trials,
        })
    );
    Ok(())
}

struct Bench<'a> {
    name: &'a str,
    start: Instant,
    args: &'a [String],
}

impl select::Task for Bench<'_> {
    type Output = ();
    fn run<B: Backend>(self, backend: B) -> Result<()> {
        run(
            &backend,
            self.name,
            self.start.elapsed().as_secs_f64(),
            self.args,
        )
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: mdma-bench MODEL.gguf PROMPT_TOKENS DECODE_STEPS REPETITIONS".into());
    }
    let name = std::env::var("MDMA_BACKEND").unwrap_or_else(|_| "cpu".into());
    let bench = Bench {
        name: &name,
        start: Instant::now(),
        args: &args,
    };
    select::dispatch(&name, bench)
}
