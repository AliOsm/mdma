use super::*;
use crate::gguf::tests::Fixture;
use crate::backend::cpu::CpuBackend;
use std::assert_matches;

#[test]
fn local_generation_and_every_hybrid_partition_match() {
    let file = Fixture::new(3, 8).with_tokenizer().write();
    let mut g = GgufModel::open(file.path()).unwrap();
    let reference = generate(&CpuBackend, &mut g, "hello", 4, None).unwrap();
    assert_eq!(reference.tokens.len(), 4);
    for count in 0..=3 {
        let got = generate(&CpuBackend, &mut g, "hello", 4, Some(count)).unwrap();
        assert_eq!(got.prompt_tokens, reference.prompt_tokens);
        assert_eq!(got.tokens, reference.tokens);
        assert_eq!(got.text, reference.text);
        assert_eq!(got.gpu_layers, Some(count));
    }
}

#[test]
fn rejects_context_and_tokenizer_before_execution() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut g = GgufModel::open(file.path()).unwrap();
    for (prompt, max) in [("", 4), ("hello", 0), ("hello", 64), ("hello", usize::MAX)] {
        assert!(generate(&crate::backend::test_support::NoUpload, &mut g, prompt, max, None).is_err());
    }
    let file = Fixture::new(1, 8).write();
    let mut g = GgufModel::open(file.path()).unwrap();
    assert_matches!(generate(&CpuBackend, &mut g, "hello", 4, None), Err(Error::UnsupportedModel(_)));
}
