//! Raw-packet interception, selected once at compile time.
//!
//! Transport pumps use the same interface with or without impairment support.
//! The disabled implementation is allocation-free and occupies no storage.

#[derive(Clone, Copy)]
pub(crate) enum Direction {
    Incoming,
    Outgoing,
}

#[cfg(feature = "packet-conditioner")]
mod enabled;
#[cfg(feature = "packet-conditioner")]
pub(crate) use enabled::PacketGate;
#[cfg(not(feature = "packet-conditioner"))]
mod disabled;
#[cfg(not(feature = "packet-conditioner"))]
pub(crate) use disabled::PacketGate;
