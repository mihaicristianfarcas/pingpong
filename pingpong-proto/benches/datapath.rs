//! The per-frame cost of pingpong's own data path, host and client side.
//!
//!   cargo bench -p pingpong-proto
//!
//! Frame sizes bracket what the host sends: a static-desktop P-frame, a busy
//! 1080p60 P-frame at 20 Mbit/s, and a 4K IDR.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pingpong_proto::audio::{AudioDepacketizer, AudioPacketizer};
use pingpong_proto::header::Header;
use pingpong_proto::packetize::{Datagrams, Packetizer};
use pingpong_proto::reassemble::Reassembler;

const FRAMES: [(&str, usize); 3] = [("2KB P", 2_000), ("42KB P", 42_000), ("600KB IDR", 600_000)];

fn frame(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

fn packetize(c: &mut Criterion) {
    let mut g = c.benchmark_group("packetize");
    for (name, len) in FRAMES {
        let data = frame(len);
        let mut p = Packetizer::new();
        let mut out = Datagrams::new();
        g.throughput(Throughput::Bytes(len as u64));
        g.bench_with_input(BenchmarkId::from_parameter(name), &data, |b, data| {
            let mut id = 0u32;
            b.iter(|| {
                id = id.wrapping_add(1);
                p.packetize_into(data, id, 0, false, false, &mut out)
                    .unwrap();
                out.len()
            })
        });
    }
    g.finish();
}

fn reassemble(c: &mut Criterion) {
    let mut g = c.benchmark_group("reassemble");
    for (name, len) in FRAMES {
        let data = frame(len);
        let datagrams = Packetizer::new()
            .packetize(&data, 1, 0, false, false)
            .unwrap();
        g.throughput(Throughput::Bytes(len as u64));
        // As the client runs it: one reassembler, frames' buffers given back.
        g.bench_with_input(BenchmarkId::new("lossless", name), &datagrams, |b, d| {
            let mut r = Reassembler::new(8);
            let mut id = 0u32;
            let mut d = d.clone();
            b.iter(|| {
                id = id.wrapping_add(1);
                for g in d.iter_mut() {
                    g[16..20].copy_from_slice(&id.to_le_bytes());
                }
                let f = d.iter().find_map(|g| r.push(g)).expect("frame completes");
                r.recycle(f.data);
            })
        });
        // Drop the first datagram of every FEC block: every block recovers.
        let lossy: Vec<&Vec<u8>> = datagrams
            .iter()
            .filter(|g| Header::decode(g).unwrap().fragment_idx != 0)
            .collect();
        let mut lossy: Vec<Vec<u8>> = lossy.into_iter().cloned().collect();
        g.bench_function(BenchmarkId::new("fec recovery", name), |b| {
            let mut r = Reassembler::new(8);
            let mut id = 0u32;
            b.iter(|| {
                id = id.wrapping_add(1);
                for g in lossy.iter_mut() {
                    g[16..20].copy_from_slice(&id.to_le_bytes());
                }
                let f = lossy
                    .iter()
                    .find_map(|g| r.push(g))
                    .expect("frame recovers");
                r.recycle(f.data);
            })
        });
    }
    g.finish();
}

fn audio(c: &mut Criterion) {
    let mut g = c.benchmark_group("audio");
    let opus = vec![0x5Au8; 60];
    g.bench_function("packetize 5 ms", |b| {
        let mut p = AudioPacketizer::new();
        b.iter(|| p.push(&opus, 0))
    });
    let mut p = AudioPacketizer::new();
    let block: Vec<Vec<u8>> = (0..4).flat_map(|_| p.push(&opus, 0)).collect();
    g.bench_function("depacketize block, one lost", |b| {
        b.iter(|| {
            let mut d = AudioDepacketizer::new();
            let mut out = Vec::new();
            for g in block.iter().skip(1) {
                d.push(g, &mut out);
            }
            out
        })
    });
    g.finish();
}

criterion_group!(benches, packetize, reassemble, audio);
criterion_main!(benches);
