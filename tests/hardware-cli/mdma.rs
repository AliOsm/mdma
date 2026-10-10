//! Minimal native inventory harness, without inference dependencies.
use usage_rs::Cli;

#[path = "../../src/bin/mdma/info.rs"]
mod info;

#[derive(Debug, Cli)]
#[usage(bin = "mdma", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Command {
    Info(info::Args),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Args::parse().command {
        Command::Info(args) => info::run(&args),
    }
}
