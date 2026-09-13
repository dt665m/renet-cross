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
    packet_io::{Direction, ServerPacketGate, ServerPeerId},
};

#[derive(Debug)]
pub struct UdpNetcodeServerTransport {
    egress: crate::egress::Egress<SocketAddr>,
    socket: UdpSocket,
    netcode_server: NetcodeServer,
    // Enforce packet size after receiving the entire datagram; don't interpret a
    // truncated prefix as a valid packet or fail the pump on Windows oversize IO.
    buffer: [u8; 65_535],
    max_datagrams_per_update: NonZeroUsize,
    packets: ServerPacketGate,
}

impl UdpNetcodeServerTransport {
    pub fn new(server_config: ServerConfig, socket: UdpSocket) -> Result<Self, io::Error> {
        Self::new_with_config(server_config, socket, Default::default())
    }

    /// Install runtime packet controls before accepting clients.
    pub fn new_with_config(
        server_config: ServerConfig,
        socket: UdpSocket,
        config: crate::ServerTransportConfig,
    ) -> Result<Self, io::Error> {
        socket.set_nonblocking(true)?;
        let netcode_server = NetcodeServer::new(server_config);

        let mut packets = ServerPacketGate::default();
        if let Some(handle) = config.conditioner {
            packets.attach(handle);
        }
        Ok(Self {
            egress: crate::egress::Egress::new(crate::EgressBasis::NativeUdpIp),
            socket,
            netcode_server,
            buffer: [0; 65_535],
            max_datagrams_per_update: NonZeroUsize::new(256).unwrap(),
            packets,
        })
    }

    /// Per-destination ceiling, including netcode handshake/disconnect traffic.
    pub fn set_egress_limit(
        &mut self,
        config: Option<crate::EgressConfig>,
    ) -> Result<(), crate::EgressConfigError> {
        self.egress.configure(config)
    }
    pub fn egress_stats(&self) -> crate::EgressStats {
        self.egress.stats()
    }
    pub fn peer_egress_stats(&self, client_id: ClientId) -> Option<crate::EgressStats> {
        self.client_addr(client_id)
            .and_then(|addr| self.egress.peer_stats(addr))
    }

    pub fn peer_egress_allowance(&self, client_id: ClientId) -> Option<crate::EgressAllowance> {
        self.client_addr(client_id).map(|addr| {
            let mut allowance = self.egress.peer_allowance(addr);
            allowance.ip_udp_overhead = crate::egress::ip_overhead(addr);
            let queued = self.packets.queued_outgoing(ServerPeerId::Udp(addr));
            allowance.reserve_queued(queued.queued_bytes, queued.queued_packets);
            allowance
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
        self.packets.reset();
    }

    pub fn update(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
    ) -> Result<(), TransportError> {
        self.netcode_server.update(duration);

        crate::udp_io::receive_with_budget(self.max_datagrams_per_update, || {
            let (len, source) = self.socket.recv_from(&mut self.buffer)?;
            if len <= NETCODE_MAX_PACKET_BYTES
                && !self.packets.defer(
                    Direction::Incoming,
                    ServerPeerId::Udp(source),
                    &self.buffer[..len],
                )
            {
                let result = self
                    .netcode_server
                    .process_packet(source, &mut self.buffer[..len]);
                let result = to_owned_server_result(result);
                self.handle_server_result(result, server);
            }
            Ok(())
        })?;

        for (peer, mut packet) in self.packets.drain(Direction::Incoming) {
            if let ServerPeerId::Udp(source) = peer {
                let result = self.netcode_server.process_packet(source, &mut packet);
                let result = to_owned_server_result(result);
                self.handle_server_result(result, server);
            }
        }

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

        self.flush_outgoing();
        Ok(())
    }

    pub fn send_packets(&mut self, server: &mut RenetServer) {
        self.flush_outgoing();
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
                        if let Err(err) = send_packet(
                            &mut self.egress,
                            &mut self.packets,
                            &self.socket,
                            payload,
                            addr,
                        ) {
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
        self.flush_outgoing();
    }

    fn flush_outgoing(&mut self) {
        for (peer, packet) in self.packets.drain(Direction::Outgoing) {
            if let ServerPeerId::Udp(addr) = peer
                && let Err(err) = send_datagram(&mut self.egress, &self.socket, &packet, addr)
            {
                log::debug!("Failed to send queued packet to {addr}: {err}");
            }
        }
    }

    fn handle_server_result(&mut self, result: OwnedServerResult, server: &mut RenetServer) {
        match result {
            OwnedServerResult::None => {}
            OwnedServerResult::PacketToSend { addr, payload } => {
                if let Err(err) = send_packet(
                    &mut self.egress,
                    &mut self.packets,
                    &self.socket,
                    &payload,
                    addr,
                ) {
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
                        "Duplicate identity for client {client_id}. Rejecting new UDP connection."
                    );
                    let disconnect = self.netcode_server.disconnect(client_id);
                    if let OwnedServerResult::ClientDisconnected {
                        payload: Some(disconnect_payload),
                        addr,
                        ..
                    } = to_owned_server_result(disconnect)
                    {
                        let _ = send_datagram(
                            &mut self.egress,
                            &self.socket,
                            &disconnect_payload,
                            addr,
                        );
                    }
                    self.packets.remove(ServerPeerId::Udp(addr));
                    return;
                }

                server.add_connection(client_id);
                if let Err(err) = send_packet(
                    &mut self.egress,
                    &mut self.packets,
                    &self.socket,
                    &payload,
                    addr,
                ) {
                    log::debug!("Failed to send connect payload to {addr}: {err}");
                }
            }
            OwnedServerResult::ClientDisconnected {
                client_id,
                addr,
                payload,
            } => {
                // Teardown discards old traffic; send the terminal notification
                // immediately rather than retaining a queue for a departed session.
                self.packets.remove(ServerPeerId::Udp(addr));
                server.remove_connection(client_id);
                if let Some(payload) = payload {
                    let _ = send_datagram(&mut self.egress, &self.socket, &payload, addr);
                }
            }
        }
    }
}

impl UdpNetcodeServerTransport {
    /// Apply all-client policy, including new clients' netcode handshakes.
    pub fn set_conditioner(&mut self, handle: crate::server_conditioner::ServerConditionerHandle) {
        self.packets.attach(handle);
    }
    pub fn conditioner(&self) -> Option<crate::server_conditioner::ServerConditionerHandle> {
        self.packets.handle()
    }
    pub fn clear_conditioner(&mut self) {
        self.packets.detach();
    }
}

fn send_packet(
    egress: &mut crate::egress::Egress<SocketAddr>,
    gate: &mut ServerPacketGate,
    socket: &UdpSocket,
    bytes: &[u8],
    addr: SocketAddr,
) -> io::Result<usize> {
    if gate.defer(Direction::Outgoing, ServerPeerId::Udp(addr), bytes) {
        Ok(bytes.len())
    } else {
        send_datagram(egress, socket, bytes, addr)
    }
}

fn send_datagram(
    egress: &mut crate::egress::Egress<SocketAddr>,
    socket: &UdpSocket,
    payload: &[u8],
    addr: SocketAddr,
) -> io::Result<usize> {
    let overhead = crate::egress::ip_overhead(addr);
    if !egress.admit(addr, payload, overhead) {
        return Ok(payload.len());
    }
    let result = socket.send_to(payload, addr);
    egress.complete(
        addr,
        payload,
        overhead,
        result.as_ref().is_ok_and(|len| *len == payload.len()),
    );
    result
}
