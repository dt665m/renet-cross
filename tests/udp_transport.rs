#![cfg(not(target_arch = "wasm32"))]

//! Real UDP/netcode/Renet traffic with an in-process fault proxy. All protocol
//! time and fault scheduling use integer steps; no sleeps or external services.
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    time::Duration,
};

use renet::{ConnectionConfig, DefaultChannel, RenetClient, RenetServer};
use renet_cross::{
    ClientAuthentication, ConnectToken, ServerAuthentication, ServerConfig,
    UdpNetcodeClientTransport, UdpNetcodeServerTransport,
};

const ID: u64 = 41;
const PROTOCOL: u64 = 7;
const DT: Duration = Duration::from_millis(10);
const START: Duration = Duration::from_secs(1_700_000_000);

fn socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
}

struct Datagram {
    due: u64,
    target: SocketAddr,
    bytes: Vec<u8>,
}

struct Harness {
    server: RenetServer,
    server_transport: UdpNetcodeServerTransport,
    client: RenetClient,
    client_transport: UdpNetcodeClientTransport,
    proxy: UdpSocket,
    rogue: UdpSocket,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    step: u64,
    packet: u64,
    impaired: bool,
    wrong_source: bool,
    dropped: usize,
    delayed: usize,
    duplicated: usize,
    pending: Vec<Datagram>,
    emitted_packets: [u64; 2],
    emitted_bytes: [u64; 2],
    maximum_data_datagram: usize,
    measured_data_datagrams: usize,
    drop_first_keepalive: bool,
    lost_keepalives: usize,
}

impl Harness {
    fn new(secure: bool) -> Self {
        let proxy = socket();
        let public_addr = proxy.local_addr().unwrap();
        let server_socket = socket();
        let server_addr = server_socket.local_addr().unwrap();
        let client_socket = socket();
        let client_addr = client_socket.local_addr().unwrap();
        let private_key = [42; 32];
        let authentication = if secure {
            ClientAuthentication::Secure {
                connect_token: ConnectToken::generate(
                    START,
                    PROTOCOL,
                    300,
                    ID,
                    15,
                    vec![public_addr],
                    None,
                    &private_key,
                )
                .unwrap(),
            }
        } else {
            ClientAuthentication::Unsecure {
                protocol_id: PROTOCOL,
                client_id: ID,
                server_addr: public_addr,
                user_data: None,
            }
        };
        Self {
            server: RenetServer::new(ConnectionConfig::default()),
            server_transport: UdpNetcodeServerTransport::new(
                ServerConfig {
                    current_time: START,
                    max_clients: 8,
                    protocol_id: PROTOCOL,
                    public_addresses: vec![public_addr],
                    authentication: if secure {
                        ServerAuthentication::Secure { private_key }
                    } else {
                        ServerAuthentication::Unsecure
                    },
                },
                server_socket,
            )
            .unwrap(),
            client: RenetClient::new(ConnectionConfig::default()),
            client_transport: UdpNetcodeClientTransport::new(START, authentication, client_socket)
                .unwrap(),
            proxy,
            rogue: socket(),
            server_addr,
            client_addr,
            step: 0,
            packet: 0,
            impaired: false,
            wrong_source: false,
            dropped: 0,
            delayed: 0,
            duplicated: 0,
            pending: Vec::new(),
            emitted_packets: [0; 2],
            emitted_bytes: [0; 2],
            maximum_data_datagram: 0,
            measured_data_datagrams: 0,
            drop_first_keepalive: false,
            lost_keepalives: 0,
        }
    }

    fn pump(&mut self) {
        let mut buffer = [0; 65_535];
        loop {
            let (len, source) = match self.proxy.recv_from(&mut buffer) {
                Ok(packet) => packet,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("proxy receive at step {}: {err}", self.step),
            };
            let target = if source == self.server_addr {
                self.client_addr
            } else {
                assert_eq!(source, self.client_addr);
                self.server_addr
            };
            let direction = usize::from(source == self.client_addr);
            self.emitted_packets[direction] += 1;
            self.emitted_bytes[direction] += len as u64;
            self.packet += 1;
            if self.drop_first_keepalive
                && source == self.server_addr
                && len > 0
                && buffer[0] & 0x0f == 4
            {
                self.drop_first_keepalive = false;
                self.lost_keepalives += 1;
                continue;
            }
            // Inspect the public netcode packet discriminator, never decrypted
            // message contents; these are actual socket-emitted UDP payloads.
            if len > 0 && buffer[0] & 0x0f == 5 {
                self.maximum_data_datagram = self.maximum_data_datagram.max(len);
                self.measured_data_datagrams += 1;
            }
            if self.impaired && self.packet.is_multiple_of(7) {
                self.dropped += 1;
                continue;
            }
            let delay = if self.impaired && self.packet.is_multiple_of(3) {
                self.delayed += 1;
                3
            } else {
                0
            };
            self.pending.push(Datagram {
                due: self.step + delay,
                target,
                bytes: buffer[..len].to_vec(),
            });
            if self.impaired && self.packet.is_multiple_of(11) {
                self.duplicated += 1;
                self.pending.push(Datagram {
                    due: self.step + delay + 1,
                    target,
                    bytes: buffer[..len].to_vec(),
                });
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for packet in pending {
            if packet.due > self.step {
                self.pending.push(packet);
                continue;
            }
            let sender = if self.wrong_source && packet.target == self.client_addr {
                &self.rogue
            } else {
                &self.proxy
            };
            sender.send_to(&packet.bytes, packet.target).unwrap();
        }
    }

    fn tick(&mut self) {
        self.step += 1;
        self.server.update(DT);
        self.client.update(DT);
        self.client_transport.update(DT, &mut self.client).unwrap();
        if self.client.is_connected() {
            self.client_transport
                .send_packets(&mut self.client)
                .unwrap();
        }
        self.pump();
        self.server_transport.update(DT, &mut self.server).unwrap();
        self.server_transport.send_packets(&mut self.server);
        self.pump();
    }

    fn connect(&mut self) {
        for _ in 0..500 {
            self.tick();
            if self.client.is_connected() && self.server.is_connected(ID) {
                return;
            }
        }
        panic!(
            "connection did not converge at simulated step {}",
            self.step
        );
    }
}

#[test]
fn lost_initial_keepalive_recovers_during_continuous_payloads() {
    let mut h = Harness::new(true);
    h.drop_first_keepalive = true;
    let mut received = false;
    let mut replied = false;
    for _ in 0..200 {
        if h.server.is_connected(ID) {
            h.server
                .send_message(ID, DefaultChannel::Unreliable, vec![7; 32]);
        }
        h.tick();
        if h.client
            .receive_message(DefaultChannel::Unreliable)
            .is_some()
        {
            received = true;
            h.client
                .send_message(DefaultChannel::ReliableOrdered, vec![9; 8]);
        }
        if h.server
            .receive_message(ID, DefaultChannel::ReliableOrdered)
            .is_some()
        {
            replied = true;
            break;
        }
    }
    assert_eq!(h.lost_keepalives, 1);
    assert!(
        h.client.is_connected(),
        "payloads must not suppress stock confirmation retries"
    );
    assert!(
        received,
        "application traffic must resume after confirmation"
    );
    assert!(replied, "authenticated bidirectional traffic must resume");
}

#[test]
fn reliable_fragmented_messages_survive_loss_duplication_and_reordering() {
    for secure in [false, true] {
        let mut h = Harness::new(secure);
        h.connect();
        h.impaired = true;
        let messages: Vec<Vec<u8>> = (0..12u8)
            .map(|index| vec![index; 4096 + index as usize])
            .collect();
        for message in &messages {
            h.client
                .send_message(DefaultChannel::ReliableOrdered, message.clone());
            h.server
                .send_message(ID, DefaultChannel::ReliableOrdered, message.clone());
        }
        let mut received_client = Vec::new();
        let mut received_server = Vec::new();
        for _ in 0..1000 {
            h.tick();
            while let Some(message) = h.client.receive_message(DefaultChannel::ReliableOrdered) {
                received_client.push(message.to_vec());
            }
            while let Some(message) = h
                .server
                .receive_message(ID, DefaultChannel::ReliableOrdered)
            {
                received_server.push(message.to_vec());
            }
            if received_client.len() == messages.len() && received_server.len() == messages.len() {
                break;
            }
        }
        assert_eq!(
            received_client, messages,
            "client, secure={secure}, step={}",
            h.step
        );
        assert_eq!(
            received_server, messages,
            "server, secure={secure}, step={}",
            h.step
        );
        assert!(h.dropped > 0 && h.delayed > 0 && h.duplicated > 0);
        // Flush delayed duplicates and prove they don't become extra messages.
        for _ in 0..50 {
            h.tick();
        }
        assert!(
            h.client
                .receive_message(DefaultChannel::ReliableOrdered)
                .is_none()
        );
        assert!(
            h.server
                .receive_message(ID, DefaultChannel::ReliableOrdered)
                .is_none()
        );
    }
}

#[test]
fn valid_handshake_packets_from_wrong_source_are_ignored() {
    let mut h = Harness::new(false);
    h.wrong_source = true;
    for _ in 0..50 {
        h.tick();
    }
    assert!(!h.client.is_connected());
    h.wrong_source = false;
    h.connect();
}

#[test]
fn oversized_datagrams_do_not_break_the_pump_or_valid_traffic() {
    let mut h = Harness::new(false);
    // Exercise the server and the client's expected source, not just source filtering.
    let oversized = vec![0; 4096];
    h.proxy.send_to(&oversized, h.server_addr).unwrap();
    h.proxy.send_to(&oversized, h.client_addr).unwrap();
    h.connect();
    h.client
        .send_message(DefaultChannel::Unreliable, b"still alive".to_vec());
    for _ in 0..50 {
        h.tick();
        if let Some(message) = h.server.receive_message(ID, DefaultChannel::Unreliable) {
            assert_eq!(message.as_ref(), b"still alive");
            return;
        }
    }
    panic!("valid traffic failed after oversized datagrams");
}

#[test]
fn duplicate_udp_identity_does_not_remove_existing_renet_connection() {
    let mut h = Harness::new(false);
    // An identity already owned by the other transport.
    h.server.add_connection(ID);
    for _ in 0..100 {
        h.server.update(DT);
        h.client.update(DT);
        let _ = h.client_transport.update(DT, &mut h.client);
        h.pump();
        h.server_transport.update(DT, &mut h.server).unwrap();
        h.pump();
    }
    assert!(h.server.is_connected(ID));
    assert_eq!(h.server_transport.connected_clients(), 0);
    assert!(!h.client.is_connected());
}

#[test]
fn idle_connection_times_out_without_wall_clock_sleep() {
    let mut h = Harness::new(true);
    h.connect();
    // Keep the proxy unpumped to simulate a link outage.
    let result = h
        .client_transport
        .update(Duration::from_secs(20), &mut h.client);
    assert!(result.is_err());
    assert!(h.client.is_disconnected());
    h.server_transport
        .update(Duration::from_secs(20), &mut h.server)
        .unwrap();
    assert!(!h.server.is_connected(ID));
}

#[test]
fn default_secure_udp_packets_and_retransmissions_fit_netcode_payload() {
    let mut h = Harness::new(true);
    h.connect();
    h.impaired = true;
    let messages = (0..25_u8)
        .flat_map(|id| {
            [40, 64, 594, 1100, 1105, 1106, 16384]
                .into_iter()
                .map(move |length| vec![id; length])
        })
        .collect::<Vec<_>>();
    for message in &messages {
        h.client
            .send_message(DefaultChannel::ReliableOrdered, message.clone());
        h.server
            .send_message(ID, DefaultChannel::ReliableOrdered, message.clone());
    }
    let mut at_client = Vec::new();
    let mut at_server = Vec::new();
    for _ in 0..2000 {
        h.tick();
        while let Some(message) = h.client.receive_message(DefaultChannel::ReliableOrdered) {
            at_client.push(message.to_vec());
        }
        while let Some(message) = h
            .server
            .receive_message(ID, DefaultChannel::ReliableOrdered)
        {
            at_server.push(message.to_vec());
        }
        assert!(
            h.maximum_data_datagram <= 1300 + 25,
            "encrypted UDP payload {}",
            h.maximum_data_datagram
        );
        assert!(h.maximum_data_datagram + 48 <= 1373);
        if at_client.len() == messages.len() && at_server.len() == messages.len() {
            break;
        }
    }
    assert_eq!(at_client, messages);
    assert_eq!(at_server, messages);
    assert!(h.measured_data_datagrams > 512);
    assert!(
        h.maximum_data_datagram + 48 > 1200,
        "stock slice payloads deliberately exceed the former IP cap"
    );
    assert!(h.dropped > 0 && h.delayed > 0 && h.duplicated > 0);
}

#[test]
fn paced_secure_udp_counts_actual_wire_control_and_retries_and_delivers_reliable() {
    use renet_cross::{EgressBasis, EgressConfig};
    let mut h = Harness::new(true);
    let config = EgressConfig::new(EgressBasis::NativeUdpIp, 60_000, 6000, 512).unwrap();
    h.client_transport.set_egress_limit(Some(config)).unwrap();
    h.server_transport.set_egress_limit(Some(config)).unwrap();
    let start = std::time::Instant::now();
    h.connect();
    h.impaired = true;
    let count = 64;
    for n in 0..count {
        let mut message = vec![n as u8; 900];
        message[0] = n as u8;
        h.client
            .send_message(DefaultChannel::ReliableOrdered, message.clone());
        h.server
            .send_message(ID, DefaultChannel::ReliableOrdered, message);
    }
    let mut received = [0; 2];
    while received != [count; 2] && start.elapsed() < Duration::from_secs(10) {
        h.tick();
        while let Some(message) = h
            .server
            .receive_message(ID, DefaultChannel::ReliableOrdered)
        {
            assert_eq!(message, vec![received[0] as u8; 900]);
            received[0] += 1;
        }
        while let Some(message) = h.client.receive_message(DefaultChannel::ReliableOrdered) {
            assert_eq!(message, vec![received[1] as u8; 900]);
            received[1] += 1;
        }
        let stats = [
            h.server_transport.egress_stats(),
            h.client_transport.egress_stats(),
        ];
        for (side, stats) in stats.iter().enumerate() {
            // The loopback socket may deliver an accepted send after this pump.
            assert!(stats.sent_packets >= h.emitted_packets[side]);
            assert!(stats.encrypted_logical_bytes >= h.emitted_bytes[side]);
            assert_eq!(
                stats.native_udp_ip_bytes,
                stats.encrypted_logical_bytes + 28 * stats.sent_packets
            );
            assert!(
                stats.native_udp_ip_bytes
                    <= config.burst_bytes()
                        + (start.elapsed().as_secs_f64() * config.bytes_per_second() as f64) as u64
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(received, [count; 2]);
    assert!(h.client_transport.egress_stats().pacing_drops > 0);
    assert!(h.server_transport.egress_stats().pacing_drops > 0);
    assert!(h.client_transport.egress_stats().control_packets > 0);
    assert!(
        h.server_transport
            .peer_egress_stats(ID)
            .unwrap()
            .control_packets
            > 0
    );
    assert_eq!(h.server_transport.egress_stats().deferred_packets, 0);
    assert!(h.dropped > 0 && h.delayed > 0 && h.duplicated > 0);
    h.impaired = false;
    h.server_transport.disconnect_all(&mut h.server);
    for _ in 0..100 {
        h.pump();
        if h.server_transport.egress_stats().sent_packets == h.emitted_packets[0]
            && h.client_transport.egress_stats().sent_packets == h.emitted_packets[1]
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    for (side, stats) in [
        h.server_transport.egress_stats(),
        h.client_transport.egress_stats(),
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(stats.sent_packets, h.emitted_packets[side]);
        assert_eq!(stats.encrypted_logical_bytes, h.emitted_bytes[side]);
        assert_eq!(
            stats.native_udp_ip_bytes,
            h.emitted_bytes[side] + 28 * h.emitted_packets[side]
        );
    }
}
