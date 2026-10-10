use super::*;
use crate::backend::cpu::CpuBackend;
use crate::gguf::tests::Fixture;
use crate::grpc::MdmaService;
use crate::registry::ModelsFile;

pub(crate) fn native_service() -> MdmaService<CpuBackend> {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models[0].name = "native".into();
    config.models[0].n_ctx = Some(32);
    let mut alias = config.models[0].clone();
    alias.name = "alias".into();
    alias.n_ctx = Some(8);
    config.models.push(alias);
    let mut registry =
        Registry::load_available_frozen_with_budget(&CpuBackend, &config, 64, None, |_| {})
            .unwrap();
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 2);
    // The published inventory must refer to already loaded bytes, never reopen
    // the source after the fixture is destroyed on return.
    MdmaService::new(Arc::new(CpuBackend), registry, "not-a-profile-claim")
}

fn exports() -> Vec<String> {
    vec!["native".into(), "alias".into(), "unknown".into()]
}

#[test]
fn only_explicit_loaded_exact_identities_are_visible_with_alias_caps() {
    let service = native_service();
    let inventory = service.federation_inventory();
    assert!(inventory.snapshot(&[]).models.is_empty());
    assert!(
        inventory.snapshot(&[String::new()]).models.is_empty(),
        "no default alias"
    );
    let snapshot = inventory.snapshot(&exports());
    assert_eq!(snapshot.state, ServerState::Serving);
    assert_eq!(snapshot.models.len(), 2);
    let (native, alias) = (&snapshot.models[0], &snapshot.models[1]);
    assert_eq!(native.model, "native");
    assert_eq!(alias.model, "alias");
    assert_eq!(native.artifact_id, alias.artifact_id);
    assert_eq!(native.profile_id, alias.profile_id);
    assert_eq!(
        native.backend, "cpu",
        "never use informational backend label"
    );
    assert!(native.ready && alias.ready);
    assert_eq!(
        native.limits,
        Limits::Synthesize {
            n_ctx: 32,
            max_output_tokens: 32
        }
    );
    assert_eq!(
        alias.limits,
        Limits::Synthesize {
            n_ctx: 8,
            max_output_tokens: 8
        }
    );
}

#[test]
fn power_changes_restrict_caps_critical_and_dead_watchers_withdraw_readiness() {
    use crate::power::ServerState as PowerState;
    let (tx, rx) = watch::channel(PowerStatus::default());
    let service = native_service().with_power(
        rx,
        DegradedLimits {
            n_ctx: 12,
            max_tokens: 3,
        },
    );
    let inventory = service.federation_inventory();
    let serving = inventory.snapshot(&exports());
    tx.send(PowerStatus {
        state: PowerState::Degraded,
        reading: None,
    })
    .unwrap();
    let degraded = inventory.snapshot(&exports());
    assert_eq!(degraded.state, ServerState::Degraded);
    assert_eq!(
        degraded.models[0].limits,
        Limits::Synthesize {
            n_ctx: 12,
            max_output_tokens: 3
        }
    );
    assert_eq!(
        degraded.models[1].limits,
        Limits::Synthesize {
            n_ctx: 8,
            max_output_tokens: 3
        }
    );
    for model in &degraded.models {
        assert!(model.ready);
    }
    assert_eq!(serving.models[0].profile_id, degraded.models[0].profile_id);
    tx.send(PowerStatus {
        state: PowerState::Critical,
        reading: None,
    })
    .unwrap();
    let critical = inventory.snapshot(&exports());
    assert_eq!(critical.state, ServerState::Critical);
    assert!(critical.models.iter().all(|m| !m.ready));
    tx.send(PowerStatus::default()).unwrap();
    assert_eq!(inventory.snapshot(&exports()), serving);
    drop(tx);
    let closed = inventory.snapshot(&exports());
    assert_eq!(closed.state, ServerState::Critical);
    assert!(closed.models.iter().all(|m| !m.ready));
}

#[tokio::test]
async fn shutdown_overrides_power_and_marks_all_models_unready() {
    let trigger = tokio_util::sync::CancellationToken::new();
    let shutdown = rama::graceful::Shutdown::new(trigger.clone().cancelled_owned());
    let weak = shutdown.guard_weak();
    let service = native_service().with_shutdown_guard(weak.clone());
    let inventory = service.federation_inventory();
    assert!(
        inventory
            .snapshot(&exports())
            .models
            .iter()
            .all(|m| m.ready)
    );
    trigger.cancel();
    weak.cancelled().await;
    let drained = inventory.snapshot(&exports());
    assert_eq!(drained.state, ServerState::Draining);
    assert!(drained.models.iter().all(|m| !m.ready));
}

#[test]
fn ordinary_unavailable_or_unprofiled_native_loads_are_not_exportable() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models[0].name = "native".into();
    let registry = Registry::load(&CpuBackend, &config, 64, |_| {}).unwrap();
    let service = MdmaService::new(Arc::new(CpuBackend), registry, "cpu");
    assert!(
        service
            .federation_inventory()
            .snapshot(&exports())
            .models
            .is_empty()
    );
    let registry =
        Registry::load_available_frozen_with_budget(&CpuBackend, &config, 64, None, |_| {})
            .unwrap();
    let service = MdmaService::new(Arc::new(CpuBackend), registry, "cpu");
    assert!(
        service
            .federation_inventory()
            .snapshot(&exports())
            .models
            .is_empty()
    );
    let budget = crate::memory::NativeMemoryBudget::new(0);
    let mut registry = Registry::load_available_frozen_with_budget(
        &CpuBackend,
        &config,
        64,
        Some(&budget),
        |_| panic!("no upload"),
    )
    .unwrap();
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 0);
    let service = MdmaService::new(Arc::new(CpuBackend), registry, "cpu");
    let snapshot = service.federation_inventory().snapshot(&exports());
    assert_eq!(snapshot.state, ServerState::Degraded);
    assert!(snapshot.models.is_empty());
}
