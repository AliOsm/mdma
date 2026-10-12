use super::*;
use crate::backend::cpu::CpuBackend;
use crate::gguf::tests::Fixture;
use crate::memory::NativeMemoryBudget;
use crate::model::memory::weight_bytes;

#[test]
fn sizes_match_owned_buffers_and_outlive_the_model() {
    let file = Fixture::new(2, 8).write();
    let mut gguf = GgufModel::open(file.path()).unwrap();
    let weights = weight_bytes(&gguf).unwrap();
    let budget = NativeMemoryBudget::new(weights + 100_000);
    let model = Model::load_with_budget(&CpuBackend, &mut gguf, &budget).unwrap();
    assert_eq!(budget.used(), weights);
    let Decoder::Llama(llama) = &model.architecture else {
        panic!("expected Llama");
    };
    let stored_weights = model.token_embd.buf.0.len()
        + model.output_weight().buf.0.len()
        + model.output_norm.0.len()
        + llama
            .layers
            .iter()
            .map(|layer| {
                layer.attn_norm.0.len()
                    + layer.ffn_norm.0.len()
                    + [
                        &layer.wq,
                        &layer.wk,
                        &layer.wv,
                        &layer.wo,
                        &layer.w_gate,
                        &layer.w_up,
                        &layer.w_down,
                    ]
                    .iter()
                    .map(|weight| weight.buf.0.len())
                    .sum::<usize>()
            })
            .sum::<usize>();
    assert_eq!(weights, stored_weights);
    let kv = model.new_kv_cache(&CpuBackend, 7).unwrap();
    let kv_bytes: usize = (0..2)
        .map(|layer| kv.view(layer).k.0.len() + kv.view(layer).v.0.len())
        .sum();
    assert_eq!(model.kv_bytes(&CpuBackend, 7).unwrap(), kv_bytes);
    let ws = model.alloc_workspace(&CpuBackend, 5).unwrap();
    let scratch: usize = [
        &ws.x,
        &ws.x_alt,
        &ws.normed,
        &ws.q,
        &ws.k,
        &ws.v,
        &ws.attn_out,
        &ws.proj,
        &ws.gate,
        &ws.up,
        &ws.act,
        &ws.last_row,
        &ws.logits,
    ]
    .iter()
    .map(|buf| buf.0.len())
    .sum();
    assert_eq!(model.workspace_bytes(5).unwrap(), scratch);
    assert_eq!(budget.used(), weights + kv_bytes + scratch);
    drop(model);
    assert_eq!(budget.used(), kv_bytes + scratch);
    drop(ws);
    assert_eq!(budget.used(), kv_bytes);
    drop(kv);
    assert_eq!(budget.used(), 0);
}

pub(in crate::model) fn tiny_model<B: Backend>(backend: &B) -> Model<B> {
    use crate::backend::Pool;
    use crate::gguf::GgmlDType;
    let floats = |data: Vec<f32>| {
        let mut buf = backend.alloc(data.len() * 4, Pool::Weights).unwrap();
        backend
            .upload(&mut buf, 0, bytemuck::cast_slice(&data))
            .unwrap();
        buf
    };
    let weight = |rows, cols, seed| QWeight {
        dtype: GgmlDType::F32,
        shape: vec![rows, cols],
        buf: floats(
            (0..rows * cols)
                .map(|i| ((i + seed) as f32 * 0.37).sin() * 0.2)
                .collect(),
        ),
    };
    Model {
        config: ModelConfig {
            n_layers: 1,
            n_embd: 8,
            n_ff: 16,
            n_heads: 2,
            n_kv_heads: 1,
            head_dim: 4,
            rope_theta: 10000.0,
            rms_eps: 1e-5,
            n_ctx: 64,
        },
        token_embd: weight(128, 8, 3),
        architecture: Decoder::Llama(Llama {
            layers: vec![Layer {
                attn_norm: floats(vec![1.0; 8]),
                wq: weight(8, 8, 5),
                wk: weight(4, 8, 7),
                wv: weight(4, 8, 11),
                wo: weight(8, 8, 13),
                ffn_norm: floats(vec![1.0; 8]),
                w_gate: weight(16, 8, 17),
                w_up: weight(16, 8, 19),
                w_down: weight(8, 16, 23),
            }],
        }),
        output_norm: floats(vec![1.0; 8]),
        output: Some(weight(128, 8, 29)),
        memory: None,
    }
}
