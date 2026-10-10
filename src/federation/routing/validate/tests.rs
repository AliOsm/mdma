use super::*;
use crate::federation::{
    catalog::Kind,
    control::{Completed, Token},
};
use serde_json::json;

fn fixture() -> (SynthesizeRequest, ModelDescriptor, SynthesizeEvent) {
    let descriptor = ModelDescriptor {
        model: "model".into(),
        kind: Kind::Gguf,
        artifact_id: format!("sha256:{}", "11".repeat(32)),
        profile_id: format!("sha256:{}", "22".repeat(32)),
        architecture: "test".into(),
        backend: "cpu".into(),
        ready: true,
        max_input_bytes: 65536,
        limits: Limits::Synthesize {
            n_ctx: 32,
            max_output_tokens: 8,
        },
    };
    let envelope = ExecutionEnvelope {
        origin: "origin.mdma.test".into(),
        destination: "executor.mdma.test".into(),
        request_id: vec![1; 16],
        model: descriptor.model.clone(),
        artifact_id: descriptor.artifact_id.clone(),
        profile_id: descriptor.profile_id.clone(),
        timeout_ms: 1000,
    };
    let request = SynthesizeRequest {
        envelope: Some(envelope.clone()),
        prompt: "hello".into(),
        max_tokens: 4,
        sampler: Some(SamplerConfig {
            temperature: Some(0.),
            ..Default::default()
        }),
    };
    let accepted = Accepted {
        request_id: envelope.request_id,
        executor: envelope.destination,
        model: envelope.model,
        artifact_id: envelope.artifact_id,
        profile_id: envelope.profile_id,
        sampler: Some(resolved_sampler(request.sampler.as_ref())),
        prompt_tokens: 3,
        max_tokens: 4,
        n_ctx: 32,
    };
    (
        request,
        descriptor,
        SynthesizeEvent {
            event: Some(Event::Accepted(accepted)),
        },
    )
}
fn token(index: u32) -> SynthesizeEvent {
    SynthesizeEvent {
        event: Some(Event::Token(Token {
            index,
            token_id: 42,
            text: String::new(),
        })),
    }
}
fn completed(count: u32, reason: &str) -> SynthesizeEvent {
    SynthesizeEvent {
        event: Some(Event::Completed(Completed {
            request_id: vec![1; 16],
            emitted_tokens: count,
            finish_reason: reason.into(),
        })),
    }
}

#[test]
fn valid_order_caps_eos_context_and_ok_are_all_required() {
    let (request, descriptor, accepted) = fixture();
    for (count, reason) in [(0, "eos"), (2, "eos"), (4, "max_tokens")] {
        let mut validator = Synthesis::new(&request, &descriptor).unwrap();
        validator.event(&accepted).unwrap();
        for i in 0..count {
            validator.event(&token(i)).unwrap();
        }
        validator.event(&completed(count, reason)).unwrap();
        validator.finish(Ok(())).unwrap();
    }
    let mut context = accepted;
    if let Some(Event::Accepted(a)) = context.event.as_mut() {
        a.n_ctx = 7;
    }
    let mut validator = Synthesis::new(&request, &descriptor).unwrap();
    validator.event(&context).unwrap();
    for i in 0..4 {
        validator.event(&token(i)).unwrap();
    }
    validator.event(&completed(4, "context_limit")).unwrap();
    validator.finish(Ok(())).unwrap();
}

#[test]
fn admission_mismatch_in_any_identity_or_effective_parameter_poison_stream() {
    let (request, descriptor, event) = fixture();
    let Some(Event::Accepted(original)) = event.event else {
        unreachable!()
    };
    let mut invalid = Vec::new();
    for mutate in [
        (|a: &mut Accepted| a.request_id[0] ^= 1) as fn(&mut Accepted),
        |a| a.executor = "other.mdma.test".into(),
        |a| a.model = "other".into(),
        |a| a.artifact_id.push('0'),
        |a| a.profile_id.push('0'),
        |a| a.sampler = None,
        |a| a.sampler.as_mut().unwrap().temperature = Some(0.8),
        |a| a.sampler.as_mut().unwrap().min_p = None,
        |a| a.sampler.as_mut().unwrap().top_p = Some(f32::NAN),
        |a| a.prompt_tokens = 0,
        |a| a.prompt_tokens = 32,
        |a| a.n_ctx = 33,
        |a| a.max_tokens = 0,
        |a| a.max_tokens = 5,
        |a| {
            a.n_ctx = 4;
            a.max_tokens = 2;
        },
    ] {
        let mut changed = original.clone();
        mutate(&mut changed);
        invalid.push(changed);
    }
    for changed in invalid {
        let mut validator = Synthesis::new(&request, &descriptor).unwrap();
        assert!(
            validator
                .event(&SynthesizeEvent {
                    event: Some(Event::Accepted(changed))
                })
                .is_err()
        );
        assert!(
            validator
                .event(&SynthesizeEvent {
                    event: Some(Event::Accepted(original.clone()))
                })
                .is_err()
        );
        assert!(validator.finish(Ok(())).is_err());
    }
}

#[test]
fn malformed_streams_never_become_success_or_authorize_a_retry() {
    let (request, descriptor, accepted) = fixture();
    for sequence in [
        vec![],
        vec![token(0)],
        vec![completed(0, "eos")],
        vec![accepted.clone()],
        vec![accepted.clone(), accepted.clone()],
        vec![accepted.clone(), token(1)],
        vec![accepted.clone(), token(0), token(0)],
        vec![accepted.clone(), completed(1, "eos")],
        vec![accepted.clone(), completed(0, "max_tokens")],
        vec![accepted.clone(), completed(0, "context_limit")],
        vec![accepted.clone(), completed(0, "invalid")],
        vec![accepted.clone(), completed(0, "eos"), token(0)],
        vec![accepted.clone(), completed(0, "eos"), completed(0, "eos")],
        vec![accepted.clone(), SynthesizeEvent { event: None }],
    ] {
        let mut validator = Synthesis::new(&request, &descriptor).unwrap();
        for event in sequence {
            if validator.event(&event).is_err() {
                break;
            }
        }
        assert!(validator.finish(Ok(())).is_err());
    }
    let mut validator = Synthesis::new(&request, &descriptor).unwrap();
    validator.event(&accepted).unwrap();
    validator.event(&completed(0, "eos")).unwrap();
    let error = validator
        .finish(Err(Status::unavailable("remote_failure")))
        .unwrap_err();
    assert_eq!(error.code(), rama::http::grpc::Code::Unavailable);
}

#[test]
fn oversized_events_token_overrun_and_wrong_completion_identity_are_rejected() {
    let (request, descriptor, accepted) = fixture();
    let mut validator = Synthesis::new(&request, &descriptor).unwrap();
    validator.event(&accepted).unwrap();
    let event = SynthesizeEvent {
        event: Some(Event::Token(Token {
            index: 0,
            token_id: 1,
            text: "x".repeat(65536),
        })),
    };
    assert!(validator.event(&event).is_err());
    let mut validator = Synthesis::new(&request, &descriptor).unwrap();
    validator.event(&accepted).unwrap();
    for i in 0..4 {
        validator.event(&token(i)).unwrap();
    }
    assert!(validator.event(&token(4)).is_err());
    let mut validator = Synthesis::new(&request, &descriptor).unwrap();
    validator.event(&accepted).unwrap();
    let mut wrong = completed(0, "eos");
    if let Some(Event::Completed(c)) = wrong.event.as_mut() {
        c.request_id[0] ^= 1;
    }
    assert!(validator.event(&wrong).is_err());
}

fn decide_fixture() -> (DecideRequest, ModelDescriptor, DecideResponse) {
    let (request, mut descriptor, _) = fixture();
    descriptor.kind = Kind::Laya;
    descriptor.limits = Limits::Decide {
        max_questions: 128,
        max_result_bytes: 524288,
    };
    let envelope = request.envelope.unwrap();
    let request = DecideRequest {
        envelope: Some(envelope.clone()),
        state_json: "{}".into(),
        questions_json: json!({
            "c": {"type": "choice", "instructions": "hello", "criteria": ["a", "b"]},
            "s": {"type": "score", "instructions": "hello", "criteria": ["bad", "good"]},
            "n": {"type": "noul", "instructions": "hello"}
        })
        .to_string(),
    };
    let result = json!({ "model": "laya-rl-agent", "usage": {"input_tokens": 64, "output_tokens": 0}, "answers": {
        "c": {"type": "choice", "choice": "a", "probabilities": {"a": 0.6, "b": 0.4},
            "confidence": 0.5, "action": {"act_probability": 0.5}},
        "s": {"type": "score", "score": 0.5, "probabilities": {"0": 0.5, "1": 0.5},
            "legend": {"0": "bad", "1": "good"}, "confidence": 0., "action": {"act_probability": 0.5}},
        "n": {"type": "noul", "noul": 0.7, "confidence": 0.7, "action": {"act_probability": 0.5}}
    }});
    let response = DecideResponse {
        request_id: envelope.request_id,
        executor: envelope.destination,
        model: envelope.model,
        artifact_id: envelope.artifact_id,
        profile_id: envelope.profile_id,
        result_json: result.to_string(),
    };
    (request, descriptor, response)
}

#[test]
fn unary_validates_exact_identity_bounds_and_typed_result_schema() {
    let (request, descriptor, response) = decide_fixture();
    let validator = Decision::new(&request, &descriptor).unwrap();
    validator.response(&response).unwrap();
    for mutate in [
        (|r: &mut DecideResponse| r.request_id[0] ^= 1) as fn(&mut DecideResponse),
        |r| r.executor.push('0'),
        |r| r.model.push('0'),
        |r| r.artifact_id.push('0'),
        |r| r.profile_id.push('0'),
        |r| r.result_json = "x".repeat(524289),
    ] {
        let mut changed = response.clone();
        mutate(&mut changed);
        assert!(validator.response(&changed).is_err());
    }
    for mutate in [
        (|v: &mut serde_json::Value| v["model"] = json!("other")) as fn(&mut serde_json::Value),
        |v| v["usage"]["output_tokens"] = json!(1),
        |v| v["answers"]["extra"] = json!({}),
        |v| {
            v["answers"].as_object_mut().unwrap().remove("n");
        },
        |v| v["answers"]["c"]["choice"] = json!("unknown"),
        |v| v["answers"]["c"]["probabilities"]["unknown"] = json!(0.),
        |v| v["answers"]["c"]["probabilities"]["a"] = json!(0.1),
        |v| v["answers"]["s"]["score"] = json!(2.),
        |v| v["answers"]["s"]["legend"]["0"] = json!("changed"),
        |v| v["answers"]["n"]["noul"] = json!(1.1),
        |v| v["answers"]["n"]["action"]["act_probability"] = json!(-0.1),
        |v| v["answers"]["n"]["confidence"] = json!(null),
        |v| v["answers"]["n"]["unknown"] = json!(true),
        |v| v["usage"]["unknown"] = json!(0),
    ] {
        let mut value = serde_json::from_str(&response.result_json).unwrap();
        mutate(&mut value);
        assert!(
            validator
                .response(&DecideResponse {
                    result_json: value.to_string(),
                    ..response.clone()
                })
                .is_err()
        );
    }
}

#[test]
fn unary_duplicate_keys_empty_questions_and_actual_native_output() {
    let (mut request, descriptor, response) = decide_fixture();
    let validator = Decision::new(&request, &descriptor).unwrap();
    for (needle, duplicate) in [
        ("\"a\":0.6", "\"a\":0.6,\"a\":0.6"),
        (
            "\"confidence\":0.5",
            "\"confidence\":0.5,\"confidence\":0.5",
        ),
        (
            "\"input_tokens\":64",
            "\"input_tokens\":64,\"input_tokens\":64",
        ),
        ("\"0\":\"bad\"", "\"0\":\"bad\",\"0\":\"bad\""),
    ] {
        assert!(response.result_json.contains(needle));
        assert!(
            validator
                .response(&DecideResponse {
                    result_json: response.result_json.replacen(needle, duplicate, 1),
                    ..response.clone()
                })
                .is_err()
        );
    }
    let dir = crate::laya::compat_tests::checkpoint();
    let predictor =
        crate::laya::Predictor::load(crate::backend::cpu::CpuBackend, dir.path()).unwrap();
    let questions: serde_json::Value = serde_json::from_str(&request.questions_json).unwrap();
    let result = predictor
        .predict(&json!({}), questions.as_object().unwrap())
        .unwrap();
    validator
        .response(&DecideResponse {
            result_json: result.to_string(),
            ..response.clone()
        })
        .unwrap();
    request.questions_json = "{}".into();
    let validator = Decision::new(&request, &descriptor).unwrap();
    let result = predictor
        .predict(&json!({}), &serde_json::Map::new())
        .unwrap();
    validator
        .response(&DecideResponse {
            result_json: result.to_string(),
            ..response
        })
        .unwrap();
}
