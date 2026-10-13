use super::*;
use mdma::grpc::{CatalogView, DeviceMemory, FederationPeer, FederationRoute, Host};

const PIN: &str = "sha256:00";

fn status() -> Status {
    let model = |name: &str, backend: &str, ready: bool| ModelStatus {
        name: name.into(),
        kind: "gguf".into(),
        backend: backend.into(),
        architecture: "llama".into(),
        n_ctx: 2048,
        trained_ctx: 4096,
        ready,
        weight_bytes: if backend.starts_with("federated/") {
            0
        } else {
            3 << 30
        },
        parameters: 1_100_048_384,
        quantization: "Q4_K_M".into(),
        class: "generative".into(),
        residency: "resident".into(),
        ..ModelStatus::default()
    };
    Status {
        connected: true,
        ready: true,
        state: "serving".into(),
        backend: "cpu".into(),
        default_model: "tiny".into(),
        models: vec![
            model("tiny", "cpu", true),
            model("<b>far</b>", "federated/rocm", false),
        ],
        ..Status::default()
    }
}

#[test]
fn pages_wire_datastar_and_the_unmorphed_scene() {
    for view in View::ALL {
        let rendered = page(view, &status(), PIN);
        assert!(rendered.starts_with("<!DOCTYPE html>"), "{view:?}");
        let html = decode_entities(&rendered);
        assert!(
            html.contains(&format!("@get('/sse/status?view={}'", view.key())),
            "{view:?}"
        );
        assert!(
            html.contains(&format!(r#"data-view="{}""#, view.key())),
            "{view:?}"
        );
        assert!(html.contains("data-ignore-morph"), "{view:?}");
        assert!(
            html.contains(&format!(r#"href="{}" class="active""#, view.path())),
            "{view:?}"
        );
        let (scene, datastar) = (
            html.find("/assets/scene.js").unwrap(),
            html.find("/assets/datastar.js").unwrap(),
        );
        assert!(
            scene < datastar,
            "{view:?}: the scene bridge must load before Datastar"
        );
        assert!(
            html.contains("chat: false"),
            "the default has no chat template: {view:?}"
        );
    }
    let playground = decode_entities(&page(View::Playground, &status(), PIN)).into_owned();
    assert!(
        playground.contains(r#"data-on:change="@post('/playground/model')""#)
            && playground.contains(r#"<label id="chat-toggle" class="check"><input type="checkbox" data-bind:chat="" disabled="">"#),
        "choosing a model resets the playground: {playground}"
    );
}

#[test]
fn fragments_escape_server_supplied_text() {
    let status = status();
    for html in [
        models_panel(&status).into_string(),
        remote_panel(&status, PIN, &[]).into_string(),
        model_select(&status).into_string(),
    ] {
        assert!(!html.contains("<b>far</b>"), "{html}");
        assert!(html.contains("&lt;b&gt;far&lt;/b&gt;"), "{html}");
    }
    assert!(
        run_error("<img src=x onerror=alert(1)>")
            .into_string()
            .contains("&lt;img")
    );
}

#[test]
fn panels_show_state_with_text_not_only_color() {
    let status = status();
    let models = models_panel(&status).into_string();
    assert!(
        models.contains(r#"class="residency ok" title="">loaded</span>"#),
        "{models}"
    );
    assert!(
        models.contains(r#"class="residency bad" title="">unavailable</span>"#),
        "{models}"
    );
    assert!(models.contains("3.0 GiB"), "{models}");
    assert!(
        models.contains("llama · 1.1B · Q4_K_M · generative · gguf · cpu · default"),
        "{models}"
    );
    assert!(
        models.contains(r#"<td class="num">2048<small>of 4096</small></td>"#),
        "the trained context under its cap: {models}"
    );
    let mut uncapped = status.clone();
    for m in &mut uncapped.models {
        (m.trained_ctx, m.parameters, m.quantization) = (m.n_ctx, 0, String::new());
    }
    let uncapped = models_panel(&uncapped).into_string();
    assert!(
        uncapped.contains(r#"<td class="num">2048</td>"#)
            && uncapped.contains("llama · generative · gguf · cpu · default"),
        "unknown facts and an uncapped context add nothing: {uncapped}"
    );
    assert!(
        remote_panel(&status, PIN, &[])
            .into_string()
            .contains("federated/rocm"),
        "backend when no node is reported"
    );

    let pill = |status: &Status| status_pill(status).into_string();
    assert!(pill(&status).contains("serving · cpu · 1/2 models"));
    let unreachable = Status {
        error: Some("Unavailable: refused".into()),
        state: "unreachable".into(),
        ..Status::default()
    };
    assert!(pill(&unreachable).contains(r#"class="pill state-unreachable""#));
    assert!(pill(&unreachable).contains("unreachable · Unavailable: refused"));
    let battery = Status {
        on_battery: true,
        battery_percent: Some(42),
        ..status
    };
    assert!(pill(&battery).contains("battery 42%"));
}

#[test]
fn models_show_residency_and_leases_and_the_gateway_holds_them() {
    let mut status = status();
    status.models[0].leases = 2;
    let model = |name: &str, residency: &str, error: &str| ModelStatus {
        name: name.into(),
        backend: "cpu".into(),
        ready: true,
        residency: residency.into(),
        error: error.into(),
        ..ModelStatus::default()
    };
    status.models.extend([
        model("cold", "available", "model does not fit now"),
        model("warming", "loading", ""),
        model("broken", "failed", "changed on disk"),
        model("odd'name", "available", ""),
    ]);
    let html = remote_panel(&status, PIN, &["tiny".into()]).into_string();
    let decoded = decode_entities(&html);
    for expected in [
        "loaded · 2 leases",
        r#"title="model does not fit now">on disk"#,
        r#"class="residency busy" title="">loading"#,
        r#"class="residency bad" title="changed on disk">failed"#,
        "@post('/models/release?model=tiny')",
        "@post('/models/hold?model=cold')",
        "@post('/models/hold?model=warming')",
        r#"<p id="lease-error" class="error">"#,
    ] {
        assert!(decoded.contains(expected), "missing {expected}: {decoded}");
    }
    for absent in ["model=broken", "model=odd", "model=&lt;b&gt;"] {
        assert!(
            !decoded.contains(absent),
            "no button for {absent}: {decoded}"
        );
    }
    assert!(lease_error("<x>").into_string().contains("&lt;x&gt;"));
}

#[test]
fn byte_units() {
    for (n, expected) in [
        (0, "0 B"),
        (1023, "1023 B"),
        (1536, "1.5 KiB"),
        (5 << 20, "5.0 MiB"),
        (3 << 40, "3072.0 GiB"),
    ] {
        assert_eq!(bytes(n), expected, "{n}");
    }
}

#[test]
fn scene_filter_matches_leaf_and_nested_display_signals() {
    let html = decode_entities(&page(View::Memory, &status(), PIN)).into_owned();
    assert!(
        html.contains(r#"data-on-signal-patch-filter="{include: /^_(scene|gen)(\.|$)/}""#),
        "{html}"
    );
}

#[test]
fn memory_summary_appears_with_watch_snapshots() {
    let waiting = models_panel(&status()).into_string();
    assert!(
        waiting.contains("Waiting for the server") && !waiting.contains("native budget"),
        "{waiting}"
    );
    let memory = MemorySnapshot {
        used_bytes: 3 << 30,
        weights_bytes: 2 << 30,
        kv_bytes: 512 << 20,
        workspace_bytes: 512 << 20,
        device: Some(DeviceMemory {
            name: "Radeon 780M".into(),
            unified_memory: true,
            available_bytes: Some(8 << 30),
        }),
        host_buffers: Some(MemoryUsage {
            used_bytes: 1 << 20,
            limit_bytes: 4 << 20,
        }),
        active_requests: 2,
        ..MemorySnapshot::default()
    };
    let html = models_panel(&Status {
        memory: Some(memory),
        ..status()
    })
    .into_string();
    for expected in [
        "3.0 GiB reserved · no limit set",
        "512.0 MiB",
        "2 requests",
        "Radeon 780M · unified memory · 8.0 GiB free",
        "1.0 MiB of 4.0 MiB",
    ] {
        assert!(html.contains(expected), "missing {expected}: {html}");
    }
    assert!(
        !html.contains("device buffers"),
        "absent quotas are not shown"
    );
}

#[test]
fn federation_panel_lists_peer_nodes_and_gates_enrollment() {
    let foundry = host(
        "foundry",
        "linux",
        "Ubuntu 24.04 · x86_64 · AMD Radeon 780M",
    );
    let citadel = host("citadel", "freebsd", "FreeBSD 16.0-CURRENT · amd64 · CPU");
    let remote = |name: &str, host: &Host, backend: &str, class: &str, ready: bool| ModelStatus {
        name: name.into(),
        kind: "gguf".into(),
        backend: format!("federated/{backend}"),
        host: Some(host.clone()),
        class: class.into(),
        ready,
        ..ModelStatus::default()
    };
    let mut models = vec![
        remote("qwen3", &foundry, "rocm", "generative", true),
        remote("numarkdown", &foundry, "rocm", "generative", true),
        remote("minicheck", &citadel, "cpu", "decision", false),
    ];
    models[0].capabilities = vec!["reasoning".into()];
    (models[0].parameters, models[0].quantization) = (8_190_735_360, "Q4_K_M".into());
    let periscope = Host {
        memory_bytes: 64 << 30,
        memory_available_bytes: 40 << 30,
        disk_bytes: 2 << 40,
        disk_available_bytes: 1 << 40,
        ..host("periscope", "macos", "macOS 15.6 · arm64 · Apple M4 Max")
    };
    let html = |federation| {
        remote_panel(
            &Status {
                models: [status().models, models.clone()].concat(),
                federation,
                host: Some(periscope.clone()),
                ..status()
            },
            PIN,
            &[],
        )
        .into_string()
    };

    let peers = html(None);
    let expected = [
        r#"<strong class="os macos">periscope</strong>"#,
        "macOS 15.6 · arm64 · Apple M4 Max",
        "RAM 40/64 GiB · disk 1.0/2.0 TiB free",
        "tiny",
        r#"<strong class="os freebsd">citadel</strong>"#,
        "FreeBSD 16.0-CURRENT · amd64 · CPU",
        r#"class="class decision""#,
        r#"<strong class="os linux">foundry</strong>"#,
        "Ubuntu 24.04 · x86_64 · AMD Radeon 780M · rocm",
        r#"<small class="facts">8.2B · Q4_K_M</small>"#,
        r#"class="cap reasoning""#,
        "numarkdown",
    ];
    let mut at = 0;
    for text in expected {
        let found = peers[at..]
            .find(text)
            .unwrap_or_else(|| panic!("missing {text} after byte {at}: {peers}"));
        at += found;
    }
    assert!(!peers.contains("localhost"), "{peers}");
    assert_eq!(
        peers.matches(r#"class="facts""#).count(),
        3,
        "tiny, far and qwen3 have known facts: {peers}"
    );
    assert!(
        !peers.contains("MDMA_OPERATOR_SPKI"),
        "nothing to gate before the first snapshot"
    );
    assert!(
        remote_panel(&Status::default(), PIN, &[])
            .into_string()
            .contains("No peers.")
    );

    let refused = html(Some(FederationTopology::default()));
    assert!(
        refused.contains("MDMA_OPERATOR_SPKI=sha256:00")
            && refused.contains(r#"<strong class="os linux">foundry</strong>"#),
        "{refused}"
    );
    let operator_without_federation = html(Some(FederationTopology {
        authorized: true,
        ..FederationTopology::default()
    }));
    assert!(
        !operator_without_federation.contains("MDMA_OPERATOR_SPKI")
            && !operator_without_federation.contains("Enrolled peers")
    );

    let federation = FederationTopology {
        authorized: true,
        enabled: true,
        server_id: "home".into(),
        trust_generation: 7,
        peers: vec![
            FederationPeer {
                peer_id: "lab".into(),
                endpoint: "https://lab:7443".into(),
                state: "active".into(),
                catalog_exports: vec!["tiny".into()],
                route_imports: vec!["big".into(), "x".into()],
                catalog: Some(CatalogView {
                    expires_at_ms: u64::MAX,
                    server_state: "serving".into(),
                    ..CatalogView::default()
                }),
                host: Some(citadel),
                ..FederationPeer::default()
            },
            FederationPeer {
                peer_id: "<old>".into(),
                state: "revoked".into(),
                ..FederationPeer::default()
            },
        ],
        routes: vec![FederationRoute {
            alias: "big".into(),
            peer_id: "lab".into(),
            model: "big".into(),
            operation: "synthesize".into(),
        }],
    };
    let shown = html(Some(federation));
    for expected in [
        "Enrolled peers",
        "home · trust generation 7",
        "https://lab:7443",
        r#"class="trust active""#,
        "sees 1 · runs 0 · routes 2",
        "0 models · serving",
        "FreeBSD 16.0-CURRENT · amd64 · CPU",
        "no fresh catalog",
        r#"class="trust revoked""#,
        "&lt;old&gt;",
        "lab/big",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
}

fn host(name: &str, os: &str, line: &str) -> Host {
    let [os_name, arch, device] = line
        .split(" · ")
        .collect::<Vec<_>>()
        .try_into()
        .expect("os · arch · device");
    Host {
        name: name.into(),
        os: os.into(),
        os_name: os_name.into(),
        arch: arch.into(),
        device: device.into(),
        ..Host::default()
    }
}

#[test]
fn marker_chips_follow_the_models_chat_format() {
    let model = |chat_format: &str, backend: &str| ModelStatus {
        name: "m".into(),
        backend: backend.into(),
        chat_format: chat_format.into(),
        ..ModelStatus::default()
    };
    let html = |m: &ModelStatus| decode_entities(&marker_chips(Some(m)).into_string()).into_owned();
    let chatml = html(&model("chatml", "cpu"));
    for expected in [
        r#"data-show="!$chat""#,
        "chatml markers",
        "data-marker-open=\"<|im_start|>system\n\"",
        "data-marker-close=\"<|im_end|>\n\"",
        ">system</button>",
        ">user</button>",
        ">assistant</button>",
        ">reply</button>",
    ] {
        assert!(chatml.contains(expected), "missing {expected}: {chatml}");
    }
    let gemma = html(&model("gemma", "cpu"));
    assert!(
        !gemma.contains(">system<") && gemma.contains(">reply<"),
        "no system role: {gemma}"
    );
    let inst = html(&model("inst", "cpu"));
    assert!(
        !inst.contains(">reply<") && inst.contains(r#"data-marker-close="</s>""#),
        "[INST] replies right after the turn: {inst}"
    );
    for m in [model("", "cpu"), model("", "federated/cuda")] {
        let shown = html(&m);
        assert!(
            !shown.contains("<button") && shown.contains("no chat template"),
            "{shown}"
        );
    }
    let routed = html(&model("chatml", "federated/cuda"));
    assert!(
        routed.contains(">user</button>"),
        "routes carry markers: {routed}"
    );
}
