use std::{
    io,
    net::{AddrParseError, SocketAddr, UdpSocket},
    num::NonZeroUsize,
    time::Duration,
};

use crate::packet_io::{Direction, PacketGate};
#[cfg(any(feature = "native-sync", feature = "native-async"))]
use renet::ConnectionConfig;
use renet::RenetClient;
use renetcode::{ClientAuthentication, NETCODE_MAX_PACKET_BYTES, NetcodeClient, NetcodeError};

#[cfg(any(feature = "native-sync", feature = "native-async"))]
use crate::{SessionCreateResponse, bootstrap::unix_now_duration};

#[derive(Debug, thiserror::Error)]
pub enum NativeClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Netcode(#[from] NetcodeError),
    #[error("invalid session udp_addr in response '{addr}': {source}")]
    InvalidSessionUdpAddr {
        addr: String,
        source: AddrParseError,
    },
    #[error("HTTP request to {url} failed: {detail}")]
    HttpRequest { url: String, detail: String },
    #[error("HTTP {status} from {url}: {body}")]
    HttpStatus {
        url: String,
        status: u16,
        body: String,
    },
    #[error("invalid JSON from {url}: {detail}")]
    HttpDecode { url: String, detail: String },
    #[error(transparent)]
    Clock(#[from] std::time::SystemTimeError),
}

#[derive(Debug, Clone)]
pub struct NativeConnectOptions {
    pub udp_bind: SocketAddr,
    pub override_udp_addr: Option<SocketAddr>,
}

impl Default for NativeConnectOptions {
    fn default() -> Self {
        Self {
            udp_bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            override_udp_addr: None,
        }
    }
}

#[derive(Debug)]
pub struct UdpNetcodeClientTransport {
    socket: UdpSocket,
    netcode_client: NetcodeClient,
    // Receive complete datagrams before enforcing the protocol size. A smaller
    // buffer silently truncates on Unix and can produce WSAEMSGSIZE on Windows.
    buffer: [u8; 65_535],
    max_datagrams_per_update: NonZeroUsize,
    packets: PacketGate<SocketAddr>,
}

impl UdpNetcodeClientTransport {
    pub fn new(
        current_time: Duration,
        authentication: ClientAuthentication,
        socket: UdpSocket,
    ) -> Result<Self, NativeClientError> {
        socket.set_nonblocking(true)?;
        let netcode_client = NetcodeClient::new(current_time, authentication)?;

        Ok(Self {
            socket,
            netcode_client,
            buffer: [0; 65_535],
            max_datagrams_per_update: NonZeroUsize::new(256).unwrap(),
            packets: PacketGate::default(),
        })
    }

    /// Bound receive work per call, including packets from unrelated sources.
    pub fn set_max_datagrams_per_update(&mut self, limit: NonZeroUsize) {
        self.max_datagrams_per_update = limit;
    }

    fn flush_outgoing(&self) -> io::Result<()> {
        for (addr, bytes) in self.packets.drain(Direction::Outgoing) {
            if addr == self.netcode_client.server_addr() {
                send_datagram(&self.socket, &bytes, addr)?;
            }
        }
        Ok(())
    }

    fn deliver_incoming(&mut self, client: &mut RenetClient) {
        for (addr, mut bytes) in self.packets.drain(Direction::Incoming) {
            if addr == self.netcode_client.server_addr()
                && let Some(payload) = self.netcode_client.process_packet(&mut bytes)
            {
                client.process_packet(payload);
            }
        }
    }

    pub fn update(
        &mut self,
        duration: Duration,
        client: &mut RenetClient,
    ) -> Result<(), NativeClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            self.packets.reset();
            client.disconnect_due_to_transport();
            return Err(NativeClientError::Netcode(NetcodeError::Disconnected(
                reason,
            )));
        }

        if client.disconnect_reason().is_some()
            && !self.netcode_client.is_disconnected()
            && let Ok((addr, packet)) = self.netcode_client.disconnect()
        {
            let _ = send_packet(&self.packets, &self.socket, packet, addr);
        }

        if self.netcode_client.is_connected() {
            client.set_connected();
        } else if self.netcode_client.is_connecting() {
            client.set_connecting();
        }

        crate::udp_io::receive_with_budget(self.max_datagrams_per_update, || {
            let (len, source) = self.socket.recv_from(&mut self.buffer)?;
            if source != self.netcode_client.server_addr() || len > NETCODE_MAX_PACKET_BYTES {
                return Ok(());
            }
            if self
                .packets
                .defer(Direction::Incoming, source, &self.buffer[..len])
            {
                return Ok(());
            }
            if let Some(payload) = self.netcode_client.process_packet(&mut self.buffer[..len]) {
                client.process_packet(payload);
            }
            Ok(())
        })?;

        self.flush_outgoing()?;
        self.deliver_incoming(client);

        if let Some((packet, addr)) = self.netcode_client.update(duration) {
            send_packet(&self.packets, &self.socket, packet, addr)?;
        }

        // Reflect handshakes and timeouts in this call rather than one frame later.
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            self.packets.reset();
            client.disconnect_due_to_transport();
            return Err(NativeClientError::Netcode(NetcodeError::Disconnected(
                reason,
            )));
        }
        if self.netcode_client.is_connected() {
            client.set_connected();
        }

        Ok(())
    }

    pub fn send_packets(&mut self, client: &mut RenetClient) -> Result<(), NativeClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            self.packets.reset();
            return Err(NativeClientError::Netcode(NetcodeError::Disconnected(
                reason,
            )));
        }

        self.flush_outgoing()?;

        let packets = client.get_packets_to_send();
        for packet in packets {
            let (addr, payload) = self.netcode_client.generate_payload_packet(&packet)?;
            send_packet(&self.packets, &self.socket, payload, addr)?;
        }

        Ok(())
    }
}

#[cfg(feature = "packet-conditioner")]
impl UdpNetcodeClientTransport {
    /// Attach a fresh session; one handle belongs to one live client.
    pub fn set_conditioner(&mut self, handle: crate::conditioner::ConditionerHandle) {
        self.packets.attach(handle);
    }
    pub fn conditioner(&self) -> Option<crate::conditioner::ConditionerHandle> {
        self.packets.handle()
    }
    /// Discard queued packets and detach conditioning.
    pub fn clear_conditioner(&mut self) {
        self.packets.detach();
    }
}

fn send_packet(
    gate: &PacketGate<SocketAddr>,
    socket: &UdpSocket,
    bytes: &[u8],
    addr: SocketAddr,
) -> io::Result<()> {
    if !gate.defer(Direction::Outgoing, addr, bytes) {
        send_datagram(socket, bytes, addr)?;
    }
    for (destination, packet) in gate.drain(Direction::Outgoing) {
        if destination == addr {
            send_datagram(socket, &packet, destination)?;
        }
    }
    Ok(())
}

fn send_datagram(socket: &UdpSocket, payload: &[u8], addr: SocketAddr) -> io::Result<()> {
    match socket.send_to(payload, addr) {
        Ok(_) => Ok(()),
        // This is a lossy transport: Renet retransmits reliable messages. Do not
        // grow a second queue of stale packets behind a saturated socket.
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(feature = "native-sync")]
pub fn connect_via_session_http_blocking(
    base_http: &str,
    protocol_id: u64,
    options: NativeConnectOptions,
) -> Result<(RenetClient, UdpNetcodeClientTransport, u64), NativeClientError> {
    let url = format!("{}/api/session/new", base_http.trim_end_matches('/'));
    let response = reqwest::blocking::Client::new()
        .post(&url)
        .send()
        .map_err(|err| NativeClientError::HttpRequest {
            url: url.clone(),
            detail: err.to_string(),
        })?;

    let session = decode_json_response_blocking::<SessionCreateResponse>(response, &url)?;
    connect_from_session(session, protocol_id, options)
}

#[cfg(feature = "native-async")]
pub async fn connect_via_session_http_async(
    base_http: &str,
    protocol_id: u64,
    options: NativeConnectOptions,
) -> Result<(RenetClient, UdpNetcodeClientTransport, u64), NativeClientError> {
    let url = format!("{}/api/session/new", base_http.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|err| NativeClientError::HttpRequest {
            url: url.clone(),
            detail: err.to_string(),
        })?;

    let session = decode_json_response_async::<SessionCreateResponse>(response, &url).await?;
    connect_from_session(session, protocol_id, options)
}

#[cfg(any(feature = "native-sync", feature = "native-async"))]
fn connect_from_session(
    session: SessionCreateResponse,
    protocol_id: u64,
    options: NativeConnectOptions,
) -> Result<(RenetClient, UdpNetcodeClientTransport, u64), NativeClientError> {
    let server_addr = if let Some(override_addr) = options.override_udp_addr {
        override_addr
    } else {
        session
            .udp_addr
            .parse()
            .map_err(|source| NativeClientError::InvalidSessionUdpAddr {
                addr: session.udp_addr.clone(),
                source,
            })?
    };

    let now = unix_now_duration()?;
    let authentication = ClientAuthentication::Unsecure {
        protocol_id,
        client_id: session.client_id,
        server_addr,
        user_data: None,
    };

    let socket = UdpSocket::bind(options.udp_bind)?;
    let transport = UdpNetcodeClientTransport::new(now, authentication, socket)?;
    let client = RenetClient::new(ConnectionConfig::default());

    Ok((client, transport, session.client_id))
}

#[cfg(feature = "native-sync")]
fn decode_json_response_blocking<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
    url: &str,
) -> Result<T, NativeClientError> {
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(NativeClientError::HttpStatus {
            url: url.to_string(),
            status,
            body,
        });
    }

    response
        .json::<T>()
        .map_err(|err| NativeClientError::HttpDecode {
            url: url.to_string(),
            detail: err.to_string(),
        })
}

#[cfg(feature = "native-async")]
async fn decode_json_response_async<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    url: &str,
) -> Result<T, NativeClientError> {
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(NativeClientError::HttpStatus {
            url: url.to_string(),
            status,
            body,
        });
    }

    response
        .json::<T>()
        .await
        .map_err(|err| NativeClientError::HttpDecode {
            url: url.to_string(),
            detail: err.to_string(),
        })
}
