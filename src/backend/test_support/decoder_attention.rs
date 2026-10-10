//! Window boundaries, grouped heads, chunked softmax and oversized outputs.
use super::*;
use crate::backend::{AttnSpec, KvView};

fn upload<B: Backend>(b: &B, x: &[f32]) -> B::Buf {
    let mut buf = b.alloc(x.len() * 4, Pool::Scratch).unwrap();
    b.upload(&mut buf, 0, &bytes(x)).unwrap();
    buf
}

fn check<B: Backend>(b: &B) {
    for (dim, len) in [(6, 19), (256, 1031), (512, 1031)] {
        let spec = AttnSpec { head_dim: dim, n_q_heads: 4, n_kv_heads: 2 };
        let q: Vec<_> = (0..3 * 4 * dim).map(|i| (i % 17) as f32 / 32.0 - 0.25).collect();
        // Exactly representable in f16 too: same oracle for both KV formats.
        let k: Vec<_> = (0..len * 2 * dim).map(|i| (i % 23) as f32 / 32.0 - 0.375).collect();
        let v: Vec<_> = (0..len * 2 * dim).map(|i| (i % 29) as f32 / 16.0 - 0.875).collect();
        let dq = upload(b, &q);
        let mut dk = b.alloc(k.len() * b.kv_elem_bytes(), Pool::Kv).unwrap();
        let mut dv = b.alloc(v.len() * b.kv_elem_bytes(), Pool::Kv).unwrap();
        b.kv_write(&upload(b, &k), &upload(b, &v), &mut dk, &mut dv, 0, k.len()).unwrap();
        let ck = CpuBuf(bytes(&k));
        let cv = CpuBuf(bytes(&v));
        for window in [0, 1, 7, 512] {
            let mut out = upload(b, &vec![12345.0; q.len() + 7]);
            b.attn_windowed(&dq, &KvView { k: &dk, v: &dv, len }, &spec, 0.25, window, &mut out).unwrap();
            let mut expected = CpuBackend.alloc(q.len() * 4, Pool::Scratch).unwrap();
            CpuBackend.attn_windowed(&CpuBuf(bytes(&q)), &KvView { k: &ck, v: &cv, len },
                &spec, 0.25, window, &mut expected).unwrap();
            let mut got = vec![0u8; (q.len() + 7) * 4];
            b.download(&out, 0, &mut got).unwrap();
            let got = f32s(&got);
            close(&got[..q.len()], &f32s(&expected.0), 2e-4);
            assert_eq!(&got[q.len()..], &[12345.0; 7]);
        }
    }
}

#[cfg(test)]
mod tests;
