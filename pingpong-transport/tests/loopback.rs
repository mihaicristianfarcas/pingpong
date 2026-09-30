//! Two endpoints over localhost: the handshake completes through pq-boringtun's
//! segmented static-KEM path, and inner packets survive byte for byte.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_transport::{Endpoint, Identity, Received, WireBatch, MAX_INNER};

fn pump(
    ep: Arc<Endpoint>,
    stop: Arc<AtomicBool>,
    got: Arc<parking_lot::Mutex<Vec<Vec<u8>>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let mut last_tick = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if let Ok(Received::Data(_, n)) = ep.recv(&mut buf) {
                got.lock().push(buf[..n].to_vec());
            }
            if last_tick.elapsed() > Duration::from_millis(100) {
                ep.tick();
                last_tick = Instant::now();
            }
        }
    })
}

/// A pingpong header is an IPv4 header as far as boringtun's receive-side
/// validation is concerned: version nibble 4, length >= 20, total_len exact.
fn inner(len: usize, fill: u8) -> Vec<u8> {
    let mut p = vec![fill; len];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p
}

#[test]
fn packets_cross_a_real_tunnel_byte_exact() {
    let host = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let client = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let host_addr: SocketAddr = format!("127.0.0.1:{}", host.local_port()).parse().unwrap();

    host.add_peer(client.identity().public().clone(), None)
        .unwrap();
    let to_host = client
        .add_peer(host.identity().public().clone(), Some(host_addr))
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let at_host = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let at_client = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let h = pump(host.clone(), stop.clone(), at_host.clone());
    let c = pump(client.clone(), stop.clone(), at_client.clone());

    client.initiate(&to_host).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !to_host.is_established() {
        assert!(Instant::now() < deadline, "handshake did not complete");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        to_host.segmented_sends() > 0,
        "the static-KEM initiation must go out segmented"
    );

    let packets: Vec<Vec<u8>> = (0..50)
        .map(|i| inner(20 + (i * 97) % (MAX_INNER - 20), i as u8))
        .collect();
    let mut scratch = WireBatch::new();
    client
        .send_batch(&to_host, packets.iter().map(|p| p.as_slice()), &mut scratch)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while at_host.lock().len() < packets.len() {
        assert!(
            Instant::now() < deadline,
            "only {} of {} arrived",
            at_host.lock().len(),
            packets.len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*at_host.lock(), packets);

    // And back: the host learned the client's address from the handshake.
    let to_client = host
        .peer_by_key(&client.identity().public().x25519)
        .unwrap();
    host.send(&to_client, &inner(64, 7)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while at_client.lock().is_empty() {
        assert!(Instant::now() < deadline, "reply never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(at_client.lock()[0], inner(64, 7));

    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    c.join().unwrap();
}

#[test]
fn a_stranger_cannot_complete_a_handshake() {
    let host = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let stranger = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let host_addr: SocketAddr = format!("127.0.0.1:{}", host.local_port()).parse().unwrap();
    // The host knows some OTHER client, not the stranger.
    host.add_peer(Identity::generate().public().clone(), None)
        .unwrap();
    let to_host = stranger
        .add_peer(host.identity().public().clone(), Some(host_addr))
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let h = pump(host.clone(), stop.clone(), sink.clone());
    let s = pump(stranger.clone(), stop.clone(), sink.clone());
    stranger.initiate(&to_host).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(!to_host.is_established());
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    s.join().unwrap();
}

#[test]
fn a_raced_initiation_settles_on_the_path_that_answers() {
    let host = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let client = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
    let v4: SocketAddr = format!("127.0.0.1:{}", host.local_port()).parse().unwrap();
    let v6: SocketAddr = format!("[::1]:{}", host.local_port()).parse().unwrap();
    // Nothing listens here: a stale LAN address, say.
    let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();

    host.add_peer(client.identity().public().clone(), None)
        .unwrap();
    let to_host = client
        .add_peer(host.identity().public().clone(), Some(dead))
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let at_host = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let at_client = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let h = pump(host.clone(), stop.clone(), at_host.clone());
    let c = pump(client.clone(), stop.clone(), at_client.clone());

    let init = client.initiation(&to_host).unwrap();
    for to in [dead, v4, v6] {
        let _ = client.send_initiation(&to_host, &init, to);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !to_host.is_established() {
        assert!(Instant::now() < deadline, "handshake did not complete");
        std::thread::sleep(Duration::from_millis(10));
    }
    let settled = to_host.addr().unwrap();
    assert!(
        settled.port() == host.local_port(),
        "settled on {settled}, not the dead address"
    );

    // And the session works along it.
    client.send(&to_host, &inner(100, 7)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while at_host.lock().is_empty() {
        assert!(Instant::now() < deadline, "data did not arrive");
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    c.join().unwrap();
}

/// A frame's datagrams as the host sends them: runs of full-size ones with a
/// shorter one inside, over IPv4 and IPv6. They go out batched (several per
/// system call), in order, byte for byte, and batching stays on.
#[test]
fn a_batch_goes_out_in_runs_byte_exact() {
    for host_addr in ["127.0.0.1", "[::1]"] {
        let host = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
        let client = Arc::new(Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap());
        let host_addr: SocketAddr = format!("{host_addr}:{}", host.local_port())
            .parse()
            .unwrap();
        host.add_peer(client.identity().public().clone(), None)
            .unwrap();
        let to_host = client
            .add_peer(host.identity().public().clone(), Some(host_addr))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let at_host = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let h = pump(host.clone(), stop.clone(), at_host.clone());
        let c = pump(
            client.clone(),
            stop.clone(),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
        );
        client.initiate(&to_host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !to_host.is_established() {
            assert!(Instant::now() < deadline, "handshake did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }

        // 70 data (the last short), 14 parity, then a lone small one.
        let mut packets: Vec<Vec<u8>> = (0..69).map(|i| inner(MAX_INNER, i as u8)).collect();
        packets.push(inner(333, 69));
        packets.extend((70..84).map(|i| inner(MAX_INNER, i as u8)));
        packets.push(inner(20, 84));
        let mut scratch = WireBatch::new();
        let sent_before = to_host.tx_datagrams();
        for group in packets.chunks(64) {
            client
                .send_batch(&to_host, group.iter().map(|p| p.as_slice()), &mut scratch)
                .unwrap();
        }
        assert_eq!(to_host.tx_datagrams() - sent_before, packets.len() as u64);

        let deadline = Instant::now() + Duration::from_secs(5);
        while at_host.lock().len() < packets.len() {
            assert!(
                Instant::now() < deadline,
                "only {} of {} arrived",
                at_host.lock().len(),
                packets.len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*at_host.lock(), packets);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(client.batch_segments() > 1, "batching was turned off");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        c.join().unwrap();
    }
}

/// Nothing arriving: `recv` still returns, within its timeout, so the
/// caller's timers run (batched receives must not block for good).
#[test]
fn an_idle_recv_times_out() {
    let ep = Endpoint::bind(Arc::new(Identity::generate()), 0).unwrap();
    ep.set_recv_timeout(Duration::from_millis(30)).unwrap();
    let mut buf = vec![0u8; 65536];
    let t = Instant::now();
    for _ in 0..3 {
        assert!(matches!(ep.recv(&mut buf), Ok(Received::Timeout)));
    }
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "took {:?}",
        t.elapsed()
    );
}
