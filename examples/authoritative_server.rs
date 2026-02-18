use std::{
    net::{SocketAddr, UdpSocket},
    thread,
    time::{Duration, Instant, SystemTime},
};

use renet::{ConnectionConfig, DefaultChannel, RenetServer, ServerEvent};
use renet_server::{
    MixedServerTransport, NetcodeError, Rtc, ServerAuthentication, ServerConfig,
    UdpNetcodeServerTransport, WebRtcNetcodeServerTransport,
};

const PROTOCOL_ID: u64 = 7;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = RenetServer::new(ConnectionConfig::default());

    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let udp_addr: SocketAddr = "0.0.0.0:5000".parse()?;
    let webrtc_addr: SocketAddr = "0.0.0.0:5001".parse()?;

    let udp_socket = UdpSocket::bind(udp_addr)?;
    let webrtc_socket = UdpSocket::bind(webrtc_addr)?;

    let udp_transport = UdpNetcodeServerTransport::new(
        ServerConfig {
            current_time: now,
            max_clients: 128,
            protocol_id: PROTOCOL_ID,
            public_addresses: vec![udp_addr],
            authentication: ServerAuthentication::Unsecure,
        },
        udp_socket,
    )?;

    let webrtc_transport = WebRtcNetcodeServerTransport::new(
        ServerConfig {
            current_time: now,
            max_clients: 128,
            protocol_id: PROTOCOL_ID,
            public_addresses: vec![webrtc_addr],
            authentication: ServerAuthentication::Unsecure,
        },
        webrtc_socket,
    )?;

    let mut transport = MixedServerTransport::new(udp_transport, webrtc_transport);
    let mut last_tick = Instant::now();

    loop {
        // 1) Poll your signaling layer and register newly accepted WebRTC peers.
        // In real code, this function should create/configure `Rtc` using offer/answer flow.
        while let Some((client_id, rtc)) = poll_signaling_for_new_webrtc_peer() {
            transport.webrtc_mut().add_peer(client_id, rtc);
        }

        // 2) Authoritative tick: update renet and both transports.
        let now = Instant::now();
        let delta = now - last_tick;
        last_tick = now;

        server.update(delta);
        transport.update(delta, &mut server)?;

        // 3) Consume connect/disconnect events once, independent of transport type.
        while let Some(event) = server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    println!("Client connected: {client_id}");
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    println!("Client disconnected: {client_id} ({reason})");
                }
            }
        }

        // 4) Authoritative game logic.
        for client_id in server.clients_id() {
            while let Some(message) =
                server.receive_message(client_id, DefaultChannel::ReliableOrdered)
            {
                let text = String::from_utf8_lossy(&message);
                println!("{client_id}: {text}");
                server.broadcast_message(DefaultChannel::ReliableOrdered, message);
            }
        }

        // 5) Flush outgoing packets through both backends.
        transport.send_packets(&mut server);
        thread::sleep(Duration::from_millis(16));
    }
}

fn poll_signaling_for_new_webrtc_peer() -> Option<(u64, Rtc)> {
    None
}

#[allow(dead_code)]
fn map_signaling_error(err: NetcodeError) -> String {
    err.to_string()
}
