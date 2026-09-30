//! The tunnel: pq-boringtun (post-quantum WireGuard with static ML-KEM
//! authentication) carrying every pingpong packet over one UDP socket.

pub mod endpoint;
pub mod identity;

/// The raw tunnel, for benchmarks: the same pq-boringtun state an
/// [`Endpoint`] keeps per peer, without the socket.
#[doc(hidden)]
pub mod bench {
    pub use boringtun::noise::{Tunn, TunnResult};

    use crate::{Identity, PublicIdentity, TransportError};

    pub fn tunn(own: &Identity, peer: &PublicIdentity, index: u32) -> Result<Tunn, TransportError> {
        crate::endpoint::new_tunn(own, peer, index)
    }
}

pub use endpoint::{
    Endpoint, Peer, PeerId, Received, TransportError, WireBatch, MAX_INNER, PATH_MTU,
};
pub use identity::{Identity, PublicIdentity};
