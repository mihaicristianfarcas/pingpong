//! Reaching a pingpong host from anywhere, without forwarding a port by hand:
//! the Tailscale recipe, minus the servers.
//!
//! - `stun`: learn the public address the NAT gives the tunnel's socket.
//! - `portmap`: ask the router to forward the tunnel's port (UPnP, NAT-PMP).
//! - `rendezvous`: signed, sealed records on the Mainline DHT, through which
//!   paired devices find each other.
//!
//! See docs/networking.md.
pub mod keys;
pub mod portmap;
pub mod rendezvous;
pub mod stun;
