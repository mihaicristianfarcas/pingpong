#![no_main]
use libfuzzer_sys::fuzz_target;
use pingpong_proto::input;

// The input parser reads hostile network input: anything that reaches the
// tunnel can be handed to it (v2 design §10.1). It must never panic. Unlike the
// control parser this one loops over a caller-supplied count, so a bounds slip
// is a real possibility rather than a theoretical one.
fuzz_target!(|data: &[u8]| {
    let _ = input::decode(data, 0);
});
