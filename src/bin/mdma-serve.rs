// Foreground, environment-configured server; the OS service manager supervises it.
#[path = "mdma-serve/config.rs"]
mod config;
#[path = "mdma-serve/lifecycle.rs"]
mod lifecycle;
#[path = "mdma-serve/vars.rs"]
mod vars;

use config::{Config, ModelsSource};
use mdma::backend::Backend;
use mdma::backend::cpu::CpuBackend;
use mdma::grpc::{MdmaServer, MdmaService};
use mdma::registry::Registry;
use rama::Layer;
use rama::error::BoxError;
use rama::futures::FutureExt;
use rama::graceful::{Shutdown, ShutdownGuard};
use rama::http::server::HttpServer;
use rama::rt::Executor;
use rama::tcp::server::TcpListener;
use rama::tls::rustls::server::TlsAcceptorLayer;
use rama::tls::server::{ClientVerifyMode, ServerAuthData, TlsServerConfig};
use rustls_pki_types::pem::{Error as PemError, PemObject};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::Duration;

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, BoxError> {
    CertificateDer::pem_file_iter(path)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, BoxError> {
    PrivateKeyDer::from_pem_file(path).map_err(|error| match error {
        PemError::NoItemsFound => format!("no private key found in {path}").into(),
        error => error.into(),
    })
}

async fn run<B>(
    init_backend: impl FnOnce() -> mdma::backend::Result<B> + Send + 'static,
    backend_name: &'static str,
    mut config: Config,
    guard: ShutdownGuard,
) -> Result<(), BoxError>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    let listen = config.listen;
    let mut power = config.power.take();
    let executor = Executor::graceful(guard.clone());
    let federation_executor = executor.clone();
    let startup = tokio::task::spawn_blocking(move || {
        // Device initialization/JIT must not block the shutdown supervisor either.
        let mut backend = init_backend()?;
        if config.device_buffer_bytes.is_some() || config.host_buffer_bytes.is_some() {
            backend.set_buffer_budget(mdma::memory::BufferBudget::new(config.device_buffer_bytes, config.host_buffer_bytes))?;
        }
        let backend = Arc::new(backend);
        if let Some(power) = power.as_mut() {
            power.gpu = gpu_probe(&*backend);
        }
        let server_cert_chain = load_certs(&config.tls_cert)?;
        let server_key = load_private_key(&config.tls_key)?;
        let ca_certs = load_certs(&config.tls_ca)?;
        let federation = config.federation_config.as_ref().map(|path| {
            mdma::federation::runtime::Prepared::load(
                mdma::federation::runtime::Config::load(std::path::Path::new(path))?,
                listen.into(), &ca_certs, &server_cert_chain,
            ).map(|prepared| prepared.saving_to(path.into()))
        }).transpose()?;
        let tls_config = TlsServerConfig::new()
            .with_single_cert(ServerAuthData {
                private_key: server_key,
                cert_chain: server_cert_chain,
                ocsp: None,
            })
            .with_client_verify(ClientVerifyMode::ClientAuth(ca_certs))
            .with_alpn_http_auto();

        let ModelsSource::Dir { path, default } = &config.models else {
            return Err("control-only mode cannot load a model registry".into());
        };
        let memory = mdma::memory::NativeMemoryBudget::new(config.native_tensor_bytes.unwrap_or_else(|| default_budget(backend_name)));
        let options = mdma::registry::Options {
            // Unset: each model's trained context.
            n_ctx: config.n_ctx.unwrap_or(usize::MAX),
            warmup_rounds: config.warmup_rounds,
            frozen: federation.is_some(),
            budget: Some(memory.clone()),
        };
        let mut registry = Registry::open(backend.clone(), std::path::Path::new(path), default.as_deref(), options)?;
        if registry.names().is_empty() {
            let routes = federation.as_ref().is_some_and(mdma::federation::runtime::Prepared::routing_enabled);
            let installs = federation.as_ref().is_some_and(mdma::federation::runtime::Prepared::accepts_installs);
            if !routes && !installs {
                return Err(format!("no usable models in {path}; only a node that routes to peers or takes their models may have none").into());
            }
            eprintln!("mdma: no local models in {path}; {}", if routes { "serving routes only" } else { "waiting for peers to install some" });
        }
        if memory.limit() != usize::MAX {
            eprintln!("mdma native tensor budget: {}/{} bytes reserved (not a process/VRAM cap)", memory.used(), memory.limit());
        }
        if let Some(buffers) = backend.buffer_budget() {
            for domain in [mdma::memory::MemoryDomain::Device, mdma::memory::MemoryDomain::Host] {
                let limit = buffers.limit(domain).map_or_else(|| "unlimited".into(), |bytes| bytes.to_string());
                eprintln!("mdma {domain} buffers: {}/{limit} bytes allocated (excludes driver internals)", buffers.used(domain));
            }
        }
        for entry in registry.entries() {
            eprintln!("mdma model {}: {} {} ({} MB on disk)", entry.name, entry.info.kind.name(), entry.info.architecture, entry.info.disk_bytes >> 20);
        }
        eprintln!("mdma models load on first use ({} warm-up rounds each)", config.warmup_rounds);
        if federation.is_some() {
            match registry.capture_execution_profiles() {
                Ok(count) => eprintln!("mdma federation native profiles: {count} verified (not export grants)"),
                Err(error) => eprintln!("mdma federation native profiles unavailable: {error}; native models cannot export"),
            }
        }
        let host = mdma::host::detect(&config.node, &backend.device_label().unwrap_or_else(|| backend_name.to_owned()));
        eprintln!("mdma node {}: {} {} ({})", host.name, host.os_name, host.arch, host.device);
        let mut service = MdmaService::new(registry, backend_name)
            .with_shutdown_guard(guard.clone_weak())
            .with_memory_budget(memory)
            .with_operators(std::mem::take(&mut config.operators))
            .with_admins(std::mem::take(&mut config.admins))
            .with_max_output_tokens(config.max_output_tokens)
            .with_host(host);
        if federation.is_some() {
            // Federation names a model by its bytes; hash them before the first load.
            let (registry, stop) = (service.registry().clone(), guard.clone_weak());
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                registry.hash_artifacts(|| stop.cancelled().now_or_never().is_some());
                eprintln!("mdma federation: model files hashed in {:.1}s", started.elapsed().as_secs_f64());
            });
        }
        // Lease expiry and idle unload.
        let (registry, idle, stop) = (service.registry().clone(), config.idle_unload, guard.clone_weak());
        guard.spawn_task(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = tick.tick() => registry.sweep(std::time::Instant::now(), idle),
                }
            }
        });
        if let Some(power) = power {
            // Started after the model loads, so the first reading applies
            // before the listener accepts anything.
            let source = match power.gpu {
                Some(gpu) => Box::new(mdma::power::WithGpu::new(
                    mdma::power::default_source(),
                    gpu,
                )) as Box<dyn mdma::power::PowerSource>,
                None => mdma::power::default_source(),
            };
            let status = mdma::power::spawn_power_watcher(source, power.watch, guard.clone());
            service = service.with_power(status, power.degraded);
        }
        let federation = federation.map(|runtime|
            runtime.with_native_service(&mut service, federation_executor)
        ).transpose()?;
        Ok::<_, BoxError>(Some((MdmaServer::new(service), tls_config, federation)))
    })
    .await??;
    let Some((grpc_svc, tls_config, federation)) = startup else {
        return Ok(());
    };

    // Do not open a new listening socket if stop was requested during startup.
    if executor
        .guard()
        .expect("Executor::graceful always carries a shutdown guard")
        .cancelled()
        .now_or_never()
        .is_some()
    {
        return Ok(());
    }
    let client_policy = federation.as_ref().map_or_else(
        Default::default,
        mdma::federation::runtime::Prepared::client_policy,
    );
    let tcp_service = client_policy.connection_service(
        TlsAcceptorLayer::new(tls_config).into_layer(client_policy.tls_service(
            HttpServer::auto(executor.clone()).service(client_policy.http_service(grpc_svc)),
        )),
    );

    let listener = TcpListener::bind_address(listen.to_string(), executor.clone()).await?;
    let federation_listener = match &federation {
        Some(runtime) => Some(runtime.bind(executor.clone()).await?),
        None => None,
    };
    eprintln!(
        "mdma listening on {} (mTLS required, backend={backend_name})",
        listener.local_addr()?
    );
    if let Some(peer_listener) = &federation_listener {
        let workloads = federation
            .as_ref()
            .is_some_and(mdma::federation::runtime::Prepared::execution_enabled);
        eprintln!(
            "mdma federation listening on {} (strict mTLS, execution={workloads})",
            peer_listener.local_addr()?
        );
    }
    let peers = async move {
        match (federation, federation_listener) {
            (Some(runtime), Some(listener)) => runtime.serve(listener, executor).await,
            _ => std::future::pending::<Result<(), BoxError>>().await,
        }
    };
    tokio::select! {
        () = listener.serve(tcp_service) => {},
        result = peers => { result?; },
    }

    Ok(())
}

/// The CPU backend has no device memory to check before a load, so the
/// budget keeps a quarter of RAM for everything else.
fn default_budget(backend_name: &str) -> usize {
    if backend_name != "cpu" {
        return usize::MAX;
    }
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    usize::try_from(system.total_memory() / 4 * 3).unwrap_or(usize::MAX)
}

/// The backend's GPU power-limit probe, if it has one (cuda, through NVML).
/// Without it the watcher still sees the power supply.
fn gpu_probe<B: 'static>(backend: &B) -> Option<Box<dyn mdma::power::GpuProbe>> {
    #[cfg(feature = "cuda")]
    if let Some(cuda) =
        (backend as &dyn std::any::Any).downcast_ref::<mdma::backend::cuda::CudaBackend>()
    {
        return match cuda
            .pci_bus_id()
            .and_then(|bus| mdma::power::NvmlProbe::new(&bus))
        {
            Ok(probe) => Some(Box::new(probe)),
            Err(e) => {
                eprintln!("mdma power: no GPU power limit ({e})");
                None
            }
        };
    }
    let _ = backend;
    None
}

async fn launch(config: Config, guard: ShutdownGuard) -> Result<(), BoxError> {
    if matches!(&config.models, ModelsSource::ControlOnly) {
        let executor = Executor::graceful(guard.clone());
        let prepared = tokio::task::spawn_blocking(move || {
            let path = config
                .federation_config
                .ok_or("missing control-only federation config")?;
            mdma::federation::runtime::Prepared::load(
                mdma::federation::runtime::Config::load(std::path::Path::new(&path))?,
                config.listen.into(),
                &load_certs(&config.tls_ca)?,
                &load_certs(&config.tls_cert)?,
            )
        })
        .await??;
        if guard.cancelled().now_or_never().is_some() {
            return Ok(());
        }
        let listener = prepared.bind(executor.clone()).await?;
        eprintln!(
            "mdma federation control-only listening on {} (strict mTLS, no client listener or workloads)",
            listener.local_addr()?
        );
        prepared.serve(listener, executor).await?;
        return Ok(());
    }
    match config.backend.as_str() {
        "cpu" => run(|| Ok(CpuBackend), "cpu", config, guard).await,
        #[cfg(feature = "rocm")]
        "rocm" => {
            let reference = config.rocm_f32;
            run(
                move || {
                    if reference {
                        mdma::backend::rocm::HipBackend::new_f32()
                    } else {
                        mdma::backend::rocm::HipBackend::new()
                    }
                },
                "rocm",
                config,
                guard,
            )
            .await
        }
        #[cfg(not(feature = "rocm"))]
        "rocm" => Err("MDMA_BACKEND=rocm requires building with --features rocm".into()),
        #[cfg(feature = "level0")]
        "level0" => {
            run(
                mdma::backend::level0::Level0Backend::new,
                "level0",
                config,
                guard,
            )
            .await
        }
        #[cfg(not(feature = "level0"))]
        "level0" => Err("MDMA_BACKEND=level0 requires building with --features level0".into()),
        #[cfg(feature = "vulkan")]
        "vulkan" => {
            run(
                mdma::backend::vulkan::VulkanBackend::new,
                "vulkan",
                config,
                guard,
            )
            .await
        }
        #[cfg(not(feature = "vulkan"))]
        "vulkan" => Err("MDMA_BACKEND=vulkan requires building with --features vulkan".into()),
        #[cfg(feature = "cuda")]
        "cuda" => {
            run(
                || {
                    let backend = mdma::backend::cuda::CudaBackend::new()?;
                    eprintln!("mdma: cuda device {}", backend.device_name());
                    Ok(backend)
                },
                "cuda",
                config,
                guard,
            )
            .await
        }
        #[cfg(not(feature = "cuda"))]
        "cuda" => Err("MDMA_BACKEND=cuda requires building with --features cuda".into()),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        "metal" => {
            run(
                || {
                    let backend = mdma::backend::metal::MetalBackend::new()?;
                    eprintln!("mdma: metal device {}", backend.device_name());
                    Ok(backend)
                },
                "metal",
                config,
                guard,
            )
            .await
        }
        #[cfg(all(not(feature = "metal"), target_os = "macos"))]
        "metal" => Err("MDMA_BACKEND=metal requires building with --features metal".into()),
        #[cfg(not(target_os = "macos"))]
        "metal" => Err("MDMA_BACKEND=metal requires macOS with a unified-memory Metal GPU".into()),
        #[cfg(all(feature = "coral", any(target_os = "freebsd", target_os = "linux")))]
        "coral" => {
            run(
                mdma::backend::coral::CoralBackend::new,
                "coral",
                config,
                guard,
            )
            .await
        }
        #[cfg(not(all(feature = "coral", any(target_os = "freebsd", target_os = "linux"))))]
        "coral" => Err(
            "MDMA_BACKEND=coral requires FreeBSD or Linux and building with --features coral"
                .into(),
        ),
        other => Err(format!(
            "unknown MDMA_BACKEND '{other}' (expected cpu, rocm, level0, vulkan, cuda, metal, or coral)"
        )
        .into()),
    }
}

fn main() -> Result<(), BoxError> {
    // Hold before backend/model initialization and through runtime shutdown.
    let _machine_lock = mdma::singleton::MachineLock::acquire()?;
    let config = Config::load(|name| std::env::var(name))?;
    let limit = config.shutdown_timeout;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let signal = lifecycle::shutdown_signal()?;
        let shutdown = Shutdown::new(async move {
            signal.await;
            eprintln!(
                "mdma stopping: draining requests for up to {}s",
                limit.as_secs()
            );
        });
        let guard = shutdown.guard();
        lifecycle::supervise(shutdown, launch(config, guard), limit).await
    });
    // Runtime::drop waits indefinitely for spawn_blocking, even after a drain timeout.
    runtime.shutdown_timeout(Duration::ZERO);
    result
}
