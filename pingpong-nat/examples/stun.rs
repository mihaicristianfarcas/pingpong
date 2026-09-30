//! What does this machine's NAT do? Asks two STUN servers for the public
//! address of one UDP socket.
//!
//!   cargo run -p pingpong-nat --example stun
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

use pingpong_nat::stun;

fn main() {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(800)))
        .unwrap();
    println!("local {}", socket.local_addr().unwrap());
    let mut mapped = Vec::new();
    for server in stun::SERVERS {
        let Some(addr) = server
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.find(|a| a.is_ipv4()))
        else {
            continue;
        };
        let id = stun::transaction_id();
        socket.send_to(&stun::binding_request(&id), addr).unwrap();
        let mut buf = [0u8; 512];
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => match stun::parse_response(&buf[..n]) {
                Some((rid, public)) if rid == id => {
                    println!("{server:>28} ({addr}) sees us as {public}");
                    mapped.push(public);
                }
                _ => println!("{server}: unexpected reply"),
            },
            Err(e) => println!("{server}: {e}"),
        }
    }
    if mapped.len() >= 2 {
        let same = mapped.windows(2).all(|w| w[0] == w[1]);
        println!(
            "mapping: {}",
            if same {
                "endpoint-independent (hole punching works)"
            } else {
                "per destination (symmetric: needs a relay or a port mapping)"
            }
        );
    }
}
