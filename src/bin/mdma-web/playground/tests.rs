use super::*;

fn parse(signals: &Value) -> Result<RunRequest, String> {
    RunRequest::parse(signals)
}

#[test]
fn empty_sampler_inputs_mean_server_defaults() {
    let request = parse(&json!({ "prompt": "hi", "model": "", "max_tokens": 128, "temperature": "", "top_p": "", "min_p": "", "repetition_penalty": "" })).unwrap();
    assert_eq!(
        request,
        RunRequest {
            model: String::new(),
            prompt: "hi".into(),
            max_tokens: 128,
            sampler: None,
            chat: false,
            system: String::new()
        }
    );
    let chat =
        parse(&json!({ "prompt": "hi", "max_tokens": 1, "chat": true, "system": "  Be kind.  " }))
            .unwrap()
            .into_proto();
    assert!(chat.chat);
    assert_eq!(chat.system, "Be kind.");
    let raw =
        parse(&json!({ "prompt": "hi", "max_tokens": 1, "chat": false, "system": "Be kind." }))
            .unwrap()
            .into_proto();
    assert_eq!(raw.system, "", "a system message only travels with chat");
}

#[test]
fn sampler_values_accept_numbers_and_numeric_strings() {
    let request = parse(&json!({ "prompt": "hi", "model": "tiny", "max_tokens": "64", "temperature": 0, "top_p": "0.9", "repetition_penalty": 1.2 })).unwrap();
    assert_eq!(request.model, "tiny");
    assert_eq!(request.max_tokens, 64);
    let sampler = request.sampler.unwrap();
    assert_eq!(
        sampler.temperature,
        Some(0.0),
        "explicit zero is kept, not treated as unset"
    );
    assert_eq!(sampler.top_p, Some(0.9));
    assert_eq!(sampler.min_p, None);
    assert_eq!(sampler.repetition_penalty, Some(1.2));
    assert_eq!(request.into_proto().max_tokens, 64);
}

#[test]
fn rejects_bad_input_at_the_boundary() {
    let big = "x".repeat(MAX_PROMPT_BYTES + 1);
    let cases = [
        (
            json!({ "prompt": "  ", "max_tokens": 1 }),
            "prompt is empty",
        ),
        (json!({ "max_tokens": 1 }), "prompt is empty"),
        (json!({ "prompt": big, "max_tokens": 1 }), "accepts up to"),
        (json!({ "prompt": "hi", "max_tokens": 0 }), "max tokens"),
        (json!({ "prompt": "hi", "max_tokens": 8193 }), "max tokens"),
        (json!({ "prompt": "hi", "max_tokens": 1.5 }), "max tokens"),
        (json!({ "prompt": "hi" }), "max tokens"),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "top_p": 1.5 }),
            "top_p must be between 0 and 1",
        ),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "temperature": -1 }),
            "temperature",
        ),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "min_p": "abc" }),
            "min_p must be a number",
        ),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "model": 3 }),
            "model must be text",
        ),
        (
            json!({ "prompt": ["hi"], "max_tokens": 1 }),
            "prompt must be text",
        ),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "chat": "yes" }),
            "chat must be true or false",
        ),
        (
            json!({ "prompt": "hi", "max_tokens": 1, "chat": true, "system": "x".repeat(MAX_SYSTEM_BYTES + 1) }),
            "system message is",
        ),
    ];
    for (signals, expected) in cases {
        let error = parse(&signals).unwrap_err();
        assert!(error.contains(expected), "{signals}: {error}");
    }
}

#[test]
fn choosing_a_model_resets_the_generation_and_follows_its_chat_template() {
    use super::super::sse::tests::{decode, status};
    use mdma::grpc::ModelStatus;
    let mut status = status();
    status.models[0].chat_format = "chatml".into();
    status.models.push(ModelStatus {
        name: "far".into(),
        kind: "gguf".into(),
        backend: "federated/cuda".into(),
        n_ctx: 131_072,
        ready: true,
        ..ModelStatus::default()
    });
    let chosen = |status: &super::super::upstream::Status, name: &str| {
        let events = decode(selection(status, name));
        let signals: Value = serde_json::from_str(&events[0].1).unwrap();
        let ids: Vec<_> = events[1..]
            .iter()
            .map(|(_, html)| html.split('"').nth(1).unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            ids,
            ["inputs", "stats", "output", "run-error"],
            "{events:?}"
        );
        (signals, events[1].1.clone())
    };

    let (signals, toggle) = chosen(&status, "");
    assert_eq!(signals["chat"], true, "the default has a chat template");
    assert_eq!(
        (
            &signals["_gen"]["n_ctx"],
            &signals["_gen"]["phase"],
            &signals["_gen"]["model"]
        ),
        (&json!(512), &json!("idle"), &json!("tiny"))
    );
    assert!(!toggle.contains("disabled"), "{toggle}");

    for (name, n_ctx, why) in [
        ("far", 131_072, "not on federated routes"),
        ("nope", 0, "no such model"),
    ] {
        let (signals, toggle) = chosen(&status, name);
        assert_eq!(
            (&signals["chat"], &signals["_gen"]["n_ctx"]),
            (&json!(false), &json!(n_ctx))
        );
        assert!(
            toggle.contains("disabled") && toggle.contains(why),
            "{toggle}"
        );
    }
    status.models[0].chat_format = String::new();
    assert!(chosen(&status, "tiny").1.contains("no chat template"));
}
