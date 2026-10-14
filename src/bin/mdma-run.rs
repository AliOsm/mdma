//! Local, deterministic model smoke runner. Serving still uses mTLS.
use mdma::backend::{Backend, select};
use mdma::{gguf::GgufModel, inference};

fn run<B: Backend>(
    backend: &B,
    path: &str,
    prompt: &str,
    max: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut gguf = GgufModel::open(path)?;
    // Compatibility runner accepts an explicit suffix. Auto placement and
    // budget preflight live in `mdma run --gpu-layers auto`.
    let count = std::env::var("MDMA_GPU_LAYERS")
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?;
    let generated = inference::generate(backend, &mut gguf, prompt, max, count)?;
    if std::env::var_os("MDMA_JSON").is_some() {
        println!("{}", serde_json::to_string(&generated)?);
    } else {
        println!("{}", generated.text);
    }
    Ok(())
}

struct Run<'a> {
    path: &'a str,
    prompt: &'a str,
    max: usize,
}

impl select::Task for Run<'_> {
    type Output = ();
    fn run<B: Backend>(self, backend: B) -> select::Result<()> {
        run(&backend, self.path, self.prompt, self.max)
    }
}

fn main() -> select::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: mdma-run MODEL.gguf PROMPT [MAX_TOKENS]".into());
    }
    let max = args.get(2).map(|n| n.parse()).transpose()?.unwrap_or(32);
    let run = Run {
        path: &args[0],
        prompt: &args[1],
        max,
    };
    select::dispatch(
        std::env::var("MDMA_BACKEND").as_deref().unwrap_or("cpu"),
        run,
    )
}
