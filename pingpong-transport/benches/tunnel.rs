//! pq-boringtun as pingpong uses it, in memory: the static-ML-KEM handshake
//! (segmented for a 1280-byte path) and the data plane.
//!
//!   cargo bench -p pingpong-transport

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pingpong_transport::bench::{tunn, Tunn, TunnResult};
use pingpong_transport::Identity;

fn datagrams(r: TunnResult<'_>) -> Vec<Vec<u8>> {
    match r {
        TunnResult::WriteToNetwork(p) => vec![p.to_vec()],
        TunnResult::WriteManyToNetwork(s) => s.iter().map(|p| p.to_vec()).collect(),
        _ => Vec::new(),
    }
}

/// Deliver `packets` to `to`, collecting what it sends back (including what
/// it had queued).
fn deliver(to: &mut Tunn, packets: &[Vec<u8>], scratch: &mut [u8]) -> Vec<Vec<u8>> {
    let mut back = Vec::new();
    for p in packets {
        back.extend(datagrams(to.decapsulate(None, p, scratch)));
        loop {
            let more = datagrams(to.decapsulate(None, &[], scratch));
            if more.is_empty() {
                break;
            }
            back.extend(more);
        }
    }
    back
}

/// A complete handshake: initiation (segmented), response, first data.
fn handshake(a_id: &Identity, b_id: &Identity) -> (Tunn, Tunn) {
    let mut a = tunn(a_id, b_id.public(), 1).unwrap();
    let mut b = tunn(b_id, a_id.public(), 2).unwrap();
    let mut scratch = vec![0u8; 65536];
    let init = datagrams(a.format_handshake_initiation(&mut scratch, false));
    let response = deliver(&mut b, &init, &mut scratch);
    let confirm = deliver(&mut a, &response, &mut scratch);
    deliver(&mut b, &confirm, &mut scratch);
    (a, b)
}

fn inner(len: usize) -> Vec<u8> {
    let mut p = vec![0xABu8; len];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p
}

fn bench_handshake(c: &mut Criterion) {
    let (a_id, b_id) = (Identity::generate(), Identity::generate());
    let mut scratch = vec![0u8; 65536];
    let (mut a, _) = handshake(&a_id, &b_id);
    let init = datagrams(a.format_handshake_initiation(&mut scratch, false));
    println!(
        "initiation: {} datagrams, {} bytes",
        init.len(),
        init.iter().map(Vec::len).sum::<usize>()
    );
    c.bench_function("handshake/complete (both sides)", |b| {
        b.iter(|| handshake(&a_id, &b_id))
    });
}

fn bench_data(c: &mut Criterion) {
    let (a_id, b_id) = (Identity::generate(), Identity::generate());
    let (mut a, mut b) = handshake(&a_id, &b_id);
    let mut g = c.benchmark_group("data");
    let mut scratch = vec![0u8; 65536];
    let mut scratch2 = vec![0u8; 65536];
    for len in [100usize, 1200] {
        let packet = inner(len);
        g.throughput(Throughput::Bytes(len as u64));
        g.bench_with_input(BenchmarkId::new("encrypt", len), &packet, |bn, p| {
            bn.iter(|| match a.encapsulate(p, &mut scratch) {
                TunnResult::WriteToNetwork(w) => w.len(),
                other => panic!("{other:?}"),
            })
        });
        g.bench_with_input(
            BenchmarkId::new("encrypt+decrypt", len),
            &packet,
            |bn, p| {
                bn.iter(|| {
                    let wire = match a.encapsulate(p, &mut scratch) {
                        TunnResult::WriteToNetwork(w) => w.to_vec(),
                        other => panic!("{other:?}"),
                    };
                    match b.decapsulate(None, &wire, &mut scratch2) {
                        TunnResult::WriteToTunnelV4(d, _) => d.len(),
                        other => panic!("{other:?}"),
                    }
                })
            },
        );
    }
    g.finish();
}

criterion_group!(benches, bench_handshake, bench_data);
criterion_main!(benches);
