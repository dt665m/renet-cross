#![cfg(not(target_arch = "wasm32"))]

use renet::{ConnectionConfig, RenetClient, RenetServer};
use renet_cross::{
    ClientAuthentication, ClientTransportConfig, MixedTransportBuilder, ServerTransportConfig,
    UdpNetcodeClientTransport,
    conditioner::{ConditionerConfig, ConditionerHandle},
    server_conditioner::{ServerConditionerConfig, ServerConditionerHandle},
};
use std::{net::UdpSocket, time::Duration};

#[test]
fn client_configuration_intercepts_the_first_handshake() {
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver.set_nonblocking(true).unwrap();
    let handle = ConditionerHandle::new(ConditionerConfig {
        enabled: true,
        packet_loss: 1.0,
        ..Default::default()
    })
    .unwrap();
    let mut transport = UdpNetcodeClientTransport::new_with_config(
        Duration::from_secs(1_700_000_000),
        ClientAuthentication::Unsecure {
            protocol_id: 19,
            client_id: 1,
            server_addr: receiver.local_addr().unwrap(),
            user_data: None,
        },
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        ClientTransportConfig {
            conditioner: Some(handle.clone()),
        },
    )
    .unwrap();
    let mut client = RenetClient::new(ConnectionConfig::default());
    transport
        .update(Duration::from_millis(100), &mut client)
        .unwrap();
    assert_eq!(handle.stats().outgoing.simulated_loss_drops, 1);
    assert_eq!(
        receiver.recv_from(&mut [0; 2048]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn mixed_config_installs_one_control_on_both_backends_before_ingress() {
    let handle = ServerConditionerHandle::new(ServerConditionerConfig {
        packets: ConditionerConfig {
            enabled: true,
            packet_loss: 1.0,
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let mut transport = MixedTransportBuilder::new(19)
        .udp_bind("127.0.0.1:0".parse().unwrap())
        .webrtc_bind("127.0.0.1:0".parse().unwrap())
        .transport_config(ServerTransportConfig {
            conditioner: Some(handle.clone()),
        })
        .build()
        .unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    sender
        .send_to(b"test", transport.udp().addresses()[0])
        .unwrap();
    let mut server = RenetServer::new(ConnectionConfig::default());
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while handle.stats().packets.incoming.simulated_loss_drops == 0 {
        transport.update(Duration::ZERO, &mut server).unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "configured ingress did not arrive"
        );
        std::thread::yield_now();
    }
    assert_eq!(handle.stats().packets.incoming.simulated_loss_drops, 1);
    let mut changed = handle.config();
    changed.packets.latency = Duration::from_millis(83);
    transport
        .webrtc()
        .conditioner()
        .unwrap()
        .configure(changed.clone())
        .unwrap();
    assert_eq!(transport.udp().conditioner().unwrap().config(), changed);
    assert_eq!(handle.config(), changed);
}
