use super::*;
use renet::{ConnectionConfig, DefaultChannel, RenetClient};
use renetcode::{ClientAuthentication, NetcodeClient, ServerAuthentication};
use str0m::{
    Candidate,
    channel::{ChannelConfig, Reliability},
};

/// Real ICE/DTLS/SCTP and netcode over loopback, driven by a synthetic clock.
/// No browser, signaling service, sleeps, or external STUN server are needed.
struct Harness {
    transport: WebRtcNetcodeServerTransport,
    server: RenetServer,
    client: RenetClient,
    netcode: NetcodeClient,
    rtc: Rtc,
    socket: UdpSocket,
    channel: ChannelId,
    channel_open: bool,
    now: Instant,
}

impl Harness {
    fn new(signaling_id: ClientId, netcode_id: ClientId) -> Self {
        let now = Instant::now();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_socket.local_addr().unwrap();
        let mut rtc = Rtc::new(now);
        let mut remote = Rtc::new(now);
        rtc.add_local_candidate(Candidate::host(socket.local_addr().unwrap(), "udp").unwrap());
        remote.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());
        let mut change = rtc.sdp_api();
        let channel = change.add_channel_with_config(ChannelConfig {
            label: "renet".into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..Default::default()
        });
        let (offer, pending) = change.apply().unwrap();
        let answer = remote.sdp_api().accept_offer(offer).unwrap();
        rtc.sdp_api().accept_answer(pending, answer).unwrap();
        let mut transport = WebRtcNetcodeServerTransport::new(
            ServerConfig {
                current_time: Duration::ZERO,
                max_clients: 8,
                protocol_id: 7,
                public_addresses: vec![server_addr],
                authentication: ServerAuthentication::Unsecure,
            },
            server_socket,
        )
        .unwrap();
        transport.add_peer(signaling_id, remote);
        Self {
            transport,
            server: RenetServer::new(ConnectionConfig::default()),
            client: RenetClient::new(ConnectionConfig::default()),
            netcode: NetcodeClient::new(
                Duration::ZERO,
                ClientAuthentication::Unsecure {
                    protocol_id: 7,
                    client_id: netcode_id,
                    server_addr,
                    user_data: None,
                },
            )
            .unwrap(),
            rtc,
            socket,
            channel,
            channel_open: false,
            now,
        }
    }

    fn drain_client(&mut self) {
        for _ in 0..4096 {
            match self.rtc.poll_output().unwrap() {
                Output::Timeout(_) => return,
                Output::Transmit(packet) => {
                    self.socket
                        .send_to(&packet.contents, packet.destination)
                        .unwrap();
                }
                Output::Event(Event::ChannelOpen(id, _)) if id == self.channel => {
                    self.channel_open = true
                }
                Output::Event(Event::ChannelData(mut data)) => {
                    if let Some(payload) = self.netcode.process_packet(&mut data.data) {
                        self.client.process_packet(payload);
                    }
                }
                _ => {}
            }
        }
        panic!("client failed to drain");
    }

    fn write(&mut self, packet: &[u8]) {
        if let Some(mut channel) = self.rtc.channel(self.channel) {
            assert!(channel.write(true, packet).unwrap());
            self.drain_client();
        }
    }

    fn step(&mut self) {
        let delta = Duration::from_millis(10);
        self.now += delta;
        self.drain_client();
        self.rtc.handle_input(Input::Timeout(self.now)).unwrap();
        self.drain_client();
        let mut buffer = [0; WEBRTC_RECV_BUFFER_BYTES];
        for _ in 0..4096 {
            let (len, source) = match self.socket.recv_from(&mut buffer) {
                Ok(packet) => packet,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("client recv: {err}"),
            };
            let receive = Receive::new(
                Protocol::Udp,
                source,
                self.socket.local_addr().unwrap(),
                &buffer[..len],
            )
            .unwrap();
            self.rtc
                .handle_input(Input::Receive(self.now, receive))
                .unwrap();
            self.drain_client();
        }
        if self.netcode.is_connected() {
            self.client.set_connected();
        }
        self.client.update(delta);
        if let Some((packet, _)) = self.netcode.update(delta) {
            let packet = packet.to_vec();
            self.write(&packet);
        }
        if self.netcode.is_connected() {
            for packet in self.client.get_packets_to_send() {
                let (_, encrypted) = self.netcode.generate_payload_packet(&packet).unwrap();
                let encrypted = encrypted.to_vec();
                self.write(&encrypted);
            }
        }
        self.server.update(delta);
        self.transport
            .update_at(delta, &mut self.server, self.now)
            .unwrap();
        self.transport.send_packets(&mut self.server);
    }

    fn connect(&mut self) {
        for _ in 0..500 {
            self.step();
            if self.client.is_connected() && self.server.is_connected(42) {
                return;
            }
        }
        panic!("connection failed after five simulated seconds");
    }
}

#[test]
fn real_webrtc_netcode_bidirectional_delivery_and_disconnect() {
    let mut harness = Harness::new(42, 42);
    harness.connect();
    harness.client.send_message(
        DefaultChannel::ReliableOrdered,
        b"input sequence 100".to_vec(),
    );
    harness.server.send_message(
        42,
        DefaultChannel::ReliableOrdered,
        b"authoritative tick 50".to_vec(),
    );
    for _ in 0..100 {
        harness.step();
    }
    assert_eq!(
        harness
            .server
            .receive_message(42, DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"input sequence 100"
    );
    assert_eq!(
        harness
            .client
            .receive_message(DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"authoritative tick 50"
    );
    harness.server.disconnect(42);
    harness.step();
    assert!(!harness.server.is_connected(42));
    assert!(harness.transport.peer(42).is_none());
    assert!(
        harness
            .transport
            .addr_to_client_id
            .values()
            .all(|id| *id != 42)
    );
}

#[test]
fn send_flush_preserves_incoming_channel_events() {
    let mut harness = Harness::new(42, 42);
    harness.connect();
    // Settle handshake traffic, then leave incoming application data queued
    // inside str0m as send_packets is about to flush its output.
    for _ in 0..10 {
        harness.step();
    }
    let mut incoming = [0; WEBRTC_RECV_BUFFER_BYTES];
    while let Ok((len, source)) = harness.socket.recv_from(&mut incoming) {
        let receive = Receive::new(
            Protocol::Udp,
            source,
            harness.socket.local_addr().unwrap(),
            &incoming[..len],
        )
        .unwrap();
        harness
            .rtc
            .handle_input(Input::Receive(harness.now, receive))
            .unwrap();
        harness.drain_client();
    }
    harness.client.send_message(
        DefaultChannel::ReliableOrdered,
        b"must not disappear".to_vec(),
    );
    harness.client.update(Duration::from_millis(10));
    let packets = harness.client.get_packets_to_send();
    assert!(!packets.is_empty());
    for packet in packets {
        let (_, encrypted) = harness.netcode.generate_payload_packet(&packet).unwrap();
        let encrypted = encrypted.to_vec();
        harness.write(&encrypted);
    }
    harness.now += Duration::from_millis(500);
    harness
        .rtc
        .handle_input(Input::Timeout(harness.now))
        .unwrap();
    harness.drain_client();
    let mut buffer = [0; WEBRTC_RECV_BUFFER_BYTES];
    let mut ingested = 0;
    loop {
        let (len, source) = match harness.transport.socket.recv_from(&mut buffer) {
            Ok(packet) => packet,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("recv: {err}"),
        };
        let receive = Receive::new(
            Protocol::Udp,
            source,
            harness.transport.receive_destination,
            &buffer[..len],
        )
        .unwrap();
        harness
            .transport
            .peers
            .get_mut(&42)
            .unwrap()
            .rtc
            .handle_input(Input::Receive(harness.now, receive))
            .unwrap();
        // This was the old event-discarding path, used after channel writes.
        harness.transport.flush_peer_output(42);
        ingested += 1;
    }
    assert!(ingested > 0);
    harness.transport.apply_pending_results(&mut harness.server);
    assert_eq!(
        harness
            .server
            .receive_message(42, DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"must not disappear"
    );
}

#[test]
fn reliable_burst_survives_per_update_ingest_budget() {
    let mut harness = Harness::new(42, 42);
    harness.connect();
    for sequence in 0u32..80 {
        let mut payload = vec![0xA5; 1200];
        payload[..4].copy_from_slice(&sequence.to_le_bytes());
        harness
            .client
            .send_message(DefaultChannel::ReliableOrdered, payload);
    }
    let mut received = 0u32;
    for _ in 0..1000 {
        harness.step();
        while let Some(message) = harness
            .server
            .receive_message(42, DefaultChannel::ReliableOrdered)
        {
            assert_eq!(
                u32::from_le_bytes(message[..4].try_into().unwrap()),
                received
            );
            assert_eq!(message.len(), 1200);
            received += 1;
        }
        if received == 80 {
            break;
        }
    }
    assert_eq!(received, 80);
    assert!(harness.server.is_connected(42));
}

#[test]
fn duplicate_webrtc_identity_does_not_disconnect_existing_renet_client() {
    let mut harness = Harness::new(42, 42);
    // Establish a real native connection before the competing WebRTC peer.
    let mut native = NativePeer::new(42);
    for _ in 0..100 {
        native.step(&mut harness.server);
        if native.client.is_connected() {
            break;
        }
    }
    assert!(native.client.is_connected());
    for _ in 0..300 {
        native.step(&mut harness.server);
        harness.step();
    }
    assert!(native.client.is_connected());
    assert!(harness.server.is_connected(42));
    assert_eq!(harness.transport.connected_clients(), 0);
    assert!(harness.transport.peer(42).is_none());
    // Even a late queued disconnect cannot remove the foreign connection.
    harness.transport.handle_server_result(
        OwnedServerResult::ClientDisconnected {
            client_id: 42,
            addr: virtual_addr_for_client(42),
            payload: None,
        },
        &mut harness.server,
    );
    assert!(harness.server.is_connected(42));
}

#[test]
fn signaling_identity_cannot_claim_another_netcode_identity() {
    let mut harness = Harness::new(42, 99);
    for _ in 0..300 {
        harness.step();
    }
    assert!(!harness.server.is_connected(99));
    assert!(!harness.server.is_connected(42));
    assert_eq!(harness.transport.connected_clients(), 0);
    assert!(harness.transport.peer(42).is_none());
}

#[test]
fn ice_candidate_checks_can_replace_previously_observed_source() {
    let mut harness = Harness::new(42, 42);
    let obsolete: SocketAddr = "127.0.0.1:43211".parse().unwrap();
    harness.transport.peers.get_mut(&42).unwrap().remote_addr = Some(obsolete);
    harness.transport.addr_to_client_id.insert(obsolete, 42);
    harness.connect();
    assert_eq!(
        harness.transport.peer(42).unwrap().remote_addr(),
        Some(harness.socket.local_addr().unwrap())
    );
    assert!(!harness.transport.addr_to_client_id.contains_key(&obsolete));
}

#[test]
fn stale_address_cache_does_not_bypass_rtc_demultiplexing() {
    let mut harness = Harness::new(42, 42);
    let source: SocketAddr = "127.0.0.1:43210".parse().unwrap();
    harness.transport.addr_to_client_id.insert(source, 42);
    // A syntactically valid DTLS record from an unrecognized candidate.
    let packet = [22, 254, 253, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let receive = Receive::new(
        Protocol::Udp,
        source,
        harness.transport.receive_destination,
        &packet,
    )
    .unwrap();
    assert_eq!(
        harness
            .transport
            .resolve_client_for_input(source, &Input::Receive(harness.now, receive)),
        None
    );
}

#[test]
fn output_budget_exhaustion_defers_mutation_until_drained() {
    let mut harness = Harness::new(42, 42);
    let mut peer = harness.transport.peers.remove(&42).unwrap();
    assert!(
        harness
            .transport
            .drive_peer_with_limit(&mut peer, 0)
            .is_ok()
    );
    assert!(peer.next_timeout.is_none());
    assert!(peer.rtc.is_alive());
    assert!(
        harness
            .transport
            .drive_peer_with_limit(&mut peer, 4096)
            .is_ok()
    );
    assert!(peer.next_timeout.is_some());
}

struct NativePeer {
    server_transport: crate::UdpNetcodeServerTransport,
    client_transport: crate::UdpNetcodeClientTransport,
    client: RenetClient,
}

impl NativePeer {
    fn new(client_id: ClientId) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().unwrap();
        Self {
            server_transport: crate::UdpNetcodeServerTransport::new(
                ServerConfig {
                    current_time: Duration::ZERO,
                    max_clients: 8,
                    protocol_id: 7,
                    public_addresses: vec![server_addr],
                    authentication: ServerAuthentication::Unsecure,
                },
                socket,
            )
            .unwrap(),
            client_transport: crate::UdpNetcodeClientTransport::new(
                Duration::ZERO,
                ClientAuthentication::Unsecure {
                    protocol_id: 7,
                    client_id,
                    server_addr,
                    user_data: None,
                },
                UdpSocket::bind("127.0.0.1:0").unwrap(),
            )
            .unwrap(),
            client: RenetClient::new(ConnectionConfig::default()),
        }
    }

    fn step(&mut self, server: &mut RenetServer) {
        let duration = Duration::from_millis(10);
        self.client.update(duration);
        self.client_transport
            .update(duration, &mut self.client)
            .unwrap();
        self.client_transport
            .send_packets(&mut self.client)
            .unwrap();
        self.server_transport.update(duration, server).unwrap();
        self.server_transport.send_packets(server);
    }
}

#[test]
fn native_udp_and_webrtc_share_renet_without_cross_routing() {
    let mut harness = Harness::new(42, 42);
    let mut native = NativePeer::new(77);
    for _ in 0..500 {
        native.step(&mut harness.server);
        harness.step();
        if harness.client.is_connected() && native.client.is_connected() {
            break;
        }
    }
    assert!(harness.client.is_connected());
    assert!(native.client.is_connected());
    harness.server.send_message(
        42,
        DefaultChannel::ReliableOrdered,
        b"browser snapshot".to_vec(),
    );
    harness.server.send_message(
        77,
        DefaultChannel::ReliableOrdered,
        b"native snapshot".to_vec(),
    );
    native
        .client
        .send_message(DefaultChannel::ReliableOrdered, b"native input".to_vec());
    harness
        .client
        .send_message(DefaultChannel::ReliableOrdered, b"browser input".to_vec());
    for _ in 0..100 {
        native.step(&mut harness.server);
        harness.step();
    }
    assert_eq!(
        harness
            .client
            .receive_message(DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"browser snapshot"
    );
    assert_eq!(
        native
            .client
            .receive_message(DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"native snapshot"
    );
    assert_eq!(
        harness
            .server
            .receive_message(42, DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"browser input"
    );
    assert_eq!(
        harness
            .server
            .receive_message(77, DefaultChannel::ReliableOrdered)
            .unwrap()
            .as_ref(),
        b"native input"
    );
    assert!(
        harness
            .client
            .receive_message(DefaultChannel::ReliableOrdered)
            .is_none()
    );
    assert!(
        native
            .client
            .receive_message(DefaultChannel::ReliableOrdered)
            .is_none()
    );
}

#[test]
fn disconnect_all_releases_half_open_peers() {
    let mut harness = Harness::new(42, 42);
    assert_eq!(harness.transport.peer_count(), 1);
    harness.transport.disconnect_all(&mut harness.server);
    assert_eq!(harness.transport.peer_count(), 0);
    assert!(harness.transport.addr_to_client_id.is_empty());
    assert_eq!(harness.transport.connected_clients(), 0);
}

#[test]
fn replacing_peer_releases_netcode_slot_without_removing_replacement() {
    let mut harness = Harness::new(42, 42);
    harness.connect();
    assert_eq!(harness.transport.connected_clients(), 1);
    let old = harness.transport.add_peer(42, Rtc::new(harness.now));
    assert!(old.is_some());
    assert_eq!(harness.transport.connected_clients(), 0);
    harness
        .transport
        .update_at(Duration::ZERO, &mut harness.server, harness.now)
        .unwrap();
    assert!(!harness.server.is_connected(42));
    assert!(harness.transport.peer(42).is_some());
    assert_eq!(
        harness
            .transport
            .addr_to_client_id
            .get(&virtual_addr_for_client(42)),
        Some(&42)
    );
}

mod conditioning {
    use super::*;
    use crate::{
        conditioner::ConditionerConfig,
        server_conditioner::{ServerConditionerConfig, ServerConditionerHandle},
    };

    fn settings(latency: Duration, loss: f32) -> ServerConditionerConfig {
        ServerConditionerConfig {
            packets: ConditionerConfig {
                enabled: true,
                latency,
                packet_loss: loss,
                seed: 42,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn server_conditioning_leaves_ice_running_and_can_restore_netcode_delivery() {
        let mut harness = Harness::new(42, 42);
        let handle = ServerConditionerHandle::new(settings(Duration::ZERO, 1.0)).unwrap();
        harness.transport.set_conditioner(handle.clone());
        for _ in 0..100 {
            harness.step();
        }
        assert!(
            harness.channel_open,
            "ICE/DTLS/SCTP control traffic must bypass netcode conditioning"
        );
        assert!(!harness.server.is_connected(42));
        assert!(handle.stats().packets.incoming.simulated_loss_drops > 0);
        handle.configure(settings(Duration::ZERO, 0.0)).unwrap();
        harness.connect();
        harness.client.send_message(
            DefaultChannel::ReliableOrdered,
            b"conditioned input".to_vec(),
        );
        harness.server.send_message(
            42,
            DefaultChannel::ReliableOrdered,
            b"conditioned snapshot".to_vec(),
        );
        for _ in 0..100 {
            harness.step();
        }
        assert_eq!(
            harness
                .server
                .receive_message(42, DefaultChannel::ReliableOrdered)
                .unwrap()
                .as_ref(),
            b"conditioned input"
        );
        assert_eq!(
            harness
                .client
                .receive_message(DefaultChannel::ReliableOrdered)
                .unwrap()
                .as_ref(),
            b"conditioned snapshot"
        );
        assert!(harness.transport.conditioner().is_some());
        harness.transport.clear_conditioner();
        assert!(harness.transport.conditioner().is_none());
        assert_eq!(handle.stats().peers, 0);
    }

    #[test]
    fn conditioned_handshake_keeps_signaling_identity_validation() {
        let mut harness = Harness::new(42, 99);
        let handle = ServerConditionerHandle::new(settings(Duration::ZERO, 0.0)).unwrap();
        harness.transport.set_conditioner(handle.clone());
        for _ in 0..300 {
            harness.step();
        }
        assert!(!harness.server.is_connected(42));
        assert!(!harness.server.is_connected(99));
        assert_eq!(harness.transport.connected_clients(), 0);
        assert!(harness.transport.peer(42).is_none());
        assert_eq!(handle.stats().peers, 0);
    }

    #[test]
    fn replacing_conditioned_peer_discards_both_packet_queues() {
        let mut harness = Harness::new(42, 42);
        harness.connect();
        let handle = ServerConditionerHandle::new(settings(Duration::from_secs(60), 0.0)).unwrap();
        harness.transport.set_conditioner(handle.clone());
        harness
            .client
            .send_message(DefaultChannel::ReliableOrdered, b"old input".to_vec());
        harness.server.send_message(
            42,
            DefaultChannel::ReliableOrdered,
            b"old snapshot".to_vec(),
        );
        for _ in 0..50 {
            harness.step();
            let stats = handle.stats().packets;
            if stats.incoming.queued_packets > 0 && stats.outgoing.queued_packets > 0 {
                break;
            }
        }
        let stats = handle.stats().packets;
        assert!(stats.incoming.queued_packets > 0);
        assert!(stats.outgoing.queued_packets > 0);
        harness.transport.add_peer(42, Rtc::new(harness.now));
        assert_eq!(handle.stats().peers, 0);
        assert_eq!(handle.stats().packets.incoming.queued_packets, 0);
        assert_eq!(handle.stats().packets.outgoing.queued_packets, 0);
        harness
            .transport
            .update_at(Duration::ZERO, &mut harness.server, harness.now)
            .unwrap();
        assert!(harness.transport.peer(42).is_some());
        assert!(!harness.server.is_connected(42));
        assert!(
            harness
                .server
                .receive_message(42, DefaultChannel::ReliableOrdered)
                .is_none()
        );
    }
}
