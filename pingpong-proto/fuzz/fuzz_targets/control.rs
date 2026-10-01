#![no_main]
use libfuzzer_sys::fuzz_target;
use pingpong_proto::control::Control;
use pingpong_proto::screen::{self, ScreenText};

// The control parser reads hostile network input: anything that reaches the
// tunnel can be handed to it (v2 design §10.1). It must never panic.
//
// v2 gave control bodies fields and variable length, so this is no longer the
// single-opcode match it was in v1 -- there are now length checks and enum
// range checks that can get the bounds wrong.
//
// Screen text rides the same packets (agent sessions) and carries strings
// and counts from the host: its parts and the reply they assemble into are
// parsed here too.
fuzz_target!(|data: &[u8]| {
    let _ = Control::decode(data);
    let _ = screen::decode(data);
    let _ = ScreenText::decode(data);
});
