use super::Direction;
use std::marker::PhantomData;

#[derive(Debug, Clone)]
pub(crate) struct PacketGate<T>(PhantomData<T>);

impl<T> Default for PacketGate<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> PacketGate<T> {
    #[inline]
    pub fn defer(&self, _: Direction, _: T, _: &[u8]) -> bool {
        false
    }
    #[inline]
    pub fn drain(&self, _: Direction) -> Vec<(T, Vec<u8>)> {
        Vec::new()
    }
    #[inline]
    pub fn reset(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bypass_has_no_storage_or_pending_packets() {
        assert_eq!(std::mem::size_of::<PacketGate<usize>>(), 0);
        let gate = PacketGate::<usize>::default();
        assert!(!gate.defer(Direction::Incoming, 42, b"packet"));
        assert!(!gate.defer(Direction::Outgoing, 42, b"packet"));
        assert!(gate.drain(Direction::Incoming).is_empty());
        gate.reset();
    }
}
