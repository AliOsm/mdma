use super::dequantize_row;
use candle_core::Device;
use candle_core::quantized::GgmlDType;
use candle_core::quantized::gguf_file::Content;
use std::fs::File;

use crate::backend::test_support::fixture;

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn matches_candles_own_dequant_for_a_q4k_tensor() {
    let mut file = File::open(fixture()).expect("open");
    let content = Content::read(&mut file).expect("read content");
    let (name, info) = content
        .tensor_infos
        .iter()
        .find(|(_, info)| info.ggml_dtype == GgmlDType::Q4K)
        .expect("fixture has a Q4_K tensor");
    let name = name.clone();

    let qtensor = content
        .tensor(&mut file, &name, &Device::Cpu)
        .expect("candle load");
    let expected = qtensor.dequantize(&Device::Cpu).expect("candle dequant");
    let expected: Vec<f32> = expected.flatten_all().unwrap().to_vec1().unwrap();

    let mut model = crate::gguf::GgufModel::open(fixture()).expect("open gguf");
    let raw = model.read_tensor_bytes(&name).expect("read raw bytes");
    let mut ours = vec![0f32; expected.len()];
    dequantize_row(info.ggml_dtype, &raw, &mut ours).expect("dequantize_row");

    assert_eq!(ours.len(), expected.len());
    for (a, b) in ours.iter().zip(expected.iter()) {
        assert!((a - b).abs() < 1e-3, "mismatch: {a} vs {b}");
    }
}

#[test]
fn dequantize_row_rejects_misaligned_rows_and_decodes_aligned_ones() {
    let values = [1.5f32, -2.0, 0.25];
    let mut storage = vec![0u8; values.len() * 4 + 4];
    let offset = (0..4)
        .find(|&o| storage[o..].as_ptr().align_offset(align_of::<f32>()) == 0)
        .expect("an aligned offset");
    let misaligned = if offset == 0 { 1 } else { offset - 1 };
    for (i, v) in values.iter().enumerate() {
        storage[offset + i * 4..offset + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut out = [0f32; 3];
    dequantize_row(GgmlDType::F32, &storage[offset..offset + 12], &mut out).expect("aligned row");
    assert_eq!(out, values);
    let err = dequantize_row(
        GgmlDType::F32,
        &storage[misaligned..misaligned + 12],
        &mut out,
    );
    assert!(
        err.is_err(),
        "misaligned f32 row must be rejected, not reinterpreted"
    );
}
