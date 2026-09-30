#![no_main]
use libfuzzer_sys::fuzz_target;
use pingpong_proto::control::Control;

// The control parser reads hostile network input: anything that reaches the
// tunnel can be handed to it (v2 design §10.1). It must never panic.
//
// v2 gave control bodies fields and variable length, so this is no longer the
// single-opcode match it was in v1 -- there are now length checks and enum
// range checks that can get the bounds wrong.
fuzz_target!(|data: &[u8]| {
    let _ = Control::decode(data);
});
