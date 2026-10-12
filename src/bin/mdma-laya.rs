// Laya typed decisions on the CPU backend, one request per run.
//   MDMA_LAYA_PATH   checkpoint directory (model.safetensors, rl_agent_config.json, encoder/, tokenizer/)
// Usage: mdma-laya [request.json]   - or the request on stdin:
//   {"state": <string|object|array>, "questions": {"<id>": {"type", "instructions", "criteria"}}}
use mdma::backend::cpu::CpuBackend;
use mdma::laya::Predictor;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(
        std::env::var("MDMA_LAYA_PATH").map_err(|_| "missing required env var MDMA_LAYA_PATH")?,
    );
    let request = if let Some(path) = std::env::args().nth(1) {
        std::fs::read_to_string(path)?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let request: Value = serde_json::from_str(&request)?;
    let state = request.get("state").ok_or("request needs a \"state\"")?;
    let questions = request
        .get("questions")
        .and_then(Value::as_object)
        .ok_or("request needs a \"questions\" object")?;

    let started = std::time::Instant::now();
    let predictor = Predictor::load(CpuBackend, &dir)?;
    eprintln!("loaded {} in {:.1?}", dir.display(), started.elapsed());
    let started = std::time::Instant::now();
    let result = predictor.predict(state, questions)?;
    eprintln!(
        "predicted {} questions in {:.1?}",
        questions.len(),
        started.elapsed()
    );
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
