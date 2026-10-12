//! Federation administration: a node's identity, its peers and their grants,
//! and routes to their models. Changes need `mdma-serve` stopped.
use mdma::federation::admin::{self, Bundle, Grants};
use mdma::federation::config::{ExecuteExport, Operation};
use mdma::registry::Source;
use std::path::{Path, PathBuf};
use usage_rs::Cli;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Cli)]
#[usage(bin = "mdma-fed", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Command {
    /// Create a node: identity, catalog signer, trust store, runtime.toml.
    Init(Init),
    /// Print this node's bundle for its peers to enroll.
    Export(Export),
    /// Trust a peer's bundle and grant it models from the models folder.
    Enroll(Enroll),
    /// Route a peer's ready models, as PREFIX-MODEL, for one local client.
    Routes(Routes),
    /// Show peers, grants and routes; works while mdma-serve runs.
    Status(Status),
}

#[derive(Debug, usage_rs::Args)]
struct Init {
    /// New directory for the node's files.
    #[usage(value_hint = usage_rs::ValueHint::DirPath)]
    dir: PathBuf,
    /// Lowercase DNS-style name, e.g. pathfinder.fleet.
    #[usage(long)]
    id: String,
    /// Where peers reach the federation listener, e.g. https://192.168.3.41:6365.
    #[usage(long)]
    endpoint: String,
    /// Days the certificates and pins stay valid.
    #[usage(long, default = "30")]
    days: u32,
}

#[derive(Debug, usage_rs::Args)]
struct Export {
    #[usage(value_hint = usage_rs::ValueHint::DirPath)]
    dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
struct Enroll {
    #[usage(value_hint = usage_rs::ValueHint::DirPath)]
    dir: PathBuf,
    /// A bundle from `mdma-fed export` on the peer.
    #[usage(value_hint = usage_rs::ValueHint::FilePath)]
    bundle: PathBuf,
    /// Dial the peer here instead of at its own endpoint, e.g. through a tunnel.
    #[usage(long)]
    endpoint: Option<String>,
    /// Models the peer may see: all, or a comma list. Defaults to --execute.
    #[usage(long)]
    catalog: Option<String>,
    /// Models the peer may run: all, or a comma list.
    #[usage(long)]
    execute: Option<String>,
    /// The models folder; defaults to MDMA_MODELS_DIR.
    #[usage(long, value_hint = usage_rs::ValueHint::DirPath)]
    models: Option<PathBuf>,
}

#[derive(Debug, usage_rs::Args)]
struct Routes {
    #[usage(value_hint = usage_rs::ValueHint::DirPath)]
    dir: PathBuf,
    /// The enrolled peer's server id.
    #[usage(long)]
    peer: String,
    /// Certificate of the local client allowed to use the routes (mdma-certs client.pem).
    #[usage(long, value_hint = usage_rs::ValueHint::FilePath)]
    client_cert: PathBuf,
    /// Alias prefix; defaults to the first label of the peer's id.
    #[usage(long)]
    prefix: Option<String>,
}

#[derive(Debug, usage_rs::Args)]
struct Status {
    #[usage(value_hint = usage_rs::ValueHint::DirPath)]
    dir: PathBuf,
}

/// The folder's models and the operation each one serves.
fn offered(models: Option<PathBuf>) -> Result<Vec<(String, Operation)>> {
    let dir = models
        .or_else(|| std::env::var_os("MDMA_MODELS_DIR").map(PathBuf::from))
        .ok_or("--models or MDMA_MODELS_DIR names the models folder")?;
    let found = mdma::registry::scan(&dir)?;
    Ok(found
        .into_iter()
        .map(|(name, source)| {
            let operation = match source {
                Source::Gguf(_) => Operation::Synthesize,
                Source::Laya(_) => Operation::Decide,
            };
            (name, operation)
        })
        .collect())
}

fn pick(spec: &str, offered: &[(String, Operation)]) -> Result<Vec<(String, Operation)>> {
    if spec == "all" {
        return Ok(offered.to_vec());
    }
    spec.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            offered
                .iter()
                .find(|(offered, _)| offered == name)
                .cloned()
                .ok_or_else(|| format!("no model named {name:?} in the models folder").into())
        })
        .collect()
}

fn grants(args: &Enroll) -> Result<Grants> {
    if args.catalog.is_none() && args.execute.is_none() {
        return Ok(Grants::default());
    }
    let offered = offered(args.models.clone())?;
    let execute = args
        .execute
        .as_deref()
        .map(|spec| pick(spec, &offered))
        .transpose()?
        .unwrap_or_default();
    let catalog = match &args.catalog {
        Some(spec) => pick(spec, &offered)?,
        None => execute.clone(),
    };
    Ok(Grants {
        catalog: catalog.into_iter().map(|(name, _)| name).collect(),
        execute: execute
            .into_iter()
            .map(|(model, operation)| ExecuteExport { model, operation })
            .collect(),
    })
}

fn digest(bytes: &[u8]) -> String {
    mdma::federation::tls::sha256(bytes)
}

fn read_bundle(path: &Path) -> Result<(Bundle, String)> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bundle = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((bundle, digest(&bytes)))
}

/// After a failed dial: macOS asks before a program first reaches the local
/// network, but shows no alert to a process that exits right after (Apple
/// FB16131937), so stay up a moment for it to ask.
#[cfg(target_os = "macos")]
fn local_network_hint() {
    eprintln!(
        "mdma-fed: macOS may be blocking local network access: allow mdma-fed, or the app you run it \
        from, in System Settings > Privacy & Security > Local Network (Terminal and SSH need nothing), then run again"
    );
    std::thread::sleep(std::time::Duration::from_secs(10));
}

#[cfg(not(target_os = "macos"))]
const fn local_network_hint() {}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Init(args) => {
            let config = admin::init(&args.dir, &args.id, &args.endpoint, args.days)?;
            println!("created {} in {}", config.server_id, args.dir.display());
            println!(
                "next: mdma-fed export {} > {}.json, then enroll it on each peer",
                args.dir.display(),
                config.server_id
            );
        }
        Command::Export(args) => {
            let mut bytes = serde_json::to_vec_pretty(&admin::export(&args.dir)?)?;
            bytes.push(b'\n');
            std::io::Write::write_all(&mut std::io::stdout(), &bytes)?;
            eprintln!(
                "bundle {}; compare it with what the peer's enroll prints",
                digest(&bytes)
            );
        }
        Command::Enroll(args) => {
            let (bundle, digest) = read_bundle(&args.bundle)?;
            let grants = grants(&args)?;
            let peer = admin::enroll(&args.dir, &bundle, args.endpoint.as_deref(), grants)?;
            println!(
                "enrolled {} at {} from bundle {digest}",
                peer.server_id, peer.endpoint
            );
            if !peer.catalog_exports.is_empty() {
                println!("  sees {}", peer.catalog_exports.join(", "));
            }
            for export in &peer.execute_exports {
                println!("  runs {}", export.model);
            }
        }
        Command::Routes(args) => {
            let prefix = args
                .prefix
                .unwrap_or_else(|| args.peer.split('.').next().unwrap_or(&args.peer).to_owned());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let routes = runtime
                .block_on(admin::routes(
                    &args.dir,
                    &args.peer,
                    &args.client_cert,
                    &prefix,
                ))
                .inspect_err(|error| {
                    if error.downcast_ref::<admin::Unreachable>().is_some() {
                        local_network_hint();
                    }
                })?;
            for route in routes {
                println!("{} -> {} {}", route.alias, route.peer_id, route.model);
            }
        }
        Command::Status(args) => print!("{}", admin::status(&args.dir)?),
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run(Args::parse().command) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mdma-fed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
