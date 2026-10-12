use super::*;

fn introspect() -> IntrospectResponse {
    IntrospectResponse {
        ready: true,
        server_state: "serving".into(),
        backend: "cpu".into(),
        battery_percent: -1,
        default_model: "tiny".into(),
        models: vec![ModelStatus { name: "tiny".into(), kind: "gguf".into(), n_ctx: 512, ready: true, weight_bytes: 1 << 20, ..Default::default() }],
        ..Default::default()
    }
}

#[test]
fn maps_introspect_and_keeps_models_when_unreachable() {
    let status = Status::from(introspect());
    assert!(status.connected && status.ready);
    assert_eq!(status.battery_percent, None);
    assert_eq!(status.model("").map(|m| m.n_ctx), Some(512));
    assert_eq!(status.model("tiny").map(|m| m.weight_bytes), Some(1 << 20));
    assert!(status.model("other").is_none());

    let down = status.unreachable("Unavailable: connection refused".into());
    assert!(!down.connected && !down.ready);
    assert_eq!(down.state, "unreachable");
    assert_eq!(down.models, status.models);
}

#[test]
fn watch_snapshots_keep_server_messages_and_their_json_names() {
    use mdma::grpc::{DeviceMemory, FederationTopology, MemorySnapshot, ModelGeometry, ServerSnapshot, WeightUpload};
    let mut status = introspect();
    status.models[0].geometry = Some(ModelGeometry { n_layers: 22, kv_bytes_per_token: 1024, layer_kv_widths: vec![256; 22], ..Default::default() });
    let memory = MemorySnapshot {
        kv_bytes: 64, workspace_bytes: 36, active_requests: 1,
        device: Some(DeviceMemory { name: "gfx1103".into(), unified_memory: true, available_bytes: Some(4096) }),
        weights: vec![WeightUpload { upload_id: 0, bytes: 200, reserved: true, models: vec!["tiny".into()] }],
        ..Default::default()
    };
    let snapshot = ServerSnapshot { status: Some(status), memory: Some(memory.clone()), federation: Some(FederationTopology::default()) };
    let status = Status::from(snapshot);
    assert!(status.connected);
    assert_eq!(status.memory, Some(memory));
    assert_eq!(status.federation, Some(FederationTopology::default()), "refused topology stays empty");
    // The scene JSON keeps the field names the three.js layout reads.
    let json = serde_json::to_value(&status).unwrap();
    for path in ["/memory/kv_bytes", "/memory/workspace_bytes", "/memory/limit_bytes", "/memory/device/available_bytes",
        "/memory/weights/0/upload_id", "/models/0/geometry/n_layers", "/models/0/chat_format", "/federation/authorized"] {
        assert!(json.pointer(path).is_some(), "{path} missing from {json}");
    }
    assert!(Status::from(ServerSnapshot::default()).models.is_empty());
}
