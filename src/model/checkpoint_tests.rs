//! Optional checkpoint parity gates and ROCm diagnostics; default tests are in compat_tests.
//! Reference: llama.cpp 552f18f912a32ea86edf82e2b76431cb7131538d,
//! Metal, greedy, context 2048, microbatch 1, f32 KV, flash attention disabled. The llama.cpp
//! CPU quantized-activation path differs on TinyLlama arithmetic.
use super::*;
use crate::{backend::cpu::CpuBackend, sampler::greedy, tokenizer::Tokenizer};

struct Case {
    prompt: &'static str,
    input: &'static [u32],
    output: &'static [u32],
    first_logprobs: &'static [(usize, f64)],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reference {
    Strict,
    #[cfg(feature = "rocm")]
    Tokens,
    Diagnostic,
}

#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and MDMA_TEST_GEMMA4_GGUF; metadata only"]
fn checkpoint_metadata_preflight() {
    for variable in ["MDMA_TEST_TINYLLAMA_GGUF", "MDMA_TEST_GEMMA4_GGUF"] {
        let gguf = GgufModel::open(std::env::var(variable).expect(variable)).unwrap();
        Model::validate(&CpuBackend, &gguf).unwrap();
        assert!(memory::weight_bytes(&gguf).unwrap() > 0);
        if variable == "MDMA_TEST_GEMMA4_GGUF" {
            gemma_contract(&gguf);
        }
    }
}

/// Google text config, with llama.cpp's conversion conventions applied.
/// Gemma4Model.norm_shift is 0 (do not add 1 to GGUF norm weights).
/// Shared-layer FFNs double; global proportional RoPE uses rope_freqs.weight
/// to leave 3/4 of each head unrotated rather than shrinking the GGUF head.
fn gemma_contract(g: &GgufModel) {
    let contract: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/fixtures/model/gemma4-contract.json"
    )))
    .unwrap();
    let c = &contract["text_config"];
    for (key, field) in [
        ("embedding_length", "hidden_size"),
        (
            "embedding_length_per_layer_input",
            "hidden_size_per_layer_input",
        ),
        ("block_count", "num_hidden_layers"),
        ("attention.head_count", "num_attention_heads"),
        ("attention.head_count_kv", "num_key_value_heads"),
        ("attention.shared_kv_layers", "num_kv_shared_layers"),
        ("attention.key_length_swa", "head_dim"),
        ("attention.key_length", "global_head_dim"),
        ("attention.sliding_window", "sliding_window"),
        ("context_length", "max_position_embeddings"),
    ] {
        assert_eq!(
            u64::from(g.metadata_u32(&format!("gemma4.{key}")).unwrap()),
            c[field].as_u64().unwrap(),
            "{key}"
        );
    }
    let (model, gemma) = gemma4::Config::read(g).unwrap();
    assert_eq!(
        gemma.local,
        (0..model.n_layers).map(|i| i % 5 != 4).collect::<Vec<_>>()
    );
    let ff = c["intermediate_size"].as_u64().unwrap() as usize;
    assert_eq!(
        gemma.ffn,
        (0..model.n_layers)
            .map(|i| if i < gemma.producers { ff } else { 2 * ff })
            .collect::<Vec<_>>()
    );
    assert_eq!(model.rms_eps, c["rms_norm_eps"].as_f64().unwrap() as f32);
    assert_eq!(
        gemma.softcap,
        c["final_logit_softcapping"].as_f64().unwrap() as f32
    );
    assert_eq!(
        model.rope_theta,
        c["rope_parameters"]["full_attention"]["rope_theta"]
            .as_f64()
            .unwrap() as f32
    );
    assert_eq!(
        gemma.local_theta,
        c["rope_parameters"]["sliding_attention"]["rope_theta"]
            .as_f64()
            .unwrap() as f32
    );
    assert!(
        g.tensor_desc("output.weight").is_none(),
        "pinned checkpoint has tied embeddings"
    );
}

const TINY: &[Case] = &[
    Case {
        prompt: "The capital of France is",
        input: &[1, 450, 7483, 310, 3444, 338],
        first_logprobs: &[
            (3681, -0.981140),
            (5982, -1.529333),
            (29901, -3.282010),
            (29973, -3.553754),
            (29889, -3.674513),
        ],
        output: &[
            3681, 29889, 13, 13, 29906, 29889, 350, 29889, 29907, 29889, 450, 7483, 310, 12297,
            9184, 471,
        ],
    },
    Case {
        prompt: "Once upon a time",
        input: &[1, 9038, 2501, 263, 931],
        first_logprobs: &[
            (29892, -1.197046),
            (297, -1.335549),
            (856, -2.848663),
            (29889, -2.980360),
            (322, -3.742365),
        ],
        output: &[
            29892, 727, 471, 263, 4123, 6114, 4257, 365, 2354, 29889, 365, 2354, 471, 263, 2924,
            322, 9914, 10752, 29892, 2337, 3063, 363, 5837, 304, 1371, 4045, 29889, 2296, 750, 263,
            15935, 363,
        ],
    },
    Case {
        prompt: "2 + 2 =",
        input: &[1, 29871, 29906, 718, 29871, 29906, 353],
        first_logprobs: &[
            (29871, -0.175483),
            (1577, -2.564544),
            (29973, -3.851336),
            (903, -4.284522),
            (13626, -5.270587),
        ],
        output: &[
            29871, 29946, 13, 13, 1576, 2533, 310, 278, 937, 1023, 4958, 310, 278, 5665, 338, 29871,
        ],
    },
    Case {
        prompt: "<|user|>\nWhat is 2 + 2?</s>\n<|assistant|>\n",
        input: &[
            1, 529, 29989, 1792, 29989, 29958, 13, 5618, 338, 29871, 29906, 718, 29871, 29906,
            29973, 2, 29871, 13, 29966, 29989, 465, 22137, 29989, 29958, 13,
        ],
        first_logprobs: &[
            (29906, -0.732481),
            (8241, -1.418028),
            (1576, -2.320162),
            (3782, -3.201039),
            (797, -3.489187),
        ],
        output: &[29906, 718, 29871, 29906, 353, 29871, 29946, 2],
    },
];

const GEMMA: &[Case] = &[
    Case {
        prompt: "The capital of France is",
        input: &[2, 818, 5279, 529, 7001, 563],
        first_logprobs: &[
            (9079, -0.002430),
            (5213, -6.069520),
            (1018, -10.488822),
            (84750, -10.851142),
            (711, -11.660806),
        ],
        output: &[9079, 236761, 106],
    },
    Case {
        prompt: "Once upon a time",
        input: &[2, 14946, 3324, 496, 990],
        first_logprobs: &[
            (236764, -0.000037),
            (528, -10.897420),
            (1390, -11.027472),
            (40287, -13.709164),
            (106, -14.877584),
        ],
        output: &[
            236764, 528, 496, 1902, 16715, 607, 28239, 66418, 236764, 11742, 496, 33070, 7489,
            48757, 1630, 236761, 48757, 1630, 691, 496, 1646, 529, 8176, 2214, 236764, 1116, 1418,
            7296, 496, 12847, 529, 174804,
        ],
    },
    Case {
        prompt: "<|turn>user\nWhat is 2 + 2? Answer briefly.<turn|>\n<|turn>model\n",
        first_logprobs: &[
            (236812, -0.000000238),
            (1018, -16.303305),
            (26391, -16.547108),
            (236795, -18.359188),
            (3292, -20.454248),
        ],
        input: &[
            2, 105, 2364, 107, 3689, 563, 236743, 236778, 900, 236743, 236778, 236881, 25685,
            21485, 236761, 106, 107, 105, 4368, 107,
        ],
        output: &[236812, 106],
    },
];

fn check<B: Backend>(backend: &B, gemma: bool) {
    check_inference(backend, gemma, Reference::Strict);
}

fn check_inference<B: Backend>(backend: &B, gemma: bool, reference: Reference) {
    check_execution(backend, gemma, reference, false);
}

pub(super) fn test_decoder<'a, B: Backend>(
    backend: &'a B,
    model: &'a Model<B>,
    kv: &'a mut KvCache<B>,
    ws: &'a mut Workspace<B>,
    executor: bool,
) -> Box<dyn decoder::Decoder + 'a> {
    if executor {
        backend.decoder(model, kv, ws).unwrap()
    } else {
        Box::new(decoder::EagerDecoder {
            backend,
            model,
            kv,
            ws,
        })
    }
}

fn check_execution<B: Backend>(backend: &B, gemma: bool, reference: Reference, executor: bool) {
    let variable = if gemma {
        "MDMA_TEST_GEMMA4_GGUF"
    } else {
        "MDMA_TEST_TINYLLAMA_GGUF"
    };
    let path = std::env::var(variable)
        .unwrap_or_else(|_| panic!("set {variable} to the pinned checkpoint"));
    let mut gguf = GgufModel::open(path).unwrap();
    assert_eq!(
        gguf.metadata_str("general.architecture"),
        Some(if gemma { "gemma4" } else { "llama" })
    );
    let tokenizer = Tokenizer::from_gguf(&gguf).unwrap();
    if gemma {
        for (text, ids) in [
            ("", vec![2]),
            (
                "  hello\tworld\n\n café 🦀 中文",
                vec![
                    2, 138, 23391, 255968, 12392, 108, 33443, 236743, 248714, 17346, 237364,
                ],
            ),
            ("<eos><turn|><|tool_response>", vec![2, 1, 106, 50]),
        ] {
            assert_eq!(tokenizer.encode(text), ids, "{text:?}");
        }
        for id in [1, 106, 50] {
            assert!(tokenizer.is_eos(id));
        }
    } else {
        for (text, ids) in [
            ("", vec![1]),
            (
                "  hello\tworld\n\n café 🦀 中文",
                vec![
                    1, 1678, 12199, 12, 11526, 13, 13, 5777, 29888, 29948, 29871, 243, 162, 169,
                    131, 29871, 30275, 30333,
                ],
            ),
            ("<s>Hi</s>there", vec![1, 1, 6324, 2, 727]),
            (
                "<|user|>\nWhat is 2 + 2?</s>\n<|assistant|>\n",
                vec![
                    1, 529, 29989, 1792, 29989, 29958, 13, 5618, 338, 29871, 29906, 718, 29871,
                    29906, 29973, 2, 29871, 13, 29966, 29989, 465, 22137, 29989, 29958, 13,
                ],
            ),
        ] {
            assert_eq!(tokenizer.encode(text), ids, "{text:?}");
        }
    }
    let model = Model::load(backend, &mut gguf).unwrap();
    drop(gguf);
    assert_eq!(model.config.n_layers, if gemma { 35 } else { 22 });
    assert_eq!(model.config.n_ctx, if gemma { 131072 } else { 2048 });
    for case in if gemma { GEMMA } else { TINY } {
        let input = tokenizer.encode(case.prompt);
        assert_eq!(input, case.input, "tokenization: {:?}", case.prompt);
        let capacity = input.len() + case.output.len();
        let mut kv = model.new_kv_cache(backend, capacity).unwrap();
        let mut prefill = model.alloc_workspace(backend, input.len()).unwrap();
        let mut logits = model
            .prefill(backend, &mut kv, &mut prefill, &input, 0)
            .unwrap();
        let max = f64::from(logits.iter().copied().fold(f32::NEG_INFINITY, f32::max));
        let log_z = max
            + logits
                .iter()
                .map(|&v| (f64::from(v) - max).exp())
                .sum::<f64>()
                .ln();
        let mut logprob_error = 0.0f64;
        for &(id, expected) in case.first_logprobs {
            let actual = f64::from(logits[id]) - log_z;
            logprob_error = logprob_error.max((actual - expected).abs());
            if reference == Reference::Strict {
                assert!(
                    (actual - expected).abs() < 0.01,
                    "{:?}: reference logprob token {id}: {actual} != {expected}",
                    case.prompt
                );
            }
        }
        let mut decode = model.alloc_workspace(backend, 1).unwrap();
        let mut sequential = model.new_kv_cache(backend, capacity).unwrap();
        let mut other = Vec::new();
        {
            let mut decoder = test_decoder(backend, &model, &mut sequential, &mut decode, executor);
            for (pos, &token) in input.iter().enumerate() {
                other = decoder.forward(token, pos as u32).unwrap();
            }
        }
        assert!(other.iter().all(|x| x.is_finite()));
        let drift = logits
            .iter()
            .zip(&other)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(
            drift < 0.005,
            "{:?}: prefill/decode maximum logit drift {drift}",
            case.prompt
        );
        drop(sequential);
        drop(prefill);
        let mut generated = Vec::new();
        let mut decoder = test_decoder(backend, &model, &mut kv, &mut decode, executor);
        for (i, &expected) in case.output.iter().enumerate() {
            assert!(logits.iter().all(|x| x.is_finite()));
            let token = greedy(&logits);
            generated.push(token);
            if reference != Reference::Diagnostic {
                assert_eq!(token, expected, "{:?}: generated position {i}", case.prompt);
            }
            if tokenizer.is_eos(token) {
                if reference != Reference::Diagnostic {
                    assert_eq!(i + 1, case.output.len(), "unexpected early EOS");
                }
                break;
            } else if i + 1 < case.output.len() {
                logits = decoder.forward(token, (input.len() + i) as u32).unwrap();
            }
        }
        if reference != Reference::Strict {
            eprintln!(
                "{:?}: prefill/decode drift {drift}; max reference logprob error {logprob_error}; generated {generated:?}: {:?}",
                case.prompt,
                tokenizer.decode(&generated)
            );
        }
    }
}

#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; heavyweight pinned checkpoint"]
fn tinyllama_cpu_checkpoint() {
    check(&CpuBackend, false);
}

#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; heavyweight pinned checkpoint"]
fn gemma4_cpu_checkpoint() {
    check(&CpuBackend, true);
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; heavyweight pinned checkpoint"]
fn tinyllama_metal_checkpoint() {
    check(&crate::backend::metal::MetalBackend::new().unwrap(), false);
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; heavyweight pinned checkpoint"]
fn gemma4_metal_checkpoint() {
    check(&crate::backend::metal::MetalBackend::new().unwrap(), true);
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; batched executor checkpoint"]
fn tinyllama_metal_executor_checkpoint() {
    check_execution(
        &crate::backend::metal::MetalBackend::new().unwrap(),
        false,
        Reference::Strict,
        true,
    );
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; batched executor checkpoint"]
fn gemma4_metal_executor_checkpoint() {
    check_execution(
        &crate::backend::metal::MetalBackend::new().unwrap(),
        true,
        Reference::Strict,
        true,
    );
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and an NVIDIA GPU; f32 activations, eager decoder"]
fn tinyllama_cuda_checkpoint() {
    check(
        &crate::backend::cuda::CudaBackend::new()
            .unwrap()
            .without_int8(),
        false,
    );
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and an NVIDIA GPU; f32 activations, graph-replay executor"]
fn tinyllama_cuda_executor_checkpoint() {
    check_execution(
        &crate::backend::cuda::CudaBackend::new()
            .unwrap()
            .without_int8(),
        false,
        Reference::Strict,
        true,
    );
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "known CUDA int8 prefill drift; requires MDMA_TEST_TINYLLAMA_GGUF and an NVIDIA GPU"]
fn tinyllama_cuda_int8_prefill_diagnostic() {
    check_execution(
        &crate::backend::cuda::CudaBackend::new().unwrap(),
        false,
        Reference::Diagnostic,
        true,
    );
}

// Gemma prefill is one token at a time, so the int8 GEMM never runs: the
// default backend is the f32-activation path. Graph capture declines its
// 512-wide heads, so the executor gate covers eager decode on the backend
// decoder.
#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an NVIDIA GPU; f16 KV"]
fn gemma4_cuda_checkpoint() {
    check(&crate::backend::cuda::CudaBackend::new().unwrap(), true);
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an NVIDIA GPU; backend decoder"]
fn gemma4_cuda_executor_checkpoint() {
    check_execution(
        &crate::backend::cuda::CudaBackend::new().unwrap(),
        true,
        Reference::Strict,
        true,
    );
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "known ROCm prefill/decode drift; requires MDMA_TEST_TINYLLAMA_GGUF and an AMD GPU"]
fn tinyllama_rocm_prefill_decode_diagnostic() {
    check_inference(
        &crate::backend::rocm::HipBackend::new().unwrap(),
        false,
        Reference::Diagnostic,
    );
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and an AMD GPU; f32 compute/KV"]
fn tinyllama_rocm_f32_checkpoint() {
    check(&crate::backend::rocm::HipBackend::new_f32().unwrap(), false);
}

#[cfg(feature = "vulkan")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and a Vulkan GPU"]
fn tinyllama_vulkan_checkpoint() {
    check(
        &crate::backend::vulkan::VulkanBackend::new()
            .unwrap()
            .with_f16_weights(false),
        false,
    );
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an AMD GPU; f32 compute/KV"]
fn gemma4_rocm_f32_checkpoint() {
    check(&crate::backend::rocm::HipBackend::new_f32().unwrap(), true);
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an AMD GPU; exact greedy tokens, not f32 logprob parity"]
fn gemma4_rocm_greedy_checkpoint() {
    check_inference(
        &crate::backend::rocm::HipBackend::new().unwrap(),
        true,
        Reference::Tokens,
    );
}

#[cfg(feature = "vulkan")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and a Vulkan GPU"]
fn gemma4_vulkan_checkpoint() {
    check(
        &crate::backend::vulkan::VulkanBackend::new()
            .unwrap()
            .with_f16_weights(false),
        true,
    );
}
