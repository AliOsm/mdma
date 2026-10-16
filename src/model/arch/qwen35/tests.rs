use super::*;
use crate::backend::cpu::CpuBackend;
use crate::{model::test_support::assert_greedy, tokenizer::Tokenizer};

/// Pinned qwen35 Q4_K_M fixture. Reference: llama.cpp d812350493
/// `llama-server`, CPU, ctx 512, context checkpoints off, raw prompts, greedy,
/// no penalties. With checkpoints on, the server splits the prompt and flips a
/// near tie in the first case at output 8 (`,` for `.`).
const CASES: &[(&str, &[u32])] = &[
    (
        "The capital of France is",
        &[
            11751, 13, 1061, 369, 264, 1575, 20654, 2029, 13, 271, 248068, 198, 90700, 8340, 25,
            271, 16, 13, 220, 2972, 2014, 53983, 279, 5952, 64700, 561, 1156, 369, 26895, 264,
            2029, 3376,
        ],
    ),
    (
        "<|im_start|>user\nWhat is 2+2? Answer briefly.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
        &[19, 248046],
    ),
    (
        "中文：法国的首都是",
        &[
            109705, 1710, 198, 98936, 4960, 760, 6511, 314, 9338, 369, 11751, 13, 198, 99986, 4960,
            109705, 95761, 95892, 99676, 110161, 1710, 198, 98936, 4960, 57590, 369, 279, 6511,
            314, 9338, 13, 198,
        ],
    ),
];

fn greedy_checkpoint<B: Backend>(b: &B) {
    let mut g =
        GgufModel::open(std::env::var("MDMA_TEST_QWEN35_GGUF").expect("set MDMA_TEST_QWEN35_GGUF"))
            .unwrap();
    assert_eq!(
        crate::cache::source_fingerprint(&mut g.file).unwrap().0,
        "df13d66021cef676f82be74053220fd75af6bf2a6a7fb77f5222ab9e50744a7a"
    );
    let tok = Tokenizer::from_gguf(&g).unwrap();
    let model = Model::load(b, &mut g).unwrap();
    drop(g);
    for &(prompt, expected) in CASES {
        assert_greedy(b, &model, &tok, prompt, expected);
    }
}

#[test]
#[ignore = "requires MDMA_TEST_QWEN35_GGUF; heavyweight pinned checkpoint"]
fn qwen35_cpu_greedy_checkpoint() {
    greedy_checkpoint(&CpuBackend);
}
