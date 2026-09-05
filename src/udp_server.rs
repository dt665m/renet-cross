use std::{
    io,
    net::{SocketAddr, UdpSocket},
    num::NonZeroUsize,
    time::Duration,
};

use renet::{ClientId, RenetServer};
use renetcode::{NETCODE_MAX_PACKET_BYTES, NETCODE_USER_DATA_BYTES, NetcodeServer, ServerConfig};

use crate::{
    TransportError,
    netcode_result::{OwnedServerResult, to_owned_server_result},
};

#[derive(Debug)]
pub struct UdpNetcodeServerTransport {
    socket: UdpSocket,
    netcode_server: NetcodeServer,
    // Enforce packet size after receiving the entire datagram; don't interpret a
    // truncated prefix as a valid packet or fail the pump on Windows oversize IO.
    buffer: [u8; 65_535],
    max_datagrams_per_update: NonZeroUsize,
}

impl UdpNetcodeServerTransport {
    pub fn new(server_config: ServerConfig, socket: UdpSocket) -> Result<Self, io::Error> {
        socket.set_nonblocking(true)?;
        let netcode_server = NetcodeServer::new(server_config);

        Ok(Self {
            socket,
            netcode_server,
            buffer: [0; 65_535],
            max_datagrams_per_update: NonZeroUsize::new(256).unwrap(),
        })
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.netcode_server.addresses()
    }

    /// Bound receive work per call, including malformed datagrams. Remaining
    /// traffic stays in the OS queue while keepalives and disconnects progress.
    pub fn set_max_datagrams_per_update(&mut self, limit: NonZeroUsize) {
        self.max_datagrams_per_update = limit;
    }

    pub fn max_clients(&self) -> usize {
        self.netcode_server.max_clients()
    }

    pub fn set_max_clients(&mut self, max_clients: usize) {
        self.netcode_server.set_max_clients(max_clients);
    }

    pub fn connected_clients(&self) -> usize {
        self.netcode_server.connected_clients()
    }

    pub fn user_data(&self, client_id: ClientId) -> Option<[u8; NETCODE_USER_DATA_BYTES]> {
        self.netcode_server.user_data(client_id)
    }

    pub fn client_addr(&self, client_id: ClientId) -> Option<SocketAddr> {
        self.netcode_server.client_addr(client_id)
    }

    pub fn time_since_last_received_packet(&self, client_id: ClientId) -> Option<Duration> {
        self.netcode_server
            .time_since_last_received_packet(client_id)
    }

    pub fn disconnect_all(&mut self, server: &mut RenetServer) {
        for client_id in self.netcode_server.clients_id() {
            let result = self.netcode_server.disconnect(client_id);
            let result = to_owned_server_result(result);
            self.handle_server_result(result, server);
        }
    }

    pub fn update(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
    ) -> Result<(), TransportError> {
        self.netcode_server.update(duration);

        crate::udp_io::receive_with_budget(self.max_datagrams_per_update, || {
            let (len, source) = self.socket.recv_from(&mut self.buffer)?;
            if len <= NETCODE_MAX_PACKET_BYTES {
                let result = self
                    .netcode_server
                    .process_packet(source, &mut self.buffer[..len]);
                let result = to_owned_server_result(result);
                self.handle_server_result(result, server);
            }
            Ok(())
        })?;

        for client_id in self.netcode_server.clients_id() {
            let result = self.netcode_server.update_client(client_id);
            let result = to_owned_server_result(result);
            self.handle_server_result(result, server);
        }

        for disconnection_id in server.disconnections_id() {
            let result = self.netcode_server.disconnect(disconnection_id);
            let result = to_owned_server_result(result);
            self.handle_server_result(result, server);
        }

        Ok(())
    }

    pub fn send_packets(&mut self, server: &mut RenetServer) {
        for client_id in self.netcode_server.clients_id() {
            let packets = match server.get_packets_to_send(client_id) {
                Ok(value) => value,
                Err(err) => {
                    log::error!("Cannot get outgoing packets for {client_id}: {err}");
                    continue;
                }
            };

            for packet in packets {
                match self
                    .netcode_server
                    .generate_payload_packet(client_id, &packet)
                {
                    Ok((addr, payload)) => {
                        if let Err(err) = self.socket.send_to(payload, addr) {
                            log::debug!(
                                "Failed to send packet to client {client_id} ({addr}): {err}"
                            );
                            break;
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to encrypt payload packet for {client_id}: {err}");
                        break;
                    }
                }
            }
        }
    }

    fn handle_server_result(&mut self, result: OwnedServerResult, server: &mut RenetServer) {
        match result {
            OwnedServerResult::None => {}
            OwnedServerResult::PacketToSend { addr, payload } => {
                if let Err(err) = self.socket.send_to(&payload, addr) {
                    log::debug!("Failed to send packet to {addr}: {err}");
                }
            }
            OwnedServerResult::Payload { client_id, payload } => {
                if let Err(err) = server.process_packet_from(&payload, client_id) {
                    log::error!("Failed to process payload for {client_id}: {err}");
                }
            }
            OwnedServerResult::ClientConnected {
                client_id,
                addr,
                payload,
            } => {
                if server.is_connected(client_id) {
                    log::error!(
                        "Duplicate client_id {client_id} across transports. Rejecting new UDP connection."
                    );
                    let disconnect = self.netcode_server.disconnect(client_id);
                    if let OwnedServerResult::ClientDisconnected {
                        payload: Some(disconnect_payload),
                        addr,
                        ..
                    } = to_owned_server_result(disconnect)
                    {
                        let _ = self.socket.send_to(&disconnect_payload, addr);
                    }
                    return;
                }

                server.add_connection(client_id);
                if let Err(err) = self.socket.send_to(&payload, addr) {
                    log::debug!("Failed to send connect payload to {addr}: {err}");
                }
            }
            OwnedServerResult::ClientDisconnected {
                client_id,
                addr,
                payload,
            } => {
                server.remove_connection(client_id);
                if let Some(payload) = payload {
                    let _ = self.socket.send_to(&payload, addr);
                }
            }
        }
    }
}
