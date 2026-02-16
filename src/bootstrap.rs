use std::sync::atomic::{AtomicU64, Ordering};

use renet::ClientId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct MonotonicClientIdAllocator {
    next_id: AtomicU64,
}

impl MonotonicClientIdAllocator {
    pub fn new(start_at: ClientId) -> Self {
        Self {
            next_id: AtomicU64::new(start_at),
        }
    }

    pub fn next(&self) -> ClientId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateResponse {
    pub client_id: ClientId,
    pub udp_addr: String,
    pub webrtc_addr: String,
    pub webrtc_offer_url: String,
}

#[cfg(test)]
mod tests {
    use super::MonotonicClientIdAllocator;

    #[test]
    fn monotonic_allocator_is_monotonic() {
        let allocator = MonotonicClientIdAllocator::new(10);
        assert_eq!(allocator.next(), 10);
        assert_eq!(allocator.next(), 11);
        assert_eq!(allocator.next(), 12);
    }

    #[test]
    fn monotonic_allocator_unique_in_sequence() {
        let allocator = MonotonicClientIdAllocator::new(1);
        let mut values = vec![];
        for _ in 0..256 {
            values.push(allocator.next());
        }

        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), 256);
    }
}
