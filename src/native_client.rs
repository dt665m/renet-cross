use std::{
    io,
    net::{AddrParseError, SocketAddr, UdpSocket},
    num::NonZeroUsize,
    time::Duration,
};

use crate::packet_io::{Direction, PacketGate};
use renet::ConnectionConfig;
use renet::RenetClient;
use renetcode::{ClientAuthentication, NETCODE_MAX_PACKET_BYTES, NetcodeClient, NetcodeError};

use crate::{SessionCreateResponse, bootstrap::unix_now_duration};

#[derive(Debug, thiserror::Error)]
pub enum NativeClientError {
    #[error(transparent)]
    BootstrapAuth(#[from] crate::BootstrapAuthError),
    #[error(transparent)]
    Security(#[from] crate::SessionSecurityError),
    #[error("bootstrap HTTP body exceeds its size limit")]
    BodyTooLarge,
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
    pub connection_config: ConnectionConfig,
    pub udp_bind: SocketAddr,
    pub override_udp_addr: Option<SocketAddr>,
    pub transport: crate::ClientTransportConfig,
    pub session_request: crate::SessionCreateRequest,
    pub require_secure: bool,
}

impl Default for NativeConnectOptions {
    fn default() -> Self {
        Self {
            connection_config: ConnectionConfig::default(),
            udp_bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            override_udp_addr: None,
            transport: Default::default(),
            session_request: Default::default(),
            require_secure: false,
        }
    }
}

#[derive(Debug)]
pub struct UdpNetcodeClientTransport {
    egress: crate::egress::Egress<SocketAddr>,
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
        Self::new_with_config(current_time, authentication, socket, Default::default())
    }

    /// Construct with runtime packet controls installed before any handshake.
    pub fn new_with_config(
        current_time: Duration,
        authentication: ClientAuthentication,
        socket: UdpSocket,
        config: crate::ClientTransportConfig,
    ) -> Result<Self, NativeClientError> {
        socket.set_nonblocking(true)?;
        let netcode_client = NetcodeClient::new(current_time, authentication)?;

        let packets = PacketGate::default();
        if let Some(handle) = config.conditioner {
            packets.attach(handle);
        }
        Ok(Self {
            egress: crate::egress::Egress::new(crate::EgressBasis::NativeUdpIp),
            socket,
            netcode_client,
            buffer: [0; 65_535],
            max_datagrams_per_update: NonZeroUsize::new(256).unwrap(),
            packets,
        })
    }

    /// Configure before the first update to include handshake traffic.
    pub fn set_egress_limit(
        &mut self,
        config: Option<crate::EgressConfig>,
    ) -> Result<(), crate::EgressConfigError> {
        self.egress.configure(config)
    }
    pub fn egress_stats(&self) -> crate::EgressStats {
        self.egress.stats()
    }

    /// Bound receive work per call, including packets from unrelated sources.
    pub fn set_max_datagrams_per_update(&mut self, limit: NonZeroUsize) {
        self.max_datagrams_per_update = limit;
    }

    fn flush_outgoing(&mut self) -> io::Result<()> {
        for (addr, bytes) in self.packets.drain(Direction::Outgoing) {
            if addr == self.netcode_client.server_addr() {
                send_datagram(&mut self.egress, &self.socket, &bytes, addr)?;
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
            let _ = send_packet(&mut self.egress, &self.packets, &self.socket, packet, addr);
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
            send_packet(&mut self.egress, &self.packets, &self.socket, packet, addr)?;
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
            send_packet(&mut self.egress, &self.packets, &self.socket, payload, addr)?;
        }

        Ok(())
    }
}

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
    egress: &mut crate::egress::Egress<SocketAddr>,
    gate: &PacketGate<SocketAddr>,
    socket: &UdpSocket,
    bytes: &[u8],
    addr: SocketAddr,
) -> io::Result<()> {
    if !gate.defer(Direction::Outgoing, addr, bytes) {
        send_datagram(egress, socket, bytes, addr)?;
    }
    for (destination, packet) in gate.drain(Direction::Outgoing) {
        if destination == addr {
            send_datagram(egress, socket, &packet, destination)?;
        }
    }
    Ok(())
}

fn send_datagram(
    egress: &mut crate::egress::Egress<SocketAddr>,
    socket: &UdpSocket,
    payload: &[u8],
    addr: SocketAddr,
) -> io::Result<()> {
    let overhead = crate::egress::ip_overhead(addr);
    if !egress.admit(addr, payload, overhead) {
        return Ok(());
    }
    let result = socket.send_to(payload, addr);
    egress.complete(
        addr,
        payload,
        overhead,
        result.as_ref().is_ok_and(|len| *len == payload.len()),
    );
    match result {
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
    let body = session_request_body(&options, protocol_id)?;
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| NativeClientError::HttpRequest {
            url: url.clone(),
            detail: "failed to construct HTTP client".into(),
        })?
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
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
    let body = session_request_body(&options, protocol_id)?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| NativeClientError::HttpRequest {
            url: url.clone(),
            detail: "failed to construct HTTP client".into(),
        })?
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| NativeClientError::HttpRequest {
            url: url.clone(),
            detail: err.to_string(),
        })?;

    let session = decode_json_response_async::<SessionCreateResponse>(response, &url).await?;
    connect_from_session(session, protocol_id, options)
}

/// Connect using an app-owned bootstrap endpoint's response, without HTTP features.
/// A present secure envelope is always validated; require_secure forbids fallback.
pub fn connect_from_session(
    session: SessionCreateResponse,
    protocol_id: u64,
    options: NativeConnectOptions,
) -> Result<(RenetClient, UdpNetcodeClientTransport, u64), NativeClientError> {
    let request = &options.session_request;
    let server_addr = if let Some(override_addr) = options.override_udp_addr {
        override_addr
    } else {
        session
            .udp_addr
            .parse()
            .map_err(|source| NativeClientError::InvalidSessionUdpAddr {
                addr: "<invalid endpoint>".into(),
                source,
            })?
    };

    let now = unix_now_duration()?;
    let authentication = session.authentication(
        protocol_id,
        server_addr,
        crate::SessionTransport::Udp,
        now,
        options.require_secure,
        request,
    )?;

    let socket = UdpSocket::bind(options.udp_bind)?;
    let transport =
        UdpNetcodeClientTransport::new_with_config(now, authentication, socket, options.transport)?;
    let client = RenetClient::new(options.connection_config);

    Ok((client, transport, session.client_id))
}

#[cfg(any(feature = "native-sync", feature = "native-async"))]
fn session_request_body(
    options: &NativeConnectOptions,
    protocol_id: u64,
) -> Result<Vec<u8>, NativeClientError> {
    let mut request = options.session_request.clone();
    request.require_secure |= options.require_secure;
    if request.requests_authentication() {
        if request.protocol_id.is_some_and(|p| p != protocol_id) {
            return Err(crate::SessionSecurityError::BindingMismatch.into());
        }
        request.protocol_id = Some(protocol_id);
    }
    request.validate()?;
    let bytes = serde_json::to_vec(&request).map_err(|_| NativeClientError::BodyTooLarge)?;
    if bytes.len() > crate::MAX_SESSION_REQUEST_BYTES {
        return Err(NativeClientError::BodyTooLarge);
    }
    Ok(bytes)
}
#[cfg(feature = "native-sync")]
fn decode_json_response_blocking<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
    url: &str,
) -> Result<T, NativeClientError> {
    use std::io::Read;
    if !response.status().is_success() {
        return Err(NativeClientError::HttpStatus {
            url: url.into(),
            status: response.status().as_u16(),
            body: "bootstrap request rejected".into(),
        });
    }
    if response
        .content_length()
        .is_some_and(|n| n > crate::MAX_SESSION_RESPONSE_BYTES as u64)
    {
        return Err(NativeClientError::BodyTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .take(crate::MAX_SESSION_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > crate::MAX_SESSION_RESPONSE_BYTES {
        return Err(NativeClientError::BodyTooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| NativeClientError::HttpDecode {
        url: url.into(),
        detail: "invalid bootstrap JSON".into(),
    })
}
#[cfg(feature = "native-async")]
async fn decode_json_response_async<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    url: &str,
) -> Result<T, NativeClientError> {
    if !response.status().is_success() {
        return Err(NativeClientError::HttpStatus {
            url: url.into(),
            status: response.status().as_u16(),
            body: "bootstrap request rejected".into(),
        });
    }
    if response
        .content_length()
        .is_some_and(|n| n > crate::MAX_SESSION_RESPONSE_BYTES as u64)
    {
        return Err(NativeClientError::BodyTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| NativeClientError::HttpDecode {
            url: url.into(),
            detail: "failed to read bootstrap response".into(),
        })?
    {
        if chunk.len() > crate::MAX_SESSION_RESPONSE_BYTES - bytes.len() {
            return Err(NativeClientError::BodyTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| NativeClientError::HttpDecode {
        url: url.into(),
        detail: "invalid bootstrap JSON".into(),
    })
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    #[test]
    fn actual_ipv4_and_ipv6_datagrams_count_destination_headers_and_cap_drops() {
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let sender = UdpSocket::bind(bind).unwrap();
            let receiver = UdpSocket::bind(bind).unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let addr = receiver.local_addr().unwrap();
            let mut egress = crate::egress::Egress::new(crate::EgressBasis::NativeUdpIp);
            egress
                .configure(Some(
                    crate::EgressConfig::new(crate::EgressBasis::NativeUdpIp, 1, 2048, 256)
                        .unwrap(),
                ))
                .unwrap();
            send_datagram(&mut egress, &sender, &[5; 1000], addr).unwrap();
            let mut buffer = [0; 3000];
            assert_eq!(receiver.recv_from(&mut buffer).unwrap().0, 1000);
            send_datagram(&mut egress, &sender, &[5; 2000], addr).unwrap();
            send_datagram(&mut egress, &sender, &[6; 20], addr).unwrap();
            assert_eq!(receiver.recv_from(&mut buffer).unwrap().0, 20);
            let stats = egress.stats();
            assert_eq!(stats.sent_packets, 2);
            assert_eq!(stats.control_packets, 1);
            assert_eq!(stats.cap_drops, 1);
            assert_eq!(stats.encrypted_logical_bytes, 1020);
            assert_eq!(
                stats.native_udp_ip_bytes,
                1020 + if addr.is_ipv4() { 56 } else { 96 }
            );
        }
    }
}
