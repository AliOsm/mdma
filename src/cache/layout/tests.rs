use super::*;
use crate::backend::{Backend, QWeight, cpu::{CpuBackend, CpuBuf}};

#[test]
fn prepared_blocks_preserve_every_dequantized_value() {
    // Exercise every header byte value, not only well-behaved quantizer output.
    for seed in 0..256u32 {
        let mut raw: Vec<u8> = (0..144 * 3u32).map(|i| ((i * 73 + seed * 31) & 255) as u8).collect();
        for block in raw.as_chunks_mut::<144>().0 {
            block[..2].copy_from_slice(&half::f16::from_f32(0.125).to_le_bytes());
            block[2..4].copy_from_slice(&half::f16::from_f32(0.0625).to_le_bytes());
        }
        let packed = pack_q4k(&raw).unwrap();
        assert_eq!(packed.len(), raw.len());
        let weight = QWeight { dtype: GgmlDType::Q4K, shape: vec![3, 256], buf: CpuBuf(raw.clone()) };
        let mut output = CpuBuf(vec![0; 3 * 256 * 4]);
        CpuBackend.embed(&weight, &[0, 1, 2], &mut output).unwrap();
        let reference: &[f32] = bytemuck::cast_slice(&output.0);
        for (b, block) in packed.as_chunks::<Q4K_BLOCK_BYTES>().0.iter().enumerate() {
            assert_eq!(&block[..4], &raw[b * 144..b * 144 + 4]);
            assert_eq!(&block[Q4K_QUANTS_OFFSET..], &raw[b * 144 + 16..(b + 1) * 144]);
            for i in 0..256 {
                let j = i / 32;
                let shift = (j / 2) * 8;
                let parity = j % 2;
                let sc = u32::from_le_bytes(block[4..8].try_into().unwrap());
                let mn = u32::from_le_bytes(block[8..12].try_into().unwrap());
                let hi = u32::from_le_bytes(block[12..16].try_into().unwrap());
                let scale = ((sc >> (shift + parity * 4)) & 15) | (((hi >> (shift + parity * 2)) & 3) << 4);
                let min = ((mn >> (shift + parity * 4)) & 15) | (((hi >> (shift + 4 + parity * 2)) & 3) << 4);
                let q = (block[Q4K_QUANTS_OFFSET + (i / 64) * 32 + i % 32] >> ((j % 2) * 4)) & 15;
                let value = 0.125 * scale as f32 * f32::from(q) - 0.0625 * min as f32;
                assert_eq!(value, reference[b * 256 + i], "seed={seed} block={b} elem={i}");
            }
        }
    }
}

#[test]
fn partial_blocks_and_invalid_layouts_are_rejected() {
    assert!(pack_q4k(&[0; 143]).is_err());
    assert!(pack_q4k(&[]).unwrap().is_empty());
    let desc = TensorDesc { dtype: GgmlDType::F32, shape: vec![256], offset: 0, n_bytes: 1024 };
    assert!(WeightLayout::RocmQ4kV3.bytes(&desc).is_err());
    assert!(serde_json::from_str::<WeightLayout>("\"rocm_q4k_v1\"").is_err());
    assert!(serde_json::from_str::<WeightLayout>("\"rocm_q4k_v2\"").is_err());
}
