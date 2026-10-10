//! Always-on encoder checks with generated weights, no Laya heads or downloads.
use super::*;
use crate::backend::cpu::CpuBackend;
use safetensors::{Dtype, tensor::TensorView};
use serde_json::json;

pub(crate) struct Builder(Vec<(String, Vec<usize>, Vec<u8>)>);

impl Builder {
    pub(crate) fn tensor(&mut self, name: &str, shape: &[usize], norm: bool) {
        let seed = self.0.len() + 1;
        let values: Vec<f32> = (0..shape.iter().product()).map(|i| {
            let v = ((i * 17 + seed * 13) as f32 * 0.17).sin() * 0.15;
            if norm { 1.0 + v } else { v }
        }).collect();
        self.0.push((name.into(), shape.to_vec(), bytemuck::cast_slice(&values).to_vec()));
    }

    pub(crate) fn norm(&mut self, name: &str, bias: bool) {
        self.tensor(&format!("{name}.weight"), &[8], true);
        if bias { self.tensor(&format!("{name}.bias"), &[8], false); }
    }

    pub(crate) fn linear(&mut self, name: &str, rows: usize, cols: usize, bias: bool) {
        self.tensor(&format!("{name}.weight"), &[rows, cols], false);
        if bias { self.tensor(&format!("{name}.bias"), &[rows], false); }
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let views: Vec<_> = self.0.iter().map(|(name, shape, bytes)| {
            (name.as_str(), TensorView::new(Dtype::F32, shape.clone(), bytes).unwrap())
        }).collect();
        safetensors::serialize(views, None).unwrap()
    }
}

pub(crate) fn fixture(bias: bool) -> (Builder, EncoderConfig) {
    let encoder = EncoderConfig::from_json(&json!({
        "vocab_size": 32, "hidden_size": 8, "intermediate_size": 12,
        "num_hidden_layers": 3, "num_attention_heads": 2,
        "norm_bias": bias, "attention_bias": bias, "mlp_bias": bias,
        "local_attention": 2, "global_attn_every_n_layers": 2,
        "max_position_embeddings": 16
    }).to_string()).unwrap();
    let mut b = Builder(Vec::new());
    b.linear("encoder.embeddings.tok_embeddings", 32, 8, false);
    b.norm("encoder.embeddings.norm", bias);
    for i in 0..3 {
        let p = format!("encoder.layers.{i}");
        if i != 0 { b.norm(&format!("{p}.attn_norm"), bias); }
        b.linear(&format!("{p}.attn.Wqkv"), 24, 8, bias);
        b.linear(&format!("{p}.attn.Wo"), 8, 8, bias);
        b.norm(&format!("{p}.mlp_norm"), bias);
        b.linear(&format!("{p}.mlp.Wi"), 24, 8, bias);
        b.linear(&format!("{p}.mlp.Wo"), 8, 12, bias);
    }
    b.norm("encoder.final_norm", bias);
    (b, encoder)
}

fn hidden(model: &ModernBert<CpuBackend>, ids: &[u32], lens: &[u32]) -> Vec<f32> {
    let mut ws = model.alloc_workspace(&CpuBackend, lens.len(), ids.len() / lens.len()).unwrap();
    model.forward(&CpuBackend, &mut ws, ids, lens).unwrap();
    let mut values = vec![0f32; ids.len() * model.config().hidden_size];
    CpuBackend.download(ws.hidden_states(), 0, bytemuck::cast_slice_mut(&mut values)).unwrap();
    values
}

fn close(got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len());
    for (&got, &want) in got.iter().zip(want) {
        assert!(got.is_finite() && (got - want).abs() < 2e-5, "{got} != {want}");
    }
}

#[test]
fn modernbert_compat_standalone_padding_and_prefixes() {
    let ids = [1, 3, 7, 2, 4, 9, 2, 6, 3, 8, 0, 0];
    for bias in [false, true] {
        let (mut fixture, config) = fixture(bias);
        let bytes = fixture.serialize();
        let w = Weights::parse(&bytes).unwrap();
        assert!(w.names().iter().all(|name| name.starts_with("encoder.")));
        let model = ModernBert::load(&CpuBackend, &w, config.clone(), "encoder").unwrap();
        let batched = hidden(&model, &ids, &[6, 4]);
        close(&batched[..6 * 8], &hidden(&model, &ids[..6], &[6]));
        close(&batched[6 * 8..10 * 8], &hidden(&model, &ids[6..10], &[4]));

        let mut padded = ids;
        padded[10..].copy_from_slice(&[11, 12]);
        close(&batched[..10 * 8], &hidden(&model, &padded, &[6, 4])[..10 * 8]);
        for (name, _, _) in &mut fixture.0 {
            *name = name.strip_prefix("encoder.").unwrap().to_owned();
        }
        let bytes = fixture.serialize();
        let plain = ModernBert::load(&CpuBackend, &Weights::parse(&bytes).unwrap(), config, "").unwrap();
        close(&batched, &hidden(&plain, &ids, &[6, 4]));
    }
}

#[test]
fn modernbert_compat_explicit_attention_and_rope_config() {
    let (fixture, config) = fixture(true);
    let bytes = fixture.serialize();
    let weights = Weights::parse(&bytes).unwrap();
    let model = ModernBert::load(&CpuBackend, &weights, config.clone(), "encoder").unwrap();
    let mut explicit = config;
    explicit.layer_types = Some(["full_attention", "sliding_attention", "full_attention"].map(str::to_owned).to_vec());
    explicit.rope_parameters = Some(serde_json::from_value(json!({
        "full_attention": {"rope_type": "default", "rope_theta": 160000.0},
        "sliding_attention": {"rope_type": "default", "rope_theta": 10000.0}
    })).unwrap());
    // Explicit metadata, not these fallbacks, must control the encoder.
    explicit.global_attn_every_n_layers = 1;
    explicit.global_rope_theta = 10.0;
    explicit.local_rope_theta = 20.0;
    assert_eq!(explicit.layer_kind(1), LayerKind::Sliding);
    assert_eq!(explicit.rope_theta(LayerKind::Full), 160000.0);
    assert_eq!(explicit.rope_theta(LayerKind::Sliding), 10000.0);
    let model_explicit = ModernBert::load(&CpuBackend, &weights, explicit.clone(), "encoder").unwrap();
    let ids = [1, 3, 7, 2, 4, 9];
    let expected = hidden(&model, &ids, &[6]);
    close(&expected, &hidden(&model_explicit, &ids, &[6]));

    explicit.layer_types = Some(vec!["sliding_attention".into(); 3]);
    let local = ModernBert::load(&CpuBackend, &weights, explicit, "encoder").unwrap();
    assert!(expected.iter().zip(hidden(&local, &ids, &[6])).any(|(a, b)| (a - b).abs() > 1e-4));
}

#[test]
fn modernbert_compat_rejects_invalid_inputs_and_missing_tensors() {
    let (mut fixture, config) = fixture(false);
    let bytes = fixture.serialize();
    let model = ModernBert::load(&CpuBackend, &Weights::parse(&bytes).unwrap(), config.clone(), "encoder").unwrap();
    assert!(model.alloc_workspace(&CpuBackend, 0, 4).is_err());
    assert!(model.alloc_workspace(&CpuBackend, 1, 0).is_err());
    assert!(model.alloc_workspace(&CpuBackend, 1, 17).is_err());
    let mut ws = model.alloc_workspace(&CpuBackend, 1, 4).unwrap();
    for (ids, lens) in [
        (vec![1, 2, 3], vec![3]),
        (vec![1, 2, 3, 4], vec![]),
        (vec![1, 2, 3, 4], vec![5]),
        (vec![1, 2, 3, 4], vec![0]),
        (vec![1, 2, 3, 32], vec![4]),
    ] {
        assert!(model.forward(&CpuBackend, &mut ws, &ids, &lens).is_err());
    }
    // A rejected call must leave the workspace usable.
    model.forward(&CpuBackend, &mut ws, &[1, 2, 3, 4], &[4]).unwrap();
    fixture.0.retain(|(name, _, _)| name != "encoder.layers.1.attn.Wqkv.weight");
    let bytes = fixture.serialize();
    assert!(ModernBert::load(&CpuBackend, &Weights::parse(&bytes).unwrap(), config, "encoder").is_err());
}

#[test]
fn modernbert_compat_config_defaults_and_validation() {
    let base = json!({
        "vocab_size": 32, "hidden_size": 8, "intermediate_size": 12,
        "num_hidden_layers": 3, "num_attention_heads": 2
    });
    let config = EncoderConfig::from_json(&base.to_string()).unwrap();
    assert_eq!(config.model_type, "modernbert");
    assert_eq!(config.layer_kind(0), LayerKind::Full);
    assert_eq!(config.layer_kind(1), LayerKind::Sliding);
    assert_eq!(config.rope_theta(LayerKind::Full), 160000.0);
    assert_eq!(config.rope_theta(LayerKind::Sliding), 10000.0);
    for (key, value) in [
        ("model_type", json!("bert")),
        ("hidden_activation", json!("relu")),
        ("num_attention_heads", json!(0)),
        ("num_attention_heads", json!(3)),
        ("layer_types", json!(["full_attention"])),
        ("layer_types", json!(["unknown", "unknown", "unknown"])),
        ("rope_parameters", json!({"full_attention": {"rope_type": "linear"}})),
    ] {
        let mut invalid = base.clone();
        invalid[key] = value;
        assert!(EncoderConfig::from_json(&invalid.to_string()).is_err(), "{key}");
    }
}
