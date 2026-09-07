//! Runtime packet interception shared by native and browser transports.
//! No UI dependency or feature selection; absent controls pass packets through.

#[derive(Clone, Copy)]
pub(crate) enum Direction {
    Incoming,
    Outgoing,
}

mod client;
pub(crate) use client::PacketGate;

/// Transport-specific identity used to isolate server packet conditioning queues.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServerPeerId {
    Udp(std::net::SocketAddr),
    WebRtc(u64),
}

#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use server::ServerPacketGate;
