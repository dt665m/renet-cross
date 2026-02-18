use std::{
    io,
    net::{AddrParseError, SocketAddr, UdpSocket},
    time::Duration,
};

use renet::{ConnectionConfig, RenetClient};
use renetcode::{ClientAuthentication, NETCODE_MAX_PACKET_BYTES, NetcodeClient, NetcodeError};

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
    buffer: [u8; NETCODE_MAX_PACKET_BYTES],
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
            buffer: [0; NETCODE_MAX_PACKET_BYTES],
        })
    }

    pub fn update(
        &mut self,
        duration: Duration,
        client: &mut RenetClient,
    ) -> Result<(), NativeClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            client.disconnect_due_to_transport();
            return Err(NativeClientError::Netcode(NetcodeError::Disconnected(
                reason,
            )));
        }

        if client.disconnect_reason().is_some()
            && !self.netcode_client.is_disconnected()
            && let Ok((addr, packet)) = self.netcode_client.disconnect()
        {
            let _ = self.socket.send_to(packet, addr);
        }

        if self.netcode_client.is_connected() {
            client.set_connected();
        } else if self.netcode_client.is_connecting() {
            client.set_connecting();
        }

        loop {
            match self.socket.recv_from(&mut self.buffer) {
                Ok((len, _source)) => {
                    let packet = &mut self.buffer[..len];
                    if let Some(payload) = self.netcode_client.process_packet(packet) {
                        client.process_packet(payload);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => break,
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(err) => return Err(err.into()),
            }
        }

        if let Some((packet, addr)) = self.netcode_client.update(duration) {
            self.socket.send_to(packet, addr)?;
        }

        Ok(())
    }

    pub fn send_packets(&mut self, client: &mut RenetClient) -> Result<(), NativeClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            return Err(NativeClientError::Netcode(NetcodeError::Disconnected(
                reason,
            )));
        }

        let packets = client.get_packets_to_send();
        for packet in packets {
            let (addr, payload) = self.netcode_client.generate_payload_packet(&packet)?;
            self.socket.send_to(payload, addr)?;
        }

        Ok(())
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
