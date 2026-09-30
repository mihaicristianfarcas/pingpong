#![no_main]
use libfuzzer_sys::fuzz_target;
use pingpong_proto::reassemble::Reassembler;

// The depacketizer parses hostile network input -- the one component an
// attacker can reach with arbitrary bytes (v1 design §13.1).
fuzz_target!(|data: &[u8]| {
    let mut r = Reassembler::new(4);
    for chunk in data.chunks(1200) {
        let _ = r.push(chunk);
    }
});
