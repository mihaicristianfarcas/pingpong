//! Streaming from an Xbox: a console at home ("xHome", the Xbox app's
//! remote play) and Xbox Cloud Gaming ("xCloud", Game Pass's servers).
//! Both are the same protocol: a Microsoft account signs in, a web API
//! starts a session, and the picture, sound and input travel over WebRTC.
//!
//! Greenlight (github.com/unknownskl/greenlight, MIT) is the reference
//! client: what the services expect, field by field, was learnt from it and
//! from its libraries (`xal-node`, `xbox-webapi`, `xbox-xcloud-player`), and
//! comments name the file a behaviour comes from. The code is pingpong's
//! own. Where it differs from Greenlight it says why.
//!
//! - [`auth`]: signing in with a Microsoft account (a code to type on any
//!   device), and the tokens each service takes.
//! - [`consoles`]: the account's consoles, turning them on and off.
//! - [`gssv`]: the streaming service: sessions, the cloud catalogue.
//! - [`connection`]: the WebRTC connection (str0m) and the stream over it.
//! - [`stream`]: a stream from start to end, over all of the above.
//! - Pure protocol, tested everywhere: [`input`] (the input channel's
//!   reports), [`messages`] (the message and control channels),
//!   [`keymap`] (keys as Windows key codes, and as a controller),
//!   [`rumble`] (the console's rumble patterns as motor states), [`ice`]
//!   (candidates, Teredo).
//!
//! Nothing here touches a window, a decoder or a speaker: the client
//! (`ping_core::xbox`) hands frames to the platform's own decoder and
//! presenter, as it does a Pong host's.

pub mod auth;
pub mod connection;
pub mod consoles;
pub mod gssv;
pub mod http;
pub mod ice;
pub mod input;
pub mod keymap;
pub mod messages;
pub mod rumble;
pub mod store;
pub mod stream;
pub mod time;

/// A random (version 4) UUID, as the services take for ids.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    // The OS's generator does not fail where Ping runs; if it ever did,
    // an all-zero id still works (ids only tell messages apart).
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn uuids_are_version_four() {
        let a = super::uuid_v4();
        let b = super::uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
