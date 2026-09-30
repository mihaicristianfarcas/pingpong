//! The Annex-B split every frame goes through before decode.
//!
//!   cargo bench -p pingpong-decode --bench annexb

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pingpong_decode::annexb::nal_units;

/// A frame the way an encoder emits one: a few parameter-set NALs, then one
/// slice of `len` bytes of entropy-coded data (emulation prevention keeps
/// start codes out of it).
fn frame(len: usize) -> Vec<u8> {
    let mut out = vec![
        0, 0, 0, 1, 0x40, 0x01, 0x0C, 0, 0, 0, 1, 0x42, 0x01, 0x01, 0, 0, 0, 1, 0x44, 0x01, 0xC1,
    ];
    out.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01]);
    let mut seed = 0x9E37_79B9u32;
    let mut zeros = 0;
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let b = seed as u8;
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The byte-at-a-time loop the decoder used before (for comparison).
fn bytewise(data: &[u8]) -> usize {
    let mut starts = 0;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
}

fn split(c: &mut Criterion) {
    let mut g = c.benchmark_group("annexb");
    for (name, len) in [
        ("2KB P", 2_000),
        ("120KB P", 120_000),
        ("600KB IDR", 600_000),
    ] {
        let data = frame(len);
        g.throughput(Throughput::Bytes(len as u64));
        g.bench_with_input(BenchmarkId::new("simd", name), &data, |b, d| {
            b.iter(|| nal_units(d).count())
        });
        g.bench_with_input(BenchmarkId::new("bytewise", name), &data, |b, d| {
            b.iter(|| bytewise(d))
        });
    }
    g.finish();
}

criterion_group!(benches, split);
criterion_main!(benches);
