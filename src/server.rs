use std::{
    any::Any,
    collections::HashMap,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

use renet::{ClientId, RenetServer};
use renetcode::{NETCODE_MAX_PACKET_BYTES, NETCODE_USER_DATA_BYTES, NetcodeServer, ServerConfig};
use str0m::{
    Event, IceConnectionState, Input, Output, Rtc,
    channel::ChannelId,
    net::{Protocol, Receive},
};

use crate::{
    TransportError,
    netcode_result::{OwnedServerResult, to_owned_server_result},
};

const WEBRTC_RECV_BUFFER_BYTES: usize = 65_535;
const MAX_WEBRTC_DATAGRAMS_PER_UPDATE: usize = 256;
const MAX_WEBRTC_DATAGRAMS_PER_PEER_PER_UPDATE: usize = 24;
const WEBRTC_INGEST_MICRO_BATCH_SIZE: usize = 8;
const MAX_WEBRTC_DRAIN_STEPS_PER_PEER: usize = 256;

#[derive(Debug)]
pub struct ServerPeer {
    client_id: ClientId,
    virtual_addr: SocketAddr,
    rtc: Rtc,
    data_channel: Option<ChannelId>,
    remote_addr: Option<SocketAddr>,
}

impl ServerPeer {
    pub fn new(client_id: ClientId, rtc: Rtc) -> Self {
        Self {
            client_id,
            virtual_addr: virtual_addr_for_client(client_id),
            rtc,
            data_channel: None,
            remote_addr: None,
        }
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn data_channel(&self) -> Option<ChannelId> {
        self.data_channel
    }

    pub fn virtual_addr(&self) -> SocketAddr {
        self.virtual_addr
    }

    pub fn set_data_channel(&mut self, channel_id: ChannelId) {
        self.data_channel = Some(channel_id);
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    pub fn rtc(&self) -> &Rtc {
        &self.rtc
    }

    pub fn rtc_mut(&mut self) -> &mut Rtc {
        &mut self.rtc
    }
}

#[derive(Debug)]
pub struct WebRtcNetcodeServerTransport {
    socket: UdpSocket,
    netcode_server: NetcodeServer,
    receive_destination: SocketAddr,
    peers: HashMap<ClientId, ServerPeer>,
    addr_to_client_id: HashMap<SocketAddr, ClientId>,
    drain_round_robin_cursor: usize,
    send_payload_scratch: Vec<u8>,
    buffer: [u8; WEBRTC_RECV_BUFFER_BYTES],
}

fn virtual_addr_for_client(client_id: ClientId) -> SocketAddr {
    let hi = (client_id >> 48) as u16;
    let mid_hi = (client_id >> 32) as u16;
    let mid_lo = (client_id >> 16) as u16;
    let lo = client_id as u16;
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, hi, mid_hi, mid_lo, lo)),
        1,
    )
}

impl WebRtcNetcodeServerTransport {
    pub fn new(server_config: ServerConfig, socket: UdpSocket) -> Result<Self, io::Error> {
        socket.set_nonblocking(true)?;
        let receive_destination = server_config
            .public_addresses
            .first()
            .copied()
            .unwrap_or(socket.local_addr()?);
        let netcode_server = NetcodeServer::new(server_config);

        Ok(Self {
            socket,
            netcode_server,
            receive_destination,
            peers: HashMap::new(),
            addr_to_client_id: HashMap::new(),
            drain_round_robin_cursor: 0,
            send_payload_scratch: Vec::with_capacity(NETCODE_MAX_PACKET_BYTES),
            buffer: [0; WEBRTC_RECV_BUFFER_BYTES],
        })
    }

    pub fn add_peer(&mut self, client_id: ClientId, rtc: Rtc) -> Option<ServerPeer> {
        self.addr_to_client_id
            .retain(|_, value| *value != client_id);
        let peer = ServerPeer::new(client_id, rtc);
        self.addr_to_client_id
            .insert(peer.virtual_addr(), client_id);
        self.peers.insert(client_id, peer)
    }

    pub fn remove_peer(&mut self, client_id: ClientId) -> Option<ServerPeer> {
        self.addr_to_client_id
            .retain(|_, value| *value != client_id);
        self.peers.remove(&client_id)
    }

    pub fn peer(&self, client_id: ClientId) -> Option<&ServerPeer> {
        self.peers.get(&client_id)
    }

    pub fn peer_mut(&mut self, client_id: ClientId) -> Option<&mut ServerPeer> {
        self.peers.get_mut(&client_id)
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.netcode_server.addresses()
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
            let server_result = self.netcode_server.disconnect(client_id);
            let server_result = to_owned_server_result(server_result);
            self.handle_server_result(server_result, server);
        }
    }

    pub fn update(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
    ) -> Result<(), TransportError> {
        self.netcode_server.update(duration);
        let routed_results = self.route_socket_input_to_peers()?;
        for result in routed_results {
            self.handle_server_result(result, server);
        }

        // Drive every peer's RTC state.
        let peer_ids: Vec<ClientId> = self.peers.keys().copied().collect();
        for client_id in peer_ids {
            let Some(mut peer) = self.peers.remove(&client_id) else {
                continue;
            };

            let mut pending_server_results = Vec::new();
            if peer.rtc.is_alive() {
                match self.drive_peer_with_limit(&mut peer, MAX_WEBRTC_DRAIN_STEPS_PER_PEER) {
                    Ok(results) => {
                        pending_server_results = results;
                    }
                    Err(err) => {
                        if matches!(
                            err,
                            TransportError::Rtc(_) | TransportError::PeerPanicked { .. }
                        ) {
                            log::warn!(
                                "webRTC peer {client_id} runtime failure; disconnecting peer: {err}"
                            );
                            peer.rtc.disconnect();
                        } else {
                            return Err(err);
                        }
                    }
                }
            }

            if peer.rtc.is_alive() {
                self.peers.insert(client_id, peer);
            } else {
                self.addr_to_client_id
                    .retain(|_, value| *value != client_id);
                let result = self.netcode_server.disconnect(client_id);
                let result = to_owned_server_result(result);
                self.handle_server_result(result, server);
            }

            for result in pending_server_results {
                self.handle_server_result(result, server);
            }
        }

        // Keep netcode server-side keepalive/timeout state advancing.
        for client_id in self.netcode_server.clients_id() {
            let server_result = self.netcode_server.update_client(client_id);
            let server_result = to_owned_server_result(server_result);
            self.handle_server_result(server_result, server);
        }

        // Respect requested disconnections from renet.
        for disconnection_id in server.disconnections_id() {
            let server_result = self.netcode_server.disconnect(disconnection_id);
            let server_result = to_owned_server_result(server_result);
            self.handle_server_result(server_result, server);
        }

        Ok(())
    }

    pub fn send_packets(&mut self, server: &mut RenetServer) {
        let mut dirty_peers = Vec::new();

        for client_id in self.netcode_server.clients_id() {
            let packets = match server.get_packets_to_send(client_id) {
                Ok(value) => value,
                Err(err) => {
                    log::error!("Cannot get outgoing packets for {client_id}: {err}");
                    continue;
                }
            };

            if packets.is_empty() {
                continue;
            }

            for packet in packets {
                match self
                    .netcode_server
                    .generate_payload_packet(client_id, &packet)
                {
                    Ok((_addr, payload)) => {
                        self.send_payload_scratch.clear();
                        self.send_payload_scratch.extend_from_slice(payload);
                        let payload = std::mem::take(&mut self.send_payload_scratch);
                        self.send_to_peer_lossy(client_id, &payload);
                        self.send_payload_scratch = payload;
                    }
                    Err(err) => {
                        log::error!("Failed to encrypt payload packet for {client_id}: {err}");
                        break;
                    }
                }
            }

            dirty_peers.push(client_id);
        }

        // Flush RTC output immediately so SCTP data is transmitted this frame
        // rather than sitting buffered until the next update().
        for client_id in dirty_peers {
            self.flush_peer_output(client_id);
        }
    }

    fn flush_peer_output(&mut self, client_id: ClientId) {
        let Some(peer) = self.peers.get_mut(&client_id) else {
            return;
        };
        if !peer.rtc.is_alive() {
            return;
        }

        for _ in 0..MAX_WEBRTC_DRAIN_STEPS_PER_PEER {
            match peer.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    if let Err(err) = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                    {
                        if is_non_fatal_webrtc_send_error(&err) {
                            continue;
                        }
                        log::debug!("send error during flush for client {client_id}: {err}");
                        break;
                    }
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(_)) | Err(_) => break,
            }
        }
    }

    fn route_socket_input_to_peers(&mut self) -> Result<Vec<OwnedServerResult>, TransportError> {
        let destination = self.receive_destination;
        let mut pending_server_results = Vec::new();
        let mut per_peer_datagrams = HashMap::<ClientId, usize>::new();
        let mut touched_peers = Vec::<ClientId>::new();
        let mut datagrams_since_drain = 0usize;
        let mut received_datagrams = 0usize;
        loop {
            if received_datagrams >= MAX_WEBRTC_DATAGRAMS_PER_UPDATE {
                log::trace!(
                    "webRTC socket ingest budget reached for this update (processed={MAX_WEBRTC_DATAGRAMS_PER_UPDATE})"
                );
                break;
            }
            match self.socket.recv_from(&mut self.buffer) {
                Ok((len, source)) => {
                    received_datagrams = received_datagrams.saturating_add(1);
                    datagrams_since_drain = datagrams_since_drain.saturating_add(1);
                    let receive =
                        match Receive::new(Protocol::Udp, source, destination, &self.buffer[..len])
                        {
                            Ok(value) => value,
                            Err(err) => {
                                log::debug!("Ignoring non-webrtc packet from {source}: {err}");
                                continue;
                            }
                        };

                    let input = Input::Receive(Instant::now(), receive);

                    if self.peers.is_empty() {
                        continue;
                    }

                    let matched_client_id = self.resolve_client_for_input(source, &input);

                    if let Some(client_id) = matched_client_id {
                        let seen_for_peer = per_peer_datagrams.entry(client_id).or_insert(0);
                        if *seen_for_peer >= MAX_WEBRTC_DATAGRAMS_PER_PEER_PER_UPDATE {
                            log::trace!(
                                "dropping excess webRTC datagram for client {client_id} (per-update cap {MAX_WEBRTC_DATAGRAMS_PER_PEER_PER_UPDATE})"
                            );
                            if datagrams_since_drain >= WEBRTC_INGEST_MICRO_BATCH_SIZE {
                                self.drain_touched_peers_round_robin(
                                    &mut touched_peers,
                                    &mut pending_server_results,
                                )?;
                                datagrams_since_drain = 0;
                            }
                            continue;
                        }
                        *seen_for_peer += 1;

                        let mut queue_overflow = false;
                        if let Some(peer) = self.peers.get_mut(&client_id) {
                            peer.remote_addr.get_or_insert(source);
                            self.addr_to_client_id.insert(source, client_id);
                            let handle_input_result =
                                catch_unwind(AssertUnwindSafe(|| peer.rtc.handle_input(input)));
                            let handle_input_result = match handle_input_result {
                                Ok(result) => result,
                                Err(payload) => {
                                    let panic = panic_payload_to_string(payload.as_ref());
                                    log::warn!(
                                        "webRTC peer {client_id} panicked during handle_input from {source}: {panic}; disconnecting peer"
                                    );
                                    peer.rtc.disconnect();
                                    touched_peers.push(client_id);
                                    continue;
                                }
                            };

                            if let Err(err) = handle_input_result {
                                if is_receive_queue_full_error(&err) {
                                    log::debug!(
                                        "dropping overloaded DTLS datagram from {source} for client {client_id}: {err}"
                                    );
                                    queue_overflow = true;
                                } else if is_malformed_webrtc_input_error(&err) {
                                    log::debug!(
                                        "dropping malformed DTLS/SCTP datagram from {source} for client {client_id}: {err}"
                                    );
                                    continue;
                                } else {
                                    return Err(err.into());
                                }
                            }
                            touched_peers.push(client_id);
                        } else {
                            self.addr_to_client_id.remove(&source);
                        }

                        if queue_overflow || datagrams_since_drain >= WEBRTC_INGEST_MICRO_BATCH_SIZE
                        {
                            self.drain_touched_peers_round_robin(
                                &mut touched_peers,
                                &mut pending_server_results,
                            )?;
                            datagrams_since_drain = 0;
                        }
                    } else {
                        log::debug!(
                            "No peer accepted incoming packet from {source} (destination={destination}, peers={})",
                            self.peers.len()
                        );
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => break,
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(err) => return Err(err.into()),
            }
        }

        self.drain_touched_peers_round_robin(&mut touched_peers, &mut pending_server_results)?;
        Ok(pending_server_results)
    }

    fn resolve_client_for_input(&self, source: SocketAddr, input: &Input) -> Option<ClientId> {
        if let Some(client_id) = self.addr_to_client_id.get(&source).copied()
            && self.peers.contains_key(&client_id)
        {
            return Some(client_id);
        }

        self.peers.iter().find_map(|(client_id, peer)| {
            if !source_matches_known_remote(source, peer.remote_addr()) {
                return None;
            }

            if peer.rtc.accepts(input) {
                Some(*client_id)
            } else {
                None
            }
        })
    }

    fn drain_touched_peers_round_robin(
        &mut self,
        touched_peers: &mut Vec<ClientId>,
        pending_server_results: &mut Vec<OwnedServerResult>,
    ) -> Result<(), TransportError> {
        if touched_peers.is_empty() {
            return Ok(());
        }

        touched_peers.sort_unstable();
        touched_peers.dedup();

        let count = touched_peers.len();
        let start = self.drain_round_robin_cursor % count;

        for offset in 0..count {
            let index = (start + offset) % count;
            let client_id = touched_peers[index];
            self.drive_peer_with_budget(
                client_id,
                MAX_WEBRTC_DRAIN_STEPS_PER_PEER,
                pending_server_results,
            )?;
        }

        self.drain_round_robin_cursor = (start + 1) % count;
        touched_peers.clear();
        Ok(())
    }

    fn drive_peer_with_budget(
        &mut self,
        client_id: ClientId,
        max_steps: usize,
        pending_server_results: &mut Vec<OwnedServerResult>,
    ) -> Result<(), TransportError> {
        let Some(mut peer) = self.peers.remove(&client_id) else {
            return Ok(());
        };

        let mut peer_results = Vec::new();
        if peer.rtc.is_alive() {
            peer_results = self.drive_peer_with_limit(&mut peer, max_steps)?;
        }

        if peer.rtc.is_alive() {
            self.peers.insert(client_id, peer);
        } else {
            self.addr_to_client_id
                .retain(|_, value| *value != client_id);
            let result = self.netcode_server.disconnect(client_id);
            pending_server_results.push(to_owned_server_result(result));
        }

        pending_server_results.extend(peer_results);
        Ok(())
    }

    fn drive_peer_with_limit(
        &mut self,
        peer: &mut ServerPeer,
        max_steps: usize,
    ) -> Result<Vec<OwnedServerResult>, TransportError> {
        let timeout_result = catch_unwind(AssertUnwindSafe(|| {
            peer.rtc.handle_input(Input::Timeout(Instant::now()))
        }));
        match timeout_result {
            Ok(result) => result?,
            Err(payload) => {
                return Err(TransportError::PeerPanicked {
                    client_id: peer.client_id(),
                    context: "handle_timeout_input",
                    panic: panic_payload_to_string(payload.as_ref()),
                });
            }
        }

        let mut pending_server_results = Vec::new();
        let mut steps = 0usize;

        loop {
            if steps >= max_steps {
                break;
            }
            steps = steps.saturating_add(1);

            let poll_output_result = catch_unwind(AssertUnwindSafe(|| peer.rtc.poll_output()));
            let output = match poll_output_result {
                Ok(result) => result?,
                Err(payload) => {
                    return Err(TransportError::PeerPanicked {
                        client_id: peer.client_id(),
                        context: "poll_output",
                        panic: panic_payload_to_string(payload.as_ref()),
                    });
                }
            };

            match output {
                Output::Timeout(_) => break,
                Output::Transmit(transmit) => {
                    if let Err(err) = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                    {
                        if is_non_fatal_webrtc_send_error(&err) {
                            log::trace!(
                                "ignoring non-fatal webrtc send error to {}: {}",
                                transmit.destination,
                                err
                            );
                            continue;
                        }
                        return Err(err.into());
                    }
                }
                Output::Event(event) => {
                    self.handle_peer_event(peer, event, &mut pending_server_results)
                }
            }
        }

        Ok(pending_server_results)
    }

    fn handle_peer_event(
        &mut self,
        peer: &mut ServerPeer,
        event: Event,
        pending_server_results: &mut Vec<OwnedServerResult>,
    ) {
        match event {
            Event::ChannelOpen(channel_id, label) => {
                if peer.data_channel.is_none() {
                    peer.data_channel = Some(channel_id);
                }
                log::info!(
                    "webrtc channel opened for client {}: id={channel_id:?} label={label}",
                    peer.client_id
                );
            }
            Event::ChannelData(data) => {
                if peer.data_channel.is_none() {
                    peer.data_channel = Some(data.id);
                }

                let source_addr = peer.virtual_addr();
                log::trace!(
                    "webrtc channel data client_id={} bytes={} binary={}",
                    peer.client_id,
                    data.data.len(),
                    data.binary
                );

                let mut packet = data.data;
                let result = self.netcode_server.process_packet(source_addr, &mut packet);
                let result = to_owned_server_result(result);
                pending_server_results.push(result);
            }
            Event::IceConnectionStateChange(state) => {
                if state == IceConnectionState::Disconnected {
                    log::info!(
                        "webrtc ICE state {:?} for client {}, disconnecting peer",
                        state,
                        peer.client_id
                    );
                    peer.rtc.disconnect();
                }
            }
            Event::ChannelClose(channel_id) => {
                if peer.data_channel == Some(channel_id) {
                    log::info!(
                        "webrtc channel closed for client {}: id={channel_id:?}; disconnecting peer",
                        peer.client_id
                    );
                    peer.rtc.disconnect();
                }
            }
            _ => {}
        }
    }

    fn handle_server_result(&mut self, server_result: OwnedServerResult, server: &mut RenetServer) {
        match server_result {
            OwnedServerResult::None => {}
            OwnedServerResult::PacketToSend { payload, addr } => {
                if let Some(client_id) = self.addr_to_client_id.get(&addr).copied() {
                    self.send_to_peer_lossy(client_id, &payload);
                } else {
                    log::warn!(
                        "Netcode produced packet for unknown address {addr} (known_routes={})",
                        self.addr_to_client_id.len()
                    );
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
                        "Duplicate client_id {client_id} across transports. Rejecting new WebRTC connection."
                    );
                    let disconnect = self.netcode_server.disconnect(client_id);
                    if let OwnedServerResult::ClientDisconnected {
                        payload: Some(disconnect_payload),
                        ..
                    } = to_owned_server_result(disconnect)
                    {
                        self.send_to_peer_lossy(client_id, &disconnect_payload);
                    }
                    return;
                }

                server.add_connection(client_id);
                self.addr_to_client_id.insert(addr, client_id);
                log::info!("netcode connected client_id={client_id} via webrtc route={addr}");
                self.send_to_peer_lossy(client_id, &payload);
            }
            OwnedServerResult::ClientDisconnected {
                client_id,
                addr,
                payload,
            } => {
                server.remove_connection(client_id);
                self.addr_to_client_id.remove(&addr);

                if let Some(payload) = payload {
                    self.send_to_peer_lossy(client_id, &payload);
                }

                self.addr_to_client_id
                    .retain(|_, value| *value != client_id);
                self.peers.remove(&client_id);
            }
        }
    }

    fn send_to_peer_lossy(&mut self, client_id: ClientId, payload: &[u8]) {
        if let Err(err) = self.send_to_peer(client_id, payload) {
            log::debug!("Failed to send packet to peer {client_id}: {err}");
        }
    }

    fn send_to_peer(&mut self, client_id: ClientId, payload: &[u8]) -> Result<(), TransportError> {
        let peer = self
            .peers
            .get_mut(&client_id)
            .ok_or(TransportError::MissingPeer { client_id })?;
        let channel_id = peer
            .data_channel
            .ok_or(TransportError::DataChannelNotOpen { client_id })?;
        let mut channel = peer
            .rtc
            .channel(channel_id)
            .ok_or(TransportError::DataChannelNotOpen { client_id })?;

        let accepted = channel.write(true, payload)?;
        if !accepted {
            return Err(TransportError::DataChannelBackpressure { client_id });
        }

        Ok(())
    }
}

fn is_receive_queue_full_error(err: &str0m::RtcError) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("receive queue full")
}

fn is_malformed_webrtc_input_error(err: &str0m::RtcError) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("chunk too short")
        || message.contains("unable to parse sctp packet")
        || message.contains("failed to parse sctp packet")
}

fn source_matches_known_remote(source: SocketAddr, known_remote: Option<SocketAddr>) -> bool {
    match known_remote {
        Some(remote) => source == remote,
        None => true,
    }
}

fn is_non_fatal_webrtc_send_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::NotConnected
    )
}

fn panic_payload_to_string(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_owned();
    }

    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }

    "<non-string panic payload>".to_owned()
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::source_matches_known_remote;

    #[test]
    fn source_matches_when_remote_unknown() {
        let source = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 7), 44321));
        assert!(source_matches_known_remote(source, None));
    }

    #[test]
    fn source_matches_exact_remote() {
        let source = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 9), 55000));
        assert!(source_matches_known_remote(source, Some(source)));
    }

    #[test]
    fn source_rejects_same_ip_different_port() {
        let source = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 9), 55001));
        let known = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 9), 55000));
        assert!(!source_matches_known_remote(source, Some(known)));
    }

    #[test]
    fn source_rejects_different_ip() {
        let source = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 9), 55001));
        let known = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 10), 55001));
        assert!(!source_matches_known_remote(source, Some(known)));
    }
}
