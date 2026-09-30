//! How long the real Mainline DHT takes: join, publish, resolve from a second
//! client. Publishes one sealed record under a throwaway key.
//!
//!   cargo run -p pingpong-nat --example dht
use std::time::{Duration, Instant};

use pingpong_nat::rendezvous::{public_key, random_32, Kind, Record, Rendezvous, SALT};

fn wait_ready(r: &Rendezvous, what: &str, t: Instant) {
    while !r.is_ready() {
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{what} joined in {:?}", t.elapsed());
}

fn main() {
    let (seed, secret) = (random_32(), random_32());
    let t = Instant::now();
    let a = Rendezvous::join().unwrap();
    let b = Rendezvous::join().unwrap();
    wait_ready(&a, "publisher", t);
    wait_ready(&b, "resolver", t);

    let record = Record::new(Kind::Host, vec!["203.0.113.7:47800".parse().unwrap()]);
    let t = Instant::now();
    a.publish(&seed, &secret, SALT, &record).unwrap();
    println!("published in {:?}", t.elapsed());

    for i in 0..3 {
        let t = Instant::now();
        let got = b.resolve(&public_key(&seed), &secret, SALT);
        println!(
            "resolve #{i}: {:?} in {:?}",
            got.as_ref().map(|r| &r.endpoints),
            t.elapsed()
        );
    }
}
