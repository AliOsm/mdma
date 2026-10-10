//! MDMA command-line entry point.
use usage_rs::Cli;

#[path = "mdma/info.rs"]
mod info;
#[path = "mdma/device.rs"]
mod device;
#[path = "mdma/doctor.rs"]
mod doctor;
#[path = "mdma/plan.rs"]
mod plan;
#[path = "mdma/run.rs"]
mod run;
#[path = "mdma/bench.rs"]
mod bench;
#[path = "mdma/node.rs"]
mod node;

#[derive(Debug, Cli)]
#[usage(bin = "mdma", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Command {
    /// Resource-advertisement LAN POC; no inference or model transfer.
    Node(node::Args),
    /// Inspect CPU, RAM, GPUs and their connections without loading a model.
    Info(info::Args),
    /// Diagnose compiled backends with a tiny transfer/compute test.
    Doctor(doctor::Args),
    /// Estimate model memory and CPU/GPU placement without loading weights.
    Plan(plan::Args),
    /// Generate locally, optionally offloading a suffix of layers to the GPU.
    Run(run::Args),
    /// Bounded RAM, device-copy and host/device transfer measurements.
    Bench(bench::Args),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Node(args) => node::run(args),
        Command::Info(args) => info::run(&args),
        Command::Doctor(args) => doctor::run(&args),
        Command::Plan(args) => plan::run(&args),
        Command::Run(args) => run::run(&args),
        Command::Bench(args) => bench::run(&args),
    }
}

#[cfg(test)]
#[path = "mdma/tests.rs"]
mod tests;
