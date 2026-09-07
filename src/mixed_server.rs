use std::time::Duration;

use renet::RenetServer;

use crate::{TransportError, UdpNetcodeServerTransport, WebRtcNetcodeServerTransport};

#[derive(Debug)]
pub struct MixedServerTransport {
    udp: UdpNetcodeServerTransport,
    webrtc: WebRtcNetcodeServerTransport,
}

impl MixedServerTransport {
    pub fn new(udp: UdpNetcodeServerTransport, webrtc: WebRtcNetcodeServerTransport) -> Self {
        Self { udp, webrtc }
    }

    pub fn udp(&self) -> &UdpNetcodeServerTransport {
        &self.udp
    }

    pub fn udp_mut(&mut self) -> &mut UdpNetcodeServerTransport {
        &mut self.udp
    }

    pub fn webrtc(&self) -> &WebRtcNetcodeServerTransport {
        &self.webrtc
    }

    pub fn webrtc_mut(&mut self) -> &mut WebRtcNetcodeServerTransport {
        &mut self.webrtc
    }

    pub fn disconnect_all(&mut self, server: &mut RenetServer) {
        self.udp.disconnect_all(server);
        self.webrtc.disconnect_all(server);
    }

    pub fn update(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
    ) -> Result<(), TransportError> {
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

impl MixedServerTransport {
    /// Share policy and telemetry across both backends, with independent peer queues.
    pub fn set_conditioner(&mut self, handle: crate::server_conditioner::ServerConditionerHandle) {
        self.udp.set_conditioner(handle.clone());
        self.webrtc.set_conditioner(handle);
    }

    pub fn clear_conditioner(&mut self) {
        self.udp.clear_conditioner();
        self.webrtc.clear_conditioner();
    }
}
