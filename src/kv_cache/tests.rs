use super::KvCache;
use crate::backend::cpu::CpuBackend;
use crate::backend::{Backend, Pool};

fn buf_from(backend: &CpuBackend, bytes: &[u8]) -> crate::backend::cpu::CpuBuf {
    let mut b = backend.alloc(bytes.len(), Pool::Scratch).expect("alloc");
    backend.upload(&mut b, 0, bytes).expect("upload");
    b
}

#[test]
fn positional_writes_land_at_correct_offsets_and_dont_cross_layers() {
    let backend = CpuBackend;
    let n_kv_heads = 2;
    let head_dim = 4;
    let mut cache = KvCache::new(&backend, 2, n_kv_heads, 8, head_dim).expect("new cache");

    let row_bytes = cache.row_bytes();
    let row0: Vec<u8> = (0..row_bytes as u8).collect();
    let row1: Vec<u8> = (100..100 + row_bytes as u8).collect();
    let src0 = buf_from(&backend, &row0);
    let src1 = buf_from(&backend, &row1);

    cache.write(&backend, 0, 3, &src0, &src0, 1).expect("write layer0 pos3");
    cache.write(&backend, 1, 3, &src1, &src1, 1).expect("write layer1 pos3");
    cache.set_len(4);

    let mut got0 = vec![0u8; row_bytes];
    backend.download(cache.view(0).k, 3 * row_bytes, &mut got0).expect("read back layer0");
    assert_eq!(got0, row0);

    let mut got1 = vec![0u8; row_bytes];
    backend.download(cache.view(1).k, 3 * row_bytes, &mut got1).expect("read back layer1");
    assert_eq!(got1, row1);

    assert_eq!(cache.view(0).len, 4);
}

#[test]
fn multi_row_write_lands_consecutive_rows_in_one_call() {
    let backend = CpuBackend;
    let n_kv_heads = 1;
    let head_dim = 2;
    let mut cache = KvCache::new(&backend, 1, n_kv_heads, 8, head_dim).expect("new cache");
    let row_bytes = cache.row_bytes();

    // 3 distinct rows, asymmetric content so a wrong offset/order is caught.
    let rows: Vec<u8> = (0..3 * row_bytes as u8).collect();
    let src = buf_from(&backend, &rows);

    cache.write(&backend, 0, 2, &src, &src, 3).expect("write 3 rows at pos 2");
    cache.set_len(5);

    let mut got = vec![0u8; 3 * row_bytes];
    backend.download(cache.view(0).k, 2 * row_bytes, &mut got).expect("read back");
    assert_eq!(got, rows);
}
