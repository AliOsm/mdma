//! Temperature calibration, entropy confidence and per-question answers.
use super::prompt::{Kind, Question};
use serde_json::{Map, Value, json};

pub const TEMP_MIN: f32 = 0.5;
pub const TEMP_MAX: f32 = 5.0;

/// Clamp to [0.5, 5.0]; non-finite values fall back to 1.0.
pub fn clamp_temperature(t: f32) -> f32 {
    if t.is_finite() {
        t.clamp(TEMP_MIN, TEMP_MAX)
    } else {
        1.0
    }
}

pub fn temp_bucket(kind: Kind, k: usize) -> String {
    let size = match k {
        0..=2 => "2",
        3..=5 => "3-5",
        6..=10 => "6-10",
        _ => "11+",
    };
    format!("{}:{size}", kind.name())
}

pub fn softmax(z: &[f32]) -> Vec<f32> {
    let max = z.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let e: Vec<f32> = z.iter().map(|v| (v - max).exp()).collect();
    let sum: f32 = e.iter().sum();
    e.into_iter().map(|v| v / sum).collect()
}

/// `1 - H(p) / log(k)`, clipped to [0, 1]; 1.0 for fewer than two options.
pub fn confidence(p: &[f32]) -> f32 {
    let k = p.len();
    if k < 2 {
        return 1.0;
    }
    let ent: f64 = -p
        .iter()
        .map(|&v| {
            let v = f64::from(v).clamp(1e-12, 1.0);
            v * v.ln()
        })
        .sum::<f64>();
    (1.0 - ent / (k as f64).ln()).clamp(0.0, 1.0) as f32
}

pub fn round4(x: f32) -> f64 {
    (f64::from(x) * 1e4).round() / 1e4
}

/// The answer for one question from its calibrated distribution and the
/// action head's `P(act)`.
pub fn answer(q: &Question, p: &[f32], act_probability: f32) -> Value {
    let k = p.len();
    let mut out = Map::new();
    out.insert("type".into(), json!(q.kind.name()));
    out.insert("confidence".into(), json!(round4(confidence(p))));
    out.insert(
        "action".into(),
        json!({ "act_probability": round4(act_probability) }),
    );
    match q.kind {
        Kind::Choice => {
            let best = (0..k).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap_or(0);
            let best = (0..k).find(|&i| p[i] == p[best]).unwrap_or(best);
            out.insert("choice".into(), json!(q.labels[best]));
            out.insert(
                "probabilities".into(),
                q.labels
                    .iter()
                    .zip(p)
                    .map(|(l, &v)| (l.clone(), json!(round4(v))))
                    .collect::<Map<_, _>>()
                    .into(),
            );
        }
        Kind::Score => {
            let score: f32 = p.iter().enumerate().map(|(i, &v)| i as f32 * v).sum();
            out.insert("score".into(), json!(round4(score)));
            out.insert(
                "legend".into(),
                q.legend
                    .iter()
                    .enumerate()
                    .map(|(i, l)| (i.to_string(), l.clone()))
                    .collect::<Map<_, _>>()
                    .into(),
            );
            out.insert(
                "probabilities".into(),
                p.iter()
                    .enumerate()
                    .map(|(i, &v)| (i.to_string(), json!(round4(v))))
                    .collect::<Map<_, _>>()
                    .into(),
            );
        }
        Kind::Noul => {
            let p_true = p.get(1).copied().unwrap_or(0.0);
            out.insert("noul".into(), json!(round4(p_true)));
            out.insert("confidence".into(), json!(round4(p_true.max(1.0 - p_true))));
        }
    }
    Value::Object(out)
}
