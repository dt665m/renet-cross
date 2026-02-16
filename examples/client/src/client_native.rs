use std::{
    net::UdpSocket,
    time::{Duration, SystemTime},
};

use renet::{ConnectionConfig, RenetClient};
use renetcode::{ClientAuthentication, NetcodeClient, NETCODE_MAX_PACKET_BYTES};

use renet_server::SessionCreateResponse;

pub struct NativeUdpTransport {
    socket: UdpSocket,
    netcode_client: NetcodeClient,
    buffer: [u8; NETCODE_MAX_PACKET_BYTES],
}

impl NativeUdpTransport {
    pub fn new(current_time: Duration, authentication: ClientAuthentication, socket: UdpSocket) -> Result<Self, String> {
        socket.set_nonblocking(true).map_err(|err| err.to_string())?;
        let netcode_client = NetcodeClient::new(current_time, authentication).map_err(|err| err.to_string())?;

        Ok(Self {
            socket,
            netcode_client,
            buffer: [0; NETCODE_MAX_PACKET_BYTES],
        })
    }

    pub fn update(&mut self, duration: Duration, client: &mut RenetClient) -> Result<(), String> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            client.disconnect_due_to_transport();
            return Err(format!("netcode disconnected: {reason}"));
        }

        if client.disconnect_reason().is_some() && !self.netcode_client.is_disconnected() {
            if let Ok((addr, packet)) = self.netcode_client.disconnect() {
                let _ = self.socket.send_to(packet, addr);
            }
        }

        if self.netcode_client.is_connected() {
            client.set_connected();
        } else if self.netcode_client.is_connecting() {
            client.set_connecting();
        }

        loop {
            match self.socket.recv_from(&mut self.buffer) {
                Ok((len, _source)) => {
                    let packet = &mut self.buffer[..len];
                    if let Some(payload) = self.netcode_client.process_packet(packet) {
                        client.process_packet(payload);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => break,
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(err) => return Err(err.to_string()),
            }
        }

        if let Some((packet, addr)) = self.netcode_client.update(duration) {
            self.socket.send_to(packet, addr).map_err(|err| err.to_string())?;
        }

        Ok(())
    }

    pub fn send_packets(&mut self, client: &mut RenetClient) -> Result<(), String> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            return Err(format!("netcode disconnected: {reason}"));
        }

        let packets = client.get_packets_to_send();
        for packet in packets {
            let (addr, payload) = self
                .netcode_client
                .generate_payload_packet(&packet)
                .map_err(|err| err.to_string())?;
            self.socket
                .send_to(payload, addr)
                .map_err(|err| err.to_string())?;
        }

        Ok(())
    }
}

pub fn connect(base_http: &str, protocol_id: u64) -> Result<(RenetClient, NativeUdpTransport, u64), String> {
    let session_url = format!("{}/api/session/new", base_http.trim_end_matches('/'));
    log::debug!("requesting native session: {session_url}");

    let response = reqwest::blocking::Client::new()
        .post(session_url)
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?;

    let session = response
        .json::<SessionCreateResponse>()
        .map_err(|err| err.to_string())?;
    log::info!(
        "native session response: client_id={} udp_addr={} webrtc_addr={} webrtc_offer_url={}",
        session.client_id,
        session.udp_addr,
        session.webrtc_addr,
        session.webrtc_offer_url
    );

    let server_addr = session
        .udp_addr
        .parse()
        .map_err(|err: std::net::AddrParseError| err.to_string())?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|err| err.to_string())?;

    let authentication = ClientAuthentication::Unsecure {
        protocol_id,
        client_id: session.client_id,
        server_addr,
        user_data: None,
    };

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|err| err.to_string())?;
    let transport = NativeUdpTransport::new(now, authentication, socket)?;
    let renet = RenetClient::new(ConnectionConfig::default());

    Ok((renet, transport, session.client_id))
}
