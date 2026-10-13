use candle_core::{
    Device, Tensor,
    quantized::{
        GgmlDType, QTensor,
        gguf_file::{self, Value},
    },
};
use std::collections::BTreeMap;

pub fn model(width: usize) -> tempfile::NamedTempFile {
    build(width, |_| {})
}

/// The same weights, shaped like Bespoke-MiniCheck: named and tagged as it,
/// with a ChatML template whose markers and `Yes`/`yes` take the byte
/// slots 1 to 4 (control bytes no test text uses).
#[allow(dead_code)] // Not every test binary checks claims.
pub fn minicheck(width: usize) -> tempfile::NamedTempFile {
    build(width, |metadata| {
        let Some(Value::Array(tokens)) = metadata.get_mut("tokenizer.ggml.tokens") else {
            unreachable!("build lists the tokens")
        };
        for (id, piece) in [
            (1, "<|im_start|>"),
            (2, "<|im_end|>"),
            (3, "Yes"),
            (4, "yes"),
        ] {
            tokens[id] = Value::String(piece.into());
        }
        let types = (0..128)
            .map(|id| match id {
                1 | 2 => Value::I32(3),
                3 | 4 => Value::I32(1),
                _ => Value::I32(6),
            })
            .collect();
        for (key, value) in [
            ("tokenizer.ggml.token_type", Value::Array(types)),
            (
                "tokenizer.chat_template",
                Value::String(
                    "{% for m in messages %}<|im_start|>{{ m['role'] }}\n{{ m['content'] }}<|im_end|>\n{% endfor %}".into(),
                ),
            ),
            ("general.basename", Value::String("Bespoke-MiniCheck".into())),
            // Byte tokens: the system prompt alone is ~400 of them.
            ("internlm2.context_length", Value::U32(1024)),
            (
                "general.tags",
                Value::Array(vec![Value::String("text-classification".into())]),
            ),
        ] {
            metadata.insert(key.into(), value);
        }
    })
}

fn build(width: usize, edit: impl FnOnce(&mut BTreeMap<String, Value>)) -> tempfile::NamedTempFile {
    let mut metadata = BTreeMap::from([
        (
            "general.architecture".to_owned(),
            Value::String("internlm2".into()),
        ),
        ("internlm2.rope.freq_base".into(), Value::F32(10000.)),
        (
            "internlm2.attention.layer_norm_rms_epsilon".into(),
            Value::F32(1e-5),
        ),
        ("tokenizer.ggml.model".into(), Value::String("llama".into())),
        (
            "tokenizer.ggml.tokens".into(),
            Value::Array(
                (0..128)
                    .map(|i| Value::String(format!("<0x{i:02X}>")))
                    .collect(),
            ),
        ),
        (
            "tokenizer.ggml.scores".into(),
            Value::Array(vec![Value::F32(0.); 128]),
        ),
        ("tokenizer.ggml.add_space_prefix".into(), Value::Bool(false)),
    ]);
    for (key, value) in [
        ("block_count", 2),
        ("embedding_length", width),
        ("feed_forward_length", width * 2),
        ("attention.head_count", 2),
        ("attention.head_count_kv", 1),
        ("context_length", 64),
    ] {
        metadata.insert(format!("internlm2.{key}"), Value::U32(value as u32));
    }
    edit(&mut metadata);
    let weight = |shape: &[usize], seed: usize| {
        let values: Vec<f32> = (0..shape.iter().product())
            .map(|i| {
                if shape.len() == 1 {
                    1.
                } else {
                    ((i + seed) as f32 * 0.37).sin() * 0.2
                }
            })
            .collect();
        QTensor::quantize(
            &Tensor::from_vec(values, shape, &Device::Cpu).unwrap(),
            GgmlDType::F32,
        )
        .unwrap()
    };
    let mut tensors = vec![
        ("token_embd.weight".into(), weight(&[128, width], 3)),
        ("output_norm.weight".into(), weight(&[width], 0)),
        ("output.weight".into(), weight(&[128, width], 29)),
    ];
    for i in 0..2 {
        for (name, shape, seed) in [
            ("attn_norm", vec![width], 0),
            ("ffn_norm", vec![width], 0),
            ("attn_q", vec![width, width], 5),
            ("attn_k", vec![width / 2, width], 7),
            ("attn_v", vec![width / 2, width], 11),
            ("attn_output", vec![width, width], 13),
            ("ffn_gate", vec![width * 2, width], 17),
            ("ffn_up", vec![width * 2, width], 19),
            ("ffn_down", vec![width, width * 2], 23),
        ] {
            tensors.push((format!("blk.{i}.{name}.weight"), weight(&shape, seed)));
        }
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let metadata: Vec<_> = metadata.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let tensors: Vec<_> = tensors.iter().map(|(k, v)| (k.as_str(), v)).collect();
    gguf_file::write(file.as_file_mut(), &metadata, &tensors).unwrap();
    file
}
