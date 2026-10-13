use super::KvCache;
use crate::backend::cpu::CpuBackend;
use crate::backend::{Backend, Error, Pool, RopeSpec};
use std::assert_matches;
use std::ops::Range;

fn buf_from(backend: &CpuBackend, bytes: &[u8]) -> crate::backend::cpu::CpuBuf {
    let mut b = backend.alloc(bytes.len(), Pool::Scratch).expect("alloc");
    backend.upload(&mut b, 0, bytes).expect("upload");
    b
}

/// Layer 0 keeps a window of 3 rows, layer 1 all 8 positions.
fn ring(backend: &CpuBackend) -> KvCache<CpuBackend> {
    KvCache::with_geometry_reserved(backend, &[2, 2], &[3, 0], 8, None).expect("ring cache")
}

/// Decode-style writes: every layer gets one row per position, filled with
/// the position's value, then the prefix grows past it.
fn write_through(backend: &CpuBackend, cache: &mut KvCache<CpuBackend>, positions: Range<usize>) {
    for pos in positions {
        let row = buf_from(backend, &vec![pos as u8; cache.row_bytes()]);
        for layer in 0..cache.rows.len() {
            cache
                .write(backend, layer, pos, &row, &row, 1)
                .expect("write one row");
        }
        cache.set_len(pos + 1);
    }
}

#[test]
fn ring_layers_store_only_their_window_and_expose_it_as_a_prefix() {
    let backend = CpuBackend;
    let mut cache = ring(&backend);
    let row_bytes = cache.row_bytes();
    assert_eq!(cache.view(0).k.0.len(), 3 * row_bytes);
    assert_eq!(cache.view(1).v.0.len(), 8 * row_bytes);
    assert_eq!(
        KvCache::allocation_bytes(&backend, &[2, 2], &[3, 0], 8).unwrap(),
        (3 + 8) * row_bytes * 2
    );
    assert_matches!(
        KvCache::allocation_bytes(&backend, &[2, 2], &[3], 8),
        Err(Error::ShapeMismatch(_))
    );
    for pos in 0..5 {
        write_through(&backend, &mut cache, pos..pos + 1);
        assert_eq!(cache.view(0).len, (pos + 1).min(3), "ring view at {pos}");
        assert_eq!(cache.view(1).len, pos + 1, "linear view at {pos}");
    }
    let stored = |layer: usize| -> Vec<u8> {
        let view = cache.view(layer);
        view.k
            .0
            .chunks(row_bytes)
            .take(view.len)
            .map(|row| row[0])
            .collect()
    };
    assert_eq!(stored(0), [3, 4, 2]);
    assert_eq!(stored(1), [0, 1, 2, 3, 4]);
}

#[test]
fn rings_reject_batched_and_rope_fused_writes_and_writes_past_capacity() {
    let backend = CpuBackend;
    let mut cache = ring(&backend);
    let row = buf_from(&backend, &vec![0; cache.row_bytes()]);
    let rows = buf_from(&backend, &vec![0; 2 * cache.row_bytes()]);
    assert_matches!(
        cache.write(&backend, 0, 0, &rows, &rows, 2),
        Err(Error::ShapeMismatch(_))
    );
    cache
        .write(&backend, 1, 0, &rows, &rows, 2)
        .expect("full-context layers take batches");
    for (layer, pos, n_rows) in [(0, 8, 1), (1, 7, 2), (1, usize::MAX, 2)] {
        assert_matches!(
            cache.write(&backend, layer, pos, &rows, &rows, n_rows),
            Err(Error::ShapeMismatch(_)),
            "layer {layer}, {n_rows} rows at {pos}"
        );
    }
    let (mut q, mut k) = (buf_from(&backend, &[0; 16]), buf_from(&backend, &[0; 8]));
    let spec = RopeSpec {
        head_dim: 2,
        theta: 10000.0,
    };
    assert_matches!(
        cache.rope_write(&backend, 0, 0, &mut q, &mut k, &row, 1, &spec),
        Err(Error::ShapeMismatch(_))
    );
}

#[test]
fn rewinding_after_a_ring_wraps_only_returns_to_the_last_position_written() {
    let backend = CpuBackend;
    // (positions written, resume at, resumable) with a 3-row window.
    let cases = [
        (3, 1, true),  // not wrapped yet
        (4, 0, true),  // a new sequence
        (4, 1, false), // position 3 overwrote position 0
        (4, 2, false),
        (4, 3, true), // rewrites the last position
        (4, 4, true),
        (4, 5, false), // beyond the prefix
        (8, 6, false),
        (8, 7, true),
    ];
    for (written, pos, resumable) in cases {
        let mut cache = ring(&backend);
        write_through(&backend, &mut cache, 0..written);
        let kept = if resumable { pos } else { 0 };
        assert_eq!(
            cache.can_resume(pos),
            resumable,
            "{written} written, at {pos}"
        );
        assert_eq!(cache.rewind(pos), kept, "{written} written, at {pos}");
        assert_eq!(cache.len(), kept);
    }
    // Starting over forgets how far the old sequence wrapped.
    let mut cache = ring(&backend);
    write_through(&backend, &mut cache, 0..8);
    assert_eq!(cache.rewind(2), 0);
    write_through(&backend, &mut cache, 0..2);
    assert!(cache.can_resume(1));
    // Full-context layers resume anywhere in the prefix.
    let mut linear = KvCache::new(&backend, 2, 1, 8, 2).expect("linear cache");
    write_through(&backend, &mut linear, 0..8);
    assert!((0..=8).all(|pos| linear.can_resume(pos)));
    assert!(!linear.can_resume(9));
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

    cache
        .write(&backend, 0, 3, &src0, &src0, 1)
        .expect("write layer0 pos3");
    cache
        .write(&backend, 1, 3, &src1, &src1, 1)
        .expect("write layer1 pos3");
    cache.set_len(4);

    let mut got0 = vec![0u8; row_bytes];
    backend
        .download(cache.view(0).k, 3 * row_bytes, &mut got0)
        .expect("read back layer0");
    assert_eq!(got0, row0);

    let mut got1 = vec![0u8; row_bytes];
    backend
        .download(cache.view(1).k, 3 * row_bytes, &mut got1)
        .expect("read back layer1");
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

    cache
        .write(&backend, 0, 2, &src, &src, 3)
        .expect("write 3 rows at pos 2");
    cache.set_len(5);

    let mut got = vec![0u8; 3 * row_bytes];
    backend
        .download(cache.view(0).k, 2 * row_bytes, &mut got)
        .expect("read back");
    assert_eq!(got, rows);
}
