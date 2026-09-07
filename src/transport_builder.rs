use std::{
    io,
    net::{SocketAddr, UdpSocket},
};

use crate::{
    MixedServerTransport, ServerAuthentication, ServerConfig, TransportError,
    UdpNetcodeServerTransport, WebRtcNetcodeServerTransport, bootstrap::unix_now_duration,
};

pub struct MixedTransportBuilder {
    protocol_id: u64,
    udp_bind: SocketAddr,
    webrtc_bind: SocketAddr,
    public_udp_addr: Option<SocketAddr>,
    public_webrtc_addr: Option<SocketAddr>,
    max_clients: usize,
    authentication: ServerAuthentication,
    transport: crate::ServerTransportConfig,
}

impl MixedTransportBuilder {
    pub fn new(protocol_id: u64) -> Self {
        Self {
            protocol_id,
            udp_bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            webrtc_bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            public_udp_addr: None,
            public_webrtc_addr: None,
            max_clients: 512,
            authentication: ServerAuthentication::Unsecure,
            transport: Default::default(),
        }
    }

    pub fn udp_bind(mut self, addr: SocketAddr) -> Self {
        self.udp_bind = addr;
        self
    }

    pub fn webrtc_bind(mut self, addr: SocketAddr) -> Self {
        self.webrtc_bind = addr;
        self
    }

    pub fn public_udp_addr(mut self, addr: SocketAddr) -> Self {
        self.public_udp_addr = Some(addr);
        self
    }

    pub fn public_webrtc_addr(mut self, addr: SocketAddr) -> Self {
        self.public_webrtc_addr = Some(addr);
        self
    }

    pub fn max_clients(mut self, max: usize) -> Self {
        self.max_clients = max;
        self
    }

    pub fn authentication(mut self, auth: ServerAuthentication) -> Self {
        self.authentication = auth;
        self
    }

    /// Shared packet policy applied to UDP and WebRTC before accepting clients.
    pub fn transport_config(mut self, config: crate::ServerTransportConfig) -> Self {
        self.transport = config;
        self
    }

    pub fn build(self) -> Result<MixedServerTransport, TransportError> {
        let current_time = unix_now_duration()
            .map_err(|err| io::Error::other(format!("failed to read unix timestamp: {err}")))?;
        let auth_for_udp = duplicate_server_authentication(&self.authentication);
        let auth_for_webrtc = self.authentication;

        let udp_socket = UdpSocket::bind(self.udp_bind)?;
        let default_public_udp_addr = udp_socket.local_addr()?;
        let public_udp_addr = self.public_udp_addr.unwrap_or(default_public_udp_addr);

        let udp_transport = UdpNetcodeServerTransport::new_with_config(
            ServerConfig {
                current_time,
                max_clients: self.max_clients,
                protocol_id: self.protocol_id,
                public_addresses: vec![public_udp_addr],
                authentication: auth_for_udp,
            },
            udp_socket,
            self.transport.clone(),
        )?;

        let webrtc_socket = UdpSocket::bind(self.webrtc_bind)?;
        let default_public_webrtc_addr = webrtc_socket.local_addr()?;
        let public_webrtc_addr = self
            .public_webrtc_addr
            .unwrap_or(default_public_webrtc_addr);

        let webrtc_transport = WebRtcNetcodeServerTransport::new_with_config(
            ServerConfig {
                current_time,
                max_clients: self.max_clients,
                protocol_id: self.protocol_id,
                public_addresses: vec![public_webrtc_addr],
                authentication: auth_for_webrtc,
            },
            webrtc_socket,
            self.transport,
        )?;

        Ok(MixedServerTransport::new(udp_transport, webrtc_transport))
    }
}

fn duplicate_server_authentication(authentication: &ServerAuthentication) -> ServerAuthentication {
    match authentication {
        ServerAuthentication::Unsecure => ServerAuthentication::Unsecure,
        ServerAuthentication::Secure { private_key } => ServerAuthentication::Secure {
            private_key: *private_key,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::MixedTransportBuilder;

    #[test]
    fn builder_defaults_allocate_valid_public_addresses() {
        let transport = MixedTransportBuilder::new(7).build().expect("builder");
        let udp_addr = transport
            .udp()
            .addresses()
            .first()
            .copied()
            .expect("udp addr");
        let webrtc_addr = transport
            .webrtc()
            .addresses()
            .first()
            .copied()
            .expect("webrtc addr");

        assert_ne!(udp_addr.port(), 0);
        assert_ne!(webrtc_addr.port(), 0);
    }

    #[test]
    fn builder_public_addr_overrides_take_precedence() {
        let expected_udp: SocketAddr = "127.0.0.1:41000".parse().expect("udp");
        let expected_webrtc: SocketAddr = "127.0.0.1:41001".parse().expect("webrtc");

        let transport = MixedTransportBuilder::new(7)
            .udp_bind("127.0.0.1:0".parse().expect("udp bind"))
            .webrtc_bind("127.0.0.1:0".parse().expect("webrtc bind"))
            .public_udp_addr(expected_udp)
            .public_webrtc_addr(expected_webrtc)
            .build()
            .expect("builder");

        let udp_addr = transport
            .udp()
            .addresses()
            .first()
            .copied()
            .expect("udp addr");
        let webrtc_addr = transport
            .webrtc()
            .addresses()
            .first()
            .copied()
            .expect("webrtc addr");

        assert_eq!(udp_addr, expected_udp);
        assert_eq!(webrtc_addr, expected_webrtc);
    }
}
