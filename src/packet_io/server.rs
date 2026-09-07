use super::{Direction, ServerPeerId};
use crate::server_conditioner::ServerConditionerHandle;

#[derive(Debug, Default)]
pub(crate) struct ServerPacketGate {
    attached: Option<(ServerConditionerHandle, u64)>,
}
impl ServerPacketGate {
    pub fn attach(&mut self, handle: ServerConditionerHandle) {
        self.detach();
        let owner = handle.allocate_owner();
        self.attached = Some((handle, owner));
    }
    pub fn handle(&self) -> Option<ServerConditionerHandle> {
        self.attached.as_ref().map(|(handle, _)| handle.clone())
    }
    pub fn detach(&mut self) {
        self.reset();
        self.attached = None;
    }
    pub fn reset(&mut self) {
        if let Some((handle, owner)) = &self.attached {
            handle.remove_owner(*owner);
        }
    }
    pub fn remove(&mut self, peer: ServerPeerId) {
        if let Some((handle, owner)) = &self.attached {
            handle.remove_peer(*owner, peer);
        }
    }
    pub fn defer(&mut self, direction: Direction, peer: ServerPeerId, bytes: &[u8]) -> bool {
        self.attached.as_ref().is_some_and(|(handle, owner)| {
            handle.defer_at(*owner, direction.into(), peer, bytes, handle.elapsed())
        })
    }
    pub fn drain(&mut self, direction: Direction) -> Vec<(ServerPeerId, Vec<u8>)> {
        self.attached
            .as_ref()
            .map(|(handle, owner)| handle.drain_at(*owner, direction.into(), handle.elapsed()))
            .unwrap_or_default()
    }
}
impl Drop for ServerPacketGate {
    fn drop(&mut self) {
        self.reset();
    }
}
