use std::time::Duration;

use renet::RenetServer;

use crate::{Str0mNetcodeServerTransport, TransportError, UdpNetcodeServerTransport};

#[derive(Debug)]
pub struct MixedServerTransport {
    udp: UdpNetcodeServerTransport,
    webrtc: Str0mNetcodeServerTransport,
}

impl MixedServerTransport {
    pub fn new(udp: UdpNetcodeServerTransport, webrtc: Str0mNetcodeServerTransport) -> Self {
        Self { udp, webrtc }
    }

    pub fn udp(&self) -> &UdpNetcodeServerTransport {
        &self.udp
    }

    pub fn udp_mut(&mut self) -> &mut UdpNetcodeServerTransport {
        &mut self.udp
    }

    pub fn webrtc(&self) -> &Str0mNetcodeServerTransport {
        &self.webrtc
    }

    pub fn webrtc_mut(&mut self) -> &mut Str0mNetcodeServerTransport {
        &mut self.webrtc
    }

    pub fn disconnect_all(&mut self, server: &mut RenetServer) {
        self.udp.disconnect_all(server);
        self.webrtc.disconnect_all(server);
    }

    pub fn update(&mut self, duration: Duration, server: &mut RenetServer) -> Result<(), TransportError> {
        self.udp.update(duration, server)?;
        self.webrtc.update(duration, server)?;
        Ok(())
    }

    pub fn send_packets(&mut self, server: &mut RenetServer) {
        // Important: renet queues are per-client, so each backend sends only packets
        // for clients managed by its own netcode server.
        self.udp.send_packets(server);
        self.webrtc.send_packets(server);
    }
}
