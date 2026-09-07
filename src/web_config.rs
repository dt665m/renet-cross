use renet::ConnectionConfig;

/// One browser ICE server. TURN credentials should be short-lived credentials
/// obtained by the application, rather than secrets embedded in a client build.
#[derive(Clone, Default)]
pub struct WebRtcIceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Browser bootstrap and buffering policy. Empty `ice_servers` allows host-only
/// ICE. The defaults preserve the existing public Google STUN configuration.
pub struct WebRtcConnectOptions {
    pub override_webrtc_addr: Option<String>,
    pub connection_config: ConnectionConfig,
    pub transport: crate::ClientTransportConfig,
    pub ice_servers: Vec<WebRtcIceServer>,
    /// Maximum bytes queued in the browser's outgoing DataChannel buffer.
    /// Packets exceeding this budget are dropped; Renet owns retransmission.
    pub max_buffered_amount: u32,
    /// Maximum received packets retained until the next update. Oldest packets
    /// are dropped on overflow to avoid retaining a stale backlog after a pause.
    /// Each packet is also bounded by renetcode's maximum packet size.
    pub max_inbox_packets: usize,
}

impl Default for WebRtcConnectOptions {
    fn default() -> Self {
        Self {
            override_webrtc_addr: None,
            connection_config: ConnectionConfig::default(),
            transport: Default::default(),
            ice_servers: vec![WebRtcIceServer {
                urls: vec![
                    "stun:stun.l.google.com:19302".into(),
                    "stun:stun1.l.google.com:19302".into(),
                ],
                ..Default::default()
            }],
            max_buffered_amount: 64 * 1024,
            max_inbox_packets: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid WebRTC option: {0}")]
pub struct WebRtcOptionsError(pub &'static str);

impl WebRtcConnectOptions {
    /// Validate local policy before making any HTTP requests.
    pub fn validate(&self) -> Result<(), WebRtcOptionsError> {
        if self.max_buffered_amount < renetcode::NETCODE_MAX_PACKET_BYTES as u32 {
            return Err(WebRtcOptionsError(
                "max_buffered_amount must fit a netcode packet",
            ));
        }
        if self.max_inbox_packets == 0 {
            return Err(WebRtcOptionsError("max_inbox_packets must be nonzero"));
        }
        if self.ice_servers.iter().any(|server| {
            server.urls.is_empty() || server.urls.iter().any(|url| url.trim().is_empty())
        }) {
            return Err(WebRtcOptionsError("ICE servers must contain nonempty URLs"));
        }
        Ok(())
    }
}

/// Packet drops caused by local browser transport limits, not network loss.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebRtcClientStats {
    pub send_backpressure_drops: u64,
    pub receive_overflow_drops: u64,
    pub receive_invalid_size_drops: u64,
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn fits_send_budget(buffered: u32, packet_len: usize, limit: u32) -> bool {
    packet_len <= limit.saturating_sub(buffered) as usize && buffered <= limit
}

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) struct PacketInbox {
    packets: std::collections::VecDeque<Vec<u8>>,
    capacity: usize,
    pub stats: WebRtcClientStats,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PacketInbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            packets: Default::default(),
            capacity,
            stats: Default::default(),
        }
    }

    // Check before copying browser-owned data into Rust memory.
    pub fn accepts_size(&mut self, len: usize) -> bool {
        if len == 0 || len > renetcode::NETCODE_MAX_PACKET_BYTES {
            self.stats.receive_invalid_size_drops += 1;
            false
        } else {
            true
        }
    }

    pub fn push(&mut self, packet: Vec<u8>) {
        if self.capacity == 0 {
            self.stats.receive_overflow_drops += 1;
            return;
        }
        if self.packets.len() == self.capacity {
            self.packets.pop_front();
            self.stats.receive_overflow_drops += 1;
        }
        self.packets.push_back(packet);
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.packets.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_send_includes_new_packet_and_handles_overflow() {
        assert!(fits_send_budget(100, 1400, 1500));
        assert!(!fits_send_budget(101, 1400, 1500));
        assert!(!fits_send_budget(u32::MAX, 1400, u32::MAX));
        assert!(!fits_send_budget(0, usize::MAX, u32::MAX));
        assert!(!fits_send_budget(1501, 0, 1500));
    }

    #[test]
    fn paused_receiver_keeps_newest_packets_in_order_and_counts_drops() {
        let mut inbox = PacketInbox::new(2);
        for sequence in 0..100 {
            inbox.push(vec![sequence]);
        }
        assert_eq!(inbox.stats.receive_overflow_drops, 98);
        assert_eq!(inbox.pop(), Some(vec![98]));
        assert_eq!(inbox.pop(), Some(vec![99]));
        assert_eq!(inbox.pop(), None);
        inbox.push(vec![100]);
        assert_eq!(inbox.pop(), Some(vec![100]));
        assert_eq!(inbox.stats.receive_overflow_drops, 98);
    }

    #[test]
    fn oversized_browser_messages_are_rejected_before_copy() {
        let mut inbox = PacketInbox::new(2);
        assert!(!inbox.accepts_size(0));
        assert!(inbox.accepts_size(renetcode::NETCODE_MAX_PACKET_BYTES));
        assert!(!inbox.accepts_size(renetcode::NETCODE_MAX_PACKET_BYTES + 1));
        assert!(!inbox.accepts_size(usize::MAX));
        assert_eq!(inbox.stats.receive_invalid_size_drops, 3);
    }

    #[test]
    fn validates_buffer_limits_and_allows_host_only_ice() {
        let mut options = WebRtcConnectOptions::default();
        assert!(options.validate().is_ok());
        options.ice_servers.clear();
        assert!(options.validate().is_ok());
        options.max_inbox_packets = 0;
        assert!(options.validate().is_err());
        options.max_inbox_packets = 1;
        options.max_buffered_amount = renetcode::NETCODE_MAX_PACKET_BYTES as u32 - 1;
        assert!(options.validate().is_err());
        options.max_buffered_amount += 1;
        assert!(options.validate().is_ok());
        options.ice_servers.push(WebRtcIceServer::default());
        assert!(options.validate().is_err());
    }
}
