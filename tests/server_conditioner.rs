#![cfg(not(target_arch = "wasm32"))]

//! Real loopback sockets and elapsed wall time exercise the server adapter boundary.
use renet::{ConnectionConfig, DefaultChannel, RenetClient, RenetServer};
use renet_cross::{
    ClientAuthentication, ServerAuthentication, ServerConfig, UdpNetcodeClientTransport,
    UdpNetcodeServerTransport,
    conditioner::ConditionerConfig,
    server_conditioner::{ServerConditionerConfig, ServerConditionerHandle},
};
use std::{
    net::UdpSocket,
    thread,
    time::{Duration, Instant},
};

#[test]
fn startup_delay_affects_two_clients_rtt_and_reconfiguration_recovers() {
    let start = Duration::from_secs(1_700_000_000);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let mut transport = UdpNetcodeServerTransport::new(
        ServerConfig {
            current_time: start,
            max_clients: 8,
            protocol_id: 17,
            public_addresses: vec![addr],
            authentication: ServerAuthentication::Unsecure,
        },
        socket,
    )
    .unwrap();
    let handle = ServerConditionerHandle::new(ServerConditionerConfig {
        packets: ConditionerConfig {
            enabled: true,
            latency: Duration::from_millis(75),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    transport.set_conditioner(handle.clone());
    let mut server = RenetServer::new(ConnectionConfig::default());
    let mut clients: Vec<_> = (1..=2)
        .map(|id| {
            (
                id,
                RenetClient::new(ConnectionConfig::default()),
                UdpNetcodeClientTransport::new(
                    start,
                    ClientAuthentication::Unsecure {
                        protocol_id: 17,
                        client_id: id,
                        server_addr: addr,
                        user_data: None,
                    },
                    UdpSocket::bind("127.0.0.1:0").unwrap(),
                )
                .unwrap(),
            )
        })
        .collect();
    let begun = Instant::now();
    let mut previous = begun;
    let mut tick =
        |server: &mut RenetServer,
         transport: &mut UdpNetcodeServerTransport,
         clients: &mut Vec<(u64, RenetClient, UdpNetcodeClientTransport)>| {
            let now = Instant::now();
            let dt = now.duration_since(previous);
            previous = now;
            server.update(dt);
            for (id, client, link) in clients.iter_mut() {
                client.update(dt);
                link.update(dt, client).unwrap();
                if client.is_connected() {
                    client.send_message(DefaultChannel::Unreliable, vec![1]);
                    link.send_packets(client).unwrap();
                }
                if server.is_connected(*id) {
                    server.send_message(*id, DefaultChannel::Unreliable, vec![2]);
                }
                while client.receive_message(DefaultChannel::Unreliable).is_some() {}
                while server
                    .receive_message(*id, DefaultChannel::Unreliable)
                    .is_some()
                {}
            }
            transport.update(dt, server).unwrap();
            transport.send_packets(server);
            thread::sleep(Duration::from_millis(3));
        };
    while begun.elapsed() < Duration::from_secs(4) {
        tick(&mut server, &mut transport, &mut clients);
    }
    assert_eq!(transport.connected_clients(), 2);
    assert_eq!(handle.per_peer_stats().len(), 2);
    for (_, client, _) in &clients {
        let rtt = client.rtt();
        eprintln!(
            "server-only configured RTT=150ms observed={:.1}ms",
            rtt * 1000.0
        );
        assert!(
            (0.120..0.400).contains(&rtt),
            "unexpected conditioned RTT {rtt}"
        );
    }
    let mut config = handle.config();
    config.packets.enabled = false;
    handle.configure(config).unwrap();
    let resumed = Instant::now();
    while resumed.elapsed() < Duration::from_secs(2) {
        tick(&mut server, &mut transport, &mut clients);
    }
    for (_, client, _) in &clients {
        eprintln!("after Off observed={:.1}ms", client.rtt() * 1000.0);
        assert!(
            client.rtt() < 0.080,
            "RTT did not recover: {}",
            client.rtt()
        );
    }
    transport.disconnect_all(&mut server);
    assert!(handle.per_peer_stats().is_empty());
    assert_eq!(handle.stats().packets.incoming.queued_packets, 0);
    assert_eq!(handle.stats().packets.outgoing.queued_packets, 0);
}
