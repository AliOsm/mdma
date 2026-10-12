use super::*;
use mdma::grpc::ModelStatus;
use rama::http::sse::datastar::EventData;

fn status() -> Status {
    Status {
        connected: true,
        state: "serving".into(),
        default_model: "tiny".into(),
        models: vec![ModelStatus {
            name: "tiny".into(),
            kind: "gguf".into(),
            backend: "cpu".into(),
            architecture: "llama".into(),
            n_ctx: 512,
            ready: true,
            weight_bytes: 1 << 20,
            ..ModelStatus::default()
        }],
        ..Status::default()
    }
}

/// (event type, selector-less element html or signals json)
fn decode(events: Vec<Event>) -> Vec<(String, String)> {
    events
        .into_iter()
        .map(|event| {
            let event = event.unwrap();
            let kind = event.event().unwrap().to_owned();
            let body = match event.into_data().unwrap() {
                EventData::PatchElements(patch) => patch.elements.unwrap().to_string(),
                EventData::PatchSignals(patch) => patch.signals,
                EventData::ExecuteScript(_) => {
                    unreachable!("never sent: the CSP forbids inline scripts")
                }
            };
            (kind, body)
        })
        .collect()
}

#[test]
fn scene_snapshot_is_one_json_string_signal() {
    let signals = scene(&status());
    let inner: serde_json::Value =
        serde_json::from_str(signals["_scene"].as_str().unwrap()).unwrap();
    assert_eq!(inner["status"]["models"][0]["n_ctx"], 512);
    assert_eq!(inner["status"]["state"], "serving");
}

#[test]
fn status_events_patch_only_what_the_view_shows() {
    let ids = |view, changed| {
        decode(status_events(&status(), view, changed, "sha256:00", &[]))
            .into_iter()
            .map(|(kind, body)| {
                if kind == "datastar-patch-signals" {
                    "signals".to_owned()
                } else {
                    body.split("id=\"")
                        .nth(1)
                        .and_then(|s| s.split('"').next())
                        .unwrap()
                        .to_owned()
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(View::Memory, false),
        ["signals", "status-pill", "models-panel"]
    );
    assert_eq!(
        ids(View::Federation, false),
        ["signals", "status-pill", "remote-panel"]
    );
    assert_eq!(ids(View::Playground, false), ["signals", "status-pill"]);
    assert_eq!(
        ids(View::Playground, true),
        ["signals", "status-pill", "model-select"]
    );
}

#[test]
fn append_targets_selector() {
    let event = append("#output", "<x>").unwrap();
    let EventData::PatchElements(patch) = event.into_data().unwrap() else {
        panic!("not an element patch")
    };
    assert_eq!(patch.selector.unwrap().to_string(), "#output");
    assert_eq!(patch.mode, ElementPatchMode::Append);
    assert_eq!(
        patch.elements.unwrap().to_string(),
        "&lt;x&gt;",
        "bare text is escaped"
    );
}
