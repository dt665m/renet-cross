use renet::{ClientId, RenetServer};

use crate::MixedServerTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Udp,
    WebRtc,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ClientNetworkSnapshot {
    pub client_id: ClientId,
    pub transport: TransportKind,
    pub rtt_seconds: f64,
    pub packet_loss: f64,
    pub bytes_sent_per_second: f64,
    pub bytes_received_per_second: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ServerNetworkSnapshot {
    pub clients: Vec<ClientNetworkSnapshot>,
}

pub fn collect_server_network_snapshot(
    server: &RenetServer,
    transport: &MixedServerTransport,
) -> ServerNetworkSnapshot {
    collect_with_lookup(server, transport)
}

trait ClientRouteLookup {
    fn is_udp_client(&self, client_id: ClientId) -> bool;
    fn is_webrtc_client(&self, client_id: ClientId) -> bool;
}

impl ClientRouteLookup for MixedServerTransport {
    fn is_udp_client(&self, client_id: ClientId) -> bool {
        self.udp().client_addr(client_id).is_some()
    }

    fn is_webrtc_client(&self, client_id: ClientId) -> bool {
        self.webrtc().client_addr(client_id).is_some()
    }
}

fn collect_with_lookup<L>(server: &RenetServer, lookup: &L) -> ServerNetworkSnapshot
where
    L: ClientRouteLookup,
{
    let mut clients = server.clients_id();
    clients.sort_unstable();

    let mut snapshots = Vec::with_capacity(clients.len());
    for client_id in clients {
        let transport_kind = classify_transport_kind(lookup, client_id);
        let (rtt_seconds, packet_loss, bytes_sent_per_second, bytes_received_per_second) =
            match server.network_info(client_id) {
                Ok(info) => (
                    info.rtt,
                    info.packet_loss,
                    info.bytes_sent_per_second,
                    info.bytes_received_per_second,
                ),
                Err(_) => (0.0, 0.0, 0.0, 0.0),
            };

        snapshots.push(ClientNetworkSnapshot {
            client_id,
            transport: transport_kind,
            rtt_seconds,
            packet_loss,
            bytes_sent_per_second,
            bytes_received_per_second,
        });
    }

    ServerNetworkSnapshot { clients: snapshots }
}

fn classify_transport_kind<L>(lookup: &L, client_id: ClientId) -> TransportKind
where
    L: ClientRouteLookup,
{
    if lookup.is_udp_client(client_id) {
        TransportKind::Udp
    } else if lookup.is_webrtc_client(client_id) {
        TransportKind::WebRtc
    } else {
        TransportKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use renet::{ConnectionConfig, RenetServer};

    use super::{ClientRouteLookup, TransportKind, classify_transport_kind, collect_with_lookup};

    struct FakeLookup {
        udp: HashSet<u64>,
        webrtc: HashSet<u64>,
    }

    impl ClientRouteLookup for FakeLookup {
        fn is_udp_client(&self, client_id: u64) -> bool {
            self.udp.contains(&client_id)
        }

        fn is_webrtc_client(&self, client_id: u64) -> bool {
            self.webrtc.contains(&client_id)
        }
    }

    #[test]
    fn transport_kind_classification_prefers_udp_then_webrtc() {
        let lookup = FakeLookup {
            udp: [1, 3].into_iter().collect(),
            webrtc: [2, 3].into_iter().collect(),
        };

        assert_eq!(classify_transport_kind(&lookup, 1), TransportKind::Udp);
        assert_eq!(classify_transport_kind(&lookup, 2), TransportKind::WebRtc);
        assert_eq!(classify_transport_kind(&lookup, 3), TransportKind::Udp);
        assert_eq!(classify_transport_kind(&lookup, 99), TransportKind::Unknown);
    }

    #[test]
    fn snapshot_contains_all_connected_clients() {
        let mut server = RenetServer::new(ConnectionConfig::default());
        server.add_connection(10);
        server.add_connection(11);

        let lookup = FakeLookup {
            udp: [10].into_iter().collect(),
            webrtc: [11].into_iter().collect(),
        };

        let snapshot = collect_with_lookup(&server, &lookup);
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.clients[0].client_id, 10);
        assert_eq!(snapshot.clients[0].transport, TransportKind::Udp);
        assert_eq!(snapshot.clients[1].client_id, 11);
        assert_eq!(snapshot.clients[1].transport, TransportKind::WebRtc);
    }
}
