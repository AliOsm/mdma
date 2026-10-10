use super::*;
use crate::gguf::GgmlDType;

#[test]
fn byte_ranges_handle_zero_lengths_and_overflow_at_capacity() {
    for capacity in [0, 1, 8, usize::MAX] {
        assert!(range("test", 0, capacity, capacity).is_ok());
        assert!(range("test", capacity, 0, capacity).is_ok());
        assert!(range("test", capacity, 1, capacity).is_err());
    }
    assert!(range("test", 1, usize::MAX, usize::MAX).is_err());
    assert!(range("test", usize::MAX, usize::MAX, usize::MAX).is_err());
    assert!(range("test", 9, 0, 8).is_err());
}

#[test]
fn matmul_infers_tokens_and_rejects_partial_rows() {
    let m = matmul(&[3, 4], 8, 6).unwrap();
    assert_eq!((m.n_out, m.n_in, m.n_tokens), (3, 4, 2));
    assert!(matmul(&[3, 4], 7, 6).is_err());
    assert!(matmul(&[3, 4], 8, 5).is_err());
    assert_eq!(matmul(&[3, 4], 8, 7).unwrap().n_tokens, 2, "oversized output is fine");
    assert!(matmul(&[3, 4], 0, 0).is_err());
    assert!(matmul(&[3], 8, 6).is_err());
    assert!(matmul(&[0, 4], 8, 0).is_err());
}

#[test]
fn kquant_alignment_only_applies_to_k_quants() {
    assert!(kquant_aligned("q", GgmlDType::Q4K, 512).is_ok());
    assert!(kquant_aligned("q", GgmlDType::Q5K, 512).is_ok());
    assert!(kquant_aligned("q", GgmlDType::Q5K, 300).is_err());
    assert!(kquant_aligned("q", GgmlDType::Q6K, 300).is_err());
    assert!(kquant_aligned("q", GgmlDType::F16, 300).is_ok());
}

#[test]
fn embed_checks_ids_and_output() {
    let e = embed(&[10, 4], &[1, 9], 8).unwrap();
    assert_eq!((e.vocab, e.n_embd), (10, 4));
    assert!(embed(&[10, 4], &[10], 4).is_err());
    assert!(embed(&[10, 4], &[], 0).is_err());
    assert!(embed(&[10, 4], &[1], 3).is_err());
    assert!(embed(&[10, 4], &[1], 5).is_ok());
}

#[test]
fn rows_same_and_range() {
    assert_eq!(rows("n", 4, 12, 12).unwrap(), 3);
    assert!(rows("n", 4, 10, 10).is_err());
    assert!(rows("n", 4, 12, 8).is_err());
    assert!(rows("n", 4, 12, 13).is_ok());
    assert!(rows("n", 0, 0, 0).is_err());
    assert!(rows("n", 4, 0, 0).is_err(), "empty input is not a dispatch");
    assert_eq!(elementwise("e", &[5, 5], &[5, 6]).unwrap(), 5);
    assert!(elementwise("e", &[5, 4], &[5]).is_err());
    assert!(elementwise("e", &[5, 5], &[4]).is_err());
    assert!(range("u", 2, 2, 4).is_ok());
    assert!(range("u", 3, 2, 4).is_err());
    assert!(range("u", usize::MAX, 1, 4).is_err());
    assert_eq!(elems("e", 8).unwrap(), 2);
    assert!(elems("e", 6).is_err());
}

#[test]
fn rope_and_attn_geometry() {
    let spec = RopeSpec { head_dim: 4, theta: 1.0 };
    let r = rope(&spec, 16, 8, 2).unwrap();
    assert_eq!((r.n_q_heads, r.n_kv_heads), (2, 1));
    assert!(rope(&spec, 16, 8, 3).is_err());
    assert!(rope(&RopeSpec { head_dim: 3, theta: 1.0 }, 6, 6, 2).is_err());

    let spec = AttnSpec { head_dim: 2, n_q_heads: 2, n_kv_heads: 1 };
    let a = attn(&spec, 8, 5, 10, 10, 8).unwrap();
    assert_eq!((a.n_q, a.prefix), (2, 3));
    assert!(attn(&spec, 8, 1, 10, 10, 8).is_err(), "n_q exceeds kv.len");
    assert!(attn(&spec, 8, 5, 9, 10, 8).is_err(), "short cache");
    assert!(attn(&spec, 8, 5, 10, 10, 4).is_err(), "o too small");
    assert!(attn(&spec, 8, 5, 10, 10, 9).is_ok(), "o oversized");
    assert!(attn(&AttnSpec { head_dim: 2, n_q_heads: 3, n_kv_heads: 2 }, 12, 5, 20, 20, 12).is_err());
}

#[test]
fn encoder_geometry() {
    let spec = EncoderSpec { seq_len: 3, n_heads: 2, head_dim: 2, window: None };
    assert_eq!(encoder_batch("e", &spec, 24).unwrap(), 2);
    assert!(encoder_batch("e", &spec, 20).is_err());
    assert_eq!(rope_neox(&spec, 24, 24).unwrap(), 2);
    assert!(rope_neox(&EncoderSpec { head_dim: 3, ..spec }, 18, 18).is_err());
    assert_eq!(attn_bidir(&spec, &[3, 1], 24, 24, 24, 24).unwrap(), 2);
    assert!(attn_bidir(&spec, &[3], 24, 24, 24, 24).is_err());
    assert!(attn_bidir(&spec, &[4, 1], 24, 24, 24, 24).is_err());
    assert!(attn_bidir(&spec, &[3, 1], 24, 12, 24, 24).is_err());
    assert!(attn_bidir(&spec, &[3, 1], 24, 24, 24, 20).is_err());
    assert_eq!(layernorm(4, Some(4), 8, 8).unwrap(), 2);
    assert!(layernorm(4, Some(3), 8, 8).is_err());
}
