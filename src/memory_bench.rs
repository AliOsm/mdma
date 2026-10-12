//! Bounded copy benchmarks. Payload GB/s includes completion, not allocation.
use crate::backend::{Backend, Error, Pool, Result};
use serde::Serialize;
use std::{hint::black_box, time::Instant};

pub const MAX_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_ITERATIONS: usize = 32;

pub fn validate(bytes: usize, iterations: usize) -> Result<()> {
    if bytes == 0 || bytes > MAX_BYTES || iterations == 0 || iterations > MAX_ITERATIONS {
        return Err(Error::ShapeMismatch(
            "memory bench requires 1..=256 MiB bytes and 1..=32 iterations".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Measurement {
    pub operation: &'static str,
    pub bytes_per_copy: usize,
    pub iterations: usize,
    pub seconds: f64,
    pub payload_gb_per_second: f64,
}

fn measured(
    operation: &'static str,
    bytes: usize,
    iterations: usize,
    work: impl FnOnce() -> Result<()>,
) -> Result<Measurement> {
    let start = Instant::now();
    work()?;
    let seconds = start.elapsed().as_secs_f64();
    if seconds == 0. {
        return Err(Error::Backend(
            "timer resolution too low for benchmark".into(),
        ));
    }
    Ok(Measurement {
        operation,
        bytes_per_copy: bytes,
        iterations,
        seconds,
        payload_gb_per_second: bytes as f64 * iterations as f64 / seconds / 1e9,
    })
}

fn source(bytes: usize) -> Vec<u8> {
    (0..bytes).map(|i| (i % 251) as u8).collect()
}

fn verify(actual: &[u8], expected: &[u8]) -> Result<()> {
    if actual != expected {
        return Err(Error::Backend(
            "memory benchmark copy verification failed".into(),
        ));
    }
    Ok(())
}

pub fn host(bytes: usize, iterations: usize) -> Result<Measurement> {
    validate(bytes, iterations)?;
    let src = source(bytes);
    let mut dst = vec![0; bytes];
    dst.copy_from_slice(&src);
    let result = measured("ram_copy", bytes, iterations, || {
        for _ in 0..iterations {
            black_box(&mut dst).copy_from_slice(black_box(&src));
        }
        Ok(())
    })?;
    verify(&dst, &src)?;
    Ok(result)
}

/// Backend methods may enqueue work: a blocking download completes each timed
/// section. Full verification and allocation are outside the timing window.
pub fn device<B: Backend>(b: &B, bytes: usize, iterations: usize) -> Result<Vec<Measurement>> {
    validate(bytes, iterations)?;
    if !bytes.is_multiple_of(4) {
        return Err(Error::ShapeMismatch(
            "device benchmark requires four-byte aligned copy sizes".into(),
        ));
    }
    let src = source(bytes);
    let mut dst = vec![0; bytes];
    let mut a = b.alloc(bytes, Pool::Scratch)?;
    let mut c = b.alloc(bytes, Pool::Scratch)?;
    let fence_bytes = bytes.min(4);
    let fence = |buf: &B::Buf| b.download(buf, 0, &mut [0u8; 4][..fence_bytes]);
    b.upload(&mut a, 0, &src)?;
    fence(&a)?;
    let upload = measured("host_to_device", bytes, iterations, || {
        for _ in 0..iterations {
            b.upload(&mut a, 0, &src)?;
        }
        fence(&a)
    })?;
    b.download(&a, 0, &mut dst)?;
    verify(&dst, &src)?;
    b.copy(&a, 0, &mut c, 0, bytes)?;
    fence(&c)?;
    let copy = measured("device_copy", bytes, iterations, || {
        for _ in 0..iterations {
            b.copy(&a, 0, &mut c, 0, bytes)?;
        }
        fence(&c)
    })?;
    b.download(&c, 0, &mut dst)?;
    verify(&dst, &src)?;
    let download = measured("device_to_host", bytes, iterations, || {
        for _ in 0..iterations {
            b.download(&c, 0, &mut dst)?;
        }
        Ok(())
    })?;
    verify(&dst, &src)?;
    Ok(vec![upload, copy, download])
}

#[cfg(test)]
mod tests;
