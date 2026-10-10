//! BF16 and Q5_K byte-layout checks, independent of external checkpoints.
use super::*;

fn check<B: Backend>(backend: &B) {
    for dtype in [GgmlDType::BF16, GgmlDType::Q5K] {
        assert!(backend.supports_weight_dtype(dtype));
        let (rows, width) = (5, if dtype == GgmlDType::BF16 { 259 } else { 512 });
        let raw: Vec<u8> = if dtype == GgmlDType::BF16 {
            (0..rows * width).flat_map(|i|
                half::bf16::from_f32(((i * 17) as f32).sin()).to_le_bytes()).collect()
        } else {
            (0..rows * width / 256).flat_map(|block| {
                let mut raw: Vec<u8> = (0..176).map(|i| ((i * 37 + block * 19) % 256) as u8).collect();
                raw[..2].copy_from_slice(&half::f16::from_f32(0.002 * (block + 1) as f32).to_le_bytes());
                raw[2..4].copy_from_slice(&half::f16::from_f32(0.001).to_le_bytes());
                raw
            }).collect()
        };
        let shape = vec![rows, width];
        let mut buf = backend.alloc(raw.len(), Pool::Weights).unwrap();
        backend.upload(&mut buf, 0, &raw).unwrap();
        let weight = QWeight { dtype, shape: shape.clone(), buf };
        let cpu_weight = QWeight { dtype, shape: shape.clone(), buf: CpuBuf(raw.clone()) };
        for tokens in [1, 3] {
            let x: Vec<_> = (0..tokens * width).map(|i| ((i * 13) as f32).cos() * 0.25).collect();
            let mut input = backend.alloc(x.len() * 4, Pool::Scratch).unwrap();
            backend.upload(&mut input, 0, &bytes(&x)).unwrap();
            let n = rows * tokens;
            let mut y = backend.alloc((n + 7) * 4, Pool::Scratch).unwrap();
            backend.upload(&mut y, 0, &bytes(&vec![12345.0f32; n + 7])).unwrap();
            backend.qmatmul(&weight, &input, &mut y).unwrap();
            let mut got = vec![0u8; (n + 7) * 4];
            backend.download(&y, 0, &mut got).unwrap();
            let got = f32s(&got);
            close(&got[..n], &cpu_qmatmul(dtype, &raw, &shape, &x), 2e-3);
            assert_eq!(&got[n..], &[12345.0; 7]);
        }
        let ids = [4, 0, 3];
        let n = ids.len() * width;
        let mut y = backend.alloc((n + 7) * 4, Pool::Scratch).unwrap();
        backend.upload(&mut y, 0, &bytes(&vec![12345.0f32; n + 7])).unwrap();
        backend.embed(&weight, &ids, &mut y).unwrap();
        let mut expected = CpuBackend.alloc(n * 4, Pool::Scratch).unwrap();
        CpuBackend.embed(&cpu_weight, &ids, &mut expected).unwrap();
        let mut got = vec![0u8; (n + 7) * 4];
        backend.download(&y, 0, &mut got).unwrap();
        let got = f32s(&got);
        close(&got[..n], &f32s(&expected.0), 1e-5);
        assert_eq!(&got[n..], &[12345.0; 7]);
    }
}

#[cfg(test)]
mod tests;
