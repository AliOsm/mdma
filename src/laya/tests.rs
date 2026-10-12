use super::Predictor;
use super::calibrate::{clamp_temperature, confidence, temp_bucket};
use super::prompt::{Item, Kind, Question, collate, collate_iter};
use super::pyjson::dumps;
use crate::backend::cpu::CpuBackend;
use serde_json::{Value, json};
use std::path::PathBuf;

/// The convaiinnovations/laya snapshot the reference fixture was generated
/// from; select its directory with `MDMA_LAYA_PATH`. Run explicitly:
/// `cargo test --release --lib laya -- --ignored`.
fn checkpoint() -> PathBuf {
    let path = std::env::var_os("MDMA_LAYA_PATH")
        .filter(|path| !path.is_empty())
        .expect("set MDMA_LAYA_PATH to the pinned convaiinnovations/laya checkpoint directory");
    PathBuf::from(path)
}

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/fixtures/laya/reference.json"
    )))
    .unwrap()
}

#[test]
fn pyjson_matches_python_dumps_separators_and_escapes() {
    let v = json!({"a": [1, 2.5, true, null], "é": "x\ny\"z", "n": {"k": "v"}});
    assert_eq!(
        dumps(&v, false),
        r#"{"a": [1, 2.5, true, null], "é": "x\ny\"z", "n": {"k": "v"}}"#
    );
    assert_eq!(dumps(&json!("é𝄞"), true), r#""\u00e9\ud834\udd1e""#);
    assert_eq!(dumps(&json!(1e20), false), "1e+20");
}

#[test]
fn pyjson_preserves_integer_precision_and_finite_float_formatting() {
    for (value, expected) in [
        (json!(u64::MAX), "18446744073709551615"),
        (json!(i64::MIN), "-9223372036854775808"),
        (json!(0), "0"),
        (json!(-0.0), "-0.0"),
        (json!(1.0), "1.0"),
        (json!(-1e20), "-1e+20"),
        (json!(1e-20), "1e-20"),
    ] {
        assert_eq!(dumps(&value, false), expected);
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(serde_json::Number::from_f64(value).is_none());
    }
}

#[test]
fn question_rendering_follows_laya_mlx() {
    let q = Question::from_json(&json!({"type": "choice", "instructions": "Who?", "criteria": {"zero": 0, "no": false, "none": null, "blank": "", "d": {"desc": "x"}}})).unwrap();
    assert_eq!(
        q.options,
        [
            "zero: 0",
            "no: false",
            "none",
            "blank",
            r#"d: {"desc": "x"}"#
        ]
    );
    assert_eq!(q.labels, ["zero", "no", "none", "blank", "d"]);

    let q = Question::from_json(
        &json!({"type": "choice", "instructions": "Who?", "criteria": ["second", "first"]}),
    )
    .unwrap();
    assert_eq!(q.options, ["second", "first"]);
    assert_eq!(q.labels, q.options);

    let q = Question::from_json(&json!({"type": "score", "instructions": {"task": "rate"}, "criteria": [{"level": "low"}, "high"]})).unwrap();
    assert_eq!(q.instructions, r#"{"task": "rate"}"#);
    assert_eq!(q.options, [r#"level 0: {"level": "low"}"#, "level 1: high"]);
    assert_eq!(q.legend, [json!({"level": "low"}), json!("high")]);

    let q = Question::from_json(&json!({"type": "noul", "instructions": "Refund?", "criteria": {"true": {"reason": "money back"}}})).unwrap();
    assert_eq!(
        q.options,
        [
            "false: no, the statement does not hold",
            r#"true: {"reason": "money back"}"#
        ]
    );

    for bad in [
        json!({"type": "choice", "instructions": "x", "criteria": []}),
        json!({"type": "choice", "instructions": "x", "criteria": ["a", "a"]}),
        json!({"type": "choice", "instructions": "x", "criteria": ["a", 1]}),
        json!({"type": "choice", "instructions": "x", "criteria": {}}),
        json!({"type": "score", "instructions": "x", "criteria": []}),
        json!({"type": "noul", "instructions": "x", "criteria": [1]}),
        json!({"type": "other", "instructions": "x"}),
        json!({"type": "noul"}),
        json!("not a dict"),
    ] {
        assert!(Question::from_json(&bad).is_err(), "{bad}");
    }
}

#[test]
fn calibration_helpers() {
    assert_eq!(temp_bucket(Kind::Choice, 2), "choice:2");
    assert_eq!(temp_bucket(Kind::Score, 5), "score:3-5");
    assert_eq!(temp_bucket(Kind::Choice, 10), "choice:6-10");
    assert_eq!(temp_bucket(Kind::Noul, 11), "noul:11+");
    assert_eq!(clamp_temperature(0.1006), 0.5);
    assert_eq!(clamp_temperature(7.0), 5.0);
    assert_eq!(clamp_temperature(f32::NAN), 1.0);
    assert_eq!(confidence(&[1.0]), 1.0);
    assert!((confidence(&[0.5, 0.5])).abs() < 1e-6);
    assert!((confidence(&[1.0, 0.0]) - 1.0).abs() < 1e-6);
}

#[test]
fn collate_pads_right_and_keeps_at_least_two_marker_slots() {
    let items = [
        Item {
            ids: vec![1, 2, 3],
            markers: vec![1],
            kind: Kind::Choice,
        },
        Item {
            ids: vec![4],
            markers: vec![0, 0, 0],
            kind: Kind::Noul,
        },
    ];
    let b = collate(&items, 9).unwrap();
    assert_eq!((b.n, b.seq_len, b.kmax), (2, 3, 3));
    assert_eq!(b.ids, [1, 2, 3, 4, 9, 9]);
    assert_eq!(b.lens, [3, 1]);
    assert_eq!(b.markers, [1, 0, 0, 0, 0, 0]);
    assert_eq!(b.counts, [1, 3]);
    assert!(collate(&[], 0).is_err());
}

#[test]
fn collate_borrowed_items_preserves_order_and_padding() {
    let items = [
        (
            "long",
            Item {
                ids: vec![1, 2, 3],
                markers: vec![1],
                kind: Kind::Choice,
            },
        ),
        (
            "short",
            Item {
                ids: vec![4],
                markers: Vec::new(),
                kind: Kind::Score,
            },
        ),
    ];
    let batch = collate_iter(items.iter().rev().map(|(_, item)| item), 9).unwrap();
    assert_eq!((batch.n, batch.seq_len, batch.kmax), (2, 3, 2));
    assert_eq!(batch.ids, [4, 9, 9, 1, 2, 3]);
    assert_eq!(batch.lens, [1, 3]);
    assert_eq!(batch.markers, [0, 0, 1, 0]);
    assert_eq!(batch.counts, [0, 1]);
    assert_eq!(batch.kinds, [Kind::Score, Kind::Choice]);
    assert_eq!(items[0].1.ids, [1, 2, 3]);
    assert!(collate_iter(std::iter::empty(), 0).is_err());
}

fn load() -> Predictor<CpuBackend> {
    let dir = checkpoint();
    assert!(
        dir.join("model.safetensors").is_file(),
        "Laya checkpoint not found: {}",
        dir.display()
    );
    Predictor::load(CpuBackend, &dir).expect("load checkpoint")
}

#[test]
#[ignore = "requires MDMA_LAYA_PATH; pinned Laya checkpoint"]
fn prompts_match_laya_mlx_token_for_token() {
    let p = load();
    for case in fixture()["cases"].as_array().unwrap() {
        let prepared = p
            .prepare(&case["state"], case["questions"].as_object().unwrap())
            .expect("prepare");
        let items = case["items"].as_array().unwrap();
        assert_eq!(prepared.len(), items.len(), "{}", case["name"]);
        for ((q, item), want) in prepared.iter().zip(items) {
            let ids: Vec<u32> = want["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            let markers: Vec<usize> = want["markers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as usize)
                .collect();
            assert_eq!(item.ids, ids, "{} ids", case["name"]);
            assert_eq!(item.markers, markers, "{} markers", case["name"]);
            assert_eq!(u64::from(q.kind.index()), want["qtype"].as_u64().unwrap());
        }
    }
}

fn check_case(p: &Predictor<CpuBackend>, case: &Value) {
    let name = case["name"].as_str().unwrap();
    let questions = case["questions"].as_object().unwrap();
    let prepared = p.prepare(&case["state"], questions).unwrap();
    let batch = collate_iter(prepared.iter().map(|(_, it)| it), 50283).unwrap();
    let mut ws = p
        .model
        .alloc_workspace(&p.backend, batch.n, batch.seq_len, batch.kmax)
        .unwrap();
    let out = p.model.forward(&p.backend, &mut ws, &batch).unwrap();
    let mut worst = 0f32;
    for (row, want) in case["items"].as_array().unwrap().iter().enumerate() {
        let logits: Vec<f32> = want["logits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let act: Vec<f32> = want["act"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        for (i, (&got, &ref_)) in out.logits[row].iter().zip(&logits).enumerate() {
            worst = worst.max((got - ref_).abs());
            assert!(
                (got - ref_).abs() < 2e-3,
                "{name} row {row} logit {i}: got {got}, want {ref_}"
            );
        }
        for (i, (&got, &ref_)) in out.act[row].iter().zip(&act).enumerate() {
            assert!(
                (got - ref_).abs() < 0.5 + ref_.abs() * 1e-4,
                "{name} row {row} act {i}: got {got}, want {ref_}"
            );
        }
    }
    let result = p.predict(&case["state"], questions).unwrap();
    for (qid, want) in case["answers"].as_object().unwrap() {
        let got = &result["answers"][qid];
        assert_eq!(got["type"], want["type"], "{name}/{qid}");
        for key in ["choice", "legend"] {
            assert_eq!(got.get(key), want.get(key), "{name}/{qid} {key}");
        }
        for key in ["confidence", "score", "noul"] {
            if let Some(w) = want.get(key).and_then(Value::as_f64) {
                let g = got[key].as_f64().unwrap();
                assert!(
                    (g - w).abs() <= 2e-4,
                    "{name}/{qid} {key}: got {g}, want {w}"
                );
            }
        }
        if let Some(probs) = want.get("probabilities") {
            for (label, w) in probs.as_object().unwrap() {
                let g = got["probabilities"][label].as_f64().unwrap();
                assert!(
                    (g - w.as_f64().unwrap()).abs() <= 2e-4,
                    "{name}/{qid} p[{label}]: got {g}, want {w}"
                );
            }
        }
        let (g, w) = (
            got["action"]["act_probability"].as_f64().unwrap(),
            want["action"]["act_probability"].as_f64().unwrap(),
        );
        assert!(
            (g - w).abs() <= 2e-4,
            "{name}/{qid} act_probability: got {g}, want {w}"
        );
    }
    eprintln!(
        "{name}: {} questions, seq_len {}, max |Δlogit| {worst:.2e}",
        batch.n, batch.seq_len
    );
}

/// The README email example: 3 questions against upstream PyTorch fp32.
#[test]
#[ignore = "requires MDMA_LAYA_PATH; heavyweight pinned Laya checkpoint"]
fn email_case_matches_upstream_pytorch() {
    let p = load();
    let fx = fixture();
    let case = fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "email")
        .unwrap();
    check_case(&p, case);
}

/// All 16 parity cases (63 questions): raw logits, action logits and the
/// public answers against upstream PyTorch fp32. Slow on the scalar CPU
/// backend.
#[test]
#[ignore = "requires MDMA_LAYA_PATH; heavyweight pinned Laya checkpoint"]
fn all_cases_match_upstream_pytorch() {
    let p = load();
    for case in fixture()["cases"].as_array().unwrap() {
        check_case(&p, case);
    }
}
