use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
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
const MAX_WEBRTC_DRAIN_STEPS_PER_PEER: usize = 256;

#[derive(Debug)]
pub struct ServerPeer {
    client_id: ClientId,
    virtual_addr: SocketAddr,
    rtc: Rtc,
    data_channel: Option<ChannelId>,
    remote_addr: Option<SocketAddr>,
    next_timeout: Option<Instant>,
}

impl ServerPeer {
    pub fn new(client_id: ClientId, rtc: Rtc) -> Self {
        Self {
            client_id,
            virtual_addr: virtual_addr_for_client(client_id),
            rtc,
            data_channel: None,
            remote_addr: None,
            next_timeout: None,
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
    connected_client_ids: HashSet<ClientId>,
    pending_renet_removals: HashSet<ClientId>,
    addr_to_client_id: HashMap<SocketAddr, ClientId>,
    pending_server_results: Vec<OwnedServerResult>,
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
            connected_client_ids: HashSet::new(),
            pending_renet_removals: HashSet::new(),
            addr_to_client_id: HashMap::new(),
            pending_server_results: Vec::new(),
            send_payload_scratch: Vec::with_capacity(NETCODE_MAX_PACKET_BYTES),
            buffer: [0; WEBRTC_RECV_BUFFER_BYTES],
        })
    }

    pub fn add_peer(&mut self, client_id: ClientId, rtc: Rtc) -> Option<ServerPeer> {
        let previous = self.remove_peer(client_id);
        let peer = ServerPeer::new(client_id, rtc);
        self.addr_to_client_id
            .insert(peer.virtual_addr(), client_id);
        self.peers.insert(client_id, peer);
        previous
    }

    /// Remove a peer and release its netcode slot immediately. The shared Renet
    /// connection is removed on the next update or send_packets call.
    pub fn remove_peer(&mut self, client_id: ClientId) -> Option<ServerPeer> {
        self.addr_to_client_id
            .retain(|_, value| *value != client_id);
        let _ = self.netcode_server.disconnect(client_id);
        if self.connected_client_ids.remove(&client_id) {
            self.pending_renet_removals.insert(client_id);
        }
        // Results queued by the old RTC must never be delivered to a replacement.
        let old_addr = virtual_addr_for_client(client_id);
        self.pending_server_results.retain(|result| match result {
            OwnedServerResult::None => true,
            OwnedServerResult::PacketToSend { addr, .. } => *addr != old_addr,
            OwnedServerResult::Payload { client_id: id, .. }
            | OwnedServerResult::ClientConnected { client_id: id, .. }
            | OwnedServerResult::ClientDisconnected { client_id: id, .. } => *id != client_id,
        });
        self.peers.remove(&client_id)
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
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
        self.apply_pending_results(server);
        for client_id in self.netcode_server.clients_id() {
            let server_result = self.netcode_server.disconnect(client_id);
            let server_result = to_owned_server_result(server_result);
            self.handle_server_result(server_result, server);
        }
        // Half-open ICE/DTLS sessions have no netcode client yet.
        self.peers.clear();
        self.addr_to_client_id.clear();
        self.apply_pending_results(server);
    }

    pub fn update(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
    ) -> Result<(), TransportError> {
        self.update_at(duration, server, Instant::now())
    }

    fn update_at(
        &mut self,
        duration: Duration,
        server: &mut RenetServer,
        now: Instant,
    ) -> Result<(), TransportError> {
        self.apply_pending_results(server);
        self.netcode_server.update(duration);
        // Peers supplied by signaling can already have output pending.
        for client_id in self.peers.keys().copied().collect::<Vec<_>>() {
            self.flush_peer_output(client_id);
        }
        self.apply_pending_results(server);
        let routed_results = self.route_socket_input_to_peers(now)?;
        for result in routed_results {
            self.handle_server_result(result, server);
        }

        // Drain pending negotiation/output before advancing time. str0m requires a
        // complete output drain between every mutation, including timeout inputs.
        let peer_ids: Vec<ClientId> = self.peers.keys().copied().collect();
        for client_id in peer_ids {
            self.flush_peer_output(client_id);
            if let Some(peer) = self.peers.get_mut(&client_id)
                && peer.rtc.is_alive()
                && peer.next_timeout.is_some_and(|deadline| deadline <= now)
                && let Err(err) = peer.rtc.handle_input(Input::Timeout(now))
            {
                log::warn!("WebRTC timeout failed for {client_id}: {err}");
                peer.rtc.disconnect();
            }
            self.flush_peer_output(client_id);
        }
        self.apply_pending_results(server);

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

        self.apply_pending_results(server);
        Ok(())
    }

    pub fn send_packets(&mut self, server: &mut RenetServer) {
        self.apply_pending_results(server);
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
        }

        self.apply_pending_results(server);
    }

    fn apply_pending_results(&mut self, server: &mut RenetServer) {
        for client_id in self.pending_renet_removals.drain() {
            server.remove_connection(client_id);
        }
        // Handling netcode results can write to the channel and yield more events.
        while !self.pending_server_results.is_empty() {
            for result in std::mem::take(&mut self.pending_server_results) {
                self.handle_server_result(result, server);
            }
        }
    }

    fn flush_peer_output(&mut self, client_id: ClientId) {
        let Some(mut peer) = self.peers.remove(&client_id) else {
            return;
        };
        if peer.rtc.is_alive() {
            match self.drive_peer_with_limit(&mut peer, MAX_WEBRTC_DRAIN_STEPS_PER_PEER) {
                Ok(results) => self.pending_server_results.extend(results),
                Err(err) => {
                    // A peer failure must not abort the shared socket pump, or leave
                    // stale routes to a peer removed temporarily for borrowing.
                    log::warn!("WebRTC peer {client_id} output failed: {err}");
                    peer.rtc.disconnect();
                }
            }
        }
        if peer.rtc.is_alive() {
            self.peers.insert(client_id, peer);
        } else {
            self.addr_to_client_id
                .retain(|_, value| *value != client_id);
            let result = self.netcode_server.disconnect(client_id);
            self.pending_server_results
                .push(to_owned_server_result(result));
        }
    }

    fn route_socket_input_to_peers(
        &mut self,
        now: Instant,
    ) -> Result<Vec<OwnedServerResult>, TransportError> {
        let destination = self.receive_destination;
        let mut per_peer_datagrams = HashMap::<ClientId, usize>::new();
        for _ in 0..MAX_WEBRTC_DATAGRAMS_PER_UPDATE {
            let (len, source) = match self.socket.recv_from(&mut self.buffer) {
                Ok(packet) => packet,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let Ok(receive) = Receive::new(Protocol::Udp, source, destination, &self.buffer[..len])
            else {
                continue;
            };
            let input = Input::Receive(now, receive);
            let Some(client_id) = self.resolve_client_for_input(source, &input) else {
                continue;
            };
            let seen = per_peer_datagrams.entry(client_id).or_default();
            if *seen >= MAX_WEBRTC_DATAGRAMS_PER_PEER_PER_UPDATE {
                continue;
            }
            *seen += 1;
            if let Some(peer) = self.peers.get_mut(&client_id) {
                // A bounded drain can yield before reaching Timeout. Drop this
                // datagram rather than violating str0m's mutation contract.
                if peer.next_timeout.is_none() {
                    self.flush_peer_output(client_id);
                    continue;
                }
                if let Some(old_source) = peer.remote_addr.replace(source)
                    && old_source != source
                    && self.addr_to_client_id.get(&old_source) == Some(&client_id)
                {
                    self.addr_to_client_id.remove(&old_source);
                }
                self.addr_to_client_id.insert(source, client_id);
                if let Err(err) = peer.rtc.handle_input(input)
                    && !is_receive_queue_full_error(&err)
                    && !is_malformed_webrtc_input_error(&err)
                {
                    log::warn!("WebRTC input failed for {client_id}: {err}");
                    peer.rtc.disconnect();
                }
            }
            // Never ingest a second datagram while str0m still has queued output.
            self.flush_peer_output(client_id);
        }
        Ok(std::mem::take(&mut self.pending_server_results))
    }

    fn resolve_client_for_input(&self, source: SocketAddr, input: &Input) -> Option<ClientId> {
        if let Some(client_id) = self.addr_to_client_id.get(&source).copied()
            && let Some(peer) = self.peers.get(&client_id)
            && peer.rtc.accepts(input)
        {
            return Some(client_id);
        }
        // ICE credentials can authenticate checks from a new candidate address.
        // Pinning the first observed source blocks candidate switching/rebinding.
        self.peers
            .iter()
            .find_map(|(id, peer)| peer.rtc.accepts(input).then_some(*id))
    }

    fn drive_peer_with_limit(
        &mut self,
        peer: &mut ServerPeer,
        max_steps: usize,
    ) -> Result<Vec<OwnedServerResult>, TransportError> {
        peer.next_timeout = None;
        let mut pending_server_results = Vec::new();
        let mut steps = 0usize;

        loop {
            if steps >= max_steps {
                // Resume draining on the next opportunity. next_timeout remains
                // None so no input/write/timeout mutation can run in between.
                break;
            }
            steps = steps.saturating_add(1);

            match peer.rtc.poll_output()? {
                Output::Timeout(deadline) => {
                    peer.next_timeout = Some(deadline);
                    break;
                }
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
                if peer.data_channel != Some(data.id) || !data.binary {
                    return;
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
                if let OwnedServerResult::ClientConnected { client_id, .. } = &result
                    && *client_id != peer.client_id
                {
                    // The netcode identity must belong to the signaling peer. Never
                    // let a packet overwrite another peer's virtual address route.
                    let claimed_id = *client_id;
                    let _ = self.netcode_server.disconnect(claimed_id);
                    peer.rtc.disconnect();
                    return;
                }
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
            Event::ChannelClose(channel_id) if peer.data_channel == Some(channel_id) => {
                log::info!(
                    "webrtc channel closed for client {}: id={channel_id:?}; disconnecting peer",
                    peer.client_id
                );
                peer.rtc.disconnect();
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
                    self.remove_peer(client_id);
                    return;
                }

                server.add_connection(client_id);
                self.connected_client_ids.insert(client_id);
                self.addr_to_client_id.insert(addr, client_id);
                log::info!("netcode connected client_id={client_id} via webrtc route={addr}");
                self.send_to_peer_lossy(client_id, &payload);
            }
            OwnedServerResult::ClientDisconnected {
                client_id,
                addr,
                payload,
            } => {
                if self.connected_client_ids.remove(&client_id) {
                    server.remove_connection(client_id);
                }
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
        self.flush_peer_output(client_id);
        let peer = self
            .peers
            .get_mut(&client_id)
            .ok_or(TransportError::MissingPeer { client_id })?;
        if peer.next_timeout.is_none() {
            return Err(TransportError::DataChannelBackpressure { client_id });
        }
        let channel_id = peer
            .data_channel
            .ok_or(TransportError::DataChannelNotOpen { client_id })?;
        let mut channel = peer
            .rtc
            .channel(channel_id)
            .ok_or(TransportError::DataChannelNotOpen { client_id })?;

        let accepted = channel.write(true, payload);
        self.flush_peer_output(client_id);
        if !accepted? {
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

fn is_non_fatal_webrtc_send_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::NotConnected
    )
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
