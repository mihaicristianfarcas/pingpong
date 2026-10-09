//! Ping: the pingpong streaming client.
//!
//! Everything a stream needs lives here -- the tunnel, session negotiation,
//! loss recovery, decoding, presentation, input capture -- so the app
//! (`ping-app`) is only a launcher over it, and the `pingctl` CLI drives exactly
//! the same code. The platform's window, decoder and presenter are in `mac`
//! and `win`, behind [`session::Session`]. A stream comes from a Pong host
//! or from an Xbox ([`xbox`]), through the same platform layer.

pub mod aspect;
pub mod input;
pub mod keyboard;
pub mod keymap;
pub mod logging;
pub mod pad;
pub mod pair;
pub mod pointer;
pub mod priority;
pub mod session;
pub mod stats;
pub mod store;
pub mod stream;
pub mod wake;
pub mod wan;
pub mod xbox;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(windows)]
pub mod win;
