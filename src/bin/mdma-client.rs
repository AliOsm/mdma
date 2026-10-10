// Minimal mTLS gRPC client for a running `mdma-serve`: streams a completion to
// stdout, or prints Introspect when given no prompt. Env, like the server:
//   MDMA_ADDR=host:port            (default localhost:50051; must match the server cert's SAN)
//   MDMA_TLS_CERT / MDMA_TLS_KEY   the client cert/key mdma-certs wrote (client.pem, client-key.pem)
//   MDMA_TLS_CA                    ca.pem
//   MDMA_MAX_TOKENS                default 64
//   MDMA_MODEL                     registry name; empty means the server's default
//   MDMA_READY_TIMEOUT_SECS        default 120; 0 makes one bounded Introspect check
//   MDMA_GREEDY                    1 requests greedy decoding without repetition penalties; default 0
//   MDMA_RPC_TIMEOUT_MS            optional 1..300000; required for federation routes
// Usage: mdma-client [prompt words...]
//        mdma-client --decide request.json   ({"state": ..., "questions": {...}} to a Laya model)
use mdma::grpc::client::mtls_client;
use mdma::grpc::{DecideRequest, IntrospectRequest, SynthesizeRequest};
use rama::error::BoxError;
use std::io::Write;
use tokio_stream::StreamExt;

#[path = "mdma-client/readiness.rs"]
mod readiness;
#[path = "mdma-client/sampling.rs"]
mod sampling;
#[path = "mdma-client/deadline.rs"]
mod deadline;

fn env(name: &str) -> Result<String, BoxError> {
    std::env::var(name).map_err(|_| format!("missing required env var {name}").into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BoxError> {
    let addr = std::env::var("MDMA_ADDR").unwrap_or_else(|_| "localhost:50051".to_string());
    let max_tokens: u32 = std::env::var("MDMA_MAX_TOKENS").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
    let ready_timeout = readiness::timeout(|name| std::env::var(name))?;
    let sampler = sampling::config(|name| std::env::var(name))?;
    let rpc_timeout = deadline::timeout(|name| std::env::var(name))?;
    let client = mtls_client(
        &format!("https://{addr}"),
        env("MDMA_TLS_CERT")?.as_ref(),
        env("MDMA_TLS_KEY")?.as_ref(),
        env("MDMA_TLS_CA")?.as_ref(),
    )?;
    let model = std::env::var("MDMA_MODEL").unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let decision = match args.as_slice() {
        [flag, path] if flag == "--decide" => {
            let request: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            let state_json = request.get("state").ok_or("request needs a \"state\"")?.to_string();
            let questions_json = request.get("questions").ok_or("request needs a \"questions\" object")?.to_string();
            Some(DecideRequest { model: model.clone(), state_json, questions_json })
        }
        _ => None,
    };
    let prompt: String = args.join(" ");
    let selected = if prompt.is_empty() && model.is_empty() { None } else { Some(model.as_str()) };
    let h = readiness::wait(
        || async { Ok(client.introspect(IntrospectRequest {}).await?.into_inner()) },
        selected,
        ready_timeout,
    ).await?;

    if let Some(request) = decision {
        let reply = client.decide(deadline::request(request, rpc_timeout)?).await?.into_inner();
        let result: serde_json::Value = serde_json::from_str(&reply.result_json)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if prompt.is_empty() {
        println!("ready={} state={} backend={} default={}", h.ready, h.server_state, h.backend, h.default_model);
        for m in h.models {
            let size = if m.weight_bytes > 0 { format!(" {} MB", m.weight_bytes >> 20) } else { String::new() };
            let target = if m.upstream.is_empty() { m.backend } else { m.upstream };
            println!("  {:<20} {:<9} {:<10} ready={} arch={} n_ctx={}{size}", m.name, m.kind, target, m.ready, m.architecture, m.n_ctx);
        }
        return Ok(());
    }
    let mut stream = client.synthesize(deadline::request(
        SynthesizeRequest { prompt, max_tokens, sampler, model }, rpc_timeout,
    )?).await?.into_inner();
    let mut out = std::io::stdout();
    let started = std::time::Instant::now();
    let mut n = 0u32;
    while let Some(item) = stream.next().await {
        let token = item?;
        out.write_all(token.text.as_bytes())?;
        out.flush()?;
        n += 1;
    }
    let secs = started.elapsed().as_secs_f64();
    eprintln!("\n[{n} tokens in {secs:.2}s, {:.1} tok/s]", f64::from(n) / secs);
    Ok(())
}
